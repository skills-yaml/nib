//! Where a session works (T080 phase 5c): the user's folder (in place, the
//! default) or the session's own managed worktree.
//!
//! The decision is made at the start of each run and recorded on the session
//! with its reason. A session never moves from its worktree back into the
//! folder on its own; its work is landed or discarded first.

use crate::session::SessionWorkspace;
use std::path::Path;

/// Directive in a project instruction file that requires session worktrees.
pub(crate) const WORKTREE_DIRECTIVE: &str = "nib-workspace: worktree";
pub(crate) const DEFAULT_REASON: &str = "nib works in your folder by default";
pub(crate) const LINKED_PROJECT_REASON: &str = "this folder is itself a Git worktree";
pub(crate) const CONFIGURED_REASON: &str = "execution.workspace is \"worktree\"";
pub(crate) const LEGACY_REASON: &str = "this session already had a worktree";
pub(crate) const COMMAND_REASON: &str = "you ran /worktree";
pub(crate) const LANDED_REASON: &str = "its worktree was landed with /land";
pub(crate) const DISCARDED_REASON: &str = "its worktree was discarded with /land";
pub(crate) const FLAG_REASON: &str = "nib was started with --worktree";
pub(crate) const SCHEDULED_REASON: &str = "scheduled work always runs in a worktree";
pub(crate) const MCP_SERVER_REASON: &str = "MCP server calls always run in a worktree";

/// Whether the project is itself a linked Git worktree (its `.git` is a
/// file). nib does not nest session worktrees there; sessions work in place.
pub(crate) fn project_is_linked_worktree(project_root: &Path) -> bool {
    project_root.join(".git").is_file()
}

/// Decides a session's workspace. `recorded` is what the session file says;
/// `configured` is `execution.workspace`. Forced worktrees (scheduled runs,
/// MCP-server calls) are applied by the caller and never recorded.
pub(crate) fn decide(
    project_root: &Path,
    configured: &str,
    recorded: Option<&SessionWorkspace>,
    session_id: &str,
) -> SessionWorkspace {
    if project_is_linked_worktree(project_root) {
        return SessionWorkspace::in_place(LINKED_PROJECT_REASON);
    }
    if let Some(recorded) = recorded.filter(|recorded| recorded.is_worktree()) {
        return recorded.clone();
    }
    // Instructions first: an executor's configuration already carries their
    // requirement, and the instruction file is the better reason.
    if let Some(reason) = instruction_requirement(project_root) {
        return SessionWorkspace::worktree(reason);
    }
    if configured == "worktree" {
        return SessionWorkspace::worktree(CONFIGURED_REASON);
    }
    match recorded {
        Some(recorded) => recorded.clone(),
        // A session from before phase 5c keeps the worktree its edits are in.
        None if has_session_worktree(project_root, session_id) => {
            SessionWorkspace::worktree(LEGACY_REASON)
        }
        None => SessionWorkspace::in_place(DEFAULT_REASON),
    }
}

/// The instruction file that requires session worktrees, as a reason.
/// Unreadable instruction files already deny every tool call (see
/// `load_instruction_policy_rules`), so they are skipped here.
pub(crate) fn instruction_requirement(project_root: &Path) -> Option<String> {
    crate::tools::executor::instruction_policy_files(project_root)
        .into_iter()
        .find_map(|file| {
            let contents = crate::tools::executor::read_instruction_policy_file(&file).ok()?;
            contents
                .lines()
                .map(|line| line.trim().trim_start_matches(['-', '*']).trim())
                .any(|line| line == WORKTREE_DIRECTIVE)
                .then(|| {
                    let name = file.strip_prefix(project_root).unwrap_or(&file);
                    format!(
                        "{} requires a worktree ({WORKTREE_DIRECTIVE})",
                        name.display()
                    )
                })
        })
}

/// Whether the session has a session worktree receipt. A receipt that
/// cannot be read counts, so earlier work is never hidden.
fn has_session_worktree(project_root: &Path, session_id: &str) -> bool {
    let Ok(root) = project_root.canonicalize() else {
        return false;
    };
    !matches!(
        crate::sandbox::worktree::load_managed_worktree_ownership(
            &root,
            crate::sandbox::worktree::ManagedWorktreeKind::Session,
            session_id,
        ),
        Ok(None)
    )
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
