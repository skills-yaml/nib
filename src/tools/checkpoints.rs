//! Per-turn file checkpoints and `/rewind` (T080 phase 4).
//!
//! Before `apply_patch` writes, the executor saves the prior state of every
//! path the patch touches. A turn's first edit opens its checkpoint; later
//! edits in the same turn add only paths not yet saved. Contents live in a
//! content-addressed blob store next to the session files, which the sandbox
//! masks from tool commands. Restores go through a capability handle on the
//! recorded workspace root and never touch `.git` or `.nib`.

use cap_std::fs::Dir;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_CHECKPOINTS: usize = 100;
pub(crate) const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LABEL_CHARS: usize = 80;
const INDEX_FILE: &str = "index.json";
const BLOB_DIR: &str = "blobs";

/// The turn an edit belongs to: the count of human turns in the session and
/// the text of the latest one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnMarker {
    pub turn: usize,
    pub label: String,
}

impl TurnMarker {
    pub(crate) fn from_session(session: &crate::session::Session) -> Self {
        Self {
            turn: session.human_intent.len(),
            label: match session.human_intent.last() {
                Some(intent) if intent.kind == crate::session::HumanIntentKind::Continue => {
                    format!("continue plan {}", intent.text)
                }
                Some(intent) => intent.text.clone(),
                None => "session start".to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum FileImage {
    Absent,
    File { blob: String, executable: bool },
    Symlink { target: String },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SavedFile {
    pub path: String,
    #[serde(flatten)]
    pub image: FileImage,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckpointKind {
    Turn,
    Rewind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Checkpoint {
    pub number: u64,
    pub kind: CheckpointKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub root: PathBuf,
    /// A command that is not read-only ran during the turn; its effects are
    /// not part of the checkpoint.
    #[serde(default)]
    pub commands: bool,
    #[serde(default)]
    pub files: Vec<SavedFile>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    next_number: u64,
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RewindReport {
    pub number: u64,
    pub undo_number: Option<u64>,
    pub restored: Vec<String>,
    pub removed: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub commands: bool,
}

pub(crate) struct CheckpointStore {
    dir: PathBuf,
}

impl CheckpointStore {
    /// The store for one session, beside the profile's `sessions` directory.
    pub(crate) fn for_session(sessions_dir: &Path, session_id: &str) -> Result<Self, String> {
        if session_id.is_empty()
            || !session_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(format!(
                "invalid session id for checkpoints: {session_id:?}"
            ));
        }
        let state = sessions_dir
            .parent()
            .ok_or("the session store has no profile state directory")?;
        Ok(Self {
            dir: state.join("checkpoints").join(session_id),
        })
    }

    pub(crate) fn list(&self) -> Result<Vec<Checkpoint>, String> {
        Ok(self.load()?.checkpoints)
    }

    /// Saves the current state of `paths` (relative to `root`) into the
    /// turn's checkpoint unless the turn already saved them.
    pub(crate) fn record_before_edit(
        &self,
        turn: &TurnMarker,
        root: &Path,
        paths: &[String],
    ) -> Result<(), String> {
        let root = canonical_root(root)?;
        let mut index = self.load()?;
        let position = self.turn_checkpoint(&mut index, turn, &root);
        let workspace = open_root(&root)?;
        let mut wrote_blob = false;
        for path in paths {
            let checkpoint = &index.checkpoints[position];
            if checkpoint.files.iter().any(|saved| &saved.path == path) {
                continue;
            }
            let image = self.capture(&workspace, path, &mut wrote_blob);
            index.checkpoints[position].files.push(SavedFile {
                path: path.clone(),
                image,
            });
        }
        self.finish(index, wrote_blob)
    }

    /// Notes that a command that may write files ran during the turn.
    pub(crate) fn record_command(&self, turn: &TurnMarker, root: &Path) -> Result<(), String> {
        let root = canonical_root(root)?;
        let mut index = self.load()?;
        // A command before the turn's first edit opens the checkpoint, so
        // the warning is not lost.
        let position = self.turn_checkpoint(&mut index, turn, &root);
        if index.checkpoints[position].commands {
            return Ok(());
        }
        index.checkpoints[position].commands = true;
        self.finish(index, false)
    }

    /// Restores every file saved by checkpoint `number` and later ones to its
    /// state before checkpoint `number` began. `allowed_roots` are the
    /// workspaces this session may write: the project and its validated
    /// session worktree.
    pub(crate) fn rewind(
        &self,
        number: u64,
        allowed_roots: &[PathBuf],
    ) -> Result<RewindReport, String> {
        let mut index = self.load()?;
        let Some(start) = index
            .checkpoints
            .iter()
            .position(|checkpoint| checkpoint.number == number)
        else {
            return Err(format!("no checkpoint #{number} in this session"));
        };
        let mut report = RewindReport {
            number,
            ..RewindReport::default()
        };
        // The earliest saved image of each path is its state before `number`.
        let mut targets: BTreeMap<(PathBuf, String), FileImage> = BTreeMap::new();
        for checkpoint in &index.checkpoints[start..] {
            report.commands |= checkpoint.commands;
            for saved in &checkpoint.files {
                targets
                    .entry((checkpoint.root.clone(), saved.path.clone()))
                    .or_insert_with(|| saved.image.clone());
            }
        }
        let allowed: Vec<PathBuf> = allowed_roots
            .iter()
            .filter_map(|root| canonical_root(root).ok())
            .collect();
        let mut by_root: BTreeMap<PathBuf, Vec<(String, FileImage)>> = BTreeMap::new();
        for ((root, path), image) in targets {
            by_root.entry(root).or_default().push((path, image));
        }
        for root in by_root.keys() {
            if !allowed.contains(root) {
                return Err(format!(
                    "checkpoint #{number} edited {}, which is not this session's workspace",
                    root.display()
                ));
            }
        }
        if by_root.is_empty() {
            return Ok(report);
        }

        // Save the current state first so this rewind can be undone.
        let mut wrote_blob = false;
        index.next_number = index.next_number.max(highest_number(&index)) + 1;
        let undo_number = index.next_number;
        for (root, files) in &by_root {
            let workspace = open_root(root)?;
            let saved = files
                .iter()
                .map(|(path, _)| SavedFile {
                    path: path.clone(),
                    image: self.capture(&workspace, path, &mut wrote_blob),
                })
                .collect();
            index.checkpoints.push(Checkpoint {
                number: undo_number,
                kind: CheckpointKind::Rewind,
                turn: None,
                label: format!("before /rewind to #{number}"),
                created_at: Utc::now(),
                root: root.clone(),
                commands: false,
                files: saved,
            });
        }
        self.finish(index, wrote_blob)?;
        report.undo_number = Some(undo_number);

        for (root, files) in by_root {
            let workspace = open_root(&root)?;
            for (path, image) in files {
                match self.restore(&workspace, &path, &image) {
                    Ok(Restored::Written) => report.restored.push(path),
                    Ok(Restored::Removed) => report.removed.push(path),
                    Ok(Restored::Unchanged) => {}
                    Err(reason) => report.skipped.push((path, reason)),
                }
            }
        }
        Ok(report)
    }

    fn turn_checkpoint(&self, index: &mut Index, turn: &TurnMarker, root: &Path) -> usize {
        // Only the newest checkpoint can still belong to the current turn;
        // a rewind in between starts a new one.
        if let Some(position) = index.checkpoints.iter().rposition(|checkpoint| {
            checkpoint.kind == CheckpointKind::Turn
                && checkpoint.turn == Some(turn.turn)
                && checkpoint.root == root
        }) {
            let newest = index.checkpoints.last().map(|checkpoint| checkpoint.number);
            if Some(index.checkpoints[position].number) == newest {
                return position;
            }
        }
        index.next_number = index.next_number.max(highest_number(index)) + 1;
        index.checkpoints.push(Checkpoint {
            number: index.next_number,
            kind: CheckpointKind::Turn,
            turn: Some(turn.turn),
            label: short_label(&turn.label),
            created_at: Utc::now(),
            root: root.to_path_buf(),
            commands: false,
            files: Vec::new(),
        });
        index.checkpoints.len() - 1
    }

    fn capture(&self, workspace: &Dir, path: &str, wrote_blob: &mut bool) -> FileImage {
        let Some(relative) = safe_relative(path) else {
            return FileImage::Unavailable {
                reason: "path is outside the workspace or inside .git or .nib".to_string(),
            };
        };
        if let Err(reason) = parents_are_directories(workspace, &relative) {
            return FileImage::Unavailable { reason };
        }
        let metadata = match workspace.symlink_metadata(&relative) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return FileImage::Absent,
            Err(error) => {
                return FileImage::Unavailable {
                    reason: format!("cannot read: {error}"),
                }
            }
        };
        if metadata.is_symlink() {
            return match workspace.read_link(&relative) {
                Ok(target) => FileImage::Symlink {
                    target: target.to_string_lossy().into_owned(),
                },
                Err(error) => FileImage::Unavailable {
                    reason: format!("cannot read the link: {error}"),
                },
            };
        }
        if !metadata.is_file() {
            return FileImage::Unavailable {
                reason: "not a regular file".to_string(),
            };
        }
        if metadata.len() > MAX_FILE_BYTES {
            return FileImage::Unavailable {
                reason: "larger than 16 MiB".to_string(),
            };
        }
        let bytes = match workspace.read(&relative) {
            Ok(bytes) => bytes,
            Err(error) => {
                return FileImage::Unavailable {
                    reason: format!("cannot read: {error}"),
                }
            }
        };
        match self.store_blob(&bytes) {
            Ok((blob, new)) => {
                *wrote_blob |= new;
                FileImage::File {
                    blob,
                    executable: is_executable(&metadata),
                }
            }
            Err(error) => FileImage::Unavailable {
                reason: format!("cannot save: {error}"),
            },
        }
    }

    fn restore(&self, workspace: &Dir, path: &str, image: &FileImage) -> Result<Restored, String> {
        let relative =
            safe_relative(path).ok_or("path is outside the workspace or inside .git or .nib")?;
        if let FileImage::Unavailable { reason } = image {
            return Err(format!("not saved ({reason})"));
        }
        parents_are_directories(workspace, &relative)?;
        let current = match workspace.symlink_metadata(&relative) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("cannot inspect: {error}")),
        };
        if current.as_ref().is_some_and(|metadata| metadata.is_dir()) {
            return Err("a directory is there now".to_string());
        }
        let remove_current = |workspace: &Dir| match &current {
            Some(_) => workspace
                .remove_file(&relative)
                .map_err(|error| format!("cannot replace: {error}")),
            None => Ok(()),
        };
        match image {
            FileImage::Absent => {
                if current.is_none() {
                    return Ok(Restored::Unchanged);
                }
                remove_current(workspace)?;
                Ok(Restored::Removed)
            }
            FileImage::File { blob, executable } => {
                let bytes = self.read_blob(blob)?;
                if let Some(parent) = relative.parent().filter(|p| !p.as_os_str().is_empty()) {
                    workspace
                        .create_dir_all(parent)
                        .map_err(|error| format!("cannot create its directory: {error}"))?;
                    parents_are_directories(workspace, &relative)?;
                }
                remove_current(workspace)?;
                workspace
                    .write(&relative, &bytes)
                    .map_err(|error| format!("cannot write: {error}"))?;
                set_executable(workspace, &relative, *executable)
                    .map_err(|error| format!("cannot set permissions: {error}"))?;
                Ok(Restored::Written)
            }
            FileImage::Symlink { target } => {
                remove_current(workspace)?;
                create_symlink(workspace, target, &relative)
                    .map_err(|error| format!("cannot create the link: {error}"))?;
                Ok(Restored::Written)
            }
            FileImage::Unavailable { .. } => unreachable!("handled above"),
        }
    }

    fn load(&self) -> Result<Index, String> {
        match std::fs::read(self.dir.join(INDEX_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("checkpoint index is unreadable: {error}")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Index::default()),
            Err(error) => Err(format!("cannot read checkpoints: {error}")),
        }
    }

    fn finish(&self, mut index: Index, wrote_blob: bool) -> Result<(), String> {
        let pruned = prune_count(&index);
        if pruned > 0 {
            index.checkpoints.drain(..pruned);
        }
        let mut must_collect = pruned > 0;
        if wrote_blob {
            while index.checkpoints.len() > 1 && self.referenced_bytes(&index) > MAX_BLOB_BYTES {
                index.checkpoints.remove(0);
                must_collect = true;
            }
        }
        self.save(&index)?;
        if must_collect {
            self.collect_garbage(&index);
        }
        Ok(())
    }

    fn save(&self, index: &Index) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("cannot create the checkpoint store: {error}"))?;
        let bytes = serde_json::to_vec_pretty(index)
            .map_err(|error| format!("cannot encode checkpoints: {error}"))?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)
            .map_err(|error| format!("cannot save checkpoints: {error}"))?;
        io::Write::write_all(&mut file, &bytes)
            .map_err(|error| format!("cannot save checkpoints: {error}"))?;
        file.persist(self.dir.join(INDEX_FILE))
            .map_err(|error| format!("cannot save checkpoints: {}", error.error))?;
        Ok(())
    }

    fn blob_path(&self, blob: &str) -> PathBuf {
        self.dir.join(BLOB_DIR).join(blob)
    }

    fn store_blob(&self, bytes: &[u8]) -> io::Result<(String, bool)> {
        let blob = hex_digest(bytes);
        let path = self.blob_path(&blob);
        if path.is_file() {
            return Ok((blob, false));
        }
        let blobs = self.dir.join(BLOB_DIR);
        std::fs::create_dir_all(&blobs)?;
        let mut file = tempfile::NamedTempFile::new_in(&blobs)?;
        io::Write::write_all(&mut file, bytes)?;
        file.persist(&path).map_err(|error| error.error)?;
        Ok((blob, true))
    }

    fn read_blob(&self, blob: &str) -> Result<Vec<u8>, String> {
        if blob.len() != 64 || !blob.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err("checkpoint data is damaged".to_string());
        }
        let bytes = std::fs::read(self.blob_path(blob))
            .map_err(|error| format!("checkpoint data is missing: {error}"))?;
        if hex_digest(&bytes) != blob {
            return Err("checkpoint data is damaged".to_string());
        }
        Ok(bytes)
    }

    fn referenced_bytes(&self, index: &Index) -> u64 {
        referenced_blobs(index)
            .iter()
            .filter_map(|blob| std::fs::metadata(self.blob_path(blob)).ok())
            .map(|metadata| metadata.len())
            .sum()
    }

    fn collect_garbage(&self, index: &Index) {
        let keep = referenced_blobs(index);
        let Ok(entries) = std::fs::read_dir(self.dir.join(BLOB_DIR)) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !keep.contains(&name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

enum Restored {
    Written,
    Removed,
    Unchanged,
}

fn highest_number(index: &Index) -> u64 {
    index
        .checkpoints
        .iter()
        .map(|checkpoint| checkpoint.number)
        .max()
        .unwrap_or(0)
}

fn prune_count(index: &Index) -> usize {
    // A rewind records one undo checkpoint per workspace root under one
    // number; count numbers, not entries.
    let mut numbers: Vec<u64> = index.checkpoints.iter().map(|c| c.number).collect();
    numbers.dedup();
    if numbers.len() <= MAX_CHECKPOINTS {
        return 0;
    }
    let oldest_kept = numbers[numbers.len() - MAX_CHECKPOINTS];
    index
        .checkpoints
        .iter()
        .take_while(|checkpoint| checkpoint.number < oldest_kept)
        .count()
}

fn referenced_blobs(index: &Index) -> HashSet<String> {
    index
        .checkpoints
        .iter()
        .flat_map(|checkpoint| &checkpoint.files)
        .filter_map(|saved| match &saved.image {
            FileImage::File { blob, .. } => Some(blob.clone()),
            _ => None,
        })
        .collect()
}

fn short_label(label: &str) -> String {
    let line = label.lines().next().unwrap_or("").trim();
    if line.chars().count() <= MAX_LABEL_CHARS {
        return line.to_string();
    }
    let mut short: String = line.chars().take(MAX_LABEL_CHARS - 3).collect();
    short.push_str("...");
    short
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    root.canonicalize()
        .map_err(|error| format!("cannot resolve the workspace {}: {error}", root.display()))
}

fn open_root(root: &Path) -> Result<Dir, String> {
    Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .map_err(|error| format!("cannot open the workspace {}: {error}", root.display()))
}

/// A workspace-relative path made only of normal components, none of them
/// `.git` or `.nib` (ignoring case and trailing dots or spaces, as Windows
/// and macOS file systems do).
pub(crate) fn safe_relative(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    let mut relative = PathBuf::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        let name = part.to_string_lossy();
        let name = name.trim_end_matches(['.', ' ']);
        if name.is_empty() || name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case(".nib")
        {
            return None;
        }
        relative.push(part);
    }
    (!relative.as_os_str().is_empty()).then_some(relative)
}

/// Every existing parent of `relative` must be a real directory: a symlink
/// there could redirect the write elsewhere inside the workspace.
fn parents_are_directories(workspace: &Dir, relative: &Path) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    let parts: Vec<_> = relative.components().collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        prefix.push(part);
        match workspace.symlink_metadata(&prefix) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "{} is a link or file, not a directory",
                    prefix.display()
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("cannot inspect {}: {error}", prefix.display())),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &cap_std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(workspace: &Dir, relative: &Path, executable: bool) -> io::Result<()> {
    use cap_std::fs::PermissionsExt;
    let mut permissions = workspace.metadata(relative)?.permissions();
    let mode = permissions.mode();
    let wanted = if executable {
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    };
    if wanted != mode {
        permissions.set_mode(wanted);
        workspace.set_permissions(relative, permissions)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_workspace: &Dir, _relative: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(workspace: &Dir, target: &str, relative: &Path) -> io::Result<()> {
    workspace.symlink(target, relative)
}

#[cfg(not(unix))]
fn create_symlink(_workspace: &Dir, _target: &str, _relative: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are restored only on Unix",
    ))
}

#[cfg(test)]
#[path = "checkpoints_tests.rs"]
mod tests;
