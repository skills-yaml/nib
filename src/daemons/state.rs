use serde::Serialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
#[cfg(test)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const STRICT_RECOVERY_LIVE_WRITER_WAIT: Duration = Duration::from_millis(250);
const STRICT_RECOVERY_POLL_INTERVAL: Duration = Duration::from_millis(5);
const FILE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandlePublication {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Linked,
    #[cfg(any(windows, target_os = "macos", target_os = "ios"))]
    Moved,
}

#[derive(Clone, Copy)]
pub(crate) enum FileExpectation<'a> {
    #[cfg(test)]
    Any,
    Missing,
    Present(&'a File),
}

#[derive(Debug)]
pub(crate) struct FilePublicationReceipt {
    pub(crate) file: File,
    pub(crate) exact_identity: bool,
}

#[derive(Debug)]
pub(crate) struct FilePublicationError {
    pub(crate) message: String,
    pub(crate) receipt: Option<FilePublicationReceipt>,
}

impl From<String> for FilePublicationError {
    fn from(message: String) -> Self {
        Self {
            message,
            receipt: None,
        }
    }
}

struct AtomicSaveExpectation<'a> {
    require_attached_before_commit: bool,
    file: FileExpectation<'a>,
    retain_publication_lock: bool,
}

struct AtomicSaveHooks<BeforeCommit, AfterEvacuation, BeforeReceipt> {
    before_commit: BeforeCommit,
    after_evacuation: AfterEvacuation,
    before_receipt: BeforeReceipt,
}

#[derive(Clone, Copy)]
struct AtomicRecoveryPolicy {
    skip_live_writer: bool,
    reject_obscured_live_writer: bool,
    strict_deadline: Option<Instant>,
}

struct AtomicRecoveryHooks<'a> {
    previous_open: &'a mut dyn FnMut() -> Result<(), String>,
    live_target: &'a mut dyn FnMut(),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StableEntryKind {
    File,
    Directory,
}

pub(crate) struct StableDirectory {
    path: PathBuf,
    directory: cap_std::fs::Dir,
    identity: crate::fs_security::FileIdentity,
    delete_capable: bool,
}

impl std::fmt::Debug for StableDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StableDirectory")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl StableDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        crate::fs_security::verify_directory_without_symlinks(path)
            .map_err(|error| format!("state directory is unsafe: {error}"))?;
        #[cfg(not(windows))]
        let directory = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
            .map_err(|error| {
                format!("failed to open state directory {}: {error}", path.display())
            })?;
        #[cfg(windows)]
        let directory = cap_std::fs::Dir::from_std_file(
            crate::fs_security::open_directory_observation_windows(path).map_err(|error| {
                format!("failed to open state directory {}: {error}", path.display())
            })?,
        );
        let identity = stable_directory_identity(&directory, path)?;
        let stable = Self {
            path: path.to_path_buf(),
            directory,
            identity,
            delete_capable: false,
        };
        stable.verify_visible()?;
        Ok(stable)
    }

    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        self.identity == other.identity
    }

    pub(crate) fn directory_removal_receipt(
        &self,
    ) -> Result<crate::fs_security::DirectoryRemovalReceipt, String> {
        #[cfg(windows)]
        let file = {
            let file = windows_visible_directory_file(&self.path)?;
            let visible_identity =
                crate::fs_security::FileIdentity::from_file(file.try_clone().map_err(|error| {
                    format!(
                        "failed to clone directory observation {}: {error}",
                        self.path.display()
                    )
                })?)
                .map_err(|error| {
                    format!(
                        "failed to identify directory observation {}: {error}",
                        self.path.display()
                    )
                })?;
            if visible_identity != self.identity {
                return Err(format!(
                    "state directory identity changed while its ownership was retained: {}",
                    self.path.display()
                ));
            }
            file
        };
        #[cfg(not(windows))]
        let file = self
            .directory
            .try_clone()
            .map(cap_std::fs::Dir::into_std_file)
            .map_err(|error| {
                format!(
                    "failed to retain directory ownership for {}: {error}",
                    self.path.display()
                )
            })?;
        crate::fs_security::DirectoryRemovalReceipt::from_open_directory(file)
            .map_err(|error| format!("failed to retain {}: {error}", self.path.display()))
    }

    pub(crate) fn try_clone(&self) -> Result<Self, String> {
        let directory = self
            .directory
            .try_clone()
            .map_err(|error| format!("failed to clone {}: {error}", self.path.display()))?;
        let identity = stable_directory_identity(&directory, &self.path)?;
        Ok(Self {
            path: self.path.clone(),
            directory,
            identity,
            delete_capable: self.delete_capable,
        })
    }

    pub(crate) fn try_clone_at(&self, path: &Path) -> Result<Self, String> {
        self.verify_visible_at(path)?;
        let directory = self
            .directory
            .try_clone()
            .map_err(|error| format!("failed to clone {}: {error}", path.display()))?;
        let identity = stable_directory_identity(&directory, path)?;
        if identity != self.identity {
            return Err(format!(
                "state directory identity changed while it was relocated: {}",
                path.display()
            ));
        }
        let stable = Self {
            path: path.to_path_buf(),
            directory,
            identity,
            delete_capable: self.delete_capable,
        };
        stable.verify_visible()?;
        Ok(stable)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn open_child(&self, path: &Path) -> Result<Self, String> {
        let relative = self.relative_file(path)?;
        #[cfg(not(windows))]
        let directory = self.directory.open_dir(relative).map_err(|error| {
            format!("failed to open state directory {}: {error}", path.display())
        })?;
        #[cfg(windows)]
        let directory = {
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true)._cap_fs_ext_maybe_dir(true);
            configure_capability_no_follow(&mut options);
            let file = self
                .directory
                .open_with(relative, &options)
                .map(cap_std::fs::File::into_std)
                .map_err(|error| {
                    format!("failed to open state directory {}: {error}", path.display())
                })?;
            let metadata = file.metadata().map_err(|error| {
                format!(
                    "failed to inspect state directory {}: {error}",
                    path.display()
                )
            })?;
            if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(format!(
                    "state directory must be local and must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            cap_std::fs::Dir::from_std_file(file)
        };
        let identity = stable_directory_identity(&directory, path)?;
        let stable = Self {
            path: path.to_path_buf(),
            directory,
            identity,
            delete_capable: false,
        };
        stable.verify_visible()?;
        Ok(stable)
    }

    pub(crate) fn open_owned_child(&self, path: &Path) -> Result<Self, String> {
        let relative = self.relative_file(path)?;
        #[cfg(not(windows))]
        let directory = self.directory.open_dir(relative).map_err(|error| {
            format!("failed to open state directory {}: {error}", path.display())
        })?;
        #[cfg(windows)]
        let directory = {
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true)._cap_fs_ext_maybe_dir(true);
            configure_capability_no_follow(&mut options);
            configure_capability_delete_access(&mut options, true, false);
            let file = self
                .directory
                .open_with(relative, &options)
                .map(cap_std::fs::File::into_std)
                .map_err(|error| {
                    format!("failed to open state directory {}: {error}", path.display())
                })?;
            let metadata = file.metadata().map_err(|error| {
                format!(
                    "failed to inspect state directory {}: {error}",
                    path.display()
                )
            })?;
            if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(format!(
                    "state directory must be local and must not be a symlink or reparse point: {}",
                    path.display()
                ));
            }
            cap_std::fs::Dir::from_std_file(file)
        };
        let identity = stable_directory_identity(&directory, path)?;
        let stable = Self {
            path: path.to_path_buf(),
            directory,
            identity,
            delete_capable: true,
        };
        stable.verify_visible()?;
        Ok(stable)
    }

    pub(crate) fn create_child_directory(&self, path: &Path) -> Result<Self, String> {
        let relative = self.relative_file(path)?;
        if self.entry_kind(path)?.is_some() {
            return Err(format!("state entry already exists: {}", path.display()));
        }
        self.verify_visible()?;
        self.directory
            .create_dir(relative)
            .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
        self.sync_directory()?;
        self.open_child(path)
    }

    pub(crate) fn create_owned_child_directory(&self, path: &Path) -> Result<Self, String> {
        let relative = self.relative_file(path)?;
        if self.entry_kind(path)?.is_some() {
            return Err(format!("state entry already exists: {}", path.display()));
        }
        self.verify_visible()?;
        self.directory
            .create_dir(relative)
            .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
        self.sync_directory()?;
        self.open_owned_child(path)
    }

    pub(crate) fn verify_visible(&self) -> Result<(), String> {
        self.verify_visible_at(&self.path)
    }

    pub(crate) fn verify_visible_at(&self, path: &Path) -> Result<(), String> {
        crate::fs_security::verify_directory_without_symlinks(path)
            .map_err(|error| format!("state directory changed: {error}"))?;
        #[cfg(not(windows))]
        let visible = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
            .map_err(|error| {
                format!(
                    "failed to re-open state directory {}: {error}",
                    path.display()
                )
            })?;
        #[cfg(not(windows))]
        let visible_identity = stable_directory_identity(&visible, path)?;
        #[cfg(windows)]
        let visible_identity = windows_visible_directory_identity(path)?;
        if visible_identity != self.identity {
            return Err(format!(
                "state directory identity changed while it was in use: {}",
                path.display()
            ));
        }
        Ok(())
    }

    pub(crate) fn open_read(&self, path: &Path) -> Result<File, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        configure_capability_no_follow(&mut options);
        configure_capability_delete_access(&mut options, true, false);
        let file = self
            .directory
            .open_with(relative, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        self.verify_file_identity(path, &file)?;
        Ok(file)
    }

    pub(crate) fn open_read_write(&self, path: &Path) -> Result<File, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true);
        configure_capability_no_follow(&mut options);
        configure_capability_delete_access(&mut options, true, true);
        let file = self
            .directory
            .open_with(relative, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        self.verify_file_identity(path, &file)?;
        Ok(file)
    }

    pub(crate) fn open_read_write_create(&self, path: &Path) -> Result<File, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        configure_capability_owner_only(&mut options);
        configure_capability_no_follow(&mut options);
        configure_capability_delete_access(&mut options, true, true);
        let file = self
            .directory
            .open_with(relative, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        self.verify_file_identity(path, &file)?;
        Ok(file)
    }

    pub(crate) fn hard_link_to(
        &self,
        source: &Path,
        destination_directory: &Self,
        destination: &Path,
    ) -> Result<(), String> {
        let source = self.relative_file(source)?;
        let destination_relative = destination_directory.relative_file(destination)?;
        match self.directory.hard_link(
            source,
            &destination_directory.directory,
            destination_relative,
        ) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(format!(
                "failed to create stable link {}: {error}",
                destination.display()
            )),
        }
    }

    pub(crate) fn open_append_create(&self, path: &Path) -> Result<File, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.append(true).create(true);
        configure_capability_owner_only(&mut options);
        configure_capability_no_follow(&mut options);
        let file = self
            .directory
            .open_with(relative, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        self.verify_file_identity(path, &file)?;
        Ok(file)
    }

    pub(crate) fn path_exists(&self, path: &Path) -> Result<bool, String> {
        let relative = self.relative_file(path)?;
        match self.directory.symlink_metadata(relative) {
            Ok(metadata) => {
                validate_capability_file_metadata(path, &metadata)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("failed to inspect {}: {error}", path.display())),
        }
    }

    pub(crate) fn entry_kind(&self, path: &Path) -> Result<Option<StableEntryKind>, String> {
        let relative = self.relative_file(path)?;
        match self.directory.symlink_metadata(relative) {
            Ok(metadata) => {
                if capability_file_metadata_is_link(&metadata) {
                    return Err(format!(
                        "state entry must not be a symlink or reparse point: {}",
                        path.display()
                    ));
                }
                if metadata.is_file() {
                    Ok(Some(StableEntryKind::File))
                } else if metadata.is_dir() {
                    Ok(Some(StableEntryKind::Directory))
                } else {
                    Err(format!(
                        "state entry must be a regular file or directory: {}",
                        path.display()
                    ))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("failed to inspect {}: {error}", path.display())),
        }
    }

    pub(crate) fn verify_file_expectation(
        &self,
        path: &Path,
        expected: FileExpectation<'_>,
    ) -> Result<(), String> {
        match expected {
            #[cfg(test)]
            FileExpectation::Any => self.path_exists(path).map(|_| ()),
            FileExpectation::Missing => {
                if self.path_exists(path)? {
                    Err(format!(
                        "state file appeared before publication: {}",
                        path.display()
                    ))
                } else {
                    Ok(())
                }
            }
            FileExpectation::Present(file) => self.verify_file_identity(path, file),
        }
    }

    fn remove_file_bound_without_sync(&self, path: &Path) -> Result<(), String> {
        let relative = self.relative_file(path)?;
        self.directory
            .remove_file(relative)
            .map_err(|error| format!("failed to remove {}: {error}", path.display()))
    }

    pub(crate) fn recover_quarantined_file(
        &self,
        path: &Path,
        quarantine_prefix: &str,
    ) -> Result<(), String> {
        let quarantine =
            self.deterministic_artifact_path(path, quarantine_prefix, ".quarantine")?;
        let source_exists = self.path_exists(path)?;
        let quarantine_exists = self.path_exists(&quarantine)?;
        match (source_exists, quarantine_exists) {
            (_, false) => Ok(()),
            (true, true) => {
                let source_file = self.open_read_write(path)?;
                let quarantine_file = self.open_read_write(&quarantine)?;
                if same_open_file_identity(&source_file, &quarantine_file)? {
                    self.remove_visible_file_if_matches(&quarantine, &quarantine_file, || Ok(()))
                } else {
                    Err(format!(
                        "state file and an ambiguous deletion quarantine both exist; both were preserved: {}",
                        path.display()
                    ))
                }
            }
            (false, true) => Err(format!(
                "state file is missing while an unproven deletion quarantine exists; the quarantine was preserved: {}",
                path.display()
            )),
        }
    }

    pub(crate) fn remove_file_if_matches(
        &self,
        path: &Path,
        expected: &File,
        quarantine_prefix: &str,
    ) -> Result<(), String> {
        self.remove_file_if_matches_with_hook(path, expected, quarantine_prefix, || Ok(()))
    }

    pub(crate) fn remove_visible_file_if_matches_direct(
        &self,
        path: &Path,
        expected: &File,
    ) -> Result<(), String> {
        self.verify_file_identity(path, expected)?;
        self.remove_visible_file_if_matches(path, expected, || Ok(()))
    }

    pub(crate) fn remove_file_if_matches_with_hook(
        &self,
        path: &Path,
        expected: &File,
        quarantine_prefix: &str,
        before_quarantine: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.remove_file_if_matches_with_hooks(
            path,
            expected,
            quarantine_prefix,
            before_quarantine,
            || Ok(()),
        )
    }

    pub(crate) fn remove_file_if_matches_with_hooks(
        &self,
        path: &Path,
        expected: &File,
        quarantine_prefix: &str,
        before_quarantine: impl FnOnce() -> Result<(), String>,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.recover_quarantined_file(path, quarantine_prefix)?;
        let quarantine =
            self.deterministic_artifact_path(path, quarantine_prefix, ".quarantine")?;
        self.verify_file_identity(path, expected)?;
        before_quarantine()?;
        self.move_open_file_no_replace(path, expected, &quarantine)?;
        before_delete()?;
        self.remove_visible_file_if_matches(&quarantine, expected, || Ok(()))
    }

    pub(crate) fn rename_child_directory(
        &self,
        source: &Path,
        expected: &Self,
        destination: &Path,
    ) -> Result<(), String> {
        #[cfg(windows)]
        if !expected.delete_capable {
            return Err(format!(
                "state directory lacks the retained DELETE capability required for rename: {}",
                source.display()
            ));
        }
        if self.entry_kind(source)? != Some(StableEntryKind::Directory) {
            return Err(format!(
                "state directory does not exist or is not local: {}",
                source.display()
            ));
        }
        if self.entry_kind(destination)?.is_some() {
            return Err(format!(
                "state directory quarantine already exists: {}",
                destination.display()
            ));
        }
        self.verify_visible()?;
        expected.verify_visible_at(source)?;
        rename_open_directory_no_replace_platform(
            &self.directory,
            self.relative_file(source)?,
            &expected.directory,
            self.relative_file(destination)?,
        )
        .map_err(|error| {
            format!(
                "failed to quarantine state directory {}: {error}",
                source.display()
            )
        })?;
        self.sync_directory()?;
        expected.verify_visible_at(destination)?;
        self.verify_visible()
    }

    pub(crate) fn remove_empty_child_directory_if_matches(
        &self,
        path: &Path,
        expected: Self,
    ) -> Result<(), String> {
        #[cfg(windows)]
        if !expected.delete_capable {
            return Err(format!(
                "state directory lacks the retained DELETE capability required for removal: {}",
                path.display()
            ));
        }
        self.verify_visible()?;
        self.relative_file(path)?;
        expected.verify_visible_at(path)?;
        #[cfg(windows)]
        {
            let Self {
                directory,
                identity,
                path: _,
                delete_capable: _,
            } = expected;
            drop(identity);
            let path_bound = directory.into_std_file();
            delete_open_file_platform(&path_bound).map_err(|error| {
                format!(
                    "failed to remove the expected open state directory {}: {error}",
                    path.display()
                )
            })?;
            drop(path_bound);
        }
        #[cfg(not(windows))]
        drop(expected);
        #[cfg(not(windows))]
        let relative = self.relative_file(path)?;
        #[cfg(not(windows))]
        self.directory
            .remove_dir(relative)
            .map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
        self.sync_directory()?;
        if self.entry_kind(path)?.is_some() {
            return Err(format!(
                "state directory reappeared during removal; replacement preserved: {}",
                path.display()
            ));
        }
        self.verify_visible()
    }

    pub(crate) fn rename_file_if_matches(
        &self,
        source: &Path,
        destination: &Path,
        expected: &File,
    ) -> Result<(), String> {
        if self.path_exists(destination)? {
            return Err(format!(
                "state destination already exists: {}",
                destination.display()
            ));
        }
        self.verify_visible()?;
        self.verify_file_identity(source, expected)?;
        self.move_open_file_no_replace(source, expected, destination)
    }

    fn move_open_file_no_replace(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
    ) -> Result<(), String> {
        self.verify_visible()?;
        self.move_open_file_no_replace_bound(source, expected, destination)?;
        self.verify_visible()
    }

    fn move_open_file_no_replace_bound(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
    ) -> Result<(), String> {
        self.move_open_file_no_replace_bound_with_hook(source, expected, destination, || Ok(()))
    }

    fn move_open_file_no_replace_bound_with_hook(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
        after_identity_check: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        #[cfg(unix)]
        {
            self.verify_file_identity(source, expected)?;
            after_identity_check()?;
            rename_file_no_replace_platform(
                &self.directory,
                self.relative_file(source)?,
                self.relative_file(destination)?,
            )
            .map_err(|error| {
                format!(
                    "failed to move state file {} without replacing {}: {error}",
                    source.display(),
                    destination.display()
                )
            })?;

            let moved = match self.open_read(destination) {
                Ok(moved) => moved,
                Err(error) => {
                    let rescue = self.rescue_moved_file(destination, source);
                    return Err(match rescue {
                        Ok(()) => format!(
                            "moved state file could not be verified and was restored to {}: {error}",
                            source.display()
                        ),
                        Err(rescue_error) => format!(
                            "moved state file could not be verified; all visible entries were preserved: {error}; rescue failed: {rescue_error}"
                        ),
                    });
                }
            };
            let identity_failure = match same_open_file_identity(&moved, expected) {
                Ok(true) => None,
                Ok(false) => Some("state source changed while it was moved".to_string()),
                Err(error) => Some(format!(
                    "moved state file identity could not be verified: {error}"
                )),
            };
            if let Some(identity_failure) = identity_failure {
                let rescue = self.rescue_moved_file(destination, source);
                return Err(match rescue {
                    Ok(()) => format!(
                        "{identity_failure} and the unverified file was restored to {}",
                        source.display()
                    ),
                    Err(rescue_error) => format!(
                        "{identity_failure}; all visible entries were preserved because rescue failed: {rescue_error}"
                    ),
                });
            }
            if self.path_exists(source)? {
                return Err(format!(
                    "state source path reappeared after its expected file was moved; both entries were preserved: {}",
                    source.display()
                ));
            }
            self.sync_directory()
        }

        #[cfg(windows)]
        {
            self.verify_file_identity(source, expected)?;
            let source_bound = self.open_read(source)?;
            if !same_open_file_identity(&source_bound, expected)? {
                return Err(format!(
                    "state source changed before its path-bound handle was retained: {}",
                    source.display()
                ));
            }
            after_identity_check()?;
            let publication =
                self.publish_open_file_no_replace(source, &source_bound, destination)?;
            self.verify_published_file(destination, &source_bound, publication)?;
            if self.path_exists(source)? {
                return Err(format!(
                    "state source path reappeared while its open file was moved: {}",
                    source.display()
                ));
            }
            self.sync_directory()
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = (source, expected, destination, after_identity_check);
            Err("this platform has no safe state-file relocation primitive".to_string())
        }
    }

    #[cfg(unix)]
    fn rescue_moved_file(&self, source: &Path, destination: &Path) -> Result<(), String> {
        rename_file_no_replace_platform(
            &self.directory,
            self.relative_file(source)?,
            self.relative_file(destination)?,
        )
        .map_err(|error| {
            format!(
                "failed to restore moved state file {} to {}: {error}",
                source.display(),
                destination.display()
            )
        })?;
        self.sync_directory()
    }

    fn verify_published_file(
        &self,
        destination: &Path,
        expected: &File,
        publication: HandlePublication,
    ) -> Result<(), String> {
        let _ = publication;
        self.verify_file_identity(destination, expected)
    }

    fn publish_open_file_no_replace(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
    ) -> Result<HandlePublication, String> {
        let destination_relative = self.relative_file(destination)?;
        let source_relative = self.relative_file(source)?;
        let publication = publish_open_file_no_replace_platform(
            &self.directory,
            source_relative,
            expected,
            destination_relative,
        )
        .map_err(|error| {
            format!(
                "failed to publish the open state file without replacing {}: {error}",
                destination.display()
            )
        })?;

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Err(identity_error) = self.verify_file_identity(destination, expected) {
            let rescue = self.open_read(destination).and_then(|published| {
                self.move_open_file_no_replace_bound(destination, &published, source)
            });
            return Err(match rescue {
                Ok(()) => format!(
                    "published state source changed before pathname-bound publication and the unverified file was restored to {}: {identity_error}",
                    source.display()
                ),
                Err(rescue_error) => format!(
                    "published state source changed before pathname-bound publication; all visible entries were preserved: {identity_error}; rescue failed: {rescue_error}"
                ),
            });
        }

        Ok(publication)
    }

    fn remove_visible_file_if_matches(
        &self,
        path: &Path,
        expected: &File,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.verify_visible()?;
        self.remove_bound_file_if_matches(path, expected, before_delete)?;
        self.verify_visible()
    }

    fn remove_bound_file_if_matches(
        &self,
        path: &Path,
        expected: &File,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.remove_bound_file_if_matches_with_hook_after_identity(
            path,
            expected,
            before_delete,
            || Ok(()),
        )
    }

    fn remove_bound_file_if_matches_with_hook_after_identity(
        &self,
        path: &Path,
        expected: &File,
        before_delete: impl FnOnce() -> Result<(), String>,
        after_identity_check: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.verify_file_identity(path, expected)?;
        before_delete()?;
        #[cfg(windows)]
        let path_bound = {
            let path_bound = self.open_read(path)?;
            if !same_open_file_identity(&path_bound, expected)? {
                return Err(format!(
                    "state file changed before its path-bound deletion handle was retained: {}",
                    path.display()
                ));
            }
            path_bound
        };
        #[cfg(not(windows))]
        self.verify_file_identity(path, expected)?;
        after_identity_check()?;
        #[cfg(windows)]
        delete_open_file_platform(&path_bound).map_err(|error| {
            format!(
                "failed to remove the expected open state file {}: {error}",
                path.display()
            )
        })?;
        #[cfg(not(windows))]
        self.remove_file_bound_without_sync(path)?;
        self.sync_directory()
    }

    pub(crate) fn for_each_entry_bounded(
        &self,
        max_entries: usize,
        max_name_bytes: usize,
        mut visit: impl FnMut(OsString) -> Result<(), String>,
    ) -> Result<(), String> {
        self.verify_visible()?;
        let mut entries = 0_usize;
        let mut name_bytes = 0_usize;
        for entry in self
            .directory
            .entries()
            .map_err(|error| format!("failed to list {}: {error}", self.path.display()))?
        {
            let name = entry
                .map_err(|error| format!("failed to list {}: {error}", self.path.display()))?
                .file_name();
            entries = entries
                .checked_add(1)
                .ok_or_else(|| "state directory entry count overflowed".to_string())?;
            name_bytes = name_bytes
                .checked_add(name.as_encoded_bytes().len())
                .ok_or_else(|| "state directory name byte count overflowed".to_string())?;
            if entries > max_entries || name_bytes > max_name_bytes {
                return Err(format!(
                    "state directory {} exceeds the bounded scan limit ({max_entries} entries, {max_name_bytes} filename bytes)",
                    self.path.display()
                ));
            }
            visit(name)?;
        }
        self.verify_visible()?;
        Ok(())
    }

    pub(crate) fn save_json_atomically_expected<T: Serialize>(
        &self,
        path: &Path,
        value: &T,
        expected: FileExpectation<'_>,
    ) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
        self.save_bytes_atomically_expected(path, &encoded, ".nib-daemon-", expected)
    }

    #[cfg(test)]
    pub(crate) fn save_bytes_atomically(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected(path, encoded, temporary_prefix, FileExpectation::Any)
    }

    pub(crate) fn save_bytes_atomically_expected(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_receipt(path, encoded, temporary_prefix, expected)
            .map(drop)
            .map_err(|error| error.message)
    }

    pub(crate) fn save_bytes_atomically_expected_with_receipt(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: expected,
                retain_publication_lock: false,
            },
            || Ok(()),
            || {},
        )
    }

    pub(crate) fn save_bytes_atomically_expected_with_locked_receipt(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: expected,
                retain_publication_lock: true,
            },
            || Ok(()),
            || {},
        )
    }

    #[cfg(test)]
    fn save_bytes_atomically_expected_with_locked_receipt_before_return(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
        before_receipt: impl FnOnce(),
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_all_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: expected,
                retain_publication_lock: true,
            },
            AtomicSaveHooks {
                before_commit: || Ok(()),
                after_evacuation: || {},
                before_receipt,
            },
        )
    }

    #[cfg(all(test, unix))]
    pub(crate) fn save_bytes_atomically_after_effects(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
    ) -> Result<(), String> {
        self.save_bytes_atomically_after_effects_expected(
            path,
            encoded,
            temporary_prefix,
            FileExpectation::Any,
        )
    }

    #[cfg(all(test, unix))]
    pub(crate) fn save_bytes_atomically_after_effects_expected(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hook(
            path,
            encoded,
            temporary_prefix,
            false,
            expected,
            || Ok(()),
        )
    }

    #[cfg(test)]
    pub(crate) fn save_bytes_atomically_with_hook(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        require_attached_before_commit: bool,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hook(
            path,
            encoded,
            temporary_prefix,
            require_attached_before_commit,
            FileExpectation::Any,
            before_commit,
        )
    }

    pub(crate) fn save_bytes_atomically_expected_with_hook(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        require_attached_before_commit: bool,
        expected: FileExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit,
                file: expected,
                retain_publication_lock: false,
            },
            before_commit,
            || {},
        )
        .map(drop)
        .map_err(|error| error.message)
    }

    #[cfg(all(test, unix))]
    fn save_bytes_atomically_expected_with_recovery_hooks(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expectation: AtomicSaveExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
        after_evacuation: impl FnOnce(),
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            expectation,
            before_commit,
            after_evacuation,
        )
        .map(drop)
        .map_err(|error| error.message)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn save_bytes_atomically_expected_with_after_evacuation_hook(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expected: FileExpectation<'_>,
        after_evacuation: impl FnOnce(),
    ) -> Result<(), String> {
        self.save_bytes_atomically_expected_with_hooks(
            path,
            encoded,
            temporary_prefix,
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: expected,
                retain_publication_lock: false,
            },
            || Ok(()),
            after_evacuation,
        )
        .map(drop)
        .map_err(|error| error.message)
    }

    fn save_bytes_atomically_expected_with_hooks(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expectation: AtomicSaveExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
        after_evacuation: impl FnOnce(),
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        self.save_bytes_atomically_expected_with_all_hooks(
            path,
            encoded,
            temporary_prefix,
            expectation,
            AtomicSaveHooks {
                before_commit,
                after_evacuation,
                before_receipt: || {},
            },
        )
    }

    fn save_bytes_atomically_expected_with_all_hooks(
        &self,
        path: &Path,
        encoded: &[u8],
        temporary_prefix: &str,
        expectation: AtomicSaveExpectation<'_>,
        hooks: AtomicSaveHooks<impl FnOnce() -> Result<(), String>, impl FnOnce(), impl FnOnce()>,
    ) -> Result<FilePublicationReceipt, FilePublicationError> {
        let AtomicSaveExpectation {
            require_attached_before_commit,
            file: expected,
            retain_publication_lock,
        } = expectation;
        let AtomicSaveHooks {
            before_commit,
            after_evacuation,
            before_receipt,
        } = hooks;
        let destination = self.relative_file(path)?.to_path_buf();
        let temporary = deterministic_artifact_name(
            temporary_prefix,
            destination.as_os_str().as_encoded_bytes(),
            ".tmp",
        );
        let temporary_path = self.path.join(&temporary);
        let previous =
            deterministic_previous_artifact_name(temporary_prefix, destination.as_os_str())?;
        let previous_path = self.path.join(&previous);
        self.recover_atomic_transaction(path, &temporary_path, &previous_path, false, false)?;

        #[cfg(test)]
        let any_expected_file = match expected {
            FileExpectation::Any if self.path_exists(path)? => Some(self.open_read(path)?),
            _ => None,
        };
        let expected = match expected {
            #[cfg(test)]
            FileExpectation::Any if any_expected_file.is_some() => {
                FileExpectation::Present(any_expected_file.as_ref().expect("opened expected file"))
            }
            #[cfg(test)]
            FileExpectation::Any => FileExpectation::Missing,
            expectation => expectation,
        };
        self.verify_file_expectation(path, expected)?;

        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        configure_capability_owner_only(&mut options);
        configure_capability_no_follow(&mut options);
        configure_capability_delete_access(&mut options, true, true);
        let mut file = self
            .directory
            .open_with(&temporary, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to create temporary state file: {error}"))?;
        file.lock()
            .map_err(|error| format!("failed to lock temporary state file: {error}"))?;
        let mut evacuated_previous = None;
        let mut exact_publication_committed = false;
        let write_result = (|| {
            file.write_all(encoded).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            let before_commit_attachment_error = self.verify_visible().err();
            if require_attached_before_commit {
                if let Some(error) = before_commit_attachment_error.as_ref() {
                    return Err(error.clone());
                }
            }
            self.verify_file_expectation(path, expected)?;
            before_commit()?;
            self.verify_file_identity(&temporary_path, &file)?;

            if let FileExpectation::Present(previous_file) = expected {
                if let Err(error) =
                    self.move_open_file_no_replace_bound(path, previous_file, &previous_path)
                {
                    if let Ok(opened_previous) = self.open_read(&previous_path) {
                        let _ = self.rollback_previous_file(path, &previous_path, &opened_previous);
                    }
                    return Err(error);
                }
                evacuated_previous = Some(self.open_read(&previous_path)?);
            } else {
                self.verify_file_expectation(path, FileExpectation::Missing)?;
            }
            after_evacuation();

            let publication = match self.publish_open_file_no_replace(&temporary_path, &file, path)
            {
                Ok(publication) => publication,
                Err(error) => {
                    if let Some(previous_file) = evacuated_previous.as_ref() {
                        let rollback =
                            self.rollback_previous_file(path, &previous_path, previous_file);
                        return match rollback {
                            Ok(()) => Err(error),
                            Err(rollback_error) => Err(format!(
                                "{error}; failed to restore the prior state: {rollback_error}"
                            )),
                        };
                    }
                    return Err(error);
                }
            };
            exact_publication_committed = true;
            self.verify_published_file(path, &file, publication)?;
            let published = read_open_file_prefix(&file, encoded.len().saturating_add(1))
                .map_err(|error| format!("failed to verify published state: {error}"))?;
            if published != encoded {
                return Err(format!(
                    "published state did not retain the requested bytes: {}",
                    path.display()
                ));
            }
            let post_publication = (|| {
                self.sync_directory()?;
                if let Some(previous_file) = evacuated_previous.as_ref() {
                    self.remove_bound_file_if_matches(&previous_path, previous_file, || {
                        self.verify_publication_bytes(path, &file, encoded)
                    })?;
                }
                self.verify_publication_bytes(path, &file, encoded)?;
                let after_commit_attachment_error = self.verify_visible().err();
                match (
                    before_commit_attachment_error,
                    after_commit_attachment_error,
                ) {
                    (Some(error), _) | (None, Some(error)) => Err(error),
                    (None, None) => Ok(()),
                }
            })();
            match post_publication {
                Ok(()) => Ok(()),
                Err(error) => Err(error),
            }
        })();
        let temporary_cleanup = self
            .cleanup_open_temporary_file(&temporary_path, &file, || {
                if exact_publication_committed {
                    self.verify_publication_bytes(path, &file, encoded)
                } else {
                    Ok(())
                }
            })
            .and_then(|()| {
                if exact_publication_committed {
                    self.verify_publication_bytes(path, &file, encoded)
                } else {
                    Ok(())
                }
            });
        let unlock_result = if retain_publication_lock && exact_publication_committed {
            Ok(())
        } else {
            file.unlock()
                .map_err(|error| format!("failed to unlock published state file: {error}"))
        };
        before_receipt();
        let temporary_cleanup = match (temporary_cleanup, unlock_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(cleanup_error), Err(unlock_error)) => {
                Err(format!("{cleanup_error}; {unlock_error}"))
            }
        };
        match (write_result, temporary_cleanup) {
            (Ok(()), Ok(())) => Ok(FilePublicationReceipt {
                file,
                exact_identity: true,
            }),
            (Err(message), Ok(())) => {
                let receipt = exact_publication_committed.then_some(FilePublicationReceipt {
                    file,
                    exact_identity: true,
                });
                Err(FilePublicationError { message, receipt })
            }
            (Ok(()), Err(message)) => Err(FilePublicationError {
                message,
                receipt: Some(FilePublicationReceipt {
                    file,
                    exact_identity: true,
                }),
            }),
            (Err(message), Err(cleanup_error)) => {
                let receipt = exact_publication_committed.then_some(FilePublicationReceipt {
                    file,
                    exact_identity: true,
                });
                Err(FilePublicationError {
                    message: format!(
                        "{message}; temporary state cleanup also failed: {cleanup_error}"
                    ),
                    receipt,
                })
            }
        }
    }

    pub(crate) fn recover_stale_temporary_files(
        &self,
        temporary_prefix: &str,
        max_entries: usize,
        max_name_bytes: usize,
    ) -> Result<usize, String> {
        self.recover_stale_temporary_files_with_policy(
            temporary_prefix,
            max_entries,
            max_name_bytes,
            false,
        )
    }

    pub(crate) fn recover_stale_temporary_files_strict(
        &self,
        temporary_prefix: &str,
        max_entries: usize,
        max_name_bytes: usize,
    ) -> Result<usize, String> {
        self.recover_stale_temporary_files_with_policy(
            temporary_prefix,
            max_entries,
            max_name_bytes,
            true,
        )
    }

    fn recover_stale_temporary_files_with_policy(
        &self,
        temporary_prefix: &str,
        max_entries: usize,
        max_name_bytes: usize,
        reject_obscured_live_writer: bool,
    ) -> Result<usize, String> {
        let mut previous_targets = Vec::new();
        let mut temporary_names = Vec::new();
        self.for_each_entry_bounded(max_entries, max_name_bytes, |name| {
            if let Some(target) = parse_previous_artifact_name(&name, temporary_prefix) {
                previous_targets.push(target);
            } else if is_deterministic_artifact_name(&name, temporary_prefix, ".tmp") {
                temporary_names.push(name);
            }
            Ok(())
        })?;

        let mut removed = 0_usize;
        for target_name in previous_targets {
            let target = self.path.join(&target_name);
            let temporary = self.path.join(deterministic_artifact_name(
                temporary_prefix,
                target_name.as_encoded_bytes(),
                ".tmp",
            ));
            let previous = self.path.join(deterministic_previous_artifact_name(
                temporary_prefix,
                &target_name,
            )?);
            if self.recover_atomic_transaction(
                &target,
                &temporary,
                &previous,
                true,
                reject_obscured_live_writer,
            )? {
                removed = removed.saturating_add(1);
            }
        }

        for name in temporary_names {
            let path = self.path.join(&name);
            if !self.path_exists(&path)? {
                continue;
            }
            let file = match self.open_read_write(&path) {
                Ok(file) => file,
                Err(_) if !self.path_exists(&path)? => continue,
                Err(error) => return Err(error),
            };
            match file.try_lock() {
                Ok(()) => {
                    self.remove_visible_file_if_matches(&path, &file, || Ok(()))?;
                    removed = removed.saturating_add(1);
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to inspect temporary state ownership {}: {error}",
                        path.display()
                    ))
                }
            }
        }
        Ok(removed)
    }

    pub(crate) fn is_atomic_transaction_artifact_name(
        name: &std::ffi::OsStr,
        temporary_prefix: &str,
    ) -> bool {
        parse_previous_artifact_name(name, temporary_prefix).is_some()
            || is_deterministic_artifact_name(name, temporary_prefix, ".tmp")
    }

    pub(crate) fn atomic_previous_target_name(
        name: &std::ffi::OsStr,
        temporary_prefix: &str,
    ) -> Option<OsString> {
        parse_previous_artifact_name(name, temporary_prefix)
    }

    fn recover_atomic_transaction(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        skip_live_writer: bool,
        reject_obscured_live_writer: bool,
    ) -> Result<bool, String> {
        self.recover_atomic_transaction_with_live_target_hook(
            target,
            temporary,
            previous,
            skip_live_writer,
            reject_obscured_live_writer,
            &mut || {},
        )
    }

    fn recover_atomic_transaction_with_live_target_hook(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        skip_live_writer: bool,
        reject_obscured_live_writer: bool,
        live_target_hook: &mut impl FnMut(),
    ) -> Result<bool, String> {
        let mut previous_open_hook = || Ok(());
        self.recover_atomic_transaction_with_hooks(
            target,
            temporary,
            previous,
            skip_live_writer,
            reject_obscured_live_writer,
            AtomicRecoveryHooks {
                previous_open: &mut previous_open_hook,
                live_target: live_target_hook,
            },
        )
    }

    fn recover_atomic_transaction_with_hooks(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        skip_live_writer: bool,
        reject_obscured_live_writer: bool,
        mut hooks: AtomicRecoveryHooks<'_>,
    ) -> Result<bool, String> {
        const MAX_NAMESPACE_RETRIES: usize = 8;

        let policy = AtomicRecoveryPolicy {
            skip_live_writer,
            reject_obscured_live_writer,
            strict_deadline: reject_obscured_live_writer
                .then(|| Instant::now() + STRICT_RECOVERY_LIVE_WRITER_WAIT),
        };
        for attempt in 0..MAX_NAMESPACE_RETRIES {
            let observed = (
                self.path_exists(target)?,
                self.path_exists(temporary)?,
                self.path_exists(previous)?,
            );
            match self
                .recover_atomic_transaction_once(target, temporary, previous, policy, &mut hooks)
            {
                Ok(recovered) => return Ok(recovered),
                Err(error) => {
                    let current = (
                        self.path_exists(target)?,
                        self.path_exists(temporary)?,
                        self.path_exists(previous)?,
                    );
                    if current == observed {
                        return Err(error);
                    }
                    if attempt + 1 == MAX_NAMESPACE_RETRIES {
                        return Err(format!(
                            "atomic state namespace changed repeatedly while recovering {}: {error}",
                            target.display()
                        ));
                    }
                }
            }
        }
        unreachable!("bounded atomic recovery loop always returns")
    }

    fn recover_atomic_transaction_once(
        &self,
        target: &Path,
        temporary: &Path,
        previous: &Path,
        policy: AtomicRecoveryPolicy,
        hooks: &mut AtomicRecoveryHooks<'_>,
    ) -> Result<bool, String> {
        let AtomicRecoveryPolicy {
            skip_live_writer,
            reject_obscured_live_writer,
            strict_deadline,
        } = policy;
        let temporary_file = if self.path_exists(temporary)? {
            let file = self.open_read_write(temporary)?;
            match file.try_lock() {
                Ok(()) => Some(file),
                Err(std::fs::TryLockError::WouldBlock) if skip_live_writer => {
                    if !reject_obscured_live_writer
                        || (!self.path_exists(target)? && !self.path_exists(previous)?)
                    {
                        return Ok(false);
                    }

                    let deadline = strict_deadline
                        .expect("strict live-writer rejection carries a recovery deadline");
                    loop {
                        let now = Instant::now();
                        if now >= deadline {
                            return Err(format!(
                                "atomic state transaction is still owned by a live writer and may obscure its target: {}",
                                target.display()
                            ));
                        }
                        thread::sleep(STRICT_RECOVERY_POLL_INTERVAL.min(deadline - now));
                        match file.try_lock() {
                            Ok(()) => break Some(file),
                            Err(std::fs::TryLockError::WouldBlock) => {}
                            Err(std::fs::TryLockError::Error(error)) => {
                                return Err(format!(
                                    "failed while waiting for temporary state ownership {}: {error}",
                                    temporary.display()
                                ));
                            }
                        }
                    }
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "temporary state file is still owned by a live writer: {}",
                        temporary.display()
                    ))
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to inspect temporary state ownership {}: {error}",
                        temporary.display()
                    ))
                }
            }
        } else {
            None
        };

        let mut recovered = false;
        if self.path_exists(previous)? {
            (hooks.previous_open)()?;
            let previous_file = self.open_read_write(previous)?;
            if self.path_exists(target)? {
                let target_file = self.open_read_write(target)?;
                if same_open_file_identity(&target_file, &previous_file)? {
                    self.remove_visible_file_if_matches(previous, &previous_file, || Ok(()))?;
                    recovered = true;
                } else {
                    match target_file.try_lock() {
                        Ok(()) => {
                            self.verify_file_identity(target, &target_file)?;
                            if !self.path_exists(previous)? {
                                return Ok(false);
                            }
                            self.verify_file_identity(previous, &previous_file)?;
                        }
                        Err(std::fs::TryLockError::WouldBlock) if skip_live_writer => {
                            (hooks.live_target)();
                            if !reject_obscured_live_writer {
                                return Ok(false);
                            }

                            let deadline = strict_deadline
                                .expect("strict live-writer rejection carries a recovery deadline");
                            loop {
                                let now = Instant::now();
                                if now >= deadline {
                                    return Err(format!(
                                        "atomic state target is still owned by a live writer: {}",
                                        target.display()
                                    ));
                                }
                                thread::sleep(STRICT_RECOVERY_POLL_INTERVAL.min(deadline - now));
                                match target_file.try_lock() {
                                    Ok(()) => break,
                                    Err(std::fs::TryLockError::WouldBlock) => {}
                                    Err(std::fs::TryLockError::Error(error)) => {
                                        return Err(format!(
                                            "failed while waiting for published state ownership {}: {error}",
                                            target.display()
                                        ));
                                    }
                                }
                            }

                            self.verify_file_identity(target, &target_file)?;
                            if !self.path_exists(previous)? {
                                return Ok(false);
                            }
                            self.verify_file_identity(previous, &previous_file)?;
                        }
                        Err(std::fs::TryLockError::WouldBlock) => {
                            return Err(format!(
                                "atomic state target is still owned by a live writer: {}",
                                target.display()
                            ));
                        }
                        Err(std::fs::TryLockError::Error(error)) => {
                            return Err(format!(
                                "failed to inspect published state ownership {}: {error}",
                                target.display()
                            ));
                        }
                    }
                    return Err(format!(
                        "atomic state target and an ambiguous prior artifact both exist; both were preserved: {}",
                        target.display()
                    ));
                }
            } else {
                return Err(format!(
                    "atomic state target is missing while an unproven prior artifact exists; the artifact was preserved: {}",
                    target.display()
                ));
            }
        }

        if let Some(file) = temporary_file {
            if self.path_exists(temporary)? {
                match self.verify_file_identity(temporary, &file) {
                    Ok(()) => {
                        self.remove_visible_file_if_matches(temporary, &file, || Ok(()))?;
                        recovered = true;
                    }
                    Err(error) => {
                        return Err(format!(
                            "temporary state path was substituted during recovery and was preserved: {error}"
                        ))
                    }
                }
            }
        }
        Ok(recovered)
    }

    fn rollback_previous_file(
        &self,
        target: &Path,
        previous: &Path,
        expected: &File,
    ) -> Result<(), String> {
        if !self.path_exists(previous)? {
            return Ok(());
        }
        self.verify_file_identity(previous, expected)?;
        if self.path_exists(target)? {
            let visible_target = self.open_read(target)?;
            match same_open_file_identity(&visible_target, expected) {
                Ok(true) => self.remove_bound_file_if_matches(previous, expected, || Ok(())),
                Ok(false) => Err(format!(
                    "atomic state target and evacuated prior state are identity-distinct; both were preserved as ambiguous: {} and {}",
                    target.display(),
                    previous.display()
                )),
                Err(error) => Err(format!(
                    "atomic state target identity could not be compared with the evacuated prior state; both were preserved: {error}"
                )),
            }
        } else {
            self.move_open_file_no_replace_bound(previous, expected, target)
        }
    }

    fn cleanup_open_temporary_file(
        &self,
        path: &Path,
        expected: &File,
        before_delete: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.path_exists(path)? {
            return Ok(());
        }
        match self.verify_file_identity(path, expected) {
            Ok(()) => self.remove_bound_file_if_matches(path, expected, before_delete),
            Err(error) => Err(format!(
                "temporary state path was substituted and was preserved: {error}"
            )),
        }
    }

    pub(crate) fn sync_directory(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true);
            self.directory
                .open_with(".", &options)
                .map(cap_std::fs::File::into_std)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| format!("failed to sync {}: {error}", self.path.display()))
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    }

    pub(crate) fn finalize_failed_exact_publication(
        &self,
        path: &Path,
        previous_expected: Option<&File>,
        receipt: &FilePublicationReceipt,
        temporary_prefix: &str,
        expected_bytes: &[u8],
    ) -> Result<(), String> {
        if !receipt.exact_identity {
            return Err("state publication does not carry an exact identity receipt".to_string());
        }
        let destination = self.relative_file(path)?;
        let temporary = self.path.join(deterministic_artifact_name(
            temporary_prefix,
            destination.as_os_str().as_encoded_bytes(),
            ".tmp",
        ));
        let previous = self.path.join(deterministic_previous_artifact_name(
            temporary_prefix,
            destination.as_os_str(),
        )?);

        self.verify_visible()?;
        self.verify_exact_publication_bytes(path, receipt, expected_bytes)?;
        self.sync_directory()?;
        if self.path_exists(&previous)? {
            let previous_expected = previous_expected.ok_or_else(|| {
                format!(
                    "unexpected prior-state artifact was preserved while finalizing {}",
                    path.display()
                )
            })?;
            self.verify_file_identity(&previous, previous_expected)?;
            self.remove_bound_file_if_matches(&previous, previous_expected, || {
                self.verify_exact_publication_bytes(path, receipt, expected_bytes)
            })?;
        }
        if self.path_exists(&temporary)? {
            self.verify_file_identity(&temporary, &receipt.file)?;
            self.remove_bound_file_if_matches(&temporary, &receipt.file, || {
                self.verify_exact_publication_bytes(path, receipt, expected_bytes)
            })?;
        }
        self.sync_directory()?;
        self.verify_exact_publication_bytes(path, receipt, expected_bytes)?;
        self.verify_visible()
    }

    fn verify_exact_publication_bytes(
        &self,
        path: &Path,
        receipt: &FilePublicationReceipt,
        expected_bytes: &[u8],
    ) -> Result<(), String> {
        self.verify_publication_bytes(path, &receipt.file, expected_bytes)
    }

    fn verify_publication_bytes(
        &self,
        path: &Path,
        expected_file: &File,
        expected_bytes: &[u8],
    ) -> Result<(), String> {
        self.verify_file_identity(path, expected_file)?;
        let actual = read_open_file_prefix(expected_file, expected_bytes.len().saturating_add(1))
            .map_err(|error| {
            format!(
                "failed to read state publication while finalizing {}: {error}",
                path.display()
            )
        })?;
        if actual != expected_bytes {
            return Err(format!(
                "state publication bytes changed while finalizing {}",
                path.display()
            ));
        }
        self.verify_file_identity(path, expected_file)
    }

    pub(crate) fn verify_file_identity(&self, path: &Path, file: &File) -> Result<(), String> {
        let expected = crate::fs_security::FileIdentity::from_file(
            file.try_clone()
                .map_err(|error| format!("failed to clone {}: {error}", path.display()))?,
        )
        .map_err(|error| format!("failed to identify {}: {error}", path.display()))?;
        let reopened = self.open_read_unchecked(path)?;
        let actual = crate::fs_security::FileIdentity::from_file(reopened)
            .map_err(|error| format!("failed to identify {}: {error}", path.display()))?;
        if actual != expected {
            return Err(format!(
                "state file identity changed while it was in use: {}",
                path.display()
            ));
        }
        Ok(())
    }

    fn open_read_unchecked(&self, path: &Path) -> Result<File, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        configure_capability_no_follow(&mut options);
        let file = self
            .directory
            .open_with(relative, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to re-open {}: {error}", path.display()))?;
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        Ok(file)
    }

    pub(crate) fn deterministic_artifact_path(
        &self,
        path: &Path,
        prefix: &str,
        suffix: &str,
    ) -> Result<PathBuf, String> {
        let relative = self.relative_file(path)?;
        Ok(self.path.join(deterministic_artifact_name(
            prefix,
            relative.as_os_str().as_encoded_bytes(),
            suffix,
        )))
    }

    pub(crate) fn deterministic_previous_artifact_path(
        &self,
        path: &Path,
        prefix: &str,
    ) -> Result<PathBuf, String> {
        let relative = self.relative_file(path)?;
        Ok(self.path.join(deterministic_previous_artifact_name(
            prefix,
            relative.as_os_str(),
        )?))
    }

    pub(crate) fn restore_visible_file_no_replace_if_matches(
        &self,
        source: &Path,
        expected: &File,
        destination: &Path,
    ) -> Result<(), String> {
        self.move_open_file_no_replace_bound(source, expected, destination)
    }

    fn relative_file<'a>(&self, path: &'a Path) -> Result<&'a Path, String> {
        if path.parent() != Some(self.path.as_path()) || path.file_name().is_none() {
            return Err(format!(
                "state path is not a direct child of the opened directory: {}",
                path.display()
            ));
        }
        Ok(Path::new(path.file_name().expect("checked file name")))
    }
}

fn deterministic_artifact_name(prefix: &str, destination: &[u8], suffix: &str) -> OsString {
    let digest = Sha256::digest(destination);
    let mut name = String::with_capacity(prefix.len() + 32 + suffix.len());
    name.push_str(prefix);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(suffix);
    OsString::from(name)
}

fn deterministic_previous_artifact_name(
    prefix: &str,
    destination: &std::ffi::OsStr,
) -> Result<OsString, String> {
    let destination = destination.to_str().ok_or_else(|| {
        "atomic state filenames must be valid UTF-8 so recovery can identify their target"
            .to_string()
    })?;
    let digest = Sha256::digest(destination.as_bytes());
    let mut name = String::with_capacity(prefix.len() + 32 + 10 + destination.len());
    name.push_str(prefix);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(".previous-");
    name.push_str(destination);
    Ok(OsString::from(name))
}

fn parse_previous_artifact_name(name: &std::ffi::OsStr, prefix: &str) -> Option<OsString> {
    let name = name.to_str()?;
    let remainder = name.strip_prefix(prefix)?;
    let (digest, target) = remainder.split_once(".previous-")?;
    if digest.len() != 32
        || !digest
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    let mut components = Path::new(target).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
    {
        return None;
    }
    let expected =
        deterministic_previous_artifact_name(prefix, std::ffi::OsStr::new(target)).ok()?;
    (expected == name).then(|| OsString::from(target))
}

fn is_deterministic_artifact_name(name: &std::ffi::OsStr, prefix: &str, suffix: &str) -> bool {
    let bytes = name.as_encoded_bytes();
    let prefix = prefix.as_bytes();
    let suffix = suffix.as_bytes();
    let Some(expected_length) = prefix
        .len()
        .checked_add(32)
        .and_then(|length| length.checked_add(suffix.len()))
    else {
        return false;
    };
    bytes.len() == expected_length
        && bytes.starts_with(prefix)
        && bytes.ends_with(suffix)
        && bytes[prefix.len()..prefix.len() + 32]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(windows)]
fn delete_open_file_platform(opened: &File) -> std::io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfoEx, SetFileInformationByHandle, FILE_DISPOSITION_FLAG_DELETE,
        FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
        FILE_DISPOSITION_INFO_EX,
    };

    let extended = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    let deleted = unsafe {
        SetFileInformationByHandle(
            opened.as_raw_handle(),
            FileDispositionInfoEx,
            std::ptr::from_ref(&extended).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if deleted != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    Err(std::io::Error::new(
        error.kind(),
        format!(
            "POSIX handle deletion is unavailable; legacy deferred deletion is not safe for retained ownership handles: {error}"
        ),
    ))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn rename_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn rename_file_no_replace_platform(
    _directory: &cap_std::fs::Dir,
    _source: &Path,
    _destination: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no no-replace file relocation primitive",
    ))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn publish_open_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    _source: &Path,
    source_file: &File,
    destination: &Path,
) -> std::io::Result<HandlePublication> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state filename contains an interior NUL byte",
        )
    })?;
    let empty = c"";
    // AT_EMPTY_PATH binds the new name to the retained file description rather than a mutable
    // source pathname. The destination is relative to the retained directory capability.
    let result = unsafe {
        libc::linkat(
            source_file.as_raw_fd(),
            empty.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if result == 0 {
        return Ok(HandlePublication::Linked);
    }
    let empty_path_error = std::io::Error::last_os_error();
    let fd_path = CString::new(format!("/proc/self/fd/{}", source_file.as_raw_fd()))
        .expect("numeric file descriptor path has no NUL bytes");
    let fallback = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            fd_path.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if fallback == 0 {
        Ok(HandlePublication::Linked)
    } else {
        let fallback_error = std::io::Error::last_os_error();
        Err(std::io::Error::new(
            fallback_error.kind(),
            format!(
                "AT_EMPTY_PATH failed ({empty_path_error}); /proc/self/fd publication failed ({fallback_error})"
            ),
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_open_directory_no_replace_platform(
    parent: &cap_std::fs::Dir,
    source: &Path,
    _source_directory: &cap_std::fs::Dir,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_open_directory_no_replace_platform(
    parent: &cap_std::fs::Dir,
    _source: &Path,
    source_directory: &cap_std::fs::Dir,
    destination: &Path,
) -> std::io::Result<()> {
    crate::fs_security::rename_open_entry_no_replace_windows(parent, source_directory, destination)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn rename_open_directory_no_replace_platform(
    parent: &cap_std::fs::Dir,
    source: &Path,
    _source_directory: &cap_std::fs::Dir,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source contains a NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(
    not(windows),
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn rename_open_directory_no_replace_platform(
    _parent: &cap_std::fs::Dir,
    _source: &Path,
    _source_directory: &cap_std::fs::Dir,
    _destination: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no no-replace directory quarantine primitive",
    ))
}

#[cfg(windows)]
fn publish_open_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    _source: &Path,
    source_file: &File,
    destination: &Path,
) -> std::io::Result<HandlePublication> {
    crate::fs_security::rename_open_entry_no_replace_windows(directory, source_file, destination)?;
    Ok(HandlePublication::Moved)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn publish_open_file_no_replace_platform(
    directory: &cap_std::fs::Dir,
    source: &Path,
    _source_file: &File,
    destination: &Path,
) -> std::io::Result<HandlePublication> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state filename contains an interior NUL byte",
        )
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state filename contains an interior NUL byte",
        )
    })?;
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(HandlePublication::Moved)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn publish_open_file_no_replace_platform(
    _directory: &cap_std::fs::Dir,
    _source: &Path,
    _source_file: &File,
    _destination: &Path,
) -> std::io::Result<HandlePublication> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no handle-bound no-replace state publication primitive",
    ))
}

#[cfg(not(any(unix, windows)))]
fn publish_open_file_no_replace_platform(
    _directory: &cap_std::fs::Dir,
    _source: &Path,
    _source_file: &File,
    _destination: &Path,
) -> std::io::Result<HandlePublication> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no handle-bound no-replace state publication primitive",
    ))
}

pub(crate) fn same_open_file_identity(left: &File, right: &File) -> Result<bool, String> {
    let left = crate::fs_security::FileIdentity::from_file(
        left.try_clone()
            .map_err(|error| format!("failed to clone state file: {error}"))?,
    )
    .map_err(|error| format!("failed to identify state file: {error}"))?;
    let right = crate::fs_security::FileIdentity::from_file(
        right
            .try_clone()
            .map_err(|error| format!("failed to clone state file: {error}"))?,
    )
    .map_err(|error| format!("failed to identify state file: {error}"))?;
    Ok(left == right)
}

fn stable_directory_identity(
    directory: &cap_std::fs::Dir,
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(
        directory
            .try_clone()
            .map(cap_std::fs::Dir::into_std_file)
            .map_err(|error| format!("failed to clone directory {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("failed to identify directory {}: {error}", path.display()))
}

#[cfg(windows)]
fn windows_visible_directory_file(path: &Path) -> Result<File, String> {
    crate::fs_security::open_directory_observation_windows(path).map_err(|error| {
        format!(
            "failed to observe state directory {}: {error}",
            path.display()
        )
    })
}

#[cfg(windows)]
fn windows_visible_directory_identity(
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(windows_visible_directory_file(path)?)
        .map_err(|error| format!("failed to identify directory {}: {error}", path.display()))
}

pub(crate) fn read_open_file_prefix(file: &File, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = vec![0_u8; limit];
    let mut read_total = 0_usize;
    while read_total < limit {
        let offset = u64::try_from(read_total)
            .map_err(|_| std::io::Error::other("state read offset overflowed"))?;
        match read_open_file_at(file, &mut bytes[read_total..], offset) {
            Ok(0) => break,
            Ok(read) => read_total = read_total.saturating_add(read),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    bytes.truncate(read_total);
    Ok(bytes)
}

#[cfg(unix)]
fn read_open_file_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(windows)]
fn read_open_file_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_open_file_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.read(buffer)
}

fn configure_capability_owner_only(_options: &mut cap_std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        _options.mode(0o600);
    }
}

fn configure_capability_no_follow(options: &mut cap_std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn configure_capability_delete_access(
    options: &mut cap_std::fs::OpenOptions,
    read: bool,
    write: bool,
) {
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::DELETE;

        let mut access = DELETE;
        if read {
            access |= GENERIC_READ;
        }
        if write {
            access |= GENERIC_WRITE;
        }
        options.access_mode(access);
    }
    #[cfg(not(windows))]
    {
        let _ = (options, read, write);
    }
}

fn validate_stable_file_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if crate::fs_security::metadata_is_link_or_reparse(metadata) || !metadata.is_file() {
        return Err(format!(
            "state file must be a regular local file and must not be a symlink or reparse point: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_capability_file_metadata(
    path: &Path,
    metadata: &cap_std::fs::Metadata,
) -> Result<(), String> {
    if capability_file_metadata_is_link(metadata) || !metadata.is_file() {
        return Err(format!(
            "state file must be a regular local file and must not be a symlink or reparse point: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn capability_file_metadata_is_link(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_std::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn capability_file_metadata_is_link(metadata: &cap_std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

pub(crate) fn with_file_lock<T>(
    lock_path: &Path,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    let protected_directory = lock_path
        .parent()
        .ok_or_else(|| format!("lock path has no parent: {}", lock_path.display()))?;
    with_file_lock_in(lock_path, protected_directory, operation)
}

pub(crate) struct HeldFileLock {
    _anchor_file: File,
    lock_directory: StableDirectory,
    lock_path: PathBuf,
    anchor_directory: StableDirectory,
    anchor_path: PathBuf,
    locked_identity: crate::fs_security::FileIdentity,
    protected_directory: StableDirectory,
}

impl HeldFileLock {
    pub(crate) fn verify(&self) -> Result<(), String> {
        self.lock_directory.verify_visible()?;
        self.anchor_directory.verify_visible()?;
        verify_daemon_lock_paths_bound(
            &self.lock_directory,
            &self.lock_path,
            &self.anchor_directory,
            &self.anchor_path,
            &self.locked_identity,
        )?;
        self.protected_directory.verify_visible()
    }
}

pub(crate) fn try_acquire_file_lock_in(
    lock_path: &Path,
    protected_directory: &Path,
) -> Result<HeldFileLock, String> {
    let parent = lock_path
        .parent()
        .ok_or_else(|| format!("lock path has no parent: {}", lock_path.display()))?;
    let parent = crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| error.to_string())?;
    let file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("daemon lock path has no file name: {}", lock_path.display()))?;
    let lock_path = parent.join(file_name);
    let anchor_path = daemon_lock_anchor_path(&lock_path)?;
    let anchor_parent = anchor_path.parent().ok_or_else(|| {
        format!(
            "daemon lock anchor has no parent: {}",
            anchor_path.display()
        )
    })?;
    crate::fs_security::ensure_directory_without_symlinks(anchor_parent)
        .map_err(|error| format!("daemon lock anchor directory is unsafe: {error}"))?;
    let lock_directory = StableDirectory::open(&parent)?;
    let anchor_directory = StableDirectory::open(anchor_parent)?;
    let protected_directory = StableDirectory::open(protected_directory)?;

    let anchor_file = open_daemon_lock_anchor_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
    )?;
    let locked_identity = daemon_lock_identity(&anchor_file, &anchor_path)?;
    match anchor_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(format!(
                "state lock is already held by another owner: {}",
                lock_path.display()
            ));
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(format!(
                "failed to acquire state lock {}: {error}",
                lock_path.display()
            ));
        }
    }

    lock_directory.verify_visible()?;
    anchor_directory.verify_visible()?;
    repair_daemon_lock_anchor(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
    )?;
    verify_daemon_lock_paths_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
    )?;
    protected_directory.verify_visible()?;

    Ok(HeldFileLock {
        _anchor_file: anchor_file,
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        locked_identity,
        protected_directory,
    })
}

pub(crate) fn with_file_lock_in<T>(
    lock_path: &Path,
    protected_directory: &Path,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    with_file_lock_in_with_deadline(lock_path, protected_directory, None, operation)
}

pub(crate) fn with_file_lock_in_until<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Instant,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    with_file_lock_in_with_deadline(lock_path, protected_directory, Some(deadline), operation)
}

fn with_file_lock_in_with_deadline<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Option<Instant>,
    operation: impl FnOnce(&StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    let parent = lock_path
        .parent()
        .ok_or_else(|| format!("lock path has no parent: {}", lock_path.display()))?;
    let parent = crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| error.to_string())?;
    let file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("daemon lock path has no file name: {}", lock_path.display()))?;
    let lock_path = parent.join(file_name);
    let anchor_path = daemon_lock_anchor_path(&lock_path)?;
    let anchor_parent = anchor_path.parent().ok_or_else(|| {
        format!(
            "daemon lock anchor has no parent: {}",
            anchor_path.display()
        )
    })?;
    crate::fs_security::ensure_directory_without_symlinks(anchor_parent)
        .map_err(|error| format!("daemon lock anchor directory is unsafe: {error}"))?;
    let lock_directory = StableDirectory::open(&parent)?;
    let anchor_directory = StableDirectory::open(anchor_parent)?;
    let protected_directory = StableDirectory::open(protected_directory)?;

    let anchor_file = open_daemon_lock_anchor_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
    )?;
    let locked_identity = daemon_lock_identity(&anchor_file, &anchor_path)?;
    lock_daemon_anchor(&anchor_file, &lock_path, deadline)?;

    lock_directory.verify_visible()?;
    anchor_directory.verify_visible()?;
    repair_daemon_lock_anchor(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
    )?;
    verify_daemon_lock_paths_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
    )?;
    protected_directory.verify_visible()?;
    let result = operation(&protected_directory);
    let attachment_result = (|| {
        lock_directory.verify_visible()?;
        anchor_directory.verify_visible()?;
        verify_daemon_lock_paths_bound(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
        )?;
        protected_directory.verify_visible()
    })();
    let outcome = match (result, attachment_result) {
        (_, Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    };
    if outcome.is_ok() {
        cleanup_daemon_lock_anchor(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
        )?;
    }
    outcome
}

fn lock_daemon_anchor(
    anchor_file: &File,
    lock_path: &Path,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let Some(deadline) = deadline else {
        return anchor_file.lock().map_err(|error| {
            format!(
                "failed to lock daemon state {}: {error}",
                lock_path.display()
            )
        });
    };

    loop {
        match anchor_file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(format!(
                        "timed out acquiring daemon state lock: {}",
                        lock_path.display()
                    ));
                }
                thread::sleep(FILE_LOCK_POLL_INTERVAL.min(deadline - now));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to lock daemon state {}: {error}",
                    lock_path.display()
                ));
            }
        }
    }
}

pub(crate) fn daemon_lock_anchor_path(lock_path: &Path) -> Result<PathBuf, String> {
    let visible_directory = lock_path.parent().ok_or_else(|| {
        format!(
            "daemon lock directory has no parent: {}",
            lock_path.display()
        )
    })?;
    let fallback_anchor_directory = visible_directory.parent().ok_or_else(|| {
        format!(
            "daemon lock directory has no persistent anchor parent: {}",
            visible_directory.display()
        )
    })?;
    let visible_directory_name = visible_directory.file_name().ok_or_else(|| {
        format!(
            "daemon lock directory has no file name: {}",
            visible_directory.display()
        )
    })?;
    let anchor_directory = if visible_directory_name == ".nib" {
        git_lock_anchor_directory(fallback_anchor_directory)
    } else {
        None
    }
    .unwrap_or_else(|| fallback_anchor_directory.to_path_buf());
    let lock_file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("daemon lock path has no file name: {}", lock_path.display()))?;

    let mut anchor_name = OsString::from(format!(
        ".nib-lock-{}-",
        visible_directory_name.as_encoded_bytes().len()
    ));
    anchor_name.push(visible_directory_name);
    anchor_name.push("-");
    anchor_name.push(lock_file_name);
    anchor_name.push(".anchor");
    Ok(anchor_directory.join(anchor_name))
}

fn git_lock_anchor_directory(project_root: &Path) -> Option<PathBuf> {
    let dot_git = project_root.join(".git");
    let metadata = fs::symlink_metadata(&dot_git).ok()?;
    if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return None;
    }
    Some(dot_git.join("nib").join("locks"))
}

#[cfg(any(unix, windows))]
pub(crate) fn cleanup_legacy_lock_pair(
    visible_directory: &StableDirectory,
    visible_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
) -> Result<(), String> {
    cleanup_legacy_lock_pair_with_hook(
        visible_directory,
        visible_path,
        anchor_directory,
        anchor_path,
        || Ok(()),
    )
}

#[cfg(any(unix, windows))]
fn cleanup_legacy_lock_pair_with_hook(
    visible_directory: &StableDirectory,
    visible_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    before_delete: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let visible_exists = visible_directory.path_exists(visible_path)?;
    let anchor_exists = anchor_directory.path_exists(anchor_path)?;
    if !visible_exists && !anchor_exists {
        return Ok(());
    }
    let (source_directory, source_path) = if anchor_exists {
        (anchor_directory, anchor_path)
    } else {
        (visible_directory, visible_path)
    };
    let file = source_directory.open_read_write(source_path)?;
    let identity = crate::fs_security::FileIdentity::from_file(
        file.try_clone()
            .map_err(|error| format!("failed to clone legacy lock: {error}"))?,
    )
    .map_err(|error| format!("failed to identify legacy lock: {error}"))?;
    for (exists, directory, path) in [
        (visible_exists, visible_directory, visible_path),
        (anchor_exists, anchor_directory, anchor_path),
    ] {
        if !exists {
            continue;
        }
        let probe = directory.open_read_write(path)?;
        let probe_identity = crate::fs_security::FileIdentity::from_file(probe)
            .map_err(|error| format!("failed to identify legacy lock: {error}"))?;
        if probe_identity != identity {
            return Err(format!(
                "legacy lock and anchor have different identities: {}",
                visible_path.display()
            ));
        }
    }
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(format!(
                "legacy lock is still owned and cannot be migrated: {}",
                visible_path.display()
            ))
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(format!(
                "failed to acquire legacy lock {}: {error}",
                visible_path.display()
            ))
        }
    }
    before_delete()?;
    for (exists, directory, path) in [
        (visible_exists, visible_directory, visible_path),
        (anchor_exists, anchor_directory, anchor_path),
    ] {
        if exists {
            directory.remove_file_if_matches(path, &file, ".nib-legacy-lock-delete-")?;
        }
    }
    visible_directory.verify_visible()?;
    anchor_directory.verify_visible()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn cleanup_legacy_lock_pair(
    _visible_directory: &StableDirectory,
    visible_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
) -> Result<(), String> {
    Err(format!(
        "legacy lock migration is unsupported on this platform: {}",
        visible_path.display()
    ))
}

#[cfg(any(unix, windows))]
fn open_daemon_lock_anchor_bound(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
) -> Result<File, String> {
    open_daemon_lock_anchor_bound_with_hook(
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        || Ok(()),
    )
}

#[cfg(any(unix, windows))]
fn open_daemon_lock_anchor_bound_with_hook(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    mut before_open: impl FnMut() -> Result<(), String>,
) -> Result<File, String> {
    const MAX_NAMESPACE_RETRIES: usize = 8;

    for attempt in 0..MAX_NAMESPACE_RETRIES {
        let observed = (
            lock_directory.path_exists(lock_path)?,
            anchor_directory.path_exists(anchor_path)?,
        );
        let opened = (|| {
            let (lock_exists, anchor_exists) = observed;
            if !anchor_exists {
                return if lock_exists {
                    lock_directory.open_read_write(lock_path)
                } else {
                    lock_directory.open_read_write_create(lock_path)
                };
            }

            before_open()?;
            let anchor_file = anchor_directory.open_read_write(anchor_path)?;
            if lock_exists {
                let visible = lock_directory.open_read_write(lock_path)?;
                let anchor_identity = daemon_lock_identity(&anchor_file, anchor_path)?;
                if daemon_lock_identity(&visible, lock_path)? != anchor_identity {
                    return Err(format!(
                        "daemon lock and persistent anchor have different identities: {}",
                        lock_path.display()
                    ));
                }
            }
            Ok(anchor_file)
        })();
        match opened {
            Ok(file) => return Ok(file),
            Err(error) => {
                let current = (
                    lock_directory.path_exists(lock_path)?,
                    anchor_directory.path_exists(anchor_path)?,
                );
                if current == observed {
                    return Err(error);
                }
                if attempt + 1 == MAX_NAMESPACE_RETRIES {
                    return Err(format!(
                        "daemon lock namespace changed repeatedly while opening {}: {error}",
                        lock_path.display()
                    ));
                }
            }
        }
    }
    unreachable!("bounded daemon lock open loop always returns")
}

#[cfg(not(any(unix, windows)))]
fn open_daemon_lock_anchor_bound(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
) -> Result<File, String> {
    Err(format!(
        "persistent daemon lock anchors are unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(all(test, any(unix, windows)))]
fn open_daemon_lock_file_with_hook(
    path: &Path,
    after_inspect: impl FnOnce() -> Result<(), String>,
) -> Result<File, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_daemon_lock_metadata(path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "failed to inspect daemon lock {}: {error}",
                path.display()
            ))
        }
    }
    after_inspect()?;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    configure_daemon_lock_no_follow(&mut options);
    let file = options
        .open(path)
        .map_err(|error| format!("failed to open daemon lock {}: {error}", path.display()))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect daemon lock {}: {error}", path.display()))?;
    let opened_metadata = file.metadata().map_err(|error| {
        format!(
            "failed to inspect open daemon lock {}: {error}",
            path.display()
        )
    })?;
    validate_daemon_lock_metadata(path, &path_metadata)?;
    validate_daemon_lock_metadata(path, &opened_metadata)?;
    let opened_identity = daemon_lock_identity(&file, path)?;
    let path_identity = open_daemon_lock_identity(path)?;
    if opened_identity != path_identity {
        return Err(format!(
            "daemon lock changed while it was opened: {}",
            path.display()
        ));
    }
    Ok(file)
}

#[cfg(all(test, any(unix, windows)))]
fn open_daemon_lock_identity(path: &Path) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(open_daemon_lock_probe(path)?)
        .map_err(|error| format!("failed to identify daemon lock {}: {error}", path.display()))
}

#[cfg(all(test, any(unix, windows)))]
fn open_daemon_lock_probe(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    configure_daemon_lock_no_follow(&mut options);
    let file = options
        .open(path)
        .map_err(|error| format!("failed to re-open daemon lock {}: {error}", path.display()))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect daemon lock {}: {error}", path.display()))?;
    let opened_metadata = file.metadata().map_err(|error| {
        format!(
            "failed to inspect re-opened daemon lock {}: {error}",
            path.display()
        )
    })?;
    validate_daemon_lock_metadata(path, &path_metadata)?;
    validate_daemon_lock_metadata(path, &opened_metadata)?;
    Ok(file)
}

#[cfg(all(test, any(unix, windows)))]
fn configure_daemon_lock_no_follow(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(all(test, any(unix, windows)))]
fn validate_daemon_lock_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if daemon_lock_metadata_is_link(metadata) || !metadata.is_file() {
        return Err(format!(
            "daemon lock must be a regular local file and must not be a symlink or reparse point: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(all(test, windows))]
fn daemon_lock_metadata_is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(all(test, unix))]
fn daemon_lock_metadata_is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(any(unix, windows))]
fn daemon_lock_identity(
    file: &File,
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(
        file.try_clone()
            .map_err(|error| format!("failed to clone daemon lock {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("failed to identify daemon lock {}: {error}", path.display()))
}

#[cfg(not(any(unix, windows)))]
fn daemon_lock_identity(_file: &File, path: &Path) -> Result<(), String> {
    Err(format!(
        "stable daemon lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
fn verify_daemon_lock_paths_bound(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    expected: &crate::fs_security::FileIdentity,
) -> Result<(), String> {
    for (directory, path) in [(lock_directory, lock_path), (anchor_directory, anchor_path)] {
        let probe = directory.open_read_write(path)?;
        if daemon_lock_identity(&probe, path)? != *expected {
            return Err(format!(
                "daemon lock and persistent anchor have different identities: {}",
                lock_path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn repair_daemon_lock_anchor(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    expected: &crate::fs_security::FileIdentity,
) -> Result<(), String> {
    let visible = lock_directory.open_read_write(lock_path)?;
    if daemon_lock_identity(&visible, lock_path)? != *expected {
        return Err(format!(
            "daemon lock visible path changed before anchor repair: {}",
            lock_path.display()
        ));
    }
    if !anchor_directory.path_exists(anchor_path)? {
        lock_directory.hard_link_to(lock_path, anchor_directory, anchor_path)?;
        anchor_directory.sync_directory()?;
    }
    verify_daemon_lock_paths_bound(
        lock_directory,
        lock_path,
        anchor_directory,
        anchor_path,
        expected,
    )
}

#[cfg(any(unix, windows))]
fn cleanup_daemon_lock_anchor(
    lock_directory: &StableDirectory,
    lock_path: &Path,
    anchor_directory: &StableDirectory,
    anchor_path: &Path,
    expected: &crate::fs_security::FileIdentity,
) -> Result<(), String> {
    let visible = lock_directory.open_read_write(lock_path)?;
    if daemon_lock_identity(&visible, lock_path)? != *expected {
        return Err(format!(
            "daemon lock visible path changed before anchor cleanup: {}",
            lock_path.display()
        ));
    }
    if anchor_directory.path_exists(anchor_path)? {
        let anchor = anchor_directory.open_read_write(anchor_path)?;
        if daemon_lock_identity(&anchor, anchor_path)? != *expected {
            return Err(format!(
                "daemon lock anchor changed before cleanup: {}",
                anchor_path.display()
            ));
        }
        anchor_directory.remove_file_if_matches(
            anchor_path,
            &anchor,
            ".nib-daemon-lock-anchor-delete-",
        )?;
    }
    lock_directory.verify_visible()?;
    anchor_directory.verify_visible()
}

#[cfg(not(any(unix, windows)))]
fn verify_daemon_lock_paths_bound(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _expected: &(),
) -> Result<(), String> {
    Err(format!(
        "stable daemon lock identity is unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(not(any(unix, windows)))]
fn repair_daemon_lock_anchor(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _expected: &(),
) -> Result<(), String> {
    Err(format!(
        "stable daemon lock anchor repair is unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(not(any(unix, windows)))]
fn cleanup_daemon_lock_anchor(
    _lock_directory: &StableDirectory,
    lock_path: &Path,
    _anchor_directory: &StableDirectory,
    _anchor_path: &Path,
    _expected: &(),
) -> Result<(), String> {
    Err(format!(
        "stable daemon lock anchor cleanup is unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    const LOCK_CHILD_PATH: &str = "NIB_DAEMON_LOCK_CHILD_PATH";
    const LOCK_CHILD_READY: &str = "NIB_DAEMON_LOCK_CHILD_READY";
    const LOCK_CHILD_ENTERED: &str = "NIB_DAEMON_LOCK_CHILD_ENTERED";
    const LOCK_CHILD_EXPECTATION: &str = "NIB_DAEMON_LOCK_CHILD_EXPECTATION";
    #[cfg(unix)]
    const ATOMIC_CRASH_CHILD_ROOT: &str = "NIB_ATOMIC_CRASH_CHILD_ROOT";
    #[cfg(unix)]
    const ATOMIC_CRASH_CHILD_MODE: &str = "NIB_ATOMIC_CRASH_CHILD_MODE";
    #[cfg(unix)]
    const ATOMIC_CRASH_CHILD_READY: &str = "NIB_ATOMIC_CRASH_CHILD_READY";

    #[test]
    fn atomic_transaction_artifact_names_are_recognized_exactly() {
        let prefix = ".nib-session-";
        let target = std::ffi::OsStr::new("session-id.json");
        let previous = deterministic_previous_artifact_name(prefix, target)
            .expect("previous transaction artifact name");
        let temporary = deterministic_artifact_name(prefix, target.as_encoded_bytes(), ".tmp");

        assert!(StableDirectory::is_atomic_transaction_artifact_name(
            &previous, prefix
        ));
        assert!(StableDirectory::is_atomic_transaction_artifact_name(
            &temporary, prefix
        ));
        assert!(!StableDirectory::is_atomic_transaction_artifact_name(
            std::ffi::OsStr::new("ordinary-session.json"),
            prefix,
        ));
        assert!(!StableDirectory::is_atomic_transaction_artifact_name(
            std::ffi::OsStr::new(
                ".nib-session-00000000000000000000000000000000.previous-session-id.json"
            ),
            prefix,
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_namespace_children_reopen_before_exclusive_deletion() {
        let root = tempdir().expect("tempdir");
        let child_path = root.path().join("child");
        fs::create_dir(&child_path).expect("child directory");
        let parent = StableDirectory::open(root.path()).expect("stable parent");
        let child = parent
            .open_child(&child_path)
            .expect("ordinary namespace child");
        let reopened = parent
            .open_child(&child_path)
            .expect("second ordinary namespace child");
        assert!(child.same_identity(&reopened));

        child.verify_visible().expect("first visible identity");
        child
            .verify_visible_at(&child_path)
            .expect("second visible identity");
        let receipt = child
            .directory_removal_receipt()
            .expect("share-compatible ownership receipt");

        drop(reopened);
        drop(child);
        let owned = parent
            .open_owned_child(&child_path)
            .expect("delete-capable child beside observation receipt");
        parent
            .remove_empty_child_directory_if_matches(&child_path, owned)
            .expect("handle-bound empty-directory deletion");
        assert!(!child_path.exists());
        let _retained_identity = receipt.identity();
    }

    #[cfg(windows)]
    #[test]
    fn windows_observation_receipt_supports_recursive_handle_bound_cleanup() {
        let root = tempdir().expect("tempdir");
        let tree_path = root.path().join("tree");
        let parent = StableDirectory::open(root.path()).expect("stable parent");
        let tree = parent
            .create_owned_child_directory(&tree_path)
            .expect("owned tree");
        fs::write(tree_path.join("payload"), b"owned").expect("tree payload");
        let receipt = tree
            .directory_removal_receipt()
            .expect("observation receipt");
        let retained_receipt = receipt.clone();
        drop(tree);

        crate::fs_security::remove_directory_tree_capability_bound_if_matches(
            root.path(),
            &tree_path,
            receipt,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("receipt-bound recursive cleanup");

        assert!(!tree_path.exists());
        let _retained_identity = retained_receipt.identity();
    }

    #[test]
    fn daemon_file_lock_replacement_child_process() {
        let Some(lock_path) = std::env::var_os(LOCK_CHILD_PATH) else {
            return;
        };
        let ready_path = PathBuf::from(
            std::env::var_os(LOCK_CHILD_READY).expect("child ready path must be configured"),
        );
        let entered_path = PathBuf::from(
            std::env::var_os(LOCK_CHILD_ENTERED).expect("child entered path must be configured"),
        );
        let expectation =
            std::env::var(LOCK_CHILD_EXPECTATION).expect("child expectation must be configured");
        fs::write(&ready_path, b"ready").expect("publish child readiness");

        let result = with_file_lock(Path::new(&lock_path), |_| {
            fs::write(&entered_path, b"entered").map_err(|error| error.to_string())
        });
        match expectation.as_str() {
            "blocked" => panic!("parent must terminate a blocked lock child, got {result:?}"),
            "identity" => {
                let error = result.expect_err("replacement lock identity must fail closed");
                assert!(
                    error.contains("persistent anchor have different identities"),
                    "{error}"
                );
            }
            "entered" => {
                result.expect("waiter must enter after the prior owner releases the lock");
                assert!(
                    entered_path.exists(),
                    "successful waiter did not enter operation"
                );
            }
            value => panic!("unsupported child expectation: {value}"),
        }
        if expectation != "entered" {
            assert!(
                !entered_path.exists(),
                "failed lock child entered operation"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_anchor_prevents_replaced_daemon_lock_domains() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        fs::create_dir(&state_dir).expect("state directory");
        let lock_path = state_dir.join("shared.json.lock");
        let ready_path = root.path().join("child.ready");
        let entered_path = root.path().join("child.entered");

        with_file_lock(&lock_path, |_| {
            let displaced_lock = state_dir.join("shared.json.lock.displaced");
            fs::rename(&lock_path, &displaced_lock)
                .map_err(|error| format!("failed to displace visible lock: {error}"))?;
            fs::write(&lock_path, b"replacement")
                .map_err(|error| format!("failed to replace visible lock: {error}"))?;
            run_identity_failure_child(&lock_path, &ready_path, &entered_path);

            fs::remove_file(&lock_path)
                .map_err(|error| format!("failed to remove replacement lock: {error}"))?;
            fs::rename(&displaced_lock, &lock_path)
                .map_err(|error| format!("failed to restore visible lock: {error}"))?;

            let displaced_state = root.path().join("state.displaced");
            fs::rename(&state_dir, &displaced_state)
                .map_err(|error| format!("failed to displace lock directory: {error}"))?;
            fs::create_dir(&state_dir)
                .map_err(|error| format!("failed to replace lock directory: {error}"))?;
            let mut child = spawn_lock_child(&lock_path, &ready_path, &entered_path, "blocked");
            assert_child_remains_blocked(&mut child, &ready_path, &entered_path);
            fs::remove_dir_all(&state_dir)
                .map_err(|error| format!("failed to remove replacement directory: {error}"))?;
            fs::rename(&displaced_state, &state_dir)
                .map_err(|error| format!("failed to restore lock directory: {error}"))?;
            Ok(())
        })
        .expect("held persistent daemon lock");

        with_file_lock(&lock_path, |_| {
            fs::write(&entered_path, b"restored").map_err(|error| error.to_string())
        })
        .expect("restored daemon lock remains usable");
        assert_eq!(
            fs::read(&entered_path).expect("restored operation marker"),
            b"restored"
        );
        let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
        assert_eq!(anchor_path.parent(), state_dir.parent());
        assert!(!anchor_path.starts_with(&state_dir));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn git_lock_anchor_is_removed_and_repository_status_stays_clean() {
        let root = tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .expect("initialize Git repository");
        assert!(status.success());
        fs::write(root.path().join(".git/info/exclude"), ".nib/\n").expect("ignore local state");
        let state_dir = root.path().join(".nib");
        fs::create_dir(&state_dir).expect("state directory");
        let lock_path = state_dir.join("shared.json.lock");
        let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
        assert!(anchor_path.starts_with(root.path().join(".git/nib/locks")));

        with_file_lock(&lock_path, |_| Ok(())).expect("lock operation");

        assert!(
            !anchor_path.exists(),
            "successful lock left its anchor file"
        );
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(root.path())
            .output()
            .expect("inspect Git status");
        assert!(output.status.success());
        assert!(
            output.stdout.is_empty(),
            "lock lifecycle dirtied Git status: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn nested_state_lock_does_not_use_a_sibling_git_directory() {
        let root = tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .expect("initialize Git repository");
        assert!(status.success());
        let state_dir = root.path().join("state");
        fs::create_dir(&state_dir).expect("state directory");
        let lock_path = state_dir.join("shared.json.lock");

        let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");

        assert_eq!(anchor_path.parent(), Some(root.path()));
        assert!(!anchor_path.starts_with(root.path().join(".git")));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn gitdir_file_cannot_redirect_lock_anchor_outside_project() {
        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside");
        fs::write(
            root.path().join(".git"),
            format!("gitdir: {}\n", outside.path().display()),
        )
        .expect("malicious gitdir file");
        let state_dir = root.path().join("state");
        fs::create_dir(&state_dir).expect("state directory");
        let lock_path = state_dir.join("shared.json.lock");
        let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
        assert_eq!(anchor_path.parent(), Some(root.path()));

        with_file_lock(&lock_path, |_| Ok(())).expect("fallback lock operation");

        assert!(!anchor_path.exists(), "fallback anchor was not cleaned");
        assert!(
            fs::read_dir(outside.path())
                .expect("outside directory")
                .next()
                .is_none(),
            "gitdir contents redirected anchor creation outside the project"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn blocked_waiter_repairs_then_cleans_removed_anchor() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        fs::create_dir(&state_dir).expect("state directory");
        let lock_path = state_dir.join("shared.json.lock");
        let ready_path = root.path().join("waiter.ready");
        let entered_path = root.path().join("waiter.entered");
        let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
        let mut waiter = None;

        with_file_lock(&lock_path, |_| {
            let child = spawn_lock_child(&lock_path, &ready_path, &entered_path, "entered");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready_path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "waiter did not reach lock acquisition"
                );
                thread::sleep(Duration::from_millis(10));
            }
            thread::sleep(Duration::from_millis(150));
            assert!(!entered_path.exists(), "waiter entered while lock was held");
            waiter = Some(child);
            Ok(())
        })
        .expect("first lock owner");

        let status = waiter
            .expect("spawned waiter")
            .wait()
            .expect("wait for repaired-anchor waiter");
        assert!(status.success(), "waiter failed: {status}");
        assert!(entered_path.exists(), "waiter did not enter after release");
        assert!(!anchor_path.exists(), "waiter left repaired anchor behind");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn anchor_removed_before_open_is_repaired_after_visible_inode_lock() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        fs::create_dir(&state_dir).expect("state directory");
        let lock_path = state_dir.join("shared.json.lock");
        let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
        let lock_directory = StableDirectory::open(&state_dir).expect("stable state directory");
        let anchor_directory = StableDirectory::open(root.path()).expect("stable anchor directory");
        drop(
            lock_directory
                .open_read_write_create(&lock_path)
                .expect("visible lock"),
        );
        lock_directory
            .hard_link_to(&lock_path, &anchor_directory, &anchor_path)
            .expect("initial anchor");
        let mut removed = false;

        let lock_file = open_daemon_lock_anchor_bound_with_hook(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            || {
                if removed {
                    return Ok(());
                }
                let anchor = anchor_directory.open_read_write(&anchor_path)?;
                anchor_directory.remove_file_if_matches(
                    &anchor_path,
                    &anchor,
                    ".nib-test-anchor-delete-",
                )?;
                removed = true;
                Ok(())
            },
        )
        .expect("retain visible inode after disappearing anchor");

        assert!(removed, "test did not remove the first anchor");
        let identity = daemon_lock_identity(&lock_file, &lock_path).expect("lock identity");
        lock_file.lock().expect("lock retained visible inode");
        repair_daemon_lock_anchor(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &identity,
        )
        .expect("repair disappearing anchor after locking");
        verify_daemon_lock_paths_bound(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &identity,
        )
        .expect("re-established lock pair");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn lock_pair_removed_before_open_retries_first_use_acquisition() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        fs::create_dir(&state_dir).expect("state directory");
        let lock_path = state_dir.join("shared.json.lock");
        let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
        let lock_directory = StableDirectory::open(&state_dir).expect("stable state directory");
        let anchor_directory = StableDirectory::open(root.path()).expect("stable anchor directory");
        drop(
            lock_directory
                .open_read_write_create(&lock_path)
                .expect("visible lock"),
        );
        lock_directory
            .hard_link_to(&lock_path, &anchor_directory, &anchor_path)
            .expect("initial anchor");
        let mut removed = false;

        let lock_file = open_daemon_lock_anchor_bound_with_hook(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            || {
                if removed {
                    return Ok(());
                }
                let anchor = anchor_directory.open_read_write(&anchor_path)?;
                anchor_directory.remove_file_if_matches(
                    &anchor_path,
                    &anchor,
                    ".nib-test-anchor-delete-",
                )?;
                let visible = lock_directory.open_read_write(&lock_path)?;
                lock_directory.remove_file_if_matches(
                    &lock_path,
                    &visible,
                    ".nib-test-lock-delete-",
                )?;
                removed = true;
                Ok(())
            },
        )
        .expect("retry after disappearing lock pair");

        assert!(removed, "test did not remove the first lock pair");
        let identity = daemon_lock_identity(&lock_file, &lock_path).expect("lock identity");
        lock_file.lock().expect("lock recreated visible inode");
        repair_daemon_lock_anchor(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &identity,
        )
        .expect("repair recreated lock pair");
        verify_daemon_lock_paths_bound(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &identity,
        )
        .expect("verified recreated lock pair");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn legacy_lock_cleanup_preserves_a_replacement_at_the_delete_boundary() {
        let root = tempdir().expect("tempdir");
        let visible_root = root.path().join("visible");
        let anchor_root = root.path().join("anchor");
        fs::create_dir(&visible_root).expect("visible directory");
        fs::create_dir(&anchor_root).expect("anchor directory");
        let visible_path = visible_root.join("legacy.lock");
        let anchor_path = anchor_root.join("legacy.anchor");
        let displaced = visible_root.join("legacy.displaced");
        fs::write(&visible_path, b"legacy").expect("legacy lock");
        fs::hard_link(&visible_path, &anchor_path).expect("legacy hard-link pair");
        let visible = StableDirectory::open(&visible_root).expect("visible capability");
        let anchor = StableDirectory::open(&anchor_root).expect("anchor capability");

        let error = cleanup_legacy_lock_pair_with_hook(
            &visible,
            &visible_path,
            &anchor,
            &anchor_path,
            || {
                fs::rename(&visible_path, &displaced).map_err(|error| error.to_string())?;
                fs::write(&visible_path, b"replacement").map_err(|error| error.to_string())
            },
        )
        .expect_err("replacement must stop legacy deletion");

        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(&visible_path).expect("replacement lock"),
            b"replacement"
        );
        assert_eq!(
            fs::read(&displaced).expect("displaced legacy lock"),
            b"legacy"
        );
        assert_eq!(fs::read(&anchor_path).expect("legacy anchor"), b"legacy");
    }

    #[cfg(windows)]
    #[test]
    fn open_directory_capability_blocks_daemon_lock_parent_replacement() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        fs::create_dir(&state_dir).expect("state directory");
        let lock_path = state_dir.join("shared.json.lock");
        let displaced_state = root.path().join("state.displaced");

        let error = with_file_lock(&lock_path, |_| {
            fs::rename(&state_dir, &displaced_state).map_err(|error| error.to_string())
        })
        .expect_err("a live Windows lock domain must pin its parent namespace");

        assert!(!error.is_empty());
        assert!(state_dir.is_dir(), "original lock domain remains visible");
        assert!(!displaced_state.exists(), "lock domain was not displaced");
        assert!(
            lock_path.is_file(),
            "original lock remains in the pinned domain"
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_lock_open_rejects_a_symlink_inserted_after_inspection() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("tempdir");
        let lock_path = root.path().join("race.lock");
        let displaced_path = root.path().join("race.lock.displaced");
        let outside_path = root.path().join("outside");
        fs::write(&lock_path, b"original").expect("original lock");
        fs::write(&outside_path, b"sentinel").expect("outside target");

        let error = open_daemon_lock_file_with_hook(&lock_path, || {
            fs::rename(&lock_path, &displaced_path)
                .map_err(|error| format!("failed to displace inspected lock: {error}"))?;
            symlink(&outside_path, &lock_path)
                .map_err(|error| format!("failed to insert lock symlink: {error}"))
        })
        .expect_err("no-follow open must reject the inserted symlink");

        assert!(error.contains("failed to open daemon lock"), "{error}");
        assert_eq!(
            fs::read(&outside_path).expect("outside target remains readable"),
            b"sentinel"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pure_atomic_save_aborts_when_directory_is_detached_before_commit() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        let displaced = root.path().join("state.displaced");
        let target = state_dir.join("record.json");
        fs::create_dir(&state_dir).expect("state directory");
        fs::write(&target, b"original").expect("original state");
        let directory = StableDirectory::open(&state_dir).expect("stable directory");

        fs::rename(&state_dir, &displaced).expect("detach state directory");
        fs::create_dir(&state_dir).expect("replacement state directory");
        fs::write(&target, b"replacement").expect("replacement sentinel");

        let error = directory
            .save_bytes_atomically(&target, b"new", ".pure-")
            .expect_err("known detachment must abort a pure commit");
        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(displaced.join("record.json")).expect("original tree record"),
            b"original"
        );
        assert_eq!(
            fs::read(&target).expect("replacement tree record"),
            b"replacement"
        );
        assert!(
            fs::read_dir(&displaced)
                .expect("original tree")
                .all(|entry| !entry
                    .expect("state entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pure-")),
            "aborted temporary file must be removed from the original capability"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_race_never_redirects_commit_to_replacement_directory() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        let displaced = root.path().join("state.displaced");
        let target = state_dir.join("record.json");
        fs::create_dir(&state_dir).expect("state directory");
        fs::write(&target, b"original").expect("original state");
        let directory = StableDirectory::open(&state_dir).expect("stable directory");

        let error = directory
            .save_bytes_atomically_with_hook(&target, b"committed", ".race-", true, || {
                fs::rename(&state_dir, &displaced)
                    .map_err(|error| format!("failed to detach state directory: {error}"))?;
                fs::create_dir(&state_dir)
                    .map_err(|error| format!("failed to create replacement state: {error}"))?;
                fs::write(&target, b"replacement")
                    .map_err(|error| format!("failed to seed replacement state: {error}"))
            })
            .expect_err("post-check must report the attachment race");
        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(displaced.join("record.json")).expect("original capability commit"),
            b"committed"
        );
        assert_eq!(
            fs::read(&target).expect("replacement sentinel"),
            b"replacement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn post_publication_failure_retains_the_exact_publication_receipt() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        let displaced = root.path().join("state.displaced");
        let target = state_dir.join("record.json");
        fs::create_dir(&state_dir).expect("state directory");
        fs::write(&target, b"original").expect("original state");
        let directory = StableDirectory::open(&state_dir).expect("stable directory");
        let expected = directory.open_read(&target).expect("expected state");

        let failure = directory
            .save_bytes_atomically_expected_with_hooks(
                &target,
                b"committed",
                ".receipt-race-",
                AtomicSaveExpectation {
                    require_attached_before_commit: true,
                    file: FileExpectation::Present(&expected),
                    retain_publication_lock: false,
                },
                || {
                    fs::rename(&state_dir, &displaced)
                        .map_err(|error| format!("failed to detach state directory: {error}"))?;
                    fs::create_dir(&state_dir)
                        .map_err(|error| format!("failed to create replacement state: {error}"))?;
                    fs::write(&target, b"replacement")
                        .map_err(|error| format!("failed to seed replacement state: {error}"))
                },
                || {},
            )
            .expect_err("detached publication must report its receipt with the error");

        assert!(failure.message.contains("identity changed"), "{failure:?}");
        let receipt = failure
            .receipt
            .expect("post-publication failure must retain a receipt");
        assert!(receipt.exact_identity);
        let committed_path = displaced.join("record.json");
        let committed_directory = StableDirectory::open(&displaced).expect("committed directory");
        let committed = committed_directory
            .open_read(&committed_path)
            .expect("committed state");
        assert!(same_open_file_identity(&receipt.file, &committed).expect("same identity"));
        assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn paired_atomic_save_commits_original_tree_after_effects_despite_detachment() {
        let root = tempdir().expect("tempdir");
        let state_dir = root.path().join("state");
        let displaced = root.path().join("state.displaced");
        let target = state_dir.join("record.json");
        fs::create_dir(&state_dir).expect("state directory");
        fs::write(&target, b"before-effect").expect("original state");
        let directory = StableDirectory::open(&state_dir).expect("stable directory");

        fs::rename(&state_dir, &displaced).expect("detach state directory");
        fs::create_dir(&state_dir).expect("replacement state directory");
        fs::write(&target, b"replacement").expect("replacement sentinel");

        let error = directory
            .save_bytes_atomically_after_effects(&target, b"paired-transition", ".paired-")
            .expect_err("paired commit must still report detachment");
        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(displaced.join("record.json")).expect("paired original commit"),
            b"paired-transition"
        );
        assert_eq!(
            fs::read(&target).expect("replacement sentinel"),
            b"replacement"
        );
    }

    #[test]
    fn handle_bound_publication_supports_present_and_missing_expectations() {
        let root = tempdir().expect("tempdir");
        let existing = root.path().join("existing.json");
        let missing = root.path().join("missing.json");
        fs::write(&existing, b"old").expect("existing state");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory.open_read(&existing).expect("expected state");

        directory
            .save_bytes_atomically_expected(
                &existing,
                b"new",
                ".runtime-publication-",
                FileExpectation::Present(&expected),
            )
            .expect("publish over retained state");
        directory
            .save_bytes_atomically_expected(
                &missing,
                b"created",
                ".runtime-publication-",
                FileExpectation::Missing,
            )
            .expect("publish into proven absence");

        assert_eq!(fs::read(existing).expect("updated state"), b"new");
        assert_eq!(fs::read(missing).expect("created state"), b"created");
    }

    #[cfg(windows)]
    #[test]
    fn publication_receipt_does_not_retain_a_mandatory_byte_lock() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("receipt.json");
        let directory = StableDirectory::open(root.path()).expect("stable directory");

        let receipt = directory
            .save_bytes_atomically_expected_with_receipt(
                &target,
                b"published",
                ".receipt-publication-",
                FileExpectation::Missing,
            )
            .expect("publish with retained receipt");

        assert!(receipt.exact_identity);
        assert_eq!(
            read_open_file_prefix(&receipt.file, b"published".len() + 1)
                .expect("read retained receipt"),
            b"published"
        );
        assert_eq!(
            fs::read(&target).expect("read while receipt remains alive"),
            b"published"
        );
        let contender = directory
            .open_read_write(&target)
            .expect("open publication contender");
        contender
            .try_lock()
            .expect("generic receipt must not retain the publication lock");
        contender.unlock().expect("release publication contender");
        assert!(same_open_file_identity(
            &receipt.file,
            &directory.open_read(&target).expect("reopen publication")
        )
        .expect("receipt identity"));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn locked_publication_receipt_continuously_excludes_recovery() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("owned-receipt.json");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let mut checked_before_return = false;

        let receipt = directory
            .save_bytes_atomically_expected_with_locked_receipt_before_return(
                &target,
                b"owned",
                ".owned-receipt-publication-",
                FileExpectation::Missing,
                || {
                    assert!(target.is_file(), "publication is visible before return");
                    let contender = directory
                        .open_read_write(&target)
                        .expect("open pre-return recovery contender");
                    assert!(matches!(
                        contender.try_lock(),
                        Err(std::fs::TryLockError::WouldBlock)
                    ));
                    checked_before_return = true;
                },
            )
            .expect("publish retained locked receipt");
        assert!(checked_before_return);
        assert_eq!(
            read_open_file_prefix(&receipt.file, b"owned".len() + 1)
                .expect("read through lock-owning receipt"),
            b"owned"
        );

        let contender = directory
            .open_read_write(&target)
            .expect("open recovery contender");
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));

        drop(receipt);
        contender
            .try_lock()
            .expect("dropping the receipt releases ownership");
        contender.unlock().expect("release contender lock");
    }

    #[cfg(windows)]
    #[test]
    fn windows_delete_access_supports_handle_bound_directory_quarantine() {
        let root = tempdir().expect("tempdir");
        let source = root.path().join("managed-skill");
        let quarantine = root.path().join("managed-skill.quarantine");
        fs::create_dir(&source).expect("source directory");
        fs::write(source.join("marker.json"), b"managed").expect("source marker");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let ordinary = directory.open_child(&source).expect("ordinary source");
        let error = directory
            .rename_child_directory(&source, &ordinary, &quarantine)
            .expect_err("ordinary namespace capability must not mutate");
        assert!(
            error.contains("lacks the retained DELETE capability"),
            "{error}"
        );
        drop(ordinary);
        let source_directory = directory
            .open_owned_child(&source)
            .expect("source capability");

        directory
            .rename_child_directory(&source, &source_directory, &quarantine)
            .expect("handle-bound directory quarantine");
        source_directory
            .verify_visible_at(&quarantine)
            .expect("quarantined source identity");

        assert!(!source.exists());
        assert_eq!(
            fs::read(quarantine.join("marker.json")).expect("quarantined marker"),
            b"managed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_handle_delete_preserves_a_replacement_after_the_final_identity_check() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("record.json");
        let displaced = root.path().join("record.displaced.json");
        fs::write(&target, b"expected").expect("expected state");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory
            .open_read_write(&target)
            .expect("expected state handle");

        directory
            .remove_bound_file_if_matches_with_hook_after_identity(
                &target,
                &expected,
                || Ok(()),
                || {
                    fs::rename(&target, &displaced).map_err(|error| error.to_string())?;
                    fs::write(&target, b"replacement").map_err(|error| error.to_string())
                },
            )
            .expect("handle-bound deletion");
        drop(expected);

        assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
        assert!(
            !displaced.exists(),
            "opened original must be deleted by handle"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_conditional_removal_deletes_the_requested_hard_link_alias() {
        let root = tempdir().expect("tempdir");
        let anchor = root.path().join("anchor.json");
        let visible = root.path().join("visible.json");
        fs::write(&anchor, b"owned").expect("anchor state");
        fs::hard_link(&anchor, &visible).expect("visible hard link");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory.open_read(&anchor).expect("anchor handle");

        directory
            .remove_file_if_matches(&visible, &expected, ".hard-link-delete-")
            .expect("remove requested alias");

        assert!(!visible.exists(), "requested alias must be deleted");
        assert_eq!(fs::read(&anchor).expect("anchor remains"), b"owned");
    }

    #[test]
    fn temporary_path_substitution_after_fsync_preserves_destination_and_replacement() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("record.json");
        fs::write(&target, b"old").expect("old state");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory.open_read(&target).expect("expected state");
        let temporary = root.path().join(deterministic_artifact_name(
            ".temp-substitution-",
            b"record.json",
            ".tmp",
        ));
        let displaced = root.path().join("displaced-temp");

        let error = directory
            .save_bytes_atomically_expected_with_hook(
                &target,
                b"new",
                ".temp-substitution-",
                true,
                FileExpectation::Present(&expected),
                || {
                    fs::rename(&temporary, &displaced).map_err(|error| error.to_string())?;
                    fs::write(&temporary, b"replacement-temp").map_err(|error| error.to_string())
                },
            )
            .expect_err("substituted temporary pathname must fail closed");

        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(fs::read(&target).expect("destination"), b"old");
        assert_eq!(
            fs::read(&temporary).expect("replacement temporary"),
            b"replacement-temp"
        );
        assert_eq!(fs::read(displaced).expect("original temporary"), b"new");
    }

    #[test]
    fn publication_conflict_after_evacuation_preserves_prior_and_new_target() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("record.json");
        fs::write(&target, b"prior").expect("prior state");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory.open_read(&target).expect("prior handle");
        let previous = root.path().join(
            deterministic_previous_artifact_name(
                ".rollback-ambiguity-",
                std::ffi::OsStr::new("record.json"),
            )
            .expect("previous artifact name"),
        );

        let failure = directory
            .save_bytes_atomically_expected_with_hooks(
                &target,
                b"attempted",
                ".rollback-ambiguity-",
                AtomicSaveExpectation {
                    require_attached_before_commit: true,
                    file: FileExpectation::Present(&expected),
                    retain_publication_lock: false,
                },
                || Ok(()),
                || fs::write(&target, b"new-target").expect("conflicting target"),
            )
            .expect_err("identity-distinct publication conflict must fail closed");

        assert!(failure.message.contains("identity-distinct"), "{failure:?}");
        assert!(failure.receipt.is_none());
        assert_eq!(fs::read(&target).expect("new target"), b"new-target");
        assert_eq!(fs::read(&previous).expect("evacuated prior"), b"prior");
    }

    #[test]
    fn failed_publication_finalization_validates_bytes_before_prior_cleanup() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("record.json");
        let previous = root.path().join(
            deterministic_previous_artifact_name(
                ".finalize-bytes-",
                std::ffi::OsStr::new("record.json"),
            )
            .expect("previous artifact name"),
        );
        fs::write(&target, b"corrupt").expect("corrupt target");
        fs::write(&previous, b"prior").expect("prior state");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let target_file = directory.open_read(&target).expect("target handle");
        let previous_file = directory.open_read(&previous).expect("previous handle");
        let receipt = FilePublicationReceipt {
            file: target_file,
            exact_identity: true,
        };

        let error = directory
            .finalize_failed_exact_publication(
                &target,
                Some(&previous_file),
                &receipt,
                ".finalize-bytes-",
                b"expected",
            )
            .expect_err("corrupt publication must not be finalized");

        assert!(error.contains("bytes changed"), "{error}");
        assert_eq!(fs::read(&target).expect("corrupt target"), b"corrupt");
        assert_eq!(fs::read(&previous).expect("prior retained"), b"prior");
    }

    #[cfg(unix)]
    #[test]
    fn temporary_cleanup_preserves_exact_link_if_target_is_substituted() {
        let root = tempdir().expect("tempdir");
        let temporary = root.path().join("record.tmp");
        let target = root.path().join("record.json");
        let displaced = root.path().join("record.displaced");
        fs::write(&temporary, b"managed").expect("temporary state");
        fs::hard_link(&temporary, &target).expect("published hard link");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory.open_read(&temporary).expect("temporary handle");

        let error = directory
            .cleanup_open_temporary_file(&temporary, &expected, || {
                fs::rename(&target, &displaced).map_err(|error| error.to_string())?;
                fs::write(&target, b"replacement").map_err(|error| error.to_string())?;
                directory.verify_publication_bytes(&target, &expected, b"managed")
            })
            .expect_err("target substitution must preserve the exact temporary link");

        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(fs::read(&temporary).expect("managed link"), b"managed");
        assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
        assert_eq!(fs::read(&displaced).expect("displaced target"), b"managed");
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn pathname_publication_source_swap_is_rescued_and_prior_is_restored() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("record.json");
        fs::write(&target, b"prior").expect("prior state");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory.open_read(&target).expect("prior handle");
        let temporary = root.path().join(deterministic_artifact_name(
            ".mac-publication-swap-",
            b"record.json",
            ".tmp",
        ));
        let displaced = root.path().join("attempted.displaced");

        let failure = directory
            .save_bytes_atomically_expected_with_hooks(
                &target,
                b"attempted",
                ".mac-publication-swap-",
                AtomicSaveExpectation {
                    require_attached_before_commit: true,
                    file: FileExpectation::Present(&expected),
                    retain_publication_lock: false,
                },
                || Ok(()),
                || {
                    fs::rename(&temporary, &displaced).expect("displace attempted state");
                    fs::write(&temporary, b"replacement-temp").expect("replacement temporary");
                },
            )
            .expect_err("pathname source swap must fail closed");

        assert!(failure.message.contains("source changed"), "{failure:?}");
        assert!(failure.receipt.is_none());
        assert_eq!(fs::read(&target).expect("restored prior"), b"prior");
        assert_eq!(
            fs::read(&temporary).expect("rescued replacement"),
            b"replacement-temp"
        );
        assert_eq!(fs::read(&displaced).expect("attempted state"), b"attempted");
    }

    #[cfg(unix)]
    #[test]
    fn source_substitution_before_quarantine_is_rescued_without_deleting_either_file() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("record.json");
        fs::write(&target, b"expected").expect("expected state");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory.open_read(&target).expect("expected state handle");
        let displaced = root.path().join("displaced-expected");
        let quarantine = directory
            .deterministic_artifact_path(&target, ".source-swap-", ".quarantine")
            .expect("quarantine path");

        let error = directory
            .move_open_file_no_replace_bound_with_hook(&target, &expected, &quarantine, || {
                fs::rename(&target, &displaced).map_err(|error| error.to_string())?;
                fs::write(&target, b"replacement").map_err(|error| error.to_string())
            })
            .expect_err("source substitution must fail closed");

        assert!(error.contains("state source changed"), "{error}");
        assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
        assert_eq!(fs::read(&displaced).expect("expected"), b"expected");
        assert!(!quarantine.exists());
    }

    #[test]
    fn late_quarantine_substitution_is_preserved() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("record.json");
        fs::write(&target, b"old").expect("old state");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let expected = directory.open_read(&target).expect("expected state");
        let quarantine = directory
            .deterministic_artifact_path(&target, ".late-delete-", ".quarantine")
            .expect("quarantine path");
        let displaced = root.path().join("expected-quarantine");

        let error = directory
            .remove_file_if_matches_with_hooks(
                &target,
                &expected,
                ".late-delete-",
                || Ok(()),
                || {
                    fs::rename(&quarantine, &displaced).map_err(|error| error.to_string())?;
                    fs::write(&quarantine, b"replacement-quarantine")
                        .map_err(|error| error.to_string())
                },
            )
            .expect_err("late quarantine replacement must fail closed");

        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(&quarantine).expect("replacement quarantine"),
            b"replacement-quarantine"
        );
        assert_eq!(fs::read(displaced).expect("expected quarantine"), b"old");
    }

    #[test]
    fn restart_recovery_preserves_ambiguous_prior_and_quarantine_artifacts() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("record.json");
        fs::write(&target, b"newer").expect("newer target");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let previous = root.path().join(
            deterministic_previous_artifact_name(".restart-", target.file_name().unwrap())
                .expect("previous name"),
        );
        fs::write(&previous, b"substituted-prior").expect("prior artifact");

        let error = directory
            .recover_stale_temporary_files(".restart-", 16, 4096)
            .expect_err("ambiguous prior must fail closed");
        assert!(error.contains("both were preserved"), "{error}");
        assert_eq!(fs::read(&target).expect("target"), b"newer");
        assert_eq!(fs::read(&previous).expect("prior"), b"substituted-prior");

        fs::remove_file(&target).expect("remove target for missing-target recovery case");
        let error = directory
            .recover_stale_temporary_files(".restart-", 16, 4096)
            .expect_err("missing target with an unjournaled prior must fail closed");
        assert!(error.contains("target is missing"), "{error}");
        assert!(!target.exists());
        assert_eq!(
            fs::read(&previous).expect("preserved unproven prior"),
            b"substituted-prior"
        );
        fs::write(&target, b"newer").expect("restore target for quarantine case");

        let quarantine = directory
            .deterministic_artifact_path(&target, ".restart-delete-", ".quarantine")
            .expect("quarantine path");
        fs::write(&quarantine, b"substituted-quarantine").expect("quarantine artifact");
        let error = directory
            .recover_quarantined_file(&target, ".restart-delete-")
            .expect_err("ambiguous quarantine must fail closed");
        assert!(error.contains("both were preserved"), "{error}");
        assert!(target.exists());
        assert!(quarantine.exists());
    }

    #[test]
    fn live_atomic_writer_cannot_hide_an_evacuated_target_from_recovery() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("session.json");
        fs::write(&target, b"authoritative session").expect("session target");
        let prefix = ".nib-session-";
        let temporary = root.path().join(deterministic_artifact_name(
            prefix,
            target.file_name().unwrap().as_encoded_bytes(),
            ".tmp",
        ));
        let previous = root.path().join(
            deterministic_previous_artifact_name(prefix, target.file_name().unwrap())
                .expect("previous artifact"),
        );
        let temporary_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)
            .expect("live temporary");
        temporary_file.lock().expect("own temporary transaction");
        fs::rename(&target, &previous).expect("evacuate target");
        let directory = StableDirectory::open(root.path()).expect("stable directory");

        let error = directory
            .recover_stale_temporary_files_strict(prefix, 16, 4096)
            .expect_err("live transaction must not be silently filtered");

        assert!(error.contains("live writer"), "{error}");
        assert!(error.contains("session.json"), "{error}");
        assert!(!target.exists());
        assert_eq!(
            fs::read(previous).expect("preserved prior session"),
            b"authoritative session"
        );
    }

    #[test]
    fn strict_recovery_waits_for_a_transient_atomic_writer() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("session.json");
        fs::write(&target, b"authoritative session").expect("session target");
        let prefix = ".nib-session-";
        let temporary = root.path().join(deterministic_artifact_name(
            prefix,
            target.file_name().unwrap().as_encoded_bytes(),
            ".tmp",
        ));
        let previous = root.path().join(
            deterministic_previous_artifact_name(prefix, target.file_name().unwrap())
                .expect("previous artifact"),
        );
        let temporary_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)
            .expect("live temporary");
        temporary_file.lock().expect("own temporary transaction");
        fs::rename(&target, &previous).expect("evacuate target");

        let writer_target = target.clone();
        let writer_previous = previous.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            fs::rename(writer_previous, writer_target).expect("restore target");
            drop(temporary_file);
        });
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let started = Instant::now();

        directory
            .recover_stale_temporary_files_strict(prefix, 16, 4096)
            .expect("transient writer must finish before strict recovery returns");

        writer.join().expect("transient writer");
        assert!(started.elapsed() >= Duration::from_millis(25));
        assert_eq!(
            fs::read(&target).expect("restored session"),
            b"authoritative session"
        );
        assert!(!previous.exists());
        assert!(!temporary.exists());
    }

    #[test]
    fn atomic_recovery_retries_when_prior_disappears_before_open() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("session.json");
        fs::write(&target, b"published session").expect("session target");
        let prefix = ".nib-session-";
        let temporary = root.path().join(deterministic_artifact_name(
            prefix,
            target.file_name().unwrap().as_encoded_bytes(),
            ".tmp",
        ));
        let previous = root.path().join(
            deterministic_previous_artifact_name(prefix, target.file_name().unwrap())
                .expect("previous artifact"),
        );
        fs::write(&previous, b"prior session").expect("prior artifact");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let writer_directory = StableDirectory::open(root.path()).expect("writer directory");
        let previous_file = writer_directory
            .open_read(&previous)
            .expect("retained prior state");
        let previous_for_hook = previous.clone();
        let mut pending_cleanup = Some((writer_directory, previous_file));
        let mut remove_before_open = || {
            let (writer_directory, previous_file) =
                pending_cleanup.take().expect("prior-open hook runs once");
            writer_directory.remove_visible_file_if_matches(
                &previous_for_hook,
                &previous_file,
                || Ok(()),
            )
        };
        let mut live_target_hook = || {};

        let recovered = directory
            .recover_atomic_transaction_with_hooks(
                &target,
                &temporary,
                &previous,
                true,
                false,
                AtomicRecoveryHooks {
                    previous_open: &mut remove_before_open,
                    live_target: &mut live_target_hook,
                },
            )
            .expect("changed namespace must be re-evaluated");

        drop(remove_before_open);
        assert!(!recovered);
        assert!(pending_cleanup.is_none());
        assert_eq!(
            fs::read(&target).expect("published target"),
            b"published session"
        );
        assert!(!previous.exists());
    }

    #[test]
    fn recovery_recognizes_a_live_writer_after_temporary_publication() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("session.json");
        fs::write(&target, b"prior session").expect("session target");
        let prefix = ".nib-session-";
        let temporary = root.path().join(deterministic_artifact_name(
            prefix,
            target.file_name().unwrap().as_encoded_bytes(),
            ".tmp",
        ));
        let previous = root.path().join(
            deterministic_previous_artifact_name(prefix, target.file_name().unwrap())
                .expect("previous artifact"),
        );
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let mut published = directory
            .open_read_write_create(&temporary)
            .expect("temporary publication");
        published.write_all(b"new session").expect("new state");
        published.sync_all().expect("sync new state");
        published.lock().expect("own atomic publication");
        fs::rename(&target, &previous).expect("evacuate prior state");
        fs::rename(&temporary, &target).expect("publish locked temporary");

        let recovered = directory
            .recover_stale_temporary_files(prefix, 16, 4096)
            .expect("live published writer must be skipped");
        assert_eq!(recovered, 0);
        assert_eq!(fs::read(&target).expect("published target"), b"new session");
        assert_eq!(
            fs::read(&previous).expect("preserved prior"),
            b"prior session"
        );

        let writer_directory = StableDirectory::open(root.path()).expect("writer directory");
        let writer_previous = previous.clone();
        let writer_previous_file = writer_directory
            .open_read(&previous)
            .expect("retained prior state");
        let (blocked_tx, blocked_rx) = std::sync::mpsc::sync_channel(0);
        let writer = thread::spawn(move || {
            blocked_rx.recv().expect("recovery reached target lock");
            writer_directory
                .remove_visible_file_if_matches(&writer_previous, &writer_previous_file, || Ok(()))
                .expect("finish prior cleanup");
            published.unlock().expect("release publication");
        });
        let mut report_live_target = || {
            blocked_tx
                .send(())
                .expect("report live published target to writer");
        };
        directory
            .recover_atomic_transaction_with_live_target_hook(
                &target,
                &temporary,
                &previous,
                true,
                true,
                &mut report_live_target,
            )
            .expect("strict recovery must observe completed publication");

        writer.join().expect("atomic writer");
        assert_eq!(fs::read(&target).expect("published target"), b"new session");
        assert!(!previous.exists());
        assert!(!temporary.exists());
    }

    #[test]
    fn strict_recovery_times_out_then_preserves_unlocked_publication_ambiguity() {
        let root = tempdir().expect("tempdir");
        let target = root.path().join("session.json");
        let previous = root.path().join(
            deterministic_previous_artifact_name(".nib-session-", target.file_name().unwrap())
                .expect("previous artifact"),
        );
        fs::write(&target, b"published").expect("published target");
        fs::write(&previous, b"prior").expect("prior artifact");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        let target_file = directory.open_read_write(&target).expect("target handle");
        target_file.lock().expect("own published target");

        let started = Instant::now();
        let error = directory
            .recover_stale_temporary_files_strict(".nib-session-", 16, 4096)
            .expect_err("live published target must reach the strict deadline");
        assert!(
            started.elapsed() >= STRICT_RECOVERY_LIVE_WRITER_WAIT / 2,
            "strict recovery returned before its live-writer wait"
        );
        assert!(
            error.contains("target is still owned by a live writer"),
            "{error}"
        );
        assert_eq!(fs::read(&target).expect("preserved target"), b"published");
        assert_eq!(fs::read(&previous).expect("preserved prior"), b"prior");

        target_file.unlock().expect("release published target");
        let error = directory
            .recover_stale_temporary_files_strict(".nib-session-", 16, 4096)
            .expect_err("unlocked identity-distinct state must stay ambiguous");
        assert!(error.contains("both were preserved"), "{error}");
        assert_eq!(fs::read(&target).expect("preserved target"), b"published");
        assert_eq!(fs::read(&previous).expect("preserved prior"), b"prior");
    }

    #[test]
    fn stale_temporary_recovery_ignores_near_match_names() {
        let root = tempdir().expect("tempdir");
        let near_matches = [
            ".bounded-abc.tmp",
            ".bounded-0000000000000000000000000000000g.tmp",
            ".bounded-00000000000000000000000000000000.tmp.extra",
        ];
        for name in near_matches {
            fs::write(root.path().join(name), b"preserve").expect("near-match artifact");
        }
        let directory = StableDirectory::open(root.path()).expect("stable directory");

        assert_eq!(
            directory
                .recover_stale_temporary_files(".bounded-", 16, 4096)
                .expect("bounded recovery"),
            0
        );
        for name in near_matches {
            assert!(root.path().join(name).exists(), "near match {name} removed");
        }
    }

    #[test]
    fn pre_evacuation_stale_temporary_is_recoverable() {
        let root = tempdir().expect("tempdir");
        let target_name = std::ffi::OsStr::new("record.json");
        let temporary = root.path().join(deterministic_artifact_name(
            ".recoverable-",
            target_name.as_encoded_bytes(),
            ".tmp",
        ));
        fs::write(&temporary, b"unpublished temporary").expect("stale temporary");
        let directory = StableDirectory::open(root.path()).expect("stable directory");

        assert_eq!(
            directory
                .recover_stale_temporary_files(".recoverable-", 16, 4096)
                .expect("recover stale pre-evacuation temporary"),
            1
        );
        assert!(!temporary.exists());
        assert!(!root.path().join(target_name).exists());
    }

    #[cfg(unix)]
    #[test]
    fn real_child_atomic_fsync_crash_recovery_matrix() {
        if let Some(root) = std::env::var_os(ATOMIC_CRASH_CHILD_ROOT) {
            run_atomic_crash_child(Path::new(&root));
            return;
        }

        let root = tempdir().expect("tempdir");
        let pre_root = root.path().join("pre-evacuation");
        fs::create_dir(&pre_root).expect("pre-evacuation directory");
        let pre_target = pre_root.join("record.json");
        fs::write(&pre_target, b"old-pre").expect("pre-evacuation target");
        let pre_ready = root.path().join("pre.ready");
        let mut pre_child = spawn_atomic_crash_child(&pre_root, "before", &pre_ready);
        wait_for_atomic_child(&mut pre_child, &pre_ready);
        let pre_temporary = pre_root.join(deterministic_artifact_name(
            ".child-crash-",
            b"record.json",
            ".tmp",
        ));
        assert!(pre_temporary.exists(), "fsynced temporary was not visible");
        pre_child.kill().expect("kill pre-evacuation writer");
        pre_child.wait().expect("reap pre-evacuation writer");

        let pre_directory = StableDirectory::open(&pre_root).expect("pre recovery capability");
        assert_eq!(
            pre_directory
                .recover_stale_temporary_files(".child-crash-", 16, 4096)
                .expect("recover killed pre-evacuation writer"),
            1
        );
        assert_eq!(fs::read(&pre_target).expect("pre target"), b"old-pre");
        assert!(!pre_temporary.exists(), "recovery left the stale temporary");

        let post_root = root.path().join("post-evacuation");
        fs::create_dir(&post_root).expect("post-evacuation directory");
        let post_target = post_root.join("record.json");
        fs::write(&post_target, b"old-post").expect("post-evacuation target");
        let post_ready = root.path().join("post.ready");
        let mut post_child = spawn_atomic_crash_child(&post_root, "after", &post_ready);
        wait_for_atomic_child(&mut post_child, &post_ready);
        let post_temporary = post_root.join(deterministic_artifact_name(
            ".child-crash-",
            b"record.json",
            ".tmp",
        ));
        let post_previous = post_root.join(
            deterministic_previous_artifact_name(
                ".child-crash-",
                post_target.file_name().expect("post target name"),
            )
            .expect("post previous name"),
        );
        assert!(!post_target.exists(), "target was not evacuated");
        assert!(post_temporary.exists(), "post-evacuation temp missing");
        assert!(post_previous.exists(), "post-evacuation prior missing");
        post_child.kill().expect("kill post-evacuation writer");
        post_child.wait().expect("reap post-evacuation writer");

        let post_directory = StableDirectory::open(&post_root).expect("post recovery capability");
        let error = post_directory
            .recover_stale_temporary_files(".child-crash-", 16, 4096)
            .expect_err("post-evacuation crash must fail closed");
        assert!(error.contains("target is missing"), "{error}");
        assert!(!post_target.exists(), "recovery invented a target");
        assert_eq!(
            fs::read(&post_previous).expect("preserved prior"),
            b"old-post"
        );
        assert_eq!(
            fs::read(&post_temporary).expect("preserved temporary"),
            b"new-state"
        );
    }

    #[test]
    fn stable_directory_scan_enforces_entry_and_filename_byte_budgets() {
        let root = tempdir().expect("tempdir");
        for name in ["one", "two", "three"] {
            fs::write(root.path().join(name), b"x").expect("scan fixture");
        }
        let directory = StableDirectory::open(root.path()).expect("stable directory");

        let entry_error = directory
            .for_each_entry_bounded(2, 1024, |_| Ok(()))
            .expect_err("entry cap must stop the streaming scan");
        assert!(entry_error.contains("bounded scan limit"), "{entry_error}");

        let name_error = directory
            .for_each_entry_bounded(3, 8, |_| Ok(()))
            .expect_err("filename byte cap must stop the streaming scan");
        assert!(name_error.contains("bounded scan limit"), "{name_error}");
    }

    #[cfg(windows)]
    #[test]
    fn delete_capable_directory_scan_uses_its_retained_handle() {
        let root = tempdir().expect("tempdir");
        let child = root.path().join("owned");
        fs::create_dir(&child).expect("owned child");
        fs::write(child.join("payload"), b"data").expect("owned child payload");
        let root_directory = StableDirectory::open(root.path()).expect("stable root");
        let owned = root_directory
            .open_owned_child(&child)
            .expect("delete-capable child");
        let mut names = Vec::new();

        owned
            .for_each_entry_bounded(4, 128, |name| {
                names.push(name);
                Ok(())
            })
            .expect("scan through retained delete-capable handle");

        assert_eq!(names, [OsString::from("payload")]);
    }

    #[cfg(any(unix, windows))]
    fn spawn_lock_child(
        lock_path: &Path,
        ready_path: &Path,
        entered_path: &Path,
        expectation: &str,
    ) -> Child {
        let _ = fs::remove_file(ready_path);
        let _ = fs::remove_file(entered_path);
        Command::new(std::env::current_exe().expect("current test binary"))
            .args([
                "--exact",
                "daemons::state::tests::daemon_file_lock_replacement_child_process",
                "--nocapture",
            ])
            .env(LOCK_CHILD_PATH, lock_path)
            .env(LOCK_CHILD_READY, ready_path)
            .env(LOCK_CHILD_ENTERED, entered_path)
            .env(LOCK_CHILD_EXPECTATION, expectation)
            .spawn()
            .expect("spawn daemon lock child")
    }

    #[cfg(unix)]
    fn run_identity_failure_child(lock_path: &Path, ready_path: &Path, entered_path: &Path) {
        let status = spawn_lock_child(lock_path, ready_path, entered_path, "identity")
            .wait()
            .expect("wait for daemon lock child");
        assert!(status.success(), "identity failure child failed: {status}");
        assert!(ready_path.exists(), "identity child did not run");
        assert!(!entered_path.exists(), "identity failure entered operation");
    }

    #[cfg(unix)]
    fn assert_child_remains_blocked(child: &mut Child, ready_path: &Path, entered_path: &Path) {
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() {
            if let Some(status) = child.try_wait().expect("inspect daemon lock child") {
                panic!("daemon lock child exited before readiness: {status}");
            }
            assert!(
                Instant::now() < ready_deadline,
                "daemon lock child did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }

        let blocked_until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < blocked_until {
            assert!(
                !entered_path.exists(),
                "contender entered while lock was held"
            );
            if let Some(status) = child.try_wait().expect("inspect blocked lock child") {
                panic!("daemon lock child failed instead of blocking: {status}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        child.kill().expect("terminate blocked lock child");
        child.wait().expect("reap blocked lock child");
        assert!(
            !entered_path.exists(),
            "terminated contender entered operation"
        );
    }

    #[cfg(unix)]
    fn run_atomic_crash_child(root: &Path) {
        let ready = PathBuf::from(
            std::env::var_os(ATOMIC_CRASH_CHILD_READY)
                .expect("atomic crash child ready path must be configured"),
        );
        let mode = std::env::var(ATOMIC_CRASH_CHILD_MODE)
            .expect("atomic crash child mode must be configured");
        let target = root.join("record.json");
        let directory = StableDirectory::open(root).expect("atomic crash child capability");
        let expected = directory
            .open_read(&target)
            .expect("atomic crash child expected target");
        match mode.as_str() {
            "before" => {
                let _ = directory.save_bytes_atomically_expected_with_recovery_hooks(
                    &target,
                    b"new-state",
                    ".child-crash-",
                    AtomicSaveExpectation {
                        require_attached_before_commit: true,
                        file: FileExpectation::Present(&expected),
                        retain_publication_lock: false,
                    },
                    || -> Result<(), String> {
                        fs::write(&ready, b"ready").expect("publish atomic crash readiness");
                        let deadline = Instant::now() + Duration::from_secs(30);
                        loop {
                            if Instant::now() >= deadline {
                                return Err("atomic crash child timed out".to_string());
                            }
                            thread::sleep(Duration::from_secs(1));
                        }
                    },
                    || {},
                );
            }
            "after" => {
                let _ = directory.save_bytes_atomically_expected_with_recovery_hooks(
                    &target,
                    b"new-state",
                    ".child-crash-",
                    AtomicSaveExpectation {
                        require_attached_before_commit: true,
                        file: FileExpectation::Present(&expected),
                        retain_publication_lock: false,
                    },
                    || Ok(()),
                    || {
                        fs::write(&ready, b"ready").expect("publish atomic crash readiness");
                        let deadline = Instant::now() + Duration::from_secs(30);
                        loop {
                            assert!(Instant::now() < deadline, "atomic crash child timed out");
                            thread::sleep(Duration::from_secs(1));
                        }
                    },
                );
            }
            value => panic!("unsupported atomic crash child mode: {value}"),
        }
        panic!("atomic crash child unexpectedly left its commit barrier");
    }

    #[cfg(unix)]
    fn spawn_atomic_crash_child(root: &Path, mode: &str, ready: &Path) -> Child {
        let _ = fs::remove_file(ready);
        Command::new(std::env::current_exe().expect("current test binary"))
            .args([
                "--exact",
                "daemons::state::tests::real_child_atomic_fsync_crash_recovery_matrix",
                "--nocapture",
            ])
            .env(ATOMIC_CRASH_CHILD_ROOT, root)
            .env(ATOMIC_CRASH_CHILD_MODE, mode)
            .env(ATOMIC_CRASH_CHILD_READY, ready)
            .spawn()
            .expect("spawn atomic crash child")
    }

    #[cfg(unix)]
    fn wait_for_atomic_child(child: &mut Child, ready: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect atomic crash child") {
                panic!("atomic crash child exited before readiness: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "atomic crash child did not become ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
