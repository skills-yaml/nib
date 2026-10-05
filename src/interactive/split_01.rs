//! T043 split.

use super::*;

pub fn persist_queued_follow_up(
    store: &SessionStore,
    session_id: &str,
    text: &str,
    source: &str,
) -> Result<QueuedFollowUp, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("queued follow-up must not be empty".to_string());
    }
    if text.len() > MAX_QUEUE_TEXT_BYTES {
        return Err(format!(
            "queued follow-up must be at most {MAX_QUEUE_TEXT_BYTES} bytes"
        ));
    }
    store
        .update_session(session_id, |session| {
            if session.queued_follow_ups.len() >= MAX_QUEUED_FOLLOW_UPS {
                return Err(crate::session::SessionError::InvalidMutation(format!(
                    "at most {MAX_QUEUED_FOLLOW_UPS} queued follow-ups are retained"
                )));
            }
            let item = QueuedFollowUp {
                id: Uuid::new_v4().to_string(),
                text: text.to_string(),
                created_at: Utc::now(),
                source: source.to_string(),
            };
            session.queued_follow_ups.push(item.clone());
            Ok(item)
        })
        .map_err(|error| format!("failed to persist queued follow-up: {error}"))
}

pub fn queued_follow_up_count(store: &SessionStore, session_id: &str) -> Result<usize, String> {
    Ok(store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .map(|session| session.queued_follow_ups.len())
        .unwrap_or(0))
}

pub fn queue_disposition_message(
    store: &SessionStore,
    session_id: &str,
    action: &str,
) -> Result<String, String> {
    let count = queued_follow_up_count(store, session_id)?;
    if count == 0 {
        Ok(format!("{action}; no queued follow-ups."))
    } else {
        Ok(format!(
            "{action}; {count} queued follow-up(s) retained on session {session_id}."
        ))
    }
}

pub fn take_next_queued_follow_up(
    store: &SessionStore,
    session_id: &str,
) -> Result<Option<String>, String> {
    store
        .update_session(session_id, |session| {
            if session.queued_follow_ups.is_empty() {
                return Ok(None);
            }
            Ok(Some(session.queued_follow_ups.remove(0).text))
        })
        .map_err(|error| format!("failed to take queued follow-up: {error}"))
}

pub(crate) fn append_queue_start_event(
    session: &mut Session,
    kind: &str,
    item: &QueuedFollowUp,
    phase: &str,
    disposition: &str,
) {
    session.events.push(SessionEvent {
        index: session.events.len(),
        kind: kind.to_string(),
        details: serde_json::json!({
            "queue_id": item.id,
            "source": item.source,
            "phase": phase,
            "disposition": disposition,
        }),
        timestamp: Some(Utc::now()),
    });
}

/// Prepare the next FIFO item without releasing it to execution, then durably
/// claim it only after worker startup succeeds.
///
/// `startup` must return a prepared execution handle that cannot process the
/// item until the caller explicitly activates it. If preparation fails, the
/// queue remains unchanged and the retained disposition is recorded in the
/// session audit.
pub fn claim_next_queued_follow_up_after_startup<T>(
    store: &SessionStore,
    session_id: &str,
    startup: impl FnOnce(&QueuedFollowUp) -> Result<T, String>,
) -> Result<Option<(QueuedFollowUp, T)>, String> {
    let Some(item) = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load queued follow-up: {error}"))?
        .and_then(|session| session.queued_follow_ups.first().cloned())
    else {
        return Ok(None);
    };

    let prepared = match startup(&item) {
        Ok(prepared) => prepared,
        Err(error) => {
            let audit = store.update_session(session_id, |session| {
                let retained = session
                    .queued_follow_ups
                    .iter()
                    .any(|queued| queued.id == item.id);
                append_queue_start_event(
                    session,
                    "queued_follow_up_start_failed",
                    &item,
                    "worker_startup",
                    if retained { "retained" } else { "not_retained" },
                );
                Ok(retained)
            });
            return match audit {
                Ok(true) => Err(format!(
                    "queued follow-up {} could not start and remains queued: {error}",
                    item.id
                )),
                Ok(false) => Err(format!(
                    "queued follow-up {} could not start but was no longer queued: {error}",
                    item.id
                )),
                Err(audit_error) => Err(format!(
                    "queued follow-up {} could not start: {error}; failed to record its retained disposition: {audit_error}",
                    item.id
                )),
            };
        }
    };

    let claimed = store
        .update_session(session_id, |session| {
            let Some(head) = session.queued_follow_ups.first() else {
                return Err(crate::session::SessionError::InvalidMutation(format!(
                    "queued follow-up {} disappeared before startup commit",
                    item.id
                )));
            };
            if head.id != item.id {
                return Err(crate::session::SessionError::InvalidMutation(format!(
                    "queued follow-up order changed before startup commit: expected {}, found {}",
                    item.id, head.id
                )));
            }
            let claimed = session.queued_follow_ups.remove(0);
            append_queue_start_event(
                session,
                "queued_follow_up_start_committed",
                &claimed,
                "worker_startup",
                "claimed",
            );
            Ok(claimed)
        })
        .map_err(|error| format!("failed to claim queued follow-up after startup: {error}"))?;
    Ok(Some((claimed, prepared)))
}

/// Restore an item whose prepared worker could not be activated after the
/// durable claim. Reinsertion is idempotent and returns the item to the FIFO
/// head before recording the recoverable failure.
pub fn restore_queued_follow_up_after_start_failure(
    store: &SessionStore,
    session_id: &str,
    item: QueuedFollowUp,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if !session
                .queued_follow_ups
                .iter()
                .any(|queued| queued.id == item.id)
            {
                session.queued_follow_ups.insert(0, item.clone());
            }
            append_queue_start_event(
                session,
                "queued_follow_up_start_failed",
                &item,
                "worker_activation",
                "retained",
            );
            Ok(())
        })
        .map_err(|error| format!("failed to restore queued follow-up {}: {error}", item.id))
}

pub fn unicode_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Wrap plain presentation text into the exact visual rows consumed by the TUI.
/// The renderer displays these rows without applying a second wrapping policy.
pub fn wrapped_display_rows(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    for logical_line in text.split('\n') {
        let mut row = String::new();
        // A variation selector or joiner can change the preceding glyph's width.
        // Decide whether to wrap only after its whole grapheme is available.
        for grapheme in logical_line.graphemes(true) {
            let mut candidate = row.clone();
            candidate.push_str(grapheme);
            if !row.is_empty() && unicode_display_width(&candidate) > width {
                rows.push(std::mem::take(&mut row));
            }
            row.push_str(grapheme);
        }
        if !row.is_empty() || logical_line.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

pub fn wrapped_line_count(text: &str, width: u16) -> usize {
    wrapped_display_rows(text, width).len()
}

pub fn bottom_scroll_for_wrap(text: &str, width: u16, height: u16) -> u16 {
    if text.is_empty() || width == 0 || height == 0 {
        return 0;
    }
    wrapped_line_count(text, width)
        .saturating_sub(usize::from(height))
        .min(usize::from(u16::MAX)) as u16
}

#[derive(Debug)]
pub(crate) struct PersistedActivity {
    pub(crate) timestamp: Option<chrono::DateTime<Utc>>,
    pub(crate) source_rank: u8,
    pub(crate) source_index: usize,
    pub(crate) activity: ActivityEntry,
}

pub(crate) fn bounded_activity_label(value: &str) -> String {
    let mut label = String::new();
    let mut truncated = false;
    let maximum = MAX_ACTIVITY_LABEL_BYTES.saturating_sub(3);
    for character in value.chars() {
        if label.len() >= maximum {
            truncated = true;
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ' ') {
            label.push(character);
        } else {
            label.push('?');
        }
    }
    if truncated {
        label.push_str("...");
    }
    if label.trim().is_empty() {
        "unknown".to_string()
    } else {
        label
    }
}

pub(crate) fn safe_event_atom(details: &serde_json::Value, field: &str) -> Option<String> {
    let value = details.get(field)?.as_str()?;
    if value.is_empty()
        || value.len() > MAX_ACTIVITY_LABEL_BYTES
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return None;
    }
    Some(value.to_string())
}

pub(crate) fn safe_event_number(details: &serde_json::Value, field: &str) -> Option<String> {
    details
        .get(field)
        .and_then(|value| value.as_u64().map(|value| value.to_string()))
}

pub(crate) fn safe_event_fields(details: &serde_json::Value, fields: &[&str]) -> String {
    fields
        .iter()
        .filter_map(|field| {
            safe_event_atom(details, field)
                .or_else(|| safe_event_number(details, field))
                .map(|value| format!("{field}={value}"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(crate) fn safe_failure_fields(details: &serde_json::Value) -> String {
    let failure = details.get("failure").filter(|value| value.is_object());
    let evidence = failure.unwrap_or(details);
    safe_event_fields(
        evidence,
        &["class", "phase", "retry", "incident_code", "code"],
    )
}

pub(crate) fn is_failure_outcome(outcome: &str) -> bool {
    outcome.contains("failed")
        || outcome.contains("failure")
        || outcome.contains("refusal")
        || outcome.contains("interrupted")
        || matches!(
            outcome,
            "invalid_plan"
                | "blocked_step_unresolved"
                | "required_verification_unresolved"
                | "turn_limit_reached"
                | "transition_limit_reached"
                | "model_refusal"
                | "empty_model_response"
                | "instruction_context_missing"
                | "tool_scope_outside_worktree"
                | "tool_scope_required"
                | "planning_required_active_plan"
                | "planning_required_active_run"
                | "plan_binding_changed"
                | "plan_approval_denied"
        )
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn project_session_event(
    event: &SessionEvent,
    sensitive_values: &[String],
) -> Option<ActivityEntry> {
    let kind = event.kind.as_str();
    if matches!(
        kind,
        "tool_started"
            | "tool_completed"
            | "run_started"
            | "state_transition"
            | "context_bounded"
            | "plan_generated"
            | "plan_approved"
    ) {
        // ToolCallRecord and reconciliation are the authoritative completed
        // projections. Routine lifecycle events remain persisted in the session
        // audit but do not compete with the conversation in the default ledger.
        return None;
    }
    if kind == "reconciliation"
        && event
            .details
            .get("continue")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    {
        return None;
    }

    let (activity_kind, title, body) = match kind {
        "run_terminal" => {
            let outcome =
                safe_event_atom(&event.details, "outcome").unwrap_or_else(|| "unknown".to_string());
            let activity_kind = if outcome.contains("cancel") {
                ActivityKind::Cancellation
            } else if outcome == "local_error" || is_failure_outcome(&outcome) {
                ActivityKind::Failure
            } else {
                ActivityKind::Reconcile
            };
            let message = terminal_outcome_message(&outcome);
            (
                activity_kind,
                message.title.to_string(),
                message.detail.to_string(),
            )
        }
        "steering_input" => (
            ActivityKind::User,
            "steer".to_string(),
            safe_event_fields(&event.details, &["sequence", "source"]),
        ),
        "steering_intake" => (
            ActivityKind::System,
            "steering accepted at safe boundary".to_string(),
            safe_event_fields(&event.details, &["sequence"]),
        ),
        "steering_delivery_failed" => (
            ActivityKind::Failure,
            "steering delivery failed".to_string(),
            safe_event_fields(&event.details, &["sequence", "reason"]),
        ),
        "provider_continuation_abandoned_by_steering" => (
            ActivityKind::System,
            "provider response superseded by steering".to_string(),
            String::new(),
        ),
        "tool_proposal_superseded_by_steering" => (
            ActivityKind::System,
            "tool proposal superseded by steering".to_string(),
            safe_event_fields(&event.details, &["first_sequence", "last_sequence"]),
        ),
        "question_required" => {
            let option_count = event
                .details
                .get("options")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            (
                ActivityKind::Question,
                "agent question required".to_string(),
                format!("options={option_count}"),
            )
        }
        "approval_required" => {
            let approval_kind =
                safe_event_atom(&event.details, "kind").unwrap_or_else(|| "action".to_string());
            if approval_kind == "plan" {
                (
                    ActivityKind::Approval,
                    "plan approval requested".to_string(),
                    safe_event_fields(&event.details, &["step_count"]),
                )
            } else {
                let tool = safe_event_atom(&event.details, "tool_name")
                    .unwrap_or_else(|| "tool".to_string());
                (
                    ActivityKind::Approval,
                    format!("{tool} approval requested"),
                    String::new(),
                )
            }
        }
        "compression" => (
            ActivityKind::Compression,
            "context compressed".to_string(),
            safe_event_fields(
                &event.details,
                &["before_tokens", "after_tokens", "summarized_through"],
            ),
        ),
        "cancel_requested" | "timer_cancelled" | "mcp_request_cancelled" => (
            ActivityKind::Cancellation,
            "cancellation requested".to_string(),
            safe_event_fields(&event.details, &["reason", "state"]),
        ),
        "reconciliation" => {
            let outcome =
                safe_event_atom(&event.details, "outcome").unwrap_or_else(|| "unknown".to_string());
            if outcome.contains("cancel") {
                let message = terminal_outcome_message(&outcome);
                (
                    ActivityKind::Cancellation,
                    message.title.to_string(),
                    message.detail.to_string(),
                )
            } else if outcome == "instruction_context_missing" {
                let reason = event
                    .details
                    .get("reason")
                    .and_then(|value| value.as_str())
                    .unwrap_or(
                        "Project instructions could not be loaded. Restore readable project instructions within the size limit, or increase llm.context_length, then retry the same plan.",
                    );
                (
                    ActivityKind::Failure,
                    "project instructions unavailable".to_string(),
                    bounded_public_text(reason, sensitive_values, MAX_ACTIVITY_BODY_BYTES, true),
                )
            } else if event
                .details
                .get("failure")
                .is_some_and(|value| !value.is_null())
                || is_failure_outcome(&outcome)
            {
                let message = terminal_outcome_message(&outcome);
                let fields = safe_failure_fields(&event.details);
                let body = if fields.is_empty() {
                    message.detail.to_string()
                } else {
                    format!("{}\n{fields}", message.detail)
                };
                (ActivityKind::Failure, message.title.to_string(), body)
            } else {
                let message = terminal_outcome_message(&outcome);
                (
                    ActivityKind::Reconcile,
                    message.title.to_string(),
                    message.detail.to_string(),
                )
            }
        }
        kind @ ("plan_generation_conflict"
        | "stale_plan_approval_ignored"
        | "plan_invalidated"
        | "plan_binding_conflict"
        | "plan_superseded_by_steering"
        | "scheduled_plan_archived") => (
            ActivityKind::Plan,
            bounded_activity_label(&kind.replace('_', " ")),
            safe_event_fields(&event.details, &["reason", "stage"]),
        ),
        kind @ ("prepared_task_start_failed"
        | "queued_follow_up_start_failed"
        | "timer_failed"
        | "timer_schedule_failed"
        | "background_task_failed"
        | "delivery_failed"
        | "scheduled_agent_run_failed"
        | "tool_batch_rejected"
        | "role_violation") => (
            ActivityKind::Failure,
            bounded_activity_label(&kind.replace('_', " ")),
            safe_failure_fields(&event.details),
        ),
        "provider_continuation_interrupted" => (
            ActivityKind::Failure,
            "provider continuation interrupted".to_string(),
            String::new(),
        ),
        "scheduled_agent_run_completed" => (
            ActivityKind::Reconcile,
            "scheduled agent run completed".to_string(),
            safe_event_fields(&event.details, &["occurrence", "repeat_count"]),
        ),
        "background_task_completed" => (
            ActivityKind::Tool,
            "background task completed".to_string(),
            String::new(),
        ),
        "queued_follow_up_start_committed" | "timer_fired" => (
            ActivityKind::System,
            bounded_activity_label(&kind.replace('_', " ")),
            String::new(),
        ),
        _ => return None,
    };
    Some(ActivityEntry::new(
        activity_kind,
        title,
        bounded_activity_body(&body, sensitive_values),
    ))
}

pub(crate) fn project_session_message(
    message: &crate::session::SessionMessage,
    sensitive_values: &[String],
) -> ActivityEntry {
    let normalized_role = message.role.to_ascii_lowercase();
    let (kind, title, body) = match normalized_role.as_str() {
        "user" => (
            ActivityKind::User,
            String::new(),
            bounded_activity_body(&message.content, sensitive_values),
        ),
        "assistant" => (
            ActivityKind::Assistant,
            String::new(),
            bounded_activity_body(&message.content, sensitive_values),
        ),
        "tool" => {
            let observation_count = serde_json::from_str::<serde_json::Value>(&message.content)
                .ok()
                .and_then(|value| {
                    value
                        .get("observations")
                        .and_then(serde_json::Value::as_array)
                        .map(Vec::len)
                });
            let body = observation_count
                .map(|count| format!("{count} persisted observation(s)"))
                .unwrap_or_else(|| "persisted tool result context".to_string());
            (ActivityKind::Tool, "tool result context".to_string(), body)
        }
        "system" => (
            ActivityKind::System,
            String::new(),
            bounded_activity_body(&message.content, sensitive_values),
        ),
        _ => (
            ActivityKind::System,
            "unsupported legacy message role".to_string(),
            "legacy message content omitted".to_string(),
        ),
    };
    ActivityEntry::new(kind, title, body)
}

pub(crate) fn is_transport_only_message(message: &crate::session::SessionMessage) -> bool {
    if message.role.eq_ignore_ascii_case("tool") {
        return true;
    }
    if !message.role.eq_ignore_ascii_case("assistant") {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&message.content)
        .ok()
        .is_some_and(|value| {
            let has_tool_calls = value
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|calls| !calls.is_empty());
            let has_visible_content = value
                .get("content")
                .is_some_and(|content| !content.is_null() && content.as_str() != Some(""));
            has_tool_calls && !has_visible_content
        })
}

pub(crate) fn approved_plan_continuation<'a>(
    session: &Session,
    message: &'a crate::session::SessionMessage,
) -> Option<&'a str> {
    let step = message
        .role
        .eq_ignore_ascii_case("user")
        .then(|| {
            message
                .content
                .strip_prefix("Continue with approved plan step: ")
        })
        .flatten()?;
    session
        .plan
        .as_ref()
        .is_some_and(|plan| {
            plan.steps
                .iter()
                .any(|candidate| candidate.description == step)
        })
        .then_some(step)
}

/// Project only human-visible conversation messages for compact history surfaces.
/// Provider transport envelopes, tool-result payloads, and runtime-authored
/// continuation prompts remain private even when loading legacy sessions.
pub fn project_session_conversation(
    session: &Session,
    sensitive_values: &[String],
) -> Vec<ActivityEntry> {
    session
        .messages
        .iter()
        .filter(|message| !is_transport_only_message(message))
        .filter(|message| approved_plan_continuation(session, message).is_none())
        .map(|message| project_session_message(message, sensitive_values))
        .collect()
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub fn project_session_activities(
    session: &Session,
    sensitive_values: &[String],
) -> Vec<ActivityEntry> {
    let mut activities = Vec::new();
    if let Some(name) = session.display_name.as_deref() {
        activities.push(ActivityEntry::new(
            ActivityKind::System,
            format!("session {name}"),
            String::new(),
        ));
    }
    if let Some(parent) = session.forked_from.as_deref() {
        activities.push(ActivityEntry::new(
            ActivityKind::System,
            format!("forked from {parent}"),
            String::new(),
        ));
    }
    let mut persisted = Vec::new();
    for message in &session.messages {
        if is_transport_only_message(message) {
            continue;
        }
        if approved_plan_continuation(session, message).is_some() {
            continue;
        }
        let activity = project_session_message(message, sensitive_values);
        persisted.push(PersistedActivity {
            timestamp: message.timestamp,
            source_rank: 0,
            source_index: message.index,
            activity,
        });
    }
    let event_start = session
        .events
        .len()
        .saturating_sub(MAX_PROJECTED_SESSION_EVENTS);
    if event_start > 0 {
        persisted.push(PersistedActivity {
            timestamp: None,
            source_rank: 1,
            source_index: 0,
            activity: ActivityEntry::new(
                ActivityKind::System,
                format!("{event_start} earlier session event(s) omitted"),
                String::new(),
            ),
        });
    }
    let projected_events = &session.events[event_start..];
    for (offset, event) in projected_events.iter().enumerate() {
        if event.kind == "run_terminal"
            && terminal_has_projected_reconciliation(&projected_events[..offset], event)
        {
            continue;
        }
        if let Some(activity) = project_session_event(event, sensitive_values) {
            persisted.push(PersistedActivity {
                timestamp: event.timestamp,
                source_rank: 1,
                source_index: event.index.saturating_add(1),
                activity,
            });
        }
    }
    for (index, call) in session.tool_calls.iter().enumerate() {
        let name = call
            .tool_name
            .as_deref()
            .map(bounded_activity_label)
            .unwrap_or_else(|| "tool".to_string());
        let status = if call.error.is_some() { "failed" } else { "ok" };
        let body = if call.error.is_some() {
            "recorded tool error; inspect the bounded session audit for details"
        } else {
            ""
        };
        persisted.push(PersistedActivity {
            timestamp: call.timestamp,
            source_rank: 2,
            source_index: index,
            activity: ActivityEntry::new(
                ActivityKind::Tool,
                format!("{name} {status}"),
                body.to_string(),
            )
            .folded(),
        });
    }
    persisted.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.source_rank.cmp(&right.source_rank))
            .then_with(|| left.source_index.cmp(&right.source_index))
    });
    activities.extend(persisted.into_iter().map(|entry| entry.activity));
    if let Some(plan) = session.plan.as_ref().filter(|plan| plan.steps.len() > 1) {
        activities.push(plan_activity(plan, sensitive_values));
    }
    if let Some(summary) = &session.summary {
        activities.push(
            ActivityEntry::new(
                ActivityKind::Compression,
                format!("summarized through message {}", session.summary_index),
                bounded_activity_body(summary, sensitive_values),
            )
            .folded(),
        );
    }
    for activity in &mut activities {
        sanitize_activity(activity, sensitive_values);
    }
    activities
}

pub(crate) fn terminal_has_projected_reconciliation(
    events: &[SessionEvent],
    terminal: &SessionEvent,
) -> bool {
    let Some(run_id) = terminal
        .details
        .get("run_id")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let Some(outcome) = safe_event_atom(&terminal.details, "outcome") else {
        return false;
    };
    let Some(start) = events.iter().rposition(|event| event.kind == "run_started") else {
        return false;
    };
    if run_id.is_empty()
        || events[start]
            .details
            .get("run_id")
            .and_then(serde_json::Value::as_str)
            != Some(run_id)
    {
        return false;
    }
    let run_events = &events[start + 1..];
    if run_events.iter().any(|event| event.kind == "run_terminal") {
        return false;
    }
    // Reconciliation records predate exact run IDs. Their enclosing admitted run
    // proves ownership, but an explicit conflicting ID must never be ignored.
    run_events
        .iter()
        .rev()
        .find(|event| event.kind == "reconciliation")
        .is_some_and(|event| {
            event
                .details
                .get("run_id")
                .is_none_or(|id| id.as_str() == Some(run_id))
                && event
                    .details
                    .get("continue")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
                && safe_event_atom(&event.details, "outcome").as_deref() == Some(outcome.as_str())
        })
}

pub(crate) fn sanitize_activity(activity: &mut ActivityEntry, sensitive_values: &[String]) {
    activity.title = bounded_public_text(
        &activity.title,
        sensitive_values,
        MAX_ACTIVITY_LABEL_BYTES,
        false,
    );
    activity.body = bounded_public_text(
        &activity.body,
        sensitive_values,
        MAX_ACTIVITY_BODY_BYTES,
        true,
    );
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub fn apply_stream_event(
    activities: &mut Vec<ActivityEntry>,
    event: StreamEvent,
    live_state: &mut Option<String>,
    sensitive_values: &[String],
) {
    let terminal_event = match &event {
        StreamEvent::End(_) => true,
        StreamEvent::Reconciled { outcome } => {
            !matches!(outcome.as_str(), "step_completed" | "verification_recovery")
        }
        _ => false,
    };
    if terminal_event {
        close_unfinished_tool_activities(activities);
    }
    if matches!(
        &event,
        StreamEvent::StateTransition { .. } | StreamEvent::Reconciled { .. } | StreamEvent::End(_)
    ) {
        // Quiet lifecycle events still end the current assistant turn. Otherwise
        // the next plan step's content would append to the preceding response.
        if let Some(last) = activities.last_mut() {
            if last.kind == ActivityKind::Assistant && last.title == "live" {
                last.title.clear();
            }
        }
    }
    match event {
        StreamEvent::StateTransition { state } => {
            *live_state = Some(state.clone());
            if is_thought_state(&state) {
                upsert_thought_activity(activities, &state);
            }
        }
        StreamEvent::Content(content) => {
            if let Some(last) = activities.last_mut() {
                if last.kind == ActivityKind::Assistant && last.title == "live" {
                    last.body.push_str(&content);
                    last.body = bounded_activity_body(&last.body, sensitive_values);
                    sanitize_activity(last, sensitive_values);
                    return;
                }
            }
            freeze_open_thought(activities);
            activities.push(ActivityEntry::new(
                ActivityKind::Assistant,
                "live",
                bounded_activity_body(&content, sensitive_values),
            ));
        }
        StreamEvent::ToolCallChunk {
            invocation_id,
            name: Some(name),
            arguments,
            ..
        } if !name.is_empty() => {
            freeze_open_thought(activities);
            let hint = tool_argument_summary(&name, arguments.as_deref());
            let detail = tool_argument_detail(&name, arguments.as_deref());
            upsert_tool_activity(activities, invocation_id, &name, "requested", detail, &hint)
        }
        StreamEvent::ToolCallChunk { .. } => {}
        StreamEvent::PlanGenerated { .. } => {
            freeze_open_thought(activities);
        }
        StreamEvent::PlanProgress(progress) => {
            if progress.steps.len() > 1 {
                let activity = plan_progress_activity(&progress, sensitive_values);
                if let Some(previous) = activities.iter_mut().rev().find(|entry| {
                    entry.kind == ActivityKind::Plan
                        && entry.plan_id.as_deref() == Some(progress.plan_id.as_str())
                }) {
                    *previous = activity;
                } else {
                    activities.push(activity);
                }
            }
        }
        StreamEvent::ApprovalRequired { tool_name } => {
            let tool_name = bounded_status_value(&crate::tools::executor::redact_text(&tool_name));
            activities.push(ActivityEntry::new(
                ActivityKind::Approval,
                format!("{tool_name} needs approval"),
                "Waiting for Y to approve once or N to deny.",
            ));
        }
        StreamEvent::QuestionRequired { question, options } => {
            let options = if options.is_empty() {
                String::new()
            } else {
                format!("options: {}", options.join(" | "))
            };
            activities.push(ActivityEntry::new(
                ActivityKind::Question,
                question,
                options,
            ));
        }
        StreamEvent::ToolStarted {
            invocation_id,
            tool_name,
        } => {
            freeze_open_thought(activities);
            let tool_name = bounded_status_value(&crate::tools::executor::redact_text(&tool_name));
            upsert_tool_activity(
                activities,
                invocation_id,
                &tool_name,
                "running",
                String::new(),
                "",
            );
        }
        StreamEvent::TerminalOutput {
            invocation_id,
            chunk,
            ..
        } => {
            if let Some(last) = find_tool_activity_mut(activities, invocation_id) {
                if !last.body.is_empty() && !last.body.ends_with('\n') {
                    last.body.push('\n');
                }
                last.body.push_str(chunk.trim_end_matches(['\r', '\n']));
                last.body = bounded_activity_body(&last.body, sensitive_values);
            }
        }
        StreamEvent::ToolCompleted {
            invocation_id,
            tool_name,
            success,
            output,
            error,
        } => {
            let (status, summary, detail) =
                summarize_tool_result(&tool_name, success, output.as_ref(), error.as_deref());
            let body = bounded_activity_body(&detail, sensitive_values);
            if let Some(existing) = find_tool_activity_mut(activities, invocation_id) {
                let hint = tool_arg_hint_from_title(&existing.title);
                existing.title = compose_tool_title(&tool_name, &status, &hint, &summary);
                existing.body = merge_tool_body(&existing.body, &body);
                existing.folded = !existing.body.is_empty();
            } else {
                activities.push(
                    ActivityEntry::new(
                        ActivityKind::Tool,
                        compose_tool_title(&tool_name, &status, "", &summary),
                        body,
                    )
                    .for_tool_invocation(invocation_id)
                    .folded(),
                );
            }
        }
        StreamEvent::Compression {
            before_tokens,
            after_tokens,
            summarized_through,
        } => activities.push(
            ActivityEntry::new(
                ActivityKind::Compression,
                format!("{before_tokens} -> {after_tokens} tokens"),
                format!("summarized through message {summarized_through}"),
            )
            .folded(),
        ),
        StreamEvent::Reconciled { outcome } if outcome == "step_completed" => {}
        StreamEvent::Reconciled { outcome } if outcome == "verification_recovery" => {}
        StreamEvent::Reconciled { outcome } if outcome == "instruction_context_missing" => {
            activities.push(ActivityEntry::new(
                ActivityKind::Failure,
                "project instructions unavailable".to_string(),
                "Restore readable project instructions within the size limit, or increase llm.context_length, then retry the same plan.".to_string(),
            ));
        }
        StreamEvent::Reconciled { outcome } => {
            let reduction = reduce_interaction(
                &InteractionState::default(),
                InteractionInput::ReconciledOutcome {
                    outcome: &outcome,
                    failure: false,
                },
            );
            let (outcome, terminal) = match reduction {
                InteractionReduction::Reconciled { outcome, terminal } => (outcome, terminal),
                _ => unreachable!("reconciliation input always has a terminal reduction"),
            };
            let message = terminal_outcome_message(&outcome);
            activities.push(ActivityEntry::new(
                if terminal == InteractionTerminalOutcome::Failed {
                    ActivityKind::Failure
                } else {
                    ActivityKind::Reconcile
                },
                message.title.to_string(),
                message.detail.to_string(),
            ));
        }
        StreamEvent::Failure {
            failure,
            session_id,
        } => {
            let report = failure.user_report(session_id.as_deref());
            let (title, body) = report
                .split_once('\n')
                .map_or((report.as_str(), ""), |(title, body)| (title, body));
            activities.push(ActivityEntry::new(
                ActivityKind::Failure,
                title.to_string(),
                bounded_activity_body(body, sensitive_values),
            ));
        }
        StreamEvent::End(reason) => {
            let reduction = reduce_interaction(
                &InteractionState::default(),
                InteractionInput::ReconciledOutcome {
                    outcome: &reason,
                    failure: false,
                },
            );
            let terminal = match reduction {
                InteractionReduction::Reconciled { terminal, .. } => terminal,
                _ => InteractionTerminalOutcome::Failed,
            };
            let message = terminal_outcome_message(&reason);
            let kind = if terminal == InteractionTerminalOutcome::Cancelled {
                ActivityKind::Cancellation
            } else if terminal == InteractionTerminalOutcome::Failed {
                ActivityKind::Failure
            } else {
                ActivityKind::System
            };
            activities.push(ActivityEntry::new(
                kind,
                message.title.to_string(),
                message.detail.to_string(),
            ));
        }
    }
    if let Some(last) = activities.last_mut() {
        sanitize_activity(last, sensitive_values);
    }
}

fn close_unfinished_tool_activities(activities: &mut [ActivityEntry]) {
    for entry in activities
        .iter_mut()
        .filter(|entry| entry.kind == ActivityKind::Tool)
    {
        let Some((name, rest)) = entry.title.split_once(' ') else {
            continue;
        };
        let (phase, _) = rest.split_once(" · ").unwrap_or((rest, ""));
        let detail = match phase {
            "requested" => "The tool was proposed but did not run.",
            "running" => "The run stopped before a tool result was recorded.",
            _ => continue,
        };
        let hint = tool_arg_hint_from_title(&entry.title);
        entry.title = compose_tool_title(name, "stopped", &hint, "");
        entry.body = merge_tool_body(&entry.body, detail);
        entry.folded = true;
    }
}

pub(crate) fn find_tool_activity_mut(
    activities: &mut [ActivityEntry],
    invocation_id: ToolInvocationId,
) -> Option<&mut ActivityEntry> {
    activities.iter_mut().rev().find(|entry| {
        entry.kind == ActivityKind::Tool && entry.tool_invocation_id == Some(invocation_id)
    })
}

pub(crate) fn tool_arg_hint_from_title(title: &str) -> String {
    title
        .split(" · ")
        .skip(1)
        .filter(|part| !is_tool_result_summary(part))
        .collect::<Vec<_>>()
        .join(" · ")
}

pub(crate) fn is_tool_result_summary(part: &str) -> bool {
    part.starts_with("exit ")
        || part.ends_with(" entries")
        || part.ends_with(" entries, truncated")
        || part.ends_with(" lines")
        || part.ends_with(" matches")
}

pub(crate) fn compose_tool_title(name: &str, phase: &str, hint: &str, result: &str) -> String {
    let mut title = format!("{name} {phase}");
    if !hint.is_empty() {
        title.push_str(" · ");
        title.push_str(hint);
    }
    if !result.is_empty() {
        title.push_str(" · ");
        title.push_str(result);
    }
    title
}

pub(crate) const TOOL_TITLE_HINT_CELLS: usize = 96;
pub(crate) const RUN_TERMINAL_TITLE_HINT_CELLS: usize = 120;

pub(crate) fn json_arg(value: &serde_json::Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value.get(*key).and_then(|item| {
                item.as_str()
                    .map(ToString::to_string)
                    .or_else(|| (!item.is_null()).then(|| item.to_string()))
            })
        })
        .unwrap_or_default()
}

pub fn tool_argument_summary(tool_name: &str, arguments: Option<&str>) -> String {
    let Some(raw) = arguments.filter(|value| !value.is_empty()) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return String::new();
    };
    let (hint, max_cells) = match tool_name {
        "read_file" | "list_directory" | "search_replace" => (
            json_arg(&value, &["path", "target_file", "file_path"]),
            TOOL_TITLE_HINT_CELLS,
        ),
        "run_terminal" => {
            let command = json_arg(&value, &["command"]);
            let cwd = json_arg(&value, &["cwd", "working_directory", "workdir"]);
            let hint = if cwd.is_empty() {
                command
            } else if command.is_empty() {
                format!("in {cwd}")
            } else {
                format!("{command}  in {cwd}")
            };
            (hint, RUN_TERMINAL_TITLE_HINT_CELLS)
        }
        "grep" => {
            let pattern = json_arg(&value, &["pattern", "query"]);
            let path = json_arg(&value, &["path"]);
            let hint = [pattern, path]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            (hint, TOOL_TITLE_HINT_CELLS)
        }
        "apply_patch" => ("patch".to_string(), TOOL_TITLE_HINT_CELLS),
        _ => (
            json_arg(&value, &["path", "command", "query", "pattern", "url"]),
            TOOL_TITLE_HINT_CELLS,
        ),
    };
    if hint.is_empty() {
        return String::new();
    }
    truncate_display_cells(
        &bounded_status_value(&crate::tools::executor::redact_text(&hint)),
        max_cells,
    )
}

pub(crate) fn tool_argument_detail(tool_name: &str, arguments: Option<&str>) -> String {
    let Some(raw) = arguments.filter(|value| !value.is_empty()) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return String::new();
    };
    if tool_name != "run_terminal" {
        return String::new();
    }
    let command = crate::tools::executor::redact_text(&json_arg(&value, &["command"]));
    let cwd = crate::tools::executor::redact_text(&json_arg(
        &value,
        &["cwd", "working_directory", "workdir"],
    ));
    let background = value
        .get("background")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut lines = Vec::new();
    if !command.is_empty() {
        lines.push(command);
    }
    if !cwd.is_empty() {
        lines.push(format!("in {cwd}"));
    }
    if background {
        lines.push("background".to_string());
    }
    lines.join("\n")
}

pub(crate) fn tool_body_prefix(body: &str) -> String {
    let mut prefix = Vec::new();
    for line in body.lines() {
        if prefix.is_empty() || line.starts_with("in ") || line == "background" {
            prefix.push(line);
        } else {
            break;
        }
    }
    prefix.join("\n")
}

pub(crate) fn merge_tool_body(existing: &str, output: &str) -> String {
    let prefix = tool_body_prefix(existing);
    if prefix.is_empty() {
        return output.to_string();
    }
    if output.is_empty() {
        return prefix;
    }
    if output.starts_with(&prefix) || existing.contains(output) {
        return if existing.len() >= output.len() {
            existing.to_string()
        } else {
            output.to_string()
        };
    }
    format!("{prefix}\n{output}")
}

pub(crate) fn upsert_tool_activity(
    activities: &mut Vec<ActivityEntry>,
    invocation_id: ToolInvocationId,
    tool_name: &str,
    phase: &str,
    body: String,
    arg_hint: &str,
) {
    if let Some(existing) = find_tool_activity_mut(activities, invocation_id) {
        let hint = if arg_hint.is_empty() {
            tool_arg_hint_from_title(&existing.title)
        } else {
            arg_hint.to_string()
        };
        existing.title = compose_tool_title(tool_name, phase, &hint, "");
        if !body.is_empty() {
            existing.body = merge_tool_body(&existing.body, &body);
        }
        existing.folded = matches!(phase, "requested" | "running") || !existing.body.is_empty();
        return;
    }
    let mut entry = ActivityEntry::new(
        ActivityKind::Tool,
        compose_tool_title(tool_name, phase, arg_hint, ""),
        body,
    )
    .for_tool_invocation(invocation_id);
    entry.folded = matches!(phase, "requested" | "running") || !entry.body.is_empty();
    activities.push(entry);
}

pub fn summarize_tool_result(
    tool_name: &str,
    success: bool,
    output: Option<&serde_json::Value>,
    error: Option<&str>,
) -> (String, String, String) {
    let status = if success { "ok" } else { "failed" };
    let (summary, mut detail) = match (tool_name, output) {
        ("list_directory", Some(value)) => {
            let count = value
                .get("entries")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .or_else(|| {
                    value
                        .get("entries_scanned")
                        .and_then(serde_json::Value::as_u64)
                        .map(|count| count as usize)
                })
                .unwrap_or(0);
            let truncated = value
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let extra = if truncated { ", truncated" } else { "" };
            (format!("{count} entries{extra}"), inline_json(value))
        }
        ("read_file", Some(value)) => {
            let content = value
                .get("content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let lines = if content.is_empty() {
                0
            } else {
                content.lines().count()
            };
            (format!("{lines} lines"), inline_json(value))
        }
        ("grep", Some(value)) => {
            let matches = value
                .get("matches")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            (format!("{matches} matches"), inline_json(value))
        }
        ("run_terminal", Some(value)) => {
            let code = value
                .get("exit_code")
                .or_else(|| value.get("status"))
                .map(|value| value.to_string())
                .unwrap_or_else(|| {
                    if success {
                        "0".to_string()
                    } else {
                        "error".to_string()
                    }
                });
            let output_text = value
                .get("stdout")
                .or_else(|| value.get("output"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            (format!("exit {code}"), output_text.to_string())
        }
        _ => {
            let detail = match (output, error) {
                (Some(output), Some(error)) => format!("{}; error: {error}", inline_json(output)),
                (Some(output), None) => inline_json(output),
                (None, Some(error)) => error.to_string(),
                (None, None) => String::new(),
            };
            (String::new(), detail)
        }
    };
    if let Some(error) = error {
        if !detail.contains(error) {
            if detail.is_empty() {
                detail = error.to_string();
            } else {
                detail = format!("{detail}; error: {error}");
            }
        }
    }
    (status.to_string(), summary, detail)
}

pub(crate) const TODO_PENDING: char = '○';
pub(crate) const TODO_ACTIVE: char = '◐';
pub(crate) const TODO_DONE: char = '✓';
pub(crate) const TODO_BLOCKED: char = '!';
pub(crate) const TODO_CANCELLED: char = '×';

pub(crate) fn todo_plan_title(count: usize, complete: bool) -> String {
    let noun = if count == 1 { "to-do" } else { "to-dos" };
    if complete {
        format!("Completed {count} {noun}")
    } else {
        format!("Working on {count} {noun}")
    }
}

pub(crate) fn todo_marker_for_step(status: &str) -> char {
    if status.eq_ignore_ascii_case("Completed") {
        TODO_DONE
    } else if status.eq_ignore_ascii_case("Cancelled") {
        TODO_CANCELLED
    } else if status.eq_ignore_ascii_case("Blocked") {
        TODO_BLOCKED
    } else if status.eq_ignore_ascii_case("InProgress") {
        TODO_ACTIVE
    } else {
        TODO_PENDING
    }
}

pub(crate) fn plan_progress_from_plan(
    plan: &crate::session::Plan,
    sensitive_values: &[String],
) -> crate::llm::PlanProgress {
    crate::llm::PlanProgress {
        plan_id: plan.id.clone(),
        current_step_index: plan.current_step_index,
        complete: plan.is_complete(),
        steps: plan
            .steps
            .iter()
            .map(|step| crate::llm::PlanProgressStep {
                description: bounded_public_text(
                    &step.description,
                    sensitive_values,
                    MAX_ACTIVITY_BODY_BYTES,
                    false,
                ),
                status: step.status.clone(),
                verification: step
                    .verification_obligations
                    .iter()
                    .map(|obligation| crate::llm::PlanVerificationProgress {
                        id: bounded_public_text(&obligation.id, sensitive_values, 128, false),
                        status: obligation.status,
                        authority: obligation.authority,
                        unresolved: obligation.is_unresolved_required(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

pub(crate) fn plan_activity(
    plan: &crate::session::Plan,
    sensitive_values: &[String],
) -> ActivityEntry {
    plan_progress_activity(
        &plan_progress_from_plan(plan, sensitive_values),
        sensitive_values,
    )
}

pub(crate) fn plan_progress_activity(
    progress: &crate::llm::PlanProgress,
    sensitive_values: &[String],
) -> ActivityEntry {
    let body = progress
        .steps
        .iter()
        .flat_map(|step| {
            let marker = todo_marker_for_step(&step.status);
            let mut lines = vec![format!("{marker} {}", step.description.trim())];
            lines.extend(step.verification.iter().map(|obligation| {
                format!(
                    "   verify {} [{}; {}]",
                    obligation.id,
                    verification_status_label(obligation.status),
                    verification_authority_label(obligation.authority),
                )
            }));
            lines
        })
        .collect::<Vec<_>>()
        .join("\n");
    let unresolved = progress
        .steps
        .get(progress.current_step_index)
        .map(|step| {
            step.verification
                .iter()
                .filter(|obligation| obligation.unresolved)
                .count()
        })
        .unwrap_or(0);
    let blocked = progress.steps.iter().any(|step| step.status == "Blocked");
    let cancelled = progress.steps.iter().any(|step| step.status == "Cancelled");
    let noun = if progress.steps.len() == 1 {
        "to-do"
    } else {
        "to-dos"
    };
    let mut title = if cancelled {
        format!("Stopped {} {noun}", progress.steps.len())
    } else if blocked {
        format!("Blocked {} {noun}", progress.steps.len())
    } else if !progress.complete && progress.steps.iter().all(|step| step.status == "Pending") {
        format!("Planned {} {noun}", progress.steps.len())
    } else {
        todo_plan_title(progress.steps.len(), progress.complete)
    };
    if unresolved > 0 {
        title.push_str(&format!(" · {unresolved} verification pending"));
    }
    let mut activity = ActivityEntry::new(
        ActivityKind::Plan,
        title,
        bounded_activity_body(&body, sensitive_values),
    );
    activity.plan_id = Some(progress.plan_id.clone());
    activity
}

pub(crate) fn verification_status_label(
    status: crate::session::VerificationStatus,
) -> &'static str {
    use crate::session::VerificationStatus;
    match status {
        VerificationStatus::Pending => "pending",
        VerificationStatus::Running => "running",
        VerificationStatus::Passed => "passed",
        VerificationStatus::Failed => "failed",
        VerificationStatus::Cancelled => "cancelled",
        VerificationStatus::Stale => "stale",
        VerificationStatus::Waived => "waived",
    }
}

pub(crate) fn verification_authority_label(
    authority: crate::session::VerificationAuthority,
) -> &'static str {
    use crate::session::VerificationAuthority;
    match authority {
        VerificationAuthority::Human => "human",
        VerificationAuthority::Project => "project",
        VerificationAuthority::ApprovedPlan => "approved plan",
    }
}

pub(crate) fn format_verification_status(
    session: Option<&Session>,
    sensitive_values: &[String],
) -> String {
    let Some(plan) = session.and_then(|session| session.plan.as_ref()) else {
        return "Verification: none".to_string();
    };
    let obligations = plan
        .steps
        .iter()
        .flat_map(|step| step.verification_obligations.iter())
        .collect::<Vec<_>>();
    if obligations.is_empty() {
        return "Verification: none".to_string();
    }
    let mut output = format!("Verification: {} requirement(s)", obligations.len());
    for obligation in obligations.into_iter().take(16) {
        output.push_str(&format!(
            "\n  - {} | {} | {}{}",
            bounded_sensitive_status_value(&obligation.id, sensitive_values),
            verification_status_label(obligation.status),
            verification_authority_label(obligation.authority),
            obligation
                .reason
                .as_deref()
                .map(|reason| {
                    format!(
                        " | {}",
                        bounded_sensitive_status_value(reason, sensitive_values)
                    )
                })
                .unwrap_or_default(),
        ));
    }
    bounded_public_text(&output, sensitive_values, 4_096, true)
}

pub(crate) fn bounded_activity_body(content: &str, sensitive_values: &[String]) -> String {
    bounded_public_text(content, sensitive_values, MAX_ACTIVITY_BODY_BYTES, true)
}

/// Redacts configured secret spellings, removes terminal-active controls, and returns a bounded
/// UTF-8 presentation value. `preserve_layout` permits only newline and tab layout controls; it
/// never permits carriage return, escape, bidi controls, or other terminal-active bytes.
pub fn bounded_public_text(
    value: &str,
    sensitive_values: &[String],
    max_bytes: usize,
    preserve_layout: bool,
) -> String {
    let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
        value,
        sensitive_values.iter().cloned(),
    );
    let sanitized = control_safe_text(&redacted, preserve_layout);
    if sanitized.len() <= max_bytes {
        return sanitized;
    }
    let mut end = max_bytes.saturating_sub(3);
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &sanitized[..end])
}

pub(crate) fn control_safe_text(value: &str, preserve_layout: bool) -> String {
    value
        .chars()
        .map(|character| {
            let bidi_control = matches!(
                character,
                '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            );
            if (character.is_control() || bidi_control)
                && !(preserve_layout && matches!(character, '\n' | '\t'))
            {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

pub(crate) fn bounded_status_value(value: &str) -> String {
    bounded_public_text(value, &[], MAX_STATUS_VALUE_BYTES, false)
}

pub(crate) fn bounded_sensitive_status_value(value: &str, sensitive_values: &[String]) -> String {
    bounded_public_text(value, sensitive_values, MAX_STATUS_VALUE_BYTES, false)
}

pub(crate) fn sandbox_posture_label(posture: &EffectiveExecutionPosture) -> &'static str {
    match &posture.sandbox_route {
        crate::sandbox::SandboxExecutionRoute::Bwrap => "bwrap available",
        crate::sandbox::SandboxExecutionRoute::Direct => "direct fallback (no platform sandbox)",
        crate::sandbox::SandboxExecutionRoute::FailClosed(_) => "FAIL CLOSED (sandbox unavailable)",
    }
}

pub(crate) fn instruction_posture_label(posture: &EffectiveExecutionPosture) -> &'static str {
    match posture.instruction_posture {
        InstructionExecutionPosture::Configured => "configured",
        InstructionExecutionPosture::Tightened => "tightened by project instructions",
        InstructionExecutionPosture::InvalidFailClosed => {
            "INVALID project directive; execution FAILS CLOSED"
        }
    }
}

pub(crate) fn format_effective_execution_posture(posture: &EffectiveExecutionPosture) -> String {
    let warning = if posture.broad_or_off {
        "\nWARNING: BROADER/OFF posture selected or direct execution is effective; stronger per-action controls still apply."
    } else {
        ""
    };
    format!(
        "Configured approval preset: {}\nEffective execution posture:\n  approval behavior: {}\n  provider: {}\n  profile: {}\n  network: {}\n  mutation plan gate: {}\n  mutation managed-owned-worktree gate: {}\n  platform sandbox: {}\n  instruction boundary: {}\nPer-action AGENTS.md, skill, tool policy, worktree, and platform controls may only tighten this posture; the configured preset never overrides them.{warning}",
        bounded_status_value(&posture.configured_approval_preset),
        posture.effective_approval_mode,
        bounded_status_value(&posture.provider),
        bounded_status_value(&posture.profile),
        bounded_status_value(&posture.network),
        if posture.mutation_plan_gate { "required" } else { "not configured" },
        if posture.mutation_owned_worktree_gate { "required for tool-declared mutations" } else { "not required" },
        sandbox_posture_label(posture),
        instruction_posture_label(posture),
    )
}

pub(crate) fn persisted_context_usage(session: Option<&Session>, context_limit: usize) -> usize {
    let Some(session) = session else {
        return 0;
    };
    if let Some(snapshot) = crate::context::snapshot::latest_from_session(session) {
        return snapshot
            .estimated_input
            .min(snapshot.budget.window.max(context_limit));
    }
    bounded_session_context(session, context_limit).approximate_tokens
}

pub(crate) fn abbreviated_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

pub(crate) fn truncate_display_cells(value: &str, max_cells: usize) -> String {
    if unicode_display_width(value) <= max_cells {
        return value.to_string();
    }
    if max_cells == 0 {
        return String::new();
    }
    let content_cells = max_cells.saturating_sub(1);
    let mut truncated = String::new();
    for grapheme in value.graphemes(true) {
        let mut candidate = truncated.clone();
        candidate.push_str(grapheme);
        if unicode_display_width(&candidate) > content_cells {
            break;
        }
        truncated.push_str(grapheme);
    }
    truncated.push('…');
    truncated
}

pub(crate) fn folder_label(path: &Path) -> String {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if let Some(home) = home.as_ref() {
        if let Ok(rest) = path.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

pub(crate) fn chrome_branch_label(branch: &str, session_id: &str) -> String {
    if (!session_id.is_empty() && branch.contains(session_id)) || branch.starts_with("nib/session/")
    {
        "session".to_string()
    } else {
        branch.to_string()
    }
}

pub(crate) fn git_head_branch(cwd: &Path) -> String {
    let output = crate::sandbox::worktree::run_git_bounded_sync_with_timeout(
        cwd,
        ["rev-parse", "--abbrev-ref", "HEAD"],
        Duration::from_millis(400),
    );
    match output {
        Ok(output) if output.status.success() => {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if name.is_empty() {
                "-".to_string()
            } else {
                name
            }
        }
        _ => "-".to_string(),
    }
}

/// Compact TUI chrome: folder/branch on the header, model/context on the header,
/// approval mode and agent mode on the footer. `/status` keeps the full diagnostic
/// contract via [`format_interaction_chrome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiChrome {
    pub folder: String,
    pub branch: String,
    pub model: String,
    pub context: String,
    pub approval: String,
    pub agent_mode: String,
}

impl TuiChrome {
    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self {
            folder: "workspace".to_string(),
            branch: "main".to_string(),
            model: "mock-model".to_string(),
            context: "ctx ?".to_string(),
            approval: "manual".to_string(),
            agent_mode: "idle".to_string(),
        }
    }

    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            folder: bounded_status_value(&message.into()),
            branch: "-".to_string(),
            model: "-".to_string(),
            context: "ctx ?".to_string(),
            approval: "-".to_string(),
            agent_mode: "idle".to_string(),
        }
    }
}

/// Formats the compact TUI chrome fields. The complete diagnostic contract
/// intentionally remains in [`format_interaction_chrome`] for `/status`.
pub fn format_tui_interaction_chrome(
    project_root: &Path,
    session: Option<&Session>,
    session_id: &str,
) -> Result<TuiChrome, String> {
    let config = load_nib_config_full(project_root)
        .map_err(|error| bounded_status_value(&error.to_string()))?;
    let sensitive_values = config.public_session_sensitive_values();
    let diagnostics = provider_diagnostics(&config.llm, None).map_err(|error| {
        format!(
            "failed resolving LLM transport: {}",
            bounded_sensitive_status_value(&error, &sensitive_values)
        )
    })?;
    let posture = ToolExecutor::effective_execution_posture(
        project_root,
        config.execution.clone(),
        &config.approvals,
    );
    let folder = bounded_sensitive_status_value(&folder_label(project_root), &sensitive_values);
    let managed_worktree = crate::integrations::worktree::with_validated_session_worktree(
        project_root,
        session_id,
        |path| Ok(path.to_path_buf()),
    )?;
    let git_cwd = match managed_worktree {
        Some(path) => path,
        None => project_root.to_path_buf(),
    };
    let branch = bounded_sensitive_status_value(
        &chrome_branch_label(&git_head_branch(&git_cwd), session_id),
        &sensitive_values,
    );
    let model = bounded_sensitive_status_value(&diagnostics.model, &sensitive_values);
    let snapshot = session.and_then(crate::context::snapshot::latest_from_session);
    let window = snapshot
        .as_ref()
        .map(|snapshot| snapshot.budget.window)
        .unwrap_or(config.llm.context_length);
    let context_used = persisted_context_usage(session, window);
    let context = crate::context::snapshot::occupancy_indicator(
        context_used,
        window,
        crate::context::snapshot::OccupancyStyle::Compact,
    );
    let approval = posture.effective_approval_mode.to_string();
    Ok(TuiChrome {
        folder,
        branch,
        model,
        context,
        approval,
        agent_mode: "idle".to_string(),
    })
}
