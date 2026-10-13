//! T080 phase 5b: which project files an in-place call changed (section 6b).
//!
//! In a nib-managed worktree the worktree itself is the isolation and nothing
//! is recorded. In place, each audited call that can edit project files
//! records its changed paths, so verification can be scoped to the files
//! the session changed instead of the whole shared folder.

use super::*;
use crate::session::ChangedPaths;
use crate::tools::file_reads::TrackedPath;
use crate::tools::observed_changes::Snapshot;

/// Built-in tools that never edit project files. Other tools that can change
/// local state are treated as unobservable edits.
pub(crate) fn edits_no_project_files(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file"
            | "list_directory"
            | "grep"
            | "git_status"
            | "git_commit"
            | "git_push"
            | "write_plan"
            | "spawn_subagent"
            | "invoke_subagent"
            | "manage_subagents"
            | "send_message"
            | "search_web"
            | "read_url_content"
            | "manage_task"
            | "manage_memory"
            | "schedule"
            | "ask_question"
            | "load_skill"
            | "read_skill_resource"
    )
}

/// What an in-place call may change, captured before it runs.
pub(crate) enum ChangeWatch {
    /// Not in place, no session, or a call that edits no project files.
    Off,
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
        if session_id.is_none()
            || edits_no_project_files(&call.tool_name)
            || crate::sandbox::in_managed_worktree(execution_root)
        {
            return ChangeWatch::Off;
        }
        let Ok(top) = self.project_root.canonicalize() else {
            return ChangeWatch::Off;
        };
        if !execution_root.starts_with(&top) {
            return ChangeWatch::Off;
        }
        let root = self.redact_text(&top.to_string_lossy());
        match call.tool_name.as_str() {
            "apply_patch" => {
                if call.arguments.get("dry_run").and_then(Value::as_bool) != Some(false) {
                    return ChangeWatch::Off;
                }
                match patch_targets {
                    Some(targets) => ChangeWatch::Patch {
                        root,
                        keys: targets.iter().map(|target| target.key.clone()).collect(),
                    },
                    None => ChangeWatch::Unknown { root },
                }
            }
            "run_terminal" if risk == ToolRisk::ReadOnly => ChangeWatch::Off,
            "run_terminal"
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
            Self::Patch { root, keys } => {
                if !ran {
                    return None;
                }
                ChangedPaths {
                    root,
                    paths: keys,
                    complete: true,
                }
            }
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
