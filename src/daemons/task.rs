//! Background task and timer manager.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio::time::sleep;

const MAX_DAEMON_AUDIT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DAEMON_AUDIT_RECORD_BYTES: usize = 1024 * 1024;
const MAX_IN_MEMORY_TASKS: usize = 10_000;
const MAX_TASK_ID_BYTES: usize = 160;
const MAX_TASK_KIND_BYTES: usize = 64;

/// Represents a running background task or timer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackgroundTask {
    pub id: String,
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

struct ManagedTask {
    record: BackgroundTask,
    abort_handle: Option<AbortHandle>,
    start_sender: Option<oneshot::Sender<()>>,
    durable_store: Option<crate::daemons::workload::DurableTaskStore>,
}

#[derive(Clone)]
pub struct SessionTimerRequest {
    pub id: String,
    pub initial_delay: Duration,
    pub interval: Duration,
    pub repeat_count: u32,
    pub prompt: String,
    pub session_store: crate::session::SessionStore,
    pub session_id: String,
    pub audit_log: DaemonAuditLog,
}

#[derive(Clone)]
pub struct BackgroundTaskSession {
    pub session_store: crate::session::SessionStore,
    pub session_id: String,
    pub audit_log: DaemonAuditLog,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonAuditRecord {
    pub timestamp: DateTime<Utc>,
    pub daemon: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub outcome: String,
    pub authorized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DaemonAuditLog {
    path: PathBuf,
}

impl DaemonAuditLog {
    pub fn new(project_root: &Path) -> Self {
        Self::at_path(
            project_root
                .join(".nib")
                .join("daemons")
                .join("audit.jsonl"),
        )
    }

    pub fn at_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &DaemonAuditRecord) -> Result<(), String> {
        let lock_path = audit_lock_path(&self.path);
        crate::daemons::state::with_file_lock(&lock_path, |directory| {
            self.append_unlocked(directory, record)
        })
    }

    pub(crate) fn append_once_for_detail_key(
        &self,
        record: &DaemonAuditRecord,
        detail_key: &str,
        equivalent_outcomes: &[&str],
    ) -> Result<(), String> {
        self.append_once_for_detail_key_with_legacy(record, detail_key, None, equivalent_outcomes)
    }

    pub(crate) fn append_once_for_detail_key_with_legacy(
        &self,
        record: &DaemonAuditRecord,
        detail_key: &str,
        legacy_detail_key: Option<&str>,
        equivalent_outcomes: &[&str],
    ) -> Result<(), String> {
        let lock_path = audit_lock_path(&self.path);
        crate::daemons::state::with_file_lock(&lock_path, |directory| {
            let already_recorded = self.read_all_unlocked(directory)?.iter().any(|existing| {
                existing.daemon == record.daemon
                    && existing.action == record.action
                    && existing.target == record.target
                    && equivalent_outcomes.contains(&existing.outcome.as_str())
                    && existing.detail.as_deref().is_some_and(|detail| {
                        audit_detail_has_key(detail, detail_key)
                            || legacy_detail_key.is_some_and(|legacy_key| {
                                !audit_detail_has_field(detail, "execution_id")
                                    && audit_detail_has_key(detail, legacy_key)
                            })
                    })
            });
            if already_recorded {
                return Ok(());
            }
            self.append_unlocked(directory, record)
        })
    }

    pub fn read_all(&self) -> Result<Vec<DaemonAuditRecord>, String> {
        crate::daemons::state::with_file_lock(&audit_lock_path(&self.path), |directory| {
            self.read_all_unlocked(directory)
        })
    }

    fn append_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        record: &DaemonAuditRecord,
    ) -> Result<(), String> {
        let was_missing = !directory.path_exists(&self.path)?;
        let mut encoded = serde_json::to_vec(record).map_err(|e| e.to_string())?;
        encoded.push(b'\n');
        if encoded.len() > MAX_DAEMON_AUDIT_RECORD_BYTES {
            return Err(format!(
                "daemon audit record exceeds the {MAX_DAEMON_AUDIT_RECORD_BYTES}-byte limit"
            ));
        }
        let mut file = directory.open_append_create(&self.path)?;
        let opened_metadata = file.metadata().map_err(|error| error.to_string())?;
        validate_audit_metadata(&self.path, &opened_metadata)?;
        let final_size = opened_metadata
            .len()
            .checked_add(encoded.len() as u64)
            .ok_or_else(|| "daemon audit size overflow".to_string())?;
        if final_size > MAX_DAEMON_AUDIT_BYTES {
            return Err(format!(
                "daemon audit file {} cannot exceed the {}-byte limit",
                self.path.display(),
                MAX_DAEMON_AUDIT_BYTES
            ));
        }
        directory.verify_file_identity(&self.path, &file)?;
        file.write_all(&encoded).map_err(|e| e.to_string())?;
        file.sync_data().map_err(|e| e.to_string())?;
        directory.verify_file_identity(&self.path, &file)?;
        if was_missing {
            directory.sync_directory()?;
        }
        Ok(())
    }

    fn read_all_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
    ) -> Result<Vec<DaemonAuditRecord>, String> {
        if !directory.path_exists(&self.path)? {
            return Ok(Vec::new());
        }
        let file = directory.open_read(&self.path)?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        validate_audit_metadata(&self.path, &metadata)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&file)
            .take(MAX_DAEMON_AUDIT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_DAEMON_AUDIT_BYTES {
            return Err(format!(
                "daemon audit file {} exceeds the {}-byte limit",
                self.path.display(),
                MAX_DAEMON_AUDIT_BYTES
            ));
        }
        directory.verify_file_identity(&self.path, &file)?;
        let contents = String::from_utf8(bytes)
            .map_err(|error| format!("daemon audit file is not UTF-8: {error}"))?;
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(|e| e.to_string()))
            .collect()
    }
}

fn audit_detail_has_key(detail: &str, key: &str) -> bool {
    key.split(';').all(|key_field| {
        let key_field = key_field.trim();
        detail
            .split(';')
            .any(|detail_field| detail_field.trim() == key_field)
    })
}

fn audit_detail_has_field(detail: &str, field: &str) -> bool {
    detail.split(';').any(|detail_field| {
        detail_field
            .trim()
            .split_once('=')
            .is_some_and(|(name, _)| name.trim() == field)
    })
}

fn execution_id_matches(details: &Value, execution_id: &str) -> bool {
    match details.get("execution_id").and_then(Value::as_str) {
        Some(existing) => existing == execution_id,
        None => execution_id.starts_with("legacy-"),
    }
}

fn validate_audit_metadata(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "daemon audit file must be a regular local file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_DAEMON_AUDIT_BYTES {
        return Err(format!(
            "daemon audit file {} is {} bytes; maximum is {} bytes",
            path.display(),
            metadata.len(),
            MAX_DAEMON_AUDIT_BYTES
        ));
    }
    Ok(())
}

fn audit_lock_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "audit.jsonl".into());
    file_name.push(".lock");
    path.with_file_name(file_name)
}

#[derive(Clone)]
pub struct TaskManager {
    tasks: Arc<Mutex<HashMap<String, ManagedTask>>>,
    max_tasks: usize,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            max_tasks: MAX_IN_MEMORY_TASKS,
        }
    }

    #[cfg(test)]
    fn with_task_limit(max_tasks: usize) -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            max_tasks,
        }
    }

    pub fn register_task(&self, id: String, kind: impl Into<String>) -> Result<(), String> {
        self.register_task_with_start(id, kind, None)
    }

    #[cfg(test)]
    pub(crate) fn rollback_unattached_task(&self, id: &str) -> Result<(), String> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| "background task state lock poisoned".to_string())?;
        let task = tasks
            .get(id)
            .ok_or_else(|| format!("background task not found: {id}"))?;
        if task.abort_handle.is_some()
            || task.durable_store.is_some()
            || task.start_sender.is_some()
        {
            return Err(format!(
                "background task cannot be rolled back after execution ownership is attached: {id}"
            ));
        }
        tasks.remove(id);
        Ok(())
    }

    pub fn register_paused_task(
        &self,
        id: String,
        kind: impl Into<String>,
    ) -> Result<oneshot::Receiver<()>, String> {
        let (sender, receiver) = oneshot::channel();
        self.register_task_with_start(id, kind, Some(sender))?;
        Ok(receiver)
    }

    fn register_task_with_start(
        &self,
        id: String,
        kind: impl Into<String>,
        start_sender: Option<oneshot::Sender<()>>,
    ) -> Result<(), String> {
        let kind = kind.into();
        validate_task_fields(&id, &kind)?;
        let now = Utc::now();
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| "background task state lock poisoned".to_string())?;
        if tasks.len() >= self.max_tasks {
            prune_terminal_tasks(&mut tasks);
        }
        if tasks.contains_key(&id) {
            return Err(format!("background task already exists: {id}"));
        }
        if tasks.len() >= self.max_tasks {
            return Err(format!(
                "background tasks exceed the {}-task limit",
                self.max_tasks
            ));
        }
        tasks.insert(
            id.clone(),
            ManagedTask {
                record: BackgroundTask {
                    id,
                    kind,
                    status: "running".to_string(),
                    result: None,
                    error: None,
                    created_at: now,
                    updated_at: now,
                },
                abort_handle: None,
                start_sender,
                durable_store: None,
            },
        );
        Ok(())
    }

    pub fn register_durable_task(
        &self,
        id: String,
        store: crate::daemons::workload::DurableTaskStore,
    ) -> Result<(), String> {
        if let Err(registration_error) = self.try_register_durable_task(&id, &store) {
            let existing_store = self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .and_then(|task| task.durable_store.clone());
            if existing_store.is_some_and(|existing| existing.same_store(&store)) {
                return Err(registration_error);
            }
            return Err(rollback_failed_durable_registration(
                &store,
                &id,
                registration_error,
            ));
        }
        Ok(())
    }

    fn try_register_durable_task(
        &self,
        id: &str,
        store: &crate::daemons::workload::DurableTaskStore,
    ) -> Result<(), String> {
        let record = store
            .get(id)?
            .ok_or_else(|| format!("background task not found after persistence: {id}"))?;
        validate_task_fields(&record.id, &record.kind)?;
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| "background task state lock poisoned".to_string())?;
        if tasks.len() >= self.max_tasks {
            prune_terminal_tasks(&mut tasks);
        }
        if tasks.contains_key(id) {
            return Err(format!("background task already exists: {id}"));
        }
        if tasks.len() >= self.max_tasks {
            return Err(format!(
                "background tasks exceed the {}-task limit",
                self.max_tasks
            ));
        }
        tasks.insert(
            id.to_string(),
            ManagedTask {
                record: BackgroundTask {
                    id: record.id,
                    kind: record.kind,
                    status: record.status,
                    result: record.result,
                    error: record.error,
                    created_at: record.created_at,
                    updated_at: record.updated_at,
                },
                abort_handle: None,
                start_sender: None,
                durable_store: Some(store.clone()),
            },
        );
        Ok(())
    }

    pub fn rollback_prepared_durable_task(&self, id: &str) -> Result<(), String> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| "background task state lock poisoned".to_string())?;
        let store = tasks
            .get(id)
            .ok_or_else(|| format!("background task not found: {id}"))?
            .durable_store
            .clone()
            .ok_or_else(|| format!("background task is not durable: {id}"))?;
        if !store.remove_prepared(id)? {
            return Err(format!(
                "prepared background task disappeared during rollback: {id}"
            ));
        }
        tasks.remove(id);
        Ok(())
    }

    pub fn fail_prepared_durable_task(
        &self,
        id: &str,
        error: String,
    ) -> Result<crate::daemons::workload::DurableTaskRecord, String> {
        let store = self
            .tasks
            .lock()
            .map_err(|_| "background task state lock poisoned".to_string())?
            .get(id)
            .ok_or_else(|| format!("background task not found: {id}"))?
            .durable_store
            .clone()
            .ok_or_else(|| format!("background task is not durable: {id}"))?;
        let record = store.fail_prepared(id, error)?;
        if record.status != "failed" {
            return Err(format!(
                "prepared background task {id} was not terminalized (status: {})",
                record.status
            ));
        }
        Ok(record)
    }

    pub fn compensate_prepared_task(&self, id: &str, error: String) -> Result<(), String> {
        let durable_store = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .ok_or_else(|| format!("background task not found: {id}"))?
            .durable_store
            .clone();

        let Some(store) = durable_store else {
            self.fail(id, error, None);
            return Ok(());
        };
        let compensation = match store.fail_prepared(id, error.clone()) {
            Ok(record) if record.status == "failed" => return Ok(()),
            Ok(record) => format!(
                "prepared background task {id} was not terminalized (status: {})",
                record.status
            ),
            Err(compensation) => compensation,
        };
        match store.audit_compensation_failure(
            id,
            "prepared_task_compensation",
            &error,
            &compensation,
        ) {
            Ok(()) => Err(format!(
                "{compensation}; compensation failure was recorded in the daemon audit"
            )),
            Err(audit_error) => Err(format!(
                "{compensation}; failed to persist compensation audit: {audit_error}"
            )),
        }
    }

    pub fn start_task(&self, id: &str) -> Result<(), String> {
        let durable_store = {
            let tasks = self
                .tasks
                .lock()
                .map_err(|_| "background task state lock poisoned".to_string())?;
            tasks
                .get(id)
                .ok_or_else(|| format!("background task not found: {id}"))?
                .durable_store
                .clone()
        };
        if let Some(store) = durable_store {
            store.start(id)?;
            return Ok(());
        }

        let sender = {
            let mut tasks = self
                .tasks
                .lock()
                .map_err(|_| "background task state lock poisoned".to_string())?;
            let task = tasks
                .get_mut(id)
                .ok_or_else(|| format!("background task not found: {id}"))?;
            if task.record.status != "running" {
                return Err(format!(
                    "background task {id} cannot start (status: {})",
                    task.record.status
                ));
            }
            task.start_sender
                .take()
                .ok_or_else(|| format!("background task {id} has no pending start gate"))?
        };
        sender
            .send(())
            .map_err(|_| format!("background task {id} dropped its start gate"))
    }

    pub fn attach_abort_handle(&self, id: &str, handle: AbortHandle) -> Result<(), String> {
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| "background task state lock poisoned".to_string())?;
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| format!("background task not found: {id}"))?;
        if task.record.status == "running" {
            task.abort_handle = Some(handle);
        }
        Ok(())
    }

    /// Spawns a timer that delivers a synthetic user message to its originating session.
    pub fn spawn_session_timer(&self, request: SessionTimerRequest) -> Result<(), String> {
        let SessionTimerRequest {
            id,
            initial_delay,
            interval,
            repeat_count,
            prompt,
            session_store,
            session_id,
            audit_log,
        } = request;
        if repeat_count == 0 || repeat_count > 100 {
            return Err("timer repeat_count must be between 1 and 100".to_string());
        }
        self.register_task(id.clone(), "timer")?;
        let manager = self.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            let mut deliveries = Vec::with_capacity(repeat_count as usize);
            for occurrence in 1..=repeat_count {
                sleep(if occurrence == 1 {
                    initial_delay
                } else {
                    interval
                })
                .await;
                match deliver_timer_message(
                    &task_id,
                    occurrence,
                    repeat_count,
                    &prompt,
                    &session_store,
                    &session_id,
                    &audit_log,
                ) {
                    Ok(result) => {
                        deliveries.push(result);
                        manager.update_progress(
                            &task_id,
                            json!({
                                "delivered_count": occurrence,
                                "repeat_count": repeat_count,
                                "deliveries": deliveries.clone(),
                            }),
                        );
                    }
                    Err(error) => {
                        let _ = session_store.record_event(
                            &session_id,
                            "timer_failed",
                            json!({
                                "timer_id": task_id,
                                "occurrence": occurrence,
                                "repeat_count": repeat_count,
                                "error": error.clone(),
                            }),
                        );
                        let audit_error = audit_log
                            .append(&DaemonAuditRecord {
                                timestamp: Utc::now(),
                                daemon: "timer".to_string(),
                                action: "deliver_message".to_string(),
                                target: Some(session_id.clone()),
                                outcome: "failed".to_string(),
                                authorized: true,
                                detail: Some(format!(
                                    "timer_id={task_id}; occurrence={occurrence}/{repeat_count}; error={error}"
                                )),
                            })
                            .err();
                        let error = match audit_error {
                            Some(audit_error) => {
                                format!("{error}; failed to record daemon audit: {audit_error}")
                            }
                            None => error,
                        };
                        manager.fail(
                            &task_id,
                            error,
                            Some(json!({
                                "delivered_count": occurrence - 1,
                                "repeat_count": repeat_count,
                                "deliveries": deliveries,
                            })),
                        );
                        return;
                    }
                }
            }
            manager.complete(
                &task_id,
                Some(json!({
                    "delivered_count": repeat_count,
                    "repeat_count": repeat_count,
                    "deliveries": deliveries,
                })),
            );
        });
        self.attach_abort_handle(&id, handle.abort_handle())
    }

    pub fn register_subagent(&self, id: String) {
        let _ = self.register_task(id, "subagent");
    }

    pub fn mark_completed(&self, id: &str) {
        self.complete(id, None);
    }

    pub fn complete(&self, id: &str, result: Option<Value>) {
        self.update_finished(id, "completed", result, None);
    }

    pub fn fail(&self, id: &str, error: String, result: Option<Value>) {
        self.update_finished(id, "failed", result, Some(error));
    }

    fn update_progress(&self, id: &str, result: Value) {
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(task) = tasks.get_mut(id) {
                if task.record.status == "running" {
                    task.record.result = Some(result);
                    task.record.updated_at = Utc::now();
                }
            }
        }
    }

    fn update_finished(
        &self,
        id: &str,
        status: &str,
        result: Option<Value>,
        error: Option<String>,
    ) {
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(task) = tasks.get_mut(id) {
                if let Some(store) = &task.durable_store {
                    if status == "failed" {
                        let _ = store.fail_prepared(
                            id,
                            error
                                .clone()
                                .unwrap_or_else(|| "background task failed".to_string()),
                        );
                    }
                    return;
                }
                if task.record.status != "running" {
                    return;
                }
                task.record.status = status.to_string();
                task.record.result = result;
                task.record.error = error;
                task.record.updated_at = Utc::now();
                task.abort_handle = None;
                task.start_sender = None;
            }
        }
    }

    pub fn cancel(&self, id: &str) -> Result<Value, String> {
        let durable_store = self
            .tasks
            .lock()
            .map_err(|_| "background task state lock poisoned".to_string())?
            .get(id)
            .and_then(|task| task.durable_store.clone());
        if let Some(store) = durable_store {
            return serde_json::to_value(store.cancel(id)?)
                .map_err(|error| format!("failed to encode durable task: {error}"));
        }
        let abort_handle = {
            let mut tasks = self
                .tasks
                .lock()
                .map_err(|_| "background task state lock poisoned".to_string())?;
            let task = tasks
                .get_mut(id)
                .ok_or_else(|| format!("background task not found: {id}"))?;
            if task.record.status != "running" {
                return Err(format!(
                    "background task {id} is not running (status: {})",
                    task.record.status
                ));
            }
            let handle = task
                .abort_handle
                .take()
                .ok_or_else(|| format!("background task {id} cannot be cancelled safely"))?;
            task.record.status = "cancelled".to_string();
            task.record.error = Some("cancelled by user".to_string());
            task.record.updated_at = Utc::now();
            task.start_sender = None;
            handle
        };
        abort_handle.abort();
        self.get_task(id)
            .ok_or_else(|| format!("background task not found after cancellation: {id}"))
    }

    pub fn get_status(&self, id: &str) -> Option<String> {
        let tasks = self.tasks.lock().ok()?;
        let task = tasks.get(id)?;
        if let Some(store) = &task.durable_store {
            return store.get(id).ok().flatten().map(|record| record.status);
        }
        Some(task.record.status.clone())
    }

    pub fn get_task(&self, id: &str) -> Option<Value> {
        let tasks = self.tasks.lock().ok()?;
        let task = tasks.get(id)?;
        if let Some(store) = &task.durable_store {
            return store
                .get(id)
                .ok()
                .flatten()
                .and_then(|record| serde_json::to_value(record).ok());
        }
        serde_json::to_value(&task.record).ok()
    }

    pub fn list_tasks(&self) -> Vec<Value> {
        let task_state = self.tasks.lock().unwrap();
        let mut tasks: Vec<_> = task_state
            .values()
            .filter_map(|task| {
                task.durable_store
                    .as_ref()
                    .and_then(|store| store.get(&task.record.id).ok().flatten())
                    .and_then(|record| serde_json::to_value(record).ok())
                    .or_else(|| serde_json::to_value(&task.record).ok())
            })
            .collect();
        tasks.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
        tasks
    }
}

fn prune_terminal_tasks(tasks: &mut HashMap<String, ManagedTask>) {
    tasks.retain(|id, task| {
        if let Some(store) = &task.durable_store {
            return match store.get(id) {
                Ok(Some(record)) => {
                    !matches!(record.status.as_str(), "completed" | "failed" | "cancelled")
                }
                Ok(None) => false,
                Err(_) => true,
            };
        }
        !matches!(
            task.record.status.as_str(),
            "completed" | "failed" | "cancelled"
        )
    });
}

fn rollback_failed_durable_registration(
    store: &crate::daemons::workload::DurableTaskStore,
    id: &str,
    registration_error: String,
) -> String {
    match store.remove_prepared(id) {
        Ok(_) => registration_error,
        Err(rollback_error) => {
            let failure = store.fail_prepared(
                id,
                format!("in-memory durable task admission failed: {registration_error}"),
            );
            match failure {
                Ok(record) if record.status == "failed" => format!(
                    "{registration_error}; failed to remove prepared task during rollback: {rollback_error}; task was marked failed"
                ),
                Ok(record) => format!(
                    "{registration_error}; failed to remove prepared task during rollback: {rollback_error}; task remained {}",
                    record.status
                ),
                Err(failure_error) => format!(
                    "{registration_error}; failed to remove prepared task during rollback: {rollback_error}; failed to mark task failed: {failure_error}"
                ),
            }
        }
    }
}

fn validate_task_fields(id: &str, kind: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > MAX_TASK_ID_BYTES
        || !id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(format!(
            "background task id must be non-empty, at most {MAX_TASK_ID_BYTES} bytes, and path-safe"
        ));
    }
    if kind.trim().is_empty() || kind.len() > MAX_TASK_KIND_BYTES {
        return Err(format!(
            "background task kind must be non-empty and at most {MAX_TASK_KIND_BYTES} bytes"
        ));
    }
    Ok(())
}

fn deliver_timer_message(
    timer_id: &str,
    occurrence: u32,
    repeat_count: u32,
    prompt: &str,
    store: &crate::session::SessionStore,
    session_id: &str,
    audit_log: &DaemonAuditLog,
) -> Result<Value, String> {
    store
        .update_session(session_id, |session| {
            match session.messages.last().map(|message| message.role.as_str()) {
                None | Some("assistant") => {}
                Some("user" | "tool") => session.messages.push(crate::session::SessionMessage {
                    index: session.messages.len(),
                    role: "assistant".to_string(),
                    content: json!({
                        "type": "scheduled_timer_boundary",
                        "timer_id": timer_id,
                        "occurrence": occurrence,
                    })
                    .to_string(),
                    timestamp: Some(Utc::now()),
                }),
                Some(role) => {
                    return Err(crate::session::SessionError::InvalidMutation(format!(
                        "invalid final session role before timer delivery: {role}"
                    )))
                }
            }
            session.messages.push(crate::session::SessionMessage {
                index: session.messages.len(),
                role: "user".to_string(),
                content: prompt.to_string(),
                timestamp: Some(Utc::now()),
            });
            session.events.push(crate::session::SessionEvent {
                index: session.events.len(),
                kind: "timer_fired".to_string(),
                details: json!({
                    "timer_id": timer_id,
                    "occurrence": occurrence,
                    "repeat_count": repeat_count,
                    "prompt": prompt,
                }),
                timestamp: Some(Utc::now()),
            });
            Ok(())
        })
        .map_err(|error| format!("failed to deliver timer message: {error}"))?;
    audit_log.append(&DaemonAuditRecord {
        timestamp: Utc::now(),
        daemon: "timer".to_string(),
        action: "deliver_message".to_string(),
        target: Some(session_id.to_string()),
        outcome: "delivered".to_string(),
        authorized: true,
        detail: Some(format!(
            "timer_id={timer_id}; occurrence={occurrence}/{repeat_count}"
        )),
    })?;
    Ok(json!({
        "timer_id": timer_id,
        "session_id": session_id,
        "occurrence": occurrence,
        "delivered": true,
    }))
}

pub static TASK_MANAGER: std::sync::LazyLock<TaskManager> =
    std::sync::LazyLock::new(TaskManager::new);

#[derive(Debug, Clone)]
struct BackgroundObservation {
    success: bool,
    error: Option<String>,
    legacy_generation: bool,
}

pub fn deliver_background_task_observation(
    target: &BackgroundTaskSession,
    task_id: &str,
    execution_id: &str,
    success: bool,
    result: Option<&Value>,
    error: Option<&str>,
) -> Result<(), String> {
    let delivery = target
        .session_store
        .update_session(&target.session_id, |session| {
            if let Some(observation) =
                existing_background_observation(session, task_id, execution_id)
            {
                return Ok(observation);
            }
            reconcile_background_tool_call(session, task_id, execution_id, success, result, error)
                .map_err(crate::session::SessionError::InvalidMutation)?;
            let boundary = || crate::session::SessionMessage {
                index: 0,
                role: "assistant".to_string(),
                content: json!({
                    "type": "background_task_boundary",
                    "task_id": task_id,
                    "execution_id": execution_id,
                })
                .to_string(),
                timestamp: Some(Utc::now()),
            };
            match session.messages.last().map(|message| message.role.as_str()) {
                None => {
                    session.messages.push(crate::session::SessionMessage {
                        index: session.messages.len(),
                        role: "user".to_string(),
                        content: json!({
                            "type": "background_task_context",
                            "task_id": task_id,
                            "execution_id": execution_id,
                        })
                        .to_string(),
                        timestamp: Some(Utc::now()),
                    });
                    let mut message = boundary();
                    message.index = session.messages.len();
                    session.messages.push(message);
                }
                Some("assistant") => {}
                Some("user" | "tool") => {
                    let mut message = boundary();
                    message.index = session.messages.len();
                    session.messages.push(message);
                }
                Some(role) => {
                    return Err(crate::session::SessionError::InvalidMutation(format!(
                        "invalid final session role before background task delivery: {role}"
                    )))
                }
            }
            session.messages.push(crate::session::SessionMessage {
                index: session.messages.len(),
                role: "tool".to_string(),
                content: json!({
                    "type": "background_task_result",
                    "task_id": task_id,
                    "execution_id": execution_id,
                    "success": success,
                    "result": result,
                    "error": error,
                })
                .to_string(),
                timestamp: Some(Utc::now()),
            });
            session.events.push(crate::session::SessionEvent {
                index: session.events.len(),
                kind: if success {
                    "background_task_completed"
                } else {
                    "background_task_failed"
                }
                .to_string(),
                details: json!({
                    "task_id": task_id,
                    "execution_id": execution_id,
                    "success": success,
                    "exit_code": result.and_then(|value| value.get("exit_code")),
                    "error": error,
                }),
                timestamp: Some(Utc::now()),
            });
            Ok(BackgroundObservation {
                success,
                error: error.map(str::to_string),
                legacy_generation: false,
            })
        })
        .map_err(|error| format!("failed to deliver background task observation: {error}"));

    let detail_key = format!("task_id={task_id}; execution_id={execution_id}");
    let (audit_outcome, audit_detail, equivalent_outcomes): (&str, String, &[&str]) =
        match &delivery {
            Err(delivery_error) => (
                "delivery_failed",
                format!("{detail_key}; delivery_error={delivery_error}"),
                &["delivery_failed"],
            ),
            Ok(observation) if observation.success => {
                ("completed", detail_key.clone(), &["completed", "failed"])
            }
            Ok(observation) => (
                "failed",
                observation.error.as_ref().map_or_else(
                    || detail_key.clone(),
                    |error| format!("{detail_key}; error={error}"),
                ),
                &["completed", "failed"],
            ),
        };
    let audit_error = target
        .audit_log
        .append_once_for_detail_key_with_legacy(
            &DaemonAuditRecord {
                timestamp: Utc::now(),
                daemon: "background_task".to_string(),
                action: "deliver_observation".to_string(),
                target: Some(target.session_id.clone()),
                outcome: audit_outcome.to_string(),
                authorized: true,
                detail: Some(audit_detail),
            },
            &detail_key,
            delivery
                .as_ref()
                .ok()
                .filter(|observation| observation.legacy_generation)
                .map(|_| format!("task_id={task_id}"))
                .as_deref(),
            equivalent_outcomes,
        )
        .err();

    match (delivery, audit_error) {
        (Ok(_), None) => Ok(()),
        (Err(error), None) => Err(error),
        (Ok(_), Some(audit_error)) => Err(format!(
            "failed to record background task audit: {audit_error}"
        )),
        (Err(error), Some(audit_error)) => Err(format!(
            "{error}; failed to record background task audit: {audit_error}"
        )),
    }
}

fn existing_background_observation(
    session: &crate::session::Session,
    task_id: &str,
    execution_id: &str,
) -> Option<BackgroundObservation> {
    session.events.iter().rev().find_map(|event| {
        if !matches!(
            event.kind.as_str(),
            "background_task_completed" | "background_task_failed"
        ) || event.details.get("task_id").and_then(Value::as_str) != Some(task_id)
            || !execution_id_matches(&event.details, execution_id)
        {
            return None;
        }
        Some(BackgroundObservation {
            success: event
                .details
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(event.kind == "background_task_completed"),
            error: event
                .details
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string),
            legacy_generation: event.details.get("execution_id").is_none(),
        })
    })
}

fn reconcile_background_tool_call(
    session: &mut crate::session::Session,
    task_id: &str,
    execution_id: &str,
    success: bool,
    final_output: Option<&Value>,
    error: Option<&str>,
) -> Result<(), String> {
    let record = session
        .tool_calls
        .iter_mut()
        .rev()
        .find(|record| {
            record
                .result
                .as_ref()
                .and_then(|result| result.get("output"))
                .and_then(|output| output.get("task_id"))
                .and_then(Value::as_str)
                == Some(task_id)
                && record
                    .result
                    .as_ref()
                    .and_then(|result| result.get("output"))
                    .is_some_and(|output| execution_id_matches(output, execution_id))
        })
        .ok_or_else(|| format!("background tool-call audit record not found: {task_id}"))?;
    let result = record
        .result
        .as_mut()
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("background tool-call audit record is invalid: {task_id}"))?;
    result.insert("success".to_string(), Value::Bool(success));
    result.insert(
        "output".to_string(),
        final_output.cloned().unwrap_or(Value::Null),
    );
    result.insert("error".to_string(), json!(error));
    record.error = error.map(str::to_string);
    if let Some(output) = final_output {
        record.duration_seconds = output.get("duration").and_then(Value::as_f64);
        record.provider = output
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_string);
        record.sandbox_profile = output
            .get("sandbox_profile")
            .and_then(Value::as_str)
            .map(str::to_string);
        record.bwrap_args = output
            .get("bwrap_args")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        record.boundaries = output
            .get("boundaries")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStore;
    use tempfile::tempdir;

    #[test]
    fn daemon_audit_log_roundtrips_records() {
        let dir = tempdir().expect("tempdir");
        let log = DaemonAuditLog::new(dir.path());
        let record = DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "curator".to_string(),
            action: "delete_session".to_string(),
            target: Some("session-1".to_string()),
            outcome: "skipped".to_string(),
            authorized: false,
            detail: Some("policy denied cleanup".to_string()),
        };

        log.append(&record).expect("append audit record");
        assert_eq!(log.read_all().expect("read audit log"), vec![record]);
    }

    #[test]
    fn daemon_audit_log_serializes_concurrent_appenders() {
        let dir = tempdir().expect("tempdir");
        let log = DaemonAuditLog::new(dir.path());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for worker in 0..8 {
            let log = log.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for entry in 0..50 {
                    log.append(&DaemonAuditRecord {
                        timestamp: Utc::now(),
                        daemon: "concurrent".to_string(),
                        action: "append".to_string(),
                        target: Some(format!("{worker}-{entry}")),
                        outcome: "recorded".to_string(),
                        authorized: true,
                        detail: None,
                    })
                    .expect("append audit record");
                }
            }));
        }
        for thread in threads {
            thread.join().expect("audit writer");
        }

        let records = log.read_all().expect("read concurrent audit");
        assert_eq!(records.len(), 400);
        let targets: std::collections::BTreeSet<_> = records
            .iter()
            .filter_map(|record| record.target.clone())
            .collect();
        assert_eq!(targets.len(), 400);
    }

    #[test]
    fn daemon_audit_read_rejects_oversized_file_before_allocation() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("audit.jsonl");
        std::fs::File::create(&path)
            .and_then(|file| file.set_len(MAX_DAEMON_AUDIT_BYTES + 1))
            .expect("oversized sparse audit");
        let log = DaemonAuditLog::at_path(path);

        let error = log
            .read_all()
            .expect_err("oversized audit must fail before reading");
        assert!(error.contains("maximum is"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn daemon_audit_rejects_symlink_file_for_reads_and_appends() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().expect("tempdir");
        let outside = dir.path().join("outside.jsonl");
        std::fs::write(&outside, "").expect("outside audit");
        let path = dir.path().join("audit.jsonl");
        symlink(&outside, &path).expect("audit symlink");
        let log = DaemonAuditLog::at_path(path);
        let record = DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "test".to_string(),
            action: "append".to_string(),
            target: None,
            outcome: "blocked".to_string(),
            authorized: false,
            detail: None,
        };

        assert!(log.read_all().is_err());
        assert!(log.append(&record).is_err());
        assert!(std::fs::read(&outside).unwrap().is_empty());
    }

    #[test]
    fn daemon_audit_append_never_crosses_the_read_limit() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("audit.jsonl");
        std::fs::File::create(&path)
            .and_then(|file| file.set_len(MAX_DAEMON_AUDIT_BYTES - 1))
            .expect("near-cap audit");
        let log = DaemonAuditLog::at_path(path.clone());

        let error = log
            .append(&DaemonAuditRecord {
                timestamp: Utc::now(),
                daemon: "test".to_string(),
                action: "append".to_string(),
                target: None,
                outcome: "blocked".to_string(),
                authorized: false,
                detail: None,
            })
            .expect_err("append must preserve readable size ceiling");

        assert!(error.contains("cannot exceed"));
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            MAX_DAEMON_AUDIT_BYTES - 1
        );
    }

    #[test]
    fn task_listing_is_stable() {
        let manager = TaskManager::new();
        manager.register_subagent("zeta".to_string());
        manager.register_subagent("alpha".to_string());

        let tasks = manager.list_tasks();
        assert_eq!(tasks[0]["id"], "alpha");
        assert_eq!(tasks[1]["id"], "zeta");
    }

    #[test]
    fn completed_and_failed_tasks_retain_outcomes() {
        let manager = TaskManager::new();
        manager
            .register_task("completed".to_string(), "terminal")
            .unwrap();
        manager.complete("completed", Some(json!({"stdout": "done"})));
        manager
            .register_task("failed".to_string(), "terminal")
            .unwrap();
        manager.fail(
            "failed",
            "exit code 1".to_string(),
            Some(json!({"exit_code": 1})),
        );

        let completed = manager.get_task("completed").unwrap();
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["result"]["stdout"], "done");
        let failed = manager.get_task("failed").unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["result"]["exit_code"], 1);
        assert_eq!(failed["error"], "exit code 1");
    }

    #[test]
    fn terminal_entries_are_pruned_when_the_in_memory_limit_is_reached() {
        let manager = TaskManager::with_task_limit(1);
        manager
            .register_task("finished".to_string(), "terminal")
            .expect("register finished task");
        manager.complete("finished", Some(json!({"stdout": "done"})));

        manager
            .register_task("replacement".to_string(), "terminal")
            .expect("terminal entry releases capacity");

        assert!(manager.get_task("finished").is_none());
        assert_eq!(
            manager.get_status("replacement").as_deref(),
            Some("running")
        );
    }

    #[test]
    fn active_entries_still_enforce_the_in_memory_limit() {
        let manager = TaskManager::with_task_limit(1);
        manager
            .register_task("active".to_string(), "terminal")
            .expect("register active task");

        let error = manager
            .register_task("rejected".to_string(), "terminal")
            .expect_err("active task keeps its capacity slot");

        assert!(error.contains("1-task limit"), "{error}");
        assert_eq!(manager.list_tasks().len(), 1);
    }

    #[test]
    fn terminal_durable_entries_are_pruned_using_persisted_status() {
        let directory = tempdir().expect("tempdir");
        let sessions_dir = directory.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            directory.path().join("state/daemons"),
        )
        .expect("durable store");
        let prepare = |id: &str| {
            store.prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: id.to_string(),
                command: "printf ok".to_string(),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: sessions_dir.clone(),
                session_id: "origin".to_string(),
                execution: crate::config::ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
        };
        let manager = TaskManager::with_task_limit(1);
        prepare("durable-finished").expect("prepare first durable task");
        manager
            .register_durable_task("durable-finished".to_string(), store.clone())
            .expect("register first durable task");
        store
            .fail_prepared("durable-finished", "finished".to_string())
            .expect("finish durable task");
        prepare("durable-replacement").expect("prepare replacement durable task");

        manager
            .register_durable_task("durable-replacement".to_string(), store.clone())
            .expect("persisted terminal status releases capacity");

        assert!(manager.get_task("durable-finished").is_none());
        assert_eq!(
            manager.get_status("durable-replacement").as_deref(),
            Some("prepared")
        );
    }

    #[test]
    fn duplicate_durable_registration_rolls_back_only_the_incoming_store() {
        let first_root = tempdir().expect("first root");
        let second_root = tempdir().expect("second root");
        let prepare = |root: &tempfile::TempDir| {
            let sessions_dir = root.path().join("state/sessions");
            std::fs::create_dir_all(&sessions_dir).expect("sessions");
            let session_store = SessionStore::at_dir(sessions_dir.clone());
            session_store.create_session_with_id("origin");
            let store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
                root.path().join("state/daemons"),
            )
            .expect("durable store");
            store
                .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                    id: "shared-id".to_string(),
                    command: "printf ok".to_string(),
                    cwd: root.path().to_path_buf(),
                    project_root: root.path().to_path_buf(),
                    profile_id: "default".to_string(),
                    sessions_dir,
                    session_id: "origin".to_string(),
                    execution: crate::config::ExecutionConfig::default(),
                    timeout_secs: 10,
                    max_output_bytes: 1024,
                })
                .expect("prepare durable task");
            store
        };
        let first_store = prepare(&first_root);
        let second_store = prepare(&second_root);
        let manager = TaskManager::new();
        manager
            .register_durable_task("shared-id".to_string(), first_store.clone())
            .expect("register first store");

        manager
            .register_durable_task("shared-id".to_string(), first_store.clone())
            .expect_err("same-store duplicate is rejected");
        assert!(first_store.get("shared-id").unwrap().is_some());

        manager
            .register_durable_task("shared-id".to_string(), second_store.clone())
            .expect_err("different-store duplicate is rejected");
        assert!(first_store.get("shared-id").unwrap().is_some());
        assert!(second_store.get("shared-id").unwrap().is_none());
    }

    #[test]
    fn checked_durable_terminalization_persists_failed_status() {
        let directory = tempdir().expect("tempdir");
        let sessions_dir = directory.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            directory.path().join("state/daemons"),
        )
        .expect("durable store");
        store
            .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: "terminalize-me".to_string(),
                command: "printf ok".to_string(),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir,
                session_id: "origin".to_string(),
                execution: crate::config::ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
            .expect("prepare durable task");
        let manager = TaskManager::new();
        manager
            .register_durable_task("terminalize-me".to_string(), store.clone())
            .expect("register durable task");

        let record = manager
            .fail_prepared_durable_task("terminalize-me", "compensation failed".to_string())
            .expect("terminalize durable task");

        assert_eq!(record.status, "failed");
        assert_eq!(record.error.as_deref(), Some("compensation failed"));
        assert_eq!(
            store.get("terminalize-me").unwrap().unwrap().status,
            "failed"
        );
    }

    #[test]
    fn failed_prepared_compensation_is_returned_and_persisted_to_audit() {
        let directory = tempdir().expect("tempdir");
        let sessions_dir = directory.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            directory.path().join("state/daemons"),
        )
        .expect("durable store");
        let id = "compensation-audit";
        store
            .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: id.to_string(),
                command: "printf ok".to_string(),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir,
                session_id: "origin".to_string(),
                execution: crate::config::ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
            .expect("prepare durable task");
        let manager = TaskManager::new();
        manager
            .register_durable_task(id.to_string(), store.clone())
            .expect("register durable task");
        let record_path = store.daemon_dir().join("tasks").join(format!("{id}.json"));
        std::fs::remove_file(&record_path).expect("remove prepared record");
        std::fs::create_dir(&record_path).expect("inject compensation failure");

        let error = manager
            .compensate_prepared_task(id, "injected primary failure".to_string())
            .expect_err("compensation failure must reach caller");
        assert!(error.contains("daemon audit"), "{error}");
        let records = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("compensation audit");
        assert!(records.iter().any(|record| {
            record.action == "prepared_task_compensation"
                && record.target.as_deref() == Some(id)
                && record.outcome == "compensation_failed"
                && record
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("injected primary failure"))
        }));
    }

    #[tokio::test]
    async fn timer_delivers_to_session_and_records_audit_and_status() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::new(directory.path());
        store.create_session_with_id("origin");
        let audit = DaemonAuditLog::new(directory.path());
        let manager = TaskManager::new();
        manager
            .spawn_session_timer(SessionTimerRequest {
                id: "short-timer".to_string(),
                initial_delay: Duration::from_millis(5),
                interval: Duration::from_millis(5),
                repeat_count: 2,
                prompt: "wake up".to_string(),
                session_store: store.clone(),
                session_id: "origin".to_string(),
                audit_log: audit.clone(),
            })
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if manager.get_status("short-timer").as_deref() == Some("completed") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("timer completes");

        let session = store.load("origin").expect("origin session");
        assert_eq!(session.messages.last().unwrap().role, "user");
        assert_eq!(session.messages.last().unwrap().content, "wake up");
        session.validate_message_sequence().unwrap();
        assert_eq!(session.events.last().unwrap().kind, "timer_fired");
        assert_eq!(
            manager.get_task("short-timer").unwrap()["result"]["delivered_count"],
            2
        );
        let audit_records = audit.read_all().expect("audit records");
        assert_eq!(audit_records.len(), 2);
        assert_eq!(audit_records[0].outcome, "delivered");
    }

    #[tokio::test]
    async fn running_task_can_be_cancelled() {
        let manager = TaskManager::new();
        manager
            .register_task("cancel-me".to_string(), "terminal")
            .unwrap();
        let handle = tokio::spawn(std::future::pending::<()>());
        manager
            .attach_abort_handle("cancel-me", handle.abort_handle())
            .unwrap();

        let cancelled = manager.cancel("cancel-me").expect("cancel task");
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["error"], "cancelled by user");
    }
}
