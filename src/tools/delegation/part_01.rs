//! Split for T043 C02.

use super::*;

pub(crate) fn open_owner_lease_directories(
    project_root: &Path,
    create: bool,
) -> Result<
    (
        crate::daemons::state::StableDirectory,
        crate::daemons::state::StableDirectory,
    ),
    String,
> {
    let nib = project_root.join(".nib");
    let visible = owner_lease_directory(project_root);
    if create {
        crate::fs_security::ensure_directory_without_symlinks(&visible)
            .map_err(|error| format!("subagent owner lease directory is unsafe: {error}"))?;
    } else {
        crate::fs_security::verify_directory_without_symlinks(&visible)
            .map_err(|error| format!("subagent owner lease directory is unsafe: {error}"))?;
    }
    let metadata = std::fs::symlink_metadata(&visible)
        .map_err(|error| format!("failed to inspect subagent owner lease directory: {error}"))?;
    let canonical = visible
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent owner lease directory: {error}"))?;
    let within_project =
        crate::fs_security::canonical_path_starts_with(&canonical, project_root)
            .map_err(|error| format!("failed to resolve subagent project root: {error}"))?;
    if crate::fs_security::metadata_is_link_or_reparse(&metadata)
        || !metadata.is_dir()
        || !within_project
    {
        return Err(format!(
            "subagent owner lease path must be a local project directory: {}",
            visible.display()
        ));
    }
    let anchor_directory = crate::daemons::state::StableDirectory::open(&nib)?;
    let visible_directory = anchor_directory.open_child(&visible)?;
    Ok((anchor_directory, visible_directory))
}

pub(crate) fn owner_lease_path(project_root: &Path, lease_id: &str) -> Result<PathBuf, String> {
    let parsed = uuid::Uuid::parse_str(lease_id)
        .map_err(|_| "subagent owner lease identifier is invalid".to_string())?;
    if parsed.to_string() != lease_id {
        return Err("subagent owner lease identifier is not canonical".to_string());
    }
    Ok(owner_lease_directory(project_root).join(format!("{lease_id}{OWNER_LEASE_SUFFIX}")))
}

pub(crate) fn owner_lease_anchor_path(
    project_root: &Path,
    lease_id: &str,
) -> Result<PathBuf, String> {
    let parsed = uuid::Uuid::parse_str(lease_id)
        .map_err(|_| "subagent owner lease identifier is invalid".to_string())?;
    if parsed.to_string() != lease_id {
        return Err("subagent owner lease identifier is not canonical".to_string());
    }
    Ok(project_root.join(".nib").join(format!(
        "{OWNER_LEASE_ANCHOR_PREFIX}{lease_id}{OWNER_LEASE_ANCHOR_SUFFIX}"
    )))
}

#[derive(Debug, Clone)]
pub(crate) enum CancelSubagentResolution {
    Cancelled {
        record: SubagentRecord,
    },
    Terminal {
        record: SubagentRecord,
    },
    Unresolved {
        manager_stopped: bool,
        observed_status: Option<String>,
        error: String,
    },
}

pub(crate) struct SubagentCancellationWorker {
    pub(crate) worker: Option<std::thread::JoinHandle<()>>,
}

impl SubagentCancellationWorker {
    pub(crate) fn new(worker: std::thread::JoinHandle<()>) -> Self {
        Self {
            worker: Some(worker),
        }
    }

    pub(crate) fn join(&mut self) -> Result<(), String> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().map_err(|_| {
            "subagent cancellation reconciliation worker panicked before shutdown".to_string()
        })
    }
}

impl Drop for SubagentCancellationWorker {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

pub(crate) struct NonInteractiveSubagentApproval;

#[derive(Debug)]
pub(crate) struct RepositoryMergeLock {
    pub(crate) _anchor_file: File,
}

impl RepositoryMergeLock {
    pub(crate) async fn acquire(
        project_root: &Path,
        cancellation: Option<&crate::agent::CancellationSignal>,
    ) -> Result<Self, String> {
        Self::acquire_with_timeout_and_cancellation(
            project_root,
            REPOSITORY_MERGE_LOCK_TIMEOUT,
            cancellation,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn acquire_with_timeout(
        project_root: &Path,
        timeout: Duration,
    ) -> Result<Self, String> {
        Self::acquire_with_timeout_and_cancellation(project_root, timeout, None).await
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) async fn acquire_with_timeout_and_cancellation(
        project_root: &Path,
        timeout: Duration,
        cancellation: Option<&crate::agent::CancellationSignal>,
    ) -> Result<Self, String> {
        let started = Instant::now();
        let deadline = started.checked_add(timeout).unwrap_or(started);
        ensure_repository_merge_lock_not_cancelled(cancellation)?;
        if Instant::now() >= deadline {
            return Err(repository_merge_lock_timeout(
                &records_dir(project_root).join(".merge.lock"),
                timeout,
            ));
        }
        let directory = ensure_records_directory_until(project_root, Some(deadline))?;
        let path = directory.join(".merge.lock");
        let anchor_path = repository_merge_lock_anchor_path(project_root);
        let anchor_directory = anchor_path.parent().ok_or_else(|| {
            format!(
                "repository merge lock anchor has no parent: {}",
                anchor_path.display()
            )
        })?;
        let project_directory = crate::daemons::state::StableDirectory::open(project_root)?;
        let mut setup_guard = || {
            ensure_repository_merge_lock_not_cancelled(cancellation)?;
            if Instant::now() >= deadline {
                return Err(repository_merge_lock_timeout(&path, timeout));
            }
            Ok(())
        };
        let lock_directory = project_directory.open_or_create_descendant_directory_with_guard(
            &directory,
            &mut setup_guard,
            |missing| {
                Err(format!(
                    "repository merge lock directory does not exist: {}",
                    missing.display()
                ))
            },
        )?;
        let anchor_directory = project_directory.open_or_create_descendant_directory_with_guard(
            anchor_directory,
            &mut setup_guard,
            |missing| {
                Err(format!(
                    "repository merge lock anchor directory does not exist: {}",
                    missing.display()
                ))
            },
        )?;
        setup_guard()?;
        let anchor_file = crate::daemons::state::open_daemon_lock_anchor_bound_with_guard(
            &lock_directory,
            &path,
            &anchor_directory,
            &anchor_path,
            &mut setup_guard,
        )?;
        setup_guard()?;
        let locked_identity = repository_lock_identity(&anchor_file, &anchor_path)?;
        loop {
            ensure_repository_merge_lock_not_cancelled(cancellation)?;
            if Instant::now() >= deadline {
                return Err(repository_merge_lock_timeout(&path, timeout));
            }
            match anchor_file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error))
                    if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!("failed to acquire repository merge lock: {error}"));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(repository_merge_lock_timeout(&path, timeout));
            }
            let delay = Duration::from_millis(25).min(deadline - now);
            if let Some(cancellation) = cancellation {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancellation.cancelled() => {
                        return Err(repository_merge_lock_cancelled());
                    }
                }
            } else {
                tokio::time::sleep(delay).await;
            }
        }
        ensure_repository_merge_lock_not_cancelled(cancellation)?;
        setup_guard()?;
        crate::daemons::state::repair_daemon_lock_anchor_with_guard(
            &lock_directory,
            &path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
            &mut setup_guard,
        )?;
        setup_guard()?;
        crate::daemons::state::verify_daemon_lock_paths_bound(
            &lock_directory,
            &path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
        )?;
        setup_guard()?;
        Ok(Self {
            _anchor_file: anchor_file,
        })
    }
}

pub(crate) fn ensure_repository_merge_lock_not_cancelled(
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<(), String> {
    if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
        return Err(repository_merge_lock_cancelled());
    }
    Ok(())
}

pub(crate) fn repository_merge_lock_cancelled() -> String {
    "repository merge lock acquisition was cancelled".to_string()
}

pub(crate) fn with_bounded_delegation_lock_in<T>(
    lock_path: &Path,
    protected_directory: &Path,
    timeout: Duration,
    operation: impl FnOnce(&crate::daemons::state::StableDirectory, Instant) -> Result<T, String>,
) -> Result<T, String> {
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    with_delegation_lock_in_deadline(
        lock_path,
        protected_directory,
        deadline,
        Some(timeout),
        operation,
    )
}

pub(crate) fn with_bounded_delegation_lock_in_until<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Instant,
    operation: impl FnOnce(&crate::daemons::state::StableDirectory, Instant) -> Result<T, String>,
) -> Result<T, String> {
    with_delegation_lock_in_deadline(lock_path, protected_directory, deadline, None, operation)
}

pub(crate) fn with_delegation_lock_in_deadline<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Instant,
    timeout: Option<Duration>,
    operation: impl FnOnce(&crate::daemons::state::StableDirectory, Instant) -> Result<T, String>,
) -> Result<T, String> {
    with_delegation_lock_in_deadline_with_setup_hook(
        lock_path,
        protected_directory,
        deadline,
        timeout,
        operation,
        |_| Ok(()),
    )
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn with_delegation_lock_in_deadline_with_setup_hook<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Instant,
    timeout: Option<Duration>,
    operation: impl FnOnce(&crate::daemons::state::StableDirectory, Instant) -> Result<T, String>,
    mut before_setup_mutation: impl FnMut(&Path) -> Result<(), String>,
) -> Result<T, String> {
    ensure_delegation_lock_deadline(deadline, lock_path, timeout)?;
    let lock_parent = lock_path
        .parent()
        .ok_or_else(|| format!("delegation lock has no parent: {}", lock_path.display()))?;
    let file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("delegation lock has no file name: {}", lock_path.display()))?;
    let project_root = delegation_lock_project_root(lock_path)?;
    let project_directory = crate::daemons::state::StableDirectory::open(&project_root)?;
    ensure_delegation_lock_deadline(deadline, lock_path, timeout)?;
    let mut deadline_guard = || ensure_delegation_lock_deadline(deadline, lock_path, timeout);
    let lock_directory = project_directory.open_or_create_descendant_directory_with_guard(
        lock_parent,
        &mut deadline_guard,
        &mut before_setup_mutation,
    )?;
    deadline_guard()?;
    let lock_path = lock_directory.path().join(file_name);
    let anchor_path = crate::daemons::state::daemon_lock_anchor_path(&lock_path)?;
    let anchor_parent = anchor_path.parent().ok_or_else(|| {
        format!(
            "delegation lock anchor has no parent: {}",
            anchor_path.display()
        )
    })?;
    let anchor_directory = project_directory.open_or_create_descendant_directory_with_guard(
        anchor_parent,
        &mut deadline_guard,
        &mut before_setup_mutation,
    )?;
    deadline_guard()?;
    let protected_directory = project_directory.open_or_create_descendant_directory_with_guard(
        protected_directory,
        &mut deadline_guard,
        |missing| {
            Err(format!(
                "delegation protected directory does not exist: {}",
                missing.display()
            ))
        },
    )?;
    deadline_guard()?;
    if !lock_directory.path_exists(&lock_path)? && !anchor_directory.path_exists(&anchor_path)? {
        before_setup_mutation(&lock_path)?;
        deadline_guard()?;
    }
    let anchor_file = crate::daemons::state::open_daemon_lock_anchor_bound_with_guard(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &mut deadline_guard,
    )?;
    deadline_guard()?;
    let locked_identity = repository_lock_identity(&anchor_file, &anchor_path)?;
    loop {
        ensure_delegation_lock_deadline(deadline, &lock_path, timeout)?;
        match anchor_file.try_lock() {
            Ok(()) => {
                ensure_delegation_lock_deadline(deadline, &lock_path, timeout)?;
                break;
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error))
                if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to acquire delegation state lock {}: {error}",
                    lock_path.display()
                ));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(delegation_lock_deadline_error(&lock_path, timeout));
        }
        std::thread::sleep(Duration::from_millis(25).min(deadline - now));
    }

    deadline_guard()?;
    if !anchor_directory.path_exists(&anchor_path)? {
        before_setup_mutation(&anchor_path)?;
        deadline_guard()?;
    }
    crate::daemons::state::repair_daemon_lock_anchor_with_guard(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &locked_identity,
        &mut deadline_guard,
    )?;
    deadline_guard()?;

    let verify_lock_domain = || -> Result<(), String> {
        deadline_guard()?;
        lock_directory.verify_visible()?;
        deadline_guard()?;
        anchor_directory.verify_visible()?;
        deadline_guard()?;
        crate::daemons::state::verify_daemon_lock_paths_bound(
            &lock_directory,
            &lock_path,
            &anchor_directory,
            &anchor_path,
            &locked_identity,
        )?;
        deadline_guard()?;
        protected_directory.verify_visible()?;
        deadline_guard()
    };
    verify_lock_domain()?;
    ensure_delegation_lock_deadline(deadline, &lock_path, timeout)?;
    let result = operation(&protected_directory, deadline);
    let operation_deadline = ensure_delegation_lock_deadline(deadline, &lock_path, timeout);
    let attachment = verify_lock_domain();
    match (attachment, operation_deadline, result) {
        (Err(error), _, _) => Err(error),
        (Ok(()), Err(error), _) => Err(error),
        (Ok(()), Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(()), Ok(value)) => Ok(value),
    }
}

pub(crate) fn delegation_lock_project_root(lock_path: &Path) -> Result<PathBuf, String> {
    let mut current = lock_path.parent();
    while let Some(directory) = current {
        if directory.file_name() == Some(std::ffi::OsStr::new(".nib")) {
            return directory.parent().map(Path::to_path_buf).ok_or_else(|| {
                format!(
                    "delegation state directory has no project root: {}",
                    lock_path.display()
                )
            });
        }
        current = directory.parent();
    }
    Err(format!(
        "delegation lock is not inside the project .nib namespace: {}",
        lock_path.display()
    ))
}

pub(crate) fn with_delegation_lock_in_deadline_bound_to<T>(
    lock_path: &Path,
    protected_directory: &crate::daemons::state::StableDirectory,
    deadline: Instant,
    timeout: Option<Duration>,
    operation: impl FnOnce(&crate::daemons::state::StableDirectory, Instant) -> Result<T, String>,
) -> Result<T, String> {
    ensure_delegation_lock_deadline(deadline, lock_path, timeout)?;
    protected_directory.verify_visible()?;
    with_delegation_lock_in_deadline(
        lock_path,
        protected_directory.path(),
        deadline,
        timeout,
        |opened_directory, deadline| {
            if !opened_directory.same_identity(protected_directory) {
                return Err(format!(
                    "delegation protected directory identity changed before lock acquisition: {}",
                    protected_directory.path().display()
                ));
            }
            protected_directory.verify_visible()?;
            let result = operation(protected_directory, deadline);
            let attachment = protected_directory.verify_visible();
            match (attachment, result) {
                (Err(error), _) => Err(error),
                (Ok(()), Err(error)) => Err(error),
                (Ok(()), Ok(value)) => Ok(value),
            }
        },
    )
}

pub(crate) fn ensure_delegation_lock_deadline(
    deadline: Instant,
    lock_path: &Path,
    timeout: Option<Duration>,
) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err(delegation_lock_deadline_error(lock_path, timeout));
    }
    Ok(())
}

pub(crate) fn delegation_lock_deadline_error(
    lock_path: &Path,
    timeout: Option<Duration>,
) -> String {
    match timeout {
        Some(timeout) => format!(
            "timed out acquiring delegation state lock {} after {} seconds",
            lock_path.display(),
            timeout.as_secs_f64()
        ),
        None => format!(
            "delegation state lock deadline elapsed: {}",
            lock_path.display()
        ),
    }
}

pub(crate) fn repository_merge_lock_timeout(path: &Path, timeout: Duration) -> String {
    format!(
        "timed out acquiring repository merge lock {} after {} seconds",
        path.display(),
        timeout.as_secs_f64()
    )
}

pub(crate) fn repository_merge_lock_anchor_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".nib")
        .join(".subagents.merge.lock.anchor")
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn open_repository_merge_lock_anchor(
    lock_path: &Path,
    anchor_path: &Path,
) -> Result<File, String> {
    let lock_exists = repository_merge_lock_path_exists(lock_path)?;
    let anchor_exists = repository_merge_lock_path_exists(anchor_path)?;
    match (lock_exists, anchor_exists) {
        (false, false) => {
            drop(open_repository_merge_lock(lock_path)?);
            create_repository_merge_lock_link(lock_path, anchor_path)?;
        }
        (true, false) => create_repository_merge_lock_link(lock_path, anchor_path)?,
        (false, true) => create_repository_merge_lock_link(anchor_path, lock_path)?,
        (true, true) => {}
    }

    let anchor_file = open_repository_merge_lock(anchor_path)?;
    let anchor_identity = repository_lock_identity(&anchor_file, anchor_path)?;
    let lock_identity = open_repository_lock_identity(lock_path)?;
    if anchor_identity != lock_identity {
        return Err(format!(
            "repository merge lock and persistent anchor have different identities: {}",
            lock_path.display()
        ));
    }
    Ok(anchor_file)
}

#[cfg(all(test, not(any(unix, windows))))]
pub(crate) fn open_repository_merge_lock_anchor(
    lock_path: &Path,
    _anchor_path: &Path,
) -> Result<File, String> {
    Err(format!(
        "repository merge lock anchors are unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn repository_merge_lock_path_exists(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if crate::fs_security::metadata_is_link_or_reparse(&metadata) => Err(format!(
            "repository merge lock must not be a symlink or reparse point: {}",
            path.display()
        )),
        Ok(metadata) if !metadata.is_file() => Err(format!(
            "repository merge lock must be a regular local file: {}",
            path.display()
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "failed to inspect repository merge lock {}: {error}",
            path.display()
        )),
    }
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn create_repository_merge_lock_link(
    source: &Path,
    destination: &Path,
) -> Result<(), String> {
    match std::fs::hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!(
            "failed to create persistent repository merge lock anchor {} from {}: {error}",
            destination.display(),
            source.display()
        )),
    }
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn open_repository_merge_lock(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
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
    let file = options
        .open(path)
        .map_err(|error| format!("failed to open repository merge lock: {error}"))?;
    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect repository merge lock: {error}"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect open repository merge lock: {error}"))?;
    if crate::fs_security::metadata_is_link_or_reparse(&path_metadata)
        || !path_metadata.is_file()
        || crate::fs_security::metadata_is_link_or_reparse(&opened_metadata)
        || !opened_metadata.is_file()
    {
        return Err(format!(
            "repository merge lock must be a regular local file: {}",
            path.display()
        ));
    }
    validate_repository_lock_path(&file, path)?;
    Ok(file)
}

#[cfg(all(test, not(any(unix, windows))))]
pub(crate) fn open_repository_merge_lock(path: &Path) -> Result<File, String> {
    Err(format!(
        "stable no-follow repository merge locks are unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
pub(crate) fn repository_lock_identity(
    file: &File,
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(
        file.try_clone()
            .map_err(|error| format!("failed to clone repository merge lock: {error}"))?,
    )
    .map_err(|error| {
        format!(
            "failed to identify repository merge lock {}: {error}",
            path.display()
        )
    })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn repository_lock_identity(_file: &File, path: &Path) -> Result<(), String> {
    Err(format!(
        "repository merge lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn open_repository_lock_identity(
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    let file = open_repository_lock_probe(path)?;
    let opened = crate::fs_security::FileIdentity::from_file(file).map_err(|error| {
        format!(
            "failed to identify repository merge lock path {}: {error}",
            path.display()
        )
    })?;
    let visible = crate::fs_security::FileIdentity::from_file(open_repository_lock_probe(path)?)
        .map_err(|error| {
            format!(
                "failed to re-identify repository merge lock path {}: {error}",
                path.display()
            )
        })?;
    if opened != visible {
        return Err(format!(
            "repository merge lock changed while its path identity was checked: {}",
            path.display()
        ));
    }
    Ok(opened)
}

#[cfg(all(test, not(any(unix, windows))))]
pub(crate) fn open_repository_lock_identity(path: &Path) -> Result<(), String> {
    Err(format!(
        "repository merge lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(test)]
pub(crate) fn validate_repository_lock_path(file: &File, path: &Path) -> Result<(), String> {
    let opened_identity = repository_lock_identity(file, path)?;
    let path_identity = open_repository_lock_identity(path)?;
    if opened_identity != path_identity {
        return Err(format!(
            "repository merge lock changed while it was acquired: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn open_repository_lock_probe(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("failed to re-open repository merge lock: {error}"))?;
    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect repository merge lock: {error}"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect open repository merge lock: {error}"))?;
    if crate::fs_security::metadata_is_link_or_reparse(&path_metadata)
        || !path_metadata.is_file()
        || crate::fs_security::metadata_is_link_or_reparse(&opened_metadata)
        || !opened_metadata.is_file()
    {
        return Err(format!(
            "repository merge lock must be a regular local file and must not be a symlink or reparse point: {}",
            path.display()
        ));
    }
    Ok(file)
}

#[async_trait::async_trait]
impl ApprovalHandler for NonInteractiveSubagentApproval {
    fn approval_ceiling(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: crate::tools::classifier::ToolRisk,
    ) -> Option<ApprovalDecision> {
        if call.tool_name == "approve_plan" && level == PermissionLevel::Plan {
            return None;
        }
        let mutating_level = !matches!(level, PermissionLevel::ReadOnly | PermissionLevel::Plan);
        let mutating_risk = risk != crate::tools::classifier::ToolRisk::ReadOnly;
        (mutating_level || mutating_risk).then(|| {
            ApprovalDecision::denied_by_policy(
                "spawned subagents cannot obtain mutating approval noninteractively",
            )
        })
    }

    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision {
        if call.tool_name == "approve_plan" && level == PermissionLevel::Plan {
            return ApprovalDecision::granted_policy();
        }
        ApprovalDecision::denied_by_policy(
            "spawned subagents cannot request approval from shared stdin",
        )
    }
}

#[cfg(test)]
pub(crate) struct SubagentRunGuard {
    pub(crate) project_root: PathBuf,
    pub(crate) id: String,
    pub(crate) execution_generation: u64,
    pub(crate) lease_id: String,
    pub(crate) owner_lease: Option<SubagentOwnerLease>,
    pub(crate) armed: bool,
    pub(crate) reason: String,
}

pub(crate) struct PreparedSubagentTask {
    pub(crate) record: SubagentRecord,
    pub(crate) worktree: crate::sandbox::worktree::Worktree,
    pub(crate) owner_lease: SubagentOwnerLease,
}

#[cfg(test)]
impl SubagentRunGuard {
    pub(crate) fn new(project_root: PathBuf, id: String, owner_lease: SubagentOwnerLease) -> Self {
        Self {
            project_root,
            id,
            execution_generation: owner_lease.execution_generation,
            lease_id: owner_lease.lease_id.clone(),
            owner_lease: Some(owner_lease),
            armed: true,
            reason: INTERRUPTED_ERROR.to_string(),
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
        self.cleanup_owner_lease();
    }

    pub(crate) fn cleanup_owner_lease(&mut self) {
        let Some(owner_lease) = self.owner_lease.take() else {
            return;
        };
        if let Err(error) = owner_lease.remove() {
            persist_owner_lease_cleanup_error(
                &self.project_root,
                &self.id,
                self.execution_generation,
                &self.lease_id,
                &error,
            );
        }
    }
}

#[cfg(test)]
impl Drop for SubagentRunGuard {
    fn drop(&mut self) {
        let can_cleanup = if self.armed {
            persist_interrupted_subagent(
                &self.project_root,
                &self.id,
                self.execution_generation,
                &self.lease_id,
                &self.reason,
            )
        } else {
            true
        };
        if can_cleanup {
            self.cleanup_owner_lease();
        }
    }
}

pub(crate) enum SubagentAuditPreparationPlan {
    Provided {
        store: crate::session::SessionStore,
        target: SubagentAuditTarget,
        encoded: Value,
        runtime_config: crate::config::NibConfig,
    },
    Fallback(crate::session::SessionDirectoryPreflight),
}

impl SubagentAuditPreparationPlan {
    pub(crate) fn runtime_config(&self) -> &crate::config::NibConfig {
        match self {
            Self::Provided { runtime_config, .. } => runtime_config,
            Self::Fallback(preflight) => preflight.runtime_config(),
        }
    }

    pub(crate) fn verify_continuity(&self) -> Result<(), String> {
        match self {
            Self::Provided { store, target, .. } => {
                if &subagent_audit_target_for_store(store)? != target {
                    return Err(
                        "provided subagent audit destination changed after preflight".to_string(),
                    );
                }
                Ok(())
            }
            Self::Fallback(preflight) => preflight.verify_continuity(
                Instant::now()
                    .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
                    .unwrap_or_else(Instant::now),
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn fallback_sessions_dir(&self) -> Option<&Path> {
        match self {
            Self::Fallback(preflight) => Some(preflight.sessions_dir()),
            Self::Provided { .. } => None,
        }
    }

    pub(crate) fn durable_audit_destination(&self) -> (&Path, Option<SubagentAuditTarget>) {
        match self {
            Self::Fallback(preflight) => (preflight.sessions_dir(), None),
            Self::Provided { target, .. } => (&target.sessions_dir, Some(target.clone())),
        }
    }

    #[cfg(test)]
    pub(crate) fn fallback_namespace_plan(
        &self,
        transaction_id: &str,
        worktree: Option<&crate::sandbox::worktree::Worktree>,
    ) -> Result<Option<crate::session::SessionNamespacePreparationPlan>, String> {
        match self {
            Self::Fallback(preflight) => preflight
                .durable_preparation_plan_after_owned_worktree(
                    transaction_id,
                    Instant::now()
                        .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
                        .ok_or_else(|| {
                            "subagent audit namespace plan deadline overflow".to_string()
                        })?,
                    worktree,
                )
                .map(Some),
            Self::Provided { .. } => Ok(None),
        }
    }

    pub(crate) fn fallback_namespace_plan_after_records(
        &self,
        transaction_id: &str,
        records: &crate::daemons::state::StableDirectory,
    ) -> Result<Option<crate::session::SessionNamespacePreparationPlan>, String> {
        match self {
            Self::Fallback(preflight) => preflight
                .durable_preparation_plan_after_authorized_records(
                    transaction_id,
                    Instant::now()
                        .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
                        .ok_or_else(|| {
                            "subagent audit namespace plan deadline overflow".to_string()
                        })?,
                    records,
                )
                .map(Some),
            Self::Provided { .. } => Ok(None),
        }
    }
}

pub(crate) struct PreparedSubagentAudit {
    pub(crate) encoded: Value,
    pub(crate) fallback: Option<crate::session::SessionStorePreparation>,
}

impl PreparedSubagentAudit {
    #[cfg(test)]
    pub(crate) fn cleanup(self) -> Result<(), String> {
        match self.fallback {
            Some(preparation) => preparation.cleanup(
                Instant::now()
                    .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
                    .unwrap_or_else(Instant::now),
            ),
            None => Ok(()),
        }
    }

    pub(crate) fn cleanup_with_authority(
        mut self,
        authority: &SpawnPreparationAuthority,
    ) -> Result<(), String> {
        #[cfg(test)]
        if consume_spawn_failure(&SPAWN_AUDIT_CLEANUP_FAILURES) {
            if let Some(preparation) = self.fallback.take() {
                preparation.preserve_for_durable_reconciliation();
            }
            return Err("injected subagent audit cleanup failure".to_string());
        }
        match self.fallback.take() {
            Some(preparation) => {
                let deadline = authority.operation_deadline();
                preparation.cleanup_with_guard_preserving_failure(deadline, || {
                    authority.verify_until(deadline)
                })
            }
            None => authority.verify_until(authority.operation_deadline()),
        }
    }

    pub(crate) fn disarm(self) {
        if let Some(preparation) = self.fallback {
            drop(preparation.disarm());
        }
    }
}

#[cfg(test)]
pub(crate) fn spawn_preparation_directory_path(project_root: &Path) -> PathBuf {
    records_dir(project_root).join(SPAWN_PREPARATION_DIRECTORY)
}

pub(crate) fn open_or_create_spawn_preparation_directory(
    records: &crate::daemons::state::StableDirectory,
    deadline: Instant,
) -> Result<
    (
        crate::daemons::state::StableDirectory,
        Option<crate::daemons::state::StableDirectory>,
    ),
    String,
> {
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    records.verify_visible()?;
    let path = records.path().join(SPAWN_PREPARATION_DIRECTORY);
    match records.entry_kind(&path)? {
        Some(crate::daemons::state::StableEntryKind::Directory) => records
            .open_owned_child(&path)
            .map(|directory| (directory, None)),
        Some(crate::daemons::state::StableEntryKind::File) => Err(format!(
            "subagent preparation namespace is not a directory: {}",
            path.display()
        )),
        None => {
            let directory = records
                .create_owned_child_directory_no_replace_with_guard(&path, || {
                    ensure_subagent_reconciliation_deadline(Some(deadline))
                })?;
            Ok((directory, Some(records.try_clone()?)))
        }
    }
}

impl SpawnPreparationIntent {
    // These arguments are the independently persisted authorities in a planned
    // spawn intent; keeping them explicit makes the write-ahead boundary clear.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create(
        records: &crate::daemons::state::StableDirectory,
        subagent_id: &str,
        owner: SubagentOwnerPlan,
        worktree: crate::sandbox::worktree::WorktreePreparationAuthority,
        audit_session_id: &str,
        audit_sessions_dir: &Path,
        audit_namespace_plan: Option<crate::session::SessionNamespacePreparationPlan>,
        audit_target: Option<SubagentAuditTarget>,
    ) -> Result<Self, String> {
        let deadline = Instant::now()
            .checked_add(spawn_preparation_operation_timeout())
            .ok_or_else(|| "subagent preparation deadline overflow".to_string())?;
        let authority = acquire_spawn_preparation_authority_until(records, subagent_id, deadline)?;
        authority.verify_until(deadline)?;
        let (directory, created_directory_parent) =
            open_or_create_spawn_preparation_directory(&authority.records, deadline)?;
        let path = directory.path().join(format!("{subagent_id}.json"));
        let data = SpawnPreparationIntentData {
            version: SPAWN_PREPARATION_VERSION,
            revision: 0,
            phase: SpawnPreparationPhase::Planned,
            subagent_id: subagent_id.to_string(),
            owner,
            worktree,
            audit_session_id: audit_session_id.to_string(),
            audit_sessions_dir: audit_sessions_dir.to_path_buf(),
            audit_namespace_plan,
            audit_target,
            audit_receipt: None,
            process_scope_plan: Some(SubagentProcessScopePlan {
                cleanup_lease_id: uuid::Uuid::new_v4().to_string(),
                supervisor_registration_nonce: uuid::Uuid::new_v4().to_string(),
            }),
            handoff_process_scope: None,
            created_at: Utc::now(),
        };
        let encoded = encode_spawn_preparation_intent(&data)?;
        let receipt = match directory.save_bytes_atomically_expected_with_receipt_and_guard(
            &path,
            &encoded,
            ".nib-subagent-preparation-",
            crate::daemons::state::FileExpectation::Missing,
            || authority.verify_until(deadline),
        ) {
            Ok(receipt) => receipt,
            Err(error) => adopt_spawn_preparation_publication_error(
                &directory,
                &path,
                None,
                &encoded,
                deadline,
                error,
                || authority.verify_until(deadline),
            )?,
        };
        if !receipt.exact_identity {
            return Err("subagent preparation intent lacks exact publication identity".to_string());
        }
        authority.verify_until(deadline)?;
        Ok(Self {
            data,
            authority,
            directory,
            path,
            file: receipt.file,
            created_directory_parent,
        })
    }

    pub(crate) fn revise(
        &mut self,
        phase: SpawnPreparationPhase,
        receipt: Option<crate::session::SessionPreparationReceipt>,
        audit_target: Option<SubagentAuditTarget>,
        handoff_process_scope: Option<crate::sandbox::process::ProcessScopeRecord>,
    ) -> Result<(), String> {
        let deadline = self.authority.operation_deadline();
        self.authority.verify_until(deadline)?;
        let mut next = self.data.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| "subagent preparation revision overflow".to_string())?;
        next.phase = phase;
        if matches!(
            phase,
            SpawnPreparationPhase::RecordPublished
                | SpawnPreparationPhase::ManagerRegistered
                | SpawnPreparationPhase::HandoffProven
        ) {
            if receipt.is_some() {
                return Err("handoff preparation revision cannot replace audit receipt".to_string());
            }
        } else {
            next.audit_receipt = receipt;
        }
        if let Some(target) = audit_target {
            next.audit_target = Some(target);
        }
        if phase == SpawnPreparationPhase::HandoffProven {
            next.handoff_process_scope = handoff_process_scope;
        } else if handoff_process_scope.is_some() {
            return Err(
                "process-scope handoff authority is valid only for the proven phase".to_string(),
            );
        }
        validate_spawn_preparation_revision_successor(&self.data, &next)?;
        let encoded = encode_spawn_preparation_intent(&next)?;
        let publication = match self
            .directory
            .save_bytes_atomically_expected_with_receipt_and_guard(
                &self.path,
                &encoded,
                ".nib-subagent-preparation-",
                crate::daemons::state::FileExpectation::Present(&self.file),
                || self.authority.verify_until(deadline),
            ) {
            Ok(publication) => publication,
            Err(error) => adopt_spawn_preparation_publication_error(
                &self.directory,
                &self.path,
                Some(&self.file),
                &encoded,
                deadline,
                error,
                || self.authority.verify_until(deadline),
            )?,
        };
        if !publication.exact_identity {
            return Err("revised subagent preparation intent lacks exact identity".to_string());
        }
        self.file = publication.file;
        self.data = next;
        self.authority.verify_until(deadline)?;
        Ok(())
    }

    pub(crate) fn cleanup(self) -> Result<(), String> {
        let deadline = self.authority.operation_deadline();
        self.authority.verify_until(deadline)?;
        self.directory.remove_file_if_matches_with_guard(
            &self.path,
            &self.file,
            ".nib-subagent-preparation-delete-",
            || self.authority.verify_until(deadline),
        )?;
        if let Some(parent) = self.created_directory_parent {
            let mut nonempty = false;
            self.directory.for_each_entry_bounded(
                MAX_SUBAGENT_RECORDS,
                MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
                |_| {
                    nonempty = true;
                    Ok(())
                },
            )?;
            if !nonempty {
                let path = self.directory.path().to_path_buf();
                parent.remove_empty_child_directory_if_matches_with_guard(
                    &path,
                    self.directory,
                    || self.authority.verify_until(deadline),
                )?;
            }
        }
        self.authority.verify_until(deadline)?;
        Ok(())
    }
}

pub(crate) fn encode_spawn_preparation_intent(
    data: &SpawnPreparationIntentData,
) -> Result<Vec<u8>, String> {
    validate_spawn_preparation_intent_structure(data)?;
    let encoded = serde_json::to_vec_pretty(data).map_err(|error| error.to_string())?;
    if encoded.len() as u64 > MAX_SPAWN_PREPARATION_BYTES {
        return Err("subagent preparation intent exceeds its size bound".to_string());
    }
    Ok(encoded)
}

pub(crate) fn is_canonical_spawn_preparation_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

/// Validates the one immutable process authority chain carried by a spawn
/// preparation. Version four binds the preplanned cleanup and registration
/// values to the persisted READY snapshot and, when supplied, to the observed
/// scope. Older versions retain their existing absence-of-plan contract.
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn validate_spawn_preparation_process_scope_binding(
    intent: &SpawnPreparationIntentData,
    observed: Option<&crate::sandbox::process::ProcessScopeRecord>,
) -> Result<(), String> {
    let expected = intent.handoff_process_scope.as_ref();
    let plan = match intent.version {
        SPAWN_PREPARATION_VERSION => {
            let plan = intent.process_scope_plan.as_ref().ok_or_else(|| {
                "version-four subagent preparation lacks its preplanned process authority"
                    .to_string()
            })?;
            if !is_canonical_spawn_preparation_uuid(&plan.cleanup_lease_id)
                || !is_canonical_spawn_preparation_uuid(&plan.supervisor_registration_nonce)
            {
                return Err(
                    "version-four subagent preparation process authority is invalid".to_string(),
                );
            }
            Some(plan)
        }
        HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION => {
            if intent.process_scope_plan.is_some() {
                return Err(
                    "version-three subagent preparation unexpectedly contains version-four process authority"
                        .to_string(),
                );
            }
            None
        }
        LEGACY_SPAWN_PREPARATION_VERSION => {
            if intent.process_scope_plan.is_some() || expected.is_some() {
                return Err(
                    "version-two subagent preparation unexpectedly contains process-scope authority"
                        .to_string(),
                );
            }
            None
        }
        _ => return Err("unsupported subagent preparation intent version".to_string()),
    };

    match intent.version {
        HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION | SPAWN_PREPARATION_VERSION
            if intent.phase == SpawnPreparationPhase::HandoffProven && expected.is_none() =>
        {
            return Err(format!(
                "version-{} proven handoff lacks its exact READY process authority",
                intent.version
            ));
        }
        HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION | SPAWN_PREPARATION_VERSION
            if intent.phase != SpawnPreparationPhase::HandoffProven && expected.is_some() =>
        {
            return Err(format!(
                "version-{} READY process authority is valid only for the proven phase",
                intent.version
            ));
        }
        _ => {}
    }
    if let Some(expected) = expected {
        let ready_is_exact = expected.scope_id == intent.subagent_id
            && expected.workload_kind == "subagent"
            && expected.execution_generation == intent.owner.execution_generation
            && expected.status == crate::sandbox::process::ProcessScopeStatus::Running
            && expected.launch_committed == Some(false)
            && expected.supervisor.is_some()
            && expected.direct_child.is_some()
            && expected.cleanup_proof.is_none()
            && expected.launch_abort_proof.is_none();
        let plan_matches = plan
            .map(|plan| {
                expected.cleanup_lease_id == plan.cleanup_lease_id
                    && expected.supervisor_registration_nonce.as_deref()
                        == Some(plan.supervisor_registration_nonce.as_str())
            })
            .unwrap_or(true);
        if !ready_is_exact || !plan_matches {
            return Err(
                "subagent handoff READY scope does not match its immutable preparation authority"
                    .to_string(),
            );
        }
    }

    if let Some(observed) = observed {
        if observed.scope_id != intent.subagent_id
            || observed.workload_kind != "subagent"
            || observed.execution_generation != intent.owner.execution_generation
            || plan.is_some_and(|plan| {
                observed.cleanup_lease_id != plan.cleanup_lease_id
                    || observed.supervisor_registration_nonce.as_deref()
                        != Some(plan.supervisor_registration_nonce.as_str())
            })
        {
            return Err(
                "observed subagent process scope does not match its immutable preparation authority"
                    .to_string(),
            );
        }
        if let Some(expected) = expected {
            let immutable_matches = observed.scope_id == expected.scope_id
                && observed.workload_kind == expected.workload_kind
                && observed.execution_generation == expected.execution_generation
                && observed.cleanup_lease_id == expected.cleanup_lease_id
                && observed.supervisor_registration_nonce == expected.supervisor_registration_nonce
                && observed.owner == expected.owner
                && observed.backend == expected.backend
                && observed.supervisor == expected.supervisor
                && observed.direct_child == expected.direct_child
                && observed.created_at == expected.created_at;
            if !immutable_matches {
                return Err(
                    "subagent handoff process scope does not match its exact READY execution authority"
                        .to_string(),
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_spawn_preparation_intent_structure(
    intent: &SpawnPreparationIntentData,
) -> Result<(), String> {
    validate_spawn_preparation_process_scope_binding(intent, None)?;
    if !is_valid_subagent_id(&intent.subagent_id)
        || intent
            .audit_namespace_plan
            .as_ref()
            .is_some_and(|plan| plan.sessions_dir != intent.audit_sessions_dir)
        || spawn_preparation_phase_revision(intent) != Some(intent.revision)
        || (intent.audit_namespace_plan.is_some()
            && (intent.audit_receipt.is_some()
                != matches!(
                    intent.phase,
                    SpawnPreparationPhase::AuditPlanned
                        | SpawnPreparationPhase::AuditPublished
                        | SpawnPreparationPhase::RecordPublished
                        | SpawnPreparationPhase::ManagerRegistered
                        | SpawnPreparationPhase::HandoffProven
                )))
        || (intent.audit_namespace_plan.is_none()
            && (intent.audit_receipt.is_some() || intent.audit_target.is_none()))
        || (matches!(
            intent.phase,
            SpawnPreparationPhase::AuditPublished
                | SpawnPreparationPhase::RecordPublished
                | SpawnPreparationPhase::ManagerRegistered
                | SpawnPreparationPhase::HandoffProven
        ) && intent.audit_target.is_none())
        || (intent.phase != SpawnPreparationPhase::HandoffProven
            && intent.handoff_process_scope.is_some())
    {
        return Err("subagent preparation intent structure is invalid".to_string());
    }
    validate_execution_ownership(intent.owner.execution_generation, &intent.owner.lease_id)
}

pub(crate) fn spawn_preparation_phase_revision(intent: &SpawnPreparationIntentData) -> Option<u64> {
    let audit_published = if intent.audit_namespace_plan.is_some() {
        3
    } else {
        2
    };
    Some(match intent.phase {
        SpawnPreparationPhase::Planned => 0,
        SpawnPreparationPhase::ResourcesPrepared => 1,
        SpawnPreparationPhase::AuditPlanned if intent.audit_namespace_plan.is_some() => 2,
        SpawnPreparationPhase::AuditPlanned => return None,
        SpawnPreparationPhase::AuditPublished => audit_published,
        SpawnPreparationPhase::RecordPublished => audit_published + 1,
        SpawnPreparationPhase::ManagerRegistered => audit_published + 2,
        SpawnPreparationPhase::HandoffProven => audit_published + 3,
    })
}

pub(crate) fn spawn_handoff_evidence(intent: &SpawnPreparationIntentData, state: &str) -> Value {
    json!({
        "version": SPAWN_HANDOFF_VERSION,
        "state": state,
        "subagent_id": intent.subagent_id,
        "execution_generation": intent.owner.execution_generation,
        "owner_lease": intent.owner.lease_id,
        "worktree_receipt": intent.worktree.ownership_receipt_id,
    })
}

pub(crate) fn record_spawn_handoff_matches(
    intent: &SpawnPreparationIntentData,
    record: &SubagentRecord,
    expected_state: &str,
) -> bool {
    process_scope_retirement_result(record).and_then(|result| result.get(SPAWN_HANDOFF_KEY))
        == Some(&spawn_handoff_evidence(intent, expected_state))
}

#[cfg(test)]
pub(crate) fn record_has_valid_committed_spawn_handoff(record: &SubagentRecord) -> bool {
    let Some(result) = process_scope_retirement_result(record) else {
        return false;
    };
    let Some(handoff) = result.get(SPAWN_HANDOFF_KEY).and_then(Value::as_object) else {
        return false;
    };
    let worktree_receipt = handoff.get("worktree_receipt").and_then(Value::as_str);
    handoff.get("version").and_then(Value::as_u64) == Some(SPAWN_HANDOFF_VERSION as u64)
        && handoff.get("state").and_then(Value::as_str) == Some("committed")
        && handoff.get("subagent_id").and_then(Value::as_str) == Some(record.id.as_str())
        && handoff.get("execution_generation").and_then(Value::as_u64)
            == record.execution_generation
        && handoff.get("owner_lease").and_then(Value::as_str) == record.owner_lease.as_deref()
        && worktree_receipt.is_some_and(|receipt| !receipt.is_empty())
        && result
            .get(WORKTREE_PREPARATION_RECEIPT_KEY)
            .and_then(Value::as_str)
            == worktree_receipt
}

pub(crate) fn spawn_handoff_has_execution_evidence(
    project_root: &Path,
    records: &crate::daemons::state::StableDirectory,
    intent: &SpawnPreparationIntentData,
    record: &SubagentRecord,
    deadline: Instant,
) -> Result<bool, String> {
    validate_spawn_preparation_process_scope_binding(intent, None)?;
    let Some(store) = crate::sandbox::process::ProcessScopeStore::open_existing_bound_to_records(
        project_root,
        records,
        deadline,
    )?
    else {
        return Ok(false);
    };
    let Some(scope) = store.try_load(&intent.subagent_id)? else {
        return Ok(false);
    };
    validate_spawn_preparation_process_scope_binding(intent, Some(&scope))?;
    if intent.handoff_process_scope.is_none() {
        if scope.status == crate::sandbox::process::ProcessScopeStatus::Prepared
            || scope.launch_committed == Some(false)
        {
            return Ok(false);
        }
        return Err(
            "subagent handoff lacks exact persisted READY process authority; intent and resources were preserved"
                .to_string(),
        );
    }
    match scope.status {
        crate::sandbox::process::ProcessScopeStatus::Running
        | crate::sandbox::process::ProcessScopeStatus::CleanupInProgress
        | crate::sandbox::process::ProcessScopeStatus::RecoveryRequired => {
            if scope.launch_committed == Some(true)
                && scope.cleanup_proof.is_none()
                && scope.launch_abort_proof.is_none()
            {
                Ok(true)
            } else if scope.launch_committed == Some(false) {
                Ok(false)
            } else {
                Err(
                    "subagent handoff process scope has no exact committed execution evidence"
                        .to_string(),
                )
            }
        }
        crate::sandbox::process::ProcessScopeStatus::Prepared => Ok(false),
        crate::sandbox::process::ProcessScopeStatus::Complete => {
            let Some((generation, authority)) = terminal_process_scope_authority(record)? else {
                return Err(
                    "terminal subagent handoff has no exact persisted process authority"
                        .to_string(),
                );
            };
            if generation != intent.owner.execution_generation {
                return Err("terminal subagent handoff generation is inconsistent".to_string());
            }
            match authority {
                TerminalProcessScopeAuthority::Cleanup(proof)
                    if scope.launch_committed == Some(true)
                        && scope.cleanup_proof.as_ref() == Some(&proof)
                        && scope.launch_abort_proof.is_none() =>
                {
                    Ok(true)
                }
                TerminalProcessScopeAuthority::LaunchAbort(proof)
                    if scope.launch_committed != Some(true)
                        && scope.launch_abort_proof.as_ref() == Some(&proof)
                        && scope.cleanup_proof.is_none() =>
                {
                    Ok(false)
                }
                _ => Err(
                    "terminal subagent handoff proof does not match its exact process scope"
                        .to_string(),
                ),
            }
        }
    }
}

pub(crate) fn reconcile_pre_handoff_process_scope(
    project_root: &Path,
    records: &crate::daemons::state::StableDirectory,
    intent: &SpawnPreparationIntentData,
    deadline: Instant,
) -> Result<(), String> {
    validate_spawn_preparation_process_scope_binding(intent, None)?;
    let Some(store) = crate::sandbox::process::ProcessScopeStore::open_existing_bound_to_records(
        project_root,
        records,
        deadline,
    )?
    else {
        return Ok(());
    };
    let Some(mut scope) = store.try_load(&intent.subagent_id)? else {
        return Ok(());
    };
    validate_spawn_preparation_process_scope_binding(intent, Some(&scope))?;
    if intent.process_scope_plan.is_some()
        && scope.status == crate::sandbox::process::ProcessScopeStatus::Prepared
        && scope.launch_committed == Some(false)
        && scope.supervisor.is_none()
        && scope.direct_child.is_none()
    {
        // remove_prepared takes the same exact scope lock as supervisor
        // self-registration. Whichever operation wins determines the only
        // valid next state; a late supervisor that loses exits before any
        // request, record, worktree, or worker access.
        store.remove_prepared(&scope)?;
        return Ok(());
    }
    if scope.status != crate::sandbox::process::ProcessScopeStatus::Complete {
        if scope.launch_committed != Some(false) {
            return Err(
                "pre-handoff process scope may have launched; cleanup authority was preserved"
                    .to_string(),
            );
        }
        scope = store.recover_linux_supervisor_loss(&scope).map_err(|error| {
            format!(
                "pre-handoff managed-process cleanup is not yet proven; preparation was preserved: {error}"
            )
        })?;
    }
    let proof = scope.launch_abort_proof.as_ref().ok_or_else(|| {
        "pre-handoff process scope has no exact never-launched cleanup proof; preparation was preserved"
            .to_string()
    })?;
    if scope.launch_committed == Some(true) || !proof.workload_never_launched {
        return Err(
            "pre-handoff process scope contains committed execution evidence; preparation was preserved"
                .to_string(),
        );
    }
    store.retire_launch_abort(
        &intent.subagent_id,
        intent.owner.execution_generation,
        proof,
    )?;
    Ok(())
}

pub(crate) fn preserve_spawn_internal_authority(previous: Option<&Value>, next: &mut Value) {
    let (Some(previous), Some(next)) = (previous.and_then(Value::as_object), next.as_object_mut())
    else {
        return;
    };
    for key in [
        OWNERSHIP_AUDIT_TARGET_KEY,
        WORKTREE_PREPARATION_RECEIPT_KEY,
        SPAWN_HANDOFF_KEY,
    ] {
        if let Some(value) = previous.get(key) {
            next.insert(key.to_string(), value.clone());
        }
    }
}

pub(crate) fn adopt_spawn_preparation_publication_error(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    previous: Option<&File>,
    encoded: &[u8],
    deadline: Instant,
    error: crate::daemons::state::FilePublicationError,
    mut external_guard: impl FnMut() -> Result<(), String>,
) -> Result<crate::daemons::state::FilePublicationReceipt, String> {
    let message = error.message;
    let receipt = error.receipt.ok_or(message.clone())?;
    if !receipt.exact_identity {
        return Err(format!(
            "{message}; subagent preparation publication receipt was not exact"
        ));
    }
    let mut guard = || {
        external_guard()?;
        ensure_subagent_reconciliation_deadline(Some(deadline))
    };
    directory
        .finalize_failed_exact_publication_with_guard(
            path,
            previous,
            &receipt,
            ".nib-subagent-preparation-",
            encoded,
            &mut guard,
        )
        .map_err(|recovery| {
            format!("{message}; failed to finalize exact preparation publication: {recovery}")
        })?;
    Ok(receipt)
}
