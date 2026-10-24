//! T082 phase 4: imports a project's JSON sessions into the session database.
//!
//! Each legacy profile session directory (`<project>/.nib/profiles/<id>/sessions`)
//! is planned before anything changes: every `<id>.json` file is read
//! (bounded), parsed and validated, and compared with the database. A session
//! the database already holds differently stops the migration with a
//! diagnostic, unless the project's state already moved (then the directory is
//! left in place and reported). Files nib cannot read are kept, never dropped.
//!
//! The import itself writes one transaction per session, with an entry in the
//! database's import ledger, so an interrupted run resumes where it stopped.
//! Once every session of a directory is in the database, the whole directory
//! (the original JSON files, unreadable files, locks and its identity
//! marker) and its lock anchors are renamed into
//! `<project>/.nib/legacy-<time>/sessions/profiles/<id>/`, where they stay
//! until the user removes them.

use crate::session::database::{ImportOutcome, SessionDatabase, SessionScope};
use crate::session::Session;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Prefix of the run lease locks in a session directory.
const RUN_LEASE_PREFIX: &str = ".session-run-";

/// Largest session file read.
const MAX_SESSION_FILE_BYTES: u64 = crate::session::MAX_SESSION_JSON_BYTES;

/// Most entries read from one legacy session directory.
const MAX_SESSION_DIR_ENTRIES: usize = 100_000;

/// One legacy session directory and where its sessions go.
pub(super) struct SessionDirImport {
    /// `profiles/<id>/sessions`, relative to the project's `.nib`.
    pub(super) relative: PathBuf,
    pub(super) source: PathBuf,
    pub(super) scope: SessionScope,
    files: Vec<PlannedSession>,
    /// Session files whose id the database already holds differently.
    pub(super) conflicts: Vec<PathBuf>,
    /// Files that are not readable sessions; they are kept with the
    /// directory in the legacy copy.
    pub(super) unreadable: Vec<(PathBuf, String)>,
}

struct PlannedSession {
    path: PathBuf,
    sha256: String,
}

impl SessionDirImport {
    /// Readable sessions in this directory.
    pub(super) fn sessions(&self) -> usize {
        self.files.len()
    }
}

/// The legacy session directories of `legacy` (`<project>/.nib`), except
/// configured state directories (`keep`), with their sessions compared to
/// the database. `state_key` names the project's state directory.
pub(super) fn plan(
    legacy: &Path,
    state_key: &str,
    keep: &[PathBuf],
    profile_ids: &[std::ffi::OsString],
    database: &dyn Fn() -> Result<std::sync::Arc<SessionDatabase>, String>,
) -> Result<Vec<SessionDirImport>, String> {
    let mut imports = Vec::new();
    for id in profile_ids {
        let relative = Path::new("profiles")
            .join(id)
            .join(crate::paths::LEGACY_PROFILE_SESSIONS);
        let source = legacy.join(&relative);
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("cannot inspect {}: {error}", source.display())),
        };
        if keep
            .iter()
            .any(|kept| source.starts_with(kept) || kept.starts_with(&source))
        {
            continue;
        }
        if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(format!(
                "nib will not import sessions from {}: it is not a directory (a symbolic link?)",
                source.display()
            ));
        }
        let profile_id = id
            .to_str()
            .ok_or_else(|| format!("the profile folder {} is not UTF-8", source.display()))?;
        let scope = SessionScope::new(state_key, profile_id);
        imports.push(plan_directory(relative, source, scope, database)?);
    }
    Ok(imports)
}

fn plan_directory(
    relative: PathBuf,
    source: PathBuf,
    scope: SessionScope,
    database: &dyn Fn() -> Result<std::sync::Arc<SessionDatabase>, String>,
) -> Result<SessionDirImport, String> {
    let mut paths = Vec::new();
    let entries = fs::read_dir(&source)
        .map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    for entry in entries.take(MAX_SESSION_DIR_ENTRIES) {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", source.display()))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("json")
            && !entry.file_name().to_string_lossy().starts_with('.')
        {
            paths.push(path);
        }
    }
    paths.sort();
    let mut import = SessionDirImport {
        relative,
        source,
        scope,
        files: Vec::new(),
        conflicts: Vec::new(),
        unreadable: Vec::new(),
    };
    if paths.is_empty() {
        return Ok(import);
    }
    let database = database()?;
    for path in paths {
        let (session, sha256, _) = match read_session(&path) {
            Ok(read) => read,
            Err(reason) => {
                import.unreadable.push((path, reason));
                continue;
            }
        };
        let outcome = database
            .import_status(&import.scope, &session, &sha256)
            .map_err(|error| error.to_string())?;
        if outcome == ImportOutcome::Conflict {
            import.conflicts.push(path);
        } else {
            import.files.push(PlannedSession { path, sha256 });
        }
    }
    Ok(import)
}

/// Reads, parses and validates one session file: the session, the SHA-256
/// of its bytes and its modification time. The reason is returned when the
/// file is not a readable session.
fn read_session(path: &Path) -> Result<(Session, String, std::time::SystemTime), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("not a regular file".to_string());
    }
    if metadata.len() > MAX_SESSION_FILE_BYTES {
        return Err(format!(
            "larger than the {MAX_SESSION_FILE_BYTES}-byte session limit"
        ));
    }
    let file = open_no_follow(path).map_err(|error| error.to_string())?;
    let modified = file
        .metadata()
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|_| std::time::SystemTime::now());
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SESSION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_SESSION_FILE_BYTES {
        return Err(format!(
            "larger than the {MAX_SESSION_FILE_BYTES}-byte session limit"
        ));
    }
    let session: Session =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid JSON: {error}"))?;
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if session.id != stem {
        return Err(format!("the file holds session {}", session.id));
    }
    crate::session::validate_session_id(&session.id).map_err(|error| error.to_string())?;
    session.validate().map_err(|error| error.to_string())?;
    let sha256: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((session, sha256, modified))
}

fn open_no_follow(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// A run lease held on a session of `directory`, if any: a running session
/// of this or an older version. The probe only tries the lock of each
/// existing lease file; it creates nothing.
pub(super) fn held_run_lease(directory: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(directory).ok()?;
    for entry in entries.filter_map(Result::ok).take(MAX_SESSION_DIR_ENTRIES) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(RUN_LEASE_PREFIX) || !name.ends_with(".lock") {
            continue;
        }
        let path = entry.path();
        let Ok(file) = open_no_follow(&path) else {
            continue;
        };
        if matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock)) {
            return Some(path);
        }
    }
    None
}

/// Imports the planned sessions of `import`. Every file is read again and
/// must still be the one planned. Returns how many sessions were imported.
pub(super) fn import_sessions(
    import: &SessionDirImport,
    database: &SessionDatabase,
) -> Result<usize, String> {
    let mut imported = 0;
    for file in &import.files {
        let (session, sha256, modified) = read_session(&file.path).map_err(|reason| {
            format!(
                "{} changed while nib imported it ({reason}); the import resumes on the next start",
                file.path.display()
            )
        })?;
        if sha256 != file.sha256 {
            return Err(format!(
                "{} changed while nib imported it; the import resumes on the next start",
                file.path.display()
            ));
        }
        match database
            .import(&import.scope, &session, &file.path, &sha256, modified)
            .map_err(|error| error.to_string())?
        {
            ImportOutcome::Imported => imported += 1,
            ImportOutcome::AlreadyImported => {}
            ImportOutcome::Conflict => {
                return Err(format!(
                    "nib cannot import {}: the session database already holds a different session {}",
                    file.path.display(),
                    session.id
                ));
            }
        }
    }
    Ok(imported)
}

/// The lock anchors older versions kept beside a profile's session
/// directory (`.nib-lock-8-sessions-*.anchor`).
pub(super) fn session_lock_anchors(profile_dir: &Path) -> Vec<PathBuf> {
    let prefix = format!(
        ".nib-lock-{}-{}-",
        crate::paths::LEGACY_PROFILE_SESSIONS.len(),
        crate::paths::LEGACY_PROFILE_SESSIONS
    );
    let Ok(entries) = fs::read_dir(profile_dir) else {
        return Vec::new();
    };
    let mut anchors: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .take(MAX_SESSION_DIR_ENTRIES)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&prefix) && name.ends_with(".anchor")
        })
        .map(|entry| entry.path())
        .collect();
    anchors.sort();
    anchors
}
