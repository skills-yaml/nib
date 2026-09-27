//! T043 split.

use super::*;

impl SessionStore {
    pub(crate) async fn with_lock_policy<F>(timeout: Duration, future: F) -> F::Output
    where
        F: Future,
    {
        SESSION_LOCK_POLICY
            .scope(
                SessionLockPolicy {
                    timeout,
                    offload_waits: true,
                },
                future,
            )
            .await
    }

    pub(crate) fn current_lock_policy() -> Option<SessionLockPolicy> {
        SESSION_LOCK_POLICY.try_with(|policy| *policy).ok()
    }

    pub(crate) async fn with_optional_lock_policy<F>(
        policy: Option<SessionLockPolicy>,
        future: F,
    ) -> F::Output
    where
        F: Future,
    {
        match policy {
            Some(policy) => SESSION_LOCK_POLICY.scope(policy, future).await,
            None => future.await,
        }
    }

    pub fn new(project_root: &Path) -> Self {
        let nib = project_root.join(".nib");
        let sessions_dir = nib.join("sessions");
        Self::at_dir(sessions_dir)
    }

    pub fn for_project(project_root: &Path) -> Result<Self, String> {
        Self::for_project_until(project_root, None)
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn preflight_project_sessions_dir_until(
        project_root: &Path,
        deadline: Instant,
    ) -> Result<SessionDirectoryPreflight, String> {
        ensure_session_store_open_deadline(deadline)?;
        let mut config =
            crate::config::load_nib_config_full_preflight_read_only_until(project_root, deadline)
                .map_err(|error| error.to_string())?;
        ensure_session_store_open_deadline(deadline)?;
        let (selected_profile_id, sessions_dir) =
            crate::profile::ProfileRegistry::resolve_profile_sessions_without_migration_until(
                project_root,
                &config.profiles,
                deadline,
            )
            .map_err(|error| error.to_string())?;
        // Freeze the workspace-selected profile into the runtime snapshot. The
        // child bootstrap intentionally reduces the profile set to this id;
        // retaining the configured global default here would let a nested
        // workspace inherit another profile's environment and skills.
        config.profiles.default = selected_profile_id;
        ensure_session_store_open_deadline(deadline)?;
        let sessions_dir =
            crate::fs_security::absolute_path(&sessions_dir).map_err(|error| error.to_string())?;
        let parent_path = sessions_dir
            .parent()
            .ok_or_else(|| {
                format!(
                    "session directory has no persistent identity parent: {}",
                    sessions_dir.display()
                )
            })?
            .to_path_buf();

        let mut ancestor_path = parent_path.clone();
        loop {
            ensure_session_store_open_deadline(deadline)?;
            match std::fs::symlink_metadata(&ancestor_path) {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ancestor_path = ancestor_path
                        .parent()
                        .ok_or_else(|| {
                            format!(
                                "future session directory has no existing retained ancestor: {}",
                                sessions_dir.display()
                            )
                        })?
                        .to_path_buf();
                }
                Err(error) => {
                    return Err(format!(
                        "failed to inspect future session ancestor {}: {error}",
                        ancestor_path.display()
                    ));
                }
            }
        }
        let canonical_ancestor =
            crate::fs_security::canonicalize_existing_directory_without_symlinks(&ancestor_path)
                .map_err(|error| {
                    format!(
                        "existing session ancestor is unsafe {}: {error}",
                        ancestor_path.display()
                    )
                })?;
        if !crate::fs_security::canonical_paths_match(&canonical_ancestor, &ancestor_path) {
            return Err(format!(
                "preflighted session ancestor changed while it was retained: {}",
                ancestor_path.display()
            ));
        }
        ensure_session_store_open_deadline(deadline)?;
        let retained_ancestor = crate::daemons::state::StableDirectory::open(&canonical_ancestor)?;
        ensure_session_store_open_deadline(deadline)?;

        let retained_directory = if retained_ancestor.path() == parent_path {
            match retained_ancestor.entry_kind(&sessions_dir)? {
                Some(crate::daemons::state::StableEntryKind::Directory) => {
                    Some(retained_ancestor.open_owned_child(&sessions_dir)?)
                }
                Some(crate::daemons::state::StableEntryKind::File) => {
                    return Err(format!(
                        "future session directory is not a local directory: {}",
                        sessions_dir.display()
                    ));
                }
                None => None,
            }
        } else {
            None
        };
        ensure_session_store_open_deadline(deadline)?;
        let retained_identity_file = match retained_directory.as_ref() {
            Some(directory) => {
                let identity_path = sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
                match directory.entry_kind(&identity_path)? {
                    Some(crate::daemons::state::StableEntryKind::File) => {
                        Some(directory.open_read(&identity_path)?)
                    }
                    Some(crate::daemons::state::StableEntryKind::Directory) => {
                        return Err(format!(
                            "session directory identity is not a regular file: {}",
                            identity_path.display()
                        ));
                    }
                    None => None,
                }
            }
            None => None,
        };
        ensure_session_store_open_deadline(deadline)?;
        Ok(SessionDirectoryPreflight {
            sessions_dir,
            parent_path,
            retained_ancestor,
            retained_directory,
            retained_identity_file,
            sensitive_values: config.public_session_sensitive_values(),
            runtime_config: config,
        })
    }

    pub(crate) fn for_existing_project_with_lock_deadline(
        project_root: &Path,
        deadline: Instant,
    ) -> Result<Self, String> {
        ensure_session_store_open_deadline(deadline)?;
        let config = crate::config::load_nib_config_full_read_only_until(project_root, deadline)
            .map_err(|error| error.to_string())?;
        ensure_session_store_open_deadline(deadline)?;
        let sessions_dir =
            crate::profile::ProfileRegistry::resolve_sessions_dir_without_migration_until(
                project_root,
                &config.profiles,
                deadline,
            )
            .map_err(|error| error.to_string())?;
        ensure_session_store_open_deadline(deadline)?;
        let (sessions_dir, directory, parent_directory, identity_file) =
            open_existing_session_directory_until(&sessions_dir, deadline)?;
        ensure_session_store_open_deadline(deadline)?;
        Ok(Self {
            sessions_dir,
            directory: Some(Arc::new(directory)),
            parent_directory: Some(Arc::new(parent_directory)),
            directory_identity_file: Some(Arc::new(identity_file)),
            initialization_error: None,
            lock_timeout: None,
            lock_deadline: Some(deadline),
            sensitive_values: Arc::new(config.public_session_sensitive_values()),
        })
    }

    pub(crate) fn at_existing_dir_with_identity_until(
        sessions_dir: &Path,
        expected_identity: crate::fs_security::FileIdentitySnapshot,
        deadline: Instant,
    ) -> Result<Self, String> {
        let (sessions_dir, directory, parent_directory, identity_file) =
            open_existing_session_directory_until(sessions_dir, deadline)?;
        let observed_identity = crate::fs_security::file_identity_snapshot(&identity_file)
            .map_err(|error| error.to_string())?;
        if observed_identity != expected_identity {
            return Err(format!(
                "existing session directory identity changed: {}",
                sessions_dir.display()
            ));
        }
        ensure_session_store_open_deadline(deadline)?;
        Ok(Self {
            sessions_dir,
            directory: Some(Arc::new(directory)),
            parent_directory: Some(Arc::new(parent_directory)),
            directory_identity_file: Some(Arc::new(identity_file)),
            initialization_error: None,
            lock_timeout: None,
            lock_deadline: Some(deadline),
            sensitive_values: Arc::new(Vec::new()),
        })
    }

    pub(crate) fn for_project_until(
        project_root: &Path,
        deadline: Option<Instant>,
    ) -> Result<Self, String> {
        let config = match deadline {
            Some(deadline) => crate::config::load_nib_config_full_until(project_root, deadline),
            None => crate::config::load_nib_config_full(project_root),
        }
        .map_err(|error| error.to_string())?;
        let profiles = crate::profile::ProfileRegistry::load(project_root, &config.profiles)
            .map_err(|error| error.to_string())?;
        let profile = profiles
            .for_workspace(project_root)
            .unwrap_or_else(|| profiles.default_profile());
        profile
            .ensure_state_dirs()
            .map_err(|error| error.to_string())?;
        let store = Self::at_dir(profile.sessions_dir().to_path_buf())
            .with_sensitive_values(config.public_session_sensitive_values());
        match deadline {
            Some(deadline) => {
                if Instant::now() >= deadline {
                    return Err("session store lock deadline elapsed".to_string());
                }
                Ok(store.with_lock_deadline(deadline))
            }
            None => Ok(store),
        }
    }

    pub fn at_dir(sessions_dir: PathBuf) -> Self {
        let requested = crate::fs_security::absolute_path(&sessions_dir)
            .unwrap_or_else(|_| sessions_dir.clone());
        match open_session_directory(&requested) {
            Ok((sessions_dir, directory, parent_directory, identity_file)) => Self {
                sessions_dir,
                directory: Some(Arc::new(directory)),
                parent_directory: Some(Arc::new(parent_directory)),
                directory_identity_file: Some(Arc::new(identity_file)),
                initialization_error: None,
                lock_timeout: None,
                lock_deadline: None,
                sensitive_values: Arc::new(Vec::new()),
            },
            Err(error) => Self {
                sessions_dir: requested,
                directory: None,
                parent_directory: None,
                directory_identity_file: None,
                initialization_error: Some(format!(
                    "session directory is unsafe or unavailable: {error}"
                )),
                lock_timeout: None,
                lock_deadline: None,
                sensitive_values: Arc::new(Vec::new()),
            },
        }
    }

    pub(crate) fn with_sensitive_values(mut self, values: Vec<String>) -> Self {
        self.sensitive_values = Arc::new(values);
        self
    }

    pub(crate) fn public_sensitive_values(&self) -> &[String] {
        self.sensitive_values.as_slice()
    }

    pub(crate) fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = Some(timeout);
        self.lock_deadline = None;
        self
    }

    pub(crate) fn with_lock_deadline(mut self, deadline: Instant) -> Self {
        self.lock_deadline = Some(deadline);
        self.lock_timeout = None;
        self
    }

    #[doc(hidden)]
    pub fn with_lock_timeout_for_testing(self, timeout: Duration) -> Self {
        self.with_lock_timeout(timeout)
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub(crate) fn persistent_directory_identity(
        &self,
    ) -> Result<crate::fs_security::FileIdentitySnapshot, SessionError> {
        self.verify_directory_binding()?;
        let identity = self.directory_identity_file.as_deref().ok_or_else(|| {
            SessionError::InvalidMutation(
                "session directory identity is not initialized".to_string(),
            )
        })?;
        crate::fs_security::file_identity_snapshot(identity).map_err(|error| {
            SessionError::InvalidMutation(format!(
                "failed to snapshot session directory identity: {error}"
            ))
        })
    }

    pub(crate) fn try_acquire_run_lease(&self, id: &str) -> Result<SessionRunLease, SessionError> {
        self.validate_session_id(id)?;
        self.verify_directory_binding()?;
        let lock_path = self.sessions_dir.join(format!(".session-run-{id}.lock"));
        let lock = crate::daemons::state::try_acquire_file_lock_in(&lock_path, &self.sessions_dir)
            .map_err(|error| {
                if error.contains("already held by another owner") {
                    SessionError::RunLeaseHeld(id.to_string())
                } else {
                    SessionError::InvalidMutation(format!(
                        "failed to acquire active run lease for {id}: {error}"
                    ))
                }
            })?;
        self.verify_directory_binding()?;
        Ok(SessionRunLease {
            session_id: id.to_string(),
            sessions_dir: self.sessions_dir.clone(),
            lock,
        })
    }

    pub(crate) fn path(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{id}.json"))
    }

    pub(crate) fn lock_path(&self, id: &str) -> Result<PathBuf, SessionError> {
        self.validate_session_id(id)?;
        let stripe = session_lock_stripe(id);
        Ok(self
            .sessions_dir
            .join(format!(".session-lock-{stripe:02}.lock")))
    }

    pub(crate) fn validate_session_id(&self, id: &str) -> Result<(), SessionError> {
        let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
            id,
            self.sensitive_values.iter().cloned(),
        );
        if redacted != id {
            return Err(SessionError::SensitiveSessionId);
        }
        validate_session_id(id)
    }

    pub(crate) fn process_lock(
        &self,
        path: &Path,
        deadline: Option<Instant>,
    ) -> Result<Arc<SessionMutex>, SessionError> {
        let registry = SESSION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        self.process_lock_in(registry, path, deadline)
    }

    pub(crate) fn process_lock_in(
        &self,
        registry: &SessionLockRegistry,
        path: &Path,
        deadline: Option<Instant>,
    ) -> Result<Arc<SessionMutex>, SessionError> {
        let mut registry = lock_session_mutex(registry, path, deadline)?;
        ensure_session_lock_deadline(deadline, path)?;
        registry.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
        Ok(lock)
    }

    pub(crate) fn directory(
        &self,
    ) -> Result<&crate::daemons::state::StableDirectory, SessionError> {
        if let Some(error) = &self.initialization_error {
            return Err(SessionError::InvalidMutation(error.clone()));
        }
        self.directory.as_deref().ok_or_else(|| {
            SessionError::InvalidMutation("session directory is not initialized".to_string())
        })
    }

    pub(crate) fn verify_directory_binding(&self) -> Result<(), SessionError> {
        let directory = self.directory()?;
        let parent = self.parent_directory.as_deref().ok_or_else(|| {
            SessionError::InvalidMutation("session parent directory is not initialized".to_string())
        })?;
        let identity_file = self.directory_identity_file.as_deref().ok_or_else(|| {
            SessionError::InvalidMutation(
                "session directory identity is not initialized".to_string(),
            )
        })?;
        let visible_marker = self.sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
        let anchor = session_directory_identity_anchor(&visible_marker)
            .map_err(SessionError::InvalidMutation)?;

        directory
            .verify_visible()
            .and_then(|_| parent.verify_visible())
            .and_then(|_| directory.verify_file_identity(&visible_marker, identity_file))
            .and_then(|_| parent.verify_file_identity(&anchor, identity_file))
            .map_err(SessionError::InvalidMutation)
    }

    pub(crate) fn with_anchored_lock<T>(
        &self,
        lock_path: PathBuf,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_deadline(lock_path, self.lock_deadline(), operation)
    }

    pub(crate) fn lock_deadline(&self) -> Option<Instant> {
        if let Some(deadline) = self.lock_deadline {
            return Some(deadline);
        }
        let now = Instant::now();
        self.lock_timeout
            .or_else(|| SESSION_LOCK_POLICY.try_with(|policy| policy.timeout).ok())
            .map(|timeout| now.checked_add(timeout).unwrap_or(now))
    }

    pub(crate) fn with_anchored_lock_until<T>(
        &self,
        lock_path: PathBuf,
        deadline: Instant,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_deadline(lock_path, Some(deadline), operation)
    }

    pub(crate) fn with_anchored_lock_deadline<T>(
        &self,
        lock_path: PathBuf,
        deadline: Option<Instant>,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let should_offload = SESSION_LOCK_POLICY
            .try_with(|policy| policy.offload_waits)
            .unwrap_or(false)
            && SESSION_LOCK_OFFLOAD_DEPTH.with(|depth| depth.get() == 0)
            && tokio::runtime::Handle::try_current().is_ok_and(|handle| {
                matches!(
                    handle.runtime_flavor(),
                    tokio::runtime::RuntimeFlavor::MultiThread
                )
            });
        if should_offload {
            return tokio::task::block_in_place(|| {
                let _guard = SessionLockOffloadGuard::enter();
                self.with_anchored_lock_deadline_inner(lock_path, deadline, operation)
            });
        }
        self.with_anchored_lock_deadline_inner(lock_path, deadline, operation)
    }

    pub(crate) fn with_anchored_lock_deadline_inner<T>(
        &self,
        lock_path: PathBuf,
        deadline: Option<Instant>,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.verify_directory_binding()?;
        let process_lock = self.process_lock(&lock_path, deadline)?;
        let _guard = lock_session_mutex(&process_lock, &lock_path, deadline)?;
        self.verify_directory_binding()?;
        ensure_session_lock_deadline(deadline, &lock_path)?;

        let directory = self.directory()?;
        let mut outcome = None;
        let lock_operation = |_current_directory: &crate::daemons::state::StableDirectory| {
            self.verify_directory_binding()
                .map_err(|error| error.to_string())?;
            ensure_session_lock_deadline(deadline, &lock_path)
                .map_err(|error| error.to_string())?;
            match operation(directory) {
                Ok(value) => {
                    self.verify_directory_binding()
                        .map_err(|error| error.to_string())?;
                    outcome = Some(Ok(value));
                    Ok(())
                }
                Err(error) => {
                    outcome = Some(Err(error));
                    Err(OPERATION_ERROR_SENTINEL.to_string())
                }
            }
        };
        let lock_result = match deadline {
            Some(deadline) => crate::daemons::state::with_file_lock_in_until(
                &lock_path,
                &self.sessions_dir,
                deadline,
                lock_operation,
            ),
            None => crate::daemons::state::with_file_lock_in(
                &lock_path,
                &self.sessions_dir,
                lock_operation,
            ),
        };

        match lock_result {
            Ok(()) => outcome.unwrap_or_else(|| {
                Err(SessionError::InvalidMutation(
                    "stable session lock returned without an operation result".to_string(),
                ))
            }),
            Err(error) if error == OPERATION_ERROR_SENTINEL => {
                outcome.unwrap_or_else(|| Err(SessionError::InvalidMutation(error)))
            }
            Err(error) => Err(SessionError::InvalidMutation(error)),
        }
    }

    pub(crate) fn with_session_lock<T>(
        &self,
        id: &str,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock(self.lock_path(id)?, operation)
    }

    pub(crate) fn with_session_lock_until<T>(
        &self,
        id: &str,
        deadline: Instant,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_until(self.lock_path(id)?, deadline, operation)
    }

    /// Serializes writes and destructive maintenance that depend on skill usage.
    /// Session JSON remains authoritative; the curator holds this lock while it
    /// rebuilds its cross-session aggregate and decides whether a skill is stale.
    pub(crate) fn with_skill_usage_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock(self.sessions_dir.join(SKILL_USAGE_LOCK_FILE), |_| {
            operation()
        })
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn with_skill_usage_lock_for_testing<T>(
        &self,
        operation: impl FnOnce() -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_skill_usage_lock(operation)
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn with_session_lock_for_testing<T>(
        &self,
        id: &str,
        operation: impl FnOnce() -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_session_lock(id, |_| operation())
    }

    pub(crate) fn with_skill_usage_lock_until<T>(
        &self,
        deadline: Instant,
        operation: impl FnOnce() -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_until(
            self.sessions_dir.join(SKILL_USAGE_LOCK_FILE),
            deadline,
            |_| operation(),
        )
    }

    pub(crate) fn load_opened_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        id: &str,
    ) -> Result<Option<OpenedSession>, SessionError> {
        self.load_opened_unlocked_with_hook(directory, id, || Ok(()))
    }

    pub(crate) fn load_opened_unlocked_with_hook(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        id: &str,
        after_read: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<Option<OpenedSession>, SessionError> {
        let path = self.path(id);
        if !directory
            .path_exists(&path)
            .map_err(SessionError::InvalidMutation)?
        {
            return Ok(None);
        }
        let file = directory
            .open_read(&path)
            .map_err(SessionError::InvalidMutation)?;
        let metadata = file.metadata()?;
        if metadata.len() > MAX_SESSION_JSON_BYTES {
            return Err(SessionError::FileTooLarge {
                path: path.display().to_string(),
                size: metadata.len(),
                max: MAX_SESSION_JSON_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        let mut reader = (&file).take(MAX_SESSION_JSON_BYTES + 1);
        reader.read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SESSION_JSON_BYTES {
            return Err(SessionError::FileTooLarge {
                path: path.display().to_string(),
                size: bytes.len() as u64,
                max: MAX_SESSION_JSON_BYTES,
            });
        }
        after_read()?;
        directory
            .verify_file_identity(&path, &file)
            .map_err(SessionError::InvalidMutation)?;
        let session: Session = serde_json::from_slice(&bytes)?;
        if session.id != id {
            return Err(SessionError::SessionIdMismatch {
                expected: id.to_string(),
                actual: session.id,
            });
        }
        session.validate()?;
        Ok(Some(OpenedSession {
            session,
            file,
            metadata,
        }))
    }

    /// Strictly loads a session. Missing files are distinct from unreadable, corrupt,
    /// or invariant-violating files so callers cannot accidentally recreate over them.
    pub fn load_result(&self, id: &str) -> Result<Option<Session>, SessionError> {
        self.with_session_lock(id, |directory| {
            Ok(self
                .load_opened_unlocked(directory, id)?
                .map(|opened| opened.session))
        })
    }

    pub(crate) fn load_result_with_deadline(
        &self,
        id: &str,
        deadline: Instant,
    ) -> Result<Option<Session>, SessionError> {
        self.with_session_lock_until(id, deadline, |directory| {
            Ok(self
                .load_opened_unlocked(directory, id)?
                .map(|opened| opened.session))
        })
    }

    pub(crate) fn load_result_with_metadata(
        &self,
        id: &str,
    ) -> Result<Option<(Session, fs::Metadata)>, SessionError> {
        self.with_session_lock(id, |directory| {
            Ok(self
                .load_opened_unlocked(directory, id)?
                .map(|opened| (opened.session, opened.metadata)))
        })
    }

    /// Compatibility wrapper for read-only callers. Corruption fails loudly instead of
    /// being reported as a missing session; new code should prefer [`Self::load_result`].
    pub fn load(&self, id: &str) -> Option<Session> {
        self.load_result(id)
            .unwrap_or_else(|error| panic!("failed to load session {id}: {error}"))
    }

    pub fn try_create_session(&self) -> Result<Session, SessionError> {
        self.try_create_session_with_id(Uuid::new_v4().to_string())
    }

    pub fn create_session(&self) -> Session {
        self.try_create_session()
            .unwrap_or_else(|error| panic!("failed to create session: {error}"))
    }

    pub fn try_create_session_with_id(
        &self,
        id: impl Into<String>,
    ) -> Result<Session, SessionError> {
        let id = id.into();
        self.with_session_lock(&id, |directory| {
            if let Some(opened) = self.load_opened_unlocked(directory, &id)? {
                return Ok(opened.session);
            }
            let session = Session::new(id.clone());
            self.save_unlocked(directory, &session, None)?;
            Ok(session)
        })
    }

    pub(crate) fn create_unpublished_session_with_receipt_and_guard(
        &self,
        session: &Session,
        external_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<File, SessionError> {
        let id = session.id.as_str();
        self.validate_session_id(id)?;
        self.verify_directory_binding()?;
        let deadline = self.lock_deadline().ok_or_else(|| {
            SessionError::InvalidMutation(
                "unpublished session preparation requires an absolute deadline".to_string(),
            )
        })?;
        ensure_session_lock_deadline(Some(deadline), &self.path(id))?;
        external_guard().map_err(SessionError::InvalidMutation)?;
        let directory = self.directory()?;
        let path = self.path(id);
        if directory
            .path_exists(&path)
            .map_err(SessionError::InvalidMutation)?
        {
            return Err(SessionError::InvalidMutation(format!(
                "unpublished session already exists: {id}"
            )));
        }
        session.validate()?;
        let encoded = serde_json::to_vec_pretty(session)?;
        ensure_session_lock_deadline(Some(deadline), &path)?;
        let receipt = match directory.save_bytes_atomically_expected_with_receipt_and_guard(
            &path,
            &encoded,
            ".nib-session-",
            crate::daemons::state::FileExpectation::Missing,
            || {
                external_guard()?;
                ensure_session_lock_deadline(Some(deadline), &path)
                    .map_err(|error| error.to_string())?;
                self.verify_directory_binding()
                    .map_err(|error| error.to_string())
            },
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                let message = error.message;
                let receipt = error
                    .receipt
                    .ok_or_else(|| SessionError::InvalidMutation(message.clone()))?;
                if !receipt.exact_identity {
                    return Err(SessionError::InvalidMutation(format!(
                        "{message}; unpublished session receipt was not exact"
                    )));
                }
                let mut guard = || {
                    external_guard()?;
                    ensure_session_lock_deadline(Some(deadline), &path)
                        .map_err(|error| error.to_string())?;
                    self.verify_directory_binding()
                        .map_err(|error| error.to_string())
                };
                directory
                    .finalize_failed_exact_publication_with_guard(
                        &path,
                        None,
                        &receipt,
                        ".nib-session-",
                        &encoded,
                        &mut guard,
                    )
                    .map_err(|recovery| {
                        SessionError::InvalidMutation(format!(
                            "{message}; failed to finalize exact unpublished session: {recovery}"
                        ))
                    })?;
                receipt
            }
        };
        if !receipt.exact_identity {
            return Err(SessionError::InvalidMutation(format!(
                "unpublished session preparation did not retain exact publication identity: {}",
                path.display()
            )));
        }
        ensure_session_lock_deadline(Some(deadline), &path)?;
        external_guard().map_err(SessionError::InvalidMutation)?;
        let opened = self
            .load_opened_unlocked(directory, id)?
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        directory
            .verify_file_identity(&path, &receipt.file)
            .map_err(SessionError::InvalidMutation)?;
        if opened.session != *session {
            return Err(SessionError::InvalidMutation(format!(
                "unpublished session publication changed during preparation: {}",
                path.display()
            )));
        }
        ensure_session_lock_deadline(Some(deadline), &path)?;
        external_guard().map_err(SessionError::InvalidMutation)?;
        Ok(receipt.file)
    }

    pub fn create_session_with_id(&self, id: impl Into<String>) -> Session {
        let id = id.into();
        self.try_create_session_with_id(id.clone())
            .unwrap_or_else(|error| panic!("failed to create session {id}: {error}"))
    }

    pub(crate) fn save_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        session: &Session,
        expected: Option<&File>,
    ) -> Result<(), SessionError> {
        self.save_unlocked_with_commit_check(directory, session, expected, || Ok(()))
    }

    pub(crate) fn save_unlocked_with_commit_check(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        session: &Session,
        expected: Option<&File>,
        before_commit: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        self.save_unlocked_with_namespace_guard(
            directory,
            session,
            expected,
            &mut || Ok(()),
            before_commit,
        )
    }

    pub(crate) fn save_unlocked_with_namespace_guard(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        session: &Session,
        expected: Option<&File>,
        namespace_guard: &mut impl FnMut() -> Result<(), SessionError>,
        before_commit: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        session.validate()?;
        let path = self.path(&session.id);
        let data = serde_json::to_vec_pretty(session)?;
        if data.len() as u64 > MAX_SESSION_JSON_BYTES {
            return Err(SessionError::FileTooLarge {
                path: path.display().to_string(),
                size: data.len() as u64,
                max: MAX_SESSION_JSON_BYTES,
            });
        }
        let expected = expected.map_or(
            crate::daemons::state::FileExpectation::Missing,
            crate::daemons::state::FileExpectation::Present,
        );
        directory
            .save_bytes_atomically_expected_with_guard_and_hook(
                &path,
                &data,
                ".nib-session-",
                true,
                expected,
                || namespace_guard().map_err(|error| error.to_string()),
                || {
                    self.verify_directory_binding()
                        .map_err(|error| error.to_string())?;
                    before_commit().map_err(|error| error.to_string())?;
                    self.verify_directory_binding()
                        .map_err(|error| error.to_string())
                },
            )
            .map_err(SessionError::InvalidMutation)?;
        namespace_guard()?;
        self.verify_directory_binding()?;
        namespace_guard()?;
        let published = self
            .load_opened_unlocked(directory, &session.id)?
            .ok_or_else(|| SessionError::NotFound(session.id.clone()))?;
        namespace_guard()?;
        if published.session != *session {
            return Err(SessionError::InvalidMutation(format!(
                "published session did not retain the requested {}: {}",
                session_mismatch_field(session, &published.session),
                path.display()
            )));
        }
        namespace_guard()?;
        Ok(())
    }

    pub(crate) fn save_unlocked_with_deadline(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        session: &Session,
        expected: Option<&File>,
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        self.save_unlocked_with_deadline_and_commit_check(
            directory,
            session,
            expected,
            deadline,
            || Ok(()),
        )
    }

    pub(crate) fn save_unlocked_with_deadline_and_commit_check(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        session: &Session,
        expected: Option<&File>,
        deadline: Option<Instant>,
        before_commit: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        match deadline {
            Some(deadline) => {
                let path = self.path(&session.id);
                self.save_unlocked_with_namespace_guard(
                    directory,
                    session,
                    expected,
                    &mut || ensure_session_lock_deadline(Some(deadline), &path),
                    before_commit,
                )
            }
            None => {
                before_commit()?;
                self.save_unlocked(directory, session, expected)
            }
        }
    }

    pub fn save(&self, session: &mut Session) -> Result<(), SessionError> {
        match self.lock_deadline() {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, || {
                self.save_under_skill_usage_lock_with_deadline(session, Some(deadline))
            }),
            None => self.with_skill_usage_lock(|| self.save_under_skill_usage_lock(session)),
        }
    }

    pub(crate) fn save_under_skill_usage_lock(
        &self,
        session: &mut Session,
    ) -> Result<(), SessionError> {
        self.save_under_skill_usage_lock_with_deadline(session, None)
    }

    pub(crate) fn save_under_skill_usage_lock_with_deadline(
        &self,
        session: &mut Session,
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        let committed_revision = session.revision.checked_add(1).ok_or_else(|| {
            SessionError::InvalidMutation("session revision overflowed".to_string())
        })?;
        let operation = |directory: &crate::daemons::state::StableDirectory| {
            // Refuse to replace an existing session that cannot itself be loaded and
            // validated. Recovery must be an explicit operation, never an incidental save.
            let opened = self.load_opened_unlocked(directory, &session.id)?;
            let mut next = session.clone();
            if let Some(opened) = opened.as_ref() {
                if session.revision != opened.session.revision {
                    return Err(SessionError::InvalidMutation(format!(
                        "stale session revision for {}: snapshot={}, current={}",
                        session.id, session.revision, opened.session.revision
                    )));
                }
            } else if session.revision != 0 {
                return Err(SessionError::InvalidMutation(format!(
                    "session {} is missing but snapshot revision is {}",
                    session.id, session.revision
                )));
            }
            next.revision = committed_revision;
            self.save_unlocked_with_deadline(
                directory,
                &next,
                opened.as_ref().map(|opened| &opened.file),
                deadline,
            )
        };
        match deadline {
            Some(deadline) => self.with_session_lock_until(&session.id, deadline, operation),
            None => self.with_session_lock(&session.id, operation),
        }?;
        session.revision = committed_revision;
        Ok(())
    }

    pub fn update_session<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        match self.lock_deadline() {
            Some(deadline) => self.update_session_with_deadline(id, deadline, update),
            None => self
                .with_skill_usage_lock(|| self.update_session_under_skill_usage_lock(id, update)),
        }
    }

    pub(crate) fn update_session_with_deadline<T>(
        &self,
        id: &str,
        deadline: Instant,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_skill_usage_lock_until(deadline, || {
            self.update_session_under_skill_usage_lock_with_deadline(id, Some(deadline), update)
        })
    }

    pub(crate) fn update_session_under_skill_usage_lock<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.update_session_under_skill_usage_lock_with_deadline(id, None, update)
    }

    pub(crate) fn update_session_under_skill_usage_lock_with_deadline<T>(
        &self,
        id: &str,
        deadline: Option<Instant>,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let operation = |directory: &crate::daemons::state::StableDirectory| {
            let mut opened = self
                .load_opened_unlocked(directory, id)?
                .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
            let revision = opened.session.revision;
            let result = update(&mut opened.session)?;
            opened.session.revision = revision.checked_add(1).ok_or_else(|| {
                SessionError::InvalidMutation("session revision overflowed".to_string())
            })?;
            self.save_unlocked_with_deadline(
                directory,
                &opened.session,
                Some(&opened.file),
                deadline,
            )?;
            Ok(result)
        };
        match deadline {
            Some(deadline) => self.with_session_lock_until(id, deadline, operation),
            None => self.with_session_lock(id, operation),
        }
    }

    pub fn update_or_create_session<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        match self.lock_deadline() {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, || {
                self.update_or_create_session_under_skill_usage_lock_with_deadline(
                    id,
                    Some(deadline),
                    update,
                )
            }),
            None => self.with_skill_usage_lock(|| {
                self.update_or_create_session_under_skill_usage_lock(id, update)
            }),
        }
    }

    pub(crate) fn update_or_create_session_with_deadline<T>(
        &self,
        id: &str,
        deadline: Instant,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_skill_usage_lock_until(deadline, || {
            self.update_or_create_session_under_skill_usage_lock_with_deadline(
                id,
                Some(deadline),
                update,
            )
        })
    }

    pub(crate) fn update_or_create_session_under_skill_usage_lock<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.update_or_create_session_under_skill_usage_lock_with_deadline(id, None, update)
    }

    pub(crate) fn update_or_create_session_under_skill_usage_lock_with_deadline<T>(
        &self,
        id: &str,
        deadline: Option<Instant>,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let operation = |directory: &crate::daemons::state::StableDirectory| {
            let opened = self.load_opened_unlocked(directory, id)?;
            let mut session = opened
                .as_ref()
                .map(|opened| opened.session.clone())
                .unwrap_or_else(|| Session::new(id.to_string()));
            let revision = session.revision;
            let result = update(&mut session)?;
            session.revision = revision.checked_add(1).ok_or_else(|| {
                SessionError::InvalidMutation("session revision overflowed".to_string())
            })?;
            self.save_unlocked_with_deadline(
                directory,
                &session,
                opened.as_ref().map(|opened| &opened.file),
                deadline,
            )?;
            Ok(result)
        };
        match deadline {
            Some(deadline) => self.with_session_lock_until(id, deadline, operation),
            None => self.with_session_lock(id, operation),
        }
    }

    /// Atomically reloads and conditionally deletes a session while holding the
    /// same process and file locks used by every session mutation.
    pub fn delete_if(
        &self,
        id: &str,
        should_delete: impl FnOnce(&Session, &fs::Metadata) -> bool,
    ) -> Result<SessionDeleteOutcome, SessionError> {
        self.delete_if_with_commit_check(id, should_delete, || Ok(()))
    }

    pub(crate) fn delete_if_with_commit_check(
        &self,
        id: &str,
        should_delete: impl FnOnce(&Session, &fs::Metadata) -> bool,
        before_delete: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<SessionDeleteOutcome, SessionError> {
        let deadline = self.lock_deadline();
        let operation = || {
            let session_operation = |directory: &crate::daemons::state::StableDirectory| {
                let path = self.path(id);
                directory
                    .recover_quarantined_file(&path, ".nib-session-delete-")
                    .map_err(SessionError::InvalidMutation)?;
                let Some(opened) = self.load_opened_unlocked(directory, id)? else {
                    return Ok(SessionDeleteOutcome::Missing);
                };
                if !should_delete(&opened.session, &opened.metadata) {
                    return Ok(SessionDeleteOutcome::Retained);
                }
                directory
                    .remove_file_if_matches_with_hook(
                        &path,
                        &opened.file,
                        ".nib-session-delete-",
                        || before_delete().map_err(|error| error.to_string()),
                    )
                    .map_err(SessionError::InvalidMutation)?;
                self.verify_directory_binding()?;
                Ok(SessionDeleteOutcome::Deleted)
            };
            match deadline {
                Some(deadline) => self.with_session_lock_until(id, deadline, session_operation),
                None => self.with_session_lock(id, session_operation),
            }
        };
        match deadline {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, operation),
            None => self.with_skill_usage_lock(operation),
        }
    }

    pub fn append_message(&self, id: &str, role: &str, content: &str) -> Session {
        match self.try_append_message(id, role, content) {
            Ok(session) => session,
            Err(error) => {
                if !matches!(
                    &error,
                    SessionError::InvalidRole(_) | SessionError::RoleViolation { .. }
                ) {
                    panic!("failed to append to session {id}: {error}");
                }
                let error_message = error.to_string();
                self.record_event(
                    id,
                    "role_violation",
                    serde_json::json!({
                        "role": role,
                        "content": content,
                        "error": error_message,
                    }),
                )
                .unwrap_or_else(|audit_error| {
                    panic!("failed to audit role violation for session {id}: {audit_error}")
                });
                self.load_result(id)
                    .unwrap_or_else(|load_error| {
                        panic!("failed to reload session {id}: {load_error}")
                    })
                    .unwrap_or_else(|| panic!("session {id} disappeared after role violation"))
            }
        }
    }

    pub fn try_append_message(
        &self,
        id: &str,
        role: &str,
        content: &str,
    ) -> Result<Session, SessionError> {
        self.try_append_message_with_origin(id, role, content, MessageOrigin::Unknown)
    }

    pub fn try_append_message_with_origin(
        &self,
        id: &str,
        role: &str,
        content: &str,
        origin: MessageOrigin,
    ) -> Result<Session, SessionError> {
        self.update_or_create_session(id, |session| {
            validate_role_transition(session.messages.last().map(|m| m.role.as_str()), role)?;
            validate_message_origin_role(role, origin)?;
            let index = session.messages.len();
            session.messages.push(SessionMessage {
                index,
                role: role.to_string(),
                content: content.to_string(),
                timestamp: Some(Utc::now()),
                attachments: Vec::new(),
            });
            if origin != MessageOrigin::Unknown {
                session.message_provenance.push(MessageProvenance {
                    message_index: index,
                    origin,
                });
            }
            Ok(session.clone())
        })
    }

    pub fn record_event(
        &self,
        id: &str,
        kind: impl Into<String>,
        details: serde_json::Value,
    ) -> Result<SessionEvent, SessionError> {
        let kind = kind.into();
        self.update_or_create_session(id, |session| {
            let event = SessionEvent {
                index: session.events.len(),
                kind,
                details,
                timestamp: Some(Utc::now()),
            };
            session.events.push(event.clone());
            Ok(event)
        })
    }

    pub(crate) fn record_event_once_with_deadline(
        &self,
        id: &str,
        kind: &str,
        reconciliation_id: &str,
        details: serde_json::Value,
        legacy_details: serde_json::Value,
        deadline: Instant,
    ) -> Result<(), SessionError> {
        self.with_skill_usage_lock_until(deadline, || {
            self.with_session_lock_until(id, deadline, |directory| {
                let opened = self.load_opened_unlocked(directory, id)?;
                let mut session = opened
                    .as_ref()
                    .map(|opened| opened.session.clone())
                    .unwrap_or_else(|| Session::new(id.to_string()));
                let mut exact_index = None;
                let mut legacy_index = None;
                for (index, event) in session.events.iter().enumerate() {
                    if event.kind != kind {
                        continue;
                    }
                    if event
                        .details
                        .get("reconciliation_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(reconciliation_id)
                    {
                        if event.details != details || exact_index.replace(index).is_some() {
                            return Err(SessionError::InvalidMutation(format!(
                                "session {id} has conflicting duplicate reconciliation audit evidence"
                            )));
                        }
                    } else if event.details == legacy_details
                        && legacy_index.replace(index).is_some()
                    {
                        return Err(SessionError::InvalidMutation(format!(
                            "session {id} has duplicate legacy reconciliation audit evidence"
                        )));
                    }
                }
                let session_path = self.path(id);
                ensure_session_lock_deadline(Some(deadline), &session_path)?;
                if exact_index.is_some() {
                    if legacy_index.is_some() {
                        return Err(SessionError::InvalidMutation(format!(
                            "session {id} has both legacy and identified reconciliation audit evidence"
                        )));
                    }
                    return Ok(());
                }

                let revision = session.revision;
                if let Some(index) = legacy_index {
                    session.events[index].details = details;
                } else {
                    let index = session.events.len();
                    session.events.push(SessionEvent {
                        index,
                        kind: kind.to_string(),
                        details,
                        timestamp: Some(Utc::now()),
                    });
                }
                session.revision = revision.checked_add(1).ok_or_else(|| {
                    SessionError::InvalidMutation("session revision overflowed".to_string())
                })?;
                self.save_unlocked_with_deadline_and_commit_check(
                    directory,
                    &session,
                    opened.as_ref().map(|opened| &opened.file),
                    Some(deadline),
                    || {
                        pause_record_event_once_commit(deadline);
                        ensure_session_lock_deadline(Some(deadline), &session_path)
                    },
                )
            })
        })
    }

    pub fn record_skill_usage(
        &self,
        id: &str,
        skill_name: impl Into<String>,
        reason: Option<String>,
    ) -> Result<SkillUsageRecord, SessionError> {
        let skill_name = skill_name.into();
        crate::context::skills::canonical_skill_id(&skill_name).map_err(|error| {
            SessionError::InvalidMutation(format!("invalid skill name: {error}"))
        })?;
        let deadline = self.lock_deadline();
        let operation = || {
            self.update_or_create_session_under_skill_usage_lock_with_deadline(
                id,
                deadline,
                |session| {
                    if !session.active_skills.iter().any(|name| name == &skill_name) {
                        session.active_skills.push(skill_name.clone());
                    }
                    let usage = SkillUsageRecord {
                        skill_name,
                        reason,
                        timestamp: Some(Utc::now()),
                    };
                    session.skill_usage.push(usage.clone());
                    Ok(usage)
                },
            )
        };
        match deadline {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, operation),
            None => self.with_skill_usage_lock(operation),
        }
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn list_entries_result(
        &self,
        max_sessions: usize,
        max_total_bytes: Option<u64>,
        validate_contents: bool,
        deadline: Option<Instant>,
    ) -> Result<Vec<String>, SessionError> {
        self.verify_directory_binding()?;
        let directory = self.directory()?;
        directory
            .recover_stale_temporary_files_strict(
                ".nib-session-",
                MAX_SESSION_DIRECTORY_ENTRIES.saturating_mul(4),
                MAX_SESSION_DIRECTORY_NAME_BYTES.saturating_mul(4),
            )
            .map_err(SessionError::InvalidMutation)?;
        let mut ids = Vec::new();
        let mut total_bytes = 0_u64;
        directory
            .for_each_entry_bounded(
                MAX_SESSION_DIRECTORY_ENTRIES,
                MAX_SESSION_DIRECTORY_NAME_BYTES,
                |name| {
                    if crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
                        &name,
                        ".nib-session-",
                    ) {
                        return Ok(());
                    }
                    let name_path = PathBuf::from(&name);
                    if name_path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        != Some("json")
                    {
                        return Ok(());
                    }
                    let id = name_path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .ok_or_else(|| {
                            format!(
                                "session path is not valid UTF-8: {}",
                                self.sessions_dir.join(&name).display()
                            )
                        })?
                        .to_string();
                    self.validate_session_id(&id)
                        .map_err(|error| error.to_string())?;
                    if ids.len() >= max_sessions {
                        return Err(format!(
                            "session directory {} exceeds the {max_sessions}-session limit",
                            self.sessions_dir.display()
                        ));
                    }
                    let path = self.sessions_dir.join(&name);
                    let file = directory.open_read(&path)?;
                    let metadata = file.metadata().map_err(|error| error.to_string())?;
                    if !metadata.is_file() {
                        return Err(format!(
                            "session path is not a regular file: {}",
                            path.display()
                        ));
                    }
                    if metadata.len() > MAX_SESSION_JSON_BYTES {
                        return Err(format!(
                            "session file {} exceeds the {MAX_SESSION_JSON_BYTES}-byte limit",
                            path.display()
                        ));
                    }
                    directory.verify_file_identity(&path, &file)?;
                    total_bytes = total_bytes.checked_add(metadata.len()).ok_or_else(|| {
                        "profile session byte count overflowed during enumeration".to_string()
                    })?;
                    if max_total_bytes.is_some_and(|maximum| total_bytes > maximum) {
                        return Err(format!(
                            "profile sessions exceed the {}-byte skill usage aggregation limit",
                            max_total_bytes.expect("checked maximum")
                        ));
                    }
                    ids.push(id);
                    Ok(())
                },
            )
            .map_err(|error| {
                if error == SessionError::SensitiveSessionId.to_string() {
                    SessionError::SensitiveSessionId
                } else {
                    SessionError::InvalidMutation(error)
                }
            })?;
        ids.sort();
        if validate_contents {
            for id in &ids {
                let path = self.path(id);
                let validate = |locked_directory: &crate::daemons::state::StableDirectory| {
                    self.load_opened_unlocked(locked_directory, id)?
                        .map(|_| ())
                        .ok_or_else(|| {
                            SessionError::InvalidMutation(format!(
                                "session disappeared during enumeration: {}",
                                path.display()
                            ))
                        })
                };
                match deadline {
                    Some(deadline) => self.with_session_lock_until(id, deadline, validate),
                    None => self.with_session_lock(id, validate),
                }?;
            }
        }
        self.verify_directory_binding()?;
        Ok(ids)
    }

    pub fn list_result(&self) -> Result<Vec<String>, SessionError> {
        let deadline = self.lock_deadline();
        let list = || self.list_entries_result(MAX_LISTED_SESSIONS, None, true, deadline);
        match deadline {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, list),
            None => self.with_skill_usage_lock(list),
        }
    }

    pub(crate) fn list_for_skill_usage(
        &self,
        max_sessions: usize,
        max_total_bytes: u64,
    ) -> Result<Vec<String>, SessionError> {
        // This is a metadata preflight; the curator strictly loads every returned ID.
        self.list_entries_result(max_sessions, Some(max_total_bytes), false, None)
    }

    pub fn list(&self) -> Vec<String> {
        self.list_result()
            .unwrap_or_else(|error| panic!("failed to list sessions: {error}"))
    }

    pub fn record_tool_call(&self, record: ToolCallRecord) -> Result<(), SessionError> {
        let sid = record
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.update_or_create_session(&sid, |session| {
            session.tool_calls.push(record);
            Ok(())
        })
    }

    pub fn get_latest_id(&self) -> Option<String> {
        self.list().pop()
    }
}

pub(crate) fn open_session_directory(
    requested: &Path,
) -> Result<
    (
        PathBuf,
        crate::daemons::state::StableDirectory,
        crate::daemons::state::StableDirectory,
        File,
    ),
    String,
> {
    let sessions_dir = crate::fs_security::ensure_directory_without_symlinks(requested)
        .map_err(|error| error.to_string())?;
    let parent_path = sessions_dir.parent().ok_or_else(|| {
        format!(
            "session directory has no persistent identity parent: {}",
            sessions_dir.display()
        )
    })?;
    let parent = crate::daemons::state::StableDirectory::open(parent_path)?;
    let directory = parent.open_child(&sessions_dir)?;
    let identity_file = initialize_session_directory_identity(&directory, &parent, &sessions_dir)?;
    directory.recover_stale_temporary_files(
        ".nib-session-",
        MAX_SESSION_DIRECTORY_ENTRIES.saturating_mul(4),
        MAX_SESSION_DIRECTORY_NAME_BYTES.saturating_mul(4),
    )?;
    Ok((sessions_dir, directory, parent, identity_file))
}

pub(crate) fn open_existing_session_directory_until(
    requested: &Path,
    deadline: Instant,
) -> Result<
    (
        PathBuf,
        crate::daemons::state::StableDirectory,
        crate::daemons::state::StableDirectory,
        File,
    ),
    String,
> {
    ensure_session_store_open_deadline(deadline)?;
    crate::fs_security::verify_directory_without_symlinks(requested)
        .map_err(|error| error.to_string())?;
    ensure_session_store_open_deadline(deadline)?;
    let sessions_dir = requested
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let parent_path = sessions_dir.parent().ok_or_else(|| {
        format!(
            "session directory has no persistent identity parent: {}",
            sessions_dir.display()
        )
    })?;
    ensure_session_store_open_deadline(deadline)?;
    let parent = crate::daemons::state::StableDirectory::open(parent_path)?;
    ensure_session_store_open_deadline(deadline)?;
    let directory = parent.open_child(&sessions_dir)?;
    ensure_session_store_open_deadline(deadline)?;
    let visible = sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
    let anchor = session_directory_identity_anchor(&visible)?;
    if !directory.path_exists(&visible)? || !parent.path_exists(&anchor)? {
        return Err(format!(
            "existing session directory has incomplete persistent identity: {}",
            sessions_dir.display()
        ));
    }
    ensure_session_store_open_deadline(deadline)?;
    let identity_file = directory.open_read(&visible)?;
    parent.verify_file_identity(&anchor, &identity_file)?;
    directory.verify_visible()?;
    parent.verify_visible()?;
    ensure_session_store_open_deadline(deadline)?;
    Ok((sessions_dir, directory, parent, identity_file))
}

pub(crate) fn ensure_session_store_open_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("session store lock deadline elapsed".to_string());
    }
    Ok(())
}

pub(crate) fn initialize_session_directory_identity(
    directory: &crate::daemons::state::StableDirectory,
    parent: &crate::daemons::state::StableDirectory,
    sessions_dir: &Path,
) -> Result<File, String> {
    initialize_session_directory_identity_with_guard(
        directory,
        parent,
        sessions_dir,
        &mut || Ok(()),
    )
}

pub(crate) fn initialize_session_directory_identity_with_guard(
    directory: &crate::daemons::state::StableDirectory,
    parent: &crate::daemons::state::StableDirectory,
    sessions_dir: &Path,
    namespace_guard: &mut impl FnMut() -> Result<(), String>,
) -> Result<File, String> {
    namespace_guard()?;
    let visible = sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
    let anchor = session_directory_identity_anchor(&visible)?;
    namespace_guard()?;
    let visible_exists = directory.path_exists(&visible)?;
    namespace_guard()?;
    let anchor_exists = parent.path_exists(&anchor)?;
    namespace_guard()?;

    match (visible_exists, anchor_exists) {
        (false, false) => {
            drop(directory.open_read_write_create_with_guard(&visible, &mut *namespace_guard)?);
            namespace_guard()?;
            directory.hard_link_to_with_guard(&visible, parent, &anchor, &mut *namespace_guard)?;
            namespace_guard()?;
            directory.sync_directory()?;
            namespace_guard()?;
            parent.sync_directory()?;
            namespace_guard()?;
        }
        (true, false) => {
            directory.hard_link_to_with_guard(&visible, parent, &anchor, &mut *namespace_guard)?;
            namespace_guard()?;
            parent.sync_directory()?;
            namespace_guard()?;
        }
        (false, true) => {
            return Err(format!(
                "session directory identity marker is missing while its persistent anchor remains: {}",
                visible.display()
            ));
        }
        (true, true) => {}
    }

    namespace_guard()?;
    let identity_file = directory.open_read(&visible)?;
    namespace_guard()?;
    parent.verify_file_identity(&anchor, &identity_file)?;
    namespace_guard()?;
    directory.verify_visible()?;
    namespace_guard()?;
    parent.verify_visible()?;
    namespace_guard()?;
    Ok(identity_file)
}

pub(crate) fn initialize_preflighted_session_directory_identity_with_guard(
    directory: &crate::daemons::state::StableDirectory,
    parent: &crate::daemons::state::StableDirectory,
    sessions_dir: &Path,
    retained_identity_file: Option<&File>,
    planned_identity_bytes: Option<&[u8]>,
    namespace_guard: &mut impl FnMut() -> Result<(), String>,
) -> Result<File, String> {
    namespace_guard()?;
    let visible = sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
    let anchor = session_directory_identity_anchor(&visible)?;
    namespace_guard()?;
    let visible_exists = directory.path_exists(&visible)?;
    namespace_guard()?;
    let anchor_exists = parent.path_exists(&anchor)?;
    namespace_guard()?;

    match retained_identity_file {
        Some(retained) => {
            if !visible_exists {
                return Err(format!(
                    "preflighted session directory identity disappeared: {}",
                    visible.display()
                ));
            }
            directory.verify_file_identity(&visible, retained)?;
            namespace_guard()?;
            if anchor_exists {
                parent.verify_file_identity(&anchor, retained)?;
                namespace_guard()?;
            } else {
                directory.hard_link_to_with_guard(
                    &visible,
                    parent,
                    &anchor,
                    &mut *namespace_guard,
                )?;
                namespace_guard()?;
                parent.verify_file_identity(&anchor, retained)?;
                namespace_guard()?;
                parent.sync_directory()?;
                namespace_guard()?;
            }
        }
        None => {
            if visible_exists || anchor_exists {
                return Err(format!(
                    "session directory identity appeared after read-only preflight: {}",
                    sessions_dir.display()
                ));
            }
            let created =
                directory.open_read_write_create_new_with_guard(&visible, &mut *namespace_guard)?;
            namespace_guard()?;
            if let Some(bytes) = planned_identity_bytes {
                let mut writer = &created;
                writer.write_all(bytes).map_err(|error| error.to_string())?;
                namespace_guard()?;
                created.sync_all().map_err(|error| error.to_string())?;
                namespace_guard()?;
            }
            pause_session_namespace_preparation_phase("marker")?;
            directory.hard_link_to_with_guard(&visible, parent, &anchor, &mut *namespace_guard)?;
            pause_session_namespace_preparation_phase("anchor")?;
            namespace_guard()?;
            parent.verify_file_identity(&anchor, &created)?;
            namespace_guard()?;
            directory.sync_directory()?;
            pause_session_namespace_preparation_phase("sync")?;
            namespace_guard()?;
            parent.sync_directory()?;
            namespace_guard()?;
        }
    }

    namespace_guard()?;
    let identity_file = directory.open_read(&visible)?;
    namespace_guard()?;
    if let Some(retained) = retained_identity_file {
        directory.verify_file_identity(&visible, retained)?;
        let expected = crate::fs_security::file_identity_snapshot(retained)
            .map_err(|error| error.to_string())?;
        let observed = crate::fs_security::file_identity_snapshot(&identity_file)
            .map_err(|error| error.to_string())?;
        if observed != expected {
            return Err(format!(
                "preflighted session directory identity changed before initialization: {}",
                sessions_dir.display()
            ));
        }
    }
    parent.verify_file_identity(&anchor, &identity_file)?;
    namespace_guard()?;
    directory.verify_visible()?;
    namespace_guard()?;
    parent.verify_visible()?;
    namespace_guard()?;
    pause_session_namespace_preparation_phase("final")?;
    Ok(identity_file)
}
