//! Terminal-observed changes (T080 phase 5b, section 6b).
//!
//! In place, nib cannot know which files a terminal command wrote. It takes a
//! metadata snapshot of the project before and after the command and records
//! the paths that appeared, disappeared or changed. No Git command runs, so
//! the observation needs no sandbox and works on every platform. A snapshot
//! that could not see the whole project is incomplete, and so is any change
//! set derived from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A snapshot stops after this many entries and is then incomplete.
pub(crate) const MAX_ENTRIES: usize = 50_000;
/// Names skipped at any depth: Git and nib state, and common build output.
/// Changed paths therefore never name a directory the verification
/// identity would have to walk in full.
const SKIPPED: &[&str] = &[".git", ".nib", "target", "node_modules"];

/// Built-in tools that never edit project files. Any other tool's record
/// must say what it changed in place, or the session's changes are unknown.
pub(crate) fn edits_no_project_files(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "read_file"
            | "list_directory"
            | "grep"
            | "git_status"
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Directory,
    File(FileStamp),
    Link(PathBuf),
    Special,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<u128>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

/// The entries below a project top, keyed like the stale-write ledger.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    entries: BTreeMap<String, Entry>,
    complete: bool,
}

/// Paths a command changed. `complete` is false when either snapshot was
/// cut short, so other paths may have changed too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedChanges {
    pub(crate) paths: Vec<String>,
    pub(crate) complete: bool,
}

impl Snapshot {
    pub(crate) fn take(root: &Path) -> Self {
        Self::take_bounded(root, MAX_ENTRIES)
    }

    pub(crate) fn take_bounded(root: &Path, max_entries: usize) -> Self {
        let mut entries = BTreeMap::new();
        let mut complete = true;
        let mut pending = vec![(root.to_path_buf(), String::new())];
        while let Some((directory, prefix)) = pending.pop() {
            let Ok(children) = std::fs::read_dir(&directory) else {
                complete = false;
                continue;
            };
            for child in children {
                let Ok(child) = child else {
                    complete = false;
                    continue;
                };
                let name = child.file_name();
                let Some(name) = name.to_str() else {
                    // A name that cannot be keyed cannot be reported.
                    complete = false;
                    continue;
                };
                if SKIPPED.contains(&name) {
                    continue;
                }
                if entries.len() >= max_entries {
                    return Self {
                        entries,
                        complete: false,
                    };
                }
                let key = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}/{name}")
                };
                let path = child.path();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    complete = false;
                    continue;
                };
                let entry = if metadata.file_type().is_symlink() {
                    Entry::Link(std::fs::read_link(&path).unwrap_or_default())
                } else if metadata.is_dir() {
                    pending.push((path, key.clone()));
                    Entry::Directory
                } else if metadata.is_file() {
                    Entry::File(stamp(&metadata))
                } else {
                    Entry::Special
                };
                entries.insert(key, entry);
            }
        }
        Self { entries, complete }
    }

    /// The paths that differ between `before` and this later snapshot.
    pub(crate) fn changes_since(&self, before: &Self) -> ObservedChanges {
        let mut paths: Vec<String> = before
            .entries
            .iter()
            .filter(|(key, entry)| self.entries.get(*key) != Some(*entry))
            .map(|(key, _)| key.clone())
            .collect();
        paths.extend(
            self.entries
                .keys()
                .filter(|key| !before.entries.contains_key(*key))
                .cloned(),
        );
        paths.sort();
        paths.dedup();
        ObservedChanges {
            paths,
            complete: before.complete && self.complete,
        }
    }
}

fn stamp(metadata: &std::fs::Metadata) -> FileStamp {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_nanos());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileStamp {
            len: metadata.len(),
            modified,
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        }
    }
    #[cfg(not(unix))]
    FileStamp {
        len: metadata.len(),
        modified,
    }
}

#[cfg(test)]
#[path = "observed_changes_tests.rs"]
mod tests;
