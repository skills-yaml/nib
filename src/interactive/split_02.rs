//! T043 split.

use super::*;

pub fn format_interaction_chrome(
    project_root: &Path,
    runtime_profile_id: &str,
    session: Option<&Session>,
    session_id: &str,
    lifecycle: &str,
    queued: usize,
) -> Result<(String, String), String> {
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
    let provider = bounded_sensitive_status_value(&diagnostics.provider, &sensitive_values);
    let model = bounded_sensitive_status_value(&diagnostics.model, &sensitive_values);
    let transport = diagnostics.transport;
    let reasoning =
        bounded_sensitive_status_value(&diagnostics.reasoning_effort, &sensitive_values);
    let name = session
        .and_then(|session| session.display_name.as_deref())
        .unwrap_or("");
    let name = if name.is_empty() {
        String::new()
    } else {
        format!(
            " \"{}\"",
            bounded_sensitive_status_value(name, &sensitive_values)
        )
    };
    let origin = if session
        .and_then(|session| session.forked_from.as_deref())
        .is_some()
    {
        "forked"
    } else if session
        .and_then(|session| session.messages.first())
        .is_some()
    {
        "resumed"
    } else {
        "local"
    };
    let worktree = crate::integrations::worktree::with_validated_session_worktree(
        project_root,
        session_id,
        |path| Ok(path.display().to_string()),
    )?
    .unwrap_or_else(|| "-".to_string());
    let worktree = bounded_sensitive_status_value(&worktree, &sensitive_values);
    let project = bounded_sensitive_status_value(
        project_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project"),
        &sensitive_values,
    );
    let session_id = bounded_sensitive_status_value(session_id, &sensitive_values);
    let runtime_profile_id = bounded_sensitive_status_value(runtime_profile_id, &sensitive_values);
    let header = format!(
        "{project}  ·  profile {runtime_profile_id}  ·  sess {session_id}{name}  ·  {origin}  ·  worktree {worktree}"
    );
    let plan = session
        .and_then(|session| session.plan.as_ref())
        .map(|plan| {
            let current = plan.current_step_index.min(plan.steps.len());
            let title = plan
                .steps
                .get(plan.current_step_index)
                .map(|step| step.description.as_str())
                .unwrap_or("complete");
            format!(
                "  ·  plan {current}/{} {}",
                plan.steps.len(),
                bounded_sensitive_status_value(title, &sensitive_values)
            )
        })
        .unwrap_or_default();
    let context_used = persisted_context_usage(session, config.llm.context_length);
    let status = format!(
        "{}  ·  {provider}/{model} transport {transport} reasoning {reasoning}  ·  effective {}/{}:{} net {} sandbox {}  ·  context ~{context_used}/{}  ·  queue {queued}{plan}",
        bounded_sensitive_status_value(lifecycle, &sensitive_values),
        posture.effective_approval_mode,
        bounded_sensitive_status_value(&posture.provider, &sensitive_values),
        bounded_sensitive_status_value(&posture.profile, &sensitive_values),
        bounded_sensitive_status_value(&posture.network, &sensitive_values),
        sandbox_posture_label(&posture),
        config.llm.context_length,
    );
    Ok((header, status))
}

pub fn format_session_status(
    project_root: &Path,
    runtime_profile_id: &str,
    store: &SessionStore,
    session_id: &str,
    lifecycle: &str,
) -> Result<String, String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?;
    let queued = session
        .as_ref()
        .map(|session| session.queued_follow_ups.len())
        .unwrap_or(0);
    let (header, status) = format_interaction_chrome(
        project_root,
        runtime_profile_id,
        session.as_ref(),
        session_id,
        lifecycle,
        queued,
    )?;
    let config = load_nib_config_full(project_root)
        .map_err(|error| bounded_status_value(&error.to_string()))?;
    let posture = ToolExecutor::effective_execution_posture(
        project_root,
        config.execution,
        &config.approvals,
    );
    Ok(format!(
        "{header}\n{status}\n{}\n{}",
        format_effective_execution_posture(&posture),
        format_verification_status(session.as_ref(), store.public_sensitive_values()),
    ))
}

pub(crate) fn format_context_monitor(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    details: bool,
) -> Result<String, String> {
    let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?;
    let snapshot = session
        .as_ref()
        .and_then(crate::context::snapshot::latest_from_session);
    let Some(session) = session.as_ref() else {
        return Ok(crate::context::snapshot::occupancy_indicator(
            0,
            config.llm.context_length,
            crate::context::snapshot::OccupancyStyle::Compact,
        ));
    };
    let mut output = crate::context::snapshot::inspect_text(session, snapshot.as_ref(), details);
    let latest_resources = session
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "agent_resource_usage")
        .map(|event| {
            let number = |key: &str| {
                event
                    .details
                    .get(key)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
            };
            format!(
                "Latest run: generations {} · tools {} · input tokens total ~{} · max ~{} · compressions {} · repeated questions {}",
                number("generation_requests"),
                number("tool_attempts"),
                number("approximate_context_tokens_total"),
                number("approximate_context_tokens_max"),
                number("compression_requests"),
                number("repeated_questions"),
            )
        })
        .unwrap_or_else(|| "Latest run: no resource evidence yet".to_string());
    if details {
        output = format!("{output}\n{latest_resources}");
    }
    Ok(bounded_public_text(
        &output,
        &config.public_session_sensitive_values(),
        MAX_PUBLIC_PRESENTATION_BYTES,
        true,
    ))
}

pub(crate) fn session_background_tasks(
    session_store: &SessionStore,
    session_id: &str,
) -> Result<Vec<crate::daemons::workload::SessionOwnedDurableTask>, String> {
    let store =
        crate::daemons::workload::DurableTaskStore::from_sessions_dir(session_store.sessions_dir())
            .map_err(|error| bounded_status_value(&error))?;
    let mut tasks = store
        .list_for_session(session_id)
        .map_err(|error| bounded_status_value(&error))?;
    tasks.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(tasks)
}

pub(crate) fn format_session_background_tasks(
    session_store: &SessionStore,
    session_id: &str,
    stoppable_only: bool,
) -> Result<String, String> {
    let tasks = session_background_tasks(session_store, session_id)?;
    let total = tasks
        .iter()
        .filter(|task| {
            !stoppable_only || !matches!(task.status.as_str(), "completed" | "failed" | "cancelled")
        })
        .count();
    let mut shown = 0usize;
    let mut output = if stoppable_only {
        "Session-owned running background work:".to_string()
    } else {
        "Session-owned background work:".to_string()
    };
    for task in tasks.iter().filter(|task| {
        !stoppable_only || !matches!(task.status.as_str(), "completed" | "failed" | "cancelled")
    }) {
        if shown >= MAX_INTERACTIVE_BACKGROUND_TASKS {
            break;
        }
        output.push_str(&format!(
            "\n  - {} | {} | {} | updated {}",
            bounded_status_value(&task.id),
            bounded_status_value(&task.kind),
            bounded_status_value(&task.status),
            task.updated_at.to_rfc3339(),
        ));
        shown += 1;
    }
    if shown == 0 {
        output.push_str("\n  (none)");
    }
    if total > shown {
        output.push_str(&format!(
            "\n  ... {} additional tasks omitted",
            total - shown
        ));
    }
    if stoppable_only {
        output.push_str("\nUse /stop <task-id> to request cancellation of exactly one task.");
    }
    Ok(output)
}

pub(crate) fn stop_session_background_task(
    session_store: &SessionStore,
    session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    let store =
        crate::daemons::workload::DurableTaskStore::from_sessions_dir(session_store.sessions_dir())
            .map_err(|error| bounded_status_value(&error))?;
    let task = store
        .cancel_for_session(task_id, session_id)
        .map_err(|error| bounded_status_value(&error))?;
    Ok(format!(
        "Background task {} cancellation reconciled with status {}.",
        bounded_status_value(&task.id),
        bounded_status_value(&task.status),
    ))
}

pub fn path_completions(project_root: &Path, input: &str) -> Vec<InteractiveCompletion> {
    const MAX_PATHS: usize = 32;
    let Some(at) = input.rfind('@') else {
        return Vec::new();
    };
    if input[..at].contains(['\n', '\r']) {
        return Vec::new();
    }
    let prefix = &input[at + 1..];
    if prefix.contains([' ', '\n', '\r']) {
        return Vec::new();
    }
    let mut matches = Vec::new();
    collect_project_paths(project_root, project_root, prefix, 0, &mut matches);
    matches.sort();
    matches.truncate(MAX_PATHS);
    matches
        .into_iter()
        .map(|path| InteractiveCompletion {
            insertion: format!("{}@{path}", &input[..at]),
            usage: "@path",
            summary: "Attach a project path",
        })
        .collect()
}

pub(crate) fn collect_project_paths(
    root: &Path,
    current: &Path,
    prefix: &str,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth > 3 || out.len() >= 64 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.starts_with('.')
            || relative.starts_with("target/")
            || relative == "target"
            || relative.starts_with("node_modules/")
        {
            continue;
        }
        if relative.starts_with(prefix) {
            out.push(relative.clone());
        }
        if path.is_dir() {
            collect_project_paths(root, &path, prefix, depth + 1, out);
        }
    }
}

pub(crate) const MAX_PATH_ATTACHMENTS: usize = 8;

/// Resolve `@path` mentions into structured project attachments.
///
/// The returned text keeps the original mentions and does not expand file
/// contents into the prompt. Unsafe or escaped paths fail closed.
pub fn resolve_path_attachments(
    project_root: &Path,
    input: &str,
) -> Result<(String, Vec<PathAttachment>), String> {
    let Ok(root) = project_root.canonicalize() else {
        return Ok((input.to_string(), Vec::new()));
    };
    let mut attachments = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for token in path_mention_tokens(input) {
        if token.contains('\0') || token.starts_with('/') || token.contains("..") {
            return Err(format!(
                "path attachment '{token}' is outside the readable project root"
            ));
        }
        if token
            .split('/')
            .any(|segment| segment.starts_with('.') || segment.is_empty())
        {
            return Err(format!(
                "path attachment '{token}' is outside the readable project root"
            ));
        }
        let candidate = root.join(token);
        let metadata = match candidate.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                return Err(format!(
                    "path attachment '{token}' was not found in the project"
                ))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "path attachment '{token}' is not a readable project file"
            ));
        }
        let canonical = candidate.canonicalize().map_err(|_| {
            format!("path attachment '{token}' is outside the readable project root")
        })?;
        if !canonical.starts_with(&root) {
            return Err(format!(
                "path attachment '{token}' is outside the readable project root"
            ));
        }
        if !seen.insert(token.to_string()) {
            continue;
        }
        if attachments.len() >= MAX_PATH_ATTACHMENTS {
            return Err(format!(
                "at most {MAX_PATH_ATTACHMENTS} project path attachments are allowed"
            ));
        }
        attachments.push(PathAttachment {
            path: token.to_string(),
        });
    }
    Ok((input.to_string(), attachments))
}

pub(crate) fn path_mention_tokens(input: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'@' && (index == 0 || bytes[index - 1].is_ascii_whitespace()) {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len()
                && matches!(
                    bytes[end],
                    b'A'..=b'Z'
                        | b'a'..=b'z'
                        | b'0'..=b'9'
                        | b'_'
                        | b'.'
                        | b'/'
                        | b'-'
                )
            {
                end += 1;
            }
            if end > start {
                if let Ok(token) = std::str::from_utf8(&bytes[start..end]) {
                    tokens.push(token);
                }
            }
            index = end;
            continue;
        }
        index += 1;
    }
    tokens
}

pub(crate) fn bounded_workspace_diff(
    project_root: &Path,
    sensitive_values: &[String],
) -> Result<String, String> {
    let output =
        crate::sandbox::worktree::run_git_bounded_sync(project_root, ["diff", "--no-color"])
            .map_err(|error| {
                let diagnostic = bounded_public_text(
                    &error.to_string(),
                    sensitive_values,
                    MAX_ACTIVITY_LABEL_BYTES,
                    false,
                );
                format!("failed to read bounded git diff: {diagnostic}")
            })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let diagnostic = bounded_public_text(
            stderr
                .trim()
                .lines()
                .next()
                .unwrap_or("repository unavailable"),
            sensitive_values,
            MAX_ACTIVITY_LABEL_BYTES,
            false,
        );
        return Err(format!("git diff failed: {diagnostic}"));
    }
    let raw_diff = String::from_utf8_lossy(&output.stdout);
    if raw_diff.len() > MAX_DIFF_REDACTION_INPUT_BYTES && !sensitive_values.is_empty() {
        return Ok("[REDACTED]".to_string());
    }
    let mut redaction_end = raw_diff.len().min(MAX_DIFF_REDACTION_INPUT_BYTES);
    while redaction_end > 0 && !raw_diff.is_char_boundary(redaction_end) {
        redaction_end -= 1;
    }
    let mut diff = bounded_public_text(
        &raw_diff[..redaction_end],
        sensitive_values,
        MAX_DIFF_REDACTION_INPUT_BYTES,
        true,
    );
    if diff.trim().is_empty() {
        return Ok("No workspace diff.".to_string());
    }
    if diff.len() > MAX_DIFF_BYTES {
        let mut end = MAX_DIFF_BYTES;
        while end > 0 && !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
        diff.push_str("\n[diff truncated]");
    }
    Ok(diff)
}

pub(crate) fn bounded_session_workspace_diff(
    project_root: &Path,
    session_id: &str,
    sensitive_values: &[String],
) -> Result<String, String> {
    if let Some(diff) = crate::integrations::worktree::with_validated_session_worktree(
        project_root,
        session_id,
        |worktree| bounded_workspace_diff(worktree, sensitive_values),
    )? {
        return Ok(diff);
    }
    bounded_workspace_diff(project_root, sensitive_values)
}

pub(crate) fn format_current_plan(
    session: Option<&Session>,
    sensitive_values: &[String],
) -> String {
    match session.and_then(|session| session.plan.as_ref()) {
        Some(plan) => plan_activity(plan, sensitive_values).render_line(),
        None => "No plan in the active session.".to_string(),
    }
}

pub(crate) fn latest_assistant_output(session: Option<&Session>) -> Result<String, String> {
    session
        .and_then(|session| {
            session
                .messages
                .iter()
                .rev()
                .find(|message| message.role.eq_ignore_ascii_case("assistant"))
        })
        .map(|message| message.content.clone())
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "no completed assistant output to copy".to_string())
}

pub(crate) fn fork_session(
    store: &SessionStore,
    session_id: &str,
) -> Result<(String, String), String> {
    let source = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .ok_or_else(|| format!("session {session_id} no longer exists"))?;
    let created = store
        .try_create_session()
        .map_err(|error| format!("failed to fork session: {error}"))?;
    store
        .update_session(&created.id, |session| {
            session.messages = source.messages.clone();
            session.tool_calls = source.tool_calls.clone();
            session.plan = source.plan.clone();
            session.summary = source.summary.clone();
            session.summary_index = source.summary_index;
            session.events = source.events.clone();
            session.active_skills = source.active_skills.clone();
            session.skill_usage = source.skill_usage.clone();
            session.display_name = source.display_name.clone();
            session.forked_from = Some(source.id.clone());
            session.queued_follow_ups.clear();
            Ok(())
        })
        .map_err(|error| format!("failed to copy forked session: {error}"))?;
    Ok((
        created.id.clone(),
        format!("Forked session {} from {session_id}.", created.id),
    ))
}

pub fn session_title_from_conversation(
    conversation: &str,
    sensitive_values: &[String],
) -> Option<String> {
    let line = conversation.lines().find_map(|line| {
        let trimmed = line.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    })?;
    if line.starts_with("Continue with approved plan") {
        return None;
    }
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = bounded_public_text(&collapsed, sensitive_values, MAX_DISPLAY_NAME_BYTES, false);
    let title = title.trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

pub fn maybe_assign_session_display_name(
    store: &SessionStore,
    session_id: &str,
    conversation: &str,
) -> Result<Option<String>, String> {
    let Some(name) = session_title_from_conversation(conversation, store.public_sensitive_values())
    else {
        return Ok(None);
    };
    let mut assigned = None;
    store
        .update_session(session_id, |session| {
            if session
                .display_name
                .as_ref()
                .is_some_and(|existing| !existing.trim().is_empty())
            {
                return Ok(());
            }
            session.display_name = Some(name.clone());
            assigned = Some(name.clone());
            Ok(())
        })
        .map_err(|error| format!("failed to name session: {error}"))?;
    Ok(assigned)
}

pub(crate) fn rename_session(
    store: &SessionStore,
    session_id: &str,
    name: &str,
) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("usage: /rename <name>".to_string());
    }
    if name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(format!(
            "session name must be at most {MAX_DISPLAY_NAME_BYTES} bytes"
        ));
    }
    let name = bounded_public_text(
        name,
        store.public_sensitive_values(),
        MAX_DISPLAY_NAME_BYTES,
        false,
    );
    store
        .update_session(session_id, |session| {
            session.display_name = Some(name.clone());
            Ok(())
        })
        .map_err(|error| format!("failed to rename session: {error}"))?;
    Ok(format!("Renamed session {session_id} to '{name}'."))
}

pub(crate) fn set_approval_mode(project_root: &Path, mode: &str) -> Result<String, String> {
    let mode = mode.trim().to_ascii_lowercase();
    if !matches!(mode.as_str(), "manual" | "smart" | "policy" | "off") {
        return Err("usage: /permissions [manual|smart|policy|off]".to_string());
    }
    update_nib_config(project_root, {
        let selected = mode.clone();
        move |config| {
            config.approvals.mode = selected;
            Ok(())
        }
    })
    .map_err(|error| format!("failed saving permissions: {error}"))?;
    Ok(format!(
        "Configured approval preset set to '{}'. Stronger controls remain in force.\n{}",
        bounded_status_value(&mode),
        format_permissions(project_root)?
    ))
}

pub(crate) fn format_permissions(project_root: &Path) -> Result<String, String> {
    let config = load_nib_config_full(project_root)
        .map_err(|error| bounded_status_value(&error.to_string()))?;
    let posture = ToolExecutor::effective_execution_posture(
        project_root,
        config.execution,
        &config.approvals,
    );
    Ok(format_effective_execution_posture(&posture))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionResolution {
    Created(String),
    Resumed(String),
    RequestedMissing { requested: String, created: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDisplay {
    Content(String),
    Status(String),
}

#[cfg(test)]
pub(crate) fn display_stream_event(event: StreamEvent) -> Option<StreamDisplay> {
    display_stream_event_with_sensitive_values(event, &[])
}

pub fn display_stream_event_with_sensitive_values(
    event: StreamEvent,
    sensitive_values: &[String],
) -> Option<StreamDisplay> {
    let display = display_stream_event_unchecked(event)?;
    Some(match display {
        StreamDisplay::Content(content) => StreamDisplay::Content(bounded_public_text(
            &content,
            sensitive_values,
            MAX_PUBLIC_PRESENTATION_BYTES,
            true,
        )),
        StreamDisplay::Status(status) => StreamDisplay::Status(bounded_public_text(
            &status,
            sensitive_values,
            MAX_PUBLIC_PRESENTATION_BYTES,
            true,
        )),
    })
}

pub(crate) fn display_stream_event_unchecked(event: StreamEvent) -> Option<StreamDisplay> {
    let display = match event {
        StreamEvent::Content(content) => StreamDisplay::Content(content),
        StreamEvent::ToolCallChunk {
            name: Some(name), ..
        } if !name.is_empty() => StreamDisplay::Status(format!("[tool call] {name}")),
        StreamEvent::ToolCallChunk { .. } => return None,
        StreamEvent::StateTransition { state } => {
            StreamDisplay::Status(format!("[state] {state}"))
        }
        StreamEvent::PlanGenerated { step_count, .. } => {
            let noun = if step_count == 1 { "step" } else { "steps" };
            StreamDisplay::Status(format!("[plan] generated {step_count} {noun}"))
        }
        StreamEvent::PlanProgress(progress) => display_plan_progress(&progress)?,
        StreamEvent::ApprovalRequired { tool_name } => {
            let tool_name = bounded_status_value(&crate::tools::executor::redact_text(&tool_name));
            StreamDisplay::Status(format!("[approval required] {tool_name}"))
        }
        StreamEvent::QuestionRequired { question, options } => {
            let options = if options.is_empty() {
                String::new()
            } else {
                format!(" (options: {})", options.join(" | "))
            };
            StreamDisplay::Status(format!("[question] {question}{options}"))
        }
        StreamEvent::ToolStarted { tool_name, .. } => {
            let tool_name = bounded_status_value(&crate::tools::executor::redact_text(&tool_name));
            StreamDisplay::Status(format!("[tool started] {tool_name}"))
        }
        StreamEvent::TerminalOutput {
            tool_name,
            stream,
            chunk,
            background_task_id,
            ..
        } => {
            let task = background_task_id
                .as_deref()
                .map(|id| format!(" task={id}"))
                .unwrap_or_default();
            StreamDisplay::Status(format!(
                "[terminal {stream}] {tool_name}{task}: {}",
                chunk.trim_end_matches(['\r', '\n'])
            ))
        }
        StreamEvent::ToolCompleted {
            tool_name,
            success,
            output,
            error,
            ..
        } => {
            let status = if success { "ok" } else { "failed" };
            let detail = match (output.as_ref(), error.as_deref()) {
                (Some(output), Some(error)) => {
                    format!("{}; error: {error}", inline_json(output))
                }
                (Some(output), None) => inline_json(output),
                (None, Some(error)) => error.to_string(),
                (None, None) => "no result".to_string(),
            };
            StreamDisplay::Status(format!(
                "[tool completed] {tool_name}: {status} - {detail}"
            ))
        }
        StreamEvent::Compression {
            before_tokens,
            after_tokens,
            summarized_through,
        } => StreamDisplay::Status(format!(
            "[compression] {before_tokens} -> {after_tokens} tokens; summarized through message {summarized_through}"
        )),
        StreamEvent::Reconciled { outcome } => display_reconciliation_status(&outcome),
        StreamEvent::Failure {
            failure,
            session_id,
        } => StreamDisplay::Status(failure.user_report(session_id.as_deref())),
        StreamEvent::End(reason) => StreamDisplay::Status(stream_end_status_line(&reason)),
    };
    Some(display)
}

pub(crate) fn display_plan_progress(progress: &crate::llm::PlanProgress) -> Option<StreamDisplay> {
    if progress.steps.len() < 2 {
        return None;
    }
    let completed = progress
        .steps
        .iter()
        .filter(|step| step.status == "Completed")
        .count();
    let total = progress.steps.len();
    let state = if progress.complete {
        "complete".to_string()
    } else if let Some(step) = progress.steps.get(progress.current_step_index) {
        let label = bounded_status_value(&step.description);
        if step.status == "Blocked" {
            format!("blocked: {label}")
        } else if step.status == "Cancelled" {
            format!("stopped: {label}")
        } else if step.status == "InProgress" {
            format!("working on: {label}")
        } else {
            "ready".to_string()
        }
    } else {
        "ready".to_string()
    };
    Some(StreamDisplay::Status(format!(
        "[plan] {completed}/{total} done · {state}"
    )))
}

pub(crate) fn display_reconciliation_status(outcome: &str) -> StreamDisplay {
    if outcome == "verification_recovery" {
        return StreamDisplay::Status(
            "[verification] completion rejected; continuing the current step".to_string(),
        );
    }
    if outcome == "instruction_context_missing" {
        return StreamDisplay::Status(
            "[failed] Project instructions could not be loaded. Restore readable project instructions within the size limit, or increase llm.context_length, then retry the same plan.".to_string(),
        );
    }
    if outcome == "tool_scope_outside_worktree" {
        return StreamDisplay::Status(
            "[failed] A proposed tool path is outside this project. Choose a project path and retry."
                .to_string(),
        );
    }
    let reduction = reduce_interaction(
        &InteractionState::default(),
        InteractionInput::ReconciledOutcome {
            outcome,
            failure: false,
        },
    );
    let (outcome, terminal) = match reduction {
        InteractionReduction::Reconciled { outcome, terminal } => (outcome, terminal),
        _ => unreachable!("reconciliation input always has a terminal reduction"),
    };
    let message = terminal_outcome_message(&outcome);
    let label = if terminal == InteractionTerminalOutcome::Failed {
        "failed"
    } else {
        "reconciled"
    };
    StreamDisplay::Status(format!("[{label}] {}. {}", message.title, message.detail))
}

pub(crate) fn inline_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

impl SessionResolution {
    pub fn session_id(&self) -> &str {
        match self {
            Self::Created(id) | Self::Resumed(id) => id,
            Self::RequestedMissing { created, .. } => created,
        }
    }

    pub fn notice(&self) -> String {
        match self {
            Self::Created(id) => format!("Started session {id}."),
            Self::Resumed(id) => format!("Resumed session {id}."),
            Self::RequestedMissing { requested, created } => {
                format!("Session {requested} was not found; started session {created}.")
            }
        }
    }

    pub fn tui_origin(&self) -> &'static str {
        match self {
            Self::Created(_) | Self::RequestedMissing { .. } => "new",
            Self::Resumed(_) => "resumed",
        }
    }

    pub fn tui_notice(&self) -> Option<String> {
        match self {
            Self::RequestedMissing { requested, created } => Some(format!(
                "Requested session {} was not found; started {}.",
                abbreviated_session_id(requested),
                abbreviated_session_id(created),
            )),
            Self::Created(_) | Self::Resumed(_) => None,
        }
    }
}

pub fn resolve_session(
    store: &SessionStore,
    requested: Option<&str>,
) -> Result<SessionResolution, String> {
    if let Some(requested) = requested {
        if store
            .load_result(requested)
            .map_err(|error| format!("failed to load requested session {requested}: {error}"))?
            .is_some()
        {
            return Ok(SessionResolution::Resumed(requested.to_string()));
        }
        let created = store
            .try_create_session()
            .map_err(|error| format!("failed to create session: {error}"))?
            .id;
        return Ok(SessionResolution::RequestedMissing {
            requested: requested.to_string(),
            created,
        });
    }

    Ok(SessionResolution::Created(
        store
            .try_create_session()
            .map_err(|error| format!("failed to create session: {error}"))?
            .id,
    ))
}

pub fn parse_interactive_command(input: &str) -> Result<Option<InteractiveCommand>, String> {
    let Some(command_line) = input.trim().strip_prefix('/') else {
        return Ok(None);
    };
    let mut parts = command_line.split_whitespace();
    let command = parts.next().unwrap_or_default().to_ascii_lowercase();
    let arguments: Vec<String> = parts.map(ToString::to_string).collect();

    if command.is_empty() {
        return Err("empty slash command; use /help".to_string());
    }
    let Some(spec) = find_command_spec(&command) else {
        return Err(format!("unknown command: /{command}; use /help"));
    };
    if let InteractiveAvailability::Unavailable(reason) = spec.availability {
        return Err(format!("command /{} is unavailable: {reason}", spec.name));
    }
    validate_interactive_arguments(spec, &arguments)?;
    let command = spec.name;

    let parsed = match command {
        "quit" if arguments.is_empty() => InteractiveCommand::Quit,
        "help" if arguments.is_empty() => InteractiveCommand::Help,
        "status" if arguments.is_empty() => InteractiveCommand::Status,
        "context" if arguments.is_empty() => InteractiveCommand::Context { details: false },
        "context" if arguments.len() == 1 && arguments[0].eq_ignore_ascii_case("details") => {
            InteractiveCommand::Context { details: true }
        }
        "providers" if arguments.is_empty() => InteractiveCommand::Providers,
        "permissions" => InteractiveCommand::Permissions {
            selection: if arguments.is_empty() {
                None
            } else {
                Some(arguments.join(" "))
            },
        },
        "plan" => InteractiveCommand::Plan {
            prompt: if arguments.is_empty() {
                None
            } else {
                Some(arguments.join(" "))
            },
        },
        "review" if arguments.is_empty() => InteractiveCommand::Review,
        "diff" if arguments.is_empty() => InteractiveCommand::Diff,
        "compact" if arguments.is_empty() => InteractiveCommand::Compact,
        "session" if arguments.is_empty() => InteractiveCommand::Session,
        "resume" if arguments.is_empty() => InteractiveCommand::Resume,
        "new" if arguments.is_empty() => InteractiveCommand::New,
        "clear" if arguments.is_empty() => InteractiveCommand::Clear,
        "fork" if arguments.is_empty() => InteractiveCommand::Fork,
        "rename" => InteractiveCommand::Rename {
            name: arguments.join(" "),
        },
        "copy" if arguments.is_empty() => InteractiveCommand::Copy,
        "history" => InteractiveCommand::History {
            query: if arguments.is_empty() {
                None
            } else {
                Some(arguments.join(" "))
            },
        },
        "ps" if arguments.is_empty() => InteractiveCommand::Ps,
        "stop" if arguments.len() <= 1 => InteractiveCommand::Stop {
            task_id: arguments.first().cloned(),
        },
        "model" => InteractiveCommand::Model {
            selection: if arguments.is_empty() {
                None
            } else {
                Some(arguments.join(" "))
            },
        },
        "skills" => InteractiveCommand::Skills(parse_skill_command(&arguments)?),
        "mcp" => InteractiveCommand::Mcp(parse_mcp_command(&arguments)?),
        "questions" if arguments.len() <= 1 => InteractiveCommand::Questions {
            id: arguments.first().cloned(),
        },
        "continue" if arguments.len() == 1 => InteractiveCommand::Continue {
            plan_id: arguments[0].clone(),
        },
        _ => return Err(format!("usage: {}", spec.usage)),
    };
    Ok(Some(parsed))
}

pub(crate) fn validate_interactive_arguments(
    spec: &InteractiveCommandSpec,
    arguments: &[String],
) -> Result<(), String> {
    let valid = match spec.arguments {
        InteractiveArgumentSchema::None => arguments.is_empty(),
        InteractiveArgumentSchema::OptionalText => true,
        InteractiveArgumentSchema::RequiredText => !arguments.is_empty(),
        InteractiveArgumentSchema::OptionalSingle => arguments.len() <= 1,
        InteractiveArgumentSchema::Permissions => {
            arguments.len() <= 1
                && arguments.first().is_none_or(|argument| {
                    spec.completion
                        .candidates
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(argument))
                })
        }
        InteractiveArgumentSchema::Skills | InteractiveArgumentSchema::Mcp => true,
    };
    if valid {
        Ok(())
    } else {
        Err(format!("usage: {}", spec.usage))
    }
}

pub(crate) fn parse_skill_command(arguments: &[String]) -> Result<SkillCommand, String> {
    if arguments.is_empty() || (arguments.len() == 1 && arguments[0] == "list") {
        return Ok(SkillCommand::List);
    }
    match arguments {
        [command, source] if command == "install" => Ok(SkillCommand::Install {
            source: source.clone(),
        }),
        [command, name] if command == "remove" => Ok(SkillCommand::Remove { name: name.clone() }),
        _ => Err(
            "usage: /skills list | /skills install <url_or_path> | /skills remove <name>"
                .to_string(),
        ),
    }
}

pub(crate) fn parse_mcp_command(arguments: &[String]) -> Result<McpCommand, String> {
    if arguments.is_empty() || (arguments.len() == 1 && arguments[0] == "list") {
        return Ok(McpCommand::List);
    }
    match arguments {
        [command, name, executable, args @ ..] if command == "add" => Ok(McpCommand::Add {
            name: name.clone(),
            command: executable.clone(),
            args: args.to_vec(),
        }),
        [command, name] if command == "remove" => Ok(McpCommand::Remove { name: name.clone() }),
        _ => Err(
            "usage: /mcp list | /mcp add <name> <command> [args...] | /mcp remove <name>"
                .to_string(),
        ),
    }
}

pub fn execute_interactive_command(
    command: InteractiveCommand,
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
) -> Result<InteractiveEffect, String> {
    execute_interactive_command_in_state(
        command,
        project_root,
        "default",
        store,
        session_id,
        "idle",
    )
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub fn execute_interactive_command_in_state(
    command: InteractiveCommand,
    project_root: &Path,
    runtime_profile_id: &str,
    store: &SessionStore,
    session_id: &str,
    lifecycle: &str,
) -> Result<InteractiveEffect, String> {
    match command {
        InteractiveCommand::Quit => Ok(InteractiveEffect::Quit),
        InteractiveCommand::Help => Ok(InteractiveEffect::Output(interactive_help())),
        InteractiveCommand::Status => Ok(InteractiveEffect::Output(format_session_status(
            project_root,
            runtime_profile_id,
            store,
            session_id,
            lifecycle,
        )?)),
        InteractiveCommand::Context { details } => Ok(InteractiveEffect::Output(
            format_context_monitor(project_root, store, session_id, details)?,
        )),
        InteractiveCommand::Permissions { selection: None } => {
            Ok(InteractiveEffect::Output(format_permissions(project_root)?))
        }
        InteractiveCommand::Permissions {
            selection: Some(mode),
        } => Ok(InteractiveEffect::Output(set_approval_mode(
            project_root,
            &mode,
        )?)),
        InteractiveCommand::Plan { prompt: None } => {
            let session = store
                .load_result(session_id)
                .map_err(|error| format!("failed to load session {session_id}: {error}"))?;
            Ok(InteractiveEffect::Output(format_current_plan(
                session.as_ref(),
                store.public_sensitive_values(),
            )))
        }
        InteractiveCommand::Plan {
            prompt: Some(prompt),
        } => Ok(InteractiveEffect::RunAgent {
            goal: prompt,
            mode: InteractiveAgentMode::Plan,
        }),
        InteractiveCommand::Review | InteractiveCommand::Diff => {
            Ok(InteractiveEffect::Output(bounded_session_workspace_diff(
                project_root,
                session_id,
                store.public_sensitive_values(),
            )?))
        }
        InteractiveCommand::Compact => Ok(InteractiveEffect::Compact),
        InteractiveCommand::Ps => Ok(InteractiveEffect::Output(format_session_background_tasks(
            store, session_id, false,
        )?)),
        InteractiveCommand::Stop { task_id: None } => Ok(InteractiveEffect::Output(
            format_session_background_tasks(store, session_id, true)?,
        )),
        InteractiveCommand::Stop {
            task_id: Some(task_id),
        } => Ok(InteractiveEffect::Output(stop_session_background_task(
            store, session_id, &task_id,
        )?)),
        InteractiveCommand::Resume | InteractiveCommand::Session => Ok(
            InteractiveEffect::SelectSession(interactive_session_selection(store, session_id)?),
        ),
        InteractiveCommand::New | InteractiveCommand::Clear => {
            let new_session = store
                .try_create_session()
                .map_err(|error| format!("failed to create session: {error}"))?;
            Ok(InteractiveEffect::SessionChanged {
                output: format!("Started fresh session {}.", new_session.id),
                session_id: new_session.id,
            })
        }
        InteractiveCommand::Fork => {
            let (session_id, output) = fork_session(store, session_id)?;
            Ok(InteractiveEffect::SessionChanged { session_id, output })
        }
        InteractiveCommand::Rename { name } => Ok(InteractiveEffect::Output(rename_session(
            store, session_id, &name,
        )?)),
        InteractiveCommand::Copy => {
            let session = store
                .load_result(session_id)
                .map_err(|error| format!("failed to load session {session_id}: {error}"))?;
            Ok(InteractiveEffect::Output(latest_assistant_output(
                session.as_ref(),
            )?))
        }
        InteractiveCommand::History { .. } => {
            Err("draft history search requires an interactive composer".to_string())
        }
        InteractiveCommand::Providers => {
            let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
            let sensitive_values = config.public_session_sensitive_values();
            let active = config.llm.get_active_provider();
            let mut output = String::from("Configured providers:");
            if config.llm.providers.is_empty() {
                output.push_str("\n  (none - using mock)");
            }
            for (name, entry) in &config.llm.providers {
                let marker = if name == &active { " (active)" } else { "" };
                let name =
                    bounded_public_text(name, &sensitive_values, MAX_STATUS_VALUE_BYTES, false);
                let model = bounded_public_text(
                    &entry.model,
                    &sensitive_values,
                    MAX_STATUS_VALUE_BYTES,
                    false,
                );
                output.push_str(&format!("\n  - {name}: {model}{marker}"));
            }
            Ok(InteractiveEffect::Output(output))
        }
        InteractiveCommand::Model { selection: None } => {
            let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
            let sensitive_values = config.public_session_sensitive_values();
            let provider = config.llm.get_active_provider();
            let current = config
                .llm
                .get_provider(None)
                .map(|entry| entry.model.clone())
                .unwrap_or_default();
            Ok(InteractiveEffect::SelectModel(ModelSelection {
                available: config.llm.get_available_models(None),
                provider,
                current,
                sensitive_values,
            }))
        }
        InteractiveCommand::Model {
            selection: Some(model),
        } => Ok(InteractiveEffect::Output(set_active_model(
            project_root,
            &model,
        )?)),
        InteractiveCommand::Skills(SkillCommand::List) => Ok(InteractiveEffect::Output(
            skill_cmd::format_installed_skills(project_root)?,
        )),
        InteractiveCommand::Skills(SkillCommand::Install { source }) => {
            let path = skill_cmd::install_skill(&source)?;
            Ok(InteractiveEffect::Output(format!(
                "Installed skill at {}",
                path.display()
            )))
        }
        InteractiveCommand::Skills(SkillCommand::Remove { name }) => {
            skill_cmd::remove_skill(&name)?;
            Ok(InteractiveEffect::Output(format!(
                "Removed skill '{name}'."
            )))
        }
        InteractiveCommand::Mcp(McpCommand::List) => Ok(InteractiveEffect::Output(
            mcp_cmd::format_mcp_servers(project_root)?,
        )),
        InteractiveCommand::Mcp(McpCommand::Add {
            name,
            command,
            args,
        }) => {
            mcp_cmd::add_mcp_server_quiet(project_root, &name, &command, &args)?;
            Ok(InteractiveEffect::Output(format!(
                "Successfully added MCP server '{name}'."
            )))
        }
        InteractiveCommand::Mcp(McpCommand::Remove { name }) => {
            mcp_cmd::remove_mcp_server_quiet(project_root, &name)?;
            Ok(InteractiveEffect::Output(format!(
                "Successfully removed MCP server '{name}'."
            )))
        }
        InteractiveCommand::Questions { id } => {
            load_questions_effect(store, session_id, id.as_deref())
        }
        InteractiveCommand::Continue { plan_id } => {
            load_continue_plan_effect(store, session_id, &plan_id)
        }
    }
}

pub(crate) fn load_questions_effect(
    store: &SessionStore,
    session_id: &str,
    invocation_id: Option<&str>,
) -> Result<InteractiveEffect, String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .ok_or_else(|| format!("session {session_id} was not found"))?;
    let active_plan = session.plan.as_ref().map(|plan| plan.id.as_str());
    let unresolved: Vec<&crate::session::ClarificationRecord> = session
        .clarifications
        .iter()
        .filter(|record| {
            matches!(
                record.status,
                crate::session::ClarificationStatus::Pending
                    | crate::session::ClarificationStatus::Unresolved
            ) && record.plan_id.as_deref() == active_plan
        })
        .collect();
    let Some(invocation_id) = invocation_id else {
        return Ok(InteractiveEffect::Output(format_unresolved_questions(
            &unresolved,
        )));
    };
    let record = unresolved
        .iter()
        .find(|record| record.invocation_id.to_string() == invocation_id)
        .ok_or_else(|| format!("no unresolved question {invocation_id} for the current plan"))?;
    Ok(InteractiveEffect::OpenQuestion {
        invocation_id: record.invocation_id.to_string(),
        question: record.question.clone(),
        proposed_answer: record.proposed_answer.clone(),
        options: record.options.clone(),
    })
}

pub(crate) fn format_unresolved_questions(
    records: &[&crate::session::ClarificationRecord],
) -> String {
    if records.is_empty() {
        return "No unresolved questions for the current plan. Use /questions <id> with an exact invocation id.".to_string();
    }
    let mut output = String::from("Unresolved questions:");
    for record in records {
        output.push_str(&format!(
            "\n  {}  [{}]  {}",
            record.invocation_id,
            format!("{:?}", record.status).to_ascii_lowercase(),
            bounded_status_value(&record.question)
        ));
    }
    output.push_str("\nAnswer with /questions <id>, then /continue <plan-id> to resume.");
    output
}

pub(crate) fn load_continue_plan_effect(
    store: &SessionStore,
    session_id: &str,
    plan_id: &str,
) -> Result<InteractiveEffect, String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .ok_or_else(|| format!("session {session_id} was not found"))?;
    let plan = session
        .plan
        .as_ref()
        .ok_or_else(|| "no active plan is available to continue".to_string())?;
    if plan.id != plan_id {
        return Err(format!(
            "plan {plan_id} is not the current session plan {}",
            plan.id
        ));
    }
    if plan.goal.trim().is_empty() || plan.id.trim().is_empty() {
        return Err("persisted plan is missing goal or identity and cannot continue".to_string());
    }
    if plan.is_complete() {
        return Err(format!("plan {plan_id} is already complete"));
    }
    if !plan.approved {
        return Err(format!("plan {plan_id} is not approved for execution"));
    }
    if !plan.is_structured() {
        return Err(format!("plan {plan_id} is malformed and cannot continue"));
    }
    if plan.outcome.as_deref() == Some("provider_continuation_interrupted") {
        return Err(format!(
            "plan {plan_id} has an uncertain interrupted provider continuation and cannot be replayed"
        ));
    }
    if session.has_unresolved_clarification(Some(&plan.id)) {
        return Err(
            "unresolved questions still block this plan; answer them with /questions first"
                .to_string(),
        );
    }
    Ok(InteractiveEffect::ContinuePlan {
        plan_id: plan.id.clone(),
        goal: plan.goal.clone(),
    })
}

pub fn persist_recovered_question_answer(
    store: &SessionStore,
    session_id: &str,
    invocation_id: &str,
    answer: &str,
) -> Result<String, String> {
    persist_recovered_question_response(store, session_id, invocation_id, answer, false)
}

pub fn persist_recovered_proposed_answer(
    store: &SessionStore,
    session_id: &str,
    invocation_id: &str,
    answer: &str,
) -> Result<String, String> {
    persist_recovered_question_response(store, session_id, invocation_id, answer, true)
}

pub(crate) fn persist_recovered_question_response(
    store: &SessionStore,
    session_id: &str,
    invocation_id: &str,
    answer: &str,
    approved_proposal: bool,
) -> Result<String, String> {
    let answer = if approved_proposal {
        answer
    } else {
        answer.trim()
    };
    if answer.trim().is_empty() {
        return Err("recovered question answer cannot be empty".to_string());
    }
    // UI-local idleness is not sufficient because another process may own this
    // session. Hold the authoritative run lease across the identity re-read and
    // durable answer publication.
    let run_lease = store
        .try_acquire_run_lease(session_id)
        .map_err(|error| error.to_string())?;
    run_lease.verify().map_err(|error| error.to_string())?;
    let result = store
        .update_session(session_id, |session| {
            let active_plan = session
                .plan
                .as_ref()
                .map(|plan| plan.id.clone())
                .ok_or_else(|| {
                    crate::session::SessionError::InvalidMutation(
                        "recovered questions require an active plan".to_string(),
                    )
                })?;
            let index = session
                .clarifications
                .iter()
                .position(|record| record.invocation_id.to_string() == invocation_id)
                .ok_or_else(|| {
                    crate::session::SessionError::InvalidMutation(format!(
                        "question {invocation_id} was not found"
                    ))
                })?;
            let record = &session.clarifications[index];
            if record.plan_id.as_deref() != Some(active_plan.as_str()) {
                return Err(crate::session::SessionError::InvalidMutation(
                    "question does not belong to the current plan".to_string(),
                ));
            }
            if record.status == crate::session::ClarificationStatus::Answered {
                return Err(crate::session::SessionError::InvalidMutation(
                    "question was already answered".to_string(),
                ));
            }
            if approved_proposal && record.proposed_answer.as_deref() != Some(answer) {
                return Err(crate::session::SessionError::InvalidMutation(
                    "the proposed answer changed; reopen the question".to_string(),
                ));
            }
            let event_index = session.events.len();
            session.events.push(SessionEvent {
                index: event_index,
                kind: "human_question_answer_received".to_string(),
                details: serde_json::json!({
                    "invocation_id": invocation_id,
                    "answer": answer,
                    "recovered": true,
                    "decision": if approved_proposal { "approved_proposal" } else { "answered" },
                }),
                timestamp: Some(Utc::now()),
            });
            session
                .human_intent
                .push(crate::session::HumanIntentRecord {
                    kind: crate::session::HumanIntentKind::QuestionAnswer,
                    text: answer.to_string(),
                    source_message_index: None,
                    source_event_index: Some(event_index),
                });
            let record = &mut session.clarifications[index];
            record.status = crate::session::ClarificationStatus::Answered;
            record.answer = Some(answer.to_string());
            record.outcome = Some(
                if approved_proposal {
                    "approved_proposal"
                } else {
                    "answered"
                }
                .to_string(),
            );
            record.reason = Some("recovered via /questions".to_string());
            record.answer_event_index = Some(event_index);
            Ok(active_plan)
        })
        .map_err(|error| error.to_string());
    drop(run_lease);
    result
}

pub fn set_active_model(project_root: &Path, model: &str) -> Result<String, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("model ID must not be empty".to_string());
    }
    let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    let sensitive_values = config.public_session_sensitive_values();
    let provider = config.llm.get_active_provider();
    let selected_provider = provider.clone();
    let selected_model = model.to_string();
    update_nib_config(project_root, move |config| {
        if let Some(entry) = config.llm.providers.get_mut(&selected_provider) {
            entry.model = selected_model;
            Ok(())
        } else if selected_provider == "mock" {
            config
                .llm
                .add_or_update_provider(selected_provider, selected_model, None);
            Ok(())
        } else {
            Err(format!(
                "provider '{selected_provider}' is no longer configured"
            ))
        }
    })
    .map_err(|error| format!("failed saving model: {error}"))?;
    let model = bounded_public_text(model, &sensitive_values, MAX_STATUS_VALUE_BYTES, false);
    let provider = bounded_public_text(&provider, &sensitive_values, MAX_STATUS_VALUE_BYTES, false);
    Ok(format!(
        "Switched model to '{model}' for provider '{provider}'."
    ))
}
