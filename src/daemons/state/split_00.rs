//! T043 split.

use super::*;

#[cfg(debug_assertions)]
pub(crate) fn pause_atomic_publication_phase(
    path: &Path,
    encoded: &[u8],
    phase: &str,
) -> Result<(), String> {
    let Some(expected_phase) = std::env::var_os("NIB_TEST_ATOMIC_PUBLICATION_PHASE") else {
        return Ok(());
    };
    if expected_phase != std::ffi::OsStr::new(phase) {
        return Ok(());
    }
    if let Some(component) = std::env::var_os("NIB_TEST_ATOMIC_PUBLICATION_PATH_COMPONENT") {
        if !path
            .components()
            .any(|candidate| candidate.as_os_str() == component)
        {
            return Ok(());
        }
    }
    if let Some(needle) = std::env::var_os("NIB_TEST_ATOMIC_PUBLICATION_CONTENT") {
        let needle = needle.as_encoded_bytes();
        if needle.is_empty() || !encoded.windows(needle.len()).any(|window| window == needle) {
            return Ok(());
        }
    }
    let ready = std::env::var_os("NIB_TEST_ATOMIC_PUBLICATION_READY")
        .map(PathBuf::from)
        .ok_or_else(|| "missing atomic publication ready path".to_string())?;
    let publishing = ready.with_extension("publishing");
    std::fs::write(&publishing, path.as_os_str().as_encoded_bytes())
        .map_err(|error| format!("failed to prepare atomic phase readiness: {error}"))?;
    std::fs::rename(&publishing, &ready)
        .map_err(|error| format!("failed to publish atomic phase readiness: {error}"))?;
    let resume = std::env::var_os("NIB_TEST_ATOMIC_PUBLICATION_RESUME")
        .map(PathBuf::from)
        .ok_or_else(|| "missing atomic publication resume path".to_string())?;
    let started = Instant::now();
    while !resume.exists() {
        if started.elapsed() >= Duration::from_secs(30) {
            return Err(format!("timed out at atomic publication phase {phase}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
pub(crate) fn pause_atomic_publication_phase(
    _path: &Path,
    _encoded: &[u8],
    _phase: &str,
) -> Result<(), String> {
    Ok(())
}

pub(crate) const STRICT_RECOVERY_LIVE_WRITER_WAIT: Duration = Duration::from_millis(250);
pub(crate) const STRICT_RECOVERY_POLL_INTERVAL: Duration = Duration::from_millis(5);
pub(crate) const FILE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);
pub(crate) const ATOMIC_READ_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandlePublication {
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

pub(crate) fn ensure_atomic_read_deadline(deadline: Instant, path: &Path) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err(format!(
            "atomic state read deadline elapsed: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn wait_for_atomic_read_namespace(deadline: Instant, path: &Path) -> Result<(), String> {
    ensure_atomic_read_deadline(deadline, path)?;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| format!("atomic state read deadline elapsed: {}", path.display()))?;
    thread::sleep(ATOMIC_READ_POLL_INTERVAL.min(remaining));
    Ok(())
}

pub(crate) fn ensure_directory_namespace_deadline(
    deadline: Instant,
    path: &Path,
) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err(format!(
            "state directory namespace deadline elapsed: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) struct AtomicSaveExpectation<'a> {
    pub(crate) require_attached_before_commit: bool,
    pub(crate) file: FileExpectation<'a>,
    pub(crate) retain_publication_lock: bool,
}

pub(crate) struct AtomicSaveHooks<BeforeCommit, AfterEvacuation, BeforeReceipt> {
    pub(crate) before_commit: BeforeCommit,
    pub(crate) after_evacuation: AfterEvacuation,
    pub(crate) before_receipt: BeforeReceipt,
}

#[derive(Clone, Copy)]
pub(crate) struct AtomicRecoveryPolicy {
    pub(crate) skip_live_writer: bool,
    pub(crate) reject_obscured_live_writer: bool,
    pub(crate) strict_deadline: Option<Instant>,
}

pub(crate) struct AtomicRecoveryHooks<'a> {
    pub(crate) previous_open: &'a mut dyn FnMut() -> Result<(), String>,
    pub(crate) live_target: &'a mut dyn FnMut(),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StableEntryKind {
    File,
    Directory,
}

pub(crate) struct StableDirectory {
    pub(crate) path: PathBuf,
    pub(crate) directory: cap_std::fs::Dir,
    pub(crate) identity: crate::fs_security::FileIdentity,
    pub(crate) delete_capable: bool,
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
            let file =
                crate::fs_security::open_directory_child_windows(&self.directory, relative, false)
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
            // Keep the long-lived capability share-compatible with future
            // readers. DELETE access is acquired against this exact identity
            // only at the final rename or removal boundary.
            let file =
                crate::fs_security::open_directory_child_windows(&self.directory, relative, false)
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

    pub(crate) fn create_owned_child_directory_until(
        &self,
        path: &Path,
        deadline: Instant,
    ) -> Result<Self, String> {
        let mut deadline_guard = || ensure_directory_namespace_deadline(deadline, path);
        self.create_child_directory_with_guard_and_hooks(
            path,
            &mut deadline_guard,
            true,
            || Ok(()),
            || Ok(()),
        )
    }

    pub(crate) fn create_owned_child_directory_no_replace_with_guard(
        &self,
        path: &Path,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        namespace_guard()?;
        let relative = self.relative_file(path)?;
        namespace_guard()?;
        if self.entry_kind(path)?.is_some() {
            return Err(format!(
                "state directory appeared after its absence was proven: {}",
                path.display()
            ));
        }
        namespace_guard()?;
        self.verify_visible()?;
        namespace_guard()?;
        self.directory.create_dir(relative).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "state directory appeared during no-replace creation: {}",
                    path.display()
                )
            } else {
                format!("failed to create {}: {error}", path.display())
            }
        })?;
        namespace_guard()?;
        self.sync_directory()?;
        namespace_guard()?;
        let child = self.open_owned_child(path)?;
        namespace_guard()?;
        child.verify_visible_at(path)?;
        namespace_guard()?;
        self.verify_visible()?;
        namespace_guard()?;
        Ok(child)
    }

    #[cfg(test)]
    pub(crate) fn create_owned_child_directory_until_with_hook(
        &self,
        path: &Path,
        deadline: Instant,
        mut before_create: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let mut deadline_guard = || ensure_directory_namespace_deadline(deadline, path);
        self.create_child_directory_with_guard_and_hooks(
            path,
            &mut deadline_guard,
            true,
            &mut before_create,
            || Ok(()),
        )
    }

    pub(crate) fn create_child_directory_with_guard_and_hooks(
        &self,
        path: &Path,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
        delete_capable: bool,
        mut before_create: impl FnMut() -> Result<(), String>,
        mut before_parent_sync: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        namespace_guard()?;
        let relative = self.relative_file(path)?;
        namespace_guard()?;
        if self.entry_kind(path)?.is_some() {
            return Err(format!("state entry already exists: {}", path.display()));
        }
        namespace_guard()?;
        self.verify_visible()?;
        namespace_guard()?;
        before_create()?;
        namespace_guard()?;
        match self.directory.create_dir(relative) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                namespace_guard()?;
                match self.entry_kind(path)? {
                    Some(StableEntryKind::Directory) => {}
                    Some(StableEntryKind::File) => {
                        return Err(format!(
                            "state directory component is not a local directory: {}",
                            path.display()
                        ));
                    }
                    None => {
                        return Err(format!(
                            "state directory disappeared during concurrent creation: {}",
                            path.display()
                        ));
                    }
                }
            }
            Err(error) => {
                return Err(format!("failed to create {}: {error}", path.display()));
            }
        }
        namespace_guard()?;
        before_parent_sync()?;
        namespace_guard()?;
        self.sync_directory()?;
        namespace_guard()?;
        let child = if delete_capable {
            self.open_owned_child(path)?
        } else {
            self.open_child(path)?
        };
        namespace_guard()?;
        child.verify_visible_at(path)?;
        namespace_guard()?;
        self.verify_visible()?;
        namespace_guard()?;
        Ok(child)
    }

    pub(crate) fn open_or_create_descendant_directory_with_guard(
        &self,
        path: &Path,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
        mut before_create: impl FnMut(&Path) -> Result<(), String>,
    ) -> Result<Self, String> {
        self.open_or_create_descendant_directory_with_guard_and_hooks(
            path,
            &mut namespace_guard,
            &mut before_create,
            |_| Ok(()),
        )
    }

    pub(crate) fn open_or_create_descendant_directory_with_guard_and_hooks(
        &self,
        path: &Path,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
        mut before_create: impl FnMut(&Path) -> Result<(), String>,
        mut before_parent_sync: impl FnMut(&Path) -> Result<(), String>,
    ) -> Result<Self, String> {
        namespace_guard()?;
        let relative = path.strip_prefix(&self.path).map_err(|_| {
            format!(
                "state directory is not below the retained ancestor {}: {}",
                self.path.display(),
                path.display()
            )
        })?;
        if relative.as_os_str().is_empty() {
            self.verify_visible()?;
            namespace_guard()?;
            let current = self.try_clone()?;
            namespace_guard()?;
            return Ok(current);
        }

        let mut current = self.try_clone()?;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(format!(
                    "state directory descendant contains an unsafe component: {}",
                    path.display()
                ));
            };
            namespace_guard()?;
            let child_path = current.path.join(name);
            let entry_kind = current.entry_kind(&child_path).map_err(|error| {
                format!(
                    "state directory component must be a local directory and not a symlink or reparse point: {} ({error})",
                    child_path.display()
                )
            })?;
            current = match entry_kind {
                Some(StableEntryKind::Directory) => {
                    before_parent_sync(&child_path)?;
                    namespace_guard()?;
                    current.sync_directory()?;
                    namespace_guard()?;
                    let child = current.open_child(&child_path)?;
                    namespace_guard()?;
                    child
                }
                Some(StableEntryKind::File) => {
                    return Err(format!(
                        "state directory component is not a local directory: {}",
                        child_path.display()
                    ));
                }
                None => {
                    before_create(&child_path)?;
                    namespace_guard()?;
                    current.create_child_directory_with_guard_and_hooks(
                        &child_path,
                        &mut namespace_guard,
                        false,
                        || Ok(()),
                        || before_parent_sync(&child_path),
                    )?
                }
            };
            namespace_guard()?;
        }
        current.verify_visible()?;
        namespace_guard()?;
        Ok(current)
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

    pub(crate) fn open_read_write_if_exists(&self, path: &Path) -> Result<Option<File>, String> {
        self.open_read_write_if_exists_with_hook(path, || Ok(()))
    }

    pub(crate) fn open_read_write_if_exists_with_hook(
        &self,
        path: &Path,
        after_open: impl FnOnce() -> Result<(), String>,
    ) -> Result<Option<File>, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true);
        configure_capability_no_follow(&mut options);
        configure_capability_delete_access(&mut options, true, true);
        let file = match self.directory.open_with(relative, &options) {
            Ok(file) => file.into_std(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("failed to open {}: {error}", path.display()));
            }
        };
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        after_open()?;

        let mut visible_options = cap_std::fs::OpenOptions::new();
        visible_options.read(true);
        configure_capability_no_follow(&mut visible_options);
        let visible = match self.directory.open_with(relative, &visible_options) {
            Ok(file) => file.into_std(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!("failed to re-open {}: {error}", path.display()));
            }
        };
        validate_stable_file_metadata(
            path,
            &visible.metadata().map_err(|error| error.to_string())?,
        )?;
        if !same_open_file_identity(&file, &visible)? {
            return Err(format!(
                "state file identity changed while it was in use: {}",
                path.display()
            ));
        }
        Ok(Some(file))
    }

    pub(crate) fn open_read_write_create(&self, path: &Path) -> Result<File, String> {
        self.open_read_write_create_with_guard(path, || Ok(()))
    }

    pub(crate) fn open_read_write_create_with_guard(
        &self,
        path: &Path,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<File, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        configure_capability_owner_only(&mut options);
        configure_capability_no_follow(&mut options);
        configure_capability_delete_access(&mut options, true, true);
        namespace_guard()?;
        let file = self
            .directory
            .open_with(relative, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        self.verify_file_identity(path, &file)?;
        namespace_guard()?;
        Ok(file)
    }

    pub(crate) fn open_read_write_create_new_with_guard(
        &self,
        path: &Path,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<File, String> {
        let relative = self.relative_file(path)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        configure_capability_owner_only(&mut options);
        configure_capability_no_follow(&mut options);
        configure_capability_delete_access(&mut options, true, true);
        namespace_guard()?;
        let file = self
            .directory
            .open_with(relative, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
        validate_stable_file_metadata(path, &file.metadata().map_err(|error| error.to_string())?)?;
        self.verify_file_identity(path, &file)?;
        namespace_guard()?;
        Ok(file)
    }

    pub(crate) fn hard_link_to(
        &self,
        source: &Path,
        destination_directory: &Self,
        destination: &Path,
    ) -> Result<(), String> {
        self.hard_link_to_with_guard(source, destination_directory, destination, || Ok(()))
    }

    pub(crate) fn hard_link_to_with_guard(
        &self,
        source: &Path,
        destination_directory: &Self,
        destination: &Path,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let source = self.relative_file(source)?;
        let destination_relative = destination_directory.relative_file(destination)?;
        namespace_guard()?;
        match self.directory.hard_link(
            source,
            &destination_directory.directory,
            destination_relative,
        ) {
            Ok(()) => namespace_guard(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => namespace_guard(),
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

    #[cfg(not(windows))]
    pub(crate) fn remove_file_bound_without_sync(&self, path: &Path) -> Result<(), String> {
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
        self.recover_quarantined_file_guarded(path, quarantine_prefix, &mut || Ok(()))
    }

    pub(crate) fn recover_quarantined_file_guarded(
        &self,
        path: &Path,
        quarantine_prefix: &str,
        namespace_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        namespace_guard()?;
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
                    self.remove_visible_file_if_matches_guarded(
                        &quarantine,
                        &quarantine_file,
                        namespace_guard,
                        || Ok(()),
                    )
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

    pub(crate) fn remove_file_if_matches_with_guard(
        &self,
        path: &Path,
        expected: &File,
        quarantine_prefix: &str,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.recover_quarantined_file_guarded(path, quarantine_prefix, &mut namespace_guard)?;
        let quarantine =
            self.deterministic_artifact_path(path, quarantine_prefix, ".quarantine")?;
        self.verify_file_identity(path, expected)?;
        namespace_guard()?;
        self.verify_visible()?;
        self.move_open_file_no_replace_bound_guarded(
            path,
            expected,
            &quarantine,
            &mut namespace_guard,
        )?;
        self.verify_visible()?;
        namespace_guard()?;
        self.remove_visible_file_if_matches_guarded(
            &quarantine,
            expected,
            &mut namespace_guard,
            || Ok(()),
        )
    }

    pub(crate) fn remove_visible_file_if_matches_direct(
        &self,
        path: &Path,
        expected: &File,
    ) -> Result<(), String> {
        self.verify_file_identity(path, expected)?;
        self.remove_visible_file_if_matches(path, expected, || Ok(()))
    }

    pub(crate) fn remove_visible_file_if_matches_direct_with_guard(
        &self,
        path: &Path,
        expected: &File,
        mut namespace_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.remove_visible_file_if_matches_guarded(path, expected, &mut namespace_guard, || Ok(()))
    }

    /// Opens one stable, read-only snapshot of an atomically published file.
    ///
    /// A live atomic writer may temporarily evacuate `path` to its deterministic
    /// previous artifact. Treating that gap as a missing file would let a reader
    /// incorrectly substitute defaults. This read-only helper waits for those
    /// transaction artifacts to clear without recovering, deleting, or creating
    /// any namespace entry, and returns an opened file whose identity was
    /// verified at the read linearization point.
    pub(crate) fn open_atomic_file_read_only_until(
        &self,
        path: &Path,
        temporary_prefix: &str,
        deadline: Instant,
    ) -> Result<Option<File>, String> {
        let destination = self.relative_file(path)?.to_path_buf();
        let temporary = deterministic_artifact_name(
            temporary_prefix,
            destination.as_os_str().as_encoded_bytes(),
            ".tmp",
        );
        let temporary_path = self.path.join(temporary);
        let previous =
            deterministic_previous_artifact_name(temporary_prefix, destination.as_os_str())?;
        let previous_path = self.path.join(previous);

        loop {
            ensure_atomic_read_deadline(deadline, path)?;
            if self.path_exists(&temporary_path)? || self.path_exists(&previous_path)? {
                wait_for_atomic_read_namespace(deadline, path)?;
                continue;
            }
            if !self.path_exists(path)? {
                ensure_atomic_read_deadline(deadline, path)?;
                if self.path_exists(&temporary_path)? || self.path_exists(&previous_path)? {
                    wait_for_atomic_read_namespace(deadline, path)?;
                    continue;
                }
                return Ok(None);
            }

            let file = match self.open_read(path) {
                Ok(file) => file,
                Err(error) => {
                    if !self.path_exists(path)?
                        || self.path_exists(&temporary_path)?
                        || self.path_exists(&previous_path)?
                    {
                        wait_for_atomic_read_namespace(deadline, path)?;
                        continue;
                    }
                    return Err(error);
                }
            };
            ensure_atomic_read_deadline(deadline, path)?;
            if self.path_exists(&temporary_path)? || self.path_exists(&previous_path)? {
                wait_for_atomic_read_namespace(deadline, path)?;
                continue;
            }
            match self.verify_file_identity(path, &file) {
                Ok(()) => {
                    ensure_atomic_read_deadline(deadline, path)?;
                    return Ok(Some(file));
                }
                Err(error) => {
                    if !self.path_exists(path)?
                        || self.path_exists(&temporary_path)?
                        || self.path_exists(&previous_path)?
                    {
                        wait_for_atomic_read_namespace(deadline, path)?;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
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
        #[cfg(windows)]
        let mutation_directory = self.open_directory_for_mutation(source, expected)?;
        rename_open_directory_no_replace_platform(
            &self.directory,
            self.relative_file(source)?,
            #[cfg(not(windows))]
            &expected.directory,
            #[cfg(windows)]
            &mutation_directory,
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

    pub(crate) fn rename_child_directory_until(
        &self,
        source: &Path,
        expected: &Self,
        destination: &Path,
        deadline: Instant,
    ) -> Result<(), String> {
        self.rename_child_directory_until_with_hook(source, expected, destination, deadline, || {
            Ok(())
        })
    }

    pub(crate) fn rename_child_directory_until_with_hook(
        &self,
        source: &Path,
        expected: &Self,
        destination: &Path,
        deadline: Instant,
        before_publication: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.rename_child_directory_until_with_hooks(
            source,
            expected,
            destination,
            deadline,
            before_publication,
            || Ok(()),
        )
    }
}
