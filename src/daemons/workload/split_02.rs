//! T043 split.

use super::*;

impl TaskLock {
    pub(crate) fn acquire(path: PathBuf, anchor_path: PathBuf) -> Result<Self, String> {
        Self::acquire_with_hook_and_retries(path, anchor_path, LOCK_RETRIES, || Ok(()))
    }

    #[cfg(test)]
    pub(crate) fn acquire_with_hook(
        path: PathBuf,
        anchor_path: PathBuf,
        after_open: impl FnOnce() -> Result<(), String>,
    ) -> Result<Self, String> {
        Self::acquire_with_hook_and_retries(path, anchor_path, LOCK_RETRIES, after_open)
    }

    #[cfg(test)]
    pub(crate) fn acquire_with_retries(
        path: PathBuf,
        anchor_path: PathBuf,
        retries: usize,
    ) -> Result<Self, String> {
        Self::acquire_with_hook_and_retries(path, anchor_path, retries, || Ok(()))
    }

    pub(crate) fn acquire_with_hook_and_retries(
        path: PathBuf,
        anchor_path: PathBuf,
        retries: usize,
        after_open: impl FnOnce() -> Result<(), String>,
    ) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or_else(|| format!("task lock has no parent: {}", path.display()))?;
        let anchor_parent = anchor_path
            .parent()
            .ok_or_else(|| format!("task lock anchor has no parent: {}", anchor_path.display()))?;
        let lock_directory = crate::daemons::state::StableDirectory::open(parent)?;
        let anchor_directory = crate::daemons::state::StableDirectory::open(anchor_parent)?;
        let file =
            open_task_lock_anchor_bound(&lock_directory, &path, &anchor_directory, &anchor_path)?;
        let opened_identity = task_file_identity(&file, &anchor_path)?;
        after_open()?;
        verify_task_lock_paths_bound(
            &lock_directory,
            &path,
            &anchor_directory,
            &anchor_path,
            &opened_identity,
        )?;
        for _ in 0..retries {
            match file.try_lock() {
                Ok(()) => {
                    lock_directory.verify_visible()?;
                    anchor_directory.verify_visible()?;
                    verify_task_lock_paths_bound(
                        &lock_directory,
                        &path,
                        &anchor_directory,
                        &anchor_path,
                        &opened_identity,
                    )?;
                    return Ok(Self {
                        _file: file,
                        _lock_directory: lock_directory,
                        _anchor_directory: anchor_directory,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(std::fs::TryLockError::Error(error))
                    if error.kind() == std::io::ErrorKind::Interrupted =>
                {
                    std::thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to acquire task lock {}: {error}",
                        path.display()
                    ))
                }
            }
        }
        Err(format!("timed out acquiring task lock: {}", path.display()))
    }
}

#[cfg(any(unix, windows))]
pub(crate) fn open_task_lock_anchor_bound(
    lock_directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    anchor_directory: &crate::daemons::state::StableDirectory,
    anchor_path: &Path,
) -> Result<File, String> {
    let path_exists = lock_directory.path_exists(path)?;
    let anchor_exists = anchor_directory.path_exists(anchor_path)?;
    match (path_exists, anchor_exists) {
        (false, false) => {
            drop(lock_directory.open_read_write_create(path)?);
            lock_directory.hard_link_to(path, anchor_directory, anchor_path)?;
        }
        (true, false) => {
            lock_directory.hard_link_to(path, anchor_directory, anchor_path)?;
        }
        (false, true) => {
            anchor_directory.hard_link_to(anchor_path, lock_directory, path)?;
        }
        (true, true) => {}
    }
    let anchor = anchor_directory.open_read_write(anchor_path)?;
    let expected = task_file_identity(&anchor, anchor_path)?;
    verify_task_lock_paths_bound(
        lock_directory,
        path,
        anchor_directory,
        anchor_path,
        &expected,
    )?;
    Ok(anchor)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_task_lock_anchor_bound(
    _lock_directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    _anchor_directory: &crate::daemons::state::StableDirectory,
    _anchor_path: &Path,
) -> Result<File, String> {
    Err(format!(
        "persistent task lock anchors are unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
pub(crate) fn cleanup_existing_task_lock_artifacts(
    path: &Path,
    anchor_path: &Path,
    tasks_directory: &crate::daemons::state::StableDirectory,
    daemon_directory: &crate::daemons::state::StableDirectory,
) -> Result<(), String> {
    let path_exists = tasks_directory.path_exists(path)?;
    let anchor_exists = daemon_directory.path_exists(anchor_path)?;
    if !path_exists && !anchor_exists {
        return Ok(());
    }

    let (source_directory, source) = if anchor_exists {
        (daemon_directory, anchor_path)
    } else {
        (tasks_directory, path)
    };
    let file = source_directory.open_read_write(source)?;
    let identity = task_file_identity(&file, source)?;
    for (exists, candidate, directory) in [
        (path_exists, path, tasks_directory),
        (anchor_exists, anchor_path, daemon_directory),
    ] {
        if !exists {
            continue;
        }
        let probe = directory.open_read_write(candidate)?;
        if task_file_identity(&probe, candidate)? != identity {
            return Err(format!(
                "legacy task lock and anchor have different identities: {}",
                path.display()
            ));
        }
    }
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(format!(
                "legacy task lock is still owned and cannot be migrated: {}",
                path.display()
            ))
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(format!(
                "failed to acquire legacy task lock {}: {error}",
                path.display()
            ))
        }
    }
    for (exists, candidate, directory) in [
        (path_exists, path, tasks_directory),
        (anchor_exists, anchor_path, daemon_directory),
    ] {
        if !exists {
            continue;
        }
        let probe = directory.open_read_write(candidate)?;
        if task_file_identity(&probe, candidate)? != identity {
            return Err(format!(
                "legacy task lock identity changed while it was migrated: {}",
                candidate.display()
            ));
        }
        directory.remove_file_if_matches(candidate, &file, ".nib-legacy-task-lock-delete-")?;
    }
    tasks_directory.verify_visible()?;
    daemon_directory.verify_visible()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn cleanup_existing_task_lock_artifacts(
    path: &Path,
    _anchor_path: &Path,
    _tasks_directory: &crate::daemons::state::StableDirectory,
    _daemon_directory: &crate::daemons::state::StableDirectory,
) -> Result<(), String> {
    Err(format!(
        "legacy task lock migration is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
pub(crate) fn verify_task_lock_paths_bound(
    lock_directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    anchor_directory: &crate::daemons::state::StableDirectory,
    anchor_path: &Path,
    expected: &crate::fs_security::FileIdentity,
) -> Result<(), String> {
    for (directory, candidate) in [(lock_directory, path), (anchor_directory, anchor_path)] {
        let probe = directory.open_read_write(candidate)?;
        if task_file_identity(&probe, candidate)? != *expected {
            return Err(format!(
                "task lock identity changed while it was acquired: {}",
                candidate.display()
            ));
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn verify_task_lock_paths_bound(
    _lock_directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    _anchor_directory: &crate::daemons::state::StableDirectory,
    _anchor_path: &Path,
    _expected: &(),
) -> Result<(), String> {
    Err(format!(
        "stable task lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
pub(crate) fn task_file_identity(
    file: &File,
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(
        file.try_clone()
            .map_err(|error| format!("failed to clone {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("failed to identify {}: {error}", path.display()))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn task_file_identity(_file: &File, path: &Path) -> Result<(), String> {
    Err(format!(
        "stable task file identity is unsupported on this platform: {}",
        path.display()
    ))
}

pub(crate) fn append_compensation_error(
    primary_error: String,
    compensation_error: String,
    audit_error: Option<String>,
) -> String {
    match audit_error {
        Some(audit_error) => format!(
            "{primary_error}; durable compensation failed: {compensation_error}; failed to persist compensation audit: {audit_error}"
        ),
        None => format!(
            "{primary_error}; durable compensation failed: {compensation_error}; compensation failure was recorded in the daemon audit"
        ),
    }
}
