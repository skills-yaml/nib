//! File-based session persistence under the active profile's sessions directory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

pub mod memory;

#[cfg(debug_assertions)]
fn pause_session_namespace_preparation_phase(phase: &str) -> Result<(), String> {
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
fn pause_session_namespace_preparation_phase(_phase: &str) -> Result<(), String> {
    Ok(())
}

pub(crate) struct SessionDirectoryPreflight {
    sessions_dir: PathBuf,
    parent_path: PathBuf,
    retained_ancestor: crate::daemons::state::StableDirectory,
    retained_directory: Option<crate::daemons::state::StableDirectory>,
    retained_identity_file: Option<File>,
    sensitive_values: Vec<String>,
    runtime_config: crate::config::NibConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionNamespacePreparationPlan {
    version: u32,
    pub(crate) sessions_dir: PathBuf,
    retained_ancestor: PathBuf,
    retained_ancestor_identity: crate::fs_security::DirectoryIdentity,
    proven_missing_directories: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retained_identity: Option<crate::fs_security::FileIdentitySnapshot>,
    retained_anchor_present: bool,
    identity_marker_bytes: Vec<u8>,
}

struct CreatedSessionDirectoryTree {
    parent: crate::daemons::state::StableDirectory,
    path: PathBuf,
    directory: crate::daemons::state::StableDirectory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CreatedSessionDirectoryReceipt {
    parent: PathBuf,
    path: PathBuf,
    identity: crate::fs_security::DirectoryIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SessionPreparationReceipt {
    version: u32,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) session_id: String,
    sessions_directory_identity: crate::fs_security::DirectoryIdentity,
    directory_identity: crate::fs_security::FileIdentitySnapshot,
    planned_session: Session,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_identity: Option<crate::fs_security::FileIdentitySnapshot>,
    created_identity: bool,
    created_directories: Vec<CreatedSessionDirectoryReceipt>,
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
    store: Option<SessionStore>,
    created_tree: Vec<CreatedSessionDirectoryTree>,
    created_identity_file: Option<File>,
    created_session_file: Option<(PathBuf, File)>,
    planned_session: Option<Session>,
    parent_directory: Option<crate::daemons::state::StableDirectory>,
    directory: Option<crate::daemons::state::StableDirectory>,
    sessions_dir: PathBuf,
    namespace_lock: Arc<SessionMutex>,
    armed: bool,
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

    fn cleanup_inner(
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

    fn durable_preparation_plan_with_authority(
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

    fn open_until_with_owned_worktree(
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

impl Plan {
    pub fn new(goal: &str, steps: Vec<PlanStep>) -> Self {
        Self {
            id: format!("plan-{}", Uuid::new_v4().simple()),
            goal: normalize_plan_goal(goal),
            steps,
            current_step_index: 0,
            approved: false,
            approved_at: None,
            outcome: None,
        }
    }

    pub fn has_identity(&self) -> bool {
        !self.id.trim().is_empty() && !self.goal.trim().is_empty()
    }

    pub fn matches_goal(&self, goal: &str) -> bool {
        self.has_identity() && self.goal == normalize_plan_goal(goal)
    }

    pub fn is_resumable_for(&self, goal: &str) -> bool {
        self.is_structured() && !self.is_complete() && self.matches_goal(goal)
    }

    pub fn is_structured(&self) -> bool {
        if !self.has_identity()
            || self.steps.is_empty()
            || self.current_step_index > self.steps.len()
            || self
                .steps
                .iter()
                .any(|step| step.description.trim().is_empty())
            || self.steps[..self.current_step_index]
                .iter()
                .any(|step| step.status != "Completed")
        {
            return false;
        }
        if self.current_step_index == self.steps.len() {
            return self.steps.iter().all(|step| step.status == "Completed");
        }

        let current_status = self.steps[self.current_step_index].status.as_str();
        let current_is_valid = if self.approved {
            matches!(current_status, "InProgress" | "Blocked")
        } else {
            current_status == "Pending"
        };
        current_is_valid
            && self.steps[self.current_step_index + 1..]
                .iter()
                .all(|step| step.status == "Pending")
    }

    pub fn is_complete(&self) -> bool {
        self.current_step_index >= self.steps.len()
            && self.steps.iter().all(|step| step.status == "Completed")
    }

    pub fn approve(&mut self) {
        self.approved = true;
        self.approved_at = Some(Utc::now());
        if let Some(step) = self.steps.get_mut(self.current_step_index) {
            step.status = "InProgress".to_string();
            step.attempts = step.attempts.saturating_add(1);
            step.updated_at = Some(Utc::now());
        }
    }

    pub fn reject(&mut self, reason: impl Into<String>) {
        self.approved = false;
        self.outcome = Some(reason.into());
    }

    pub fn record_tool_outcome(&mut self, success: bool, outcome: impl Into<String>) {
        let Some(step) = self.steps.get_mut(self.current_step_index) else {
            return;
        };
        step.status = if success { "InProgress" } else { "Blocked" }.to_string();
        step.outcome = Some(outcome.into());
        step.updated_at = Some(Utc::now());
    }

    pub fn complete_current_step(&mut self, outcome: impl Into<String>) {
        let Some(step) = self.steps.get_mut(self.current_step_index) else {
            return;
        };
        step.status = "Completed".to_string();
        step.outcome = Some(outcome.into());
        step.updated_at = Some(Utc::now());
        self.current_step_index += 1;
        if let Some(next) = self.steps.get_mut(self.current_step_index) {
            next.status = "InProgress".to_string();
            next.attempts = next.attempts.saturating_add(1);
            next.updated_at = Some(Utc::now());
        } else {
            self.outcome = Some("completed".to_string());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionEvent {
    #[serde(default)]
    pub index: usize,
    pub kind: String,
    #[serde(default)]
    pub details: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillUsageRecord {
    pub skill_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    #[serde(default, skip_serializing_if = "revision_is_zero")]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub messages: Vec<SessionMessage>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub summary_index: usize,
    #[serde(default)]
    pub events: Vec<SessionEvent>,
    #[serde(default)]
    pub active_skills: Vec<String>,
    #[serde(default)]
    pub skill_usage: Vec<SkillUsageRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_follow_ups: Vec<QueuedFollowUp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedFollowUp {
    pub id: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
    pub source: String,
}

fn revision_is_zero(revision: &u64) -> bool {
    *revision == 0
}

fn session_mismatch_field(expected: &Session, published: &Session) -> &'static str {
    if expected.id != published.id {
        return "id";
    }
    if expected.revision != published.revision {
        return "revision";
    }
    if expected.started_at != published.started_at {
        return "started_at";
    }
    if expected.messages != published.messages {
        return "messages";
    }
    if expected.tool_calls.len() != published.tool_calls.len() {
        return "tool_calls.length";
    }
    for (expected, published) in expected.tool_calls.iter().zip(&published.tool_calls) {
        if expected.invocation_id != published.invocation_id {
            return "tool_calls.invocation_id";
        }
        if expected.id != published.id {
            return "tool_calls.id";
        }
        if expected.session_id != published.session_id {
            return "tool_calls.session_id";
        }
        if expected.tool_name != published.tool_name {
            return "tool_calls.tool_name";
        }
        if expected.arguments != published.arguments {
            return "tool_calls.arguments";
        }
        if expected.result != published.result {
            return "tool_calls.result";
        }
        if expected.error != published.error {
            return "tool_calls.error";
        }
        if expected.duration_seconds != published.duration_seconds {
            return "tool_calls.duration_seconds";
        }
        if expected.worktree_path != published.worktree_path {
            return "tool_calls.worktree_path";
        }
        if expected.timestamp != published.timestamp {
            return "tool_calls.timestamp";
        }
        if expected.provider != published.provider {
            return "tool_calls.provider";
        }
        if expected.sandbox_profile != published.sandbox_profile {
            return "tool_calls.sandbox_profile";
        }
        if expected.bwrap_args != published.bwrap_args {
            return "tool_calls.bwrap_args";
        }
        if expected.boundaries != published.boundaries {
            return "tool_calls.boundaries";
        }
        if expected.plan_id != published.plan_id {
            return "tool_calls.plan_id";
        }
    }
    if expected.plan != published.plan {
        return "plan";
    }
    if expected.summary != published.summary {
        return "summary";
    }
    if expected.summary_index != published.summary_index {
        return "summary_index";
    }
    if expected.events != published.events {
        return "events";
    }
    if expected.active_skills != published.active_skills {
        return "active_skills";
    }
    if expected.skill_usage != published.skill_usage {
        return "skill_usage";
    }
    if expected.queued_follow_ups != published.queued_follow_ups {
        return "queued_follow_ups";
    }
    if expected.display_name != published.display_name {
        return "display_name";
    }
    if expected.forked_from != published.forked_from {
        return "forked_from";
    }
    "unknown field"
}

impl Session {
    fn new(id: String) -> Self {
        Self {
            id,
            revision: 0,
            started_at: Some(Utc::now()),
            messages: vec![],
            tool_calls: vec![],
            plan: None,
            summary: None,
            summary_index: 0,
            events: vec![],
            active_skills: vec![],
            skill_usage: vec![],
            queued_follow_ups: vec![],
            display_name: None,
            forked_from: None,
        }
    }

    pub fn validate_message_sequence(&self) -> Result<(), SessionError> {
        let mut previous: Option<&str> = None;
        for (expected_index, message) in self.messages.iter().enumerate() {
            if message.index != expected_index {
                return Err(SessionError::InvalidMessageIndex {
                    expected: expected_index,
                    actual: message.index,
                });
            }
            validate_role_transition(previous, &message.role)?;
            previous = Some(&message.role);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), SessionError> {
        validate_session_id(&self.id)?;
        self.validate_message_sequence()?;
        for (expected_index, event) in self.events.iter().enumerate() {
            if event.index != expected_index {
                return Err(SessionError::InvalidEventIndex {
                    expected: expected_index,
                    actual: event.index,
                });
            }
        }
        if self.summary_index > self.messages.len() {
            return Err(SessionError::InvalidSummaryIndex {
                summary_index: self.summary_index,
                message_count: self.messages.len(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse session JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid session message role: {0}")]
    InvalidRole(String),
    #[error("invalid session role transition: {previous:?} -> {next}")]
    RoleViolation {
        previous: Option<String>,
        next: String,
    },
    #[error("invalid session message index: expected {expected}, got {actual}")]
    InvalidMessageIndex { expected: usize, actual: usize },
    #[error("invalid session event index: expected {expected}, got {actual}")]
    InvalidEventIndex { expected: usize, actual: usize },
    #[error(
        "invalid session summary index: {summary_index} exceeds message count {message_count}"
    )]
    InvalidSummaryIndex {
        summary_index: usize,
        message_count: usize,
    },
    #[error("session file for {expected} contains session {actual}")]
    SessionIdMismatch { expected: String, actual: String },
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session lock was poisoned for {0}")]
    LockPoisoned(String),
    #[error("session already has an active agent run: {0}")]
    RunLeaseHeld(String),
    #[error("invalid session mutation: {0}")]
    InvalidMutation(String),
    #[error("invalid session id: {0}")]
    InvalidSessionId(String),
    #[error("session identifier conflicts with configured sensitive data")]
    SensitiveSessionId,
    #[error("session file {path} is {size} bytes; maximum is {max} bytes")]
    FileTooLarge { path: String, size: u64, max: u64 },
}

fn validate_role_transition(previous: Option<&str>, next: &str) -> Result<(), SessionError> {
    if !matches!(next, "user" | "assistant" | "tool") {
        return Err(SessionError::InvalidRole(next.to_string()));
    }
    let allowed = matches!(
        (previous, next),
        (None, "user")
            | (Some("user"), "user")
            | (Some("user"), "assistant")
            | (Some("assistant"), "user")
            | (Some("assistant"), "tool")
            | (Some("tool"), "assistant")
    );
    if allowed {
        Ok(())
    } else {
        Err(SessionError::RoleViolation {
            previous: previous.map(str::to_string),
            next: next.to_string(),
        })
    }
}

pub(crate) fn validate_session_id(id: &str) -> Result<(), SessionError> {
    if id.is_empty()
        || id.len() > 128
        || matches!(id, "." | "..")
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(SessionError::InvalidSessionId(id.to_string()));
    }
    Ok(())
}

type SessionMutex = Mutex<()>;
type SessionLockRegistry = Mutex<HashMap<PathBuf, Weak<SessionMutex>>>;

fn lock_session_mutex<'a, T>(
    mutex: &'a Mutex<T>,
    path: &Path,
    deadline: Option<Instant>,
) -> Result<std::sync::MutexGuard<'a, T>, SessionError> {
    let Some(deadline) = deadline else {
        return mutex
            .lock()
            .map_err(|_| SessionError::LockPoisoned(path.display().to_string()));
    };

    loop {
        ensure_session_lock_deadline(Some(deadline), path)?;
        match mutex.try_lock() {
            Ok(guard) => {
                ensure_session_lock_deadline(Some(deadline), path)?;
                return Ok(guard);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(SessionError::LockPoisoned(path.display().to_string()));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(SessionError::InvalidMutation(format!(
                        "timed out acquiring session lock: {}",
                        path.display()
                    )));
                }
                std::thread::sleep(SESSION_LOCK_POLL_INTERVAL.min(deadline - now));
            }
        }
    }
}

fn ensure_session_lock_deadline(
    deadline: Option<Instant>,
    path: &Path,
) -> Result<(), SessionError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(SessionError::InvalidMutation(format!(
            "timed out acquiring session lock: {}",
            path.display()
        )));
    }
    Ok(())
}

static SESSION_LOCKS: OnceLock<SessionLockRegistry> = OnceLock::new();
static SESSION_PREPARATION_LOCKS: OnceLock<SessionLockRegistry> = OnceLock::new();

fn session_preparation_mutex(
    path: &Path,
    deadline: Option<Instant>,
) -> Result<Arc<SessionMutex>, String> {
    let registry = SESSION_PREPARATION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry =
        lock_session_mutex(registry, path, deadline).map_err(|error| error.to_string())?;
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}
const MAX_SESSION_JSON_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LISTED_SESSIONS: usize = 10_000;
const MAX_SESSION_DIRECTORY_ENTRIES: usize = MAX_LISTED_SESSIONS + SESSION_LOCK_STRIPES + 16;
const MAX_SESSION_DIRECTORY_NAME_BYTES: usize = MAX_SESSION_DIRECTORY_ENTRIES * 256;
const SESSION_LOCK_STRIPES: usize = 64;
const SESSION_DIRECTORY_IDENTITY_FILE: &str = ".session-directory.identity";
const SKILL_USAGE_LOCK_FILE: &str = ".skill-usage.lock";
const OPERATION_ERROR_SENTINEL: &str = "session operation failed under stable lock";
const SESSION_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy)]
pub(crate) struct SessionLockPolicy {
    timeout: Duration,
    offload_waits: bool,
}

tokio::task_local! {
    static SESSION_LOCK_POLICY: SessionLockPolicy;
}

thread_local! {
    static SESSION_LOCK_OFFLOAD_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct SessionLockOffloadGuard;

impl SessionLockOffloadGuard {
    fn enter() -> Self {
        SESSION_LOCK_OFFLOAD_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for SessionLockOffloadGuard {
    fn drop(&mut self) {
        SESSION_LOCK_OFFLOAD_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    sessions_dir: PathBuf,
    directory: Option<Arc<crate::daemons::state::StableDirectory>>,
    parent_directory: Option<Arc<crate::daemons::state::StableDirectory>>,
    directory_identity_file: Option<Arc<File>>,
    initialization_error: Option<String>,
    lock_timeout: Option<Duration>,
    lock_deadline: Option<Instant>,
    sensitive_values: Arc<Vec<String>>,
}

pub(crate) struct SessionRunLease {
    session_id: String,
    sessions_dir: PathBuf,
    lock: crate::daemons::state::HeldFileLock,
}

impl SessionRunLease {
    pub(crate) fn verify(&self) -> Result<(), SessionError> {
        self.lock.verify().map_err(|error| {
            SessionError::InvalidMutation(format!(
                "active run lease for {} became unsafe: {error}",
                self.session_id
            ))
        })
    }

    pub(crate) fn verify_for(
        &self,
        session_id: &str,
        sessions_dir: &Path,
    ) -> Result<(), SessionError> {
        if self.session_id != session_id {
            return Err(SessionError::InvalidMutation(format!(
                "active run lease for {} cannot authorize session {session_id}",
                self.session_id
            )));
        }
        if !crate::fs_security::canonical_paths_match(&self.sessions_dir, sessions_dir) {
            return Err(SessionError::InvalidMutation(format!(
                "active run lease for {} cannot authorize session directory {}",
                self.session_id,
                sessions_dir.display()
            )));
        }
        self.verify()
    }
}

#[derive(Debug)]
struct OpenedSession {
    session: Session,
    file: File,
    metadata: fs::Metadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDeleteOutcome {
    Missing,
    Retained,
    Deleted,
}

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

    fn for_project_until(project_root: &Path, deadline: Option<Instant>) -> Result<Self, String> {
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

    fn with_lock_deadline(mut self, deadline: Instant) -> Self {
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

    fn path(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{id}.json"))
    }

    fn lock_path(&self, id: &str) -> Result<PathBuf, SessionError> {
        self.validate_session_id(id)?;
        let stripe = session_lock_stripe(id);
        Ok(self
            .sessions_dir
            .join(format!(".session-lock-{stripe:02}.lock")))
    }

    fn validate_session_id(&self, id: &str) -> Result<(), SessionError> {
        let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
            id,
            self.sensitive_values.iter().cloned(),
        );
        if redacted != id {
            return Err(SessionError::SensitiveSessionId);
        }
        validate_session_id(id)
    }

    fn process_lock(
        &self,
        path: &Path,
        deadline: Option<Instant>,
    ) -> Result<Arc<SessionMutex>, SessionError> {
        let registry = SESSION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        self.process_lock_in(registry, path, deadline)
    }

    fn process_lock_in(
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

    fn directory(&self) -> Result<&crate::daemons::state::StableDirectory, SessionError> {
        if let Some(error) = &self.initialization_error {
            return Err(SessionError::InvalidMutation(error.clone()));
        }
        self.directory.as_deref().ok_or_else(|| {
            SessionError::InvalidMutation("session directory is not initialized".to_string())
        })
    }

    fn verify_directory_binding(&self) -> Result<(), SessionError> {
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

    fn with_anchored_lock<T>(
        &self,
        lock_path: PathBuf,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_deadline(lock_path, self.lock_deadline(), operation)
    }

    fn lock_deadline(&self) -> Option<Instant> {
        if let Some(deadline) = self.lock_deadline {
            return Some(deadline);
        }
        let now = Instant::now();
        self.lock_timeout
            .or_else(|| SESSION_LOCK_POLICY.try_with(|policy| policy.timeout).ok())
            .map(|timeout| now.checked_add(timeout).unwrap_or(now))
    }

    fn with_anchored_lock_until<T>(
        &self,
        lock_path: PathBuf,
        deadline: Instant,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_deadline(lock_path, Some(deadline), operation)
    }

    fn with_anchored_lock_deadline<T>(
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

    fn with_anchored_lock_deadline_inner<T>(
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

    fn with_session_lock<T>(
        &self,
        id: &str,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock(self.lock_path(id)?, operation)
    }

    fn with_session_lock_until<T>(
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

    fn with_skill_usage_lock_until<T>(
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

    fn load_opened_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        id: &str,
    ) -> Result<Option<OpenedSession>, SessionError> {
        self.load_opened_unlocked_with_hook(directory, id, || Ok(()))
    }

    fn load_opened_unlocked_with_hook(
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

    fn create_unpublished_session_with_receipt_and_guard(
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

    fn save_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        session: &Session,
        expected: Option<&File>,
    ) -> Result<(), SessionError> {
        self.save_unlocked_with_commit_check(directory, session, expected, || Ok(()))
    }

    fn save_unlocked_with_commit_check(
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

    fn save_unlocked_with_namespace_guard(
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

    fn save_unlocked_with_deadline(
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

    fn save_unlocked_with_deadline_and_commit_check(
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

    fn save_under_skill_usage_lock(&self, session: &mut Session) -> Result<(), SessionError> {
        self.save_under_skill_usage_lock_with_deadline(session, None)
    }

    fn save_under_skill_usage_lock_with_deadline(
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

    fn update_session_under_skill_usage_lock<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.update_session_under_skill_usage_lock_with_deadline(id, None, update)
    }

    fn update_session_under_skill_usage_lock_with_deadline<T>(
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

    fn update_or_create_session_under_skill_usage_lock<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.update_or_create_session_under_skill_usage_lock_with_deadline(id, None, update)
    }

    fn update_or_create_session_under_skill_usage_lock_with_deadline<T>(
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
        self.update_or_create_session(id, |session| {
            validate_role_transition(session.messages.last().map(|m| m.role.as_str()), role)?;
            let index = session.messages.len();
            session.messages.push(SessionMessage {
                index,
                role: role.to_string(),
                content: content.to_string(),
                timestamp: Some(Utc::now()),
                attachments: Vec::new(),
            });
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

    fn list_entries_result(
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

fn open_session_directory(
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

fn open_existing_session_directory_until(
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

fn ensure_session_store_open_deadline(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("session store lock deadline elapsed".to_string());
    }
    Ok(())
}

fn initialize_session_directory_identity(
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

fn initialize_session_directory_identity_with_guard(
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

fn initialize_preflighted_session_directory_identity_with_guard(
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

fn session_directory_identity_anchor(visible_marker: &Path) -> Result<PathBuf, String> {
    crate::daemons::state::daemon_lock_anchor_path(visible_marker)
}

fn session_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash as usize) % SESSION_LOCK_STRIPES
}

#[cfg(test)]
thread_local! {
    static PAUSE_RECORD_EVENT_ONCE_COMMIT: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn pause_record_event_once_commit(deadline: Instant) {
    if PAUSE_RECORD_EVENT_ONCE_COMMIT.get() {
        while Instant::now() < deadline {
            std::thread::yield_now();
        }
    }
}

#[cfg(not(test))]
fn pause_record_event_once_commit(_deadline: Instant) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn session_namespace_snapshot(path: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
        let mut snapshot = fs::read_dir(path)
            .expect("read session namespace")
            .map(|entry| {
                let entry = entry.expect("session namespace entry");
                (
                    entry.file_name(),
                    crate::fs_security::read_namespace_snapshot_file(&entry.path())
                        .expect("session namespace bytes"),
                )
            })
            .collect::<Vec<_>>();
        snapshot.sort_by(|left, right| left.0.cmp(&right.0));
        snapshot
    }

    fn session_namespace_shape(path: &Path) -> Vec<(std::ffi::OsString, u64)> {
        let mut snapshot = fs::read_dir(path)
            .expect("read session namespace shape")
            .map(|entry| {
                let entry = entry.expect("session namespace shape entry");
                (
                    entry.file_name(),
                    entry
                        .metadata()
                        .expect("session namespace shape metadata")
                        .len(),
                )
            })
            .collect::<Vec<_>>();
        snapshot.sort_by(|left, right| left.0.cmp(&right.0));
        snapshot
    }

    #[test]
    fn failed_preparation_preserves_a_concurrently_adopted_session_namespace() {
        let root = tempfile::tempdir().expect("project root");
        let mut config = crate::config::NibConfig::default();
        crate::config::save_nib_config_full(root.path(), &mut config).expect("config");
        let deadline = Instant::now() + Duration::from_secs(5);
        let preflight_a = SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
            .expect("A preflight");
        let mut preparation_a = preflight_a.open_until(deadline).expect("A preparation");
        preparation_a
            .create_unpublished_session("session-a")
            .expect("A session");

        let preflight_b = SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
            .expect("B preflight after A publication");
        let mut preparation_b = preflight_b
            .open_until(deadline)
            .expect("B adopts namespace");
        preparation_b
            .create_unpublished_session("session-b")
            .expect("B session");
        let sessions_dir = preparation_b.store().sessions_dir().to_path_buf();
        let store_b = preparation_b.disarm();
        let committed_b = session_namespace_snapshot(&sessions_dir)
            .into_iter()
            .filter(|(name, _)| name != "session-a.json")
            .collect::<Vec<_>>();

        preparation_a
            .cleanup(deadline)
            .expect("A leaf-only compensation");

        assert_eq!(session_namespace_snapshot(&sessions_dir), committed_b);
        assert!(store_b.load_result("session-b").expect("load B").is_some());
        assert!(!sessions_dir.join("session-a.json").exists());
        assert!(sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE).is_file());
    }

    #[test]
    fn durable_planned_session_receipt_cleans_publication_before_identity_update() {
        let root = tempfile::tempdir().expect("project root");
        let mut config = crate::config::NibConfig::default();
        crate::config::save_nib_config_full(root.path(), &mut config).expect("config");
        let deadline = Instant::now() + Duration::from_secs(5);
        let preflight = SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
            .expect("session preflight");
        let mut preparation = preflight.open_until(deadline).expect("session preparation");
        preparation
            .plan_unpublished_session("planned-before-publication")
            .expect("durable session plan");
        let receipt = preparation
            .durable_receipt("planned-before-publication")
            .expect("receipt before publication");
        assert!(receipt.session_identity.is_none());

        preparation
            .create_unpublished_session("planned-before-publication")
            .expect("publish planned session");
        let sessions_dir = preparation.store().sessions_dir().to_path_buf();
        assert!(sessions_dir
            .join("planned-before-publication.json")
            .is_file());
        drop(preparation.disarm());

        SessionStorePreparation::cleanup_durable(&receipt, deadline)
            .expect("restart cleans the exact planned publication");
        assert!(!sessions_dir
            .join("planned-before-publication.json")
            .exists());
        assert!(!sessions_dir.exists());
    }

    #[test]
    fn planned_namespace_cleanup_preserves_unrelated_or_ambiguous_state_byte_exactly() {
        let root = tempfile::tempdir().expect("project root");
        let mut config = crate::config::NibConfig::default();
        crate::config::save_nib_config_full(root.path(), &mut config).expect("config");
        let deadline = Instant::now() + Duration::from_secs(5);
        let preflight = SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
            .expect("session preflight");
        let plan = preflight
            .durable_preparation_plan_after_owned_worktree("hostile-preservation", deadline, None)
            .expect("durable namespace plan");
        let preparation = preflight
            .open_until_with_owned_worktree(deadline, None, Some(&plan), &mut || Ok(()))
            .expect("publish planned namespace");
        let sessions_dir = preparation.store().sessions_dir().to_path_buf();
        drop(preparation.disarm());
        std::fs::write(sessions_dir.join("hostile-sentinel"), b"preserve-me")
            .expect("hostile sentinel");
        let before = session_namespace_snapshot(&sessions_dir);

        let error = SessionStorePreparation::cleanup_planned_namespace(&plan, deadline)
            .expect_err("unrelated state must fail closed");
        assert!(
            error.contains("unrelated entry"),
            "unexpected error: {error}"
        );
        assert_eq!(session_namespace_snapshot(&sessions_dir), before);

        std::fs::remove_file(sessions_dir.join("hostile-sentinel")).expect("remove sentinel");
        let marker = sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
        std::fs::write(&marker, b"ambiguous-marker").expect("replace marker content");
        let anchor = session_directory_identity_anchor(&marker).expect("identity anchor");
        let before_marker = session_namespace_snapshot(&sessions_dir);
        let before_anchor = std::fs::read(&anchor).expect("anchor bytes");
        let error = SessionStorePreparation::cleanup_planned_namespace(&plan, deadline)
            .expect_err("ambiguous marker must fail closed");
        assert!(
            error.contains("marker changed") || error.contains("marker is ambiguous"),
            "unexpected marker error: {error}"
        );
        assert_eq!(session_namespace_snapshot(&sessions_dir), before_marker);
        assert_eq!(
            std::fs::read(anchor).expect("preserved anchor"),
            before_anchor
        );
    }

    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_ROOT: &str = "NIB_SESSION_COMMIT_CHILD_ROOT";
    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_ID: &str = "NIB_SESSION_COMMIT_CHILD_ID";
    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_MODE: &str = "NIB_SESSION_COMMIT_CHILD_MODE";
    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_READY: &str = "NIB_SESSION_COMMIT_CHILD_READY";
    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_RELEASE: &str = "NIB_SESSION_COMMIT_CHILD_RELEASE";

    fn plan_step(description: &str) -> PlanStep {
        PlanStep {
            description: description.to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        }
    }

    #[test]
    fn adjacent_user_turns_are_valid_after_a_run_without_an_assistant_message() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session_with_id("adjacent-user-turns");

        store
            .try_append_message(&session.id, "user", "first request")
            .expect("first user turn");
        store
            .try_append_message(&session.id, "user", "retry request")
            .expect("a reconciled run may accept the next user turn directly");

        let persisted = store.load(&session.id).expect("persisted session");
        assert_eq!(
            persisted
                .messages
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            ["user", "user"]
        );
        persisted.validate().expect("valid persisted sequence");
    }

    #[test]
    fn plan_identity_is_unique_and_goal_matching_is_exact_after_normalization() {
        let first = Plan::new(
            "  Implement\tplan identity\nand resume  ",
            vec![plan_step("implement")],
        );
        let second = Plan::new(
            "Implement plan identity and resume",
            vec![plan_step("implement")],
        );

        assert_ne!(first.id, second.id);
        assert_eq!(first.goal, "Implement plan identity and resume");
        assert!(first.matches_goal("Implement   plan identity and resume"));
        assert!(!first.matches_goal("implement plan identity and resume"));
        assert!(first.is_resumable_for("Implement plan identity and resume"));

        let mut malformed = Plan::new(
            "Implement plan identity and resume",
            vec![plan_step("implement")],
        );
        malformed.steps[0].description.clear();
        malformed.approve();
        assert!(!malformed.is_structured());
        assert!(!malformed.is_resumable_for("Implement plan identity and resume"));

        let mut stale_cursor = Plan::new(
            "Implement plan identity and resume",
            vec![plan_step("implement"), plan_step("verify")],
        );
        stale_cursor.approve();
        stale_cursor.steps[0].status = "Completed".to_string();
        assert!(!stale_cursor.is_structured());
        assert!(!stale_cursor.is_resumable_for("Implement plan identity and resume"));

        let mut advanced_future = Plan::new(
            "Implement plan identity and resume",
            vec![plan_step("implement"), plan_step("verify")],
        );
        advanced_future.approve();
        advanced_future.steps[1].status = "InProgress".to_string();
        assert!(!advanced_future.is_structured());
    }

    #[test]
    fn legacy_plan_metadata_defaults_empty_for_runtime_invalidation() {
        let legacy: Plan = serde_json::from_value(serde_json::json!({
            "steps": [{
                "description": "legacy step",
                "status": "Pending"
            }],
            "current_step_index": 0
        }))
        .expect("legacy plan");

        assert!(!legacy.has_identity());
        assert!(!legacy.is_structured());
        assert!(!legacy.is_resumable_for("legacy goal"));
    }

    #[cfg(windows)]
    fn create_directory_junction(junction: &Path, target: &Path) {
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(junction)
            .arg(target)
            .output()
            .expect("create directory junction");
        assert!(
            output.status.success(),
            "mklink failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn session_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session();
        store.append_message(&session.id, "user", "hello");

        let loaded = store.load(&session.id).expect("load session");
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].role, "user");
        assert_eq!(loaded.messages[0].content, "hello");
        assert!(loaded.messages[0].timestamp.is_some());

        let raw = fs::read_to_string(store.path(&session.id)).expect("read file");
        let reparsed: Session = serde_json::from_str(&raw).expect("parse");
        assert_eq!(reparsed, loaded);
    }

    #[test]
    fn session_audit_floats_roundtrip_without_losing_a_ulp() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session();
        let duration = 1.551_978_736_000_000_1_f64;
        let invocation_id = crate::tools::ToolInvocationId::new();

        store
            .record_tool_call(ToolCallRecord {
                invocation_id: Some(invocation_id),
                session_id: Some(session.id.clone()),
                tool_name: Some("roundtrip_probe".to_string()),
                arguments: serde_json::json!({"nested_duration": duration}),
                duration_seconds: Some(duration),
                ..ToolCallRecord::default()
            })
            .expect("persist exact audit float");

        let loaded = store.load(&session.id).expect("load session");
        let call = loaded.tool_calls.last().expect("audit call");
        assert_eq!(call.invocation_id, Some(invocation_id));
        assert_eq!(
            call.duration_seconds.expect("duration").to_bits(),
            duration.to_bits()
        );
        assert_eq!(
            call.arguments["nested_duration"]
                .as_f64()
                .expect("nested duration")
                .to_bits(),
            duration.to_bits()
        );
    }

    #[test]
    fn loads_legacy_session_without_timestamps() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let legacy = r#"{
  "id": "legacy-session",
  "messages": [
    {"role": "user", "content": "hi"}
  ],
  "tool_calls": []
}"#;
        fs::write(store.path("legacy-session"), legacy).expect("write legacy");

        let loaded = store.load("legacy-session").expect("load legacy");
        assert_eq!(loaded.messages.len(), 1);
        assert!(loaded.messages[0].timestamp.is_none());
    }

    #[test]
    fn loads_legacy_tool_call_record_without_invocation_id() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let legacy = r#"{
  "id": "legacy-tool-record",
  "messages": [],
  "tool_calls": [
    {
      "id": "tool-legacy",
      "tool_name": "read_file",
      "arguments": {"path": "README.md"}
    }
  ]
}"#;
        fs::write(store.path("legacy-tool-record"), legacy).expect("write legacy");

        let loaded = store.load("legacy-tool-record").expect("load legacy");
        let call = loaded.tool_calls.first().expect("legacy tool record");
        assert_eq!(call.id.as_deref(), Some("tool-legacy"));
        assert_eq!(call.invocation_id, None);
    }

    #[test]
    fn oversized_sparse_session_is_rejected_before_reading() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let path = store.path("oversized");
        fs::File::create(&path)
            .and_then(|file| file.set_len(MAX_SESSION_JSON_BYTES + 1))
            .expect("create sparse session");

        let error = store
            .load_result("oversized")
            .expect_err("oversized session must fail closed");

        assert!(matches!(error, SessionError::FileTooLarge { .. }));
        assert_eq!(
            fs::metadata(path).unwrap().len(),
            MAX_SESSION_JSON_BYTES + 1
        );
    }

    #[test]
    fn list_result_rejects_valid_named_corrupt_or_mismatched_sessions() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let path = store.path("corrupt-session");
        fs::write(&path, b"{not valid json").expect("write corrupt session");

        let corrupt_error = store
            .list_result()
            .expect_err("corrupt session must fail strict enumeration");
        assert!(
            corrupt_error.to_string().contains("parse session JSON"),
            "{corrupt_error}"
        );

        fs::write(&path, r#"{"id":"different-session"}"#).expect("write mismatched session");
        let mismatch_error = store
            .list_result()
            .expect_err("mismatched session must fail strict enumeration");
        assert!(
            mismatch_error
                .to_string()
                .contains("contains session different-session"),
            "{mismatch_error}"
        );
    }

    #[test]
    fn record_skill_usage_rejects_noncanonicalizable_name_without_persisting() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session();

        let error = store
            .record_skill_usage(&session.id, "!!!", Some("invalid".to_string()))
            .expect_err("invalid skill name must be rejected");

        assert!(matches!(error, SessionError::InvalidMutation(_)));
        let persisted = store.load(&session.id).expect("load session");
        assert!(persisted.active_skills.is_empty());
        assert!(persisted.skill_usage.is_empty());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_file_replacement_during_read_is_rejected() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-during-read");
        let path = store.path(&session.id);
        let displaced = store.sessions_dir().join("displaced-read-session.json");
        let mut replacement = Session::new(session.id.clone());
        replacement.summary = Some("replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

        let error = store
            .with_session_lock(&session.id, |directory| {
                store.load_opened_unlocked_with_hook(directory, &session.id, || {
                    fs::rename(&path, &displaced)?;
                    fs::write(&path, &replacement_bytes)?;
                    Ok(())
                })
            })
            .expect_err("replacement during read must fail");

        assert!(error.to_string().contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(&path).expect("replacement bytes"),
            replacement_bytes
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_file_replacement_during_update_is_not_overwritten() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-file");
        let path = store.path(&session.id);
        let displaced = store.sessions_dir().join("displaced-session.json");
        let mut replacement = Session::new(session.id.clone());
        replacement.summary = Some("replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

        let error = store
            .update_session(&session.id, |current| {
                fs::rename(&path, &displaced)?;
                fs::write(&path, &replacement_bytes)?;
                current.summary = Some("must-not-publish".to_string());
                Ok(())
            })
            .expect_err("replaced session identity must fail");

        assert!(error.to_string().contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(&path).expect("replacement bytes"),
            replacement_bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_child_session_commit_barrier_and_fsync_crash_recovery() {
        if let Some(root) = std::env::var_os(SESSION_COMMIT_CHILD_ROOT) {
            run_session_commit_child(Path::new(&root));
            return;
        }

        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let replacement_session = store.create_session_with_id("child-session-commit-substitution");
        let replacement_path = store.path(&replacement_session.id);
        let displaced_path = store.sessions_dir().join("child-session-displaced");
        let ready = root.path().join("session-replacement.ready");
        let release = root.path().join("session-replacement.release");
        let mut child = spawn_session_commit_child(
            root.path(),
            &replacement_session.id,
            "replace",
            &ready,
            Some(&release),
        );
        wait_for_session_commit_child(&mut child, &ready);

        let mut replacement = replacement_session.clone();
        replacement.revision = 1;
        replacement.summary = Some("authoritative replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");
        fs::rename(&replacement_path, &displaced_path).expect("displace expected session");
        fs::write(&replacement_path, &replacement_bytes).expect("install replacement session");
        fs::write(&release, b"release").expect("release replacement child");
        let status = child.wait().expect("wait for replacement child");
        assert!(status.success(), "replacement child failed: {status}");
        assert_eq!(
            fs::read(&replacement_path).expect("replacement session bytes"),
            replacement_bytes
        );
        assert_eq!(
            fs::read(&displaced_path).expect("displaced session bytes"),
            serde_json::to_vec_pretty(&replacement_session).expect("serialize original")
        );

        let crash_session = store.create_session_with_id("child-session-fsync-crash");
        let crash_path = store.path(&crash_session.id);
        let crash_before = fs::read(&crash_path).expect("session before crash");
        let crash_ready = root.path().join("session-crash.ready");
        let mut crash_child =
            spawn_session_commit_child(root.path(), &crash_session.id, "kill", &crash_ready, None);
        wait_for_session_commit_child(&mut crash_child, &crash_ready);
        let temporary = session_temporary_paths(store.sessions_dir());
        assert_eq!(temporary.len(), 1, "expected one fsynced session temp");
        crash_child.kill().expect("kill session writer");
        crash_child.wait().expect("reap session writer");
        assert!(
            temporary[0].exists(),
            "killed writer temp disappeared early"
        );

        drop(store);
        let recovered = SessionStore::new(root.path());
        assert_eq!(
            fs::read(&crash_path).expect("session after recovery"),
            crash_before
        );
        assert!(
            session_temporary_paths(recovered.sessions_dir()).is_empty(),
            "fresh session store left the killed writer temp"
        );
        assert_eq!(
            recovered
                .load_result(&crash_session.id)
                .expect("load recovered session")
                .expect("recovered session"),
            crash_session
        );
    }

    #[test]
    fn stale_session_snapshot_cannot_overwrite_a_newer_revision() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("session-revision-cas");
        let mut first = store.load(&session.id).expect("first snapshot");
        let mut stale = first.clone();
        first.summary = Some("first writer".to_string());
        stale.summary = Some("stale writer".to_string());

        store.save(&mut first).expect("first revision commit");
        let error = store
            .save(&mut stale)
            .expect_err("stale snapshot revision must be rejected");

        assert!(
            error.to_string().contains("stale session revision"),
            "{error}"
        );
        assert_eq!(
            store
                .load(&session.id)
                .expect("authoritative session")
                .summary
                .as_deref(),
            Some("first writer")
        );
    }

    #[test]
    fn consecutive_session_saves_refresh_the_snapshot_revision() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let mut session = store.create_session_with_id("session-consecutive-save");
        assert_eq!(session.revision, 0);

        session.summary = Some("first".to_string());
        store.save(&mut session).expect("first save");
        assert_eq!(session.revision, 1);
        session.summary = Some("second".to_string());
        store.save(&mut session).expect("second save");
        assert_eq!(session.revision, 2);

        let persisted = store.load(&session.id).expect("persisted session");
        assert_eq!(persisted.revision, 2);
        assert_eq!(persisted.summary.as_deref(), Some("second"));
    }

    #[test]
    fn legacy_session_without_revision_defaults_to_zero() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("legacy-session-revision");
        let path = store.path(&session.id);
        let bytes = fs::read(&path).expect("legacy-compatible session bytes");
        assert!(!String::from_utf8_lossy(&bytes).contains("\"revision\""));
        assert_eq!(store.load(&session.id).expect("legacy session").revision, 0);
    }

    #[test]
    fn session_revision_overflow_preserves_disk_and_snapshot() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let mut session = store.create_session_with_id("session-revision-overflow");
        session.revision = u64::MAX;
        let path = store.path(&session.id);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&session).expect("overflow session JSON"),
        )
        .expect("write overflow session");
        let before = fs::read(&path).expect("session before failed save");

        let error = store
            .save(&mut session)
            .expect_err("revision overflow must fail closed");

        assert!(error.to_string().contains("revision overflowed"), "{error}");
        assert_eq!(session.revision, u64::MAX);
        assert_eq!(fs::read(path).expect("session after failed save"), before);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_delete_rejects_replacement_at_quarantine_boundary() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-delete");
        let path = store.path(&session.id);
        let displaced = store.sessions_dir().join("displaced-delete-session.json");
        let mut replacement = Session::new(session.id.clone());
        replacement.summary = Some("newer replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

        let error = store
            .delete_if_with_commit_check(
                &session.id,
                |_, _| true,
                || {
                    fs::rename(&path, &displaced)?;
                    fs::write(&path, &replacement_bytes)?;
                    Ok(())
                },
            )
            .expect_err("replaced session must not be deleted");

        assert!(error.to_string().contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(path).expect("replacement session"),
            replacement_bytes
        );
        assert!(displaced.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_lock_replacement_while_held_is_rejected() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-lock");
        let lock_path = store.lock_path(&session.id).expect("lock path");
        let displaced = store.sessions_dir().join("displaced-session.lock");

        let error = store
            .update_session(&session.id, |current| {
                fs::rename(&lock_path, &displaced)?;
                fs::write(&lock_path, b"replacement-lock")?;
                current.summary = Some("operation-must-report-failure".to_string());
                Ok(())
            })
            .expect_err("replaced lock identity must fail");

        assert!(
            error.to_string().contains("different identities"),
            "{error}"
        );
        assert_eq!(
            fs::read(&lock_path).expect("replacement lock"),
            b"replacement-lock"
        );
        assert!(SessionStore::new(root.path())
            .load_result(&session.id)
            .is_err());
    }

    #[test]
    fn expired_deadline_rejects_free_session_locks_without_mutating() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("expired-free-session-lock");
        let mut update_ran = false;

        let error = store
            .update_session_with_deadline(
                &session.id,
                Instant::now() - Duration::from_millis(1),
                |current| {
                    update_ran = true;
                    current.summary = Some("must not persist".to_string());
                    Ok(())
                },
            )
            .expect_err("an expired deadline must reject uncontended session locks");

        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );
        assert!(!update_ran, "expired session update entered its mutation");
        assert_eq!(
            store.load(&session.id).expect("session remains").summary,
            None
        );
    }

    #[test]
    fn session_read_crossing_its_deadline_is_rejected_without_late_anchor_cleanup() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("expired-post-read-session");
        let deadline = Instant::now() + Duration::from_millis(40);
        let mut read_namespace = None;

        let error = store
            .with_session_lock_until(&session.id, deadline, |directory| {
                store.load_opened_unlocked_with_hook(directory, &session.id, || {
                    read_namespace = Some(session_namespace_snapshot(store.sessions_dir()));
                    while Instant::now() < deadline {
                        std::thread::yield_now();
                    }
                    Ok(())
                })?;
                Ok(())
            })
            .expect_err("a session read crossing its deadline must be rejected");

        assert!(
            error
                .to_string()
                .contains("timed out acquiring daemon state lock"),
            "{error}"
        );
        let read_namespace = read_namespace.expect("captured post-read lock namespace");
        assert_eq!(
            session_namespace_snapshot(store.sessions_dir()),
            read_namespace,
            "post-expiry lock cleanup mutated its persistent anchor"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            session_namespace_snapshot(store.sessions_dir()),
            read_namespace,
            "post-expiry lock cleanup mutated its persistent anchor later"
        );
        assert_eq!(
            store
                .load_result(&session.id)
                .expect("later unbounded read")
                .expect("session")
                .id,
            session.id
        );
    }

    #[test]
    fn session_publication_rechecks_deadline_at_the_commit_boundary() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("expired-session-commit");
        let path = store.path(&session.id);
        let original = fs::read(&path).expect("original session bytes");
        let before_namespace = session_namespace_snapshot(store.sessions_dir());
        let mut paused_namespace = None;
        let mut expected_temporary = None;

        let error = store
            .with_skill_usage_lock(|| {
                store.with_session_lock(&session.id, |directory| {
                    let mut opened = store
                        .load_opened_unlocked(directory, &session.id)?
                        .ok_or_else(|| SessionError::NotFound(session.id.clone()))?;
                    opened.session.summary = Some("must not publish".to_string());
                    opened.session.revision += 1;
                    expected_temporary = Some(
                        serde_json::to_vec_pretty(&opened.session)
                            .expect("expected temporary session bytes"),
                    );
                    let commit_timeout = if cfg!(windows) {
                        Duration::from_secs(2)
                    } else {
                        Duration::from_millis(40)
                    };
                    let deadline = Instant::now() + commit_timeout;
                    store.save_unlocked_with_deadline_and_commit_check(
                        directory,
                        &opened.session,
                        Some(&opened.file),
                        Some(deadline),
                        || {
                            paused_namespace = Some(session_namespace_shape(store.sessions_dir()));
                            while Instant::now() < deadline {
                                std::thread::yield_now();
                            }
                            Ok(())
                        },
                    )
                })
            })
            .expect_err("expired session precommit must fail");

        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );
        assert_eq!(fs::read(&path).expect("unchanged session bytes"), original);
        let paused_namespace = paused_namespace.expect("captured session precommit namespace");
        let post_failure_namespace = session_namespace_snapshot(store.sessions_dir());
        assert_eq!(
            session_namespace_shape(store.sessions_dir()),
            paused_namespace,
            "expired session cleanup mutated transaction artifacts"
        );
        let expected_temporary = expected_temporary.expect("serialized temporary session");
        let mut saw_temporary = false;
        for (name, bytes) in &post_failure_namespace {
            let rendered = name.to_string_lossy();
            if rendered.starts_with(".nib-session-") && rendered.ends_with(".tmp") {
                assert!(!saw_temporary, "multiple retained session temporaries");
                assert_eq!(bytes, &expected_temporary, "retained temporary bytes");
                saw_temporary = true;
            } else if let Some((_, expected)) = before_namespace
                .iter()
                .find(|(before_name, _)| before_name == name)
            {
                assert_eq!(bytes, expected, "pre-existing session namespace bytes");
            } else {
                assert!(
                    bytes.is_empty(),
                    "unexpected nonempty session transaction artifact: {rendered}"
                );
            }
        }
        assert!(
            saw_temporary,
            "expired publication did not retain its temporary"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            session_namespace_snapshot(store.sessions_dir()),
            post_failure_namespace,
            "expired session cleanup mutated transaction artifacts later"
        );

        let mut update_ran = false;
        let missing = "preexpired-session-create";
        store
            .update_or_create_session_with_deadline(
                missing,
                Instant::now() - Duration::from_millis(1),
                |_session| {
                    update_ran = true;
                    Ok(())
                },
            )
            .expect_err("preexpired update-or-create must fail on free locks");
        assert!(!update_ran, "preexpired session mutation ran");
        assert!(
            !store.path(missing).exists(),
            "preexpired session was created"
        );
    }

    #[test]
    fn append_once_audit_expiry_preserves_the_complete_session_namespace() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("expired-append-once-audit");
        let path = store.path(&session.id);
        let original = fs::read(&path).expect("original audit session bytes");
        let before_namespace = session_namespace_snapshot(store.sessions_dir());
        let deadline = Instant::now() + Duration::from_millis(40);

        PAUSE_RECORD_EVENT_ONCE_COMMIT.set(true);
        let result = store.record_event_once_with_deadline(
            &session.id,
            "subagent_execution_reconciled",
            "reconciliation-v1",
            serde_json::json!({
                "reconciliation_id": "reconciliation-v1",
                "outcome": "cancelled_after_verified_cleanup",
            }),
            serde_json::json!({
                "outcome": "cancelled_after_verified_cleanup",
            }),
            deadline,
        );
        PAUSE_RECORD_EVENT_ONCE_COMMIT.set(false);
        let error = result.expect_err("expired append-once publication must fail closed");

        assert!(
            error
                .to_string()
                .contains("timed out acquiring daemon state lock"),
            "{error}"
        );
        assert_eq!(fs::read(&path).expect("unchanged audit session"), original);
        let expired_namespace = session_namespace_snapshot(store.sessions_dir());
        assert_ne!(
            expired_namespace, before_namespace,
            "the prepublication pause must retain its recoverable transaction"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            session_namespace_snapshot(store.sessions_dir()),
            expired_namespace,
            "expired append-once cleanup mutated the session namespace later"
        );
        assert!(store
            .load_result(&session.id)
            .expect("load unchanged audit session")
            .expect("audit session")
            .events
            .is_empty());
    }

    #[test]
    fn session_lock_registry_contention_obeys_the_absolute_deadline() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let registry: SessionLockRegistry = Mutex::new(HashMap::new());
        let held_registry = registry.lock().expect("hold session lock registry");
        let path = store.sessions_dir().join("registry-contention.lock");
        let started = Instant::now();

        let error = store
            .process_lock_in(
                &registry,
                &path,
                Some(Instant::now() + Duration::from_millis(75)),
            )
            .expect_err("the session lock registry wait must be bounded");

        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            held_registry.is_empty(),
            "contended registry was not mutated"
        );
    }

    #[test]
    fn deadline_update_times_out_behind_process_lock_without_mutating() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("deadline-session-lock");
        let holder_store = store.clone();
        let holder_id = session.id.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_store
                .update_session(&holder_id, |_current| {
                    held_tx.send(()).expect("signal held session lock");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release held session lock");
                    Ok(())
                })
                .expect("holder session update");
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("session lock is held");

        let error = store
            .update_session_with_deadline(
                &session.id,
                Instant::now() + Duration::from_millis(100),
                |current| {
                    current.summary = Some("must not persist".to_string());
                    Ok(())
                },
            )
            .expect_err("deadline must bound the process lock wait");
        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );

        release_tx.send(()).expect("release session lock");
        holder.join().expect("session lock holder");
        assert_eq!(
            store.load(&session.id).expect("session remains").summary,
            None
        );
    }

    #[test]
    fn configured_lock_timeout_bounds_default_session_updates() {
        let root = tempdir().expect("project");
        let unbounded_store = SessionStore::new(root.path());
        let session = unbounded_store.create_session_with_id("configured-deadline-session");
        let holder_store = unbounded_store.clone();
        let holder_id = session.id.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_store
                .update_session(&holder_id, |_current| {
                    held_tx.send(()).expect("signal held session lock");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release held session lock");
                    Ok(())
                })
                .expect("holder session update");
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("session lock is held");

        let bounded_store = unbounded_store
            .clone()
            .with_lock_timeout(Duration::from_millis(100));
        let started = Instant::now();
        let error = bounded_store
            .update_session(&session.id, |current| {
                current.summary = Some("must not persist".to_string());
                Ok(())
            })
            .expect_err("configured deadline must bound the default update API");
        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        release_tx.send(()).expect("release session lock");
        holder.join().expect("session lock holder");
        assert_eq!(
            unbounded_store
                .load(&session.id)
                .expect("session remains")
                .summary,
            None
        );
    }

    #[test]
    fn configured_lock_timeout_is_shared_across_nested_locks() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("shared-deadline-session");

        let session_holder_store = store.clone();
        let session_holder_id = session.id.clone();
        let (session_held_tx, session_held_rx) = std::sync::mpsc::channel();
        let (release_session_tx, release_session_rx) = std::sync::mpsc::channel();
        let session_holder = std::thread::spawn(move || {
            session_holder_store
                .with_session_lock(&session_holder_id, |_| {
                    session_held_tx.send(()).expect("signal session lock held");
                    release_session_rx
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release session lock");
                    Ok(())
                })
                .expect("hold session lock");
        });
        session_held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("session lock is held");

        let skill_holder_store = store.clone();
        let (skill_held_tx, skill_held_rx) = std::sync::mpsc::channel();
        let (release_skill_tx, release_skill_rx) = std::sync::mpsc::channel();
        let skill_holder = std::thread::spawn(move || {
            skill_holder_store
                .with_skill_usage_lock(|| {
                    skill_held_tx.send(()).expect("signal skill lock held");
                    release_skill_rx
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release skill lock");
                    Ok(())
                })
                .expect("hold skill usage lock");
        });
        skill_held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("skill usage lock is held");

        let delayed_release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            release_skill_tx.send(()).expect("release skill usage lock");
        });
        let bounded_store = store.clone().with_lock_timeout(Duration::from_secs(3));
        let started = Instant::now();
        let error = bounded_store
            .update_session(&session.id, |_current| Ok(()))
            .expect_err("one deadline must cover both nested lock waits");
        let elapsed = started.elapsed();
        assert!(
            error
                .to_string()
                .contains("timed out acquiring daemon state lock"),
            "{error}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "nested locks received separate timeout budgets: {elapsed:?}"
        );

        delayed_release.join().expect("delayed skill lock release");
        skill_holder.join().expect("skill lock holder");
        release_session_tx.send(()).expect("release session lock");
        session_holder.join().expect("session lock holder");
    }

    #[test]
    fn strict_enumeration_uses_the_session_mutation_lock() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("serialized-list-session");
        let holder_store = store.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_store
                .with_skill_usage_lock(|| {
                    held_tx.send(()).expect("signal mutation lock held");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release mutation lock");
                    Ok(())
                })
                .expect("hold mutation lock");
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("mutation lock is held");

        let bounded_store = store.clone().with_lock_timeout(Duration::from_millis(100));
        let started = Instant::now();
        let error = bounded_store
            .list_result()
            .expect_err("strict enumeration must join the mutation lock domain");
        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        release_tx.send(()).expect("release mutation lock");
        holder.join().expect("mutation lock holder");
        assert_eq!(
            store.list_result().expect("strict session enumeration"),
            vec![session.id]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn scoped_lock_policy_propagates_to_spawned_store_without_starving_runtime() {
        let root = tempdir().expect("project");
        let holder_store = SessionStore::new(root.path());
        let session = holder_store.create_session_with_id("scoped-deadline-session");
        let holder_id = session.id.clone();
        let operation_store = SessionStore::new(root.path());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_store
                .update_session(&holder_id, |_current| {
                    held_tx.send(()).expect("signal held session lock");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release held session lock");
                    Ok(())
                })
                .expect("holder session update");
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("session lock is held");

        let operation_id = session.id.clone();
        let operation = tokio::spawn(SessionStore::with_lock_policy(
            Duration::from_millis(200),
            async move {
                let policy = SessionStore::current_lock_policy();
                tokio::spawn(SessionStore::with_optional_lock_policy(
                    policy,
                    async move { operation_store.update_session(&operation_id, |_current| Ok(())) },
                ))
                .await
                .expect("spawned session operation")
            },
        ));
        tokio::time::timeout(Duration::from_millis(100), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        })
        .await
        .expect("the sole async worker remains responsive during a session lock wait");

        let error = operation
            .await
            .expect("scoped operation task")
            .expect_err("scoped lock policy must time out");
        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );

        release_tx.send(()).expect("release session lock");
        holder.join().expect("session lock holder");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn skill_usage_lock_replacement_while_held_is_rejected() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let lock_path = store.sessions_dir().join(SKILL_USAGE_LOCK_FILE);
        let displaced = store.sessions_dir().join("displaced-skill-usage.lock");

        let error = store
            .with_skill_usage_lock(|| {
                fs::rename(&lock_path, &displaced)?;
                fs::write(&lock_path, b"replacement-lock")?;
                Ok(())
            })
            .expect_err("replaced skill usage lock identity must fail");

        assert!(
            error.to_string().contains("different identities"),
            "{error}"
        );
        assert_eq!(
            fs::read(&lock_path).expect("replacement lock"),
            b"replacement-lock"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn whole_session_directory_replacement_is_rejected() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-directory");
        let sessions_dir = store.sessions_dir().to_path_buf();
        let displaced = root.path().join("displaced-sessions");
        #[cfg(unix)]
        let (replacement_path, replacement_bytes) = {
            let replacement_path = sessions_dir.join(format!("{}.json", session.id));
            let mut replacement = Session::new(session.id.clone());
            replacement.summary = Some("replacement-directory".to_string());
            let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");
            (replacement_path, replacement_bytes)
        };

        let error = store
            .update_session(&session.id, |current| {
                #[cfg(unix)]
                {
                    fs::rename(&sessions_dir, &displaced)?;
                    fs::create_dir(&sessions_dir)?;
                    fs::write(&replacement_path, &replacement_bytes)?;
                    current.summary = Some("must-not-publish".to_string());
                    Ok(())
                }
                #[cfg(windows)]
                {
                    let _ = current;
                    let error = fs::rename(&sessions_dir, &displaced)
                        .expect_err("live Windows session lock pins the sessions directory");
                    Err::<(), SessionError>(SessionError::Io(error))
                }
            })
            .expect_err("detached directory must fail");

        #[cfg(unix)]
        {
            assert!(error.to_string().contains("directory"), "{error}");
            assert_eq!(
                fs::read(&replacement_path).expect("replacement session"),
                replacement_bytes
            );
            assert!(store.list_result().is_err());
            assert!(SessionStore::new(root.path())
                .load_result(&session.id)
                .is_err());
        }
        #[cfg(windows)]
        {
            assert!(!error.to_string().is_empty());
            assert!(sessions_dir.is_dir());
            assert!(!displaced.exists());
            assert_eq!(
                store
                    .load_result(&session.id)
                    .expect("original session remains readable")
                    .expect("original session remains present")
                    .summary,
                session.summary
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_lock_artifacts_are_bounded_by_fixed_stripes() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let ids = (0..256)
            .map(|index| format!("bounded-lock-{index}"))
            .collect::<Vec<_>>();
        for id in &ids {
            store
                .try_create_session_with_id(id)
                .expect("create striped session");
        }

        let lock_names = fs::read_dir(store.sessions_dir())
            .expect("list session locks")
            .map(|entry| entry.expect("directory entry").file_name())
            .filter(|name| {
                name.to_string_lossy().starts_with(".session-lock-")
                    && name.to_string_lossy().ends_with(".lock")
            })
            .collect::<Vec<_>>();
        assert!(!lock_names.is_empty());
        assert!(lock_names.len() <= SESSION_LOCK_STRIPES);
        for id in ids {
            assert!(!store.sessions_dir().join(format!(".{id}.lock")).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_store_rejects_symlinked_nib_without_outside_write() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        symlink(outside.path(), root.path().join(".nib")).expect("symlink .nib");
        let store = SessionStore::new(root.path());

        assert!(store.try_create_session_with_id("blocked").is_err());
        assert!(!outside.path().join("sessions").exists());
    }

    #[cfg(windows)]
    #[test]
    fn direct_store_rejects_junctioned_nib_without_outside_write() {
        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        create_directory_junction(&root.path().join(".nib"), outside.path());
        let store = SessionStore::new(root.path());

        assert!(store.try_create_session_with_id("blocked").is_err());
        assert!(!outside.path().join("sessions").exists());
    }

    #[cfg(unix)]
    #[test]
    fn mutation_rechecks_directory_after_constructor_race() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        let store = SessionStore::new(root.path());
        fs::rename(root.path().join(".nib"), root.path().join(".nib-displaced"))
            .expect("displace .nib");
        symlink(outside.path(), root.path().join(".nib")).expect("swap state ancestor");

        assert!(store.try_create_session_with_id("raced").is_err());
        assert!(!outside.path().join("sessions").exists());
    }

    #[cfg(unix)]
    fn run_session_commit_child(root: &Path) {
        let id = std::env::var(SESSION_COMMIT_CHILD_ID)
            .expect("session commit child id must be configured");
        let mode = std::env::var(SESSION_COMMIT_CHILD_MODE)
            .expect("session commit child mode must be configured");
        let ready = PathBuf::from(
            std::env::var_os(SESSION_COMMIT_CHILD_READY)
                .expect("session commit child ready path must be configured"),
        );
        let release = std::env::var_os(SESSION_COMMIT_CHILD_RELEASE).map(PathBuf::from);
        let store = SessionStore::new(root);
        let result = store.with_skill_usage_lock(|| {
            store.with_session_lock(&id, |directory| {
                let mut opened = store
                    .load_opened_unlocked(directory, &id)?
                    .ok_or_else(|| SessionError::NotFound(id.clone()))?;
                opened.session.summary = Some("child must not publish".to_string());
                opened.session.revision =
                    opened.session.revision.checked_add(1).ok_or_else(|| {
                        SessionError::InvalidMutation("session revision overflowed".to_string())
                    })?;
                store.save_unlocked_with_commit_check(
                    directory,
                    &opened.session,
                    Some(&opened.file),
                    || {
                        fs::write(&ready, b"ready")?;
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(30);
                        loop {
                            if release.as_ref().is_some_and(|path| path.exists()) {
                                return Ok(());
                            }
                            if std::time::Instant::now() >= deadline {
                                return Err(SessionError::InvalidMutation(
                                    "session commit child timed out".to_string(),
                                ));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    },
                )
            })
        });

        match mode.as_str() {
            "replace" => {
                let error = result.expect_err("commit-barrier replacement must fail closed");
                assert!(error.to_string().contains("identity changed"), "{error}");
            }
            "kill" => panic!("session crash child unexpectedly left its commit barrier"),
            value => panic!("unsupported session commit child mode: {value}"),
        }
    }

    #[cfg(unix)]
    fn spawn_session_commit_child(
        root: &Path,
        id: &str,
        mode: &str,
        ready: &Path,
        release: Option<&Path>,
    ) -> std::process::Child {
        let _ = fs::remove_file(ready);
        if let Some(release) = release {
            let _ = fs::remove_file(release);
        }
        let mut command = std::process::Command::new(
            std::env::current_exe().expect("current session test binary"),
        );
        command
            .args([
                "--exact",
                "session::tests::real_child_session_commit_barrier_and_fsync_crash_recovery",
                "--nocapture",
            ])
            .env(SESSION_COMMIT_CHILD_ROOT, root)
            .env(SESSION_COMMIT_CHILD_ID, id)
            .env(SESSION_COMMIT_CHILD_MODE, mode)
            .env(SESSION_COMMIT_CHILD_READY, ready);
        if let Some(release) = release {
            command.env(SESSION_COMMIT_CHILD_RELEASE, release);
        }
        command.spawn().expect("spawn session commit child")
    }

    #[cfg(unix)]
    fn wait_for_session_commit_child(child: &mut std::process::Child, ready: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect session commit child") {
                panic!("session commit child exited before readiness: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session commit child did not become ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn session_temporary_paths(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("list session directory")
            .map(|entry| entry.expect("session directory entry").path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".nib-session-") && name.ends_with(".tmp")
                })
            })
            .collect()
    }
}
