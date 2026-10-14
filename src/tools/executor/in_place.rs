//! T080 phase 5b: which project files an in-place call changed (section 6b).
//!
//! In a nib-managed worktree the worktree itself is the isolation and nothing
//! is recorded. In place, each audited call that can edit project files
//! records its changed paths, so verification can be scoped to the files
//! the session changed instead of the whole shared folder.

use super::*;
use crate::session::ChangedPaths;
use crate::tools::file_reads::TrackedPath;
use crate::tools::observed_changes::{edits_no_project_files, Snapshot};

/// What an in-place call may change, captured before it runs.
pub(crate) enum ChangeWatch {
    /// Not in place, no session, or a built-in tool that edits no project
    /// files: nothing is recorded.
    Off,
    /// In place, a call that cannot edit project files (a dry run, a
    /// read-only command or tool): recorded as changing nothing, so a
    /// missing record can mean "unknown" (T080 phase 5b review F2).
    Nothing { root: String },
    /// A real `apply_patch`: its targets are known before it runs.
    Patch { root: String, keys: Vec<String> },
    /// A foreground terminal command: the project as it was before.
    Command {
        root: String,
        top: PathBuf,
        before: Snapshot,
    },
    /// An edit nib cannot observe, such as a background command.
    Unknown { root: String },
}

impl ToolExecutor {
    /// Starts watching what `call` changes when it runs in place.
    pub(crate) async fn watch_changes(
        &self,
        call: &ToolCall,
        risk: ToolRisk,
        execution_root: &Path,
        session_id: Option<&str>,
        patch_targets: Option<&[TrackedPath]>,
    ) -> ChangeWatch {
        if session_id.is_none() || edits_no_project_files(&call.tool_name) {
            return ChangeWatch::Off;
        }
        let Some((root, top)) = self.in_place_top(execution_root) else {
            return ChangeWatch::Off;
        };
        match call.tool_name.as_str() {
            "apply_patch" => {
                if call.arguments.get("dry_run").and_then(Value::as_bool) != Some(false) {
                    return ChangeWatch::Nothing { root };
                }
                match patch_targets {
                    Some(targets) => ChangeWatch::Patch {
                        root,
                        keys: targets.iter().map(|target| target.key.clone()).collect(),
                    },
                    None => ChangeWatch::Unknown { root },
                }
            }
            _ if risk == ToolRisk::ReadOnly => ChangeWatch::Nothing { root },
            // Commits and pushes run the user's hooks, which may rewrite
            // files (a pre-commit formatter), so they are observed too.
            "run_terminal" | "git_commit" | "git_push"
                if call.arguments.get("background").and_then(Value::as_bool) != Some(true) =>
            {
                let walk = top.clone();
                match tokio::task::spawn_blocking(move || Snapshot::take(&walk)).await {
                    Ok(before) => ChangeWatch::Command { root, top, before },
                    Err(_) => ChangeWatch::Unknown { root },
                }
            }
            _ => ChangeWatch::Unknown { root },
        }
    }
}

impl ToolExecutor {
    /// The project top, as recorded and as a path, when `execution_root`
    /// runs in place below it rather than in a nib-managed worktree.
    pub(crate) fn in_place_top(&self, execution_root: &Path) -> Option<(String, PathBuf)> {
        if crate::sandbox::in_managed_worktree(execution_root) {
            return None;
        }
        let top = self.project_root.canonicalize().ok()?;
        execution_root
            .starts_with(&top)
            .then(|| (self.redact_text(&top.to_string_lossy()), top))
    }

    /// The record for a call refused before it ran: in place it changed
    /// nothing; elsewhere nothing is recorded.
    pub(crate) fn unchanged_in_place(
        &self,
        call: &ToolCall,
        session_id: Option<&str>,
        effective_root: &Path,
    ) -> Option<ChangedPaths> {
        if session_id.is_none() || edits_no_project_files(&call.tool_name) {
            return None;
        }
        let (root, _) = self.in_place_top(effective_root)?;
        Some(ChangedPaths {
            root,
            paths: Vec::new(),
            complete: true,
        })
    }
}

impl ChangeWatch {
    /// The paths the call changed, once it has finished. `ran` is false when
    /// the call failed before it could edit anything.
    pub(crate) async fn finish(
        self,
        ran: bool,
        redact: impl Fn(&str) -> String,
    ) -> Option<ChangedPaths> {
        let changed = match self {
            Self::Off => return None,
            Self::Nothing { root } => ChangedPaths {
                root,
                paths: Vec::new(),
                complete: true,
            },
            // `git apply` applies all or nothing, so a patch that failed
            // changed nothing.
            Self::Patch { root, keys } => ChangedPaths {
                root,
                paths: if ran { keys } else { Vec::new() },
                complete: true,
            },
            Self::Command { root, top, before } => {
                match tokio::task::spawn_blocking(move || Snapshot::take(&top)).await {
                    Ok(after) => {
                        let observed = after.changes_since(&before);
                        ChangedPaths {
                            root,
                            paths: observed.paths,
                            complete: observed.complete,
                        }
                    }
                    Err(_) => ChangedPaths {
                        root,
                        paths: Vec::new(),
                        complete: false,
                    },
                }
            }
            Self::Unknown { root } => ChangedPaths {
                root,
                paths: Vec::new(),
                complete: false,
            },
        };
        // A path the audit would redact cannot be stored as it is, and a
        // redacted path would name the wrong file, so the record is then
        // incomplete instead.
        let complete = changed.complete && changed.paths.iter().all(|path| redact(path) == *path);
        Some(ChangedPaths {
            paths: if complete { changed.paths } else { Vec::new() },
            complete,
            root: changed.root,
        })
    }
}
