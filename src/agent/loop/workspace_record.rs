//! T080 phase 5b: what each run records about its workspace (section 6b).
//!
//! `run_started` names the workspace, the checkpoint before the run and the
//! other live sessions in the folder. `run_terminal` names the files the run
//! changed in place and their content identity, so a reviewer can tell what
//! a run verified and what else was going on in a shared folder.

use super::*;

/// Details added to `run_started`.
pub(crate) fn run_started_workspace(
    project_root: &Path,
    sessions_dir: &Path,
    session_id: &str,
) -> Value {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let in_place =
        ToolExecutor::edits_in_place(&root) && !crate::sandbox::in_managed_worktree(&root);
    let checkpoint_before =
        crate::tools::checkpoints::CheckpointStore::for_session(sessions_dir, session_id)
            .and_then(|store| store.list())
            .ok()
            .and_then(|checkpoints| checkpoints.last().map(|checkpoint| checkpoint.number));
    json!({
        "workspace": if in_place { "in_place" } else { "worktree" },
        "workspace_root": in_place.then(|| root.to_string_lossy().to_string()),
        "checkpoint_before": checkpoint_before,
        "other_sessions": crate::session::presence::other_sessions_in(sessions_dir, &root, session_id),
    })
}

/// Details added to `run_terminal`: in place, the files this run changed
/// (or `"unknown"`) and the content identity over them.
pub(crate) fn run_terminal_workspace(session: &crate::session::Session, run_id: &str) -> Value {
    let Some(started) = session.events.iter().find(|event| {
        event.kind == "run_started"
            && event.details.get("run_id").and_then(Value::as_str) == Some(run_id)
    }) else {
        return json!({});
    };
    let Some(root) = started
        .details
        .get("workspace_root")
        .and_then(Value::as_str)
    else {
        return json!({});
    };
    match session.in_place_changes(root, started.timestamp) {
        Some(paths) => {
            let identity = content_identity_over(Path::new(root), paths.clone()).ok();
            json!({"changed_paths": paths, "content_identity": identity})
        }
        None => json!({"changed_paths": "unknown"}),
    }
}

#[cfg(test)]
#[path = "workspace_record_tests.rs"]
mod tests;
