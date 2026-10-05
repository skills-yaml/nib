//! T043 split.

use super::*;

#[cfg(debug_assertions)]
pub(crate) fn pause_session_namespace_preparation_phase(phase: &str) -> Result<(), String> {
    if std::env::var("NIB_TEST_SESSION_PREPARATION_PHASE").as_deref() != Ok(phase) {
        return Ok(());
    }
    let ready = std::env::var_os("NIB_TEST_SESSION_PREPARATION_READY")
        .map(PathBuf::from)
        .ok_or_else(|| "missing session preparation readiness path".to_string())?;
    std::fs::write(&ready, phase.as_bytes())
        .map_err(|error| format!("publish session preparation phase: {error}"))?;
    let resume = std::env::var_os("NIB_TEST_SESSION_PREPARATION_RESUME")
        .map(PathBuf::from)
        .ok_or_else(|| "missing session preparation resume path".to_string())?;
    let started = Instant::now();
    while !resume.exists() {
        if started.elapsed() >= Duration::from_secs(30) {
            return Err(format!("timed out at session preparation phase {phase}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
pub(crate) fn pause_session_namespace_preparation_phase(_phase: &str) -> Result<(), String> {
    Ok(())
}

pub(crate) struct SessionDirectoryPreflight {
    pub(crate) sessions_dir: PathBuf,
    pub(crate) parent_path: PathBuf,
    pub(crate) retained_ancestor: crate::daemons::state::StableDirectory,
    pub(crate) retained_directory: Option<crate::daemons::state::StableDirectory>,
    pub(crate) retained_identity_file: Option<File>,
    pub(crate) sensitive_values: Vec<String>,
    pub(crate) runtime_config: crate::config::NibConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionNamespacePreparationPlan {
    pub(crate) version: u32,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) retained_ancestor: PathBuf,
    pub(crate) retained_ancestor_identity: crate::fs_security::DirectoryIdentity,
    pub(crate) proven_missing_directories: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) retained_identity: Option<crate::fs_security::FileIdentitySnapshot>,
    pub(crate) retained_anchor_present: bool,
    pub(crate) identity_marker_bytes: Vec<u8>,
}

pub(crate) struct CreatedSessionDirectoryTree {
    pub(crate) parent: crate::daemons::state::StableDirectory,
    pub(crate) path: PathBuf,
    pub(crate) directory: crate::daemons::state::StableDirectory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CreatedSessionDirectoryReceipt {
    pub(crate) parent: PathBuf,
    pub(crate) path: PathBuf,
    pub(crate) identity: crate::fs_security::DirectoryIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SessionPreparationReceipt {
    pub(crate) version: u32,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) sessions_directory_identity: crate::fs_security::DirectoryIdentity,
    pub(crate) directory_identity: crate::fs_security::FileIdentitySnapshot,
    pub(crate) planned_session: Session,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_identity: Option<crate::fs_security::FileIdentitySnapshot>,
    pub(crate) created_identity: bool,
    pub(crate) created_directories: Vec<CreatedSessionDirectoryReceipt>,
}

impl SessionPreparationReceipt {
    pub(crate) fn audit_directory_identity(&self) -> crate::fs_security::FileIdentitySnapshot {
        self.directory_identity
    }

    pub(crate) fn is_exact_publication_successor(&self, previous: &Self) -> bool {
        self.version == previous.version
            && self.sessions_dir == previous.sessions_dir
            && self.session_id == previous.session_id
            && self.sessions_directory_identity == previous.sessions_directory_identity
            && self.directory_identity == previous.directory_identity
            && self.planned_session == previous.planned_session
            && previous.session_identity.is_none()
            && self.session_identity.is_some()
            && self.created_identity == previous.created_identity
            && self.created_directories == previous.created_directories
    }
}

pub(crate) struct SessionStorePreparation {
    pub(crate) store: Option<SessionStore>,
    pub(crate) created_tree: Vec<CreatedSessionDirectoryTree>,
    pub(crate) created_identity_file: Option<File>,
    pub(crate) created_session_file: Option<(PathBuf, File)>,
    pub(crate) planned_session: Option<Session>,
    pub(crate) parent_directory: Option<crate::daemons::state::StableDirectory>,
    pub(crate) directory: Option<crate::daemons::state::StableDirectory>,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) namespace_lock: Arc<SessionMutex>,
    pub(crate) armed: bool,
}

impl SessionStorePreparation {
    pub(crate) fn store(&self) -> &SessionStore {
        self.store
            .as_ref()
            .expect("prepared session store is present")
    }

    #[cfg(test)]
    pub(crate) fn create_unpublished_session(&mut self, id: &str) -> Result<(), String> {
        self.create_unpublished_session_with_guard(id, || Ok(()))
    }

    pub(crate) fn create_unpublished_session_with_guard(
        &mut self,
        id: &str,
        mut external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let deadline = self.store().lock_deadline().ok_or_else(|| {
            "unpublished session preparation requires an absolute deadline".to_string()
        })?;
        external_guard()?;
        let namespace_lock = self.namespace_lock.clone();
        let _namespace_guard =
            lock_session_mutex(&namespace_lock, &self.sessions_dir, Some(deadline))
                .map_err(|error| error.to_string())?;
        if self.planned_session.is_none() {
            self.plan_unpublished_session(id)?;
        }
        let session = self
            .planned_session
            .as_ref()
            .ok_or_else(|| "planned session is missing".to_string())?;
        if session.id != id {
            return Err("planned session id changed before publication".to_string());
        }
        let receipt = self
            .store()
            .create_unpublished_session_with_receipt_and_guard(session, &mut external_guard)
            .map_err(|error| error.to_string())?;
        external_guard()?;
        self.created_session_file = Some((self.sessions_dir.join(format!("{id}.json")), receipt));
        Ok(())
    }

    pub(crate) fn plan_unpublished_session(&mut self, id: &str) -> Result<(), String> {
        self.store()
            .validate_session_id(id)
            .map_err(|error| error.to_string())?;
        if let Some(planned) = &self.planned_session {
            return (planned.id == id)
                .then_some(())
                .ok_or_else(|| "session preparation already planned another id".to_string());
        }
        self.planned_session = Some(Session::new(id.to_string()));
        Ok(())
    }

    pub(crate) fn durable_receipt(
        &self,
        session_id: &str,
    ) -> Result<SessionPreparationReceipt, String> {
        let directory_identity = self
            .store()
            .persistent_directory_identity()
            .map_err(|error| error.to_string())?;
        let sessions_directory_identity = self
            .directory
            .as_ref()
            .ok_or_else(|| "prepared session directory capability is missing".to_string())?
            .directory_removal_receipt()?
            .identity();
        let planned_session = self
            .planned_session
            .clone()
            .ok_or_else(|| "planned session receipt is missing".to_string())?;
        let session_identity = self
            .created_session_file
            .as_ref()
            .map(|(_, session_file)| crate::fs_security::file_identity_snapshot(session_file))
            .transpose()
            .map_err(|error| error.to_string())?;
        let created_directories = self
            .created_tree
            .iter()
            .map(|created| {
                Ok(CreatedSessionDirectoryReceipt {
                    parent: created.parent.path().to_path_buf(),
                    path: created.path.clone(),
                    identity: created.directory.directory_removal_receipt()?.identity(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(SessionPreparationReceipt {
            version: 1,
            sessions_dir: self.sessions_dir.clone(),
            session_id: session_id.to_string(),
            sessions_directory_identity,
            directory_identity,
            planned_session,
            session_identity,
            created_identity: self.created_identity_file.is_some(),
            created_directories,
        })
    }

    pub(crate) fn disarm(mut self) -> SessionStore {
        self.armed = false;
        self.store
            .take()
            .expect("prepared session store is present")
    }

    pub(crate) fn cleanup(mut self, deadline: Instant) -> Result<(), String> {
        self.cleanup_inner(deadline, &mut || Ok(()))
    }

    /// Attempt the exact cleanup once under a caller-owned durable recovery
    /// authority.  If that bounded attempt fails, ownership has already been
    /// handed to the durable receipt, so `Drop` must not renew the deadline and
    /// race restart reconciliation with an unrecorded second attempt.
    pub(crate) fn cleanup_with_guard_preserving_failure(
        mut self,
        deadline: Instant,
        mut external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let result = self.cleanup_inner(deadline, &mut external_guard);
        if result.is_err() {
            self.armed = false;
        }
        result
    }

    /// Leave every remaining exact artifact to a previously persisted durable
    /// preparation receipt instead of performing best-effort cleanup in Drop.
    #[cfg(test)]
    pub(crate) fn preserve_for_durable_reconciliation(mut self) {
        self.armed = false;
    }

    #[cfg(test)]
    pub(crate) fn cleanup_durable(
        receipt: &SessionPreparationReceipt,
        deadline: Instant,
    ) -> Result<(), String> {
        Self::cleanup_durable_with_guard(receipt, deadline, || Ok(()))
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn cleanup_durable_with_guard(
        receipt: &SessionPreparationReceipt,
        deadline: Instant,
        mut external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if receipt.version != 1 {
            return Err("unsupported session preparation receipt version".to_string());
        }
        ensure_session_store_open_deadline(deadline)?;
        external_guard()?;
        let namespace_lock = session_preparation_mutex(&receipt.sessions_dir, Some(deadline))?;
        let _namespace_guard =
            lock_session_mutex(&namespace_lock, &receipt.sessions_dir, Some(deadline))
                .map_err(|error| error.to_string())?;
        let metadata = match std::fs::symlink_metadata(&receipt.sessions_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(format!(
                "prepared session directory changed and was preserved: {}",
                receipt.sessions_dir.display()
            ));
        }
        validate_session_id(&receipt.session_id).map_err(|error| error.to_string())?;
        if receipt.planned_session.id != receipt.session_id {
            return Err("prepared session plan does not match its durable session id".to_string());
        }
        let parent_path = receipt.sessions_dir.parent().ok_or_else(|| {
            "prepared session directory has no persistent identity parent".to_string()
        })?;
        let parent = crate::daemons::state::StableDirectory::open(parent_path)
            .map_err(|error| format!("open prepared session parent: {error}"))?;
        let directory = parent
            .open_owned_child(&receipt.sessions_dir)
            .map_err(|error| format!("open prepared sessions directory: {error}"))?;
        if directory
            .directory_removal_receipt()
            .map_err(|error| format!("identify prepared sessions directory: {error}"))?
            .identity()
            != receipt.sessions_directory_identity
        {
            return Err(format!(
                "prepared session directory changed identity and was preserved: {}",
                receipt.sessions_dir.display()
            ));
        }
        let session_path = receipt
            .sessions_dir
            .join(format!("{}.json", receipt.session_id));
        let mut guard = || {
            external_guard()?;
            ensure_session_store_open_deadline(deadline)
        };
        let expected_session_bytes = serde_json::to_vec_pretty(&receipt.planned_session)
            .map_err(|error| error.to_string())?;
        // Resolve the exact missing-only atomic transaction before deciding
        // whether the canonical leaf exists. This prevents a killed writer's
        // temporary artifact from being ignored while marker/ancestor cleanup
        // proceeds.
        directory.recover_exact_missing_publication_with_guard(
            &session_path,
            ".nib-session-",
            &expected_session_bytes,
            &mut guard,
        )?;
        let deletion_quarantine = directory.deterministic_artifact_path(
            &session_path,
            ".nib-session-preparation-delete-",
            ".quarantine",
        )?;
        let canonical_exists = directory.path_exists(&session_path)?;
        let quarantine_exists = directory.path_exists(&deletion_quarantine)?;
        if canonical_exists && quarantine_exists {
            return Err(format!(
                "prepared session leaf and deletion quarantine are ambiguous and were preserved: {}",
                session_path.display()
            ));
        }
        if quarantine_exists {
            let quarantined = directory.open_read_write(&deletion_quarantine)?;
            if let Some(expected) = &receipt.session_identity {
                let observed = crate::fs_security::file_identity_snapshot(&quarantined)
                    .map_err(|error| error.to_string())?;
                if &observed != expected {
                    return Err(format!(
                        "prepared session deletion quarantine changed identity and was preserved: {}",
                        deletion_quarantine.display()
                    ));
                }
            }
            let observed = {
                let mut reader = (&quarantined).take(MAX_SESSION_JSON_BYTES + 1);
                let mut bytes = Vec::new();
                reader
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                bytes
            };
            if observed != expected_session_bytes {
                return Err(format!(
                    "prepared session deletion quarantine bytes changed and were preserved: {}",
                    deletion_quarantine.display()
                ));
            }
            directory.remove_visible_file_if_matches_direct_with_guard(
                &deletion_quarantine,
                &quarantined,
                &mut guard,
            )?;
        }
        if directory
            .path_exists(&session_path)
            .map_err(|error| format!("inspect prepared session leaf: {error}"))?
        {
            let session_file = directory
                .open_read_write(&session_path)
                .map_err(|error| format!("open prepared session leaf: {error}"))?;
            let observed = crate::fs_security::file_identity_snapshot(&session_file)
                .map_err(|error| error.to_string())?;
            match &receipt.session_identity {
                Some(expected) if &observed != expected => {
                    return Err(format!(
                        "prepared session changed identity and was preserved: {}",
                        session_path.display()
                    ));
                }
                None => {
                    if receipt.planned_session.id != receipt.session_id {
                        return Err(
                            "prepared session plan does not match its durable session id"
                                .to_string(),
                        );
                    }
                    let metadata = session_file.metadata().map_err(|error| error.to_string())?;
                    if metadata.len() > MAX_SESSION_JSON_BYTES {
                        return Err(format!(
                            "prepared session exceeds the bounded read limit and was preserved: {}",
                            session_path.display()
                        ));
                    }
                    let mut bytes = Vec::with_capacity(metadata.len() as usize);
                    let mut reader = (&session_file).take(MAX_SESSION_JSON_BYTES + 1);
                    reader
                        .read_to_end(&mut bytes)
                        .map_err(|error| error.to_string())?;
                    guard()?;
                    directory
                        .verify_file_identity(&session_path, &session_file)
                        .map_err(|error| {
                            format!("verify prepared session leaf after read: {error}")
                        })?;
                    if bytes != expected_session_bytes {
                        return Err(format!(
                            "prepared session content changed and was preserved: {}",
                            session_path.display()
                        ));
                    }
                }
                Some(_) => {}
            }
            directory.remove_file_if_matches_with_guard(
                &session_path,
                &session_file,
                ".nib-session-preparation-delete-",
                &mut guard,
            )?;
        }
        if !receipt.created_identity {
            return guard();
        }
        let mut has_shared_entries = false;
        directory
            .for_each_entry_bounded(1_024, 255, |name| {
                if name != std::ffi::OsStr::new(SESSION_DIRECTORY_IDENTITY_FILE) {
                    has_shared_entries = true;
                }
                Ok(())
            })
            .map_err(|error| format!("scan prepared session namespace: {error}"))?;
        if has_shared_entries {
            return guard();
        }
        let visible = receipt.sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
        let anchor = session_directory_identity_anchor(&visible)?;
        let visible_file = directory
            .path_exists(&visible)
            .map_err(|error| format!("inspect prepared session marker: {error}"))?
            .then(|| directory.open_read_write(&visible))
            .transpose()
            .map_err(|error| format!("open prepared session marker: {error}"))?;
        let anchor_file = parent
            .path_exists(&anchor)
            .map_err(|error| format!("inspect prepared session anchor: {error}"))?
            .then(|| parent.open_read_write(&anchor))
            .transpose()
            .map_err(|error| format!("open prepared session anchor: {error}"))?;
        let identity_file = visible_file.as_ref().or(anchor_file.as_ref());
        for candidate in [visible_file.as_ref(), anchor_file.as_ref()]
            .into_iter()
            .flatten()
        {
            let observed = crate::fs_security::file_identity_snapshot(candidate)
                .map_err(|error| error.to_string())?;
            if observed != receipt.directory_identity {
                return Err(format!(
                    "prepared session marker changed identity and was preserved: {}",
                    visible.display()
                ));
            }
        }
        if parent.path_exists(&anchor)? {
            let identity_file = identity_file.ok_or_else(|| {
                "prepared session anchor has no retained identity authority".to_string()
            })?;
            parent.remove_file_if_matches_with_guard(
                &anchor,
                identity_file,
                ".nib-session-preparation-anchor-delete-",
                &mut guard,
            )?;
        }
        if directory.path_exists(&visible)? {
            let identity_file = identity_file.ok_or_else(|| {
                "prepared session marker has no retained identity authority".to_string()
            })?;
            directory.remove_file_if_matches_with_guard(
                &visible,
                identity_file,
                ".nib-session-preparation-marker-delete-",
                &mut guard,
            )?;
        }
        drop(directory);
        drop(parent);
        for created in receipt.created_directories.iter().rev() {
            guard()?;
            if created.path.parent() != Some(created.parent.as_path()) {
                return Err("prepared session directory receipt is not a direct child".to_string());
            }
            let parent = match crate::daemons::state::StableDirectory::open(&created.parent) {
                Ok(parent) => parent,
                Err(_error) if !created.parent.exists() => continue,
                Err(error) => return Err(error),
            };
            match parent.entry_kind(&created.path)? {
                None => continue,
                Some(crate::daemons::state::StableEntryKind::Directory) => {}
                Some(crate::daemons::state::StableEntryKind::File) => {
                    return Err(format!(
                        "prepared session directory was replaced by a file and was preserved: {}",
                        created.path.display()
                    ));
                }
            }
            let child = parent.open_owned_child(&created.path)?;
            if child.directory_removal_receipt()?.identity() != created.identity {
                return Err(format!(
                    "prepared session directory changed identity and was preserved: {}",
                    created.path.display()
                ));
            }
            let mut nonempty = false;
            child.for_each_entry_bounded(1_024, 255, |_| {
                nonempty = true;
                Ok(())
            })?;
            if nonempty {
                break;
            }
            parent.remove_empty_child_directory_if_matches_with_guard(
                &created.path,
                child,
                &mut guard,
            )?;
        }
        guard()
    }

    #[cfg(test)]
    pub(crate) fn cleanup_planned_namespace(
        plan: &SessionNamespacePreparationPlan,
        deadline: Instant,
    ) -> Result<(), String> {
        Self::cleanup_planned_namespace_with_guard(plan, deadline, || Ok(()))
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn cleanup_planned_namespace_with_guard(
        plan: &SessionNamespacePreparationPlan,
        deadline: Instant,
        mut external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if plan.version != 1 {
            return Err("unsupported session namespace preparation plan version".to_string());
        }
        ensure_session_store_open_deadline(deadline)?;
        external_guard()?;
        let namespace_lock = session_preparation_mutex(&plan.sessions_dir, Some(deadline))?;
        let _namespace_guard =
            lock_session_mutex(&namespace_lock, &plan.sessions_dir, Some(deadline))
                .map_err(|error| error.to_string())?;
        let mut guard = || {
            external_guard()?;
            ensure_session_store_open_deadline(deadline)
        };
        let retained = crate::daemons::state::StableDirectory::open(&plan.retained_ancestor)?;
        if retained.directory_removal_receipt()?.identity() != plan.retained_ancestor_identity {
            return Err(format!(
                "prepared session ancestor changed identity and was preserved: {}",
                plan.retained_ancestor.display()
            ));
        }
        guard()?;
        let relative = plan
            .sessions_dir
            .strip_prefix(&plan.retained_ancestor)
            .map_err(|_| "planned session directory escaped its retained ancestor".to_string())?;
        let mut current = retained;
        let mut traversed = Vec::new();
        let mut sessions = None;
        let mut sessions_parent = None;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err("planned session directory contains an unsafe component".to_string());
            };
            let child_path = current.path().join(name);
            guard()?;
            match current.entry_kind(&child_path)? {
                None => break,
                Some(crate::daemons::state::StableEntryKind::File) => {
                    return Err(format!(
                        "planned session directory was replaced by a file and was preserved: {}",
                        child_path.display()
                    ));
                }
                Some(crate::daemons::state::StableEntryKind::Directory) => {}
            }
            let child = current.open_owned_child(&child_path)?;
            let created = plan.proven_missing_directories.contains(&child_path);
            if created {
                traversed.push((
                    current.try_clone()?,
                    child_path.clone(),
                    child.directory_removal_receipt()?.identity(),
                ));
            }
            if child_path == plan.sessions_dir {
                sessions_parent = Some(current.try_clone()?);
                sessions = Some(child);
                break;
            }
            current = child;
        }

        let visible_path = plan.sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
        let anchor_path = session_directory_identity_anchor(&visible_path)?;
        let mut visible_file = None;
        let mut anchor_file = None;
        if let (Some(directory), Some(parent)) = (sessions.as_ref(), sessions_parent.as_ref()) {
            guard()?;
            if directory.path_exists(&visible_path)? {
                visible_file = Some(directory.open_read_write(&visible_path)?);
            }
            guard()?;
            if parent.path_exists(&anchor_path)? {
                anchor_file = Some(parent.open_read_write(&anchor_path)?);
            }
            for file in [visible_file.as_ref(), anchor_file.as_ref()]
                .into_iter()
                .flatten()
            {
                let observed = crate::fs_security::file_identity_snapshot(file)
                    .map_err(|error| error.to_string())?;
                if let Some(retained) = &plan.retained_identity {
                    if &observed != retained {
                        return Err(format!(
                            "retained session identity changed and was preserved: {}",
                            visible_path.display()
                        ));
                    }
                } else {
                    let metadata = file.metadata().map_err(|error| error.to_string())?;
                    if metadata.len() != plan.identity_marker_bytes.len() as u64 {
                        return Err(format!(
                            "prepared session identity marker is ambiguous and was preserved: {}",
                            visible_path.display()
                        ));
                    }
                    let mut bytes = Vec::with_capacity(plan.identity_marker_bytes.len());
                    file.take(plan.identity_marker_bytes.len() as u64 + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|error| error.to_string())?;
                    if bytes != plan.identity_marker_bytes {
                        return Err(format!(
                            "prepared session identity marker changed and was preserved: {}",
                            visible_path.display()
                        ));
                    }
                }
            }
            if let (Some(visible), Some(anchor)) = (&visible_file, &anchor_file) {
                let visible_identity = crate::fs_security::file_identity_snapshot(visible)
                    .map_err(|error| error.to_string())?;
                let anchor_identity = crate::fs_security::file_identity_snapshot(anchor)
                    .map_err(|error| error.to_string())?;
                if visible_identity != anchor_identity {
                    return Err(format!(
                        "prepared session identity pair is ambiguous and was preserved: {}",
                        visible_path.display()
                    ));
                }
            }
        }

        // A directory this transaction proved missing is owned only while its
        // complete namespace contains the planned next component and marker.
        // Inspect the entire planned chain before the first removal so an
        // unrelated/adopted entry makes compensation fail closed.
        for (index, (_, path, _)) in traversed.iter().enumerate() {
            let directory = crate::daemons::state::StableDirectory::open(path)?;
            let next = traversed.get(index + 1).map(|(_, path, _)| path);
            let mut unexpected = None;
            directory.for_each_entry_bounded(1_024, 255, |name| {
                let entry_path = path.join(&name);
                let allowed_next = next.is_some_and(|next| next == &entry_path);
                let allowed_visible = entry_path == visible_path;
                let allowed_anchor = entry_path == anchor_path;
                if !allowed_next && !allowed_visible && !allowed_anchor {
                    unexpected = Some(entry_path);
                }
                Ok(())
            })?;
            if let Some(unexpected) = unexpected {
                return Err(format!(
                    "prepared session namespace contains an unrelated entry and was preserved: {}",
                    unexpected.display()
                ));
            }
        }
        // The anchor is a sibling of the sessions directory and therefore is
        // intentionally outside the sessions-directory scan above.
        if !plan.retained_anchor_present {
            if let (Some(parent), Some(file)) = (sessions_parent.as_ref(), anchor_file.as_ref()) {
                guard()?;
                parent.remove_file_if_matches_with_guard(
                    &anchor_path,
                    file,
                    ".nib-session-preparation-anchor-delete-",
                    &mut guard,
                )?;
            }
        }
        if plan.retained_identity.is_none() {
            if let (Some(directory), Some(file)) = (sessions.as_ref(), visible_file.as_ref()) {
                guard()?;
                directory.remove_file_if_matches_with_guard(
                    &visible_path,
                    file,
                    ".nib-session-preparation-marker-delete-",
                    &mut guard,
                )?;
            }
        }
        drop(sessions);
        drop(sessions_parent);
        for (parent, path, identity) in traversed.into_iter().rev() {
            guard()?;
            let child = match parent.entry_kind(&path)? {
                None => continue,
                Some(crate::daemons::state::StableEntryKind::Directory) => {
                    parent.open_owned_child(&path)?
                }
                Some(crate::daemons::state::StableEntryKind::File) => {
                    return Err(format!(
                        "prepared session directory changed type and was preserved: {}",
                        path.display()
                    ));
                }
            };
            if child.directory_removal_receipt()?.identity() != identity {
                return Err(format!(
                    "prepared session directory changed identity and was preserved: {}",
                    path.display()
                ));
            }
            let mut nonempty = false;
            child.for_each_entry_bounded(1_024, 255, |_| {
                nonempty = true;
                Ok(())
            })?;
            if nonempty {
                return Err(format!(
                    "prepared session directory was adopted and was preserved: {}",
                    path.display()
                ));
            }
            parent.remove_empty_child_directory_if_matches_with_guard(&path, child, &mut guard)?;
        }
        guard()
    }

    pub(crate) fn cleanup_inner(
        &mut self,
        deadline: Instant,
        external_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.armed {
            return Ok(());
        }
        external_guard()?;
        let namespace_lock = self.namespace_lock.clone();
        let _namespace_guard =
            lock_session_mutex(&namespace_lock, &self.sessions_dir, Some(deadline))
                .map_err(|error| error.to_string())?;
        drop(self.store.take());
        if self.created_tree.is_empty()
            && self.created_identity_file.is_none()
            && self.created_session_file.is_none()
        {
            self.armed = false;
            return Ok(());
        }
        let directory = self
            .directory
            .as_ref()
            .ok_or_else(|| "prepared session directory capability is missing".to_string())?;
        let parent = self
            .parent_directory
            .as_ref()
            .ok_or_else(|| "prepared session parent capability is missing".to_string())?;
        let mut guard = || {
            external_guard()?;
            ensure_session_store_open_deadline(deadline)
        };
        if let Some((session_path, session_file)) = self.created_session_file.as_ref() {
            directory.remove_file_if_matches_with_guard(
                session_path,
                session_file,
                ".nib-session-preparation-delete-",
                &mut guard,
            )?;
            self.created_session_file.take();
        }
        if let Some(identity_file) = self.created_identity_file.as_ref() {
            let mut has_shared_entries = false;
            directory.for_each_entry_bounded(1_024, 255, |name| {
                if name != std::ffi::OsStr::new(SESSION_DIRECTORY_IDENTITY_FILE) {
                    has_shared_entries = true;
                }
                Ok(())
            })?;
            if has_shared_entries {
                // Another spawn adopted this prepared namespace. The marker
                // and its ancestor directories are now shared infrastructure,
                // so this transaction owns only its exact session leaf.
                self.created_identity_file.take();
                self.created_tree.clear();
                guard()?;
                self.armed = false;
                return Ok(());
            }
            let visible = self.sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
            let anchor = session_directory_identity_anchor(&visible)?;
            if parent.path_exists(&anchor)? {
                parent.remove_file_if_matches_with_guard(
                    &anchor,
                    identity_file,
                    ".nib-session-preparation-anchor-delete-",
                    &mut guard,
                )?;
            }
            if directory.path_exists(&visible)? {
                directory.remove_file_if_matches_with_guard(
                    &visible,
                    identity_file,
                    ".nib-session-preparation-marker-delete-",
                    &mut guard,
                )?;
            }
            self.created_identity_file.take();
        }
        self.directory.take();
        self.parent_directory.take();
        while let Some(tree) = self.created_tree.pop() {
            guard()?;
            let mut nonempty = false;
            tree.directory.for_each_entry_bounded(1_024, 255, |_| {
                nonempty = true;
                Ok(())
            })?;
            if nonempty {
                // A concurrently committed session/profile adopted this
                // ancestor. Preserve it and every higher ancestor.
                self.created_tree.clear();
                break;
            }
            tree.parent
                .remove_empty_child_directory_if_matches_with_guard(
                    &tree.path,
                    tree.directory,
                    &mut guard,
                )?;
        }
        guard()?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for SessionStorePreparation {
    fn drop(&mut self) {
        if self.armed {
            let deadline = Instant::now()
                .checked_add(Duration::from_secs(5))
                .unwrap_or_else(Instant::now);
            let _ = self.cleanup_inner(deadline, &mut || Ok(()));
        }
    }
}

impl SessionDirectoryPreflight {
    #[cfg(test)]
    pub(crate) fn durable_preparation_plan_after_owned_worktree(
        &self,
        transaction_id: &str,
        deadline: Instant,
        worktree: Option<&crate::sandbox::worktree::Worktree>,
    ) -> Result<SessionNamespacePreparationPlan, String> {
        self.durable_preparation_plan_with_authority(transaction_id, deadline, worktree, None)
    }

    pub(crate) fn durable_preparation_plan_after_authorized_records(
        &self,
        transaction_id: &str,
        deadline: Instant,
        records: &crate::daemons::state::StableDirectory,
    ) -> Result<SessionNamespacePreparationPlan, String> {
        records.verify_visible()?;
        self.durable_preparation_plan_with_authority(transaction_id, deadline, None, Some(records))
    }

    pub(crate) fn durable_preparation_plan_with_authority(
        &self,
        transaction_id: &str,
        deadline: Instant,
        worktree: Option<&crate::sandbox::worktree::Worktree>,
        records: Option<&crate::daemons::state::StableDirectory>,
    ) -> Result<SessionNamespacePreparationPlan, String> {
        ensure_session_store_open_deadline(deadline)?;
        self.retained_ancestor.verify_visible()?;
        if let Some(worktree) = worktree {
            worktree.verify_owned_namespace()?;
        }
        let retained_ancestor_identity = self
            .retained_ancestor
            .directory_removal_receipt()?
            .identity();
        let mut proven_missing_directories = Vec::new();
        let relative = self
            .sessions_dir
            .strip_prefix(self.retained_ancestor.path())
            .map_err(|_| {
                "preflighted session directory escaped its retained ancestor".to_string()
            })?;
        let mut current = self.retained_ancestor.try_clone()?;
        let mut planned_path = self.retained_ancestor.path().to_path_buf();
        let mut missing = false;
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(
                    "preflighted session directory contains an unsafe component".to_string()
                );
            };
            planned_path.push(name);
            let path = planned_path.clone();
            if missing {
                proven_missing_directories.push(path);
                continue;
            }
            match current.entry_kind(&path)? {
                None => {
                    missing = true;
                    proven_missing_directories.push(path);
                }
                Some(crate::daemons::state::StableEntryKind::Directory) => {
                    let appeared_with_worktree =
                        worktree.is_some_and(|worktree| worktree.path.starts_with(&path));
                    let appeared_with_records =
                        records.is_some_and(|records| records.path().starts_with(&path));
                    let originally_retained = self
                        .retained_directory
                        .as_ref()
                        .is_some_and(|directory| directory.path() == path);
                    if !appeared_with_worktree && !appeared_with_records && !originally_retained {
                        return Err(format!(
                            "state directory appeared after its absence was proven: {}",
                            path.display()
                        ));
                    }
                    current = current.open_owned_child(&path)?;
                }
                Some(crate::daemons::state::StableEntryKind::File) => {
                    return Err(format!(
                        "future session directory component is not a directory: {}",
                        path.display()
                    ));
                }
            }
        }
        let retained_identity = self
            .retained_identity_file
            .as_ref()
            .map(crate::fs_security::file_identity_snapshot)
            .transpose()
            .map_err(|error| error.to_string())?;
        let retained_anchor_present = if retained_identity.is_some() {
            let visible = self.sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
            self.retained_ancestor
                .path_exists(&session_directory_identity_anchor(&visible)?)?
        } else {
            false
        };
        ensure_session_store_open_deadline(deadline)?;
        Ok(SessionNamespacePreparationPlan {
            version: 1,
            sessions_dir: self.sessions_dir.clone(),
            retained_ancestor: self.retained_ancestor.path().to_path_buf(),
            retained_ancestor_identity,
            proven_missing_directories,
            retained_identity,
            retained_anchor_present,
            identity_marker_bytes: format!("nib-session-preparation-v1:{transaction_id}\n")
                .into_bytes(),
        })
    }

    pub(crate) fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub(crate) fn runtime_config(&self) -> &crate::config::NibConfig {
        &self.runtime_config
    }

    pub(crate) fn verify_continuity(&self, deadline: Instant) -> Result<(), String> {
        ensure_session_store_open_deadline(deadline)?;
        self.retained_ancestor.verify_visible()?;
        ensure_session_store_open_deadline(deadline)?;
        if self.retained_ancestor.path() != self.parent_path {
            let relative = self
                .parent_path
                .strip_prefix(self.retained_ancestor.path())
                .map_err(|_| {
                    "preflighted session parent escaped its retained ancestor".to_string()
                })?;
            let first = relative.components().next().ok_or_else(|| {
                "preflighted session parent has no retained descendant".to_string()
            })?;
            let std::path::Component::Normal(first) = first else {
                return Err("preflighted session parent has an unsafe component".to_string());
            };
            let path = self.retained_ancestor.path().join(first);
            if self.retained_ancestor.entry_kind(&path)?.is_some() {
                return Err(format!(
                    "state directory appeared after its absence was proven: {}",
                    path.display()
                ));
            }
        } else {
            match &self.retained_directory {
                Some(directory) => {
                    directory.verify_visible()?;
                    if let Some(identity) = &self.retained_identity_file {
                        directory.verify_file_identity(
                            &self.sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE),
                            identity,
                        )?;
                    }
                }
                None => {
                    if self
                        .retained_ancestor
                        .entry_kind(&self.sessions_dir)?
                        .is_some()
                    {
                        return Err(format!(
                            "session directory appeared after read-only preflight: {}",
                            self.sessions_dir.display()
                        ));
                    }
                }
            }
        }
        ensure_session_store_open_deadline(deadline)
    }

    #[cfg(test)]
    pub(crate) fn open_until(self, deadline: Instant) -> Result<SessionStorePreparation, String> {
        self.open_until_with_owned_worktree(deadline, None, None, &mut || Ok(()))
    }

    pub(crate) fn open_until_with_guard(
        self,
        deadline: Instant,
        mut external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<SessionStorePreparation, String> {
        self.open_until_with_owned_worktree(deadline, None, None, &mut external_guard)
    }

    pub(crate) fn open_until_after_owned_worktree_with_guard(
        self,
        deadline: Instant,
        worktree: &crate::sandbox::worktree::Worktree,
        durable_plan: Option<&SessionNamespacePreparationPlan>,
        mut external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<SessionStorePreparation, String> {
        self.open_until_with_owned_worktree(
            deadline,
            Some(worktree),
            durable_plan,
            &mut external_guard,
        )
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn open_until_with_owned_worktree(
        self,
        deadline: Instant,
        worktree: Option<&crate::sandbox::worktree::Worktree>,
        durable_plan: Option<&SessionNamespacePreparationPlan>,
        external_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<SessionStorePreparation, String> {
        external_guard()?;
        ensure_session_store_open_deadline(deadline)?;
        let SessionDirectoryPreflight {
            sessions_dir,
            parent_path,
            retained_ancestor,
            retained_directory,
            retained_identity_file,
            sensitive_values,
            runtime_config: _,
        } = self;
        let namespace_lock = session_preparation_mutex(&sessions_dir, Some(deadline))?;
        let namespace_guard = lock_session_mutex(&namespace_lock, &sessions_dir, Some(deadline))
            .map_err(|error| error.to_string())?;
        if let Some(plan) = durable_plan {
            if plan.version != 1
                || plan.sessions_dir != sessions_dir
                || plan.retained_ancestor != retained_ancestor.path()
                || plan.retained_ancestor_identity
                    != retained_ancestor.directory_removal_receipt()?.identity()
            {
                return Err(
                    "durable session namespace plan changed before initialization".to_string(),
                );
            }
        }
        let mut preparation = SessionStorePreparation {
            store: None,
            created_tree: Vec::new(),
            created_identity_file: None,
            created_session_file: None,
            planned_session: None,
            parent_directory: None,
            directory: None,
            sessions_dir: sessions_dir.clone(),
            namespace_lock: namespace_lock.clone(),
            armed: true,
        };
        let outcome = (|| {
            external_guard()?;
            retained_ancestor.verify_visible()?;
            ensure_session_store_open_deadline(deadline)?;

            let mut parent = retained_ancestor;
            if parent.path() != parent_path {
                let relative = parent_path.strip_prefix(parent.path()).map_err(|_| {
                    format!(
                        "preflighted session parent is not below its retained ancestor {}: {}",
                        parent.path().display(),
                        parent_path.display()
                    )
                })?;
                for component in relative.components() {
                    let std::path::Component::Normal(name) = component else {
                        return Err(format!(
                            "preflighted session parent contains an unsafe component: {}",
                            parent_path.display()
                        ));
                    };
                    ensure_session_store_open_deadline(deadline)?;
                    let child_path = parent.path().join(name);
                    let child = match parent.entry_kind(&child_path)? {
                        None => {
                            let child = parent.create_owned_child_directory_no_replace_with_guard(
                                &child_path,
                                || {
                                    external_guard()?;
                                    ensure_session_store_open_deadline(deadline)
                                },
                            )?;
                            pause_session_namespace_preparation_phase("directory")?;
                            preparation.created_tree.push(CreatedSessionDirectoryTree {
                                parent: parent.try_clone()?,
                                path: child_path,
                                directory: child.try_clone()?,
                            });
                            child
                        }
                        Some(crate::daemons::state::StableEntryKind::Directory)
                            if worktree
                                .is_some_and(|worktree| worktree.path.starts_with(&child_path)) =>
                        {
                            let worktree = worktree.expect("owned worktree guard is present");
                            worktree.verify_owned_namespace()?;
                            let child = parent.open_owned_child(&child_path)?;
                            worktree.verify_owned_namespace()?;
                            child
                        }
                        Some(_) => {
                            return Err(format!(
                                "state directory appeared after its absence was proven: {}",
                                child_path.display()
                            ));
                        }
                    };
                    parent = child;
                }
            }
            ensure_session_store_open_deadline(deadline)?;
            parent.verify_visible()?;

            let directory = match retained_directory {
                Some(directory) => {
                    directory.verify_visible()?;
                    ensure_session_store_open_deadline(deadline)?;
                    if !crate::fs_security::canonical_paths_match(directory.path(), &sessions_dir) {
                        return Err(format!(
                            "preflighted session directory changed before initialization: {}",
                            sessions_dir.display()
                        ));
                    }
                    directory
                }
                None => {
                    if parent.entry_kind(&sessions_dir)?.is_some() {
                        return Err(format!(
                            "session directory appeared after read-only preflight: {}",
                            sessions_dir.display()
                        ));
                    }
                    let child = parent.create_owned_child_directory_no_replace_with_guard(
                        &sessions_dir,
                        || {
                            external_guard()?;
                            ensure_session_store_open_deadline(deadline)
                        },
                    )?;
                    pause_session_namespace_preparation_phase("directory")?;
                    preparation.created_tree.push(CreatedSessionDirectoryTree {
                        parent: parent.try_clone()?,
                        path: sessions_dir.clone(),
                        directory: child.try_clone()?,
                    });
                    child
                }
            };
            ensure_session_store_open_deadline(deadline)?;
            preparation.parent_directory = Some(parent.try_clone()?);
            preparation.directory = Some(directory.try_clone()?);
            let created_identity = retained_identity_file.is_none();
            let identity_file = initialize_preflighted_session_directory_identity_with_guard(
                &directory,
                &parent,
                &sessions_dir,
                retained_identity_file.as_ref(),
                durable_plan.map(|plan| plan.identity_marker_bytes.as_slice()),
                &mut || {
                    external_guard()?;
                    ensure_session_store_open_deadline(deadline)
                },
            )?;
            if created_identity {
                preparation.created_identity_file =
                    Some(identity_file.try_clone().map_err(|error| {
                        format!("failed to retain prepared session identity: {error}")
                    })?);
            }
            ensure_session_store_open_deadline(deadline)?;
            directory.verify_visible()?;
            parent.verify_visible()?;
            ensure_session_store_open_deadline(deadline)?;
            if let Some(worktree) = worktree {
                worktree.verify_owned_namespace()?;
            }
            external_guard()?;
            Ok(SessionStore {
                sessions_dir,
                directory: Some(Arc::new(directory)),
                parent_directory: Some(Arc::new(parent)),
                directory_identity_file: Some(Arc::new(identity_file)),
                initialization_error: None,
                lock_timeout: None,
                lock_deadline: Some(deadline),
                sensitive_values: Arc::new(sensitive_values),
            })
        })();
        drop(namespace_guard);
        match outcome {
            Ok(store) => {
                preparation.store = Some(store);
                Ok(preparation)
            }
            Err(error) => {
                let cleanup_result = preparation.cleanup_inner(deadline, external_guard);
                if cleanup_result.is_err() && durable_plan.is_some() {
                    // The durable namespace plan is now the sole recovery
                    // authority.  Do not let Drop renew this operation's
                    // absolute deadline after a bounded cleanup failure.
                    preparation.armed = false;
                }
                let cleanup = cleanup_result.err();
                Err(match cleanup {
                    Some(cleanup) => {
                        format!("{error}; session preparation cleanup failed: {cleanup}")
                    }
                    None => error,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMessage {
    #[serde(default)]
    pub index: usize,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<PathAttachment>,
}

/// Durable provenance for a transcript entry. Provider roles describe wire-format
/// ordering; they do not prove who supplied the underlying information.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrigin {
    #[default]
    Unknown,
    HumanRequest,
    HumanSteering,
    HumanQuestionAnswer,
    RuntimeContinuation,
    ModelOutput,
    ToolOutput,
}

impl MessageOrigin {
    pub fn is_human(self) -> bool {
        matches!(
            self,
            Self::HumanRequest | Self::HumanSteering | Self::HumanQuestionAnswer
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageProvenance {
    pub message_index: usize,
    pub origin: MessageOrigin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HumanIntentKind {
    Request,
    Steering,
    QuestionAnswer,
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HumanIntentRecord {
    pub kind: HumanIntentKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_message_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_index: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClarificationStatus {
    #[default]
    Pending,
    Answered,
    Unresolved,
    Cancelled,
    Discussed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ClarificationRecord {
    pub invocation_id: crate::tools::ToolInvocationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default)]
    pub question_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub option_details: Vec<crate::interactive::QuestionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_source: Option<crate::interactive::QuestionAnswerSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_invocation_id: Option<crate::tools::ToolInvocationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_question_index: Option<usize>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_answer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Worktree-relative scopes whose actions depend on this answer. An empty
    /// collection is deliberately conservative and means the whole plan step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependent_paths: Vec<String>,
    pub status: ClarificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    pub question_event_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_message_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_event_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathAttachment {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ToolCallRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<crate::tools::ToolInvocationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub arguments: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bwrap_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<crate::config::BoundaryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification_obligations: Vec<VerificationObligation>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub content_generation: u64,
}

pub(crate) fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Pending,
    Running,
    Passed,
    Failed,
    Cancelled,
    Stale,
    Waived,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationAuthority {
    Human,
    Project,
    #[default]
    ApprovedPlan,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationExpectedOutcome {
    #[default]
    Success,
    ProbeMiss,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationInvocationSpec {
    pub tool_name: String,
    pub arguments: serde_json::Value,
    #[serde(default)]
    pub expected_outcome: VerificationExpectedOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationAttempt {
    pub invocation_id: crate::tools::ToolInvocationId,
    pub status: VerificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationObligation {
    pub id: String,
    pub description: String,
    #[serde(default = "default_required_verification")]
    pub required: bool,
    #[serde(default)]
    pub authority: VerificationAuthority,
    pub status: VerificationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_invocation: Option<VerificationInvocationSpec>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_message_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<crate::tools::ToolInvocationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_identity: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub content_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver_source_message_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<VerificationAttempt>,
}

pub(crate) fn default_required_verification() -> bool {
    true
}

impl VerificationObligation {
    pub fn pending(
        id: impl Into<String>,
        description: impl Into<String>,
        affected_paths: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            required: true,
            authority: VerificationAuthority::ApprovedPlan,
            status: VerificationStatus::Pending,
            expected_invocation: None,
            plan_id: String::new(),
            step_index: None,
            source_message_index: None,
            affected_paths,
            invocation_id: None,
            worktree_identity: None,
            content_identity: None,
            content_generation: 0,
            reason: None,
            updated_at: None,
            waiver_source_message_index: None,
            attempts: Vec::new(),
        }
    }

    pub fn pending_tool(
        id: impl Into<String>,
        description: impl Into<String>,
        affected_paths: Vec<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
        expected_outcome: VerificationExpectedOutcome,
    ) -> Result<Self, String> {
        let mut obligation = Self::pending(id, description, affected_paths);
        obligation.expected_invocation = Some(VerificationInvocationSpec {
            tool_name: tool_name.into(),
            arguments: normalize_verification_arguments(&arguments)?,
            expected_outcome,
        });
        Ok(obligation)
    }

    pub fn is_unresolved_required(&self) -> bool {
        self.required
            && !matches!(
                self.status,
                VerificationStatus::Passed | VerificationStatus::Waived
            )
    }
}

pub fn normalize_verification_arguments(
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut normalized = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| "verification arguments must be an object".to_string())?;
    normalized.remove("verification_id");
    normalized.remove("plan_id");
    let normalized = serde_json::Value::Object(normalized);
    if serde_json::to_vec(&normalized)
        .map_err(|error| format!("serialize verification arguments: {error}"))?
        .len()
        > 64 * 1024
    {
        return Err("verification arguments exceed the 64 KiB persistence limit".to_string());
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub current_step_index: usize,
    #[serde(default)]
    pub approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

pub fn normalize_plan_goal(goal: &str) -> String {
    goal.split_whitespace().collect::<Vec<_>>().join(" ")
}
