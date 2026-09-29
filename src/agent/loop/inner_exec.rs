match state {
            AgentState::UserApproval => {
                preflight_failures.clear();
                executor.project_read_fallback = false;
                executor.prepared_worktree_for_batch = false;
                if !verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "user_approval",
                )? {
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
                let question_count = tool_calls
                    .iter()
                    .filter(|request| request.name == "ask_question")
                    .count();
                if question_count > 0 && tool_calls.len() != 1 {
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::ToolExecute,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }
                let clarification_blockers = store
                    .load_result(session_id)
                    .map_err(|error| error.to_string())?
                    .map(|session| {
                        unresolved_clarification_dependencies(
                            &session,
                            active_plan_id.as_deref(),
                            &instruction_root,
                            &tool_calls,
                        )
                    })
                    .transpose()?
                    .unwrap_or_default();
                if !clarification_blockers.is_empty() {
                    store
                        .record_event(
                            session_id,
                            "tool_batch_rejected",
                            json!({
                                "reason": "unresolved_clarification",
                                "tool_calls": tool_calls.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
                                "clarification_invocation_ids": clarification_blockers,
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                    update_plan_tool_outcome(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                        &normalized_goal,
                        false,
                        "required clarification remains unresolved",
                    )?;
                    tool_calls.clear();
                    response_content = None;
                    reconciliation_reason = Some("unresolved_clarification".to_string());
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
                let batch_requires_worktree = tool_calls.iter().any(|call| {
                    crate::tools::registry::get_tool_metadata(&call.name)
                        .is_some_and(|metadata| metadata.requires_worktree)
                });
                let mut instruction_root_rebound = false;
                if batch_requires_worktree {
                    let managed_root = match executor.prepare_session_worktree(session_id).await {
                        Ok(root) => {
                            executor.prepared_worktree_for_batch = true;
                            root
                        },
                        Err(error) => {
                            executor.project_read_fallback = true;
                            store
                                .record_event(
                                    session_id,
                                    "local_preflight_failed",
                                    json!({
                                        "stage": "managed_worktree",
                                        "category": worktree_preflight_category(&error),
                                        "project_read_fallback": true,
                                        "run_id": run_id,
                                    }),
                                )
                                .map_err(|error| error.to_string())?;
                            for call in &tool_calls {
                                if crate::tools::registry::get_tool_metadata(&call.name)
                                    .is_some_and(|metadata| metadata.requires_worktree)
                                {
                                    preflight_failures.insert(
                                        call.invocation_id,
                                        WORKTREE_PREFLIGHT_MESSAGE,
                                    );
                                }
                            }
                            instruction_root.clone()
                        }
                    };
                    if managed_root != instruction_root {
                        let translated_scopes = instruction_scopes
                            .iter()
                            .map(|scope| {
                                scope
                                    .strip_prefix(&instruction_root)
                                    .map(|relative| managed_root.join(relative))
                                    .map_err(|_| {
                                        "instruction scope cannot be rebound to the managed session worktree"
                                            .to_string()
                                    })
                        })
                            .collect::<Result<Vec<_>, _>>()?;
                        instruction_root = managed_root;
                        instruction_scopes = translated_scopes;
                        instruction_root_rebound = true;
                        instruction_resolver = match InstructionResolver::new(&instruction_root) {
                            Ok(resolver) => resolver,
                            Err(error) => {
                                let error = format!(
                                    "required project instructions are unavailable in the managed session worktree: {error}"
                                );
                                persist_instruction_context_block(
                                    &store,
                                    session_id,
                                    active_plan_id.as_deref(),
                                    &normalized_goal,
                                    "managed_worktree_resolution",
                                    &error,
                                )?;
                                instruction_context_detail =
                                    Some(instruction_context_user_message(&error));
                                tool_calls.clear();
                                response_content = None;
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
                    }
                }
                let mut proposed_scopes = instruction_scopes.clone();
                for call in &tool_calls {
                    if preflight_failures.contains_key(&call.invocation_id) {
                        continue;
                    }
                    let call_scopes = match tool_instruction_scopes(
                        &instruction_root,
                        &call.name,
                        &call.arguments,
                    ) {
                        Ok(scopes) => scopes,
                        Err(error) => {
                            preflight_failures.insert(
                                call.invocation_id,
                                tool_scope_preflight_message(&error),
                            );
                            continue;
                        }
                    };
                    let mut candidate_scopes = proposed_scopes.clone();
                    candidate_scopes.extend(call_scopes);
                    candidate_scopes.sort();
                    candidate_scopes.dedup();
                    match instruction_resolver.resolve_for_scopes(candidate_scopes.iter()) {
                        Ok(_) => proposed_scopes = candidate_scopes,
                        Err(error) => {
                            preflight_failures.insert(
                                call.invocation_id,
                                tool_scope_preflight_message(&error),
                            );
                        }
                    }
                }
                let scoped_resolution = instruction_resolver.resolve_for_scopes(proposed_scopes.iter());
                let resolved = match scoped_resolution {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let error = format!(
                            "required project instructions are unavailable for the proposed tool scope: {error}"
                        );
                        persist_instruction_context_block(
                            &store,
                            session_id,
                            active_plan_id.as_deref(),
                            &normalized_goal,
                            "tool_scope",
                            &error,
                        )?;
                        instruction_context_detail = Some(instruction_context_user_message(&error));
                        if provider_continuation.take().is_some() {
                            record_provider_continuation_lifecycle(
                                &store,
                                session_id,
                                "provider_continuation_abandoned",
                                &run_id,
                            )?;
                        }
                        let observations = tool_calls
                            .iter()
                            .map(|call| {
                                json!({
                                    "invocation_id": call.invocation_id,
                                    "tool": call.name,
                                    "success": false,
                                    "error": error,
                                })
                            })
                            .collect::<Vec<_>>();
                        store
                            .try_append_message_with_origin(
                                session_id,
                                "tool",
                                &json!({"observations": observations}).to_string(),
                                MessageOrigin::ToolOutput,
                            )
                            .map_err(|append_error| append_error.to_string())?;
                        tool_calls.clear();
                        response_content = None;
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
                derive_active_plan_verification_requirements(
                    &store,
                    session_id,
                    goal,
                    &resolved.render(),
                )?;
                if resolved.identity != instruction_identity {
                    if provider_continuation.take().is_some() {
                        record_provider_continuation_lifecycle(
                            &store,
                            session_id,
                            "provider_continuation_abandoned",
                            &run_id,
                        )?;
                    }
                    let previous_identity =
                        std::mem::replace(&mut instruction_identity, resolved.identity.clone());
                    context_sections.agents = resolved.render();
                    instruction_scopes = proposed_scopes;
                    let explanation =
                        "applicable project instructions were refreshed before execution; reconsider the proposed action under the updated scoped rules";
                    let observations = tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "invocation_id": call.invocation_id,
                                "tool": call.name,
                                "success": false,
                                "error": explanation,
                            })
                        })
                        .collect::<Vec<_>>();
                    store
                        .try_append_message_with_origin(
                            session_id,
                            "tool",
                            &json!({"observations": observations}).to_string(),
                            MessageOrigin::ToolOutput,
                        )
                        .map_err(|error| error.to_string())?;
                    store
                        .record_event(
                            session_id,
                            "instruction_context_refreshed",
                            json!({
                                "previous_identity": previous_identity,
                                "identity": instruction_identity,
                                "scope_count": instruction_scopes.len(),
                                "execution_deferred": true,
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                    tool_calls.clear();
                    response_content = None;
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
                if instruction_root_rebound {
                    context_sections.agents = resolved.render();
                    store
                        .record_event(
                            session_id,
                            "instruction_context_rebound",
                            json!({
                                "identity": instruction_identity,
                                "scope_count": proposed_scopes.len(),
                                "instruction_root": instruction_root,
                                "execution_deferred": false,
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                }
                instruction_scopes = proposed_scopes;
                for call in &tool_calls {
                    let tool_call = ToolCall {
                        invocation_id: call.invocation_id,
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        session_id: Some(session_id.to_string()),
                        project_root: Some(project_root.clone()),
                    };
                    if executor.requires_interactive_approval(&tool_call) {
                        emit(
                            &cfg.stream_tx,
                            StreamEvent::ApprovalRequired {
                                tool_name: call.name.clone(),
                            },
                        )
                        .await;
                        store
                            .record_event(
                                session_id,
                                "approval_required",
                                json!({"kind": "tool", "invocation_id": call.invocation_id, "tool_name": call.name}),
                            )
                            .map_err(|error| error.to_string())?;
                    }
                }
                transition_state(
                    &store,
                    session_id,
                    state,
                    AgentState::ToolExecute,
                    &mut trace,
                    &mut transition_count,
                    &cfg.stream_tx,
                )
                .await?
            }
            AgentState::ToolExecute => {
                if !verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "tool_execute",
                )? {
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
                let question_count = tool_calls
                    .iter()
                    .filter(|request| request.name == "ask_question")
                    .count();
                if question_count > 0 && tool_calls.len() != 1 {
                    let error = "ask_question must be the only tool call in its batch";
                    let observations = tool_calls
                        .iter()
                        .map(|request| {
                            json!({
                                "invocation_id": request.invocation_id,
                                "tool": request.name,
                                "success": false,
                                "output": Value::Null,
                                "error": error,
                            })
                        })
                        .collect::<Vec<_>>();
                    let classifications = vec![ToolResultClass::Error; tool_calls.len()];
                    let continuation_failure = record_provider_tool_outputs(
                        &mut provider_continuation,
                        &tool_calls,
                        &observations,
                        &classifications,
                    )
                    .err();
                    for request in &tool_calls {
                        emit(
                            &cfg.stream_tx,
                            StreamEvent::ToolCompleted {
                                invocation_id: request.invocation_id,
                                tool_name: request.name.clone(),
                                success: false,
                                output: None,
                                error: Some(error.to_string()),
                            },
                        )
                        .await;
                    }
                    store
                        .record_event(
                            session_id,
                            "tool_batch_rejected",
                            json!({
                                "reason": "mixed_question_batch",
                                "tool_calls": tool_calls.iter().map(|call| json!({
                                    "invocation_id": call.invocation_id,
                                    "name": call.name,
                                })).collect::<Vec<_>>(),
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                    store
                        .try_append_message_with_origin(
                            session_id,
                            "tool",
                            &json!({"observations": observations}).to_string(),
                            MessageOrigin::ToolOutput,
                        )
                        .map_err(|error| error.to_string())?;
                    let stalled = failed_tool_batches.observe(&tool_calls, &observations, false);
                    let plan_updated = update_plan_tool_outcome(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                        &normalized_goal,
                        false,
                        error,
                    )?;
                    tool_calls.clear();
                    response_content = None;
                    if continuation_failure.is_none()
                        && plan_updated
                        && !stalled
                        && llm_turns < max_turns
                    {
                        open_steering_admission(
                            steering_enabled,
                            &store,
                            session_id,
                            &run_id,
                            "rejected_tool_batch_build_context",
                        )?;
                    }
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        if continuation_failure.is_some() || !plan_updated || stalled {
                            AgentState::Reconciliation
                        } else {
                            AgentState::BuildContext
                        },
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    if let Some(error) = continuation_failure {
                        reconciliation_failure = Some(redact_provider_failure(
                            &nib_cfg,
                            LlmError::local(
                                LlmErrorClass::Protocol,
                                LlmErrorPhase::Continuation,
                                error,
                            ),
                        ));
                        reconciliation_reason = Some("provider_continuation_failed".to_string());
                    } else if !plan_updated {
                        reconciliation_reason = Some("plan_binding_changed".to_string());
                    } else if stalled {
                        record_repeated_tool_failure(&store, session_id, &run_id)?;
                        reconciliation_reason = Some("repeated_tool_failure".to_string());
                    }
                    continue;
                }

                let mut observations = Vec::new();
                let mut classifications = Vec::new();
                let mut batch_success = true;
                let mut batch_denied = false;
                let mut prepared_tasks = PreparedTaskBatch::default();
                for request in &tool_calls {
                    if let Some(message) = preflight_failures.get(&request.invocation_id) {
                        let category = if *message == WORKTREE_PREFLIGHT_MESSAGE {
                            "managed_worktree"
                        } else if *message == OUTSIDE_WORKTREE_MESSAGE {
                            "outside_worktree"
                        } else {
                            "instruction_scope"
                        };
                        store
                            .record_event(
                                session_id,
                                "tool_preflight_rejected",
                                json!({
                                    "invocation_id": request.invocation_id,
                                    "tool_name": request.name,
                                    "category": category,
                                }),
                            )
                            .map_err(|error| error.to_string())?;
                        if category == "instruction_scope" {
                            store
                                .record_event(
                                    session_id,
                                    "instruction_context_missing",
                                    json!({
                                        "stage": "tool_scope",
                                        "invocation_id": request.invocation_id,
                                        "category": category,
                                    }),
                                )
                                .map_err(|error| error.to_string())?;
                        }
                        emit(
                            &cfg.stream_tx,
                            StreamEvent::ToolCompleted {
                                invocation_id: request.invocation_id,
                                tool_name: request.name.clone(),
                                success: false,
                                output: None,
                                error: Some((*message).to_string()),
                            },
                        )
                        .await;
                        store
                            .record_event(
                                session_id,
                                "tool_completed",
                                json!({
                                    "invocation_id": request.invocation_id,
                                    "tool_name": request.name,
                                    "success": false,
                                    "output": Value::Null,
                                    "error": message,
                                }),
                            )
                            .map_err(|error| error.to_string())?;
                        observations.push(json!({
                            "invocation_id": request.invocation_id,
                            "tool": request.name,
                            "success": false,
                            "output": Value::Null,
                            "error": message,
                        }));
                        classifications.push(ToolResultClass::Error);
                        batch_success = false;
                        continue;
                    }
                    if request.name != "ask_question" {
                        record_tool_started_and_open_steering(
                            &store,
                            session_id,
                            &run_id,
                            request,
                            llm_turns < max_turns,
                        )?;
                    } else {
                        store
                            .record_event(
                                session_id,
                                "tool_started",
                                json!({"invocation_id": request.invocation_id, "tool_name": request.name}),
                            )
                            .map_err(|error| error.to_string())?;
                    }
                    if !request.arguments.is_null() {
                        emit(
                            &cfg.stream_tx,
                            StreamEvent::ToolCallChunk {
                                invocation_id: request.invocation_id,
                                index: 0,
                                name: Some(request.name.clone()),
                                arguments: Some(request.arguments.to_string()),
                            },
                        )
                        .await;
                    }
                    emit(
                        &cfg.stream_tx,
                        StreamEvent::ToolStarted {
                            invocation_id: request.invocation_id,
                            tool_name: request.name.clone(),
                        },
                    )
                    .await;
                    if request.name == "ask_question"
                        && crate::tools::executor::validate_registered_tool_arguments(
                            &request.name,
                            &request.arguments,
                        )
                        .is_ok()
                    {
                        pending_question = Some(request.clone());
                        continue;
                    }
                    resources.record_tool_attempt();
                    let verification_id = request
                        .arguments
                        .get("verification_id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|id| !id.is_empty());
                    let verification_started = match verification_id {
                        Some(obligation_id) => match begin_plan_verification(
                            &store,
                            session_id,
                            active_plan_id.as_deref(),
                            &normalized_goal,
                            obligation_id,
                            request,
                        ) {
                            Ok(()) => true,
                            Err(error) => {
                                store
                                    .record_event(
                                        session_id,
                                        "verification_binding_rejected",
                                        json!({
                                            "invocation_id": request.invocation_id,
                                            "verification_id": obligation_id,
                                            "reason": error,
                                        }),
                                    )
                                    .map_err(|error| error.to_string())?;
                                false
                            }
                        },
                        None => false,
                    };
                    let result = if verification_id.is_some() && !verification_started {
                        crate::tools::ToolResult {
                            invocation_id: request.invocation_id,
                            tool_name: request.name.clone(),
                            success: false,
                            output: None,
                            error: Some(
                                "verification binding was rejected before execution".to_string(),
                            ),
                            duration_seconds: 0.0,
                            approval_granted: false,
                            approval_source: Some("verification".to_string()),
                        }
                    } else {
                        executor
                            .execute(
                                ToolCall {
                                    invocation_id: request.invocation_id,
                                    tool_name: request.name.clone(),
                                    arguments: request.arguments.clone(),
                                    session_id: Some(session_id.to_string()),
                                    project_root: Some(project_root.clone()),
                                },
                                Some(session_id),
                            )
                            .await
                    };
                    tool_call_count += 1;
                    let (mutated_content, mut worktree_identity) =
                        if result.success || verification_started {
                            audited_tool_evidence(&store, session_id, request.invocation_id)?
                        } else {
                            (false, None)
                        };
                    if verification_started && worktree_identity.is_none() {
                        worktree_identity = Some(project_root.to_string_lossy().into_owned());
                    }
                    if mutated_content {
                        invalidate_plan_verification_after_mutation(
                            &store,
                            session_id,
                            active_plan_id.as_deref(),
                            &normalized_goal,
                            request.invocation_id,
                        )?;
                    }
                    if verification_started {
                        finish_plan_verification(
                            &store,
                            session_id,
                            active_plan_id.as_deref(),
                            &normalized_goal,
                            verification_id.expect("started verification has an id"),
                            worktree_identity.as_deref(),
                            &result,
                        )?;
                    }
                    batch_success &= result.success;
                    batch_denied |= !result.approval_granted
                        && (result.approval_source.as_deref() == Some("denied")
                            || (result.approval_source.as_deref() == Some("policy")
                                && result.error.as_deref() == Some("Approval denied")));
                    let prepared_work = request.name == "schedule"
                        || (request.name == "run_terminal"
                            && request
                                .arguments
                                .get("background")
                                .and_then(Value::as_bool)
                                .unwrap_or(false));
                    if result.success && prepared_work {
                        if let Some(task_id) = result
                            .output
                            .as_ref()
                            .and_then(|output| output.get("task_id"))
                            .and_then(Value::as_str)
                        {
                            prepared_tasks.track(task_id.to_string(), request.name.clone());
                        }
                    }
                    let output = result.output.clone();
                    let error = result.error.clone();
                    emit(
                        &cfg.stream_tx,
                        StreamEvent::ToolCompleted {
                            invocation_id: request.invocation_id,
                            tool_name: request.name.clone(),
                            success: result.success,
                            output: output.clone(),
                            error: error.clone(),
                        },
                    )
                    .await;
                    store
                        .record_event(
                            session_id,
                            "tool_completed",
                            json!({
                                "invocation_id": request.invocation_id,
                                "tool_name": request.name,
                                "success": result.success,
                                "output": output,
                                "error": error,
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                    observations.push(json!({
                        "invocation_id": request.invocation_id,
                        "tool": request.name,
                        "success": result.success,
                        "output": result.output,
                        "error": result.error,
                    }));
                    classifications.push(ToolResultClass::from_success(result.success));
                }
                let continuation_failure = if pending_question.is_none() {
                    record_provider_tool_outputs(
                        &mut provider_continuation,
                        &tool_calls,
                        &observations,
                        &classifications,
                    )
                    .err()
                } else {
                    None
                };
                if let Some(error) = continuation_failure {
                    store
                        .try_append_message_with_origin(
                            session_id,
                            "tool",
                            &json!({"observations": observations}).to_string(),
                            MessageOrigin::ToolOutput,
                        )
                        .map_err(|error| error.to_string())?;
                    tool_calls.clear();
                    response_content = None;
                    reconciliation_failure = Some(redact_provider_failure(
                        &nib_cfg,
                        LlmError::local(
                            LlmErrorClass::Protocol,
                            LlmErrorPhase::Continuation,
                            error,
                        ),
                    ));
                    reconciliation_reason = Some("provider_continuation_failed".to_string());
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
                } else if pending_question.is_some() {
                    pending_observations = observations;
                    pending_batch_success = batch_success;
                    tool_calls.clear();
                    response_content = None;
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::WaitingForUserInput,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    store
                        .try_append_message_with_origin(
                            session_id,
                            "tool",
                            &json!({"observations": observations}).to_string(),
                            MessageOrigin::ToolOutput,
                        )
                        .map_err(|error| error.to_string())?;
                    for (task, error) in prepared_tasks.start_all() {
                        store
                            .record_event(
                                session_id,
                                "prepared_task_start_failed",
                                json!({
                                    "task_id": task.id,
                                    "tool_name": task.tool_name,
                                    "error": error,
                                }),
                            )
                            .map_err(|error| error.to_string())?;
                        batch_success = false;
                    }
                    let all_instruction_scopes_unavailable = !tool_calls.is_empty()
                        && preflight_failures.len() == tool_calls.len()
                        && preflight_failures
                            .values()
                            .all(|message| *message == INSTRUCTION_SCOPE_MESSAGE);
                    let tool_outcome = if batch_success {
                        "tool batch succeeded"
                    } else if all_instruction_scopes_unavailable {
                        "required project instructions are unavailable for the proposed tool scope"
                    } else {
                        "one or more tools failed"
                    };
                    let plan_updated = update_plan_tool_outcome(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                        &normalized_goal,
                        batch_success,
                        tool_outcome,
                    )?;
                    let stalled =
                        failed_tool_batches.observe(&tool_calls, &observations, batch_success);
                    let preflight_outcome = if !tool_calls.is_empty()
                        && preflight_failures.len() == tool_calls.len()
                    {
                        Some(if preflight_failures
                            .values()
                            .any(|message| *message == WORKTREE_PREFLIGHT_MESSAGE)
                        {
                            "worktree_preparation_failed"
                        } else if preflight_failures
                            .values()
                            .any(|message| *message == OUTSIDE_WORKTREE_MESSAGE)
                        {
                            "tool_scope_outside_worktree"
                        } else {
                            "instruction_context_missing"
                        })
                    } else {
                        None
                    };
                    tool_calls.clear();
                    response_content = None;
                    if !plan_updated {
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
                    } else if let Some(preflight_outcome) = preflight_outcome {
                        reconciliation_reason = Some(preflight_outcome.to_string());
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
                    } else if batch_denied || stalled {
                        if stalled && !batch_denied {
                            record_repeated_tool_failure(&store, session_id, &run_id)?;
                        }
                        reconciliation_reason = Some(
                            if batch_denied {
                                "tool_execution_failed"
                            } else {
                                "repeated_tool_failure"
                            }
                            .to_string(),
                        );
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
                        if llm_turns < max_turns {
                            open_steering_admission(
                                steering_enabled,
                                &store,
                                session_id,
                                &run_id,
                                "completed_tool_batch_build_context",
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
            AgentState::WaitingForUserInput => {
                if !verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "waiting_for_user_input",
                )? {
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
                let request = pending_question
                    .take()
                    .ok_or_else(|| "waiting-for-input state has no pending question".to_string())?;
                crate::tools::executor::validate_registered_tool_arguments(
                    &request.name,
                    &request.arguments,
                )?;
                let raw_question = request
                    .arguments
                    .get("question")
                    .and_then(Value::as_str)
                    .filter(|question| !question.trim().is_empty())
                    .ok_or_else(|| "ask_question requires a non-empty question".to_string())?;
                let question = crate::interactive::bounded_public_text(
                    raw_question,
                    &public_output_sensitive_values,
                    MAX_QUESTION_BYTES,
                    false,
                );
                let proposed_answer = request
                    .arguments
                    .get("proposed_answer")
                    .and_then(Value::as_str)
                    .filter(|answer| !answer.trim().is_empty())
                    .map(|answer| {
                        crate::interactive::bounded_public_text(
                            answer,
                            &public_output_sensitive_values,
                            MAX_QUESTION_BYTES,
                            false,
                        )
                    });
                resources.observe_question(&question);
                let options = request
                    .arguments
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|option| {
                                crate::interactive::bounded_public_text(
                                    option,
                                    &public_output_sensitive_values,
                                    MAX_QUESTION_OPTION_BYTES,
                                    false,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let dependent_paths = bounded_clarification_dependency_paths(&request.arguments)?;
                let reused = persist_question_required(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    request.invocation_id,
                    ClarificationPrompt {
                        question: &question,
                        proposed_answer: proposed_answer.as_deref(),
                        options: &options,
                        dependent_paths: &dependent_paths,
                    },
                )?;
                if reused.is_none() {
                    emit(
                        &cfg.stream_tx,
                        StreamEvent::QuestionRequired {
                            question: question.clone(),
                            options: options.clone(),
                        },
                    )
                    .await;
                }

                let reused_answer = reused.is_some();
                let outcome = match reused {
                    Some(prior) => QuestionOutcome::Answered(prior.answer),
                    None => match cfg.question_handler.as_ref() {
                        Some(handler) => {
                            handler
                                .ask_with_context(QuestionRequestContext {
                                    invocation_id: request.invocation_id,
                                    question: &question,
                                    proposed_answer: proposed_answer.as_deref(),
                                    options: &options,
                                })
                                .await
                        }
                        None => QuestionOutcome::InputUnavailable(
                            "no question handler configured".to_string(),
                        ),
                    },
                };
                let outcome = match outcome {
                    QuestionOutcome::Answered(answer) => {
                        let answer = crate::interactive::bounded_public_text(
                            &answer,
                            &public_output_sensitive_values,
                            MAX_QUESTION_BYTES,
                            false,
                        );
                        if answer.trim().is_empty() {
                            QuestionOutcome::InputUnavailable(
                                "question handler returned an empty answer".to_string(),
                            )
                        } else {
                            QuestionOutcome::Answered(answer)
                        }
                    }
                    QuestionOutcome::ApprovedProposal(answer) => {
                        let answer = crate::interactive::bounded_public_text(
                            &answer,
                            &public_output_sensitive_values,
                            MAX_QUESTION_BYTES,
                            false,
                        );
                        if answer.trim().is_empty()
                            || proposed_answer.as_deref() != Some(answer.as_str())
                        {
                            QuestionOutcome::InputUnavailable(
                                "approved proposal did not match the displayed answer".to_string(),
                            )
                        } else {
                            QuestionOutcome::ApprovedProposal(answer)
                        }
                    }
                    QuestionOutcome::InputUnavailable(error) => {
                        QuestionOutcome::InputUnavailable(crate::interactive::bounded_public_text(
                            &error,
                            &public_output_sensitive_values,
                            MAX_QUESTION_BYTES,
                            false,
                        ))
                    }
                    other => other,
                };
                let answer = match &outcome {
                    QuestionOutcome::Answered(answer)
                    | QuestionOutcome::ApprovedProposal(answer) => Ok(answer.clone()),
                    QuestionOutcome::LeftUnanswered => Err("left unanswered".to_string()),
                    QuestionOutcome::Cancelled => Err("cancelled".to_string()),
                    QuestionOutcome::InputClosed => Err("input closed".to_string()),
                    QuestionOutcome::InputUnavailable(error) => Err(error.clone()),
                };

                let arguments = safe_question_execution_arguments(
                    &request.arguments,
                    &answer,
                    &public_output_sensitive_values,
                );
                resources.record_tool_attempt();
                let result = executor
                    .execute(
                        ToolCall {
                            invocation_id: request.invocation_id,
                            tool_name: request.name.clone(),
                            arguments,
                            session_id: Some(session_id.to_string()),
                            project_root: Some(project_root.clone()),
                        },
                        Some(session_id),
                    )
                    .await;
                tool_call_count += 1;
                let (question_success, question_output, question_error) = match &answer {
                    Ok(answer) if result.success => (
                        true,
                        Some(json!({"question": question, "answer": answer})),
                        None,
                    ),
                    Ok(_) => (false, result.output.clone(), result.error.clone()),
                    Err(error) => (false, result.output.clone(), Some(error.clone())),
                };
                emit(
                    &cfg.stream_tx,
                    StreamEvent::ToolCompleted {
                        invocation_id: request.invocation_id,
                        tool_name: request.name.clone(),
                        success: question_success,
                        output: question_output.clone(),
                        error: question_error.clone(),
                    },
                )
                .await;
                store
                    .record_event(
                        session_id,
                        "tool_completed",
                        json!({
                            "invocation_id": request.invocation_id,
                            "tool_name": request.name,
                            "success": question_success,
                            "output": question_output,
                            "error": question_error,
                        }),
                    )
                    .map_err(|error| error.to_string())?;
                let question_observation = json!({
                    "invocation_id": request.invocation_id,
                    "tool": request.name,
                    "success": question_success,
                    "output": question_output,
                    "error": question_error,
                });
                let continuation_failure = record_provider_tool_output(
                    &mut provider_continuation,
                    &request,
                    &question_observation,
                    ToolResultClass::from_success(question_success),
                )
                .err();
                pending_observations.push(question_observation);
                let batch_success = pending_batch_success && question_success;
                if batch_success {
                    failed_tool_batches.reset();
                }
                persist_question_observation(
                    &store,
                    session_id,
                    request.invocation_id,
                    &pending_observations,
                    &outcome,
                    reused_answer,
                )?;
                let plan_updated = update_plan_tool_outcome(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    batch_success,
                    if batch_success {
                        "question answered"
                    } else {
                        "question was not answered"
                    },
                )?;
                pending_observations.clear();
                pending_batch_success = true;
                if let Some(error) = continuation_failure {
                    reconciliation_failure = Some(redact_provider_failure(
                        &nib_cfg,
                        LlmError::local(
                            LlmErrorClass::Protocol,
                            LlmErrorPhase::Continuation,
                            error,
                        ),
                    ));
                    reconciliation_reason = Some("provider_continuation_failed".to_string());
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
                } else if !plan_updated {
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
                } else if batch_success {
                    if llm_turns < max_turns {
                        open_steering_admission(
                            steering_enabled,
                            &store,
                            session_id,
                            &run_id,
                            "answered_question_build_context",
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
                    reconciliation_reason = Some("waiting_for_user_input".to_string());
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
            AgentState::Reconciliation => {
                if provider_continuation.take().is_some() {
                    record_provider_continuation_lifecycle(
                        &store,
                        session_id,
                        "provider_continuation_abandoned",
                        &run_id,
                    )?;
                }
                let reason = reconciliation_reason
                    .take()
                    .unwrap_or_else(|| "reconciled".to_string());
                if reason == "plan_ready"
                    && !close_steering_admission(
                        steering_enabled,
                        &store,
                        session_id,
                        &run_id,
                        "plan_ready_commit",
                    )?
                {
                    reconciliation_reason = Some(reason);
                    continue;
                }
                let mut continue_plan = false;
                outcome = match reason.as_str() {
                    "model_response" => {
                        let safe_plan_outcome = safe_provider_plan_outcome(
                            response_content.as_deref(),
                            &public_output_sensitive_values,
                        );
                        let (binding_matches, blocked, should_continue, next_step) = store
                            .update_session(session_id, |session| {
                                let binding_matches = session.plan.as_ref().is_some_and(|plan| {
                                    active_plan_id.as_deref() == Some(plan.id.as_str())
                                        && plan.is_structured()
                                        && plan.matches_goal(&normalized_goal)
                                        && plan.approved
                                });
                                if !binding_matches {
                                    let current_plan_id =
                                        session.plan.as_ref().map(|plan| plan.id.clone());
                                    let current_goal =
                                        session.plan.as_ref().map(|plan| plan.goal.clone());
                                    append_session_event(
                                        session,
                                        "plan_binding_conflict",
                                        json!({
                                            "stage": "reconciliation",
                                            "expected_plan_id": active_plan_id,
                                            "expected_goal": normalized_goal,
                                            "current_plan_id": current_plan_id,
                                            "current_goal": current_goal,
                                        }),
                                    );
                                    return Ok((false, false, false, None));
                                }
                                let mut next_step = None;
                                let mut should_continue = false;
                                if let Some(plan) = session.plan.as_mut() {
                                    if plan
                                        .steps
                                        .get(plan.current_step_index)
                                        .is_some_and(|step| step.status == "Blocked")
                                    {
                                        plan.outcome = Some("blocked_step_unresolved".to_string());
                                        append_session_event(
                                            session,
                                            "step_completion_rejected",
                                            json!({"reason": "blocked_step_unresolved"}),
                                        );
                                        return Ok((true, true, false, None));
                                    }
                                    plan.complete_current_step(&safe_plan_outcome);
                                    should_continue = !plan.is_complete();
                                    next_step = plan
                                        .steps
                                        .get(plan.current_step_index)
                                        .map(|step| step.description.clone());
                                }
                                Ok((true, false, should_continue, next_step))
                            })
                            .map_err(|error| error.to_string())?;
                        if !binding_matches {
                            continue_plan = false;
                            "plan_binding_changed".to_string()
                        } else if blocked {
                            continue_plan = false;
                            "blocked_step_unresolved".to_string()
                        } else if should_continue {
                            continue_plan = true;
                            let next_step =
                                next_step.as_deref().unwrap_or("the next approved step");
                            store
                                .try_append_message_with_origin(
                                    session_id,
                                    "user",
                                    &format!("Continue with approved plan step: {next_step}"),
                                    MessageOrigin::RuntimeContinuation,
                                )
                                .map_err(|error| error.to_string())?;
                            "step_completed".to_string()
                        } else {
                            continue_plan = false;
                            "completed".to_string()
                        }
                    }
                    "plan_ready" => {
                        append_assistant_if_allowed(
                            &store,
                            session_id,
                            "Structured plan generated.",
                        )?;
                        "plan_ready".to_string()
                    }
                    "plan_approval_denied" => {
                        append_assistant_if_allowed(
                            &store,
                            session_id,
                            "Plan approval denied; no tools were executed.",
                        )?;
                        "plan_approval_denied".to_string()
                    }
                    "required_verification_unresolved" if llm_turns < max_turns => {
                        continue_plan = true;
                        "verification_recovery".to_string()
                    }
                    other => {
                        if is_agent_failure_outcome(other)
                            && !matches!(
                                other,
                                "instruction_context_missing" | "unresolved_clarification"
                            )
                        {
                            block_active_plan_for_failure(
                                &store,
                                session_id,
                                active_plan_id.as_deref(),
                                &normalized_goal,
                                other,
                            )?;
                        }
                        if reconciliation_failure.is_none()
                            && other != "worktree_preparation_failed"
                        {
                            let message = if other == "instruction_context_missing" {
                                instruction_context_detail.clone().unwrap_or_else(|| {
                                    instruction_context_user_message(
                                        "required project instructions are unavailable",
                                    )
                                })
                            } else {
                                format!("Run reconciled with outcome: {other}")
                            };
                            append_assistant_if_allowed(&store, session_id, &message)?;
                        }
                        other.to_string()
                    }
                };
                let failure_details = reconciliation_failure.clone();
                let mut reconciliation_details = json!({
                    "outcome": outcome,
                    "continue": continue_plan,
                    "failure": failure_details,
                });
                if outcome == "instruction_context_missing" {
                    reconciliation_details["reason"] =
                        json!(instruction_context_detail.clone().unwrap_or_else(|| {
                            instruction_context_user_message(
                                "required project instructions are unavailable",
                            )
                        }));
                }
                store
                    .record_event(session_id, "reconciliation", reconciliation_details)
                    .map_err(|error| error.to_string())?;
                emit_plan_progress(
                    &store,
                    session_id,
                    &cfg.stream_tx,
                    &public_output_sensitive_values,
                )?;
                emit(
                    &cfg.stream_tx,
                    StreamEvent::Reconciled {
                        outcome: outcome.clone(),
                    },
                )
                .await;
                response_content = None;
                if continue_plan && llm_turns < max_turns {
                    open_steering_admission(
                        steering_enabled,
                        &store,
                        session_id,
                        &run_id,
                        "continued_plan_build_context",
                    )?;
                }
                transition_state(
                    &store,
                    session_id,
                    state,
                    if continue_plan {
                        AgentState::BuildContext
                    } else {
                        AgentState::Done
                    },
                    &mut trace,
                    &mut transition_count,
                    &cfg.stream_tx,
                )
                .await?
            }
            AgentState::Done => AgentState::Done,

            _ => unreachable!("front-half states handled by run_agent_loop_inner"),
        }
