//! T082 phases 3 and 4: moves a project's runtime state out of
//! `<project>/.nib`.
//!
//! Older versions kept memory, daemons, plans, subagent records, process
//! scopes, owner leases, worktree receipts and remembered command prefixes in
//! the project's own `.nib`, and each profile's memory, context, daemons,
//! managed skills, checkpoints, read ledgers, presence and sessions in
//! `.nib/profiles/<id>`. This module moves them to the project's user-home
//! state directory (`~/.nib/projects/<key>`) and imports the JSON sessions
//! into the session database (`~/.nib/nib.db`, see
//! [`sessions`]); the original session files are kept in
//! `<project>/.nib/legacy-<time>/sessions/`. Managed worktrees created by
//! older versions stay where they are.
//!
//! The migration runs when a project's profiles are loaded and is:
//! - **lossless**: every entry is renamed, never copied, so its content and
//!   the file identities that subagent receipts and locks are bound to are
//!   kept. A destination that already exists is accepted only when it is an
//!   identical file (an interrupted earlier move); the legacy duplicate is
//!   then kept under `<project>/.nib/legacy-<time>/state/`. Anything else
//!   stops the migration before it changes anything;
//! - **deferred while the state is in use**: a live session heartbeat, a
//!   held session run lease, a subagent owner lease or a recorded managed
//!   process leaves the state in place, and
//!   [`crate::paths::NibPaths::project_state`] keeps resolving to it, until a
//!   later run finds the project idle;
//! - **resumable and idempotent**: a state record written before the first
//!   move makes every later run resolve to the user home, and each run moves
//!   whatever legacy entries are still left. The record is written again
//!   before each step, so it names every entry a stopped run moved, and a
//!   later run that finds nothing left finishes it;
//! - **serialized**: runs hold a file lock in the state directory.
//!
//! A user home on another file system than the project cannot take a rename;
//! the state then stays in the project and the migration says why.

use crate::paths::{
    NibPaths, LEGACY_PROFILE_SESSIONS, LEGACY_PROFILE_STATE_ENTRIES, LEGACY_STATE_ENTRIES,
    PROJECT_IDENTITY_FILE, STATE_RECORD_FILE,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Version of the state record format.
const STATE_RECORD_VERSION: u32 = 1;

/// Lock that serializes migrations of one state directory.
const MIGRATION_LOCK_FILE: &str = ".state-migration.lock";

/// Prefix of the directory that keeps legacy duplicates, shared with the
/// configuration migration of phase 2.
const LEGACY_DIRECTORY_PREFIX: &str = "legacy-";

/// Probe file used to check that a rename reaches the user home.
const RENAME_PROBE_PREFIX: &str = ".nib-state-move-probe-";

/// Most profile directories migrated in one project.
const MAX_PROFILE_DIRS: usize = 256;

/// Largest legacy file compared byte for byte against an existing copy.
const MAX_COMPARED_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Largest state record read back.
const MAX_STATE_RECORD_BYTES: u64 = 256 * 1024;

/// Live entries that make the migration wait.
const OWNER_LEASES_ENTRY: &str = "subagent-owner-leases";
const PROCESS_SCOPES_ENTRY: &str = "process-scopes";
const PRESENCE_ENTRY: &str = "presence";

/// What a migration run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateMigration {
    /// The folder keeps its state in place: a managed worktree, or there is
    /// no user home.
    NotApplicable,
    /// Nothing was left to move.
    Current,
    /// These entries, relative to `<project>/.nib`, were moved.
    Moved(Vec<String>),
    /// The state stays in the project for now, for this reason.
    Deferred(String),
}

/// The record in a state directory (`state.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct StateRecord {
    version: u32,
    /// `moving` while a run moves entries, then `done`.
    status: String,
    /// The project folder the state belongs to.
    root: PathBuf,
    started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    /// Entries moved from the project, relative to its `.nib`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    moved: Vec<String>,
    /// Where legacy duplicates of already-moved entries and the original
    /// session files were kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    kept: Vec<PathBuf>,
    /// Session directories imported into the session database, with the
    /// number of sessions each held.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    imported: Vec<String>,
    /// Session files in the legacy copy that are not in the session
    /// database: files nib could not read, and files a writer of an older
    /// version changed after their import.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    not_imported: Vec<PathBuf>,
}

/// Version of the "state moved" marker. Older versions accept only
/// version 1 of the profile migration marker it replaces.
const MOVED_MARKER_VERSION: u32 = 2;

/// The "state moved" marker. It has the fields of the profile migration
/// marker of older versions, so they parse it and reject its version.
#[derive(Debug, Serialize, Deserialize)]
struct MovedMarker {
    version: u32,
    profile_id: String,
    session_count: usize,
    memory_copied: bool,
    state_moved_to: PathBuf,
    note: String,
}

/// One legacy entry and where it goes.
struct Move {
    relative: PathBuf,
    source: PathBuf,
    destination: PathBuf,
}

/// Creates a state directory. Directories created inside the user home are
/// private (`0700`), from the user home down; elsewhere the directory is
/// created as before. Returns its canonical path.
pub(crate) fn ensure_state_directory(directory: &Path) -> io::Result<PathBuf> {
    if let Some(home) = user_home_containing(directory) {
        let mut current = home.clone();
        ensure_private_directory(&current)?;
        if let Ok(relative) = directory.strip_prefix(&home) {
            for component in relative.components() {
                current.push(component);
                ensure_private_directory(&current)?;
            }
        }
    }
    crate::fs_security::ensure_directory_without_symlinks(directory)
}

/// The parent a project's state directory is opened from, so the state
/// directory itself is a direct child of a retained capability: the project
/// for legacy state, `<user home>/projects` (created privately) otherwise.
pub(crate) fn ensure_state_parent(state: &Path) -> io::Result<PathBuf> {
    let parent = state.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("nib state directory has no parent: {}", state.display()),
        )
    })?;
    if user_home_containing(parent).is_some() {
        ensure_state_directory(parent)?;
    }
    Ok(parent.to_path_buf())
}

/// The user home, as `directory` spells it, when `directory` is inside it.
pub(crate) fn user_home_containing(directory: &Path) -> Option<PathBuf> {
    let home = NibPaths::state_home()?;
    let canonical = home.canonicalize().ok();
    [Some(home), canonical]
        .into_iter()
        .flatten()
        .find(|home| directory.starts_with(home))
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => crate::fs_security::verify_directory_without_symlinks(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            crate::fs_security::ensure_directory_without_symlinks(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Moves the legacy runtime state of the folder at `project_root` to its
/// user-home state directory. `keep` lists configured profile state
/// directories, which stay where they are configured; `profiles` names the
/// profiles whose default state lives here, which get a "state moved"
/// marker for older versions.
pub fn migrate_project_state(
    project_root: &Path,
    keep: &[PathBuf],
    profiles: &[String],
) -> Result<StateMigration, String> {
    let root = project_root
        .canonicalize()
        .map_err(|error| format!("the project folder cannot be resolved: {error}"))?;
    if NibPaths::is_managed_worktree(&root) {
        return Ok(StateMigration::NotApplicable);
    }
    let Some(state) = NibPaths::user_state_dir(&root) else {
        return Ok(StateMigration::NotApplicable);
    };
    let state = ensure_state_directory(&state).map_err(|error| {
        format!(
            "nib's state directory {} is unusable: {error}",
            state.display()
        )
    })?;
    let lock = state.join(MIGRATION_LOCK_FILE);
    crate::daemons::state::with_file_lock_in(&lock, &state, |_| {
        migrate_locked(&root, &state, keep, profiles)
    })
}

fn migrate_locked(
    root: &Path,
    state: &Path,
    keep: &[PathBuf],
    profiles: &[String],
) -> Result<StateMigration, String> {
    relink::finish_pending(state)?;
    record_identity(state, root)?;
    let legacy = NibPaths::project_dir(root);
    // A `.nib` that is a symbolic link may point at another project's state;
    // it is never moved (and never used, see `NibPaths::has_legacy_state`).
    let has_legacy = NibPaths::has_legacy_state(root);
    let mut moves = if has_legacy {
        legacy_moves(&legacy, state, keep)?
    } else {
        Vec::new()
    };
    let mut imports = if has_legacy {
        plan_imports(&legacy, state, keep)?
    } else {
        Vec::new()
    };
    let record_path = state.join(STATE_RECORD_FILE);
    let existing = read_record(&record_path)?;
    let pre_profile = if has_legacy {
        retired_pre_profile_sessions(&legacy, state, profiles)
    } else {
        None
    };
    if moves.is_empty() && imports.is_empty() && pre_profile.is_none() {
        finish_current(root, state, profiles, existing)?;
        return Ok(StateMigration::Current);
    }
    if let Some(reason) = live_activity(&legacy) {
        return Ok(StateMigration::Deferred(reason));
    }
    let (duplicates, mut left) = classify_destinations(&moves, existing.is_some())?;
    moves.retain(|entry| !left.contains(&entry.relative));
    let conflicts: Vec<String> = imports
        .iter()
        .flat_map(|import| &import.conflicts)
        .map(|path| path.display().to_string())
        .collect();
    if !conflicts.is_empty() {
        if existing.is_none() {
            return Err(format!(
                "nib cannot import {}: the session database already holds different sessions with these ids; nothing was moved. Compare them and remove the one you do not need.",
                conflicts.join(", ")
            ));
        }
        for import in imports.iter().filter(|import| !import.conflicts.is_empty()) {
            left.push(import.relative.clone());
        }
        imports.retain(|import| import.conflicts.is_empty());
    }
    if moves.is_empty() && imports.is_empty() && pre_profile.is_none() {
        return Ok(StateMigration::Deferred(left_reason(&legacy, &left)));
    }
    if !moves.is_empty() {
        if let Err(error) = probe_rename(&legacy, state) {
            return Ok(StateMigration::Deferred(format!(
                "nib's user home {} cannot take a rename from {} ({error}); the runtime state stays in the project",
                state.display(),
                legacy.display()
            )));
        }
    }

    // From here on the project resolves to the user home, so no process
    // writes new legacy state while the entries move.
    let mut record = existing.unwrap_or_else(|| new_record(root, "moving"));
    record.status = "moving".to_string();
    record.completed_at = None;
    write_record(&record_path, &record)?;
    let backup = legacy_backup_dir(&legacy);
    let mut moved = move_entries(&moves, &duplicates, &backup, &mut record, &record_path)?;
    if !imports.is_empty() {
        let database = open_session_database()?;
        for import in &imports {
            moved.push(import_directory(
                import,
                &database,
                &backup,
                &mut record,
                &record_path,
            )?);
        }
    }
    if let Some(sessions) = &pre_profile {
        moved.push(retire_directory(
            sessions,
            &backup,
            &mut record,
            &record_path,
        )?);
    }
    for profile in profiles {
        write_moved_marker(&legacy, state, profile)?;
    }
    finish_record(&record_path, &mut record)?;
    if !left.is_empty() {
        tracing::warn!("{}", left_reason(&legacy, &left));
    }
    Ok(StateMigration::Moved(moved))
}

fn open_session_database(
) -> Result<std::sync::Arc<crate::session::database::SessionDatabase>, String> {
    crate::session::database::SessionDatabase::for_user_home().map_err(|error| error.to_string())
}

/// The legacy session directories of `legacy` and their sessions, compared
/// with the session database.
fn plan_imports(
    legacy: &Path,
    state: &Path,
    keep: &[PathBuf],
) -> Result<Vec<sessions::SessionDirImport>, String> {
    let state_key = state
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("nib's state directory {} has no key", state.display()))?;
    sessions::plan(
        legacy,
        state_key,
        keep,
        &legacy_profile_ids(legacy)?,
        &open_session_database,
    )
}

/// Records a folder with nothing left to move: a new record, or the end of
/// a run that stopped after its last step, whose markers and status are
/// left to write.
fn finish_current(
    root: &Path,
    state: &Path,
    profiles: &[String],
    existing: Option<StateRecord>,
) -> Result<(), String> {
    let record_path = state.join(STATE_RECORD_FILE);
    match existing {
        None => write_record(&record_path, &new_record(root, "done")),
        Some(mut record) if record.status != "done" => {
            let legacy = NibPaths::project_dir(root);
            if fs::symlink_metadata(&legacy).is_ok_and(|metadata| metadata.is_dir()) {
                for profile in profiles {
                    write_moved_marker(&legacy, state, profile)?;
                }
            }
            finish_record(&record_path, &mut record)
        }
        Some(_) => Ok(()),
    }
}

fn finish_record(path: &Path, record: &mut StateRecord) -> Result<(), String> {
    record.status = "done".to_string();
    record.completed_at = Some(chrono::Utc::now().to_rfc3339());
    write_record(path, record)
}

/// The pre-profile session directory (`<project>/.nib/sessions`) once the
/// profile migration imported it: a profile whose default state lives here
/// has its profile migration marker, in the user home or still in the
/// project. Older versions copied these sessions into the default profile;
/// this version imports them into the session database.
fn retired_pre_profile_sessions(
    legacy: &Path,
    state: &Path,
    profiles: &[String],
) -> Option<PathBuf> {
    let sessions = legacy.join(LEGACY_PROFILE_SESSIONS);
    let directory = fs::symlink_metadata(&sessions).is_ok_and(|metadata| {
        metadata.is_dir() && !crate::fs_security::metadata_is_link_or_reparse(&metadata)
    });
    let imported = profiles.iter().any(|profile| {
        [state, legacy].iter().any(|base| {
            let marker = base
                .join("profiles")
                .join(profile)
                .join(LEGACY_PROFILE_STATE_ENTRIES[0]);
            fs::symlink_metadata(&marker).is_ok_and(|metadata| metadata.is_file())
                && !is_moved_marker(&marker)
        })
    });
    (directory && imported).then_some(sessions)
}

/// Keeps the imported pre-profile session directory and its lock anchors
/// under `<backup>/sessions/`, so the project holds no legacy state.
fn retire_directory(
    sessions: &Path,
    backup: &Path,
    record: &mut StateRecord,
    record_path: &Path,
) -> Result<String, String> {
    let kept_root = backup.join("sessions");
    let kept = kept_root.join(LEGACY_PROFILE_SESSIONS);
    create_parent(&kept, false)?;
    let anchors = sessions
        .parent()
        .map(sessions::session_lock_anchors)
        .unwrap_or_default();
    if !record.kept.contains(&kept_root) {
        record.kept.push(kept_root);
    }
    write_record(record_path, record)?;
    rename_no_replace(sessions, &kept).map_err(|error| move_error(sessions, &kept, &error))?;
    sync_parent(sessions);
    move_anchors(&anchors, &kept)?;
    Ok(format!(
        "{LEGACY_PROFILE_SESSIONS} (imported earlier, kept in {})",
        kept.display()
    ))
}

/// Renames each entry to the user home, or, for an identical duplicate the
/// user home already holds, into `<backup>/state/`. Returns the moved
/// entries.
fn move_entries(
    moves: &[Move],
    duplicates: &[PathBuf],
    backup: &Path,
    record: &mut StateRecord,
    record_path: &Path,
) -> Result<Vec<String>, String> {
    let mut moved = Vec::new();
    for entry in moves {
        let relative = entry.relative.display().to_string();
        // Recorded before the rename, so a stopped run still names it.
        if !record.moved.contains(&relative) {
            record.moved.push(relative.clone());
        }
        if duplicates.contains(&entry.relative) {
            let kept_root = backup.join("state");
            let kept = kept_root.join(&entry.relative);
            create_parent(&kept, false)?;
            if !record.kept.contains(&kept_root) {
                record.kept.push(kept_root);
            }
            write_record(record_path, record)?;
            rename_no_replace(&entry.source, &kept)
                .map_err(|error| move_error(&entry.source, &kept, &error))?;
        } else {
            create_parent(&entry.destination, true)?;
            write_record(record_path, record)?;
            rename_no_replace(&entry.source, &entry.destination)
                .map_err(|error| move_error(&entry.source, &entry.destination, &error))?;
        }
        sync_parent(&entry.source);
        sync_parent(&entry.destination);
        moved.push(relative);
    }
    Ok(moved)
}

/// Imports the sessions of one legacy session directory, then keeps the
/// directory (the original files, locks and identity marker) and its lock
/// anchors under `<backup>/sessions/`. The import is recorded before the
/// rename, so the directory's recorded path reaches the session database
/// even when the run stops right after it. Returns what it reports as moved.
fn import_directory(
    import: &sessions::SessionDirImport,
    database: &crate::session::database::SessionDatabase,
    backup: &Path,
    record: &mut StateRecord,
    record_path: &Path,
) -> Result<String, String> {
    let imported = sessions::import_sessions(import, database)?;
    #[cfg(test)]
    if let Some(hook) = tests::AFTER_IMPORT.take() {
        hook(&import.source);
    }
    for (path, reason) in &import.unreadable {
        tracing::warn!(
            "nib could not import {} ({reason}); it is kept with the original sessions",
            path.display()
        );
    }
    let kept_root = backup.join("sessions");
    let kept = kept_root.join(&import.relative);
    create_parent(&kept, false)?;
    let anchors = import
        .source
        .parent()
        .map(sessions::session_lock_anchors)
        .unwrap_or_default();
    let relative = import.relative.display().to_string();
    let prefix = format!("{relative}:");
    record.imported.retain(|entry| !entry.starts_with(&prefix));
    record.imported.push(format!(
        "{relative}: {} sessions in the session database ({imported} imported now), {} unreadable files kept",
        import.sessions(),
        import.unreadable.len()
    ));
    if !record.kept.contains(&kept_root) {
        record.kept.push(kept_root);
    }
    for (path, _) in &import.unreadable {
        if let Some(name) = path.file_name() {
            add_not_imported(record, kept.join(name));
        }
    }
    write_record(record_path, record)?;
    rename_no_replace(&import.source, &kept)
        .map_err(|error| move_error(&import.source, &kept, &error))?;
    sync_parent(&import.source);
    sync_parent(&kept);
    move_anchors(&anchors, &kept)?;
    #[cfg(test)]
    if tests::STOP_AFTER_IMPORT_RENAME.take() {
        return Err("stopped after the rename (test)".to_string());
    }
    // A writer that opened the directory before the migration may have
    // saved a session between its import and the rename; that save is only
    // in the legacy copy, so it is reported, never dropped silently.
    let changed = sessions::changed_after_import(import, &kept);
    for path in &changed {
        tracing::warn!(
            "{} changed after nib imported it; the change is only in this legacy copy",
            path.display()
        );
        add_not_imported(record, path.clone());
    }
    if !changed.is_empty() {
        write_record(record_path, record)?;
    }
    Ok(format!(
        "{relative} ({} sessions imported)",
        import.sessions()
    ))
}

fn add_not_imported(record: &mut StateRecord, path: PathBuf) {
    if !record.not_imported.contains(&path) {
        record.not_imported.push(path);
    }
}

/// Moves the lock anchors of a session directory next to its legacy copy
/// `kept`.
fn move_anchors(anchors: &[PathBuf], kept: &Path) -> Result<(), String> {
    let Some(kept_parent) = kept.parent() else {
        return Ok(());
    };
    for anchor in anchors {
        if let Some(name) = anchor.file_name() {
            let destination = kept_parent.join(name);
            rename_no_replace(anchor, &destination)
                .map_err(|error| move_error(anchor, &destination, &error))?;
        }
    }
    if !anchors.is_empty() {
        sync_parent(kept);
    }
    Ok(())
}

/// Where the sessions of a legacy profile session directory
/// (`<project>/.nib/profiles/<id>/sessions`) went, once the migration imported
/// them and kept the folder as a legacy copy: the profile's session directory
/// in the user home. Records written before the import (a scheduled task's
/// session directory, for one) then reach the session database instead of
/// starting a second, empty store in the project.
pub(crate) fn moved_sessions_dir(sessions_dir: &Path) -> Option<PathBuf> {
    if fs::symlink_metadata(sessions_dir).is_ok()
        || sessions_dir.file_name()? != LEGACY_PROFILE_SESSIONS
    {
        return None;
    }
    let profile = sessions_dir.parent()?;
    let profiles = profile.parent()?;
    let legacy = profiles.parent()?;
    if profiles.file_name()? != "profiles" || !NibPaths::is_project_dir_name(legacy.file_name()?) {
        return None;
    }
    let state = NibPaths::user_state_dir(legacy.parent()?)?;
    let record = read_record(&state.join(STATE_RECORD_FILE)).ok()??;
    let relative = Path::new("profiles")
        .join(profile.file_name()?)
        .join(LEGACY_PROFILE_SESSIONS);
    let prefix = format!("{}:", relative.display());
    record
        .imported
        .iter()
        .any(|entry| entry.starts_with(&prefix))
        .then(|| state.join(relative))
}

/// The user-home path of a legacy state entry that was renamed in place, when
/// `path` no longer exists and `state.toml` records the move as done.
///
/// The returned directory is the same inode the holder already has open.
/// Session directories are imported, not renamed, so they are not returned
/// here; [`moved_sessions_dir`] is their record.
pub(crate) fn migrated_counterpart(path: &Path) -> Option<PathBuf> {
    if fs::symlink_metadata(path).is_ok() {
        return None;
    }
    let (legacy, relative) = legacy_state_relative(path)?;
    let project = legacy.parent()?;
    if NibPaths::is_managed_worktree(project) {
        return None;
    }
    let state = NibPaths::user_state_dir(project)?;
    if state == legacy {
        return None;
    }
    let record = read_record(&state.join(STATE_RECORD_FILE)).ok()??;
    if record.status != "done" || !moved_covers(&record.moved, relative) {
        return None;
    }
    let candidate = state.join(relative);
    (candidate != path).then_some(candidate)
}

/// `<project>/.nib` and the path of `path` relative to it.
fn legacy_state_relative(path: &Path) -> Option<(&Path, &Path)> {
    let mut cursor = path;
    loop {
        if NibPaths::is_project_dir_name(cursor.file_name()?) {
            let relative = path.strip_prefix(cursor).ok()?;
            if relative.as_os_str().is_empty() {
                return None;
            }
            return Some((cursor, relative));
        }
        cursor = cursor.parent()?;
    }
}

/// Whether `relative` is a moved entry or a file inside one.
fn moved_covers(moved: &[String], relative: &Path) -> bool {
    let relative = relative.to_string_lossy();
    moved.iter().any(|entry| {
        relative == entry.as_str()
            || relative
                .strip_prefix(entry.as_str())
                .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
    })
}

/// The profile folders of a legacy `.nib`, sorted.
fn legacy_profile_ids(legacy: &Path) -> Result<Vec<std::ffi::OsString>, String> {
    let profiles = legacy.join("profiles");
    let entries = match fs::read_dir(&profiles) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot read {}: {error}", profiles.display())),
    };
    let mut ids = Vec::new();
    for entry in entries.take(MAX_PROFILE_DIRS) {
        let entry =
            entry.map_err(|error| format!("cannot read {}: {error}", profiles.display()))?;
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            ids.push(entry.file_name());
        }
    }
    ids.sort();
    Ok(ids)
}

/// Splits the entries whose destination already exists into identical
/// files, whose legacy copy is kept aside, and entries left in place. Before
/// the first move a differing destination stops the migration: nothing has
/// been decided yet, so the user chooses. Once the project's state lives in
/// the user home, legacy entries that reappeared (an older version wrote
/// them) and conflict are left in place and reported instead.
fn classify_destinations(
    moves: &[Move],
    recorded: bool,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut duplicates = Vec::new();
    let mut left = Vec::new();
    for entry in moves {
        if !exists(&entry.destination)? {
            continue;
        }
        if identical_files(&entry.source, &entry.destination)? {
            duplicates.push(entry.relative.clone());
        } else if recorded {
            left.push(entry.relative.clone());
        } else {
            return Err(format!(
                "nib cannot move {} to {}: the destination already exists and differs; nothing was moved. Compare the two and remove the one you do not need.",
                entry.source.display(),
                entry.destination.display()
            ));
        }
    }
    Ok((duplicates, left))
}

fn left_reason(legacy: &Path, left: &[PathBuf]) -> String {
    let names: Vec<String> = left.iter().map(|path| path.display().to_string()).collect();
    format!(
        "{} still holds {} after the state moved to nib's user home; they differ from the moved state and were left in place",
        legacy.display(),
        names.join(", ")
    )
}

/// Leaves a "state moved" marker where an older version looks for its
/// profile migration marker (`.nib/profiles/<id>/.legacy-state-migration-v1.json`).
/// Older versions reject its version and stop before they touch any state,
/// so they cannot start a second copy of it in the project. This version
/// never reads it.
fn write_moved_marker(legacy: &Path, state: &Path, profile: &str) -> Result<(), String> {
    let directory = legacy.join("profiles").join(profile);
    let path = directory.join(LEGACY_PROFILE_STATE_ENTRIES[0]);
    if is_moved_marker(&path) {
        return Ok(());
    }
    crate::fs_security::ensure_directory_without_symlinks(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let marker = MovedMarker {
        version: MOVED_MARKER_VERSION,
        profile_id: profile.to_string(),
        session_count: 0,
        memory_copied: false,
        state_moved_to: state.to_path_buf(),
        note: "nib keeps this project's runtime state in its user home; use nib 0.6 or later"
            .to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
    write_private_file(&path, &bytes, false)
}

/// Whether `path` is a "state moved" marker.
pub(crate) fn is_moved_marker(path: &Path) -> bool {
    let regular = fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_file() && metadata.len() <= MAX_STATE_RECORD_BYTES
    });
    regular
        && fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<MovedMarker>(&bytes).ok())
            .is_some_and(|marker| marker.version == MOVED_MARKER_VERSION)
}

/// The legacy entries still in `legacy` and where each one goes.
fn legacy_moves(legacy: &Path, state: &Path, keep: &[PathBuf]) -> Result<Vec<Move>, String> {
    let kept = |path: &Path| {
        keep.iter()
            .any(|kept| path.starts_with(kept) || kept.starts_with(path))
    };
    let mut moves = Vec::new();
    let mut add = |relative: PathBuf| -> Result<(), String> {
        let source = legacy.join(&relative);
        if !exists(&source)? || kept(&source) || is_moved_marker(&source) {
            return Ok(());
        }
        let metadata = fs::symlink_metadata(&source)
            .map_err(|error| format!("cannot inspect {}: {error}", source.display()))?;
        if crate::fs_security::metadata_is_link_or_reparse(&metadata) {
            return Err(format!(
                "nib will not move the symbolic link {}; replace it with the directory or file it points to",
                source.display()
            ));
        }
        moves.push(Move {
            destination: state.join(&relative),
            source,
            relative,
        });
        Ok(())
    };
    for entry in LEGACY_STATE_ENTRIES {
        add(PathBuf::from(entry))?;
    }
    for id in legacy_profile_ids(legacy)? {
        for entry in LEGACY_PROFILE_STATE_ENTRIES {
            add(Path::new("profiles").join(&id).join(entry))?;
        }
    }
    Ok(moves)
}

/// Why the legacy state is in use, if it is: a live session heartbeat, a
/// subagent owner lease or a recorded managed process.
fn live_activity(legacy: &Path) -> Option<String> {
    for (entry, what) in [
        (OWNER_LEASES_ENTRY, "a nib process owns running subagents"),
        (PROCESS_SCOPES_ENTRY, "nib still tracks managed processes"),
    ] {
        let has_entries =
            fs::read_dir(legacy.join(entry)).is_ok_and(|mut entries| entries.next().is_some());
        if has_entries {
            return Some(format!(
                "{what} ({}); the runtime state moves once they have finished",
                legacy.join(entry).display()
            ));
        }
    }
    let pre_profile = legacy.join(LEGACY_PROFILE_SESSIONS);
    if let Some(lease) = sessions::held_run_lease(&pre_profile) {
        return Some(format!(
            "a nib session run is active in this project ({}); the runtime state moves once it has ended",
            lease.display()
        ));
    }
    let profiles = fs::read_dir(legacy.join("profiles")).ok()?;
    for profile in profiles.filter_map(Result::ok).take(MAX_PROFILE_DIRS) {
        let presence = profile.path().join(PRESENCE_ENTRY);
        if crate::session::presence::any_live_in(&presence) {
            return Some(format!(
                "a nib session is running in this project ({}); the runtime state moves once it has ended",
                presence.display()
            ));
        }
        let sessions = profile.path().join(LEGACY_PROFILE_SESSIONS);
        if let Some(lease) = sessions::held_run_lease(&sessions) {
            return Some(format!(
                "a nib session run is active in this project ({}); the runtime state moves once it has ended",
                lease.display()
            ));
        }
    }
    None
}

/// Checks that an entry renamed from the project reaches the user home, so a
/// migration never stops halfway on a cross-device move.
fn probe_rename(legacy: &Path, state: &Path) -> io::Result<()> {
    let name = format!("{RENAME_PROBE_PREFIX}{}", uuid::Uuid::new_v4().simple());
    let source = legacy.join(&name);
    let destination = state.join(&name);
    fs::File::create_new(&source)?;
    let result = rename_no_replace(&source, &destination);
    let _ = fs::remove_file(&source);
    let _ = fs::remove_file(&destination);
    result
}

/// `<project>/.nib/legacy-<time>`: legacy duplicates go to its `state/`,
/// the original session files to its `sessions/`.
fn legacy_backup_dir(legacy: &Path) -> PathBuf {
    legacy.join(format!(
        "{LEGACY_DIRECTORY_PREFIX}{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ")
    ))
}

fn create_parent(path: &Path, private: bool) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let created = if private {
        ensure_state_directory(parent).map(|_| ())
    } else {
        crate::fs_security::ensure_directory_without_symlinks(parent).map(|_| ())
    };
    created.map_err(|error| format!("cannot create {}: {error}", parent.display()))
}

fn move_error(source: &Path, destination: &Path, error: &io::Error) -> String {
    format!(
        "nib could not move {} to {}: {error}; the entries moved so far are recorded and the next start resumes",
        source.display(),
        destination.display()
    )
}

/// Renames `source` to `destination`, failing if `destination` exists.
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
        // SAFETY: both paths are valid NUL-terminated strings for the call.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
            return Err(error);
        }
    }
    // Without an atomic no-replace rename the check runs under the migration
    // lock, which every nib process of this version takes first.
    if fs::symlink_metadata(destination).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the destination already exists",
        ));
    }
    fs::rename(source, destination)
}

fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

/// Whether both paths are regular files with the same bytes.
fn identical_files(first: &Path, second: &Path) -> Result<bool, String> {
    let regular = |path: &Path| {
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.file_type().is_file() && metadata.len() <= MAX_COMPARED_FILE_BYTES
        })
    };
    if !regular(first) || !regular(second) {
        return Ok(false);
    }
    let read = |path: &Path| {
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
    };
    Ok(read(first)? == read(second)?)
}

fn new_record(root: &Path, status: &str) -> StateRecord {
    StateRecord {
        version: STATE_RECORD_VERSION,
        status: status.to_string(),
        root: root.to_path_buf(),
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: (status == "done").then(|| chrono::Utc::now().to_rfc3339()),
        moved: Vec::new(),
        kept: Vec::new(),
        imported: Vec::new(),
        not_imported: Vec::new(),
    }
}

fn read_record(path: &Path) -> Result<Option<StateRecord>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_STATE_RECORD_BYTES {
        return Err(format!(
            "nib's state record {} is not a regular file of at most {MAX_STATE_RECORD_BYTES} bytes",
            path.display()
        ));
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let record: StateRecord = toml::from_str(&text)
        .map_err(|error| format!("nib's state record {} is invalid: {error}", path.display()))?;
    if record.version != STATE_RECORD_VERSION {
        return Err(format!(
            "nib's state record {} has unsupported version {}",
            path.display(),
            record.version
        ));
    }
    Ok(Some(record))
}

/// Writes the record atomically, owner-only.
fn write_record(path: &Path, record: &StateRecord) -> Result<(), String> {
    let text = toml::to_string_pretty(record)
        .map_err(|error| format!("cannot encode nib's state record: {error}"))?;
    write_private_file(path, text.as_bytes(), true)
}

/// Creates the user-home state directory of the folder at `project_root`
/// and records which folder it belongs to, so the worktrees nib creates in
/// it can be traced back to their project. Folders whose state stays in
/// their own `.nib` need no record.
pub(crate) fn ensure_state_identity(project_root: &Path) -> Result<(), String> {
    let root = project_root
        .canonicalize()
        .map_err(|error| format!("the project folder cannot be resolved: {error}"))?;
    let state = NibPaths::project_state(&root);
    if state.starts_with(NibPaths::project_dir(&root)) {
        return Ok(());
    }
    let state = ensure_state_directory(&state).map_err(|error| {
        format!(
            "nib's state directory {} is unusable: {error}",
            state.display()
        )
    })?;
    record_identity(&state, &root)
}

/// Records which folder the state directory belongs to, as the settings
/// directory of phase 2 does, unless the file exists.
fn record_identity(state: &Path, root: &Path) -> Result<(), String> {
    let path = state.join(PROJECT_IDENTITY_FILE);
    if exists(&path)? {
        return Ok(());
    }
    let mut identity = toml::Table::new();
    identity.insert("version".to_string(), 1.into());
    identity.insert("root".to_string(), root.display().to_string().into());
    let text = toml::to_string_pretty(&identity)
        .map_err(|error| format!("cannot encode {}: {error}", path.display()))?;
    write_private_file(&path, text.as_bytes(), false)
}

/// Writes `bytes` to `path` through a private temporary file; `replace`
/// selects whether an existing file is replaced or kept.
fn write_private_file(path: &Path, bytes: &[u8], replace: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent", path.display()))?;
    let write = || -> io::Result<()> {
        let mut file = tempfile::Builder::new()
            .prefix(".nib-state-")
            .tempfile_in(parent)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        if replace {
            file.persist(path).map_err(|error| error.error)?;
        } else {
            match file.persist_noclobber(path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.error),
            }
        }
        Ok(())
    };
    write().map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    sync_parent(path);
    Ok(())
}

fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let _ = fs::File::open(parent).and_then(|directory| directory.sync_all());
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[path = "state_migration_sessions.rs"]
mod sessions;

#[path = "state_relink.rs"]
pub mod relink;

#[cfg(test)]
#[path = "state_migration_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "store_relocation_tests.rs"]
mod store_relocation_tests;
