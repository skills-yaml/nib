//! Plan verification helpers.

use super::*;

pub(crate) fn parse_verification_waiver(content: &str) -> Option<(&str, &str)> {
    let request = content.trim().strip_prefix("waive verification ")?;
    let (id, reason) = request.split_once(':')?;
    let id = id.trim();
    let reason = reason.trim();
    (!id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && !reason.is_empty())
    .then_some((id, reason))
}

pub(crate) fn apply_human_verification_waiver(
    store: &SessionStore,
    session_id: &str,
    content: &str,
) -> Result<Option<String>, String> {
    let Some((obligation_id, reason)) = parse_verification_waiver(content) else {
        return Ok(None);
    };
    let obligation_id = obligation_id.to_string();
    let reason = reason.to_string();
    store
        .update_session(session_id, |session| {
            let source_message_index = session.messages.len().checked_sub(1).ok_or_else(|| {
                crate::session::SessionError::InvalidMutation(
                    "verification waiver has no source message".to_string(),
                )
            })?;
            if session.message_origin(source_message_index) != MessageOrigin::HumanRequest {
                return Err(crate::session::SessionError::InvalidMutation(
                    "verification waiver source is not an authenticated human request".to_string(),
                ));
            }
            let plan = session.plan.as_mut().ok_or_else(|| {
                crate::session::SessionError::InvalidMutation(
                    "verification waiver has no active plan".to_string(),
                )
            })?;
            let plan_id = plan.id.clone();
            plan.waive_verification(
                &plan_id,
                &obligation_id,
                source_message_index,
                reason.clone(),
            )
            .map_err(crate::session::SessionError::InvalidMutation)?;
            append_session_event(
                session,
                "verification_waived",
                json!({
                    "plan_id": plan_id,
                    "verification_id": obligation_id,
                    "source_message_index": source_message_index,
                    "reason": reason,
                }),
            );
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    Ok(Some(obligation_id))
}

pub(crate) fn finish_human_verification_waiver(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    verification_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AgentRunSummary, String> {
    let message =
        format!("Verification requirement {verification_id} was waived for the active plan.");
    store
        .record_event(
            session_id,
            "reconciliation",
            json!({"run_id": run_id, "outcome": "verification_waived", "continue": false}),
        )
        .map_err(|error| error.to_string())?;
    emit_nonblocking(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: "verification_waived".to_string(),
        },
    );
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        steps_taken: 0,
        last_message: Some(message),
        tool_call_count: 0,
        final_state: AgentState::Done,
        outcome: "verification_waived".to_string(),
        failure: None,
        bound_reached: false,
        trace: vec![
            AgentState::Idle.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

pub(crate) fn command_verification_obligation(
    command: &str,
    authority: VerificationAuthority,
    source_message_index: Option<usize>,
) -> Result<VerificationObligation, String> {
    let fingerprint = format!("{:x}", Sha256::digest(command.as_bytes()));
    let mut obligation = VerificationObligation::pending_tool(
        format!("required-command-{}", &fingerprint[..16]),
        format!("Run the required command: {command}"),
        vec![".".to_string()],
        "run_terminal",
        json!({"command": command, "affected_paths": ["."]}),
        VerificationExpectedOutcome::Success,
    )?;
    obligation.authority = authority;
    obligation.source_message_index = source_message_index;
    Ok(obligation)
}

pub(crate) fn explicit_human_verification_commands(goal: &str) -> Vec<String> {
    const KNOWN: &[&str] = &[
        "task verify",
        "task check",
        "task test",
        "cargo test",
        "cargo check",
        "cargo clippy",
    ];
    let mut exact_segments = goal
        .split('`')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, segment)| segment.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    exact_segments.extend(goal.lines().filter_map(|line| {
        line.trim()
            .strip_prefix("$ ")
            .map(|command| command.trim().to_ascii_lowercase())
    }));
    KNOWN
        .iter()
        .filter(|command| exact_segments.iter().any(|segment| segment == **command))
        .map(|command| (*command).to_string())
        .collect()
}

pub(crate) fn add_independent_verification_requirements(
    plan: &mut crate::session::Plan,
    goal: &str,
    project_instructions: &str,
    source_message_index: Option<usize>,
) -> Result<Vec<String>, String> {
    if plan.steps.is_empty() {
        return Ok(Vec::new());
    }
    let step_index = plan.steps.len() - 1;
    let mut candidates = explicit_human_verification_commands(goal)
        .into_iter()
        .map(|command| {
            command_verification_obligation(
                &command,
                VerificationAuthority::Human,
                source_message_index,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    if project_instructions
        .to_ascii_lowercase()
        .contains("task verify")
    {
        candidates.push(command_verification_obligation(
            "task verify",
            VerificationAuthority::Project,
            None,
        )?);
    }
    let step = &mut plan.steps[step_index];
    let mut added = Vec::new();
    for mut obligation in candidates {
        let duplicate = step
            .verification_obligations
            .iter()
            .any(|existing| existing.expected_invocation == obligation.expected_invocation);
        if duplicate {
            continue;
        }
        obligation.plan_id.clone_from(&plan.id);
        obligation.step_index = Some(step_index);
        added.push(obligation.id.clone());
        step.verification_obligations.push(obligation);
    }
    Ok(added)
}

pub(crate) fn derive_active_plan_verification_requirements(
    store: &SessionStore,
    session_id: &str,
    goal: &str,
    project_instructions: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            let source_message_index = session
                .message_provenance
                .iter()
                .rev()
                .find(|source| source.origin.is_human())
                .map(|source| source.message_index);
            let Some(plan) = session.plan.as_mut().filter(|plan| !plan.is_complete()) else {
                return Ok(());
            };
            let added = add_independent_verification_requirements(
                plan,
                goal,
                project_instructions,
                source_message_index,
            )
            .map_err(crate::session::SessionError::InvalidMutation)?;
            if !added.is_empty() {
                append_session_event(
                    session,
                    "verification_requirements_derived",
                    json!({"verification_ids": added, "source": "resolved_human_and_project_requirements"}),
                );
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn plan_invalidation_reason(
    plan: &crate::session::Plan,
    normalized_goal: &str,
) -> Option<&'static str> {
    if !plan.has_identity() {
        Some("legacy_plan")
    } else if !plan.is_structured() {
        Some("invalid_plan")
    } else if plan.outcome.as_deref() == Some("provider_continuation_interrupted") {
        Some("provider_continuation_interrupted")
    } else if plan.is_complete() {
        Some("completed_plan")
    } else if !plan.matches_goal(normalized_goal) {
        Some("goal_mismatch")
    } else {
        None
    }
}

pub(crate) fn invalidate_plan_in_session(
    session: &mut Session,
    normalized_goal: &str,
) -> Option<&'static str> {
    let reason = session
        .plan
        .as_ref()
        .and_then(|plan| plan_invalidation_reason(plan, normalized_goal))?;
    let prior = session
        .plan
        .take()
        .expect("plan was present while determining invalidation");
    append_session_event(
        session,
        "plan_invalidated",
        json!({
            "reason": reason,
            "previous_plan_id": (!prior.id.trim().is_empty()).then_some(prior.id),
            "previous_goal": (!prior.goal.trim().is_empty()).then_some(prior.goal),
            "requested_goal": normalized_goal,
            "approved": prior.approved,
            "current_step_index": prior.current_step_index,
            "step_count": prior.steps.len(),
            "outcome": prior.outcome,
        }),
    );
    Some(reason)
}

pub(crate) fn route_idle_plan(
    store: &SessionStore,
    session_id: &str,
    normalized_goal: &str,
) -> Result<(AgentState, Option<String>), String> {
    store
        .update_session(session_id, |session| {
            invalidate_plan_in_session(session, normalized_goal);
            Ok(match session.plan.as_ref() {
                None => (AgentState::Planning, None),
                Some(plan) if !plan.approved => (AgentState::PlanApproval, Some(plan.id.clone())),
                Some(plan) => (AgentState::BuildContext, Some(plan.id.clone())),
            })
        })
        .map_err(|error| format!("failed to route the active session plan: {error}"))
}

pub(crate) fn append_plan_binding_conflict(
    session: &mut Session,
    expected_plan_id: &str,
    normalized_goal: &str,
    stage: &str,
) {
    let current_plan_id = session.plan.as_ref().map(|plan| plan.id.clone());
    let current_goal = session.plan.as_ref().map(|plan| plan.goal.clone());
    append_session_event(
        session,
        "plan_binding_conflict",
        json!({
            "stage": stage,
            "expected_plan_id": expected_plan_id,
            "expected_goal": normalized_goal,
            "current_plan_id": current_plan_id,
            "current_goal": current_goal,
        }),
    );
}

pub(crate) fn record_plan_binding_conflict(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: &str,
    normalized_goal: &str,
    stage: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            append_plan_binding_conflict(session, expected_plan_id, normalized_goal, stage);
            Ok(())
        })
        .map_err(|error| format!("failed to audit plan binding conflict: {error}"))
}

pub(crate) fn verify_bound_plan(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
    normalized_goal: &str,
    require_approved: bool,
    stage: &str,
) -> Result<bool, String> {
    store
        .update_session(session_id, |session| {
            let matches = session.plan.as_ref().is_some_and(|plan| {
                expected_plan_id == Some(plan.id.as_str())
                    && plan.is_structured()
                    && plan.matches_goal(normalized_goal)
                    && (!require_approved || plan.approved)
            });
            if !matches {
                append_plan_binding_conflict(
                    session,
                    expected_plan_id.unwrap_or(""),
                    normalized_goal,
                    stage,
                );
            }
            Ok(matches)
        })
        .map_err(|error| format!("failed to verify active plan binding: {error}"))
}

pub(crate) fn invalidate_nonresumable_plan(
    store: &SessionStore,
    session_id: &str,
    goal: &str,
) -> Result<(), String> {
    let normalized_goal = normalize_plan_goal(goal);
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to inspect existing plan: {error}"))?
        .ok_or_else(|| "session disappeared while inspecting existing plan".to_string())?;
    let Some(reason) = session
        .plan
        .as_ref()
        .and_then(|plan| plan_invalidation_reason(plan, &normalized_goal))
    else {
        return Ok(());
    };

    store
        .update_session(session_id, |session| {
            invalidate_plan_in_session(session, &normalized_goal);
            Ok(())
        })
        .map_err(|error| {
            format!("failed to invalidate {reason} before starting the requested goal: {error}")
        })
}

pub(crate) fn apply_model_override(
    config: &mut crate::config::NibConfig,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<(), String> {
    let Some(model) = model_override
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return Ok(());
    };
    let provider = provider_override
        .map(str::to_string)
        .unwrap_or_else(|| config.llm.get_active_provider());
    let entry = config.llm.providers.get_mut(&provider).ok_or_else(|| {
        format!("cannot override model for unconfigured LLM provider: {provider}")
    })?;
    entry.model = model.to_string();
    Ok(())
}

pub(crate) fn append_assistant_if_allowed(
    store: &SessionStore,
    session_id: &str,
    content: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if matches!(
                session.messages.last().map(|message| message.role.as_str()),
                Some("user") | Some("tool")
            ) {
                session.messages.push(SessionMessage {
                    index: session.messages.len(),
                    role: "assistant".to_string(),
                    content: content.to_string(),
                    timestamp: Some(Utc::now()),
                    attachments: Vec::new(),
                });
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub(crate) fn record_repeated_tool_failure(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
) -> Result<(), String> {
    store
        .record_event(
            session_id,
            "tool_failure_stalled",
            json!({
                "run_id": run_id,
                "reason": "three_consecutive_identical_failed_batches",
                "consecutive_failures": MAX_IDENTICAL_FAILED_TOOL_BATCHES,
            }),
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(crate) fn begin_plan_verification(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
    normalized_goal: &str,
    obligation_id: &str,
    request: &ToolCallRequest,
) -> Result<(), String> {
    let invocation_id = request.invocation_id;
    store
        .update_session(session_id, |session| {
            let matches = session.plan.as_ref().is_some_and(|plan| {
                expected_plan_id == Some(plan.id.as_str())
                    && plan.is_structured()
                    && plan.matches_goal(normalized_goal)
                    && plan.approved
            });
            if !matches {
                return Err(crate::session::SessionError::InvalidMutation(
                    "verification binding does not match the active approved plan".to_string(),
                ));
            }
            let step_index = session
                .plan
                .as_ref()
                .expect("plan presence was checked above")
                .current_step_index;
            session
                .plan
                .as_mut()
                .expect("plan presence was checked above")
                .begin_verification(
                    obligation_id,
                    invocation_id,
                    &request.name,
                    &request.arguments,
                    None,
                )
                .map_err(crate::session::SessionError::InvalidMutation)?;
            append_session_event(
                session,
                "verification_started",
                json!({
                    "plan_id": expected_plan_id,
                    "step_index": step_index,
                    "verification_id": obligation_id,
                    "invocation_id": invocation_id,
                }),
            );
            Ok(())
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn audited_tool_evidence(
    store: &SessionStore,
    session_id: &str,
    invocation_id: crate::tools::ToolInvocationId,
) -> Result<(bool, Option<String>), String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("session disappeared after tool invocation {invocation_id}"))?;
    let record = session
        .tool_calls
        .iter()
        .rev()
        .find(|record| record.invocation_id == Some(invocation_id));
    let mutates_content = record.is_some_and(|record| match record.tool_name.as_deref() {
        Some("apply_patch" | "merge_subagent_worktree") => true,
        Some("run_terminal") => {
            record
                .result
                .as_ref()
                .and_then(|result| result.get("risk"))
                .and_then(Value::as_str)
                != Some("read_only")
        }
        // The remaining built-in tools do not edit the active worktree. Unknown
        // audited tools fail conservatively when their recorded permission can
        // mutate local state.
        Some(
            "read_file" | "list_directory" | "grep" | "write_plan" | "spawn_subagent"
            | "invoke_subagent" | "manage_subagents" | "send_message" | "search_web"
            | "read_url_content" | "manage_task" | "manage_memory" | "schedule" | "ask_question",
        ) => false,
        _ => record
            .result
            .as_ref()
            .and_then(|result| result.get("permission_level"))
            .and_then(Value::as_str)
            .is_some_and(|permission| matches!(permission, "safe" | "destructive")),
    });
    let worktree_identity = record.and_then(|record| record.worktree_path.clone());
    Ok((mutates_content, worktree_identity))
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn compute_verification_content_identity(
    worktree_root: &Path,
    affected_paths: &[String],
) -> Result<String, String> {
    use std::io::Read;
    const MAX_ENTRIES: usize = 4_096;
    const MAX_BYTES: u64 = 64 * 1024 * 1024;

    let root = worktree_root
        .canonicalize()
        .map_err(|error| format!("resolve verification worktree: {error}"))?;
    let requested = if affected_paths.is_empty() {
        vec![".".to_string()]
    } else {
        affected_paths.to_vec()
    };
    let mut pending = Vec::new();
    for relative in requested {
        let relative_path = Path::new(&relative);
        if relative_path.is_absolute()
            || relative_path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(format!(
                "verification affected path {relative:?} is not worktree-relative"
            ));
        }
        pending.push(root.join(relative_path));
    }

    let mut entries = Vec::new();
    while let Some(path) = pending.pop() {
        if entries.len() >= MAX_ENTRIES {
            return Err("verification content identity exceeds 4096 entries".to_string());
        }
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| "verification path escaped the worktree".to_string())?
            .to_path_buf();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "verification content identity rejects linked path {:?}",
                    relative
                ));
            }
            Ok(metadata) if metadata.is_dir() => {
                entries.push((relative.clone(), None));
                let mut children = std::fs::read_dir(&path)
                    .map_err(|error| format!("read verification directory: {error}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| format!("read verification directory entry: {error}"))?;
                children.sort_by_key(std::fs::DirEntry::file_name);
                for child in children.into_iter().rev() {
                    let name = child.file_name();
                    if matches!(name.to_str(), Some(".git" | ".nib" | "target")) {
                        continue;
                    }
                    pending.push(child.path());
                }
            }
            Ok(metadata) if metadata.is_file() => {
                entries.push((relative, Some((path, metadata.len()))));
            }
            Ok(_) => {
                return Err("verification content identity encountered a special file".to_string())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                entries.push((relative, Some((path, u64::MAX))));
            }
            Err(error) => return Err(format!("inspect verification content: {error}")),
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut digest = Sha256::new();
    let mut bytes_read = 0u64;
    for (relative, file) in entries {
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        match file {
            None => digest.update(b"directory\0"),
            Some((_, u64::MAX)) => digest.update(b"missing\0"),
            Some((path, size)) => {
                bytes_read = bytes_read.saturating_add(size);
                if bytes_read > MAX_BYTES {
                    return Err("verification content identity exceeds 64 MiB".to_string());
                }
                digest.update(b"file\0");
                digest.update(size.to_le_bytes());
                let mut file = std::fs::File::open(path)
                    .map_err(|error| format!("open verification content: {error}"))?;
                let mut buffer = [0u8; 16 * 1024];
                loop {
                    let count = file
                        .read(&mut buffer)
                        .map_err(|error| format!("read verification content: {error}"))?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                }
            }
        }
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

pub(crate) fn revalidate_plan_verification_content(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let snapshot = store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "session disappeared before verification revalidation".to_string())?;
    let Some(plan) = snapshot.plan.as_ref() else {
        return Ok(Vec::new());
    };
    if expected_plan_id != Some(plan.id.as_str()) {
        return Err("verification revalidation does not match the active plan".to_string());
    }
    let Some(step) = plan.steps.get(plan.current_step_index) else {
        return Ok(Vec::new());
    };
    let mut stale = Vec::new();
    for obligation in &step.verification_obligations {
        if obligation.status != crate::session::VerificationStatus::Passed {
            continue;
        }
        let current = obligation
            .worktree_identity
            .as_deref()
            .ok_or_else(|| format!("verification {:?} has no worktree", obligation.id))
            .and_then(|root| {
                compute_verification_content_identity(Path::new(root), &obligation.affected_paths)
            });
        let mismatch = match &current {
            Ok(identity) => Some(identity.as_str()) != obligation.content_identity.as_deref(),
            Err(_) => true,
        };
        if mismatch {
            stale.push((
                obligation.id.clone(),
                current.err().unwrap_or_else(|| {
                    "verified content no longer matches its recorded identity".to_string()
                }),
            ));
        }
    }
    if stale.is_empty() {
        return Ok(Vec::new());
    }
    let stale_ids = stale.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
    store
        .update_session(session_id, |session| {
            let plan = session.plan.as_mut().ok_or_else(|| {
                crate::session::SessionError::InvalidMutation(
                    "verification revalidation lost its plan".to_string(),
                )
            })?;
            if expected_plan_id != Some(plan.id.as_str()) {
                return Err(crate::session::SessionError::InvalidMutation(
                    "verification revalidation plan changed".to_string(),
                ));
            }
            let step = plan.steps.get_mut(plan.current_step_index).ok_or_else(|| {
                crate::session::SessionError::InvalidMutation(
                    "verification revalidation has no active step".to_string(),
                )
            })?;
            for (id, reason) in &stale {
                if let Some(obligation) = step
                    .verification_obligations
                    .iter_mut()
                    .find(|obligation| obligation.id == *id)
                {
                    obligation.status = crate::session::VerificationStatus::Stale;
                    obligation.reason = Some(reason.clone());
                    obligation.updated_at = Some(Utc::now());
                }
            }
            step.status = "Blocked".to_string();
            step.outcome = Some("verified worktree content changed".to_string());
            append_session_event(
                session,
                "verification_stale",
                json!({"verification_ids": stale_ids.clone(), "reason": "content_identity_changed"}),
            );
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    Ok(stale_ids)
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn finish_plan_verification(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
    normalized_goal: &str,
    obligation_id: &str,
    worktree_identity: Option<&str>,
    result: &crate::tools::ToolResult,
) -> Result<(), String> {
    let invocation_id = result.invocation_id;
    let snapshot = store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "session disappeared before verification reconciliation".to_string())?;
    let obligation = snapshot
        .plan
        .as_ref()
        .and_then(|plan| plan.steps.get(plan.current_step_index))
        .and_then(|step| {
            step.verification_obligations
                .iter()
                .find(|obligation| obligation.id == obligation_id)
        })
        .ok_or_else(|| format!("verification obligation {obligation_id:?} disappeared"))?;
    let expected_outcome = obligation
        .expected_invocation
        .as_ref()
        .map(|expected| expected.expected_outcome)
        .ok_or_else(|| {
            format!("verification obligation {obligation_id:?} has no expected outcome")
        })?;
    let success = match expected_outcome {
        VerificationExpectedOutcome::Success => result.success,
        VerificationExpectedOutcome::ProbeMiss => {
            result.success
                && result.output.as_ref().is_some_and(|output| {
                    output
                        .get("matches")
                        .and_then(Value::as_array)
                        .is_some_and(Vec::is_empty)
                        && output.get("truncated").and_then(Value::as_bool) == Some(false)
                })
        }
    };
    let reason = if success {
        None
    } else {
        result.error.clone().or_else(|| {
            Some(match expected_outcome {
                VerificationExpectedOutcome::Success => {
                    "verification tool did not report success".to_string()
                }
                VerificationExpectedOutcome::ProbeMiss => {
                    "typed absence probe returned matches or incomplete evidence".to_string()
                }
            })
        })
    };
    let content_identity = if success {
        let root = worktree_identity.ok_or_else(|| {
            format!("successful verification obligation {obligation_id:?} has no worktree")
        })?;
        Some(compute_verification_content_identity(
            Path::new(root),
            &obligation.affected_paths,
        )?)
    } else {
        None
    };
    store
        .update_session(session_id, |session| {
            let matches = session.plan.as_ref().is_some_and(|plan| {
                expected_plan_id == Some(plan.id.as_str())
                    && plan.is_structured()
                    && plan.matches_goal(normalized_goal)
                    && plan.approved
            });
            if !matches {
                return Err(crate::session::SessionError::InvalidMutation(
                    "verification result does not match the active approved plan".to_string(),
                ));
            }
            let step_index = session
                .plan
                .as_ref()
                .expect("plan presence was checked above")
                .current_step_index;
            session
                .plan
                .as_mut()
                .expect("plan presence was checked above")
                .finish_verification(
                    obligation_id,
                    invocation_id,
                    worktree_identity,
                    content_identity.clone(),
                    success,
                    reason.clone(),
                )
                .map_err(crate::session::SessionError::InvalidMutation)?;
            append_session_event(
                session,
                "verification_finished",
                json!({
                    "plan_id": expected_plan_id,
                    "step_index": step_index,
                    "verification_id": obligation_id,
                    "invocation_id": invocation_id,
                    "worktree_identity": worktree_identity,
                    "content_identity": content_identity,
                    "status": if success { "passed" } else { "failed" },
                    "reason": reason,
                }),
            );
            Ok(())
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn invalidate_plan_verification_after_mutation(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
    normalized_goal: &str,
    invocation_id: crate::tools::ToolInvocationId,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            let matches = session.plan.as_ref().is_some_and(|plan| {
                expected_plan_id == Some(plan.id.as_str())
                    && plan.is_structured()
                    && plan.matches_goal(normalized_goal)
                    && plan.approved
            });
            if !matches {
                return Err(crate::session::SessionError::InvalidMutation(
                    "mutation does not match the active approved plan".to_string(),
                ));
            }
            let step_index = session
                .plan
                .as_ref()
                .expect("plan presence was checked above")
                .current_step_index;
            let invalidated = session
                .plan
                .as_mut()
                .expect("plan presence was checked above")
                .invalidate_verification_after_mutation(invocation_id);
            if !invalidated.is_empty() {
                append_session_event(
                    session,
                    "verification_stale",
                    json!({
                        "plan_id": expected_plan_id,
                        "step_index": step_index,
                        "invocation_id": invocation_id,
                        "verification_ids": invalidated,
                        "reason": "relevant_worktree_content_changed",
                    }),
                );
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn update_plan_tool_outcome(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
    normalized_goal: &str,
    success: bool,
    outcome: &str,
) -> Result<bool, String> {
    store
        .update_session(session_id, |session| {
            let matches = session.plan.as_ref().is_some_and(|plan| {
                expected_plan_id == Some(plan.id.as_str())
                    && plan.is_structured()
                    && plan.matches_goal(normalized_goal)
                    && plan.approved
            });
            if !matches {
                let current_plan_id = session.plan.as_ref().map(|plan| plan.id.clone());
                let current_goal = session.plan.as_ref().map(|plan| plan.goal.clone());
                append_session_event(
                    session,
                    "plan_binding_conflict",
                    json!({
                        "stage": "tool_outcome",
                        "expected_plan_id": expected_plan_id,
                        "expected_goal": normalized_goal,
                        "current_plan_id": current_plan_id,
                        "current_goal": current_goal,
                    }),
                );
                return Ok(false);
            }
            let plan = session
                .plan
                .as_mut()
                .expect("plan presence was checked above");
            plan.record_tool_outcome(success, outcome);
            Ok(true)
        })
        .map_err(|error| error.to_string())
}

pub(crate) async fn transition_state(
    store: &SessionStore,
    session_id: &str,
    current: AgentState,
    next: AgentState,
    trace: &mut Vec<String>,
    transition_count: &mut u32,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AgentState, String> {
    if !current.can_transition_to(next) {
        return Err(format!("invalid agent transition: {current} -> {next}"));
    }
    *transition_count = transition_count.saturating_add(1);
    trace.push(next.as_str().to_string());
    store
        .record_event(
            session_id,
            "state_transition",
            json!({"from": current.as_str(), "to": next.as_str()}),
        )
        .map_err(|error| error.to_string())?;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: next.as_str().to_string(),
        },
    )
    .await;
    Ok(next)
}

pub(crate) async fn emit(stream_tx: &Option<Sender<StreamEvent>>, event: StreamEvent) {
    if let Some(sender) = stream_tx {
        let _ = sender.send(event).await;
    }
}

pub(crate) fn emit_plan_progress(
    store: &SessionStore,
    session_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
    sensitive_values: &[String],
) -> Result<(), String> {
    if stream_tx.is_none() {
        return Ok(());
    }
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load persisted plan progress: {error}"))?;
    if let Some(plan) = session
        .as_ref()
        .and_then(|session| session.plan.as_ref())
        .filter(|plan| plan.steps.len() > 1)
    {
        emit_progress_nonblocking(
            stream_tx,
            StreamEvent::PlanProgress(crate::interactive::plan_progress_from_plan(
                plan,
                sensitive_values,
            )),
        );
    }
    Ok(())
}

pub(crate) fn emit_progress_nonblocking(
    stream_tx: &Option<Sender<StreamEvent>>,
    event: StreamEvent,
) {
    if let Some(sender) = stream_tx {
        // Progress is advisory. Keep space for the terminal event when an
        // observer is not draining its bounded channel.
        if sender.capacity() > 4 {
            let _ = sender.try_send(event);
        }
    }
}

pub(crate) fn emit_nonblocking(stream_tx: &Option<Sender<StreamEvent>>, event: StreamEvent) {
    if let Some(sender) = stream_tx {
        let _ = sender.try_send(event);
    }
}

/// Holds provider deltas behind the private terminal-authority boundary.
///
/// Network adapters bound event count and bytes before constructing `LlmStream`. Returning
/// projections only beside a validated, non-refusal response makes it impossible for callers to
/// accidentally publish a partial stream that later fails terminal validation.
// Preserve the canonical typed error, including its retry and redaction-safe failure metadata,
// across this private terminal-authority boundary rather than introducing a boxed error API.
#[allow(clippy::result_large_err)]
pub(crate) async fn finish_private_provider_stream(
    stream: LlmStream,
    sensitive_values: &[String],
) -> Result<(LlmResponse, Vec<StreamEvent>), LlmError> {
    let response = stream.finish().await?;
    let pending = project_validated_llm_response(&response, sensitive_values);
    Ok((response, pending))
}

pub(crate) fn project_validated_llm_response(
    response: &LlmResponse,
    sensitive_values: &[String],
) -> Vec<StreamEvent> {
    if response.terminal_status == LlmTerminalStatus::Refused {
        return Vec::new();
    }
    let mut events = Vec::new();
    if let Some(content) = response.content.as_ref() {
        events.push(StreamEvent::Content(
            crate::interactive::bounded_public_text(
                content,
                sensitive_values,
                MAX_PUBLIC_PROVIDER_CONTENT_BYTES,
                true,
            ),
        ));
    }
    if let Some(tool_calls) = response.tool_calls.as_ref() {
        events.extend(tool_calls.iter().enumerate().map(|(index, call)| {
            StreamEvent::ToolCallChunk {
                invocation_id: call.invocation_id,
                index,
                name: Some(crate::interactive::bounded_public_text(
                    &call.name,
                    sensitive_values,
                    MAX_PUBLIC_PROVIDER_TOOL_CHUNK_BYTES,
                    false,
                )),
                arguments: Some(crate::interactive::bounded_public_text(
                    &call.arguments.to_string(),
                    sensitive_values,
                    MAX_PUBLIC_PROVIDER_TOOL_CHUNK_BYTES,
                    false,
                )),
            }
        }));
    }
    events
}

pub(crate) fn safe_persisted_provider_message(
    value: &str,
    sensitive_values: &[String],
    preserve_layout: bool,
) -> String {
    if value.len() > MAX_PERSISTED_PROVIDER_MESSAGE_BYTES {
        return format!(
            "[provider content omitted: exceeded the {MAX_PERSISTED_PROVIDER_MESSAGE_BYTES}-byte safety/storage limit and was not retained]"
        );
    }
    let sanitized = crate::interactive::bounded_public_text(
        value,
        sensitive_values,
        usize::MAX,
        preserve_layout,
    );
    if sanitized.len() > MAX_PERSISTED_PROVIDER_MESSAGE_BYTES {
        return format!(
            "[provider content omitted: exceeded the {MAX_PERSISTED_PROVIDER_MESSAGE_BYTES}-byte safety/storage limit and was not retained]"
        );
    }
    sanitized
}

pub(crate) fn safe_provider_plan_outcome(
    content: Option<&str>,
    sensitive_values: &[String],
) -> String {
    content
        .map(|content| safe_persisted_provider_message(content, sensitive_values, true))
        .unwrap_or_else(|| "model completed the plan step".to_string())
}

pub(crate) fn sanitize_provider_plan(plan: &mut crate::session::Plan, sensitive_values: &[String]) {
    for step in &mut plan.steps {
        step.description = crate::interactive::bounded_public_text(
            &step.description,
            sensitive_values,
            MAX_PUBLIC_PROVIDER_TOOL_CHUNK_BYTES,
            false,
        );
    }
}

pub(crate) fn skill_policy_rules(skills: &[Skill]) -> Vec<PolicyRule> {
    crate::context::skills::policy_rules_for_skills(skills)
        .into_iter()
        .map(|rule| PolicyRule {
            effect: match rule.effect {
                SkillPolicyEffect::Deny => PolicyEffect::Deny,
                SkillPolicyEffect::RequireApproval => PolicyEffect::RequireApproval,
            },
            tool_name: rule.tool_name.unwrap_or_else(|| "*".to_string()),
            argument_contains: rule.argument_contains,
            reason: format!("skill '{}' constraint", rule.skill_name),
        })
        .collect()
}

pub(crate) fn skill_after_tool_hooks(skills: &[Skill]) -> Vec<AfterToolHook> {
    skills
        .iter()
        .flat_map(|skill| {
            skill
                .frontmatter
                .hooks
                .after_tool
                .iter()
                .filter(|hook| !hook.tool.trim().is_empty() && !hook.command.trim().is_empty())
                .map(|hook| AfterToolHook {
                    source: skill.frontmatter.name.clone(),
                    tool_name: hook.tool.clone(),
                    command: hook.command.clone(),
                })
        })
        .collect()
}
