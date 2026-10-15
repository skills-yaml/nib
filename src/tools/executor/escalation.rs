//! Retrying a command outside the sandbox after the sandbox blocked it
//! (T080 phase 6, AC-7). The retry always needs a yes from a person; it is
//! never remembered and never granted by a mode, `--yes` or an allow rule.

use super::*;

/// Prompt reason shown with the command.
pub(crate) const ESCALATION_REASON: &str = "the sandbox blocked this command; retrying runs it again outside the sandbox, with full access to your files and network";

/// Added to the failure when nobody can be asked.
pub(crate) const HEADLESS_ESCALATION_NOTE: &str = "The sandbox appears to have blocked this command (a read-only file system or no network). Nobody can approve running it outside the sandbox here; run it in an interactive session, or choose --sandbox full-access.";

/// Added to the failure when commands must stay in bwrap.
pub(crate) const PINNED_SANDBOX_NOTE: &str = "The sandbox appears to have blocked this command. It is not offered outside the sandbox because execution.provider or the project instructions require bwrap.";

/// What happens after a sandboxed command failed.
pub(crate) enum Escalation {
    /// Run it again with this configuration; the user approved.
    Retry(ApprovalDecision, Box<ExecutionConfig>),
    /// Keep the failure and add this explanation.
    Note(String),
}

/// The configuration for a retry outside the sandbox: direct execution with
/// network, keeping the configured writable paths.
pub(crate) fn escalated_config(config: &ExecutionConfig) -> ExecutionConfig {
    let mut config = config.clone();
    config.provider = "internal".to_string();
    config.default_profile = "internal".to_string();
    config.boundaries.network = "enabled".to_string();
    config
}

/// Whether a terminal result is a failure inside bwrap whose output looks
/// like a sandbox block.
pub(crate) fn blocked_in_sandbox(output: &Value) -> bool {
    if output.get("command_success").and_then(Value::as_bool) != Some(false)
        || output.get("provider").and_then(Value::as_str) != Some("bwrap")
    {
        return false;
    }
    let text = ["error", "stderr", "stdout"]
        .iter()
        .filter_map(|key| output.get(*key).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    crate::sandbox::looks_blocked_by_sandbox(&text)
}

/// Appends a note to a failed terminal result's error.
pub(crate) fn add_escalation_note(output: &mut Value, note: &str) {
    let error = output
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("terminal command failed");
    let error = format!("{error}\n{note}");
    if let Some(object) = output.as_object_mut() {
        object.insert("error".to_string(), Value::String(error));
        object.insert("sandbox_blocked".to_string(), Value::Bool(true));
    }
}

impl ToolExecutor {
    /// Decides what follows a foreground terminal command that failed. `None`
    /// keeps the result as it is: not a sandbox block, or `plan` mode.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn sandbox_escalation(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
        scope: (&Path, &ExecutionConfig, bool),
        session_id: Option<&str>,
        output: &Value,
    ) -> Option<Escalation> {
        let (effective_root, config, requires_worktree) = scope;
        if call.tool_name != "run_terminal"
            || self.approval_mode == ApprovalMode::Plan
            || !blocked_in_sandbox(output)
        {
            return None;
        }
        if self.execution_config.provider == "bwrap" || config.boundaries.network == "disabled" {
            return Some(Escalation::Note(PINNED_SANDBOX_NOTE.to_string()));
        }
        if !self.approval_handler.can_prompt() {
            return Some(Escalation::Note(HEADLESS_ESCALATION_NOTE.to_string()));
        }
        let escalated = escalated_config(config);
        let decision = self
            .prompt_without_remembering(
                call,
                level,
                risk,
                (effective_root, &escalated, requires_worktree),
                session_id,
                ESCALATION_REASON,
            )
            .await;
        if decision.granted {
            return Some(Escalation::Retry(decision, Box::new(escalated)));
        }
        let note = match decision.note.as_deref() {
            Some(reason) if decision.source == "denied" && reason != "User denied" => {
                format!("The user did not retry it outside the sandbox: {reason}")
            }
            _ => "The user did not retry it outside the sandbox.".to_string(),
        };
        Some(Escalation::Note(note))
    }
}
