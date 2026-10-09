//! Agent loop internals.

use super::*;

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn run_agent_loop_inner(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    mut cfg: AgentLoopConfig,
    resources: AgentResourceTracker,
) -> Result<AgentRunSummary, String> {
    let AgentLoopRuntime {
        nib_cfg,
        profile,
        session_store: store,
    } = runtime;
    if store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .is_none()
    {
        store
            .try_create_session_with_id(session_id.to_string())
            .map_err(|error| format!("failed to create session {session_id}: {error}"))?;
    }
    let normalized_goal = normalize_plan_goal(goal);
    if normalized_goal.is_empty() {
        return Err("agent goal cannot be empty".to_string());
    }
    if !matches!(cfg.mode.as_str(), "execute" | "plan") {
        return Err(format!("unsupported agent mode: {}", cfg.mode));
    }
    let run_id = cfg
        .run_id
        .clone()
        .ok_or_else(|| "agent run identity was not initialized".to_string())?;
    let request_scope = request_scope_for_run(session_id, &run_id)?;
    if let Some(steering) = cfg.steering.as_ref() {
        steering.verify_binding(&store, session_id, &run_id)?;
    }
    let steering_enabled = cfg.steering.is_some();
    let session_before_request = store
        .load_result(session_id)
        .map_err(|error| format!("failed to inspect answer-only eligibility: {error}"))?
        .ok_or_else(|| "session disappeared before answer-only eligibility".to_string())?;
    if parse_verification_waiver(goal).is_some() {
        prepare_user_turn(&store, session_id, goal, profile.root_path())?;
        let verification_id = apply_human_verification_waiver(&store, session_id, goal)?
            .expect("waiver syntax was checked before authenticated application");
        return finish_human_verification_waiver(
            &store,
            session_id,
            &run_id,
            &verification_id,
            &cfg.stream_tx,
        );
    }
    if let Some(plan_id) = cfg.continuation_plan_id.as_deref() {
        if let Some(invocation_id) = cfg.discussion_invocation_id {
            prepare_discussion_turn(
                &store,
                session_id,
                plan_id,
                &normalized_goal,
                &run_id,
                invocation_id,
            )?;
        } else {
            prepare_continue_turn(&store, session_id, plan_id, &normalized_goal, &run_id)?;
        }
    }
    let answer_only_candidate = nib_cfg.agent.answer_only
        && cfg.interactive_request
        && cfg.mode == "execute"
        && cfg.continuation_plan_id.is_none();
    let active_plan = session_before_request
        .plan
        .as_ref()
        .is_some_and(|plan| !plan.is_complete());
    let active_prior_run = has_unterminated_prior_run(&session_before_request, &run_id);
    if answer_only_candidate && active_prior_run {
        prepare_user_turn(&store, session_id, goal, profile.root_path())?;
        return finish_answer_only_planning_required(
            &store,
            session_id,
            &run_id,
            &cfg.stream_tx,
            "active_run",
        )
        .await;
    }
    // The execution plan gate protects mutations; it does not decide whether a
    // tool-free conversational answer may be returned.
    let answer_only_eligible = answer_only_candidate && !active_prior_run;

    let project_root = profile.root_path().to_path_buf();
    let max_turns = if cfg.max_steps == 0 {
        nib_cfg.agent.max_turns.max(1)
    } else {
        cfg.max_steps
    };
    let max_transitions = max_turns.saturating_mul(10).saturating_add(10);
    let sensitive_values = nib_cfg.sensitive_values();
    let public_output_sensitive_values = nib_cfg.public_session_sensitive_values();
    let untracked_llm: Arc<dyn LlmClient> =
        match crate::llm::factory::create_client_with_sensitive_values(
            &nib_cfg.llm,
            cfg.provider.as_deref(),
            &sensitive_values,
        ) {
            Ok(llm) => llm,
            Err(error) => {
                let failure = llm_configuration_failure(
                    &nib_cfg,
                    cfg.provider.as_deref(),
                    &error,
                    &sensitive_values,
                );
                return reconcile_preflight_llm_failure(
                    &store,
                    session_id,
                    &normalized_goal,
                    failure,
                    &cfg.stream_tx,
                )
                .await;
            }
        };
    let llm: Arc<dyn LlmClient> = Arc::new(ResourceTrackingLlm {
        inner: untracked_llm,
        resources: resources.clone(),
    });
    let skill_selection = select_profile_skill_selection(&project_root, &nib_cfg, &profile, goal)?;
    let active_skills = &skill_selection.skills;
    let policy_rules = skill_policy_rules(active_skills);
    let after_tool_hooks = skill_after_tool_hooks(active_skills);
    if cfg.continuation_plan_id.is_none() {
        prepare_user_turn(&store, session_id, goal, profile.root_path())?;
    }
    for record in &skill_selection.records {
        store
            .record_skill_usage(session_id, &record.skill_name, Some(record.reason.clone()))
            .map_err(|error| error.to_string())?;
    }

    let mut answer_route_requests = 0u32;
    if answer_only_eligible {
        let memory = if nib_cfg.memory.enabled {
            profile.memory_store().load_result()?
        } else {
            crate::session::memory::MemoryStoreData::default()
        };
        let mut answer_context =
            assemble_runtime_context_sections(&project_root, goal, active_skills, &memory);
        if let Some(session) = store
            .load_result(session_id)
            .map_err(|error| format!("failed to load answer-only attachments: {error}"))?
        {
            let attachments = session
                .messages
                .iter()
                .rev()
                .find(|message| message.role == "user")
                .map(|message| message.attachments.as_slice())
                .unwrap_or(&[]);
            answer_context.attachments = attachment_context_sections(&project_root, attachments);
        }
        let workload_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            profile.daemon_dir().to_path_buf(),
        )?;
        answer_context.workload = workload_context_sections(&workload_store.list()?);
        answer_route_requests = 1;
        match run_answer_only_route(
            &store,
            session_id,
            &run_id,
            &project_root,
            &answer_context,
            &llm,
            &nib_cfg,
            &public_output_sensitive_values,
            &request_scope,
            &cfg.stream_tx,
        )
        .await?
        {
            AnswerOnlyRoute::Completed(summary) | AnswerOnlyRoute::Failed(summary) => {
                return Ok(summary)
            }
            AnswerOnlyRoute::Fallback if active_plan => {
                return finish_answer_only_planning_required(
                    &store,
                    session_id,
                    &run_id,
                    &cfg.stream_tx,
                    "active_plan",
                )
                .await;
            }
            AnswerOnlyRoute::Fallback => {}
        }
    }

    if cfg.continuation_plan_id.is_none() {
        invalidate_nonresumable_plan(&store, session_id, &normalized_goal)?;
    }
    let mcp_manager = if nib_cfg.mcp.client_enabled && !nib_cfg.mcp.servers.is_empty() {
        Some(Arc::new(
            crate::integrations::mcp::McpManager::new(
                &nib_cfg.mcp.servers,
                &public_output_sensitive_values,
            )
            .await
            .map_err(|error| format!("failed to initialize MCP clients: {error}"))?,
        ))
    } else {
        None
    };
    let mut executor = ToolExecutor::new(project_root.clone(), nib_cfg.execution.clone())
        .with_auto_approve(cfg.auto_approve)
        .with_terminal_config(&nib_cfg.terminal)
        .with_approvals_config(&nib_cfg.approvals)
        .with_session_store(store.clone())
        .with_environment(profile.custom_env())
        .with_sensitive_values(public_output_sensitive_values.clone())
        .with_deferred_background_start(true)
        .with_policy_rules(policy_rules)
        .with_after_tool_hooks(after_tool_hooks);
    if let Some(stream_tx) = cfg.stream_tx.clone() {
        let (terminal_tx, mut terminal_rx) = tokio::sync::mpsc::channel(64);
        executor = executor.with_terminal_output_sender(terminal_tx);
        tokio::spawn(async move {
            while let Some(event) = terminal_rx.recv().await {
                let stream = match event.stream {
                    crate::tools::core::TerminalOutputStream::Stdout => "stdout",
                    crate::tools::core::TerminalOutputStream::Stderr => "stderr",
                };
                let _ = stream_tx
                    .send(StreamEvent::TerminalOutput {
                        invocation_id: event.invocation_id,
                        tool_name: event.tool_name,
                        stream: stream.to_string(),
                        chunk: String::from_utf8_lossy(&event.chunk).into_owned(),
                        background_task_id: event.background_task_id,
                    })
                    .await;
            }
        });
    }
    if let Some(mcp) = mcp_manager {
        executor = executor.with_mcp_manager(mcp);
    }
    if let Some(handler) = cfg.approval_handler.clone() {
        executor = executor.with_approval_handler(handler);
    }

    if nib_cfg.daemons.cron_enabled && nib_cfg.daemons.curator_enabled {
        let maintenance_started = Instant::now();
        let maintenance = crate::daemons::cron::Cron::run_profile_maintenance_due(
            &profile,
            nib_cfg.daemons.interval_seconds,
            nib_cfg.daemons.retention_days,
            crate::daemons::curator::CuratorPolicy {
                allow_destructive_cleanup: nib_cfg.daemons.allow_destructive_cleanup,
            },
            Utc::now(),
        );
        match maintenance {
            Ok(Some(report)) => record_curator_tool_call(
                &store,
                session_id,
                profile.id(),
                &nib_cfg,
                Some(&report),
                None,
                maintenance_started.elapsed().as_secs_f64(),
            )?,
            Ok(None) => {}
            Err(error) => {
                record_curator_tool_call(
                    &store,
                    session_id,
                    profile.id(),
                    &nib_cfg,
                    None,
                    Some(&error),
                    maintenance_started.elapsed().as_secs_f64(),
                )?;
                return Err(error);
            }
        }
    }

    let tools_schema = executor.get_tools_schema().await;
    let memory = if nib_cfg.memory.enabled {
        profile.memory_store().load_result()?
    } else {
        crate::session::memory::MemoryStoreData::default()
    };
    let mut context_sections =
        assemble_runtime_context_sections(&project_root, goal, active_skills, &memory);
    let mut instruction_root = project_root.clone();
    let mut instruction_scopes = vec![instruction_root.clone()];
    if let Ok(Some(session)) = store.load_result(session_id) {
        let attachments = session
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.attachments.as_slice())
            .unwrap_or(&[]);
        instruction_scopes.extend(
            attachments
                .iter()
                .map(|attachment| instruction_root.join(&attachment.path)),
        );
        context_sections.attachments = attachment_context_sections(&project_root, attachments);
    }
    instruction_scopes.sort();
    instruction_scopes.dedup();
    let mut instruction_resolver = match InstructionResolver::new(&instruction_root) {
        Ok(resolver) => resolver,
        Err(error) => {
            return reconcile_preflight_instruction_failure(
                &store,
                session_id,
                &normalized_goal,
                "initial_resolution",
                format!("required project instructions are unavailable: {error}"),
                &cfg.stream_tx,
            )
            .await;
        }
    };
    let resolved_instructions =
        match instruction_resolver.resolve_for_scopes(instruction_scopes.iter()) {
            Ok(instructions) => instructions,
            Err(error) => {
                return reconcile_preflight_instruction_failure(
                    &store,
                    session_id,
                    &normalized_goal,
                    "initial_resolution",
                    format!("required project instructions are unavailable: {error}"),
                    &cfg.stream_tx,
                )
                .await;
            }
        };
    let mut instruction_identity = resolved_instructions.identity.clone();
    context_sections.agents = resolved_instructions.render();
    derive_active_plan_verification_requirements(
        &store,
        session_id,
        goal,
        &context_sections.agents,
    )?;
    let workload_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        profile.daemon_dir().to_path_buf(),
    )?;
    context_sections.workload = workload_context_sections(&workload_store.list()?);
    let tools_ref = (cfg.mode == "execute").then_some(tools_schema.as_slice());

    let mut state = AgentState::Idle;
    let mut trace = vec![state.as_str().to_string()];
    let mut transition_count = 0u32;
    let mut llm_turns = 0u32;
    let mut tool_call_count = 0usize;
    let mut messages = Vec::new();
    let mut llm_tools: Option<Vec<Value>> = None;
    let mut response_content: Option<String> = None;
    let mut pending_response_events: Vec<StreamEvent> = Vec::new();
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    let mut preflight_failures: std::collections::HashMap<ToolInvocationId, &'static str> =
        std::collections::HashMap::new();
    let mut reconciliation_reason: Option<String> = None;
    let mut instruction_context_detail: Option<String> = None;
    let mut reconciliation_failure: Option<LlmError> = None;
    let mut pending_question: Option<ToolCallRequest> = None;
    let mut pending_observations: Vec<Value> = Vec::new();
    let mut pending_batch_success = true;
    let mut outcome = "running".to_string();
    let mut bound_reached = false;
    let mut active_plan_id: Option<String> = None;
    let mut provider_continuation: Option<ProviderContinuation> = None;
    let mut verification_recovery: Option<Vec<String>> = None;
    let mut failed_tool_batches = FailedToolBatchGuard::default();

    store
        .record_event(
            session_id,
            "state_transition",
            json!({"from": Value::Null, "to": state.as_str()}),
        )
        .map_err(|error| error.to_string())?;
    emit(
        &cfg.stream_tx,
        StreamEvent::StateTransition {
            state: state.as_str().to_string(),
        },
    )
    .await;

    while state != AgentState::Done {
        let steering = cfg
            .steering
            .as_mut()
            .map(ExactRunSteeringReceiver::drain)
            .unwrap_or_default();
        if !steering.is_empty() {
            let can_replan_at_reconciliation = state == AgentState::Reconciliation
                && cfg.mode == "plan"
                && reconciliation_failure.is_none()
                && !bound_reached
                && llm_turns < max_turns
                && reconciliation_reason.as_deref() == Some("plan_ready");
            let cannot_apply = state == AgentState::WaitingForUserInput
                || (state == AgentState::Reconciliation && !can_replan_at_reconciliation);
            if cannot_apply {
                record_steering_delivery_failure(
                    &store,
                    session_id,
                    &run_id,
                    &steering
                        .iter()
                        .map(|instruction| instruction.sequence)
                        .collect::<Vec<_>>(),
                    "run_reconciled_before_safe_boundary",
                )?;
            } else {
                let channel_id = cfg
                    .steering
                    .as_ref()
                    .map(|receiver| receiver.channel_id.as_str())
                    .ok_or_else(|| "steering intake has no installed receiver".to_string())?;
                record_steering_intake(&store, session_id, &run_id, channel_id, &steering)?;
                append_steering_context(&mut context_sections, &steering);
                failed_tool_batches.reset();
                if provider_continuation.take().is_some() {
                    record_provider_continuation_lifecycle(
                        &store,
                        session_id,
                        "provider_continuation_abandoned_by_steering",
                        &run_id,
                    )?;
                }

                if state == AgentState::PlanApproval
                    && supersede_unapproved_plan_for_steering(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                        &steering,
                    )?
                {
                    active_plan_id = None;
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Planning,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }

                if state == AgentState::InspectLlm {
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::BuildContext,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }

                if state == AgentState::UpdateMemory {
                    response_content = None;
                    pending_response_events.clear();
                    tool_calls.clear();
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::BuildContext,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }

                if matches!(state, AgentState::UserApproval | AgentState::ToolExecute) {
                    response_content = None;
                    tool_calls.clear();
                    store
                        .record_event(
                            session_id,
                            "tool_proposal_superseded_by_steering",
                            json!({
                                "first_sequence": steering.first().map(|instruction| instruction.sequence),
                                "last_sequence": steering.last().map(|instruction| instruction.sequence),
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::BuildContext,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }

                if state == AgentState::Reconciliation {
                    reconciliation_reason = None;
                    response_content = None;
                    tool_calls.clear();
                    if !supersede_unapproved_plan_for_steering(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                        &steering,
                    )? {
                        return Err(
                            "plan steering reached reconciliation without its bound unapproved plan"
                                .to_string(),
                        );
                    }
                    active_plan_id = None;
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Planning,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }
            }
        }

        if transition_count >= max_transitions && state != AgentState::Reconciliation {
            bound_reached = true;
            reconciliation_reason = Some("transition_limit_reached".to_string());
            state = transition_state(
                &store,
                session_id,
                state,
                AgentState::Reconciliation,
                &mut trace,
                &mut transition_count,
                &cfg.stream_tx,
            )
            .await?;
            continue;
        }

        state = match state {
            AgentState::Idle => {
                let (next, plan_id) = route_idle_plan(&store, session_id, &normalized_goal)?;
                if let Some(plan_id) = plan_id.as_deref() {
                    record_run_plan_binding(&store, session_id, &run_id, plan_id)?;
                }
                active_plan_id = plan_id;
                transition_state(
                    &store,
                    session_id,
                    state,
                    next,
                    &mut trace,
                    &mut transition_count,
                    &cfg.stream_tx,
                )
                .await?
            }
            AgentState::Planning => {
                if llm_turns >= max_turns {
                    bound_reached = true;
                    reconciliation_reason = Some("turn_limit_reached".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    if llm_turns.saturating_add(1) >= max_turns
                        && !close_steering_admission(
                            steering_enabled,
                            &store,
                            session_id,
                            &run_id,
                            "final_planning_request",
                        )?
                    {
                        continue;
                    }
                    let planning_session = store
                        .load_result(session_id)
                        .map_err(|error| {
                            format!("failed to load session context for planning: {error}")
                        })?
                        .ok_or_else(|| "session disappeared before planning".to_string())?;
                    match crate::agent::planner::validate_planning_instruction_context(
                        goal,
                        &context_sections,
                        Some(&planning_session),
                        nib_cfg.llm.context_length,
                    ) {
                        Ok(bounded) => {
                            store
                                .record_event(
                                    session_id,
                                    "context_bounded",
                                    crate::context::snapshot::snapshot_from_bounded_input(
                                        "planning",
                                        "sent",
                                        "configured",
                                        nib_cfg.llm.context_length,
                                        &bounded,
                                    )
                                    .to_event_details(&run_id),
                                )
                                .map_err(|error| error.to_string())?;
                        }
                        Err(error) => {
                            let error = format!(
                            "required project instructions cannot fit the planning request: {error}"
                        );
                            persist_instruction_context_block(
                                &store,
                                session_id,
                                active_plan_id.as_deref(),
                                &normalized_goal,
                                "planning_prompt_fit",
                                &error,
                            )?;
                            instruction_context_detail =
                                Some(instruction_context_user_message(&error));
                            reconciliation_reason = Some("instruction_context_missing".to_string());
                            state = transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::Reconciliation,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?;
                            continue;
                        }
                    }
                    llm_turns += 1;
                    match crate::agent::planner::generate_plan_with_context_events_bounded_scoped(
                        &llm,
                        goal,
                        &context_sections,
                        Some(&planning_session),
                        cfg.stream_tx.as_ref(),
                        nib_cfg.llm.context_length,
                        Some(request_scope.clone()),
                    )
                    .await
                    {
                        Ok(mut plan) => {
                            let source_message_index = planning_session
                                .message_provenance
                                .iter()
                                .rev()
                                .find(|source| source.origin.is_human())
                                .map(|source| source.message_index);
                            add_independent_verification_requirements(
                                &mut plan,
                                goal,
                                &context_sections.agents,
                                source_message_index,
                            )?;
                            sanitize_provider_plan(&mut plan, &public_output_sensitive_values);
                            let step_count = plan.steps.len();
                            let plan_id = plan.id.clone();
                            let plan_goal = plan.goal.clone();
                            let plan_steps: Vec<String> = plan
                                .steps
                                .iter()
                                .map(|step| step.description.clone())
                                .collect();
                            let stored = store
                                .update_session(session_id, |session| {
                                    if let Some(current) = session.plan.as_ref() {
                                        let current_plan_id = current.id.clone();
                                        let current_goal = current.goal.clone();
                                        append_session_event(
                                            session,
                                            "plan_generation_conflict",
                                            json!({
                                                "generated_plan_id": plan_id,
                                                "generated_goal": plan_goal,
                                                "current_plan_id": current_plan_id,
                                                "current_goal": current_goal,
                                            }),
                                        );
                                        return Ok(false);
                                    }
                                    session.plan = Some(plan);
                                    session.events.push(SessionEvent {
                                        index: session.events.len(),
                                        kind: "plan_generated".to_string(),
                                        details: json!({
                                            "plan_id": plan_id,
                                            "goal": plan_goal,
                                            "step_count": step_count,
                                        }),
                                        timestamp: Some(Utc::now()),
                                    });
                                    Ok(true)
                                })
                                .map_err(|error| error.to_string())?;
                            if stored {
                                record_run_plan_binding(&store, session_id, &run_id, &plan_id)?;
                                active_plan_id = Some(plan_id);
                                emit(
                                    &cfg.stream_tx,
                                    StreamEvent::PlanGenerated {
                                        step_count,
                                        steps: plan_steps,
                                    },
                                )
                                .await;
                                emit_plan_progress(
                                    &store,
                                    session_id,
                                    &cfg.stream_tx,
                                    &public_output_sensitive_values,
                                )?;
                                if cfg.mode == "plan" {
                                    reconciliation_reason = Some("plan_ready".to_string());
                                    transition_state(
                                        &store,
                                        session_id,
                                        state,
                                        AgentState::Reconciliation,
                                        &mut trace,
                                        &mut transition_count,
                                        &cfg.stream_tx,
                                    )
                                    .await?
                                } else {
                                    transition_state(
                                        &store,
                                        session_id,
                                        state,
                                        AgentState::PlanApproval,
                                        &mut trace,
                                        &mut transition_count,
                                        &cfg.stream_tx,
                                    )
                                    .await?
                                }
                            } else {
                                reconciliation_reason =
                                    Some("plan_changed_during_generation".to_string());
                                transition_state(
                                    &store,
                                    session_id,
                                    state,
                                    AgentState::Reconciliation,
                                    &mut trace,
                                    &mut transition_count,
                                    &cfg.stream_tx,
                                )
                                .await?
                            }
                        }
                        Err(error) => {
                            reconciliation_failure = Some(redact_provider_failure(
                                &nib_cfg,
                                error.with_phase(LlmErrorPhase::Planning),
                            ));
                            reconciliation_reason = Some("planning_failed".to_string());
                            transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::Reconciliation,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?
                        }
                    }
                }
            }
            AgentState::PlanApproval => {
                let expected_plan_id = active_plan_id
                    .clone()
                    .ok_or_else(|| "plan approval state has no bound plan".to_string())?;
                let session = store
                    .load_result(session_id)
                    .map_err(|error| {
                        format!("failed to load session before plan approval: {error}")
                    })?
                    .ok_or_else(|| "session disappeared before plan approval".to_string())?;
                let plan = session
                    .plan
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| "plan approval state has no plan".to_string())?;
                if plan.id != expected_plan_id
                    || !plan.is_structured()
                    || !plan.matches_goal(&normalized_goal)
                {
                    record_plan_binding_conflict(
                        &store,
                        session_id,
                        &expected_plan_id,
                        &normalized_goal,
                        "plan_approval",
                    )?;
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else if plan.approved {
                    if llm_turns < max_turns {
                        open_steering_admission(
                            steering_enabled,
                            &store,
                            session_id,
                            &run_id,
                            "approved_plan_build_context",
                        )?;
                    }
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::BuildContext,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    if !close_steering_admission(
                        steering_enabled,
                        &store,
                        session_id,
                        &run_id,
                        "plan_approval",
                    )? {
                        continue;
                    }
                    // Print-and-continue: the plan is already in the transcript via
                    // PlanGenerated. Ask the user only for unclear requests or action
                    // approvals, not to rubber-stamp the plan.
                    let decision_applied = store
                        .update_session(session_id, |session| {
                            if session.plan.as_ref() != Some(&plan) {
                                let current_plan_id =
                                    session.plan.as_ref().map(|current| current.id.clone());
                                let current_goal =
                                    session.plan.as_ref().map(|current| current.goal.clone());
                                append_session_event(
                                    session,
                                    "stale_plan_approval_ignored",
                                    json!({
                                        "expected_plan_id": expected_plan_id,
                                        "expected_goal": normalized_goal,
                                        "current_plan_id": current_plan_id,
                                        "current_goal": current_goal,
                                        "approved": true,
                                        "auto": true,
                                    }),
                                );
                                return Ok(false);
                            }
                            let current = session.plan.as_mut().ok_or_else(|| {
                                crate::session::SessionError::InvalidMutation(
                                    "plan disappeared while applying approval".to_string(),
                                )
                            })?;
                            current.approve();
                            let plan_id = current.id.clone();
                            let plan_goal = current.goal.clone();
                            append_session_event(
                                session,
                                "plan_approved",
                                json!({
                                    "plan_id": plan_id,
                                    "goal": plan_goal,
                                    "approved": true,
                                    "auto": true,
                                }),
                            );
                            Ok(true)
                        })
                        .map_err(|error| error.to_string())?;
                    if !decision_applied {
                        reconciliation_reason = Some("plan_binding_changed".to_string());
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    } else {
                        emit_plan_progress(
                            &store,
                            session_id,
                            &cfg.stream_tx,
                            &public_output_sensitive_values,
                        )?;
                        if llm_turns < max_turns {
                            open_steering_admission(
                                steering_enabled,
                                &store,
                                session_id,
                                &run_id,
                                "approved_plan_build_context",
                            )?;
                        }
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::BuildContext,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    }
                }
            }
            AgentState::BuildContext => {
                let refreshed = match instruction_resolver
                    .resolve_for_scopes(instruction_scopes.iter())
                {
                    Ok(refreshed) => refreshed,
                    Err(error) => {
                        let error =
                            format!("required project instructions are unavailable: {error}");
                        persist_instruction_context_block(
                            &store,
                            session_id,
                            active_plan_id.as_deref(),
                            &normalized_goal,
                            "refresh",
                            &error,
                        )?;
                        instruction_context_detail = Some(instruction_context_user_message(&error));
                        reconciliation_reason = Some("instruction_context_missing".to_string());
                        state = transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?;
                        continue;
                    }
                };
                if refreshed.identity != instruction_identity {
                    let previous_identity =
                        std::mem::replace(&mut instruction_identity, refreshed.identity.clone());
                    context_sections.agents = refreshed.render();
                    store
                        .record_event(
                            session_id,
                            "instruction_context_refreshed",
                            json!({
                                "previous_identity": previous_identity,
                                "identity": instruction_identity,
                                "scope_count": instruction_scopes.len(),
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                }
                if verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "build_context",
                )? {
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Compression,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                }
            }
            AgentState::Compression => {
                if llm_turns >= max_turns {
                    bound_reached = true;
                    reconciliation_reason = Some("turn_limit_reached".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    let skip_rebuild = if let Some(continuation) = provider_continuation.as_ref() {
                        let estimated =
                            approximate_llm_input_tokens(&messages, llm_tools.as_deref());
                        match crate::context::snapshot::continuation_dispatch(
                            nib_cfg.llm.context_length,
                            estimated,
                            continuation.encoded_bytes(),
                            continuation.unresolved_count(),
                        ) {
                            crate::context::snapshot::ContinuationDispatch::Send => {
                                let snapshot = crate::context::snapshot::snapshot_with_continuation(
                                    "continuation",
                                    "sent",
                                    "configured",
                                    nib_cfg.llm.context_length,
                                    &crate::context::budget::BoundedLlmInput {
                                        messages: messages.clone(),
                                        tools: llm_tools.clone(),
                                        approximate_tokens: estimated,
                                        raw_message_count: messages.len(),
                                        raw_tool_count: llm_tools.as_ref().map_or(0, Vec::len),
                                        included_tool_count: llm_tools.as_ref().map_or(0, Vec::len),
                                    },
                                    Some(continuation.encoded_bytes()),
                                    None,
                                );
                                store
                                    .record_event(
                                        session_id,
                                        "context_bounded",
                                        snapshot.to_event_details(&run_id),
                                    )
                                    .map_err(|error| error.to_string())?;
                                true
                            }
                            crate::context::snapshot::ContinuationDispatch::Rebuild => {
                                provider_continuation = None;
                                false
                            }
                            crate::context::snapshot::ContinuationDispatch::Block { message } => {
                                store
                                    .record_event(
                                        session_id,
                                        "context_admission_blocked",
                                        json!({
                                            "run_id": run_id,
                                            "phase": "continuation",
                                            "message": message,
                                            "estimated_input": estimated,
                                            "window": nib_cfg.llm.context_length,
                                        }),
                                    )
                                    .map_err(|error| error.to_string())?;
                                reconciliation_reason = Some("context_limit".to_string());
                                state = transition_state(
                                    &store,
                                    session_id,
                                    state,
                                    AgentState::Reconciliation,
                                    &mut trace,
                                    &mut transition_count,
                                    &cfg.stream_tx,
                                )
                                .await?;
                                continue;
                            }
                        }
                    } else {
                        false
                    };
                    if skip_rebuild {
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::InspectLlm,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    } else {
                        // Automatic compression is optional context maintenance. Keep the
                        // final remaining model turn for the task request so an accepted
                        // exact-run steer can never be consumed by a summary request that
                        // does not contain steering context.
                        let compression = if llm_turns.saturating_add(1) >= max_turns {
                            None
                        } else {
                            let generation_requests_before = resources.generation_requests();
                            let result = crate::context::compression::maybe_compress_session(
                                &store, session_id, &llm, &nib_cfg,
                            )
                            .await;
                            resources.record_compression_requests_since(generation_requests_before);
                            match result {
                                Ok(compression) => compression,
                                Err(error) => {
                                    reconciliation_failure =
                                        Some(redact_provider_failure(&nib_cfg, error));
                                    reconciliation_reason = Some("compression_failed".to_string());
                                    state = transition_state(
                                        &store,
                                        session_id,
                                        state,
                                        AgentState::Reconciliation,
                                        &mut trace,
                                        &mut transition_count,
                                        &cfg.stream_tx,
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                        };
                        if let Some(report) = compression {
                            llm_turns += 1;
                            emit(
                                &cfg.stream_tx,
                                StreamEvent::Compression {
                                    before_tokens: report.before_tokens,
                                    after_tokens: report.after_tokens,
                                    summarized_through: report.summarized_through,
                                },
                            )
                            .await;
                        }
                        if llm_turns >= max_turns {
                            bound_reached = true;
                            reconciliation_reason = Some("turn_limit_reached".to_string());
                            transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::Reconciliation,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?
                        } else {
                            let session = store
                                .load_result(session_id)
                                .map_err(|error| {
                                    format!(
                                        "failed to load session while building context: {error}"
                                    )
                                })?
                                .ok_or_else(|| {
                                    "session disappeared while building context".to_string()
                                })?;
                            if !session.plan.as_ref().is_some_and(|plan| {
                                active_plan_id.as_deref() == Some(plan.id.as_str())
                                    && plan.is_structured()
                                    && plan.matches_goal(&normalized_goal)
                                    && plan.approved
                            }) {
                                record_plan_binding_conflict(
                                    &store,
                                    session_id,
                                    active_plan_id.as_deref().unwrap_or(""),
                                    &normalized_goal,
                                    "compression",
                                )?;
                                reconciliation_reason = Some("plan_binding_changed".to_string());
                                state = transition_state(
                                    &store,
                                    session_id,
                                    state,
                                    AgentState::Reconciliation,
                                    &mut trace,
                                    &mut transition_count,
                                    &cfg.stream_tx,
                                )
                                .await?;
                                continue;
                            }
                            let current_step = session
                            .plan
                            .as_ref()
                            .and_then(|plan| plan.steps.get(plan.current_step_index))
                            .map(|step| {
                                let mut content =
                                    format!("{}\nStatus: {}", step.description, step.status);
                                if let Some(verification_ids) = verification_recovery.as_ref() {
                                    content.push_str(&format!(
                                        "\nCompletion rejected: {}",
                                        json!({
                                            "reason": "required_verification_unresolved",
                                            "verification_ids": verification_ids,
                                            "instruction": "Continue this step and resolve every listed verification obligation with its exact verification_id before attempting completion again.",
                                        })
                                    ));
                                }
                                content
                            });
                            let bounded = match build_bounded_runtime_input(RuntimePromptRequest {
                                context: &context_sections,
                                session: &session,
                                current_step: current_step.as_deref(),
                                tools: tools_ref,
                                mode: &cfg.mode,
                                project_root: &instruction_root,
                                tool_use_enforcement: nib_cfg.agent.tool_use_enforcement,
                                context_length: nib_cfg.llm.context_length,
                            })
                            .and_then(|bounded| {
                                ensure_required_instructions_present(
                                    &bounded,
                                    &context_sections.agents,
                                )?;
                                Ok(bounded)
                            }) {
                                Ok(bounded) => bounded,
                                Err(error) => {
                                    let error = format!(
                                    "required project instructions cannot fit the runtime request: {error}"
                                );
                                    persist_instruction_context_block(
                                        &store,
                                        session_id,
                                        active_plan_id.as_deref(),
                                        &normalized_goal,
                                        "runtime_prompt_fit",
                                        &error,
                                    )?;
                                    instruction_context_detail =
                                        Some(instruction_context_user_message(&error));
                                    reconciliation_reason =
                                        Some("instruction_context_missing".to_string());
                                    state = transition_state(
                                        &store,
                                        session_id,
                                        state,
                                        AgentState::Reconciliation,
                                        &mut trace,
                                        &mut transition_count,
                                        &cfg.stream_tx,
                                    )
                                    .await?;
                                    continue;
                                }
                            };
                            store
                                .record_event(
                                    session_id,
                                    "context_bounded",
                                    crate::context::snapshot::snapshot_from_bounded_input(
                                        "execution",
                                        "sent",
                                        "configured",
                                        nib_cfg.llm.context_length,
                                        &bounded,
                                    )
                                    .to_event_details(&run_id),
                                )
                                .map_err(|error| error.to_string())?;
                            messages = bounded.messages;
                            llm_tools = bounded.tools;
                            transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::InspectLlm,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?
                        }
                    }
                }
            }
            AgentState::InspectLlm => {
                if llm_turns.saturating_add(1) >= max_turns
                    && !close_steering_admission(
                        steering_enabled,
                        &store,
                        session_id,
                        &run_id,
                        "final_provider_request",
                    )?
                {
                    continue;
                }
                llm_turns += 1;
                verification_recovery = None;
                let continued_turn = provider_continuation.is_some();
                let typed_messages = crate::llm::LlmMessage::from_openai_values(&messages)?;
                let typed_tools =
                    crate::llm::ToolDefinition::from_openai_values_opt(llm_tools.as_deref())?;
                let request = crate::context::snapshot::apply_response_reserve(
                    LlmRequest::new(&typed_messages, typed_tools.as_deref())
                        .with_scope(request_scope.clone())
                        .with_continuation(provider_continuation.take()),
                    nib_cfg.llm.context_length,
                );
                let stream_result = llm.stream(request).await;
                let stream = match stream_result {
                    Ok(stream) => stream,
                    Err(error) => {
                        if continued_turn {
                            record_provider_continuation_lifecycle(
                                &store,
                                session_id,
                                "provider_continuation_abandoned",
                                &run_id,
                            )?;
                        }
                        reconciliation_failure = Some(redact_provider_failure(&nib_cfg, error));
                        reconciliation_reason = Some("llm_stream_failed".to_string());
                        state = transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?;
                        continue;
                    }
                };
                match finish_private_provider_stream(stream, &public_output_sensitive_values).await
                {
                    Ok((response, pending_stream_events)) => {
                        store
                            .record_event(
                                session_id,
                                "context_usage",
                                crate::context::snapshot::usage_event_details(
                                    &run_id,
                                    llm_turns as u64,
                                    response.usage.as_ref(),
                                ),
                            )
                            .map_err(|error| error.to_string())?;
                        if continued_turn {
                            record_provider_continuation_lifecycle(
                                &store,
                                session_id,
                                "provider_continuation_closed",
                                &run_id,
                            )?;
                        }
                        if response.terminal_status == LlmTerminalStatus::Refused {
                            response_content = None;
                            pending_response_events.clear();
                            tool_calls.clear();
                            provider_continuation = None;
                            reconciliation_reason = Some("model_refusal".to_string());
                            transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::Reconciliation,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?
                        } else {
                            response_content = response.content;
                            tool_calls = response.tool_calls.unwrap_or_default();
                            if tool_calls.is_empty() {
                                pending_response_events = pending_stream_events;
                            } else {
                                for event in pending_stream_events {
                                    emit(&cfg.stream_tx, event).await;
                                }
                                pending_response_events.clear();
                            }
                            provider_continuation = response.continuation;
                            if provider_continuation.is_some() {
                                record_provider_continuation_lifecycle(
                                    &store,
                                    session_id,
                                    "provider_continuation_opened",
                                    &run_id,
                                )?;
                            }
                            transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::UpdateMemory,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?
                        }
                    }
                    Err(error) => {
                        if continued_turn {
                            record_provider_continuation_lifecycle(
                                &store,
                                session_id,
                                "provider_continuation_abandoned",
                                &run_id,
                            )?;
                        }
                        reconciliation_failure = Some(redact_provider_failure(&nib_cfg, error));
                        reconciliation_reason = Some("llm_stream_failed".to_string());
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    }
                }
            }
            AgentState::UpdateMemory => {
                if !verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "update_memory",
                )? {
                    pending_response_events.clear();
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }
                if !close_steering_admission(
                    steering_enabled,
                    &store,
                    session_id,
                    &run_id,
                    if tool_calls.is_empty() {
                        "assistant_response_commit"
                    } else {
                        "tool_proposal_commit"
                    },
                )? {
                    continue;
                }
                if tool_calls.is_empty() {
                    revalidate_plan_verification_content(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                    )?;
                    let unresolved_verifications = store
                        .update_session(session_id, |session| {
                            let matches = session.plan.as_ref().is_some_and(|plan| {
                                active_plan_id.as_deref() == Some(plan.id.as_str())
                                    && plan.is_structured()
                                    && plan.matches_goal(&normalized_goal)
                                    && plan.approved
                            });
                            if !matches {
                                return Err(crate::session::SessionError::InvalidMutation(
                                    "completion verification does not match the active approved plan"
                                        .to_string(),
                                ));
                            }
                            let plan = session
                                .plan
                                .as_mut()
                                .expect("plan presence was checked above");
                            let unresolved = plan.unresolved_verification_ids();
                            if !unresolved.is_empty() {
                                plan.record_tool_outcome(
                                    false,
                                    "required verification remains unresolved",
                                );
                                plan.outcome =
                                    Some("required_verification_unresolved".to_string());
                                append_session_event(
                                    session,
                                    "step_completion_rejected",
                                    json!({
                                        "reason": "required_verification_unresolved",
                                        "verification_ids": unresolved,
                                    }),
                                );
                            }
                            Ok(unresolved)
                        })
                        .map_err(|error| error.to_string())?;
                    if !unresolved_verifications.is_empty() {
                        response_content = None;
                        pending_response_events.clear();
                        reconciliation_reason =
                            Some("required_verification_unresolved".to_string());
                        if llm_turns < max_turns {
                            verification_recovery = Some(unresolved_verifications);
                        }
                    } else if let Some(content) = response_content.as_deref() {
                        for event in std::mem::take(&mut pending_response_events) {
                            emit(&cfg.stream_tx, event).await;
                        }
                        let content = safe_persisted_provider_message(
                            content,
                            &public_output_sensitive_values,
                            true,
                        );
                        store
                            .try_append_message_with_origin(
                                session_id,
                                "assistant",
                                &content,
                                MessageOrigin::ModelOutput,
                            )
                            .map_err(|error| error.to_string())?;
                        reconciliation_reason = Some("model_response".to_string());
                    } else {
                        pending_response_events.clear();
                        reconciliation_reason = Some("empty_model_response".to_string());
                    }
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    for event in std::mem::take(&mut pending_response_events) {
                        emit(&cfg.stream_tx, event).await;
                    }
                    let intent = json!({
                        "content": response_content,
                        "tool_calls": tool_calls.iter().map(|call| json!({
                            "invocation_id": call.invocation_id,
                            "name": call.name,
                            "arguments": if call.name == "ask_question" {
                                match crate::tools::executor::validate_registered_tool_arguments(
                                    &call.name,
                                    &call.arguments,
                                ) {
                                    Ok(()) => safe_question_arguments(
                                        &call.arguments,
                                        &public_output_sensitive_values,
                                    ),
                                    Err(_) => json!({"validation": "rejected"}),
                                }
                            } else {
                                call.arguments.clone()
                            },
                        })).collect::<Vec<_>>(),
                    });
                    let persisted_intent = safe_persisted_provider_message(
                        &intent.to_string(),
                        &public_output_sensitive_values,
                        false,
                    );
                    store
                        .try_append_message_with_origin(
                            session_id,
                            "assistant",
                            &persisted_intent,
                            MessageOrigin::ModelOutput,
                        )
                        .map_err(|error| error.to_string())?;
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::UserApproval,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                }
            }
            _ => include!("inner_exec.rs"),
        };
    }

    let final_session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load final session: {error}"))?;
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: run_id.clone(),
        steps_taken: llm_turns.saturating_add(answer_route_requests),
        last_message: if outcome == "instruction_context_missing" {
            instruction_context_detail.or_else(|| {
                final_session
                    .as_ref()
                    .and_then(|session| session.messages.last())
                    .map(|message| message.content.clone())
            })
        } else {
            final_session
                .as_ref()
                .and_then(|session| session.messages.last())
                .map(|message| message.content.clone())
        },
        tool_call_count,
        final_state: state,
        outcome,
        failure: reconciliation_failure,
        bound_reached,
        trace,
    })
}
