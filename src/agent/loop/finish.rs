//! Agent loop internals.

use super::*;

pub(crate) enum AnswerOnlyRoute {
    Completed(AgentRunSummary),
    Failed(AgentRunSummary),
    Fallback,
}

pub(crate) fn has_unterminated_prior_run(session: &Session, current_run_id: &str) -> bool {
    let terminal = session
        .events
        .iter()
        .filter(|event| event.kind == "run_terminal")
        .filter_map(|event| event.details.get("run_id").and_then(Value::as_str))
        .collect::<std::collections::BTreeSet<_>>();
    session.events.iter().any(|event| {
        if event.kind != "run_started" {
            return false;
        }
        event
            .details
            .get("run_id")
            .and_then(Value::as_str)
            .is_some_and(|run_id| run_id != current_run_id && !terminal.contains(run_id))
    })
}

#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn run_answer_only_route(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    project_root: &Path,
    context: &crate::context::RuntimeContextSections,
    llm: &Arc<dyn LlmClient>,
    config: &crate::config::NibConfig,
    sensitive_values: &[String],
    request_scope: &LlmRequestScope,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AnswerOnlyRoute, String> {
    store
        .record_event(
            session_id,
            "answer_route_started",
            json!({"run_id": run_id, "route": "answer_only"}),
        )
        .map_err(|error| format!("failed to audit answer-only route start: {error}"))?;

    let control = json!({
        "type": "function",
        "function": {
            "name": "request_plan",
            "description": "Request the normal approved planning path because the current request needs inspection, clarification, or action.",
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "strict": true
        }
    });
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load answer-only session context: {error}"))?
        .ok_or_else(|| "session disappeared before answer-only context assembly".to_string())?;
    let bounded = match build_bounded_runtime_input(RuntimePromptRequest {
        context,
        session: &session,
        current_step: None,
        tools: Some(std::slice::from_ref(&control)),
        mode: "answer_only",
        project_root,
        tool_use_enforcement: false,
        context_length: config.llm.context_length,
    }) {
        Ok(bounded) => bounded,
        Err(error) => {
            let failure = LlmError::local(LlmErrorClass::Protocol, LlmErrorPhase::Request, error);
            return finish_answer_only_failure(
                store,
                session_id,
                run_id,
                stream_tx,
                failure,
                "context_rejected",
            )
            .await
            .map(AnswerOnlyRoute::Failed);
        }
    };
    let mut details = crate::context::snapshot::snapshot_from_bounded_input(
        "answer_only",
        "sent",
        "configured",
        config.llm.context_length,
        &bounded,
    )
    .to_event_details(run_id);
    details["route"] = json!("answer_only");
    details["context_length"] = json!(config.llm.context_length);
    details["approximate_input_tokens"] = json!(bounded.approximate_tokens);
    store
        .record_event(session_id, "context_bounded", details)
        .map_err(|error| error.to_string())?;
    let typed_messages = match crate::llm::LlmMessage::from_openai_values(&bounded.messages) {
        Ok(messages) => messages,
        Err(error) => {
            return finish_answer_only_failure(
                store,
                session_id,
                run_id,
                stream_tx,
                LlmError::local(LlmErrorClass::Protocol, LlmErrorPhase::Request, error),
                "request_rejected",
            )
            .await
            .map(AnswerOnlyRoute::Failed)
        }
    };
    let mut typed_tools =
        match crate::llm::ToolDefinition::from_openai_values_opt(bounded.tools.as_deref()) {
            Ok(tools) => tools,
            Err(error) => {
                return finish_answer_only_failure(
                    store,
                    session_id,
                    run_id,
                    stream_tx,
                    LlmError::local(LlmErrorClass::Protocol, LlmErrorPhase::Request, error),
                    "request_rejected",
                )
                .await
                .map(AnswerOnlyRoute::Failed)
            }
        };
    if let Some(tools) = typed_tools.as_mut() {
        for tool in tools.iter_mut() {
            *tool = tool.clone().with_strict(true);
        }
    }
    let request = crate::context::snapshot::apply_response_reserve(
        LlmRequest::new(&typed_messages, typed_tools.as_deref()).with_scope(request_scope.clone()),
        config.llm.context_length,
    );
    let stream = match llm.stream(request).await {
        Ok(stream) => stream,
        Err(error) => {
            return finish_answer_only_failure(
                store,
                session_id,
                run_id,
                stream_tx,
                redact_provider_failure(config, error),
                "transport_failed",
            )
            .await
            .map(AnswerOnlyRoute::Failed)
        }
    };
    let (response, projected) = match finish_private_provider_stream(stream, sensitive_values).await
    {
        Ok(completed) => completed,
        Err(error) => {
            return finish_answer_only_failure(
                store,
                session_id,
                run_id,
                stream_tx,
                redact_provider_failure(config, error),
                "transport_failed",
            )
            .await
            .map(AnswerOnlyRoute::Failed)
        }
    };
    store
        .record_event(
            session_id,
            "context_usage",
            crate::context::snapshot::usage_event_details(run_id, 0, response.usage.as_ref()),
        )
        .map_err(|error| error.to_string())?;

    if response.terminal_status == LlmTerminalStatus::Refused {
        record_answer_only_fallback(store, session_id, run_id, "model_refusal")?;
        return Ok(AnswerOnlyRoute::Fallback);
    }

    let calls = response.tool_calls.as_deref().unwrap_or_default();
    if calls.is_empty() {
        let Some(content) = response
            .content
            .as_deref()
            .map(str::trim)
            .filter(|content| !content.is_empty())
        else {
            record_answer_only_fallback(store, session_id, run_id, "empty_response")?;
            return Ok(AnswerOnlyRoute::Fallback);
        };
        if response.continuation.is_some() {
            let failure = LlmError::local(
                LlmErrorClass::Protocol,
                LlmErrorPhase::TerminalValidation,
                "answer-only content unexpectedly retained provider continuation state",
            );
            return finish_answer_only_failure(
                store,
                session_id,
                run_id,
                stream_tx,
                failure,
                "malformed_control",
            )
            .await
            .map(AnswerOnlyRoute::Failed);
        }
        for event in projected {
            emit(stream_tx, event).await;
        }
        let content = safe_persisted_provider_message(content, sensitive_values, true);
        return finish_answer_only_success(store, session_id, run_id, stream_tx, &content)
            .await
            .map(AnswerOnlyRoute::Completed);
    }

    let valid_request_plan = calls.len() == 1
        && calls[0].name == "request_plan"
        && calls[0]
            .arguments
            .as_object()
            .is_some_and(serde_json::Map::is_empty);
    if valid_request_plan {
        record_answer_only_fallback(store, session_id, run_id, "request_plan")?;
        return Ok(AnswerOnlyRoute::Fallback);
    }

    finish_answer_only_failure(
        store,
        session_id,
        run_id,
        stream_tx,
        LlmError::local(
            LlmErrorClass::Protocol,
            LlmErrorPhase::TerminalValidation,
            "answer-only response contained a malformed routing control",
        ),
        "malformed_control",
    )
    .await
    .map(AnswerOnlyRoute::Failed)
}

pub(crate) fn record_answer_only_fallback(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    reason: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "answer_route_fallback",
                json!({"run_id": run_id, "route": "answer_only", "reason": reason}),
            );
            append_session_event(
                session,
                "answer_route_completed",
                json!({
                    "run_id": run_id,
                    "route": "answer_only",
                    "outcome": "fallback",
                    "reason": reason,
                }),
            );
            Ok(())
        })
        .map_err(|error| format!("failed to audit answer-only fallback: {error}"))
}

pub(crate) async fn finish_answer_only_success(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
    content: &str,
) -> Result<AgentRunSummary, String> {
    let content = content.to_string();
    let persisted_content = content.clone();
    let tool_call_count = store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "state_transition",
                json!({"from": Value::Null, "to": AgentState::Idle.as_str()}),
            );
            append_session_event(
                session,
                "state_transition",
                json!({"from": AgentState::Idle.as_str(), "to": AgentState::InspectLlm.as_str()}),
            );
            let message_index = session.messages.len();
            session.messages.push(SessionMessage {
                index: message_index,
                role: "assistant".to_string(),
                content: persisted_content.clone(),
                timestamp: Some(Utc::now()),
                attachments: Vec::new(),
            });
            session.message_provenance.push(MessageProvenance {
                message_index,
                origin: MessageOrigin::ModelOutput,
            });
            append_session_event(
                session,
                "answer_route_completed",
                json!({"run_id": run_id, "route": "answer_only", "outcome": "completed"}),
            );
            append_session_event(
                session,
                "state_transition",
                json!({"from": AgentState::InspectLlm.as_str(), "to": AgentState::Reconciliation.as_str()}),
            );
            append_session_event(
                session,
                "reconciliation",
                json!({"outcome": "completed", "continue": false, "route": "answer_only"}),
            );
            append_session_event(
                session,
                "state_transition",
                json!({"from": AgentState::Reconciliation.as_str(), "to": AgentState::Done.as_str()}),
            );
            Ok(session.tool_calls.len())
        })
        .map_err(|error| format!("failed to commit answer-only response: {error}"))?;
    emit(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: "completed".to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    )
    .await;
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        steps_taken: 1,
        last_message: Some(content),
        tool_call_count,
        final_state: AgentState::Done,
        outcome: "completed".to_string(),
        failure: None,
        bound_reached: false,
        trace: vec![
            AgentState::Idle.as_str().to_string(),
            AgentState::InspectLlm.as_str().to_string(),
            AgentState::Reconciliation.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

pub(crate) async fn finish_answer_only_failure(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
    failure: LlmError,
    reason: &str,
) -> Result<AgentRunSummary, String> {
    let persisted_failure = failure.clone();
    let tool_call_count = store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "answer_route_completed",
                json!({
                    "run_id": run_id,
                    "route": "answer_only",
                    "outcome": "failed",
                    "reason": reason,
                }),
            );
            append_session_event(
                session,
                "state_transition",
                json!({"from": Value::Null, "to": AgentState::Reconciliation.as_str()}),
            );
            append_session_event(
                session,
                "reconciliation",
                json!({
                    "outcome": "answer_only_failed",
                    "continue": false,
                    "route": "answer_only",
                    "failure": persisted_failure,
                }),
            );
            append_session_event(
                session,
                "state_transition",
                json!({"from": AgentState::Reconciliation.as_str(), "to": AgentState::Done.as_str()}),
            );
            Ok(session.tool_calls.len())
        })
        .map_err(|error| format!("failed to reconcile answer-only failure: {error}"))?;
    emit(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: "answer_only_failed".to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    )
    .await;
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        steps_taken: 1,
        last_message: None,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: "answer_only_failed".to_string(),
        failure: Some(failure),
        bound_reached: false,
        trace: vec![
            AgentState::Reconciliation.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

pub(crate) async fn finish_answer_only_planning_required(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
    reason: &str,
) -> Result<AgentRunSummary, String> {
    let outcome = if reason == "active_plan" {
        "planning_required_active_plan"
    } else {
        "planning_required_active_run"
    };
    let tool_call_count = store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "answer_route_bypassed",
                json!({"run_id": run_id, "route": "answer_only", "reason": reason}),
            );
            append_session_event(
                session,
                "state_transition",
                json!({"from": Value::Null, "to": AgentState::Reconciliation.as_str()}),
            );
            append_session_event(
                session,
                "reconciliation",
                json!({"outcome": outcome, "continue": false, "route": "answer_only"}),
            );
            append_session_event(
                session,
                "state_transition",
                json!({"from": AgentState::Reconciliation.as_str(), "to": AgentState::Done.as_str()}),
            );
            Ok(session.tool_calls.len())
        })
        .map_err(|error| format!("failed to reconcile answer-only planning requirement: {error}"))?;
    emit(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: outcome.to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    )
    .await;
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        steps_taken: 0,
        last_message: None,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: outcome.to_string(),
        failure: None,
        bound_reached: false,
        trace: vec![
            AgentState::Reconciliation.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

pub(crate) fn runtime_terminal_event(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    outcome: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if !session.events.iter().any(|event| {
                event.kind == "run_terminal"
                    && event.details.get("run_id").and_then(Value::as_str) == Some(run_id)
            }) {
                append_session_event(
                    session,
                    "run_terminal",
                    json!({"run_id": run_id, "outcome": outcome}),
                );
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn workload_context_sections(
    records: &[crate::daemons::workload::DurableTaskRecord],
) -> Vec<RuntimeContextSection> {
    let mut status_counts = std::collections::BTreeMap::<&str, usize>::new();
    for record in records {
        *status_counts.entry(record.status.as_str()).or_default() += 1;
    }
    let counts = status_counts
        .into_iter()
        .map(|(status, count)| format!("{status}={count}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sections = vec![RuntimeContextSection {
        label: "workload.snapshot".to_string(),
        content: format!(
            "authoritative durable workload: total={}; statuses=[{}]",
            records.len(),
            counts
        ),
    }];
    sections.extend(records.iter().map(|record| RuntimeContextSection {
        label: format!("workload.task.{}", record.id),
        content: format!(
            "kind={}; status={}; cancel_requested={}; occurrences={}/{}; next_run_at={}",
            record.kind,
            record.status,
            record.cancel_requested,
            record.completed_occurrences,
            record.total_occurrences,
            record
                .next_run_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "none".to_string())
        ),
    }));
    sections
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn reconcile_cancelled_run(
    store: &SessionStore,
    session_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AgentRunSummary, String> {
    if store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        store
            .try_create_session_with_id(session_id.to_string())
            .map_err(|error| error.to_string())?;
    }
    let (transitioned_to_reconciliation, tool_call_count, trace) = store
        .update_session(session_id, |session| {
            let current_state = last_persisted_state(session);
            append_session_event(
                session,
                "cancel_requested",
                json!({"reason": "cancelled_by_user", "state": current_state.clone()}),
            );
            let transitioned_to_reconciliation =
                current_state.as_deref() != Some(AgentState::Reconciliation.as_str());
            if transitioned_to_reconciliation {
                append_session_event(
                    session,
                    "state_transition",
                    json!({
                        "from": current_state.clone(),
                        "to": AgentState::Reconciliation.as_str(),
                    }),
                );
            }

            let mut cancelled_verifications = Vec::new();
            if let Some(plan) = session.plan.as_mut().filter(|plan| !plan.is_complete()) {
                cancelled_verifications =
                    plan.cancel_running_verifications("agent run cancelled by user");
                plan.outcome = Some("cancelled_by_user".to_string());
                if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                    if step.status != "Completed" {
                        step.status = "Cancelled".to_string();
                        step.outcome = Some("cancelled_by_user".to_string());
                        step.updated_at = Some(Utc::now());
                    }
                }
            }
            if !cancelled_verifications.is_empty() {
                append_session_event(
                    session,
                    "verification_cancelled",
                    json!({
                        "verification_ids": cancelled_verifications,
                        "reason": "cancelled_by_user",
                    }),
                );
            }
            append_session_event(
                session,
                "reconciliation",
                json!({"outcome": "cancelled_by_user", "continue": false}),
            );
            append_session_event(
                session,
                "state_transition",
                json!({
                    "from": AgentState::Reconciliation.as_str(),
                    "to": AgentState::Done.as_str(),
                }),
            );
            let trace = session
                .events
                .iter()
                .filter(|event| event.kind == "state_transition")
                .filter_map(|event| event.details.get("to").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            Ok((
                transitioned_to_reconciliation,
                session.tool_calls.len(),
                trace,
            ))
        })
        .map_err(|error| format!("failed to reconcile cancelled session: {error}"))?;

    if transitioned_to_reconciliation {
        emit(
            stream_tx,
            StreamEvent::StateTransition {
                state: AgentState::Reconciliation.as_str().to_string(),
            },
        )
        .await;
    }
    emit(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: "cancelled_by_user".to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    )
    .await;

    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: String::new(),
        steps_taken: 0,
        // Cancellation is local lifecycle state, never model-authored output. In
        // particular, do not echo the user's interrupted prompt as a gateway reply.
        last_message: None,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: "cancelled_by_user".to_string(),
        failure: None,
        bound_reached: false,
        trace,
    })
}

pub(crate) fn llm_configuration_failure(
    config: &crate::config::NibConfig,
    provider_override: Option<&str>,
    safe_message: &str,
    sensitive_values: &[String],
) -> LlmError {
    let provider = provider_override
        .or(config.llm.active_provider.as_deref())
        .unwrap_or("unconfigured");
    let entry = config.llm.providers.get(provider);
    let transport = crate::llm::registry::provider_descriptor(provider)
        .map(|descriptor| descriptor.configured_transport(entry).as_str())
        .unwrap_or("unknown");
    LlmError::new(
        LlmErrorClass::Configuration,
        LlmErrorPhase::Configuration,
        crate::llm::RetryDisposition::NotAttempted,
        crate::llm::LlmErrorMetadata::new(
            provider,
            transport,
            entry.map(|entry| entry.model.as_str()),
            None,
            &crate::llm::factory::provider_error_sensitive_values(sensitive_values.to_vec()),
        ),
        safe_message,
    )
}

pub(crate) async fn reconcile_preflight_llm_failure(
    store: &SessionStore,
    session_id: &str,
    normalized_goal: &str,
    failure: LlmError,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AgentRunSummary, String> {
    let persisted_failure = failure.clone();
    let (last_message, tool_call_count) = store
        .update_session(session_id, |session| {
            if let Some(plan) = session
                .plan
                .as_mut()
                .filter(|plan| plan.matches_goal(normalized_goal) && !plan.is_complete())
            {
                plan.outcome = Some("configuration_failed".to_string());
                if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                    if step.status != "Completed" {
                        step.status = "Blocked".to_string();
                        step.outcome = Some("configuration_failed".to_string());
                        step.updated_at = Some(Utc::now());
                    }
                }
            }
            let previous_state = last_persisted_state(session);
            append_session_event(
                session,
                "state_transition",
                json!({
                    "from": previous_state,
                    "to": AgentState::Reconciliation.as_str(),
                }),
            );
            append_session_event(
                session,
                "reconciliation",
                json!({
                    "outcome": "configuration_failed",
                    "continue": false,
                    "failure": persisted_failure.clone(),
                }),
            );
            append_session_event(
                session,
                "state_transition",
                json!({
                    "from": AgentState::Reconciliation.as_str(),
                    "to": AgentState::Done.as_str(),
                }),
            );
            Ok((
                session
                    .messages
                    .last()
                    .map(|message| message.content.clone()),
                session.tool_calls.len(),
            ))
        })
        .map_err(|error| format!("failed to reconcile LLM configuration failure: {error}"))?;

    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Reconciliation.as_str().to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: "configuration_failed".to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    )
    .await;

    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: String::new(),
        steps_taken: 0,
        last_message,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: "configuration_failed".to_string(),
        failure: Some(failure),
        bound_reached: false,
        trace: vec![
            AgentState::Reconciliation.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

pub(crate) fn persist_instruction_context_block(
    store: &SessionStore,
    session_id: &str,
    active_plan_id: Option<&str>,
    normalized_goal: &str,
    stage: &str,
    error: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if let Some(plan) = session.plan.as_mut().filter(|plan| {
                plan.matches_goal(normalized_goal)
                    && !plan.is_complete()
                    && active_plan_id.is_none_or(|expected| plan.id == expected)
            }) {
                plan.outcome = Some("instruction_context_missing".to_string());
                if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                    if step.status != "Completed" {
                        step.status = "Blocked".to_string();
                        step.outcome = Some(error.to_string());
                        step.updated_at = Some(Utc::now());
                    }
                }
            }
            append_session_event(
                session,
                "instruction_context_missing",
                json!({
                    "stage": stage,
                    "error": error,
                    "action": "restore readable bounded project instructions or increase the context budget, then retry the same plan",
                }),
            );
            Ok(())
        })
        .map_err(|audit_error| {
            format!("failed to persist missing instruction context: {audit_error}")
        })
}

pub(crate) async fn reconcile_preflight_instruction_failure(
    store: &SessionStore,
    session_id: &str,
    normalized_goal: &str,
    stage: &str,
    error: String,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AgentRunSummary, String> {
    let message = instruction_context_user_message(&error);
    persist_instruction_context_block(store, session_id, None, normalized_goal, stage, &error)?;
    append_assistant_if_allowed(store, session_id, &message)?;
    let tool_call_count = store
        .update_session(session_id, |session| {
            let previous_state = last_persisted_state(session);
            append_session_event(
                session,
                "state_transition",
                json!({"from": previous_state, "to": AgentState::Reconciliation.as_str()}),
            );
            append_session_event(
                session,
                "reconciliation",
                json!({
                    "outcome": "instruction_context_missing",
                    "continue": false,
                    "reason": message.clone(),
                }),
            );
            append_session_event(
                session,
                "state_transition",
                json!({
                    "from": AgentState::Reconciliation.as_str(),
                    "to": AgentState::Done.as_str(),
                }),
            );
            Ok(session.tool_calls.len())
        })
        .map_err(|audit_error| {
            format!("failed to reconcile missing instruction context: {audit_error}")
        })?;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Reconciliation.as_str().to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: "instruction_context_missing".to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    )
    .await;
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: String::new(),
        steps_taken: 0,
        last_message: Some(message),
        tool_call_count,
        final_state: AgentState::Done,
        outcome: "instruction_context_missing".to_string(),
        failure: None,
        bound_reached: false,
        trace: vec![
            AgentState::Reconciliation.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

pub(crate) fn last_persisted_state(session: &Session) -> Option<String> {
    session
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "state_transition")
        .and_then(|event| event.details.get("to"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn append_session_event(session: &mut Session, kind: &str, details: Value) {
    session.events.push(SessionEvent {
        index: session.events.len(),
        kind: kind.to_string(),
        details,
        timestamp: Some(Utc::now()),
    });
}

pub(crate) fn record_provider_continuation_lifecycle(
    store: &SessionStore,
    session_id: &str,
    kind: &str,
    run_id: &str,
) -> Result<(), String> {
    store
        .record_event(session_id, kind, json!({"run_id": run_id}))
        .map(|_| ())
        .map_err(|error| format!("failed to record provider continuation lifecycle: {error}"))
}

pub(crate) fn record_provider_tool_output(
    continuation: &mut Option<ProviderContinuation>,
    request: &ToolCallRequest,
    observation: &Value,
    classification: ToolResultClass,
) -> Result<(), String> {
    match continuation.as_mut() {
        Some(continuation) => continuation.record_tool_result(ProviderToolResult::new(
            request.invocation_id,
            observation.clone(),
            classification,
        )?),
        None => Ok(()),
    }
}

pub(crate) fn record_provider_tool_outputs(
    continuation: &mut Option<ProviderContinuation>,
    requests: &[ToolCallRequest],
    observations: &[Value],
    classifications: &[ToolResultClass],
) -> Result<(), String> {
    if continuation.is_none() {
        return Ok(());
    }
    if requests.len() != observations.len() || requests.len() != classifications.len() {
        return Err("provider continuation tool/output counts do not match".to_string());
    }
    for ((request, observation), classification) in
        requests.iter().zip(observations).zip(classifications)
    {
        record_provider_tool_output(continuation, request, observation, *classification)?;
    }
    Ok(())
}

pub(crate) fn reconcile_interrupted_provider_continuation(
    store: &SessionStore,
    session_id: &str,
) -> Result<bool, String> {
    store
        .update_session(session_id, |session| {
            let latest_lifecycle = session.events.iter().rev().find(|event| {
                matches!(
                    event.kind.as_str(),
                    "provider_continuation_opened"
                        | "provider_continuation_closed"
                        | "provider_continuation_abandoned"
                        | "provider_continuation_interrupted"
                        | "reconciliation"
                )
            });
            let Some(opened) =
                latest_lifecycle.filter(|event| event.kind == "provider_continuation_opened")
            else {
                return Ok(false);
            };
            let prior_run_id = opened
                .details
                .get("run_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let current_state = last_persisted_state(session);
            append_session_event(
                session,
                "provider_continuation_interrupted",
                json!({
                    "prior_run_id": prior_run_id,
                    "reason": "opaque continuation was discarded after process interruption",
                }),
            );
            if !matches!(
                current_state.as_deref(),
                Some("Reconciliation") | Some("Done")
            ) {
                append_session_event(
                    session,
                    "state_transition",
                    json!({
                        "from": current_state,
                        "to": AgentState::Reconciliation.as_str(),
                    }),
                );
            }
            if let Some(plan) = session.plan.as_mut().filter(|plan| !plan.is_complete()) {
                plan.outcome = Some("provider_continuation_interrupted".to_string());
                if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                    if step.status != "Completed" {
                        step.status = "Blocked".to_string();
                        step.outcome = Some("provider_continuation_interrupted".to_string());
                        step.updated_at = Some(Utc::now());
                    }
                }
            }
            if session
                .messages
                .last()
                .is_some_and(|message| message.role == "tool")
            {
                session.messages.push(SessionMessage {
                    index: session.messages.len(),
                    role: "assistant".to_string(),
                    content: json!({
                        "type": "provider_continuation_boundary",
                        "outcome": "provider_continuation_interrupted",
                    })
                    .to_string(),
                    timestamp: Some(Utc::now()),
                    attachments: Vec::new(),
                });
            }
            append_session_event(
                session,
                "reconciliation",
                json!({
                    "outcome": "provider_continuation_interrupted",
                    "continue": false,
                }),
            );
            if current_state.as_deref() != Some(AgentState::Done.as_str()) {
                append_session_event(
                    session,
                    "state_transition",
                    json!({
                        "from": AgentState::Reconciliation.as_str(),
                        "to": AgentState::Done.as_str(),
                    }),
                );
            }
            Ok(true)
        })
        .map_err(|error| format!("failed to reconcile interrupted provider turn: {error}"))
}

pub(crate) fn block_active_plan_for_failure(
    store: &SessionStore,
    session_id: &str,
    active_plan_id: Option<&str>,
    normalized_goal: &str,
    reason: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            let Some(plan) = session.plan.as_mut() else {
                return Ok(());
            };
            if active_plan_id != Some(plan.id.as_str())
                || !plan.matches_goal(normalized_goal)
                || plan.is_complete()
            {
                return Ok(());
            }
            plan.outcome = Some(reason.to_string());
            if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                if step.status != "Completed" {
                    step.status = "Blocked".to_string();
                    step.outcome = Some(reason.to_string());
                    step.updated_at = Some(Utc::now());
                }
            }
            Ok(())
        })
        .map_err(|error| format!("failed to block plan after provider failure: {error}"))
}

pub(crate) fn is_llm_failure_outcome(outcome: &str) -> bool {
    [
        "planning_failed:",
        "llm_stream_failed:",
        "invalid_tool_stream:",
        "provider_continuation_failed:",
        "compression_failed:",
        "configuration_failed:",
        "answer_only_failed:",
    ]
    .iter()
    .any(|prefix| outcome.starts_with(prefix))
        || matches!(
            outcome,
            "planning_failed"
                | "llm_stream_failed"
                | "invalid_tool_stream"
                | "provider_continuation_failed"
                | "compression_failed"
                | "configuration_failed"
                | "answer_only_failed"
        )
}

pub(crate) fn is_agent_failure_outcome(outcome: &str) -> bool {
    is_llm_failure_outcome(outcome)
        || matches!(
            outcome,
            "model_refusal"
                | "empty_model_response"
                | "tool_execution_failed"
                | "repeated_tool_failure"
                | "blocked_step_unresolved"
                | "required_verification_unresolved"
                | "transition_limit_reached"
                | "turn_limit_reached"
                | "provider_continuation_interrupted"
                | "instruction_context_missing"
                | "unresolved_clarification"
                | "planning_required_active_plan"
                | "planning_required_active_run"
                | "plan_binding_changed"
                | "plan_approval_denied"
                | "worktree_preparation_failed"
        )
}

pub(crate) fn redact_provider_failure(
    config: &crate::config::NibConfig,
    error: LlmError,
) -> LlmError {
    error.redacted_with(&crate::llm::factory::provider_error_sensitive_values(
        config.sensitive_values(),
    ))
}

pub(crate) fn record_curator_tool_call(
    store: &SessionStore,
    session_id: &str,
    profile_id: &str,
    config: &crate::config::NibConfig,
    report: Option<&crate::daemons::curator::CuratorReport>,
    error: Option<&str>,
    duration_seconds: f64,
) -> Result<(), String> {
    let policy_decision = if config.daemons.allow_destructive_cleanup {
        "destructive_cleanup_authorized_by_config"
    } else {
        "destructive_cleanup_not_authorized"
    };
    let result = match report {
        Some(report) => json!({
            "status": "completed",
            "scanned": report.scanned,
            "deleted": report.deleted,
            "pinned": report.pinned,
            "policy_skipped": report.policy_skipped,
            "retained": report.retained,
            "sessions_deleted": report.sessions_deleted,
            "memory_deleted": report.memory_deleted,
            "skills_deleted": report.skills_deleted,
            "errors": &report.errors,
        }),
        None => json!({
            "status": "error",
            "message": error.unwrap_or("curator maintenance failed"),
        }),
    };
    store
        .record_tool_call(ToolCallRecord {
            invocation_id: Some(crate::tools::ToolInvocationId::new()),
            id: Some(format!("daemon-curator-{}", uuid::Uuid::new_v4())),
            session_id: Some(session_id.to_string()),
            tool_name: Some("daemon_curator".to_string()),
            arguments: json!({
                "profile_id": profile_id,
                "retention_days": config.daemons.retention_days,
                "interval_seconds": config.daemons.interval_seconds,
                "permission_level": "destructive",
                "policy_decision": policy_decision,
                "policy_source": "daemons.allow_destructive_cleanup",
            }),
            result: Some(result),
            error: error.map(str::to_string),
            duration_seconds: Some(duration_seconds),
            worktree_path: None,
            timestamp: Some(Utc::now()),
            provider: Some("internal-daemon".to_string()),
            sandbox_profile: None,
            bwrap_args: None,
            boundaries: Some(config.execution.boundaries.clone()),
            plan_id: None,
        })
        .map_err(|record_error| {
            let context = error
                .map(|maintenance_error| format!(" after maintenance error: {maintenance_error}"))
                .unwrap_or_default();
            format!("failed to record curator maintenance in session{context}: {record_error}")
        })
}

pub(crate) fn continue_admission_error(
    session: &Session,
    plan_id: &str,
    normalized_goal: &str,
    current_run_id: &str,
) -> Option<String> {
    let Some(plan) = session.plan.as_ref() else {
        return Some("continue requires a persisted plan".to_string());
    };
    if plan.id != plan_id {
        return Some(format!(
            "continue target {plan_id} is not the current plan {}",
            plan.id
        ));
    }
    if !plan.has_identity() {
        return Some("persisted plan is missing goal or identity and cannot continue".to_string());
    }
    if !plan.matches_goal(normalized_goal) {
        return Some("continue goal does not match persisted plan provenance".to_string());
    }
    if plan.is_complete() {
        return Some(format!("plan {plan_id} is already complete"));
    }
    if !plan.approved {
        return Some(format!("plan {plan_id} is not approved for execution"));
    }
    if !plan.is_structured() {
        return Some(format!("plan {plan_id} is malformed and cannot continue"));
    }
    if plan.outcome.as_deref() == Some("provider_continuation_interrupted") {
        return Some(format!(
            "plan {plan_id} has an uncertain interrupted provider continuation and cannot be replayed"
        ));
    }
    if session.has_unresolved_clarification(Some(&plan.id)) {
        return Some("unresolved questions still block this plan".to_string());
    }
    if has_unterminated_prior_run(session, current_run_id) {
        return Some(
            "a prior run has not been reconciled and this plan cannot continue".to_string(),
        );
    }
    None
}

pub(crate) fn validate_continue_admission(
    store: &SessionStore,
    session_id: &str,
    plan_id: &str,
    goal: &str,
    current_run_id: &str,
) -> Result<(), String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to validate continuation: {error}"))?
        .ok_or_else(|| format!("session {session_id} disappeared before continuation"))?;
    continue_admission_error(
        &session,
        plan_id,
        &normalize_plan_goal(goal),
        current_run_id,
    )
    .map_or(Ok(()), Err)
}

pub(crate) fn prepare_continue_turn(
    store: &SessionStore,
    session_id: &str,
    plan_id: &str,
    goal: &str,
    run_id: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if let Some(error) = continue_admission_error(session, plan_id, goal, run_id) {
                return Err(crate::session::SessionError::InvalidMutation(error));
            }
            let plan = session.plan.as_ref().ok_or_else(|| {
                crate::session::SessionError::InvalidMutation(
                    "continue requires a persisted plan".to_string(),
                )
            })?;
            let step = plan
                .steps
                .get(plan.current_step_index)
                .map(|step| step.description.clone())
                .unwrap_or_else(|| "remaining work".to_string());
            let message_index = session.messages.len();
            session.messages.push(SessionMessage {
                index: message_index,
                role: "user".to_string(),
                content: format!("Continue with approved plan step: {step}"),
                timestamp: Some(Utc::now()),
                attachments: Vec::new(),
            });
            session.message_provenance.push(MessageProvenance {
                message_index,
                origin: MessageOrigin::RuntimeContinuation,
            });
            let event_index = session.events.len();
            append_session_event(
                session,
                "plan_continue_requested",
                json!({
                    "plan_id": plan_id,
                    "goal_provenance": "persisted_plan_goal",
                }),
            );
            session.human_intent.push(HumanIntentRecord {
                kind: HumanIntentKind::Continue,
                text: plan_id.to_string(),
                source_message_index: None,
                source_event_index: Some(event_index),
            });
            Ok(())
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn prepare_user_turn(
    store: &SessionStore,
    session_id: &str,
    content: &str,
    project_root: &Path,
) -> Result<(), String> {
    let (content, attachments) =
        crate::interactive::resolve_path_attachments(project_root, content)?;
    store
        .update_session(session_id, |session| {
            let message_index = session.messages.len();
            session.messages.push(SessionMessage {
                index: message_index,
                role: "user".to_string(),
                content: content.clone(),
                timestamp: Some(Utc::now()),
                attachments,
            });
            session.message_provenance.push(MessageProvenance {
                message_index,
                origin: MessageOrigin::HumanRequest,
            });
            session.human_intent.push(HumanIntentRecord {
                kind: HumanIntentKind::Request,
                text: content,
                source_message_index: Some(message_index),
                source_event_index: None,
            });
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}
