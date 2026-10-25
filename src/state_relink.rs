//! T082 phase 5: finds and re-links the nib state of a moved project, and
//! reports where a project's nib files are.
//!
//! nib keys a project's state by its folder (`~/.nib/projects/<key>`, the key
//! hashes the canonical root). A copied project therefore starts with state
//! of its own and leaves the original untouched, and a moved project starts
//! fresh while its earlier state stays under the old key. `nib doctor`
//! reports both; `nib doctor --fix --relink <old folder>` moves the state of
//! a moved project to its new key, after the user named the old folder.
//!
//! Re-linking is serialized by a lock in `~/.nib/projects`, refuses while
//! the old state is in use, and never overwrites or deletes state: a state
//! the new folder already has (with its sessions) is kept aside as
//! `projects/.relinked-<key>-<time>`, and worktree receipts that name the old
//! folder are kept in the state's `relinked-<time>/` directory. A journal
//! (`relink.toml`) written before the first change, and updated after each
//! step, lets an interrupted re-link finish on the next run.

use super::{live_activity, rename_no_replace, sync_parent, write_private_file};
use crate::paths::{NibPaths, PROJECT_IDENTITY_FILE, STATE_RECORD_FILE};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The re-link journal, in the state directory being re-linked.
pub(crate) const RELINK_RECORD_FILE: &str = "relink.toml";

/// Serializes re-links, in `<user home>/projects`.
const RELINK_LOCK_FILE: &str = ".relink.lock";

/// Largest identity or journal file read.
const MAX_RECORD_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RelinkRecord {
    version: u32,
    /// The key the state had.
    from: String,
    /// The key it moves to.
    to: String,
    /// The folder it belongs to now.
    root: PathBuf,
    started_at: String,
    /// States the new folder already had, kept aside.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    asides: Vec<AsideRecord>,
    /// Where worktree receipts naming the old folder were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receipts: Option<PathBuf>,
    /// Whether the sessions moved from `from` to `to`.
    #[serde(default)]
    sessions_moved: bool,
}

/// A state the new folder had, kept aside under its own key with its
/// sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AsideRecord {
    key: String,
    path: PathBuf,
    #[serde(default)]
    sessions_moved: bool,
}

/// How another state directory relates to the current folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateLinkKind {
    /// Its folder no longer exists and had the same name: this project was
    /// probably moved or renamed there.
    Moved,
    /// Its folder no longer exists under another name.
    Orphaned,
    /// Its folder still exists with the same name and repository: probably
    /// a copy. Each folder keeps its own state.
    Copied,
    /// A re-link to this folder was interrupted.
    Interrupted,
}

/// A state directory the current folder may belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateLinkCandidate {
    pub key: String,
    pub state_dir: PathBuf,
    /// The folder its identity record names.
    pub recorded_root: PathBuf,
    pub kind: StateLinkKind,
    /// Sessions the session database holds under its key.
    pub sessions: u64,
}

/// What a re-link did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelinkOutcome {
    pub state_dir: PathBuf,
    pub sessions_moved: u64,
    /// Where states the new folder already had were kept.
    pub replaced: Vec<PathBuf>,
    /// Where worktree receipts naming the old folder were kept.
    pub receipts: Option<PathBuf>,
    /// Managed worktrees whose Git links `git worktree repair` refreshed,
    /// or the reason it could not.
    pub worktree_repair: Option<String>,
}

/// Where a project's nib files are, for `nib doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutReport {
    pub user_home: Option<PathBuf>,
    pub database: Option<PathBuf>,
    /// The folder's runtime state directory.
    pub state_dir: PathBuf,
    /// Whether that directory is in the user home.
    pub state_in_user_home: bool,
    /// The state record's status (`done`, `moving`), if any.
    pub state_status: Option<String>,
    /// Sessions the database holds for this folder.
    pub sessions: Option<u64>,
    /// Older nib files still in the project's `.nib`, one line each.
    pub legacy: Vec<String>,
    /// Session files the migration kept in the project's legacy copy but
    /// could not import into the session database.
    pub not_imported: Vec<PathBuf>,
    /// States this folder had before a re-link replaced them, kept aside as
    /// `projects/.relinked-<key>-<time>`.
    pub asides: Vec<PathBuf>,
    pub candidates: Vec<StateLinkCandidate>,
}

/// The layout report of the folder at `project_root`.
pub fn layout_report(project_root: &Path) -> Result<LayoutReport, String> {
    let root = canonical_root(project_root)?;
    let state_dir = NibPaths::project_state(&root);
    let user_home = NibPaths::user_home();
    let state_in_user_home = super::user_home_containing(&state_dir).is_some();
    let state_record = read_table(&state_dir.join(STATE_RECORD_FILE));
    let state_status = state_record
        .as_ref()
        .and_then(|table| table.get("status")?.as_str().map(str::to_string));
    let not_imported = state_record
        .as_ref()
        .and_then(|table| table.get("not_imported")?.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|path| path.as_str().map(PathBuf::from))
        .collect();
    let counts = session_counts();
    let key = NibPaths::state_key(&root);
    let sessions = key.as_ref().map(|key| {
        counts
            .iter()
            .filter(|(project, _, _)| project == key)
            .map(|(_, _, count)| count)
            .sum()
    });
    Ok(LayoutReport {
        user_home,
        database: NibPaths::session_database().filter(|path| path.is_file()),
        state_dir,
        state_in_user_home,
        state_status,
        sessions,
        legacy: legacy_findings(&root),
        not_imported,
        asides: key.as_deref().map(aside_states).unwrap_or_default(),
        candidates: find_candidates_with(&root, &counts)?,
    })
}

/// The states of the folder keyed `key` that re-links kept aside, sorted.
fn aside_states(key: &str) -> Vec<PathBuf> {
    let prefix = format!(".relinked-{key}-");
    let mut asides: Vec<PathBuf> = NibPaths::user_project_dirs()
        .into_iter()
        .filter(|state_dir| {
            state_dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect();
    asides.sort();
    asides
}

/// The state directories the folder at `project_root` may belong to.
pub fn find_candidates(project_root: &Path) -> Result<Vec<StateLinkCandidate>, String> {
    let root = canonical_root(project_root)?;
    find_candidates_with(&root, &session_counts())
}

fn find_candidates_with(
    root: &Path,
    counts: &[(String, String, u64)],
) -> Result<Vec<StateLinkCandidate>, String> {
    let Some(own_key) = NibPaths::state_key(root) else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    for state_dir in NibPaths::user_project_dirs() {
        let Some(key) = state_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if key == own_key || key.starts_with('.') {
            continue;
        }
        let journal = read_journal(&state_dir.join(RELINK_RECORD_FILE));
        let Some(recorded_root) = recorded_root_of(&state_dir) else {
            continue;
        };
        let kind = if journal.is_some_and(|journal| journal.to == own_key) || recorded_root == root
        {
            StateLinkKind::Interrupted
        } else if !recorded_root.exists() {
            if recorded_root.file_name() == root.file_name() {
                StateLinkKind::Moved
            } else {
                StateLinkKind::Orphaned
            }
        } else if recorded_root.file_name() == root.file_name()
            && same_repository(root, &recorded_root)
        {
            StateLinkKind::Copied
        } else {
            continue;
        };
        let sessions = counts
            .iter()
            .filter(|(project, _, _)| project == key)
            .map(|(_, _, count)| count)
            .sum();
        candidates.push(StateLinkCandidate {
            key: key.to_string(),
            state_dir,
            recorded_root,
            kind,
            sessions,
        });
    }
    Ok(candidates)
}

/// Re-links the nib state of `from` (the folder the project was moved from,
/// or its state key) to the folder at `project_root`.
pub fn relink(project_root: &Path, from: &str) -> Result<RelinkOutcome, String> {
    let root = canonical_root(project_root)?;
    if NibPaths::is_managed_worktree(&root) {
        return Err("a worktree nib manages keeps its state inside itself".to_string());
    }
    let new_key = NibPaths::state_key(&root).ok_or("the project folder cannot be resolved")?;
    let new_state = NibPaths::user_state_dir(&root).ok_or("nib has no user home")?;
    let projects = new_state
        .parent()
        .ok_or("nib's projects directory cannot be resolved")?
        .to_path_buf();
    super::ensure_state_directory(&projects)
        .map_err(|error| format!("{} is unusable: {error}", projects.display()))?;
    let old_key = resolve_from(&projects, from, &root)?;
    if old_key == new_key {
        return Err(format!(
            "{from} is this folder's own state; there is nothing to re-link"
        ));
    }
    let lock = projects.join(RELINK_LOCK_FILE);
    crate::daemons::state::with_file_lock_in(&lock, &projects, |_| {
        relink_locked(
            &root,
            &projects.join(&old_key),
            &new_state,
            &old_key,
            &new_key,
        )
    })
}

fn relink_locked(
    root: &Path,
    old_state: &Path,
    new_state: &Path,
    old_key: &str,
    new_key: &str,
) -> Result<RelinkOutcome, String> {
    if !old_state.exists() {
        // The directory moved already; only the sessions may be left.
        return match read_journal(&new_state.join(RELINK_RECORD_FILE)) {
            Some(mut journal) if journal.from == old_key => finish(new_state, &mut journal, root),
            _ => Err(format!("no nib state named {old_key} exists")),
        };
    }
    let recorded = recorded_root_of(old_state).ok_or_else(|| {
        format!(
            "{} has no readable identity record; nothing was changed",
            old_state.display()
        )
    })?;
    if recorded != root && recorded.exists() {
        return Err(format!(
            "{} still exists, so this folder is a copy of it; nib keeps separate state for each folder and the original keeps its own. Nothing was changed.",
            recorded.display()
        ));
    }
    if let Some(reason) = live_activity(old_state) {
        return Err(format!("{reason}; nothing was changed"));
    }
    let mut journal =
        read_journal(&old_state.join(RELINK_RECORD_FILE)).unwrap_or_else(|| RelinkRecord {
            version: 1,
            from: old_key.to_string(),
            to: new_key.to_string(),
            root: root.to_path_buf(),
            started_at: chrono::Utc::now().to_rfc3339(),
            asides: Vec::new(),
            receipts: None,
            sessions_moved: false,
        });
    if journal.from != old_key || journal.to != new_key || journal.root != root {
        return Err(format!(
            "{} records another re-link ({} to {}); nothing was changed",
            old_state.join(RELINK_RECORD_FILE).display(),
            journal.from,
            journal.to
        ));
    }
    if new_state.exists() {
        if let Some(reason) = live_activity(new_state) {
            return Err(format!("{reason}; nothing was changed"));
        }
    }
    write_journal(old_state, &journal)?;

    if new_state.exists() {
        let key = format!(
            ".relinked-{new_key}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ")
        );
        let aside = new_state.with_file_name(&key);
        journal.asides.push(AsideRecord {
            key,
            path: aside.clone(),
            sessions_moved: false,
        });
        write_journal(old_state, &journal)?;
        rename_no_replace(new_state, &aside)
            .map_err(|error| move_error(new_state, &aside, &error))?;
    }
    move_aside_sessions(old_state, &mut journal)?;
    if journal.receipts.is_none() {
        if let Some(kept) = keep_stale_receipts(old_state, root)? {
            journal.receipts = Some(kept);
            write_journal(old_state, &journal)?;
        }
    }
    rewrite_root(&old_state.join(PROJECT_IDENTITY_FILE), root, true)?;
    rewrite_root(&old_state.join(STATE_RECORD_FILE), root, false)?;
    rename_no_replace(old_state, new_state)
        .map_err(|error| move_error(old_state, new_state, &error))?;
    sync_parent(new_state);
    finish(new_state, &mut journal, root)
}

/// Moves the sessions of every state kept aside to its aside key, before
/// any session of the old state moves to the new key.
fn move_aside_sessions(state: &Path, journal: &mut RelinkRecord) -> Result<(), String> {
    for index in 0..journal.asides.len() {
        if journal.asides[index].sessions_moved {
            continue;
        }
        let key = journal.asides[index].key.clone();
        relink_sessions(&journal.to, &key)?;
        journal.asides[index].sessions_moved = true;
        write_journal(state, journal)?;
    }
    Ok(())
}

/// The steps after the state directory moved: the sessions follow the key,
/// the journal goes, and Git refreshes the managed worktrees' links.
fn finish(
    new_state: &Path,
    journal: &mut RelinkRecord,
    root: &Path,
) -> Result<RelinkOutcome, String> {
    move_aside_sessions(new_state, journal)?;
    let sessions_moved = if journal.sessions_moved {
        0
    } else {
        let moved = relink_sessions(&journal.from, &journal.to)?;
        journal.sessions_moved = true;
        write_journal(new_state, journal)?;
        moved
    };
    remove_journal(new_state)?;
    Ok(RelinkOutcome {
        state_dir: new_state.to_path_buf(),
        sessions_moved,
        replaced: journal
            .asides
            .iter()
            .map(|aside| aside.path.clone())
            .collect(),
        receipts: journal.receipts.clone(),
        worktree_repair: repair_worktrees(root, new_state),
    })
}

/// Completes a re-link whose state directory already moved (the journal in
/// `state` names it); called when the profiles load.
pub(crate) fn finish_pending(state: &Path) -> Result<(), String> {
    let Some(journal) = read_journal(&state.join(RELINK_RECORD_FILE)) else {
        return Ok(());
    };
    if state.file_name().and_then(|name| name.to_str()) != Some(journal.to.as_str()) {
        // The directory has not moved yet; `nib doctor --fix --relink` resumes.
        return Ok(());
    }
    let Some(projects) = state.parent() else {
        return Ok(());
    };
    crate::daemons::state::with_file_lock_in(&projects.join(RELINK_LOCK_FILE), projects, |_| {
        match read_journal(&state.join(RELINK_RECORD_FILE)) {
            Some(mut journal) => {
                move_aside_sessions(state, &mut journal)?;
                if !journal.sessions_moved {
                    relink_sessions(&journal.from, &journal.to)?;
                }
                remove_journal(state)
            }
            None => Ok(()),
        }
    })
}

fn relink_sessions(from: &str, to: &str) -> Result<u64, String> {
    let Some(path) = NibPaths::session_database() else {
        return Ok(0);
    };
    if !path.is_file() {
        return Ok(0);
    }
    crate::session::database::SessionDatabase::open(&path)
        .and_then(|database| database.relink_project(from, to))
        .map_err(|error| error.to_string())
}

/// Keeps worktree receipts that name another folder in
/// `<state>/relinked-<time>/worktree-ownership`, so the moved state holds no
/// receipt for a worktree path it can no longer prove. Returns where.
fn keep_stale_receipts(state: &Path, root: &Path) -> Result<Option<PathBuf>, String> {
    let receipts = state.join("worktree-ownership");
    let Ok(entries) = fs::read_dir(&receipts) else {
        return Ok(None);
    };
    let stale = entries.filter_map(Result::ok).any(|entry| {
        read_table_json(&entry.path())
            .and_then(|receipt| receipt.get("project_root")?.as_str().map(PathBuf::from))
            .is_some_and(|recorded| recorded != root)
    });
    if !stale {
        return Ok(None);
    }
    let kept = state.join(format!(
        "relinked-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ")
    ));
    super::ensure_state_directory(&kept)
        .map_err(|error| format!("cannot create {}: {error}", kept.display()))?;
    let destination = kept.join("worktree-ownership");
    rename_no_replace(&receipts, &destination)
        .map_err(|error| move_error(&receipts, &destination, &error))?;
    Ok(Some(kept))
}

/// Asks Git to refresh the links of the managed worktrees, which a moved
/// project breaks. Returns a line for the report, if there were any.
fn repair_worktrees(root: &Path, state: &Path) -> Option<String> {
    let mut worktrees = Vec::new();
    for parent in [
        state.join("worktrees"),
        NibPaths::project_dir(root).join("worktrees"),
    ] {
        for kind in ["sessions", "subagents"] {
            if let Ok(entries) = fs::read_dir(parent.join(kind)) {
                worktrees.extend(
                    entries
                        .filter_map(Result::ok)
                        .filter(|entry| entry.path().join(".git").is_file())
                        .map(|entry| entry.path()),
                );
            }
        }
    }
    if worktrees.is_empty() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(root)
        .args(["worktree", "repair"])
        .args(&worktrees)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(std::process::Stdio::null())
        .output();
    Some(match output {
        Ok(output) if output.status.success() => format!(
            "git worktree repair refreshed {} managed worktrees",
            worktrees.len()
        ),
        Ok(output) => format!(
            "git worktree repair failed for {} managed worktrees: {}",
            worktrees.len(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("git worktree repair could not run: {error}"),
    })
}

/// The key of the state `from` names: a key in `projects`, or the folder an
/// identity record names.
fn resolve_from(projects: &Path, from: &str, root: &Path) -> Result<String, String> {
    if !from.contains(std::path::MAIN_SEPARATOR)
        && !from.contains('/')
        && projects.join(from).is_dir()
    {
        return Ok(from.to_string());
    }
    let wanted = crate::fs_security::absolute_path(Path::new(from))
        .map_err(|error| format!("{from} is not a usable path: {error}"))?;
    let mut keys = Vec::new();
    for state_dir in NibPaths::user_project_dirs() {
        let Some(key) = state_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if NibPaths::state_key(root).as_deref() == Some(key) {
            continue;
        }
        if recorded_root_of(&state_dir).is_some_and(|recorded| recorded == wanted) {
            keys.push(key.to_string());
        }
    }
    match keys.as_slice() {
        [key] => Ok(key.clone()),
        [] => Err(format!(
            "nib has no state for {from}; `nib doctor` lists the states this folder may belong to"
        )),
        _ => Err(format!(
            "several nib states name {from} ({}); name one by its key",
            keys.join(", ")
        )),
    }
}

/// Older nib files still in the project's `.nib`.
fn legacy_findings(root: &Path) -> Vec<String> {
    let legacy = NibPaths::project_dir(root);
    let Ok(metadata) = fs::symlink_metadata(&legacy) else {
        return Vec::new();
    };
    if !metadata.is_dir() {
        return vec![format!(
            "{} is not a directory; nib does not use it",
            legacy.display()
        )];
    }
    let mut findings = Vec::new();
    if NibPaths::has_legacy_state(root) {
        let entries: Vec<String> = crate::paths::LEGACY_STATE_ENTRIES
            .iter()
            .chain(std::iter::once(&crate::paths::LEGACY_PROFILE_SESSIONS))
            .filter(|entry| legacy.join(entry).exists())
            .map(|entry| (*entry).to_string())
            .collect();
        findings.push(if entries.is_empty() {
            format!(
                "{} still holds runtime state of an older nib (in profiles/)",
                legacy.display()
            )
        } else {
            format!(
                "{} still holds runtime state of an older nib: {}",
                legacy.display(),
                entries.join(", ")
            )
        });
    }
    if let Ok(entries) = fs::read_dir(&legacy) {
        let mut copies: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("legacy-"))
            .map(|entry| entry.path())
            .collect();
        copies.sort();
        for copy in copies {
            findings.push(format!(
                "{} keeps the originals of migrated state; remove it once you no longer need them",
                copy.display()
            ));
        }
    }
    let worktrees: usize = ["sessions", "subagents"]
        .iter()
        .filter_map(|kind| fs::read_dir(legacy.join("worktrees").join(kind)).ok())
        .map(|entries| entries.filter_map(Result::ok).count())
        .sum();
    if worktrees > 0 {
        findings.push(format!(
            "{} holds {worktrees} worktrees of an older nib; they stay manageable until they are removed",
            legacy.join("worktrees").display()
        ));
    }
    findings
}

fn session_counts() -> Vec<(String, String, u64)> {
    let Some(path) = NibPaths::session_database().filter(|path| path.is_file()) else {
        return Vec::new();
    };
    crate::session::database::SessionDatabase::open(&path)
        .and_then(|database| database.session_counts())
        .unwrap_or_default()
}

/// Whether two existing folders hold the same Git repository: they share a
/// root commit.
fn same_repository(first: &Path, second: &Path) -> bool {
    let roots = |path: &Path| -> Option<Vec<String>> {
        let output = std::process::Command::new("git")
            .args(["-c", "core.fsmonitor=false", "-C"])
            .arg(path)
            .args(["rev-list", "--max-parents=0", "HEAD"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        output.status.success().then(|| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
    };
    match (roots(first), roots(second)) {
        (Some(first), Some(second)) => first.iter().any(|commit| second.contains(commit)),
        _ => false,
    }
}

fn canonical_root(project_root: &Path) -> Result<PathBuf, String> {
    project_root
        .canonicalize()
        .map_err(|error| format!("the project folder cannot be resolved: {error}"))
}

/// The folder a state directory's identity record names, as written.
fn recorded_root_of(state: &Path) -> Option<PathBuf> {
    Some(PathBuf::from(
        read_table(&state.join(PROJECT_IDENTITY_FILE))?
            .get("root")?
            .as_str()?,
    ))
}

fn read_bounded(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_RECORD_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn read_table(path: &Path) -> Option<toml::Table> {
    read_bounded(path)?.parse().ok()
}

fn read_table_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&read_bounded(path)?).ok()
}

fn read_journal(path: &Path) -> Option<RelinkRecord> {
    toml::from_str(&read_bounded(path)?).ok()
}

fn write_journal(state: &Path, journal: &RelinkRecord) -> Result<(), String> {
    let text = toml::to_string_pretty(journal)
        .map_err(|error| format!("cannot encode the re-link record: {error}"))?;
    write_private_file(&state.join(RELINK_RECORD_FILE), text.as_bytes(), true)
}

fn remove_journal(state: &Path) -> Result<(), String> {
    let path = state.join(RELINK_RECORD_FILE);
    match fs::remove_file(&path) {
        Ok(()) => {
            sync_parent(&path);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
    }
}

/// Points an identity or state record at `root`. `required` records must
/// exist.
fn rewrite_root(path: &Path, root: &Path, required: bool) -> Result<(), String> {
    let Some(mut table) = read_table(path) else {
        if !required && !path.exists() {
            return Ok(());
        }
        return Err(format!(
            "{} is unreadable; nothing more was changed",
            path.display()
        ));
    };
    table.insert("root".to_string(), root.display().to_string().into());
    let text = toml::to_string_pretty(&table)
        .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
    write_private_file(path, text.as_bytes(), true)
}

fn move_error(source: &Path, destination: &Path, error: &io::Error) -> String {
    format!(
        "nib could not move {} to {}: {error}; run the re-link again to finish it",
        source.display(),
        destination.display()
    )
}

#[cfg(test)]
#[path = "state_relink_tests.rs"]
mod tests;
