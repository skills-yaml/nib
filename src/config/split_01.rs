//! T043 split.

use super::*;

pub(crate) fn with_config_lock_with_hook<T>(
    project_root: &Path,
    deadline: Option<Instant>,
    before_lock: impl FnOnce(&ConfigPaths) -> Result<(), ConfigError>,
    operation: impl FnOnce(
        &ConfigPaths,
        &crate::daemons::state::StableDirectory,
    ) -> Result<T, ConfigError>,
) -> Result<T, ConfigError> {
    ensure_config_lock_deadline(deadline)?;
    let paths = config_paths(project_root);
    let directory_existed = paths.nib_dir.is_dir();
    crate::fs_security::ensure_directory_without_symlinks(&paths.nib_dir)?;
    if !directory_existed {
        if let Some(parent) = paths.nib_dir.parent() {
            sync_directory(parent)?;
        }
    }
    let expected_directory =
        crate::daemons::state::StableDirectory::open(&paths.nib_dir).map_err(config_state_error)?;
    before_lock(&paths)?;

    let normalized = normalized_config_path(&paths.toml)?;
    let process_lock = config_process_lock(&normalized, deadline)?;
    let _guard = acquire_config_mutex(&process_lock, &normalized, deadline)?;
    ensure_config_lock_deadline(deadline)?;
    let lock_path = paths.nib_dir.join("config.toml.lock");
    let mut operation_error = None;
    let locked_operation = |directory: &crate::daemons::state::StableDirectory| {
        ensure_config_lock_deadline(deadline).map_err(|error| error.to_string())?;
        if !directory.same_identity(&expected_directory) {
            return Err(format!(
                "configuration directory identity changed before lock acquisition: {}",
                paths.nib_dir.display()
            ));
        }
        ensure_config_lock_deadline(deadline).map_err(|error| error.to_string())?;
        expected_directory.recover_stale_temporary_files(
            CONFIG_ATOMIC_TEMPORARY_PREFIX,
            MAX_CONFIG_DIRECTORY_ENTRIES,
            MAX_CONFIG_DIRECTORY_NAME_BYTES,
        )?;
        ensure_config_lock_deadline(deadline).map_err(|error| error.to_string())?;
        match operation(&paths, &expected_directory) {
            Ok(value) => Ok(value),
            Err(error) => {
                let message = error.to_string();
                operation_error = Some(error);
                Err(format!("{CONFIG_OPERATION_ERROR_SENTINEL}{message}"))
            }
        }
    };
    let result = match deadline {
        Some(deadline) => crate::daemons::state::with_file_lock_in_until(
            &lock_path,
            &paths.nib_dir,
            deadline,
            locked_operation,
        ),
        None => {
            crate::daemons::state::with_file_lock_in(&lock_path, &paths.nib_dir, locked_operation)
        }
    };
    match result {
        Ok(value) => Ok(value),
        Err(error) => match operation_error {
            Some(operation_error) if error.starts_with(CONFIG_OPERATION_ERROR_SENTINEL) => {
                Err(operation_error)
            }
            Some(operation_error) => Err(ConfigError::Operation(format!(
                "{operation_error}; configuration state verification failed: {error}"
            ))),
            None => Err(config_state_error(error)),
        },
    }
}

pub(crate) fn config_process_lock(
    path: &Path,
    deadline: Option<Instant>,
) -> Result<Arc<ConfigMutex>, ConfigError> {
    let registry = CONFIG_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = acquire_config_mutex(registry, path, deadline)?;
    ensure_config_lock_deadline(deadline)?;
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

pub(crate) fn acquire_config_mutex<'a, T>(
    mutex: &'a Mutex<T>,
    path: &Path,
    deadline: Option<Instant>,
) -> Result<std::sync::MutexGuard<'a, T>, ConfigError> {
    let Some(deadline) = deadline else {
        return mutex
            .lock()
            .map_err(|_| ConfigError::LockPoisoned(path.display().to_string()));
    };
    loop {
        ensure_config_lock_deadline(Some(deadline))?;
        match mutex.try_lock() {
            Ok(guard) => {
                ensure_config_lock_deadline(Some(deadline))?;
                return Ok(guard);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(ConfigError::LockPoisoned(path.display().to_string()));
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                ConfigError::Operation("configuration lock deadline elapsed".to_string())
            })?;
        if remaining.is_zero() {
            return Err(ConfigError::Operation(
                "configuration lock deadline elapsed".to_string(),
            ));
        }
        std::thread::sleep(Duration::from_millis(10).min(remaining));
    }
}

pub(crate) fn ensure_config_lock_deadline(deadline: Option<Instant>) -> Result<(), ConfigError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(ConfigError::Operation(
            "configuration lock deadline elapsed".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn normalized_config_path(path: &Path) -> Result<PathBuf, ConfigError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigError::Operation(format!(
            "configuration path has no parent: {}",
            path.display()
        ))
    })?;
    let parent = parent.canonicalize()?;
    let file_name = path.file_name().ok_or_else(|| {
        ConfigError::Operation(format!(
            "configuration path has no file name: {}",
            path.display()
        ))
    })?;
    Ok(parent.join(file_name))
}

pub(crate) struct LoadedNibConfig {
    pub(crate) config: NibConfig,
    pub(crate) source: ConfigSource,
    pub(crate) toml_file: Option<File>,
}

impl LoadedNibConfig {
    pub(crate) fn expectation(&self) -> crate::daemons::state::FileExpectation<'_> {
        self.toml_file.as_ref().map_or(
            crate::daemons::state::FileExpectation::Missing,
            crate::daemons::state::FileExpectation::Present,
        )
    }
}

pub(crate) fn load_nib_config_with_source_unlocked(
    paths: &ConfigPaths,
    directory: &crate::daemons::state::StableDirectory,
) -> Result<LoadedNibConfig, ConfigError> {
    if regular_file_exists(directory, &paths.toml)? {
        let (config, file) = load_nib_config_file(directory, &paths.toml)?;
        backup_legacy_json(paths, directory, None)?;
        return Ok(LoadedNibConfig {
            config,
            source: ConfigSource::Toml,
            toml_file: Some(file),
        });
    }

    if regular_file_exists(directory, &paths.json)? {
        return migrate_json_to_toml_unlocked(paths, directory);
    }

    let config = NibConfig::default();
    config.validate()?;
    Ok(LoadedNibConfig {
        config,
        source: ConfigSource::Default,
        toml_file: None,
    })
}

pub(crate) fn load_nib_config_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<(NibConfig, File), ConfigError> {
    let (content, file) = read_regular_file(directory, path)?;
    let config: NibConfig = toml::from_str(&content)?;
    config.validate()?;
    Ok((config, file))
}

pub(crate) fn load_nib_config_opened_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    file: File,
) -> Result<NibConfig, ConfigError> {
    let (content, _) = read_opened_regular_file(directory, path, file, false)?;
    let config: NibConfig = toml::from_str(&content)?;
    config.validate()?;
    Ok(config)
}

pub(crate) fn read_regular_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<(String, File), ConfigError> {
    let file = directory.open_read(path).map_err(config_state_error)?;
    read_opened_regular_file(directory, path, file, true)
}

pub(crate) fn read_opened_regular_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    file: File,
    verify_visible_identity_after_read: bool,
) -> Result<(String, File), ConfigError> {
    let opened_metadata = file.metadata()?;
    validate_config_file_metadata(path, &opened_metadata)?;
    #[cfg(test)]
    run_config_read_hook(path)?;
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    (&file)
        .take(MAX_CONFIG_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::FileTooLarge {
            path: path.display().to_string(),
            size: bytes.len() as u64,
            max: MAX_CONFIG_FILE_BYTES,
        });
    }
    if verify_visible_identity_after_read {
        directory
            .verify_file_identity(path, &file)
            .map_err(config_state_error)?;
    }
    directory.verify_visible().map_err(config_state_error)?;
    let contents = String::from_utf8(bytes).map_err(|error| {
        ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    Ok((contents, file))
}

pub(crate) fn validate_config_file_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), ConfigError> {
    if crate::fs_security::metadata_is_link_or_reparse(metadata) || !metadata.is_file() {
        return Err(ConfigError::InvalidFileType(path.display().to_string()));
    }
    if metadata.len() > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::FileTooLarge {
            path: path.display().to_string(),
            size: metadata.len(),
            max: MAX_CONFIG_FILE_BYTES,
        });
    }
    Ok(())
}

pub(crate) fn regular_file_exists(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<bool, ConfigError> {
    directory.path_exists(path).map_err(config_state_error)
}

pub(crate) fn save_nib_config_atomic(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    config: &NibConfig,
    expected: crate::daemons::state::FileExpectation<'_>,
) -> Result<(), ConfigError> {
    save_nib_config_atomic_with_hook(directory, path, config, expected, || Ok(()))
}

pub(crate) fn save_nib_config_atomic_with_hook(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    config: &NibConfig,
    expected: crate::daemons::state::FileExpectation<'_>,
    before_commit: impl FnOnce() -> Result<(), String>,
) -> Result<(), ConfigError> {
    config.validate()?;
    let content = toml::to_string_pretty(config)?;
    if content.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::FileTooLarge {
            path: path.display().to_string(),
            size: content.len() as u64,
            max: MAX_CONFIG_FILE_BYTES,
        });
    }
    directory
        .save_bytes_atomically_expected_with_hook(
            path,
            content.as_bytes(),
            CONFIG_ATOMIC_TEMPORARY_PREFIX,
            true,
            expected,
            before_commit,
        )
        .map_err(config_state_error)
}

pub(crate) fn restore_nib_config_atomic(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    config: &NibConfig,
) -> Result<(), ConfigError> {
    let current = if regular_file_exists(directory, path)? {
        Some(directory.open_read(path).map_err(config_state_error)?)
    } else {
        None
    };
    let expected = current.as_ref().map_or(
        crate::daemons::state::FileExpectation::Missing,
        crate::daemons::state::FileExpectation::Present,
    );
    save_nib_config_atomic(directory, path, config, expected)
}

pub(crate) fn migrate_json_to_toml_unlocked(
    paths: &ConfigPaths,
    directory: &crate::daemons::state::StableDirectory,
) -> Result<LoadedNibConfig, ConfigError> {
    let (content, json_file) = read_regular_file(directory, &paths.json)?;
    let llm: LlmConfig = serde_json::from_str(&content)?;
    let config = NibConfig {
        revision: 1,
        llm: llm.clone(),
        ..NibConfig::default()
    };
    config.validate()?;
    save_nib_config_atomic(
        directory,
        &paths.toml,
        &config,
        crate::daemons::state::FileExpectation::Missing,
    )?;
    backup_legacy_json(paths, directory, Some(&json_file))?;
    let (config, file) = load_nib_config_file(directory, &paths.toml)?;
    Ok(LoadedNibConfig {
        config,
        source: ConfigSource::MigratedFromJson,
        toml_file: Some(file),
    })
}

pub(crate) fn backup_legacy_json(
    paths: &ConfigPaths,
    directory: &crate::daemons::state::StableDirectory,
    expected_source: Option<&File>,
) -> Result<(), ConfigError> {
    if !regular_file_exists(directory, &paths.json)? {
        return Ok(());
    }
    let owned_source;
    let source = match expected_source {
        Some(source) => source,
        None => {
            owned_source = directory
                .open_read(&paths.json)
                .map_err(config_state_error)?;
            &owned_source
        }
    };
    directory
        .verify_file_identity(&paths.json, source)
        .map_err(config_state_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let writable_source = directory
            .open_read_write(&paths.json)
            .map_err(config_state_error)?;
        directory
            .verify_file_identity(&paths.json, source)
            .map_err(config_state_error)?;
        writable_source.set_permissions(fs::Permissions::from_mode(0o600))?;
        writable_source.sync_all()?;
        directory
            .verify_file_identity(&paths.json, source)
            .map_err(config_state_error)?;
    }
    if regular_file_exists(directory, &paths.json_backup)? {
        let backup = directory
            .open_read(&paths.json_backup)
            .map_err(config_state_error)?;
        directory
            .remove_file_if_matches(&paths.json_backup, &backup, ".config-backup-delete-")
            .map_err(config_state_error)?;
    }
    directory
        .rename_file_if_matches(&paths.json, &paths.json_backup, source)
        .map_err(config_state_error)?;
    Ok(())
}

pub(crate) fn config_state_error(error: String) -> ConfigError {
    ConfigError::Operation(error)
}

pub(crate) fn sync_directory(_path: &Path) -> Result<(), ConfigError> {
    #[cfg(unix)]
    File::open(_path)?.sync_all()?;
    Ok(())
}
