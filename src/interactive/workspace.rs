//! `/worktree` and the session workspace shown by the status views
//! (T080 phase 5c).

use crate::session::{Session, SessionStore, SessionWorkspace};
use crate::tools::workspace;
use std::path::Path;

/// The workspace a session works in, as its next run will decide it.
pub(crate) fn session_workspace(
    project_root: &Path,
    session: Option<&Session>,
    session_id: &str,
) -> SessionWorkspace {
    let configured = crate::config::load_nib_config_full(project_root)
        .map(|config| config.execution.workspace)
        .unwrap_or_else(|_| crate::config::default_workspace());
    workspace::decide(
        project_root,
        &configured,
        session.and_then(|session| session.workspace.as_ref()),
        session_id,
    )
}

/// The same, reading the session from the store.
pub(crate) fn stored_session_workspace(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
) -> SessionWorkspace {
    let session = store.load_result(session_id).ok().flatten();
    session_workspace(project_root, session.as_ref(), session_id)
}

/// `/worktree`: the session works in its own worktree from its next request.
pub(crate) fn request_worktree(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    reason: &str,
) -> Result<String, String> {
    if workspace::project_is_linked_worktree(project_root) {
        return Ok(
            "This folder is itself a Git worktree, so nib works in it directly. Start nib in the main checkout to give a session its own worktree."
                .to_string(),
        );
    }
    let current = stored_session_workspace(project_root, store, session_id);
    if current.is_worktree() {
        return Ok(format!(
            "This session already works in its own worktree ({}).",
            current.reason
        ));
    }
    store
        .update_session(session_id, |session| {
            session.workspace = Some(SessionWorkspace::worktree(reason));
            Ok(())
        })
        .map_err(|error| format!("failed to switch the session to a worktree: {error}"))?;
    Ok("From your next request, this session works in its own worktree on a nib/session branch, starting from the last commit. Uncommitted changes in this folder, including this session's earlier edits, stay in the folder and are not copied.".to_string())
}

/// `--worktree`: the session (created when missing) works in its own
/// worktree. Returns the workspace summary to show.
pub fn start_in_worktree(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
) -> Result<String, String> {
    if store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        store
            .try_create_session_with_id(session_id.to_string())
            .map_err(|error| format!("failed to create session: {error}"))?;
    }
    request_worktree(project_root, store, session_id, workspace::FLAG_REASON)?;
    Ok(workspace_summary(project_root, store, session_id))
}

/// One line naming where the session works and why.
pub fn workspace_summary(project_root: &Path, store: &SessionStore, session_id: &str) -> String {
    let current = stored_session_workspace(project_root, store, session_id);
    let place = if current.is_worktree() {
        "its own worktree"
    } else {
        "this folder (in place)"
    };
    format!("workspace: {place}, because {}", current.reason)
}

/// `in place` or `worktree <path>`, for `/status`.
pub(crate) fn workspace_label(
    project_root: &Path,
    session: Option<&Session>,
    session_id: &str,
) -> Result<String, String> {
    let current = session_workspace(project_root, session, session_id);
    if !current.is_worktree() {
        return Ok("in place".to_string());
    }
    let path = crate::integrations::worktree::with_validated_session_worktree(
        project_root,
        session_id,
        |path| Ok(path.display().to_string()),
    )?
    .unwrap_or_else(|| "not created yet".to_string());
    Ok(format!("worktree {path} ({})", current.reason))
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
