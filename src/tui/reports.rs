//! TUI internals split for T043 module size.

use super::*;

pub(crate) struct ApprovalPrompt {
    pub(crate) statement: String,
    pub(crate) subject: String,
    pub(crate) location: Option<String>,
}

pub(crate) fn safe_approval_text(value: &str) -> String {
    control_safe_text(&crate::tools::executor::redact_text(value), true)
}

pub(crate) fn compact_approval_risk(value: &str) -> String {
    value
        .split(" / ")
        .next()
        .unwrap_or(value)
        .trim()
        .to_string()
}

pub(crate) fn approval_prompt(call: &ToolCall, context: &ApprovalContext) -> ApprovalPrompt {
    let statement = match call.tool_name.as_str() {
        "run_terminal" => "Run this command".to_string(),
        "read_file" => "Read this file".to_string(),
        "list_directory" => "List this directory".to_string(),
        "search_replace" => "Edit this file".to_string(),
        "apply_patch" => "Apply a patch".to_string(),
        "grep" => "Search files".to_string(),
        "approve_plan" => "Approve this plan".to_string(),
        "merge_subagent_worktree" => "Merge a subagent worktree".to_string(),
        other => format!("Use {other}"),
    };
    ApprovalPrompt {
        statement,
        subject: context.display_subject.clone(),
        location: context.display_location.clone(),
    }
}

pub(crate) fn usable_approval_location(value: Option<String>, scope: &str) -> Option<String> {
    let skip = |value: &str| {
        value.is_empty()
            || value == "."
            || value.contains("not available")
            || value.contains("not classified")
    };
    if let Some(location) = value.filter(|location| !skip(location)) {
        return Some(location);
    }
    if skip(scope) {
        None
    } else if scope.starts_with('/') || scope.starts_with('.') {
        Some(safe_approval_text(scope))
    } else {
        None
    }
}

pub(crate) fn approval_dock_style(no_color: bool) -> Style {
    let style = Style::default().add_modifier(Modifier::BOLD);
    if no_color {
        style
    } else {
        style.fg(ratatui::style::Color::Yellow)
    }
}

pub(crate) fn tui_report_cancelled_run(
    store: &SessionStore,
    session_id: &str,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    tui_report_reconciled_shutdown(store, session_id, timeline, false)
}

pub(crate) fn tui_report_quit_run(
    store: &SessionStore,
    session_id: &str,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    tui_report_reconciled_shutdown(store, session_id, timeline, true)
}

pub(crate) fn tui_report_reconciled_shutdown(
    store: &SessionStore,
    session_id: &str,
    timeline: &mut ActiveTimeline,
    quitting: bool,
) -> Result<String, String> {
    let (status, action) = match (quitting, timeline.reconciled_terminal) {
        (true, Some(InteractionTerminalOutcome::Cancelled)) => (
            "[cancelled] active agent run reconciled before quit",
            "quit after cancellation",
        ),
        (true, Some(InteractionTerminalOutcome::Completed)) => (
            "[completed] active run completed before quit",
            "quit after completion",
        ),
        (true, Some(InteractionTerminalOutcome::WaitingForInput)) => (
            "[failed] active run still required user input before quit",
            "quit with question input unavailable",
        ),
        (true, Some(InteractionTerminalOutcome::Failed)) => (
            "[failed] active run reconciled with failure before quit",
            "quit after failure",
        ),
        (true, None) => (
            "[failed] active run ended without reconciliation evidence before quit",
            "quit with reconciliation unavailable",
        ),
        (false, Some(InteractionTerminalOutcome::Cancelled)) => {
            ("[cancelled] active agent run", "cancelled")
        }
        (false, Some(InteractionTerminalOutcome::Completed)) => (
            "[completed] active run completed before cancellation",
            "completed before cancellation",
        ),
        (false, Some(InteractionTerminalOutcome::WaitingForInput)) => (
            "[failed] active run still required user input after shutdown",
            "question input unavailable",
        ),
        (false, Some(InteractionTerminalOutcome::Failed)) => {
            ("[failed] active run reconciled with failure", "failed")
        }
        (false, None) => (
            "[failed] active run ended without reconciliation evidence",
            "reconciliation unavailable",
        ),
    };
    let queue = queue_disposition_message(store, session_id, action)?;
    let report = format!("{status}; {queue}");
    timeline.push_status(report.clone());
    Ok(report)
}

pub(crate) fn tui_exit_disposition(
    store: &SessionStore,
    session_id: &str,
) -> Result<String, String> {
    queue_disposition_message(store, session_id, "exited")
}

pub(crate) fn tui_complete_session_switch(
    store: &SessionStore,
    switcher: &SessionSwitcher,
    worker_active: bool,
    active_session_id: &mut String,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    let previous = active_session_id.clone();
    let disposition = queue_disposition_message(store, &previous, "switched sessions")?;
    activate_selected_session(store, switcher, worker_active, active_session_id, timeline)?;
    timeline.push_status(disposition.clone());
    Ok(disposition)
}
