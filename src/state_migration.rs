//! T082 phase 3: moves a project's runtime state out of `<project>/.nib`.
//!
//! Older versions kept memory, daemons, plans, subagent records, process
//! scopes, owner leases, worktree receipts and remembered command prefixes in
//! the project's own `.nib`, and each profile's memory, context, daemons and
//! managed skills in `.nib/profiles/<id>`. This module moves them to the
//! project's user-home state directory (`~/.nib/projects/<key>`). Sessions,
//! and what belongs to them, stay until the SQLite sessions of phase 4;
//! managed worktrees created by older versions stay where they are.
//!
//! The migration runs when a project's profiles are loaded and is:
//! - **lossless**: every entry is renamed, never copied, so its content and
//!   the file identities that subagent receipts and locks are bound to are
//!   kept. A destination that already exists is accepted only when it is an
//!   identical file (an interrupted earlier move); the legacy duplicate is
//!   then kept under `<project>/.nib/legacy-<time>/state/`. Anything else
//!   stops the migration before it changes anything;
//! - **deferred while the state is in use**: a live session heartbeat, a
//!   subagent owner lease or a recorded managed process leaves the state in
//!   place, and [`crate::paths::NibPaths::project_state`] keeps resolving to
//!   it, until a later run finds the project idle;
//! - **resumable and idempotent**: a state record written before the first
//!   move makes every later run resolve to the user home, and each run moves
//!   whatever legacy entries are still left;
//! - **serialized**: runs hold a file lock in the state directory.
//!
//! A user home on another file system than the project cannot take a rename;
//! the state then stays in the project and the migration says why.

use crate::paths::{
    NibPaths, LEGACY_PROFILE_STATE_ENTRIES, LEGACY_STATE_ENTRIES, PROJECT_IDENTITY_FILE,
    STATE_RECORD_FILE,
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
    /// Where legacy duplicates of already-moved entries were kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    kept: Vec<PathBuf>,
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
    record_identity(state, root)?;
    let legacy = NibPaths::project_dir(root);
    // A `.nib` that is a symbolic link may point at another project's state;
    // it is never moved (and never used, see `NibPaths::has_legacy_state`).
    let mut moves = if NibPaths::has_legacy_state(root) {
        legacy_moves(&legacy, state, keep)?
    } else {
        Vec::new()
    };
    let record_path = state.join(STATE_RECORD_FILE);
    let existing = read_record(&record_path)?;
    if moves.is_empty() {
        if existing.is_none() {
            write_record(&record_path, &new_record(root, "done"))?;
        }
        return Ok(StateMigration::Current);
    }
    if let Some(reason) = live_activity(&legacy) {
        return Ok(StateMigration::Deferred(reason));
    }
    let (duplicates, left) = classify_destinations(&moves, existing.is_some())?;
    moves.retain(|entry| !left.contains(&entry.relative));
    if moves.is_empty() {
        return Ok(StateMigration::Deferred(left_reason(&legacy, &left)));
    }
    if let Err(error) = probe_rename(&legacy, state) {
        return Ok(StateMigration::Deferred(format!(
            "nib's user home {} cannot take a rename from {} ({error}); the runtime state stays in the project",
            state.display(),
            legacy.display()
        )));
    }

    // From here on the project resolves to the user home, so no process
    // writes new legacy state while the entries move.
    let mut record = existing.unwrap_or_else(|| new_record(root, "moving"));
    record.status = "moving".to_string();
    record.completed_at = None;
    write_record(&record_path, &record)?;
    let backup = (!duplicates.is_empty()).then(|| legacy_backup_dir(&legacy));
    let mut moved = Vec::new();
    for entry in &moves {
        let relative = entry.relative.display().to_string();
        if duplicates.contains(&entry.relative) {
            let backup = backup.as_ref().expect("a backup for duplicates");
            let kept = backup.join(&entry.relative);
            create_parent(&kept, false)?;
            rename_no_replace(&entry.source, &kept)
                .map_err(|error| move_error(&entry.source, &kept, &error))?;
            if !record.kept.contains(backup) {
                record.kept.push(backup.clone());
            }
        } else {
            create_parent(&entry.destination, true)?;
            rename_no_replace(&entry.source, &entry.destination)
                .map_err(|error| move_error(&entry.source, &entry.destination, &error))?;
        }
        sync_parent(&entry.source);
        sync_parent(&entry.destination);
        if !record.moved.contains(&relative) {
            record.moved.push(relative.clone());
        }
        moved.push(relative);
    }
    for profile in profiles {
        write_moved_marker(&legacy, state, profile)?;
    }
    record.status = "done".to_string();
    record.completed_at = Some(chrono::Utc::now().to_rfc3339());
    write_record(&record_path, &record)?;
    if !left.is_empty() {
        tracing::warn!("{}", left_reason(&legacy, &left));
    }
    Ok(StateMigration::Moved(moved))
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
    let profiles = legacy.join("profiles");
    let entries = match fs::read_dir(&profiles) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(moves),
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
    for id in ids {
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
    let profiles = fs::read_dir(legacy.join("profiles")).ok()?;
    for profile in profiles.filter_map(Result::ok).take(MAX_PROFILE_DIRS) {
        let presence = profile.path().join(PRESENCE_ENTRY);
        if crate::session::presence::any_live_in(&presence) {
            return Some(format!(
                "a nib session is running in this project ({}); the runtime state moves once it has ended",
                presence.display()
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

/// `<project>/.nib/legacy-<time>/state`, for legacy duplicates.
fn legacy_backup_dir(legacy: &Path) -> PathBuf {
    legacy
        .join(format!(
            "{LEGACY_DIRECTORY_PREFIX}{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ")
        ))
        .join("state")
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

#[cfg(test)]
#[path = "state_migration_tests.rs"]
mod tests;
