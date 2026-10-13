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
    File {
        blob: String,
        executable: bool,
        /// Unix permission bits; restored exactly so a private file stays
        /// private.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
    Symlink {
        target: String,
    },
    Unavailable {
        reason: String,
    },
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
        let opened = index.checkpoints[position].files.is_empty();
        let mut wrote_blob = false;
        let mut added = false;
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
            added = true;
        }
        if !added && !opened {
            return Ok(());
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
        // A workspace that is no longer the session's (a landed or removed
        // worktree) is never written; its files are reported instead.
        by_root.retain(|root, files| {
            if allowed.contains(root) {
                return true;
            }
            for (path, _) in files.drain(..) {
                report.skipped.push((
                    path,
                    format!(
                        "its workspace {} is no longer this session's (worktree landed or removed?)",
                        root.display()
                    ),
                ));
            }
            false
        });
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
        // Nothing is pruned until the restore is done: pruning could drop
        // the blobs the restore is about to read.
        self.save(&index)?;
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
        if let Err(error) = self.finish(index, wrote_blob) {
            tracing::warn!("checkpoints not pruned after a rewind: {error}");
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
        match self.try_capture(workspace, path, wrote_blob) {
            Ok(image) => image,
            Err(reason) => FileImage::Unavailable { reason },
        }
    }

    fn try_capture(
        &self,
        workspace: &Dir,
        path: &str,
        wrote_blob: &mut bool,
    ) -> Result<FileImage, String> {
        // Git reports a name that is not UTF-8 lossily; it cannot be found
        // again reliably, so it is not claimed as saved.
        if path.contains(char::REPLACEMENT_CHARACTER) {
            return Err("the file name is not valid UTF-8".to_string());
        }
        let (relative, name) = split_safe(path)?;
        let Some(parent) = open_parent(workspace, &relative, false)? else {
            return Ok(FileImage::Absent);
        };
        let metadata = match parent.symlink_metadata(&name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(FileImage::Absent),
            Err(error) => return Err(format!("cannot read: {error}")),
        };
        if metadata.is_symlink() || metadata_is_reparse(&metadata) {
            let target = parent
                .read_link_contents(&name)
                .map_err(|error| format!("cannot read the link: {error}"))?;
            return Ok(FileImage::Symlink {
                target: target.to_string_lossy().into_owned(),
            });
        }
        // The entry is opened without following a link and checked through
        // the handle, so a swap after the check cannot redirect the read.
        let (bytes, executable, mode) = read_regular_file(&parent, &name)?;
        let (blob, new) = self
            .store_blob(&bytes)
            .map_err(|error| format!("cannot save: {error}"))?;
        *wrote_blob |= new;
        Ok(FileImage::File {
            blob,
            executable,
            mode,
        })
    }

    fn restore(&self, workspace: &Dir, path: &str, image: &FileImage) -> Result<Restored, String> {
        if let FileImage::Unavailable { reason } = image {
            return Err(format!("not saved ({reason})"));
        }
        let (relative, name) = split_safe(path)?;
        let create = !matches!(image, FileImage::Absent);
        let Some(parent) = open_parent(workspace, &relative, create)? else {
            return Ok(Restored::Unchanged);
        };
        let current = match parent.symlink_metadata(&name) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("cannot inspect: {error}")),
        };
        let current_is_link = current
            .as_ref()
            .is_some_and(|metadata| metadata.is_symlink() || metadata_is_reparse(metadata));
        if current
            .as_ref()
            .is_some_and(|metadata| metadata.is_dir() && !current_is_link)
        {
            return Err("a directory is there now".to_string());
        }
        match image {
            FileImage::Absent => {
                if current.is_none() {
                    return Ok(Restored::Unchanged);
                }
                remove_entry(&parent, &name).map_err(|error| format!("cannot remove: {error}"))?;
                Ok(Restored::Removed)
            }
            FileImage::File {
                blob,
                executable,
                mode,
            } => {
                if current.is_some() && !current_is_link {
                    if let Ok((bytes, now_executable, now_mode)) = read_regular_file(&parent, &name)
                    {
                        if &hex_digest(&bytes) == blob
                            && now_executable == *executable
                            && (mode.is_none() || now_mode == *mode)
                        {
                            return Ok(Restored::Unchanged);
                        }
                    }
                }
                let bytes = self.read_blob(blob)?;
                replace_with_file(&parent, &name, &bytes, *executable, *mode)?;
                Ok(Restored::Written)
            }
            FileImage::Symlink { target } => {
                if current_is_link
                    && parent
                        .read_link_contents(&name)
                        .is_ok_and(|current| current.to_string_lossy() == target.as_str())
                {
                    return Ok(Restored::Unchanged);
                }
                if current.is_some() {
                    remove_entry(&parent, &name)
                        .map_err(|error| format!("cannot replace: {error}"))?;
                }
                create_symlink(&parent, target, &name)
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
            // Drop whole checkpoint numbers, oldest first, and always keep
            // the newest one (a rewind's undo spans one number).
            while self.referenced_bytes(&index) > MAX_BLOB_BYTES {
                let oldest = index.checkpoints[0].number;
                let newest = highest_number(&index);
                if oldest == newest {
                    break;
                }
                index
                    .checkpoints
                    .retain(|checkpoint| checkpoint.number != oldest);
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

/// Splits a safe workspace path into its parent directories and file name.
fn split_safe(path: &str) -> Result<(PathBuf, PathBuf), String> {
    let relative =
        safe_relative(path).ok_or("path is outside the workspace or inside .git or .nib")?;
    let name = relative
        .file_name()
        .map(PathBuf::from)
        .ok_or("path has no file name")?;
    Ok((relative, name))
}

/// Opens the parent directory of `relative` one component at a time without
/// following links, so no component can be swapped for a link to `.git`,
/// `.nib` or anywhere else. `Ok(None)` means a parent does not exist and
/// `create` is false.
fn open_parent(workspace: &Dir, relative: &Path, create: bool) -> Result<Option<Dir>, String> {
    use cap_fs_ext::DirExt;
    let mut current = workspace
        .try_clone()
        .map_err(|error| format!("cannot open the workspace: {error}"))?;
    let parts: Vec<_> = relative.components().collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        let next = match current.open_dir_nofollow(part) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                match current.create_dir(part) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(format!("cannot create its directory: {error}")),
                }
                current
                    .open_dir_nofollow(part)
                    .map_err(|error| not_a_directory(part, &error))?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(not_a_directory(part, &error)),
        };
        current = next;
    }
    Ok(Some(current))
}

fn not_a_directory(part: &Component<'_>, error: &io::Error) -> String {
    format!(
        "{} is a link or file, not a directory ({error})",
        part.as_os_str().to_string_lossy()
    )
}

/// Reads a regular file opened without following a link, checking its type
/// and size through the open handle.
fn read_regular_file(parent: &Dir, name: &Path) -> Result<(Vec<u8>, bool, Option<u32>), String> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = parent
        .open_with(name, &options)
        .map_err(|error| format!("cannot read: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot read: {error}"))?;
    if !metadata.is_file() || metadata_is_reparse(&metadata) {
        return Err("not a regular file".to_string());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("larger than 16 MiB".to_string());
    }
    let mut bytes = Vec::new();
    io::Read::read_to_end(
        &mut io::Read::take(&mut file, MAX_FILE_BYTES + 1),
        &mut bytes,
    )
    .map_err(|error| format!("cannot read: {error}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("larger than 16 MiB".to_string());
    }
    Ok((bytes, is_executable(&metadata), permission_bits(&metadata)))
}

/// Writes a new file beside the target and renames it over the target. The
/// rename replaces the directory entry itself, so a link there is replaced,
/// never followed. Temporary files a crashed rewind left in this directory
/// are removed first.
fn replace_with_file(
    parent: &Dir,
    name: &Path,
    bytes: &[u8],
    executable: bool,
    mode: Option<u32>,
) -> Result<(), String> {
    sweep_temporary_files(parent);
    let temporary = PathBuf::from(format!(
        "{TEMPORARY_PREFIX}{}",
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let written = (|| -> io::Result<()> {
        let mut file = parent.open_with(&temporary, &options)?;
        io::Write::write_all(&mut file, bytes)?;
        set_permissions(&file, executable, mode)?;
        file.sync_all()?;
        parent.rename(&temporary, parent, name)
    })();
    if let Err(error) = written {
        let _ = parent.remove_file(&temporary);
        return Err(format!("cannot write: {error}"));
    }
    Ok(())
}

const TEMPORARY_PREFIX: &str = ".nib-rewind-";

fn sweep_temporary_files(parent: &Dir) {
    let Ok(entries) = parent.entries() else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_leftover = name
            .to_str()
            .and_then(|name| name.strip_prefix(TEMPORARY_PREFIX))
            .is_some_and(|rest| rest.len() == 32 && rest.chars().all(|ch| ch.is_ascii_hexdigit()));
        if is_leftover && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = parent.remove_file(&name);
        }
    }
}

/// Removes a file or link entry. A Windows directory link or junction needs
/// `remove_dir`, which removes the link and never its target.
fn remove_entry(parent: &Dir, name: &Path) -> io::Result<()> {
    match parent.remove_file(name) {
        Err(error) if cfg!(windows) => match parent.symlink_metadata(name) {
            Ok(metadata) if metadata.is_dir() && metadata_is_reparse(&metadata) => {
                parent.remove_dir(name)
            }
            _ => Err(error),
        },
        result => result,
    }
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(_metadata: &cap_std::fs::Metadata) -> bool {
    false
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
fn permission_bits(metadata: &cap_std::fs::Metadata) -> Option<u32> {
    use cap_std::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn permission_bits(_metadata: &cap_std::fs::Metadata) -> Option<u32> {
    None
}

/// Sets the saved Unix permission bits through the open handle, or, for an
/// image without them, just the executable bit.
#[cfg(unix)]
fn set_permissions(
    file: &cap_std::fs::File,
    executable: bool,
    saved: Option<u32>,
) -> io::Result<()> {
    use cap_std::fs::PermissionsExt;
    let mut permissions = file.metadata()?.permissions();
    let mode = permissions.mode() & 0o777;
    let wanted = match saved {
        Some(bits) => bits & 0o777,
        None if executable => mode | ((mode & 0o444) >> 2),
        None => mode & !0o111,
    };
    if wanted != mode {
        permissions.set_mode(wanted);
        file.set_permissions(permissions)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_permissions(
    _file: &cap_std::fs::File,
    _executable: bool,
    _saved: Option<u32>,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(parent: &Dir, target: &str, name: &Path) -> io::Result<()> {
    parent.symlink_contents(target, name)
}

#[cfg(not(unix))]
fn create_symlink(_parent: &Dir, _target: &str, _name: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are restored only on Unix",
    ))
}

#[cfg(test)]
#[path = "checkpoints_tests.rs"]
mod tests;
