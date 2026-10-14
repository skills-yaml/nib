//! Stale-write check (T080 phase 5a, section 6).
//!
//! `read_file` records, per session, a fingerprint of every file it returns.
//! `apply_patch` then changes a file only when this session read it and the
//! file still matches what was read, so concurrent edits by another session,
//! the user or a formatter are never silently overwritten. A successful patch
//! records the new contents, so consecutive edits need no re-read.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

/// The most files a session remembers; the least recently read are dropped.
const MAX_ENTRIES: usize = 10_000;
const LEDGER_DIR: &str = "file_reads";
/// Larger files are fingerprinted from metadata instead of hashed.
const MAX_HASHED_BYTES: u64 = 16 * 1024 * 1024;

/// Serializes ledger updates within this process; sessions do not share a
/// ledger file, so no cross-process lock is needed.
static LEDGER_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    next_sequence: u64,
    files: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    fingerprint: String,
    sequence: u64,
}

/// The fingerprints of the files one session has read or written.
pub(crate) struct ReadLedger {
    path: PathBuf,
}

/// A workspace file named both on disk and by its ledger key: the path from
/// the workspace top with `/` separators, so reads and edits from different
/// scopes, or from the project and its session worktree, share entries.
pub(crate) struct TrackedPath {
    pub(crate) key: String,
    pub(crate) absolute: PathBuf,
}

impl ReadLedger {
    pub(crate) fn for_session(sessions_dir: &Path, session_id: &str) -> Result<Self, String> {
        if session_id.is_empty()
            || !session_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(format!("invalid session id for file reads: {session_id:?}"));
        }
        let state = sessions_dir
            .parent()
            .ok_or("the session store has no profile state directory")?;
        Ok(Self {
            path: state.join(LEDGER_DIR).join(format!("{session_id}.json")),
        })
    }

    /// Records what this session saw. `None` forgets the file.
    pub(crate) fn record(&self, seen: &[(String, Option<String>)]) -> Result<(), String> {
        if seen.is_empty() {
            return Ok(());
        }
        let _guard = LEDGER_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut ledger = self.load()?;
        for (key, fingerprint) in seen {
            match fingerprint {
                Some(fingerprint) => {
                    let sequence = ledger.next_sequence;
                    ledger.next_sequence += 1;
                    ledger.files.insert(
                        key.clone(),
                        Entry {
                            fingerprint: fingerprint.clone(),
                            sequence,
                        },
                    );
                }
                None => {
                    ledger.files.remove(key);
                }
            }
        }
        if ledger.files.len() > MAX_ENTRIES {
            let mut sequences: Vec<u64> = ledger.files.values().map(|e| e.sequence).collect();
            sequences.sort_unstable();
            let cutoff = sequences[ledger.files.len() - MAX_ENTRIES];
            ledger.files.retain(|_, entry| entry.sequence >= cutoff);
        }
        self.save(&ledger)
    }

    /// Fails, naming the first offending file, unless every existing target
    /// was read in this session and is unchanged since. A target that does
    /// not exist is a creation and needs no read, even when the session saw
    /// it before: nothing is there to overwrite, and a re-read of a deleted
    /// file could never succeed.
    pub(crate) fn check_before_edit(&self, targets: &[TrackedPath]) -> Result<(), String> {
        let ledger = {
            let _guard = LEDGER_LOCK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            self.load()?
        };
        for target in targets {
            let current = fingerprint(&target.absolute)
                .map_err(|error| format!("cannot check {}: {error}", target.key))?;
            let recorded = ledger.files.get(&target.key).map(|e| &e.fingerprint);
            match (current, recorded) {
                (None, _) => {}
                (Some(_), None) => {
                    return Err(format!(
                        "read {} with read_file before editing it; apply_patch only changes files this session has read",
                        target.key
                    ))
                }
                (Some(current), Some(recorded)) if &current != recorded => {
                    return Err(format!(
                        "file changed since you read it; re-read {} and retry",
                        target.key
                    ))
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Records the targets' contents after this session changed them.
    pub(crate) fn record_after_edit(&self, targets: &[TrackedPath]) -> Result<(), String> {
        let seen = targets
            .iter()
            .map(|target| {
                fingerprint(&target.absolute)
                    .map(|current| (target.key.clone(), current))
                    .map_err(|error| format!("cannot record {}: {error}", target.key))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.record(&seen)
    }

    fn load(&self) -> Result<Ledger, String> {
        match std::fs::read(&self.path) {
            // A damaged record only costs re-reads, so it starts over.
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!("the file-read record is damaged and was reset: {error}");
                Ledger::default()
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Ledger::default()),
            Err(error) => Err(format!("cannot read the file-read record: {error}")),
        }
    }

    fn save(&self, ledger: &Ledger) -> Result<(), String> {
        let dir = self
            .path
            .parent()
            .ok_or("the file-read record has no directory")?;
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create the file-read record: {error}"))?;
        let bytes = serde_json::to_vec(ledger)
            .map_err(|error| format!("cannot encode the file-read record: {error}"))?;
        let mut file = tempfile::NamedTempFile::new_in(dir)
            .map_err(|error| format!("cannot save the file-read record: {error}"))?;
        io::Write::write_all(&mut file, &bytes)
            .map_err(|error| format!("cannot save the file-read record: {error}"))?;
        file.persist(&self.path)
            .map_err(|error| format!("cannot save the file-read record: {}", error.error))?;
        Ok(())
    }
}

/// Maps patch paths (relative to `root`, as Git reports them) to tracked
/// paths. `scope` is `root`'s location below the workspace top. An existing
/// target is keyed by its on-disk name, as `read_file` keys it, so a
/// case-insensitive file system cannot make the two disagree. A path that
/// cannot be keyed fails the check instead of skipping it.
pub(crate) fn tracked_patch_paths(
    root: &Path,
    scope: &Path,
    paths: &[String],
) -> Result<Vec<TrackedPath>, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot check the patch's files: {error}"))?;
    paths
        .iter()
        .map(|path| {
            let unchecked = || format!("cannot check {path} for changes since it was read");
            // Git reports undecodable names lossily; such a name would not
            // match the file the patch really changes.
            if path.contains('\u{FFFD}') {
                return Err(unchecked());
            }
            let relative = Path::new(path);
            let absolute = root.join(relative);
            let on_disk = match std::fs::symlink_metadata(&absolute) {
                Ok(metadata) => {
                    let canonical = if metadata.file_type().is_symlink() {
                        absolute
                            .parent()
                            .and_then(|parent| parent.canonicalize().ok())
                            .zip(absolute.file_name())
                            .map(|(parent, name)| parent.join(name))
                    } else {
                        absolute.canonicalize().ok()
                    };
                    canonical
                        .and_then(|path| path.strip_prefix(&root).ok().map(Path::to_path_buf))
                        .ok_or_else(unchecked)?
                }
                Err(_) => relative.to_path_buf(),
            };
            Ok(TrackedPath {
                key: ledger_key(&scope.join(on_disk)).ok_or_else(unchecked)?,
                absolute,
            })
        })
        .collect()
}

/// The ledger key of `path` below the workspace top, or `None` for a path
/// that is absolute or climbs out with `..`.
pub(crate) fn ledger_key(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A fingerprint of the file at `path` without following a final symlink:
/// the SHA-256 of a regular file's bytes or of a link's target, or `None`
/// when nothing is there. Directories and special files get a fixed marker
/// and are never opened, so a FIFO cannot block the check. Files over
/// [`MAX_HASHED_BYTES`] are fingerprinted by size, modification time and
/// (on Unix) inode and change time, which bounds the work per read.
pub(crate) fn fingerprint(path: &Path) -> io::Result<Option<String>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = std::fs::read_link(path)?;
        let mut hasher = Sha256::new();
        hasher.update(b"link\0");
        hasher.update(target.as_os_str().as_encoded_bytes());
        return Ok(Some(format!("link:{}", hex(&hasher.finalize()))));
    }
    if !file_type.is_file() {
        return Ok(Some(
            if file_type.is_dir() { "dir" } else { "special" }.to_string(),
        ));
    }
    let mut file = open_regular_file(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(Some("special".to_string()));
    }
    if metadata.len() > MAX_HASHED_BYTES {
        return Ok(Some(metadata_fingerprint(&metadata)));
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(Some(hex(&hasher.finalize())))
}

#[cfg(unix)]
fn open_regular_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_regular_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn metadata_fingerprint(metadata: &std::fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos());
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}:{}.{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    };
    #[cfg(not(unix))]
    let identity = String::new();
    format!("meta:{}:{modified}:{identity}", metadata.len())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[path = "file_reads_tests.rs"]
mod tests;
