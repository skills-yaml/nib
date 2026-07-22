//! Durable profile-scoped background work and detached workers.

use crate::agent::AgentLoopConfig;
use crate::config::ExecutionConfig;
use crate::daemons::task::{
    deliver_background_task_observation, BackgroundTaskSession, DaemonAuditLog, DaemonAuditRecord,
};
use crate::profile::{Profile, ProfileRegistry};
use crate::session::{SessionError, SessionEvent, SessionRunLease, SessionStore};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(all(test, windows))]
use std::process::Command;
#[cfg(not(windows))]
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
#[cfg(not(windows))]
use std::sync::{
    mpsc::{self, Receiver, RecvTimeoutError, Sender},
    OnceLock,
};
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};

const TASK_FILE_VERSION: u32 = 1;
const LOCK_RETRIES: usize = 500;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
const TASK_LOCK_STRIPES: usize = 64;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STALE_WORKER_AFTER_SECONDS: i64 = 15;
const MAX_TASK_RECORD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DURABLE_TASK_RECORDS: usize = 10_000;
const MAX_DURABLE_TASK_ENUMERATION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TASK_DIRECTORY_EXTRA_ENTRIES: usize = TASK_LOCK_STRIPES + 1_024;
const MAX_TASK_DIRECTORY_NAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_LEGACY_LOCK_MIGRATION_ENTRIES: usize = 100_000;
const MAX_RECONCILIATION_REPORT_TASKS: usize = 128;
const MAX_RECONCILIATION_ERROR_CHARS: usize = 4_096;
const MAX_TERMINAL_COMMAND_BYTES: usize = 65_536;
const MAX_TERMINAL_TIMEOUT_SECONDS: u64 = 3_600;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 1_048_576;
const MAX_SCHEDULE_PROMPT_BYTES: usize = 20_000;
const MAX_SCHEDULE_DELAY_SECONDS: u64 = 31_536_000;
const MAX_COMPENSATION_AUDIT_DETAIL_CHARS: usize = 16_384;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DurableTaskRecord {
    pub id: String,
    #[serde(default)]
    pub execution_id: String,
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<u32>,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_occurrences: u32,
    #[serde(default)]
    pub total_occurrences: u32,
}

impl DurableTaskRecord {
    fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurableReconciledTask {
    pub id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DurableReconcileReport {
    pub scanned_records: usize,
    pub reconciled_records: usize,
    pub omitted_records: usize,
    pub tasks: Vec<DurableReconciledTask>,
}

impl DurableReconcileReport {
    pub fn is_empty(&self) -> bool {
        self.reconciled_records == 0
    }
}

#[derive(Debug, Clone)]
pub struct DurableTerminalRequest {
    pub id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    pub profile_id: String,
    pub sessions_dir: PathBuf,
    pub session_id: String,
    pub execution: ExecutionConfig,
    pub timeout_secs: u64,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone)]
pub struct DurableScheduleRequest {
    pub id: String,
    pub prompt: String,
    pub project_root: PathBuf,
    pub profile_id: String,
    pub sessions_dir: PathBuf,
    pub session_id: String,
    pub initial_delay: Duration,
    pub interval: Duration,
    pub repeat_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DurableJob {
    Terminal {
        command: String,
        cwd: PathBuf,
        project_root: PathBuf,
        profile_id: String,
        sessions_dir: PathBuf,
        session_id: String,
        execution: ExecutionConfig,
        timeout_secs: u64,
        max_output_bytes: usize,
    },
    Schedule {
        prompt: String,
        project_root: PathBuf,
        profile_id: String,
        sessions_dir: PathBuf,
        session_id: String,
        interval_secs: u64,
        repeat_count: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableTaskFile {
    version: u32,
    record: DurableTaskRecord,
    job: DurableJob,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_lease: Option<WorkerLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_occurrence: Option<u32>,
}

#[derive(Debug)]
struct OpenedDurableTaskFile {
    task: DurableTaskFile,
    file: File,
    bytes_read: u64,
    needs_execution_id_migration: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkerLease {
    token: String,
}

#[derive(Debug, Clone)]
struct WorkerOwner {
    token: String,
    pid: u32,
}

enum MonitoredRun<T> {
    Completed(T),
    Cancelled,
    LeaseLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileHookPoint {
    BeforeDelivery,
    AfterDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerPublicationHookPoint {
    BeforeEffects,
    AfterEffects,
}

#[cfg(test)]
fn pause_worker_publication(
    point: WorkerPublicationHookPoint,
    task_id: &str,
) -> Result<(), String> {
    let Some(expected_task) = std::env::var_os("NIB_TEST_WORKER_PUBLICATION_TASK_ID") else {
        return Ok(());
    };
    if expected_task != std::ffi::OsStr::new(task_id) {
        return Ok(());
    }
    let expected_point = std::env::var("NIB_TEST_WORKER_PUBLICATION_POINT")
        .map_err(|error| format!("missing worker publication hook point: {error}"))?;
    let actual_point = match point {
        WorkerPublicationHookPoint::BeforeEffects => "before",
        WorkerPublicationHookPoint::AfterEffects => "after",
    };
    if expected_point != actual_point {
        return Ok(());
    }
    let ready = std::env::var_os("NIB_TEST_WORKER_PUBLICATION_READY")
        .ok_or_else(|| "missing worker publication ready path".to_string())?;
    std::fs::write(&ready, b"ready")
        .map_err(|error| format!("failed to publish worker hook readiness: {error}"))?;
    std::thread::sleep(Duration::from_secs(60));
    Err("worker publication hook was not terminated".to_string())
}

#[cfg(not(test))]
fn pause_worker_publication(
    _point: WorkerPublicationHookPoint,
    _task_id: &str,
) -> Result<(), String> {
    Ok(())
}

#[derive(Debug, Clone)]
pub struct DurableTaskStore {
    daemon_dir: PathBuf,
    tasks_dir: PathBuf,
    daemon_directory: Arc<crate::daemons::state::StableDirectory>,
    tasks_directory: Arc<crate::daemons::state::StableDirectory>,
    max_records: usize,
    max_enumeration_bytes: u64,
    max_reconciliation_report_tasks: usize,
}

impl DurableTaskStore {
    pub fn at_daemon_dir(daemon_dir: impl Into<PathBuf>) -> Result<Self, String> {
        let daemon_dir = daemon_dir.into();
        ensure_local_directory(&daemon_dir, "daemon state")?;
        let tasks_dir = daemon_dir.join("tasks");
        ensure_local_directory(&tasks_dir, "task state")?;
        let canonical_daemon = daemon_dir
            .canonicalize()
            .map_err(|error| format!("failed to resolve daemon state: {error}"))?;
        let canonical_tasks = tasks_dir
            .canonicalize()
            .map_err(|error| format!("failed to resolve task state: {error}"))?;
        if !canonical_tasks.starts_with(&canonical_daemon) {
            return Err(format!(
                "task state escapes the profile daemon directory: {}",
                tasks_dir.display()
            ));
        }
        let daemon_directory = Arc::new(crate::daemons::state::StableDirectory::open(
            &canonical_daemon,
        )?);
        let tasks_directory = Arc::new(daemon_directory.open_child(&canonical_tasks)?);
        let store = Self {
            daemon_dir: canonical_daemon,
            tasks_dir: canonical_tasks,
            daemon_directory,
            tasks_directory,
            max_records: MAX_DURABLE_TASK_RECORDS,
            max_enumeration_bytes: MAX_DURABLE_TASK_ENUMERATION_BYTES,
            max_reconciliation_report_tasks: MAX_RECONCILIATION_REPORT_TASKS,
        };
        store.initialize_lock_namespace()?;
        store.tasks_directory.recover_stale_temporary_files(
            ".task-",
            MAX_LEGACY_LOCK_MIGRATION_ENTRIES,
            MAX_TASK_DIRECTORY_NAME_BYTES,
        )?;
        store.migrate_legacy_task_locks()?;
        store.migrate_legacy_execution_ids()?;
        Ok(store)
    }

    #[cfg(test)]
    fn with_record_limit(mut self, max_records: usize) -> Self {
        self.max_records = max_records;
        self
    }

    #[cfg(test)]
    fn with_enumeration_byte_limit(mut self, max_enumeration_bytes: u64) -> Self {
        self.max_enumeration_bytes = max_enumeration_bytes;
        self
    }

    #[cfg(test)]
    fn with_reconciliation_report_limit(mut self, max_tasks: usize) -> Self {
        self.max_reconciliation_report_tasks = max_tasks;
        self
    }

    pub fn for_project(project_root: &Path) -> Result<Self, String> {
        let config =
            crate::config::load_nib_config_full(project_root).map_err(|error| error.to_string())?;
        let profiles = ProfileRegistry::load(project_root, &config.profiles)
            .map_err(|error| error.to_string())?;
        let profile = profiles
            .for_workspace(project_root)
            .unwrap_or_else(|| profiles.default_profile());
        profile
            .ensure_state_dirs()
            .map_err(|error| error.to_string())?;
        Self::at_daemon_dir(profile.daemon_dir())
    }

    pub fn from_sessions_dir(sessions_dir: &Path) -> Result<Self, String> {
        let profile_state = sessions_dir
            .parent()
            .ok_or("session directory has no profile state parent")?;
        Self::at_daemon_dir(profile_state.join("daemons"))
    }

    pub fn resolve_profile_scope(sessions_dir: &Path) -> Result<(PathBuf, String), String> {
        let sessions_dir = sessions_dir
            .canonicalize()
            .map_err(|error| format!("failed to resolve originating sessions: {error}"))?;
        for candidate in sessions_dir.ancestors().skip(1) {
            if !candidate.join(".nib").is_dir() {
                continue;
            }
            let Ok(config) = crate::config::load_nib_config_full(candidate) else {
                continue;
            };
            let Ok(profiles) = ProfileRegistry::load(candidate, &config.profiles) else {
                continue;
            };
            for profile in profiles.all() {
                if profile
                    .sessions_dir()
                    .canonicalize()
                    .is_ok_and(|path| path == sessions_dir)
                {
                    return Ok((profile.root_path().to_path_buf(), profile.id().to_string()));
                }
            }
        }
        Err(format!(
            "originating sessions do not belong to a configured workspace profile: {}",
            sessions_dir.display()
        ))
    }

    pub fn daemon_dir(&self) -> &Path {
        &self.daemon_dir
    }

    pub(crate) fn same_store(&self, other: &Self) -> bool {
        self.tasks_directory.same_identity(&other.tasks_directory)
    }

    pub(crate) fn audit_compensation_failure(
        &self,
        task_id: &str,
        action: &str,
        primary_error: &str,
        compensation_error: &str,
    ) -> Result<(), String> {
        let detail = format!(
            "task_id={task_id}; primary_error={primary_error}; compensation_error={compensation_error}"
        )
        .chars()
        .take(MAX_COMPENSATION_AUDIT_DETAIL_CHARS)
        .collect();
        DaemonAuditLog::at_path(self.daemon_dir.join("audit.jsonl")).append(&DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "task".to_string(),
            action: action.to_string(),
            target: Some(task_id.to_string()),
            outcome: "compensation_failed".to_string(),
            authorized: true,
            detail: Some(detail),
        })
    }

    pub fn prepare_terminal(
        &self,
        request: DurableTerminalRequest,
    ) -> Result<DurableTaskRecord, String> {
        validate_task_id(&request.id)?;
        validate_terminal_request(&request)?;
        let now = Utc::now();
        let task = DurableTaskFile {
            version: TASK_FILE_VERSION,
            record: DurableTaskRecord {
                id: request.id.clone(),
                execution_id: uuid::Uuid::new_v4().to_string(),
                kind: "terminal".to_string(),
                status: "prepared".to_string(),
                result: None,
                error: None,
                created_at: now,
                updated_at: now,
                worker_pid: None,
                cancel_requested: false,
                next_run_at: None,
                completed_occurrences: 0,
                total_occurrences: 1,
            },
            job: DurableJob::Terminal {
                command: request.command,
                cwd: request.cwd,
                project_root: request.project_root,
                profile_id: request.profile_id,
                sessions_dir: request.sessions_dir,
                session_id: request.session_id,
                execution: request.execution,
                timeout_secs: request.timeout_secs,
                max_output_bytes: request.max_output_bytes,
            },
            worker_lease: None,
            active_occurrence: None,
        };
        self.create(&task)?;
        Ok(task.record)
    }

    pub fn prepare_schedule(
        &self,
        request: DurableScheduleRequest,
    ) -> Result<DurableTaskRecord, String> {
        validate_task_id(&request.id)?;
        validate_schedule_request(&request)?;
        let now = Utc::now();
        let initial_delay = i64::try_from(request.initial_delay.as_secs())
            .map_err(|_| "schedule delay is too large".to_string())?;
        let next_run_at = now
            .checked_add_signed(ChronoDuration::seconds(initial_delay))
            .ok_or("schedule next-run timestamp overflow")?;
        let task = DurableTaskFile {
            version: TASK_FILE_VERSION,
            record: DurableTaskRecord {
                id: request.id.clone(),
                execution_id: uuid::Uuid::new_v4().to_string(),
                kind: "schedule".to_string(),
                status: "prepared".to_string(),
                result: Some(json!({
                    "delivered_count": 0,
                    "repeat_count": request.repeat_count,
                    "runs": [],
                    "execution_mode": "plan",
                })),
                error: None,
                created_at: now,
                updated_at: now,
                worker_pid: None,
                cancel_requested: false,
                next_run_at: Some(next_run_at),
                completed_occurrences: 0,
                total_occurrences: request.repeat_count,
            },
            job: DurableJob::Schedule {
                prompt: request.prompt,
                project_root: request.project_root,
                profile_id: request.profile_id,
                sessions_dir: request.sessions_dir,
                session_id: request.session_id,
                interval_secs: request.interval.as_secs(),
                repeat_count: request.repeat_count,
            },
            worker_lease: None,
            active_occurrence: None,
        };
        self.create(&task)?;
        Ok(task.record)
    }

    pub fn start(&self, id: &str) -> Result<DurableTaskRecord, String> {
        let executable = worker_executable()?;
        #[cfg(not(windows))]
        let worker_reaper = worker_reaper_sender()?.clone();
        let lease_token = uuid::Uuid::new_v4().to_string();
        let current = self.update(id, |task| {
            if task.record.status != "prepared" {
                return Err(format!(
                    "background task {id} cannot start (status: {})",
                    task.record.status
                ));
            }
            task.record.status = "starting".to_string();
            task.record.worker_pid = None;
            task.record.updated_at = Utc::now();
            task.worker_lease = Some(WorkerLease {
                token: lease_token.clone(),
            });
            Ok(())
        })?;
        let current = self.get_file(&current.id)?;

        #[cfg(windows)]
        let pid = match crate::daemons::windows_worker::spawn_detached_worker(
            &executable,
            &self.daemon_dir,
            id,
            &lease_token,
            job_project_root(&current.job),
        ) {
            Ok(pid) => pid,
            Err(error) => {
                let error = format!("failed to launch durable task worker: {error}");
                return Err(self.finish_worker_launch_failure(id, &lease_token, error));
            }
        };
        #[cfg(not(windows))]
        let pid = {
            let mut command = Command::new(executable);
            command
                .arg("task-worker")
                .arg("--daemon-dir")
                .arg(&self.daemon_dir)
                .arg("--task-id")
                .arg(id)
                .arg("--lease-token")
                .arg(&lease_token)
                .current_dir(job_project_root(&current.job))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            configure_worker_process(&mut command);
            let child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let error = format!("failed to launch durable task worker: {error}");
                    return Err(self.finish_worker_launch_failure(id, &lease_token, error));
                }
            };
            let pid = child.id();
            if let Err(error) = hand_off_worker(&worker_reaper, child) {
                return Err(self.finish_worker_launch_failure(id, &lease_token, error));
            }
            pid
        };

        match self.bind_worker(id, &lease_token, pid) {
            Ok(record) => Ok(record),
            Err(error) => {
                match self.get(id) {
                    Ok(Some(record)) if record.is_terminal() => return Ok(record),
                    Ok(_) => {}
                    Err(read_error) => {
                        let compensation_error = format!(
                            "failed to inspect durable state after worker bind failure: {read_error}"
                        );
                        let audit_error = self
                            .audit_compensation_failure(
                                id,
                                "bind_worker",
                                &error,
                                &compensation_error,
                            )
                            .err();
                        return Err(append_compensation_error(
                            error,
                            compensation_error,
                            audit_error,
                        ));
                    }
                }
                let compensation_error =
                    "worker ownership could not be bound; stale-worker reconciliation is required"
                        .to_string();
                let audit_error = self
                    .audit_compensation_failure(id, "bind_worker", &error, &compensation_error)
                    .err();
                Err(append_compensation_error(
                    error,
                    compensation_error,
                    audit_error,
                ))
            }
        }
    }

    fn finish_worker_launch_failure(
        &self,
        id: &str,
        lease_token: &str,
        primary_error: String,
    ) -> String {
        match self.finish_starting(id, lease_token, "failed", None, Some(primary_error.clone())) {
            Ok(_) => primary_error,
            Err(compensation_error) => {
                let audit_error = self
                    .audit_compensation_failure(
                        id,
                        "worker_launch",
                        &primary_error,
                        &compensation_error,
                    )
                    .err();
                append_compensation_error(primary_error, compensation_error, audit_error)
            }
        }
    }

    pub fn get(&self, id: &str) -> Result<Option<DurableTaskRecord>, String> {
        validate_task_id(id)?;
        let _lock = self.acquire_task_lock(id)?;
        let path = self.task_path(id);
        if !self.tasks_directory.path_exists(&path)? {
            return Ok(None);
        }
        Ok(Some(self.read_path(&path)?.record))
    }

    pub fn list(&self) -> Result<Vec<DurableTaskRecord>, String> {
        let mut records = Vec::new();
        let mut remaining_bytes = self.max_enumeration_bytes;
        for path in self.record_paths()? {
            let (_task_id, _lock) = self.acquire_record_path_lock(&path)?;
            let (task, bytes_read, _) = self.read_path_bounded(&path, remaining_bytes)?;
            remaining_bytes = remaining_bytes
                .checked_sub(bytes_read)
                .ok_or_else(|| "durable task enumeration byte count underflowed".to_string())?;
            records.push(task.record);
        }
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    pub fn cancel(&self, id: &str) -> Result<DurableTaskRecord, String> {
        let updated = self.update(id, |task| {
            if task.record.is_terminal() {
                return Err(format!(
                    "background task {id} is not running (status: {})",
                    task.record.status
                ));
            }
            task.record.cancel_requested = true;
            task.record.updated_at = Utc::now();
            if matches!(task.record.status.as_str(), "prepared" | "starting")
                && task.record.worker_pid.is_none()
            {
                task.record.status = "cancelled".to_string();
                task.record.error = Some("cancelled by user before start".to_string());
                task.worker_lease = None;
                scrub_completed_job(&mut task.job);
            } else {
                task.record.status = "cancelling".to_string();
            }
            Ok(())
        })?;
        if updated.is_terminal() {
            return Ok(updated);
        }
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(20));
            let record = self
                .get(id)?
                .ok_or_else(|| format!("background task disappeared during cancellation: {id}"))?;
            if record.is_terminal() {
                return Ok(record);
            }
        }
        self.get(id)?
            .ok_or_else(|| format!("background task disappeared during cancellation: {id}"))
    }

    pub fn fail_prepared(&self, id: &str, error: String) -> Result<DurableTaskRecord, String> {
        self.update(id, |task| {
            if task.record.status != "prepared" {
                return Ok(());
            }
            task.record.status = "failed".to_string();
            task.record.error = Some(error);
            task.record.updated_at = Utc::now();
            task.worker_lease = None;
            scrub_completed_job(&mut task.job);
            Ok(())
        })
    }

    pub fn remove_prepared(&self, id: &str) -> Result<bool, String> {
        validate_task_id(id)?;
        let _admission_lock = self.acquire_admission_lock()?;
        let _task_lock = self.acquire_task_lock(id)?;
        let path = self.task_path(id);
        if !self.tasks_directory.path_exists(&path)? {
            return Ok(false);
        }
        let opened = self.read_path_opened(&path)?;
        if opened.task.record.status != "prepared" {
            return Err(format!(
                "background task {id} cannot be rolled back (status: {})",
                opened.task.record.status
            ));
        }
        self.tasks_directory
            .remove_file_if_matches(&path, &opened.file, ".task-delete-")?;
        Ok(true)
    }

    pub fn reconcile(&self, now: DateTime<Utc>) -> Result<DurableReconcileReport, String> {
        self.reconcile_with_hook(now, |_, _| Ok(()))
    }

    fn reconcile_with_hook(
        &self,
        now: DateTime<Utc>,
        mut hook: impl FnMut(ReconcileHookPoint, &str) -> Result<(), String>,
    ) -> Result<DurableReconcileReport, String> {
        let paths = self.record_paths()?;
        let mut report = DurableReconcileReport {
            scanned_records: paths.len(),
            reconciled_records: 0,
            omitted_records: 0,
            tasks: Vec::with_capacity(self.max_reconciliation_report_tasks.min(paths.len())),
        };
        for path in paths {
            let (_task_id, read_lock) = self.acquire_record_path_lock(&path)?;
            let record = self.read_path(&path)?.record;
            let stale_worker = matches!(
                record.status.as_str(),
                "starting" | "running" | "cancelling"
            ) && now.signed_duration_since(record.updated_at).num_seconds()
                >= STALE_WORKER_AFTER_SECONDS;
            if record.status != "reconciling" && !stale_worker {
                continue;
            }
            let task_id = record.id.clone();
            drop(record);
            drop(read_lock);
            let Some(reconciled) = self.reconcile_record(&path, &task_id, now, &mut hook)? else {
                continue;
            };
            report.reconciled_records = report.reconciled_records.saturating_add(1);
            if report.tasks.len() < self.max_reconciliation_report_tasks {
                report.tasks.push(DurableReconciledTask {
                    id: reconciled.id,
                    status: reconciled.status,
                    error: reconciled
                        .error
                        .map(|error| error.chars().take(MAX_RECONCILIATION_ERROR_CHARS).collect()),
                });
            } else {
                report.omitted_records = report.omitted_records.saturating_add(1);
            }
        }
        Ok(report)
    }

    fn reconcile_record(
        &self,
        path: &Path,
        task_id: &str,
        now: DateTime<Utc>,
        hook: &mut impl FnMut(ReconcileHookPoint, &str) -> Result<(), String>,
    ) -> Result<Option<DurableTaskRecord>, String> {
        let _lock = self.acquire_task_lock(task_id)?;
        let mut opened = self.read_path_opened(path)?;
        let stale_worker = matches!(
            opened.task.record.status.as_str(),
            "starting" | "running" | "cancelling"
        ) && now
            .signed_duration_since(opened.task.record.updated_at)
            .num_seconds()
            >= STALE_WORKER_AFTER_SECONDS;
        if stale_worker {
            opened.task.record.status = "reconciling".to_string();
            opened.task.record.worker_pid = None;
            opened.task.record.updated_at = now;
            opened.task.worker_lease = None;
            self.write_path_expected(
                path,
                &opened.task,
                crate::daemons::state::FileExpectation::Present(&opened.file),
            )?;
            opened = self.read_path_opened(path)?;
        } else if opened.task.record.status != "reconciling" {
            return Ok(None);
        }
        if opened.task.worker_lease.is_some() {
            return Err(format!(
                "background task {task_id} has a worker lease while reconciling"
            ));
        }

        hook(ReconcileHookPoint::BeforeDelivery, task_id)?;
        let base_error = "worker lease expired; completion is unknown and the job was not replayed";
        let occurrence = match &opened.task.job {
            DurableJob::Schedule { .. } => {
                Some(opened.task.active_occurrence.unwrap_or_else(|| {
                    opened
                        .task
                        .record
                        .completed_occurrences
                        .saturating_add(1)
                        .min(opened.task.record.total_occurrences.max(1))
                }))
            }
            DurableJob::Terminal { .. } => None,
        };
        let delivery_error = reconcile_expired_job(
            &opened.task.job,
            &self.daemon_dir,
            task_id,
            &opened.task.record.execution_id,
            occurrence,
            base_error,
        )
        .err();
        let error = delivery_error.map_or_else(
            || base_error.to_string(),
            |delivery| format!("{base_error}; reconciliation delivery failed: {delivery}"),
        );
        hook(ReconcileHookPoint::AfterDelivery, task_id)?;

        if opened.task.record.status != "reconciling" || opened.task.worker_lease.is_some() {
            return Err(format!(
                "background task {task_id} is no longer owned by the reconciler"
            ));
        }
        let result = opened.task.record.result.clone();
        finish_task_file(&mut opened.task, "failed", result, Some(error));
        self.write_path_after_effects_expected(
            path,
            &opened.task,
            crate::daemons::state::FileExpectation::Present(&opened.file),
        )?;
        Ok(Some(opened.task.record))
    }

    fn create(&self, task: &DurableTaskFile) -> Result<(), String> {
        let _admission_lock = self.acquire_admission_lock()?;
        let _task_lock = self.acquire_task_lock(&task.record.id)?;
        let path = self.task_path(&task.record.id);
        if self.tasks_directory.path_exists(&path)? {
            return Err(format!(
                "background task already exists: {}",
                task.record.id
            ));
        }
        self.ensure_admission_capacity()?;
        self.write_path_expected(&path, task, crate::daemons::state::FileExpectation::Missing)
    }

    fn ensure_admission_capacity(&self) -> Result<(), String> {
        self.tasks_directory.verify_visible()?;
        if self.max_records == 0 {
            return Err("durable task records reached the 0-record limit".to_string());
        }
        let paths = self.record_paths()?;
        if paths.len() < self.max_records {
            return Ok(());
        }

        let mut oldest_terminal: Option<(DateTime<Utc>, DateTime<Utc>, String)> = None;
        for path in paths {
            let (_task_id, _lock) = self.acquire_record_path_lock(&path)?;
            let record = self.read_path(&path)?.record;
            if !record.is_terminal() {
                continue;
            }
            let candidate = (record.updated_at, record.created_at, record.id);
            if oldest_terminal
                .as_ref()
                .is_none_or(|current| candidate < *current)
            {
                oldest_terminal = Some(candidate);
            }
        }

        let Some((_, _, id)) = oldest_terminal else {
            return Err(format!(
                "durable task records reached the {}-record limit and no terminal record can be evicted",
                self.max_records
            ));
        };
        self.evict_terminal_record(&id)
    }

    fn evict_terminal_record(&self, id: &str) -> Result<(), String> {
        let _task_lock = self.acquire_task_lock(id)?;
        let path = self.task_path(id);
        let opened = self.read_path_opened(&path)?;
        if !opened.task.record.is_terminal() {
            return Err(format!(
                "durable task records reached the {}-record limit, but selected task {id} is no longer terminal",
                self.max_records
            ));
        }

        let audit = DaemonAuditLog::at_path(self.daemon_dir.join("audit.jsonl"));
        let detail = format!(
            "task_id={id}; status={}; updated_at={}; reason=durable_record_capacity",
            opened.task.record.status, opened.task.record.updated_at
        );
        audit.append(&DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "task".to_string(),
            action: "evict_terminal_task".to_string(),
            target: Some(id.to_string()),
            outcome: "planned".to_string(),
            authorized: true,
            detail: Some(detail.clone()),
        })?;

        if let Err(error) =
            self.tasks_directory
                .remove_file_if_matches(&path, &opened.file, ".task-evict-")
        {
            let _ = audit.append(&DaemonAuditRecord {
                timestamp: Utc::now(),
                daemon: "task".to_string(),
                action: "evict_terminal_task".to_string(),
                target: Some(id.to_string()),
                outcome: "failed".to_string(),
                authorized: true,
                detail: Some(format!("{detail}; remove_error={error}")),
            });
            return Err(format!(
                "failed to evict terminal task record {}: {error}",
                path.display()
            ));
        }
        if let Err(audit_error) = audit.append(&DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "task".to_string(),
            action: "evict_terminal_task".to_string(),
            target: Some(id.to_string()),
            outcome: "evicted".to_string(),
            authorized: true,
            detail: Some(detail),
        }) {
            let restore = self.write_path_expected(
                &path,
                &opened.task,
                crate::daemons::state::FileExpectation::Missing,
            );
            let _ = audit.append(&DaemonAuditRecord {
                timestamp: Utc::now(),
                daemon: "task".to_string(),
                action: "evict_terminal_task".to_string(),
                target: Some(id.to_string()),
                outcome: "rolled_back".to_string(),
                authorized: true,
                detail: Some(format!("completion_audit_error={audit_error}")),
            });
            return Err(match restore {
                Ok(()) => format!(
                    "failed to audit terminal task eviction: {audit_error}; evicted record was restored"
                ),
                Err(restore_error) => format!(
                    "failed to audit terminal task eviction: {audit_error}; failed to restore evicted record: {restore_error}"
                ),
            });
        }
        Ok(())
    }

    fn get_file(&self, id: &str) -> Result<DurableTaskFile, String> {
        validate_task_id(id)?;
        let _lock = self.acquire_task_lock(id)?;
        let path = self.task_path(id);
        if !self.tasks_directory.path_exists(&path)? {
            return Err(format!("background task not found: {id}"));
        }
        self.read_path(&path)
    }

    fn record_paths(&self) -> Result<Vec<PathBuf>, String> {
        self.tasks_directory.verify_visible()?;
        self.tasks_directory.recover_stale_temporary_files_strict(
            ".task-",
            MAX_LEGACY_LOCK_MIGRATION_ENTRIES,
            MAX_TASK_DIRECTORY_NAME_BYTES,
        )?;
        let mut paths = Vec::new();
        let max_entries = self
            .max_records
            .checked_add(MAX_TASK_DIRECTORY_EXTRA_ENTRIES)
            .ok_or_else(|| "durable task directory entry limit overflowed".to_string())?;
        self.tasks_directory.for_each_entry_bounded(
            max_entries,
            MAX_TASK_DIRECTORY_NAME_BYTES,
            |name| {
                if crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
                    &name, ".task-",
                ) {
                    return Ok(());
                }
                let path = self.tasks_dir.join(name);
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    return Ok(());
                }
                if paths.len() >= self.max_records {
                    return Err(format!(
                        "durable task records exceed the {}-record limit",
                        self.max_records
                    ));
                }
                paths.push(path);
                Ok(())
            },
        )?;
        self.tasks_directory.verify_visible()?;
        paths.sort();
        Ok(paths)
    }

    fn update<F>(&self, id: &str, mutate: F) -> Result<DurableTaskRecord, String>
    where
        F: FnOnce(&mut DurableTaskFile) -> Result<(), String>,
    {
        validate_task_id(id)?;
        let _lock = self.acquire_task_lock(id)?;
        let path = self.task_path(id);
        if !self.tasks_directory.path_exists(&path)? {
            return Err(format!("background task not found: {id}"));
        }
        let mut opened = self.read_path_opened(&path)?;
        mutate(&mut opened.task)?;
        self.write_path_expected(
            &path,
            &opened.task,
            crate::daemons::state::FileExpectation::Present(&opened.file),
        )?;
        Ok(opened.task.record)
    }

    fn bind_worker(
        &self,
        id: &str,
        lease_token: &str,
        pid: u32,
    ) -> Result<DurableTaskRecord, String> {
        self.update(id, |task| {
            require_lease_token(task, id, lease_token)?;
            if !matches!(task.record.status.as_str(), "starting" | "running") {
                return Err(format!(
                    "background task {id} changed before worker launch (status: {})",
                    task.record.status
                ));
            }
            if task.record.worker_pid.is_some_and(|owner| owner != pid) {
                return Err(worker_lease_lost(id));
            }
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(pid);
            task.record.updated_at = Utc::now();
            Ok(())
        })
    }

    fn claim_worker(&self, id: &str, owner: &WorkerOwner) -> Result<DurableTaskFile, String> {
        self.update_owned(id, owner, |task| {
            if !matches!(
                task.record.status.as_str(),
                "starting" | "running" | "cancelling"
            ) {
                return Err(format!(
                    "background task {id} cannot be claimed (status: {})",
                    task.record.status
                ));
            }
            task.record.status = if task.record.cancel_requested {
                "cancelling".to_string()
            } else {
                "running".to_string()
            };
            task.record.worker_pid = Some(owner.pid);
            task.record.updated_at = Utc::now();
            Ok(())
        })?;
        self.get_file(id)
    }

    fn update_owned<F>(
        &self,
        id: &str,
        owner: &WorkerOwner,
        mutate: F,
    ) -> Result<DurableTaskRecord, String>
    where
        F: FnOnce(&mut DurableTaskFile) -> Result<(), String>,
    {
        self.update_owned_with(id, owner, mutate)
            .map(|(record, ())| record)
    }

    fn update_owned_with<F, T>(
        &self,
        id: &str,
        owner: &WorkerOwner,
        mutate: F,
    ) -> Result<(DurableTaskRecord, T), String>
    where
        F: FnOnce(&mut DurableTaskFile) -> Result<T, String>,
    {
        self.update_owned_with_hook(id, owner, mutate, || Ok(()))
    }

    fn update_owned_with_hook<F, T, H>(
        &self,
        id: &str,
        owner: &WorkerOwner,
        mutate: F,
        after_mutate: H,
    ) -> Result<(DurableTaskRecord, T), String>
    where
        F: FnOnce(&mut DurableTaskFile) -> Result<T, String>,
        H: FnOnce() -> Result<(), String>,
    {
        validate_task_id(id)?;
        let _lock = self.acquire_task_lock(id)?;
        let path = self.task_path(id);
        if !self.tasks_directory.path_exists(&path)? {
            return Err(format!("background task not found: {id}"));
        }
        let mut opened = self.read_path_opened(&path)?;
        require_worker_owner(&opened.task, id, owner)?;
        self.tasks_directory.verify_visible()?;
        pause_worker_publication(WorkerPublicationHookPoint::BeforeEffects, id)?;
        let output = mutate(&mut opened.task)?;
        pause_worker_publication(WorkerPublicationHookPoint::AfterEffects, id)?;
        after_mutate()?;
        self.write_path_after_effects_expected(
            &path,
            &opened.task,
            crate::daemons::state::FileExpectation::Present(&opened.file),
        )?;
        Ok((opened.task.record, output))
    }

    fn poll_worker_owned(
        &self,
        id: &str,
        owner: &WorkerOwner,
        force_heartbeat: bool,
    ) -> Result<DurableTaskRecord, String> {
        validate_task_id(id)?;
        let _lock = self.acquire_task_lock(id)?;
        let path = self.task_path(id);
        if !self.tasks_directory.path_exists(&path)? {
            return Err(format!("background task not found: {id}"));
        }
        let mut opened = self.read_path_opened(&path)?;
        require_worker_owner(&opened.task, id, owner)?;
        if matches!(opened.task.record.status.as_str(), "running" | "cancelling")
            && (force_heartbeat
                || Utc::now()
                    .signed_duration_since(opened.task.record.updated_at)
                    .num_seconds()
                    >= 5)
        {
            opened.task.record.updated_at = Utc::now();
            self.write_path_expected(
                &path,
                &opened.task,
                crate::daemons::state::FileExpectation::Present(&opened.file),
            )?;
        }
        Ok(opened.task.record)
    }

    fn finish_owned(
        &self,
        owner: &WorkerOwner,
        id: &str,
        status: &str,
        result: Option<Value>,
        error: Option<String>,
    ) -> Result<DurableTaskRecord, String> {
        self.update_owned(id, owner, |task| {
            finish_task_file(task, status, result, error);
            Ok(())
        })
    }

    fn finish_starting(
        &self,
        id: &str,
        lease_token: &str,
        status: &str,
        result: Option<Value>,
        error: Option<String>,
    ) -> Result<DurableTaskRecord, String> {
        self.update(id, |task| {
            require_lease_token(task, id, lease_token)?;
            finish_task_file(task, status, result, error);
            Ok(())
        })
    }

    #[cfg(test)]
    fn update_schedule_progress_owned(
        &self,
        owner: &WorkerOwner,
        id: &str,
        occurrence: u32,
        next_run_at: Option<DateTime<Utc>>,
        run: Value,
    ) -> Result<DurableTaskRecord, String> {
        self.update_owned(id, owner, |task| {
            update_schedule_progress_file(task, occurrence, next_run_at, run);
            Ok(())
        })
    }

    fn task_path(&self, id: &str) -> PathBuf {
        self.tasks_dir.join(format!("{id}.json"))
    }

    fn lock_path(&self, id: &str) -> PathBuf {
        self.lock_path_for_stripe(task_lock_stripe(id))
    }

    fn lock_anchor_path(&self, id: &str) -> PathBuf {
        self.lock_anchor_path_for_stripe(task_lock_stripe(id))
    }

    fn acquire_record_path_lock(&self, path: &Path) -> Result<(String, TaskLock), String> {
        if path.parent() != Some(self.tasks_dir.as_path()) {
            return Err(format!(
                "task record is not a direct child of the task state directory: {}",
                path.display()
            ));
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("task record name is not valid UTF-8: {}", path.display()))?;
        let id = name.strip_suffix(".json").ok_or_else(|| {
            format!(
                "task record does not use the expected .json suffix: {}",
                path.display()
            )
        })?;
        validate_task_id(id)?;
        if self.task_path(id) != path {
            return Err(format!(
                "task record path does not match its derived id {id}: {}",
                path.display()
            ));
        }
        Ok((id.to_string(), self.acquire_task_lock(id)?))
    }

    fn lock_path_for_stripe(&self, stripe: usize) -> PathBuf {
        self.tasks_dir
            .join(format!(".task-stripe-{stripe:02}.lock"))
    }

    fn lock_anchor_path_for_stripe(&self, stripe: usize) -> PathBuf {
        self.daemon_dir
            .join(format!(".task-stripe-{stripe:02}.lock.anchor"))
    }

    fn admission_lock_path(&self) -> PathBuf {
        self.tasks_dir.join(".admission.lock")
    }

    fn admission_lock_anchor_path(&self) -> PathBuf {
        self.daemon_dir.join(".admission.task.lock.anchor")
    }

    fn acquire_task_lock(&self, id: &str) -> Result<TaskLock, String> {
        let lock = TaskLock::acquire(self.lock_path(id), self.lock_anchor_path(id))?;
        self.tasks_directory.verify_visible()?;
        self.daemon_directory.verify_visible()?;
        self.cleanup_legacy_task_lock(id)?;
        Ok(lock)
    }

    fn acquire_admission_lock(&self) -> Result<TaskLock, String> {
        let lock = TaskLock::acquire(
            self.admission_lock_path(),
            self.admission_lock_anchor_path(),
        )?;
        self.tasks_directory.verify_visible()?;
        self.daemon_directory.verify_visible()?;
        Ok(lock)
    }

    fn cleanup_legacy_task_lock(&self, id: &str) -> Result<(), String> {
        let path = self.tasks_dir.join(format!("{id}.lock"));
        let anchor_path = self.daemon_dir.join(format!(".{id}.task.lock.anchor"));
        cleanup_existing_task_lock_artifacts(
            &path,
            &anchor_path,
            &self.tasks_directory,
            &self.daemon_directory,
        )
    }

    fn initialize_lock_namespace(&self) -> Result<(), String> {
        self.tasks_directory.verify_visible()?;
        for stripe in 0..TASK_LOCK_STRIPES {
            drop(TaskLock::acquire(
                self.lock_path_for_stripe(stripe),
                self.lock_anchor_path_for_stripe(stripe),
            )?);
        }
        drop(self.acquire_admission_lock()?);
        self.tasks_directory.verify_visible()
    }

    fn migrate_legacy_task_locks(&self) -> Result<(), String> {
        self.tasks_directory.for_each_entry_bounded(
            MAX_LEGACY_LOCK_MIGRATION_ENTRIES,
            MAX_TASK_DIRECTORY_NAME_BYTES,
            |name| {
                let Some(id) = legacy_visible_lock_id(&name) else {
                    return Ok(());
                };
                let _stripe = TaskLock::acquire(self.lock_path(&id), self.lock_anchor_path(&id))?;
                self.cleanup_legacy_task_lock(&id)
            },
        )?;
        self.daemon_directory.for_each_entry_bounded(
            MAX_LEGACY_LOCK_MIGRATION_ENTRIES,
            MAX_TASK_DIRECTORY_NAME_BYTES,
            |name| {
                let Some(id) = legacy_anchor_lock_id(&name) else {
                    return Ok(());
                };
                let _stripe = TaskLock::acquire(self.lock_path(&id), self.lock_anchor_path(&id))?;
                self.cleanup_legacy_task_lock(&id)
            },
        )
    }

    fn migrate_legacy_execution_ids(&self) -> Result<(), String> {
        self.migrate_legacy_execution_ids_with_hook(|| Ok(()))
    }

    fn migrate_legacy_execution_ids_with_hook(
        &self,
        after_enumeration: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let _admission_lock = self.acquire_admission_lock()?;
        let paths = self.record_paths()?;
        after_enumeration()?;
        for path in paths {
            let (_task_id, _lock) = self.acquire_record_path_lock(&path)?;
            let opened = self.read_path_bounded_opened(&path, MAX_TASK_RECORD_BYTES)?;
            if opened.needs_execution_id_migration {
                self.write_path_expected(
                    &path,
                    &opened.task,
                    crate::daemons::state::FileExpectation::Present(&opened.file),
                )?;
            }
        }
        Ok(())
    }

    fn read_path(&self, path: &Path) -> Result<DurableTaskFile, String> {
        self.read_path_bounded(path, MAX_TASK_RECORD_BYTES)
            .map(|(task, _, _)| task)
    }

    fn read_path_opened(&self, path: &Path) -> Result<OpenedDurableTaskFile, String> {
        self.read_path_bounded_opened(path, MAX_TASK_RECORD_BYTES)
    }

    fn read_path_bounded(
        &self,
        path: &Path,
        allocation_limit: u64,
    ) -> Result<(DurableTaskFile, u64, bool), String> {
        self.read_path_bounded_opened(path, allocation_limit)
            .map(|opened| {
                (
                    opened.task,
                    opened.bytes_read,
                    opened.needs_execution_id_migration,
                )
            })
    }

    fn read_path_bounded_opened(
        &self,
        path: &Path,
        allocation_limit: u64,
    ) -> Result<OpenedDurableTaskFile, String> {
        self.read_path_bounded_with_hook(path, allocation_limit, || Ok(()))
    }

    fn read_path_bounded_with_hook(
        &self,
        path: &Path,
        allocation_limit: u64,
        after_open: impl FnOnce() -> Result<(), String>,
    ) -> Result<OpenedDurableTaskFile, String> {
        self.tasks_directory.verify_visible()?;
        let mut file = self.tasks_directory.open_read(path)?;
        let opened_metadata = file.metadata().map_err(|error| {
            format!(
                "failed to inspect opened task record {}: {error}",
                path.display()
            )
        })?;
        if !opened_metadata.is_file() {
            return Err(format!(
                "task record must be a regular local file: {}",
                path.display()
            ));
        }
        validate_task_record_size(path, opened_metadata.len())?;
        validate_task_enumeration_size(path, opened_metadata.len(), allocation_limit)?;
        after_open()?;
        self.tasks_directory.verify_file_identity(path, &file)?;
        let mut bytes = vec![0; opened_metadata.len() as usize];
        file.read_exact(&mut bytes)
            .map_err(|error| format!("failed to read task record {}: {error}", path.display()))?;
        let mut extra = [0_u8; 1];
        if file
            .read(&mut extra)
            .map_err(|error| format!("failed to read task record {}: {error}", path.display()))?
            != 0
        {
            return Err(format!(
                "task record changed while being read: {}",
                path.display()
            ));
        }
        validate_task_record_size(path, bytes.len() as u64)?;
        validate_task_enumeration_size(path, bytes.len() as u64, allocation_limit)?;
        let post_metadata = file.metadata().map_err(|error| {
            format!("failed to recheck task record {}: {error}", path.display())
        })?;
        if !post_metadata.is_file() {
            return Err(format!(
                "task record must be a regular local file: {}",
                path.display()
            ));
        }
        validate_task_record_size(path, post_metadata.len())?;
        validate_task_enumeration_size(path, post_metadata.len(), allocation_limit)?;
        if post_metadata.len() != opened_metadata.len() {
            return Err(format!(
                "task record changed while being read: {}",
                path.display()
            ));
        }
        self.tasks_directory.verify_file_identity(path, &file)?;
        self.tasks_directory.verify_visible()?;
        let bytes_read = bytes.len() as u64;
        let contents = String::from_utf8(bytes)
            .map_err(|error| format!("task record {} is not UTF-8: {error}", path.display()))?;
        let mut task: DurableTaskFile = serde_json::from_str(&contents)
            .map_err(|error| format!("invalid task record {}: {error}", path.display()))?;
        if task.version != TASK_FILE_VERSION {
            return Err(format!(
                "unsupported task record version {} in {}",
                task.version,
                path.display()
            ));
        }
        validate_task_id(&task.record.id)?;
        let needs_execution_id_migration = task.record.execution_id.is_empty();
        if needs_execution_id_migration {
            task.record.execution_id = legacy_execution_id(&task.record);
        }
        let expected = self.task_path(&task.record.id);
        if expected != path {
            return Err(format!(
                "task record id {} does not match file {}",
                task.record.id,
                path.display()
            ));
        }
        Ok(OpenedDurableTaskFile {
            task,
            file,
            bytes_read,
            needs_execution_id_migration,
        })
    }

    fn write_path_expected(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        expected: crate::daemons::state::FileExpectation<'_>,
    ) -> Result<(), String> {
        self.tasks_directory.verify_visible()?;
        self.write_path_with_mode(path, task, false, expected)
    }

    fn write_path_after_effects_expected(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        expected: crate::daemons::state::FileExpectation<'_>,
    ) -> Result<(), String> {
        self.write_path_with_mode(path, task, true, expected)
    }

    fn write_path_with_mode(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        after_effects: bool,
        expected: crate::daemons::state::FileExpectation<'_>,
    ) -> Result<(), String> {
        self.write_path_with_mode_and_commit_check(path, task, after_effects, expected, || Ok(()))
    }

    #[cfg(all(test, unix))]
    fn write_path_after_effects_expected_with_commit_check(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        expected: crate::daemons::state::FileExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.write_path_with_mode_and_commit_check(path, task, true, expected, before_commit)
    }

    fn write_path_with_mode_and_commit_check(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        after_effects: bool,
        expected: crate::daemons::state::FileExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(task)
            .map_err(|error| format!("failed to encode task record: {error}"))?;
        validate_task_record_size(path, encoded.len() as u64)?;
        let result = self
            .tasks_directory
            .save_bytes_atomically_expected_with_hook(
                path,
                &encoded,
                ".task-",
                !after_effects,
                expected,
                before_commit,
            );
        result.map_err(|error| format!("failed to persist task record: {error}"))
    }
}

pub async fn run_worker(daemon_dir: &Path, task_id: &str, lease_token: &str) -> Result<(), String> {
    let store = DurableTaskStore::at_daemon_dir(daemon_dir)?;
    let owner = WorkerOwner {
        token: lease_token.to_string(),
        pid: std::process::id(),
    };
    let task = match store.claim_worker(task_id, &owner) {
        Ok(task) => task,
        Err(error) if is_worker_lease_lost(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    if task.record.cancel_requested {
        return match publish_claimed_cancellation(&store, &owner, task_id, &task) {
            Ok(_) => Ok(()),
            Err(error) if is_worker_lease_lost(&error) => Ok(()),
            Err(error) => Err(error),
        };
    }

    let outcome = match task.job {
        DurableJob::Terminal {
            command,
            cwd,
            project_root,
            profile_id,
            sessions_dir,
            session_id,
            execution,
            timeout_secs,
            max_output_bytes,
        } => {
            run_terminal_worker(
                &store,
                &owner,
                task_id,
                TerminalWorkerJob {
                    command,
                    cwd,
                    project_root,
                    profile_id,
                    sessions_dir,
                    session_id,
                    execution,
                    timeout_secs,
                    max_output_bytes,
                },
            )
            .await
        }
        DurableJob::Schedule {
            prompt,
            project_root,
            profile_id,
            sessions_dir,
            session_id,
            interval_secs,
            repeat_count,
        } => {
            run_schedule_worker(
                &store,
                &owner,
                task_id,
                ScheduleWorkerJob {
                    prompt,
                    project_root,
                    profile_id,
                    sessions_dir,
                    session_id,
                    interval_secs,
                    repeat_count,
                },
            )
            .await
        }
    };
    if let Err(error) = &outcome {
        let error = crate::tools::executor::redact_text(error);
        let result = store.get(task_id)?.and_then(|record| record.result);
        match store.finish_owned(&owner, task_id, "failed", result, Some(error)) {
            Ok(_) => {}
            Err(lease_error) if is_worker_lease_lost(&lease_error) => return Ok(()),
            Err(finish_error) => return Err(finish_error),
        }
    }
    outcome
}

fn publish_claimed_cancellation(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    task: &DurableTaskFile,
) -> Result<DurableTaskRecord, String> {
    match &task.job {
        DurableJob::Terminal {
            sessions_dir,
            session_id,
            ..
        } => publish_terminal_cancellation_owned(
            store,
            owner,
            task_id,
            &BackgroundTaskSession {
                session_store: SessionStore::at_dir(sessions_dir.clone()),
                session_id: session_id.clone(),
                audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
            },
            "cancelled by user before worker start",
        ),
        DurableJob::Schedule {
            sessions_dir,
            session_id,
            repeat_count,
            ..
        } => {
            let occurrence = task.active_occurrence.unwrap_or_else(|| {
                task.record
                    .completed_occurrences
                    .saturating_add(1)
                    .min(task.record.total_occurrences.max(1))
            });
            publish_schedule_cancellation_owned(
                store,
                owner,
                task_id,
                &SessionStore::at_dir(sessions_dir.clone()),
                &DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
                ScheduleCancellation {
                    session_id,
                    repeat_count: *repeat_count,
                    occurrence,
                    reason: "cancelled by user before worker start",
                },
            )
        }
    }
}

struct TerminalWorkerJob {
    command: String,
    cwd: PathBuf,
    project_root: PathBuf,
    profile_id: String,
    sessions_dir: PathBuf,
    session_id: String,
    execution: ExecutionConfig,
    timeout_secs: u64,
    max_output_bytes: usize,
}

async fn run_terminal_worker(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    job: TerminalWorkerJob,
) -> Result<(), String> {
    let (profile, config_sensitive_values) = load_worker_profile(
        &job.project_root,
        &job.profile_id,
        &job.sessions_dir,
        store.daemon_dir(),
    )?;
    let session_target = BackgroundTaskSession {
        session_store: SessionStore::at_dir(job.sessions_dir.clone()),
        session_id: job.session_id.clone(),
        audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
    };
    let mut environment_keys: Vec<_> = profile.custom_env().keys().cloned().collect();
    environment_keys.sort();
    let started = Instant::now();
    let run = timeout(
        Duration::from_secs(job.timeout_secs.max(1)),
        crate::sandbox::run_sandboxed_streaming_with_environment(
            &job.command,
            &job.cwd,
            &job.execution.provider,
            &job.execution.default_profile,
            &job.execution.boundaries,
            profile.custom_env(),
            job.max_output_bytes,
            None,
        ),
    );
    tokio::pin!(run);
    loop {
        tokio::select! {
            outcome = &mut run => {
                let record = match store.poll_worker_owned(task_id, owner, true) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                };
                if record.cancel_requested {
                    match publish_terminal_cancellation_owned(
                        store,
                        owner,
                        task_id,
                        &session_target,
                        "cancelled by user",
                    ) {
                        Ok(_) => {}
                        Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                        Err(error) => return Err(error),
                    }
                    return Ok(());
                }
                let (success, result, error) = terminal_outcome(
                    task_id,
                    &job,
                    started.elapsed().as_secs_f64(),
                    &environment_keys,
                    outcome,
                );
                let result = crate::tools::executor::redact_value_with_sensitive_values(
                    result,
                    config_sensitive_values.iter().cloned(),
                );
                let result = crate::tools::executor::redact_value_with_environment(
                    result,
                    profile.custom_env(),
                );
                let error = error.map(|value| {
                    let value = crate::tools::executor::redact_text_with_sensitive_values(
                        &value,
                        config_sensitive_values.iter().cloned(),
                    );
                    crate::tools::executor::redact_text_with_environment(
                        &value,
                        profile.custom_env(),
                    )
                });
                match publish_terminal_outcome_owned(
                    store,
                    owner,
                    task_id,
                    &session_target,
                    success,
                    result,
                    error,
                ) {
                    Ok(_) => {}
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
                return Ok(());
            }
            _ = sleep(WORKER_POLL_INTERVAL) => {
                let record = match store.poll_worker_owned(task_id, owner, false) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                };
                if record.status == "reconciling" || record.is_terminal() {
                    return Ok(());
                }
                if record.cancel_requested {
                    match publish_terminal_cancellation_owned(
                        store,
                        owner,
                        task_id,
                        &session_target,
                        "cancelled by user",
                    ) {
                        Ok(_) => {}
                        Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                        Err(error) => return Err(error),
                    }
                    return Ok(());
                }
            }
        }
    }
}

fn publish_terminal_outcome_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_target: &BackgroundTaskSession,
    success: bool,
    result: Value,
    error: Option<String>,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        let delivery_error = deliver_background_task_observation(
            session_target,
            task_id,
            &task.record.execution_id,
            success,
            Some(&result),
            error.as_deref(),
        )
        .err();
        let final_error = combine_delivery_error(error, delivery_error, success);
        let status = if success && final_error.is_none() {
            "completed"
        } else {
            "failed"
        };
        finish_task_file(task, status, Some(result), final_error);
        Ok(())
    })
}

fn publish_terminal_cancellation_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_target: &BackgroundTaskSession,
    reason: &str,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        let delivery_error = deliver_background_task_observation(
            session_target,
            task_id,
            &task.record.execution_id,
            false,
            None,
            Some(reason),
        )
        .err();
        let error = delivery_error.map_or_else(
            || reason.to_string(),
            |delivery| format!("{reason}; cancellation delivery failed: {delivery}"),
        );
        finish_task_file(task, "cancelled", None, Some(error));
        Ok(())
    })
}

#[allow(clippy::type_complexity)]
fn terminal_outcome(
    task_id: &str,
    job: &TerminalWorkerJob,
    duration: f64,
    environment_keys: &[String],
    outcome: Result<
        Result<(crate::sandbox::BoundedOutput, Option<Vec<String>>), String>,
        tokio::time::error::Elapsed,
    >,
) -> (bool, Value, Option<String>) {
    match outcome {
        Ok(Ok((output, bwrap_args))) => {
            let success = output.status.success();
            let error = (!success)
                .then(|| format!("command exited with {}", output.status.code().unwrap_or(-1)));
            let result = json!({
                "task_id": task_id,
                "command": job.command,
                "cwd": job.cwd.to_string_lossy(),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "stdout_bytes": output.stdout_bytes,
                "stderr_bytes": output.stderr_bytes,
                "stdout_bytes_retained": output.stdout.len(),
                "stderr_bytes_retained": output.stderr.len(),
                "stdout_truncated": output.stdout_truncated(),
                "stderr_truncated": output.stderr_truncated(),
                "max_output_bytes": job.max_output_bytes,
                "exit_code": output.status.code(),
                "duration": duration,
                "provider": if bwrap_args.is_some() { "bwrap" } else { "internal" },
                "sandbox_profile": job.execution.default_profile,
                "boundaries": job.execution.boundaries,
                "bwrap_args": bwrap_args,
                "environment_keys": environment_keys,
            });
            (success, result, error)
        }
        Ok(Err(error)) => (
            false,
            empty_terminal_result(task_id, job, duration, environment_keys),
            Some(format!("background command failed: {error}")),
        ),
        Err(_) => (
            false,
            empty_terminal_result(task_id, job, duration, environment_keys),
            Some(format!("command timed out after {}s", job.timeout_secs)),
        ),
    }
}

fn empty_terminal_result(
    task_id: &str,
    job: &TerminalWorkerJob,
    duration: f64,
    environment_keys: &[String],
) -> Value {
    json!({
        "task_id": task_id,
        "command": job.command,
        "cwd": job.cwd.to_string_lossy(),
        "stdout": "",
        "stderr": "",
        "stdout_bytes": 0,
        "stderr_bytes": 0,
        "stdout_bytes_retained": 0,
        "stderr_bytes_retained": 0,
        "stdout_truncated": false,
        "stderr_truncated": false,
        "max_output_bytes": job.max_output_bytes,
        "exit_code": null,
        "duration": duration,
        "provider": job.execution.provider,
        "sandbox_profile": job.execution.default_profile,
        "boundaries": job.execution.boundaries,
        "bwrap_args": null,
        "environment_keys": environment_keys,
    })
}

fn combine_delivery_error(
    error: Option<String>,
    delivery_error: Option<String>,
    success: bool,
) -> Option<String> {
    match (error, delivery_error, success) {
        (Some(error), Some(delivery), _) => {
            Some(format!("{error}; completion delivery failed: {delivery}"))
        }
        (Some(error), None, _) => Some(error),
        (None, Some(delivery), true) => Some(format!(
            "background command completed but delivery failed: {delivery}"
        )),
        (None, Some(delivery), false) => Some(delivery),
        (None, None, true) => None,
        (None, None, false) => Some("background command failed".to_string()),
    }
}

async fn monitor_worker_future<F, T>(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    cancellation: &crate::agent::CancellationSignal,
    future: F,
) -> Result<MonitoredRun<T>, String>
where
    F: std::future::Future<Output = T>,
{
    let mut cancellation_requested = match store.poll_worker_owned(task_id, owner, true) {
        Ok(record) => record.cancel_requested,
        Err(error) if is_worker_lease_lost(&error) => {
            cancellation.cancel();
            return Ok(MonitoredRun::LeaseLost);
        }
        Err(error) => return Err(error),
    };
    if cancellation_requested {
        cancellation.cancel();
    }

    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => {
                let record = match store.poll_worker_owned(task_id, owner, true) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => {
                        cancellation.cancel();
                        return Ok(MonitoredRun::LeaseLost);
                    }
                    Err(error) => return Err(error),
                };
                if cancellation_requested || record.cancel_requested {
                    cancellation.cancel();
                    return Ok(MonitoredRun::Cancelled);
                }
                return Ok(MonitoredRun::Completed(output));
            }
            _ = sleep(WORKER_POLL_INTERVAL) => {
                match store.poll_worker_owned(task_id, owner, false) {
                    Ok(record) if record.cancel_requested => {
                        cancellation_requested = true;
                        cancellation.cancel();
                    }
                    Ok(_) => {}
                    Err(error) if is_worker_lease_lost(&error) => {
                        cancellation.cancel();
                        return Ok(MonitoredRun::LeaseLost);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

async fn wait_for_schedule_run_lease(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    session_id: &str,
) -> Result<MonitoredRun<SessionRunLease>, String> {
    loop {
        let record = match store.poll_worker_owned(task_id, owner, false) {
            Ok(record) => record,
            Err(error) if is_worker_lease_lost(&error) => return Ok(MonitoredRun::LeaseLost),
            Err(error) => return Err(error),
        };
        if record.status == "reconciling" || record.is_terminal() {
            return Ok(MonitoredRun::LeaseLost);
        }
        if record.cancel_requested {
            return Ok(MonitoredRun::Cancelled);
        }

        match session_store.try_acquire_run_lease(session_id) {
            Ok(run_lease) => {
                let record = match store.poll_worker_owned(task_id, owner, true) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => {
                        return Ok(MonitoredRun::LeaseLost)
                    }
                    Err(error) => return Err(error),
                };
                if record.status == "reconciling" || record.is_terminal() {
                    return Ok(MonitoredRun::LeaseLost);
                }
                if record.cancel_requested {
                    return Ok(MonitoredRun::Cancelled);
                }
                run_lease
                    .verify_for(session_id, session_store.sessions_dir())
                    .map_err(|error| error.to_string())?;
                return Ok(MonitoredRun::Completed(run_lease));
            }
            Err(SessionError::RunLeaseHeld(_)) => sleep(WORKER_POLL_INTERVAL).await,
            Err(error) => {
                return Err(format!(
                    "failed to acquire scheduled agent run lease for {session_id}: {error}"
                ))
            }
        }
    }
}

struct ScheduleWorkerJob {
    prompt: String,
    project_root: PathBuf,
    profile_id: String,
    sessions_dir: PathBuf,
    session_id: String,
    interval_secs: u64,
    repeat_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScheduleWakePublication {
    Started,
    Finished,
}

fn schedule_delivery_key(
    task_id: &str,
    execution_id: &str,
    occurrence: u32,
    phase: &str,
) -> String {
    format!("schedule:{task_id}:{execution_id}:{occurrence}:{phase}")
}

fn execution_id_matches(details: &Value, execution_id: &str) -> bool {
    match details.get("execution_id").and_then(Value::as_str) {
        Some(existing) => existing == execution_id,
        None => execution_id.starts_with("legacy-"),
    }
}

fn record_schedule_event_once(
    store: &SessionStore,
    session_id: &str,
    kind: &str,
    mut details: Value,
    delivery_key: &str,
) -> Result<(), String> {
    let details_object = details
        .as_object_mut()
        .ok_or_else(|| "scheduled event details must be an object".to_string())?;
    details_object.insert(
        "delivery_key".to_string(),
        Value::String(delivery_key.to_string()),
    );
    store
        .update_session(session_id, |session| {
            if session.events.iter().any(|event| {
                event.details.get("delivery_key").and_then(Value::as_str) == Some(delivery_key)
            }) {
                return Ok(());
            }
            session.events.push(SessionEvent {
                index: session.events.len(),
                kind: kind.to_string(),
                details,
                timestamp: Some(Utc::now()),
            });
            Ok(())
        })
        .map_err(|error| format!("failed to record scheduled event: {error}"))
}

fn append_schedule_audit_once(
    audit: &DaemonAuditLog,
    session_id: &str,
    action: &str,
    outcome: &str,
    detail: String,
    delivery_key: &str,
    equivalent_outcomes: &[&str],
) -> Result<(), String> {
    audit.append_once_for_detail_key(
        &DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "timer".to_string(),
            action: action.to_string(),
            target: Some(session_id.to_string()),
            outcome: outcome.to_string(),
            authorized: true,
            detail: Some(format!("delivery_key={delivery_key}; {detail}")),
        },
        &format!("delivery_key={delivery_key}"),
        equivalent_outcomes,
    )
}

fn schedule_publication_error(context: &str, errors: Vec<String>) -> Option<String> {
    (!errors.is_empty()).then(|| format!("{context}: {}", errors.join("; ")))
}

struct ScheduleCancellation<'a> {
    session_id: &'a str,
    repeat_count: u32,
    occurrence: u32,
    reason: &'a str,
}

struct ScheduleWake<'a> {
    session_id: &'a str,
    task_id: &'a str,
    execution_id: &'a str,
    occurrence: u32,
    repeat_count: u32,
    prompt: &'a str,
    delivery_key: &'a str,
}

fn publish_schedule_cancellation_effects(
    task: &mut DurableTaskFile,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    cancellation: ScheduleCancellation<'_>,
) {
    let delivery_key = schedule_delivery_key(
        task_id,
        &task.record.execution_id,
        cancellation.occurrence,
        "cancel",
    );
    let mut errors = Vec::new();
    if let Err(error) = record_schedule_event_once(
        session_store,
        cancellation.session_id,
        "timer_cancelled",
        json!({
            "timer_id": task_id,
            "execution_id": task.record.execution_id,
            "occurrence": cancellation.occurrence,
            "completed_occurrences": task.record.completed_occurrences,
            "repeat_count": cancellation.repeat_count,
        }),
        &delivery_key,
    ) {
        errors.push(error);
    }
    if let Err(error) = append_schedule_audit_once(
        audit,
        cancellation.session_id,
        "cancel",
        "cancelled",
        format!("timer_id={task_id}; occurrence={}", cancellation.occurrence),
        &delivery_key,
        &["cancelled"],
    ) {
        errors.push(format!(
            "failed to record timer cancellation audit: {error}"
        ));
    }
    let error = schedule_publication_error(cancellation.reason, errors)
        .unwrap_or_else(|| cancellation.reason.to_string());
    let result = task.record.result.clone();
    finish_task_file(task, "cancelled", result, Some(error));
}

fn publish_schedule_cancellation_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    cancellation: ScheduleCancellation<'_>,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        publish_schedule_cancellation_effects(task, task_id, session_store, audit, cancellation);
        Ok(())
    })
}

fn publish_schedule_wake_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    job: &ScheduleWorkerJob,
    occurrence: u32,
) -> Result<ScheduleWakePublication, String> {
    store
        .update_owned_with(task_id, owner, |task| {
            if task.record.cancel_requested {
                publish_schedule_cancellation_effects(
                    task,
                    task_id,
                    session_store,
                    audit,
                    ScheduleCancellation {
                        session_id: &job.session_id,
                        repeat_count: job.repeat_count,
                        occurrence,
                        reason: "cancelled by user",
                    },
                );
                return Ok(ScheduleWakePublication::Finished);
            }

            let delivery_key =
                schedule_delivery_key(task_id, &task.record.execution_id, occurrence, "start");
            let mut errors = Vec::new();
            if let Err(error) = archive_pending_plan_and_record_wake(
                session_store,
                ScheduleWake {
                    session_id: &job.session_id,
                    task_id,
                    execution_id: &task.record.execution_id,
                    occurrence,
                    repeat_count: job.repeat_count,
                    prompt: &job.prompt,
                    delivery_key: &delivery_key,
                },
            ) {
                errors.push(error);
            }
            if errors.is_empty() {
                if let Err(error) = append_schedule_audit_once(
                    audit,
                    &job.session_id,
                    "wake_agent_loop",
                    "started",
                    format!(
                        "timer_id={task_id}; occurrence={occurrence}/{}; mode=plan",
                        job.repeat_count
                    ),
                    &delivery_key,
                    &["started"],
                ) {
                    errors.push(format!("failed to record scheduled wake audit: {error}"));
                }
            }
            if let Some(error) =
                schedule_publication_error("failed to publish scheduled wake", errors)
            {
                let result = task.record.result.clone();
                finish_task_file(task, "failed", result, Some(error));
                return Ok(ScheduleWakePublication::Finished);
            }
            task.active_occurrence = Some(occurrence);
            task.record.updated_at = Utc::now();
            Ok(ScheduleWakePublication::Started)
        })
        .map(|(_, publication)| publication)
}

struct ScheduleCompletion<'a> {
    occurrence: u32,
    repeat_count: u32,
    outcome: &'a str,
    steps_taken: u32,
    next_run_at: Option<DateTime<Utc>>,
    run: Value,
}

struct ScheduleFailure<'a> {
    session_id: &'a str,
    occurrence: u32,
    repeat_count: u32,
    error: String,
}

fn publish_schedule_completion_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    session_id: &str,
    completion: ScheduleCompletion<'_>,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        let delivery_key = schedule_delivery_key(
            task_id,
            &task.record.execution_id,
            completion.occurrence,
            "terminal",
        );
        let mut errors = Vec::new();
        if let Err(error) = record_schedule_event_once(
            session_store,
            session_id,
            "scheduled_agent_run_completed",
            json!({
                "timer_id": task_id,
                "execution_id": task.record.execution_id,
                "occurrence": completion.occurrence,
                "repeat_count": completion.repeat_count,
                "outcome": completion.outcome,
                "steps_taken": completion.steps_taken,
                "mode": "plan",
            }),
            &delivery_key,
        ) {
            errors.push(error);
        }
        if let Err(error) = append_schedule_audit_once(
            audit,
            session_id,
            "wake_agent_loop",
            "completed",
            format!(
                "timer_id={task_id}; occurrence={}/{}; agent_outcome={}",
                completion.occurrence, completion.repeat_count, completion.outcome
            ),
            &delivery_key,
            &["completed", "failed"],
        ) {
            errors.push(format!(
                "failed to record scheduled completion audit: {error}"
            ));
        }
        if let Some(error) =
            schedule_publication_error("scheduled completion publication failed", errors)
        {
            let result = task.record.result.clone();
            finish_task_file(task, "failed", result, Some(error));
        } else {
            update_schedule_progress_file(
                task,
                completion.occurrence,
                completion.next_run_at,
                completion.run,
            );
        }
        Ok(())
    })
}

fn publish_schedule_failure_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    failure: ScheduleFailure<'_>,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        let delivery_key = schedule_delivery_key(
            task_id,
            &task.record.execution_id,
            failure.occurrence,
            "terminal",
        );
        let mut errors = Vec::new();
        if let Err(delivery_error) = record_schedule_event_once(
            session_store,
            failure.session_id,
            "scheduled_agent_run_failed",
            json!({
                "timer_id": task_id,
                "execution_id": task.record.execution_id,
                "occurrence": failure.occurrence,
                "repeat_count": failure.repeat_count,
                "error": failure.error,
                "mode": "plan",
            }),
            &delivery_key,
        ) {
            errors.push(delivery_error);
        }
        if let Err(audit_error) = append_schedule_audit_once(
            audit,
            failure.session_id,
            "wake_agent_loop",
            "failed",
            format!(
                "timer_id={task_id}; occurrence={}/{}; error={}",
                failure.occurrence, failure.repeat_count, failure.error
            ),
            &delivery_key,
            &["completed", "failed"],
        ) {
            errors.push(format!(
                "failed to record scheduled failure audit: {audit_error}"
            ));
        }
        let final_error =
            schedule_publication_error(&failure.error, errors).unwrap_or(failure.error);
        let result = task.record.result.clone();
        finish_task_file(task, "failed", result, Some(final_error));
        Ok(())
    })
}

async fn run_schedule_worker(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    job: ScheduleWorkerJob,
) -> Result<(), String> {
    let (_profile, config_sensitive_values) = load_worker_profile(
        &job.project_root,
        &job.profile_id,
        &job.sessions_dir,
        store.daemon_dir(),
    )?;
    let session_store = SessionStore::at_dir(job.sessions_dir.clone());
    let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
    for occurrence in 1..=job.repeat_count {
        loop {
            let record = match store.poll_worker_owned(task_id, owner, false) {
                Ok(record) => record,
                Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            if record.status == "reconciling" || record.is_terminal() {
                return Ok(());
            }
            if record.cancel_requested {
                match publish_schedule_cancellation_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    ScheduleCancellation {
                        session_id: &job.session_id,
                        repeat_count: job.repeat_count,
                        occurrence,
                        reason: "cancelled by user",
                    },
                ) {
                    Ok(_) => {}
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
                return Ok(());
            }
            let Some(next_run) = record.next_run_at else {
                return Err(format!(
                    "scheduled task {task_id} has no next run timestamp"
                ));
            };
            let remaining = next_run.signed_duration_since(Utc::now());
            if remaining <= ChronoDuration::zero() {
                break;
            }
            let delay_ms = remaining.num_milliseconds().clamp(1, 500) as u64;
            sleep(Duration::from_millis(delay_ms)).await;
        }

        let run_lease = match wait_for_schedule_run_lease(
            store,
            owner,
            task_id,
            &session_store,
            &job.session_id,
        )
        .await?
        {
            MonitoredRun::Completed(run_lease) => run_lease,
            MonitoredRun::Cancelled => {
                match publish_schedule_cancellation_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    ScheduleCancellation {
                        session_id: &job.session_id,
                        repeat_count: job.repeat_count,
                        occurrence,
                        reason: "cancelled by user",
                    },
                ) {
                    Ok(_) => return Ok(()),
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            MonitoredRun::LeaseLost => return Ok(()),
        };

        match publish_schedule_wake_owned(
            store,
            owner,
            task_id,
            &session_store,
            &audit,
            &job,
            occurrence,
        ) {
            Ok(ScheduleWakePublication::Started) => {}
            Ok(ScheduleWakePublication::Finished) => return Ok(()),
            Err(error) if is_worker_lease_lost(&error) => return Ok(()),
            Err(error) => return Err(error),
        }

        let cancellation = crate::agent::CancellationSignal::new();
        let outcome = monitor_worker_future(
            store,
            owner,
            task_id,
            &cancellation,
            crate::agent::r#loop::run_agent_loop_for_profile_with_lease(
                job.project_root.clone(),
                &job.profile_id,
                &job.sessions_dir,
                &job.session_id,
                &job.prompt,
                AgentLoopConfig {
                    mode: "plan".to_string(),
                    auto_approve: false,
                    cancellation: Some(cancellation.clone()),
                    ..AgentLoopConfig::default()
                },
                run_lease,
            ),
        )
        .await?;
        let outcome = match outcome {
            MonitoredRun::Completed(outcome) => outcome,
            MonitoredRun::Cancelled => {
                match publish_schedule_cancellation_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    ScheduleCancellation {
                        session_id: &job.session_id,
                        repeat_count: job.repeat_count,
                        occurrence,
                        reason: "cancelled by user",
                    },
                ) {
                    Ok(_) => return Ok(()),
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            MonitoredRun::LeaseLost => return Ok(()),
        };
        match outcome {
            Ok(summary) => {
                let next_run = if occurrence < job.repeat_count {
                    Some(
                        Utc::now()
                            .checked_add_signed(ChronoDuration::seconds(
                                i64::try_from(job.interval_secs)
                                    .map_err(|_| "schedule interval is too large".to_string())?,
                            ))
                            .ok_or("schedule next-run timestamp overflow")?,
                    )
                } else {
                    None
                };
                let run = json!({
                    "occurrence": occurrence,
                    "session_id": job.session_id,
                    "outcome": summary.outcome,
                    "steps_taken": summary.steps_taken,
                    "mode": "plan",
                });
                let record = match publish_schedule_completion_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    &job.session_id,
                    ScheduleCompletion {
                        occurrence,
                        repeat_count: job.repeat_count,
                        outcome: &summary.outcome,
                        steps_taken: summary.steps_taken,
                        next_run_at: next_run,
                        run,
                    },
                ) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                };
                if record.is_terminal() {
                    return Ok(());
                }
            }
            Err(error) => {
                let error = crate::tools::executor::redact_text_with_sensitive_values(
                    &error,
                    config_sensitive_values.iter().cloned(),
                );
                match publish_schedule_failure_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    ScheduleFailure {
                        session_id: &job.session_id,
                        occurrence,
                        repeat_count: job.repeat_count,
                        error,
                    },
                ) {
                    Ok(_) => {}
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

fn archive_pending_plan_and_record_wake(
    store: &SessionStore,
    wake: ScheduleWake<'_>,
) -> Result<(), String> {
    let ScheduleWake {
        session_id,
        task_id,
        execution_id,
        occurrence,
        repeat_count,
        prompt,
        delivery_key,
    } = wake;
    store
        .update_session(session_id, |session| {
            if session.events.iter().any(|event| {
                event.kind == "timer_fired"
                    && (event.details.get("delivery_key").and_then(Value::as_str)
                        == Some(delivery_key)
                        || (event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                            && execution_id_matches(&event.details, execution_id)
                            && event.details.get("occurrence").and_then(Value::as_u64)
                                == Some(u64::from(occurrence))))
            }) {
                return Ok(());
            }
            if let Some(plan) = session.plan.take() {
                if !plan.is_complete() {
                    session.events.push(SessionEvent {
                        index: session.events.len(),
                        kind: "scheduled_plan_archived".to_string(),
                        details: json!({
                            "timer_id": task_id,
                            "execution_id": execution_id,
                            "plan": plan,
                        }),
                        timestamp: Some(Utc::now()),
                    });
                }
            }
            session.events.push(SessionEvent {
                index: session.events.len(),
                kind: "timer_fired".to_string(),
                details: json!({
                    "timer_id": task_id,
                    "execution_id": execution_id,
                    "occurrence": occurrence,
                    "repeat_count": repeat_count,
                    "prompt": prompt,
                    "execution_mode": "plan",
                    "delivery_key": delivery_key,
                }),
                timestamp: Some(Utc::now()),
            });
            Ok(())
        })
        .map_err(|error| format!("failed to record scheduled wake: {error}"))
}

enum ScheduleReconcileAudit {
    ReconciledFailure,
    ExistingTerminal {
        action: &'static str,
        outcome: &'static str,
        detail: String,
        delivery_key: String,
    },
}

fn reconcile_expired_job(
    job: &DurableJob,
    daemon_dir: &Path,
    task_id: &str,
    execution_id: &str,
    occurrence: Option<u32>,
    error: &str,
) -> Result<(), String> {
    let audit = DaemonAuditLog::at_path(daemon_dir.join("audit.jsonl"));
    match job {
        DurableJob::Terminal {
            sessions_dir,
            session_id,
            ..
        } => deliver_background_task_observation(
            &BackgroundTaskSession {
                session_store: SessionStore::at_dir(sessions_dir.clone()),
                session_id: session_id.clone(),
                audit_log: audit,
            },
            task_id,
            execution_id,
            false,
            None,
            Some(error),
        ),
        DurableJob::Schedule {
            sessions_dir,
            session_id,
            repeat_count,
            ..
        } => {
            let session_store = SessionStore::at_dir(sessions_dir.clone());
            let occurrence = occurrence.unwrap_or(1);
            let delivery_key = schedule_delivery_key(task_id, execution_id, occurrence, "terminal");
            let audit_action = session_store
                .update_session(session_id, |session| {
                    if let Some(existing) = session.events.iter().find(|event| {
                        matches!(
                            event.kind.as_str(),
                            "scheduled_agent_run_completed"
                                | "scheduled_agent_run_failed"
                                | "timer_cancelled"
                        ) && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                            && execution_id_matches(&event.details, execution_id)
                            && (event.details.get("delivery_key").and_then(Value::as_str)
                                == Some(delivery_key.as_str())
                                || event.details.get("occurrence").and_then(Value::as_u64)
                                    == Some(u64::from(occurrence))
                                || event.details.get("reconciled").and_then(Value::as_bool)
                                    == Some(true))
                    }) {
                        if existing.details.get("reconciled").and_then(Value::as_bool) == Some(true)
                        {
                            return Ok(ScheduleReconcileAudit::ReconciledFailure);
                        }
                        let phase = if existing.kind == "timer_cancelled" {
                            "cancel"
                        } else {
                            "terminal"
                        };
                        let existing_delivery_key = existing
                            .details
                            .get("delivery_key")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                schedule_delivery_key(task_id, execution_id, occurrence, phase)
                            });
                        let (action, outcome, detail) = match existing.kind.as_str() {
                            "scheduled_agent_run_completed" => (
                                "wake_agent_loop",
                                "completed",
                                format!(
                                    "timer_id={task_id}; occurrence={occurrence}/{}; agent_outcome={}",
                                    existing
                                        .details
                                        .get("repeat_count")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(u64::from(*repeat_count)),
                                    existing
                                        .details
                                        .get("outcome")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown")
                                ),
                            ),
                            "scheduled_agent_run_failed" => (
                                "wake_agent_loop",
                                "failed",
                                format!(
                                    "timer_id={task_id}; occurrence={occurrence}/{}; error={}",
                                    existing
                                        .details
                                        .get("repeat_count")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(u64::from(*repeat_count)),
                                    existing
                                        .details
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown")
                                ),
                            ),
                            "timer_cancelled" => (
                                "cancel",
                                "cancelled",
                                format!("timer_id={task_id}; occurrence={occurrence}"),
                            ),
                            _ => unreachable!("filtered schedule terminal event kind"),
                        };
                        return Ok(ScheduleReconcileAudit::ExistingTerminal {
                            action,
                            outcome,
                            detail,
                            delivery_key: existing_delivery_key,
                        });
                    }
                    session.events.push(SessionEvent {
                        index: session.events.len(),
                        kind: "scheduled_agent_run_failed".to_string(),
                        details: json!({
                            "timer_id": task_id,
                            "execution_id": execution_id,
                            "occurrence": occurrence,
                            "error": error,
                            "reconciled": true,
                            "mode": "plan",
                            "delivery_key": delivery_key,
                        }),
                        timestamp: Some(Utc::now()),
                    });
                    Ok(ScheduleReconcileAudit::ReconciledFailure)
                })
                .map_err(|failure| format!("failed to reconcile scheduled session: {failure}"))?;
            match audit_action {
                ScheduleReconcileAudit::ReconciledFailure => append_schedule_audit_once(
                    &audit,
                    session_id,
                    "reconcile_expired_worker",
                    "failed",
                    format!("timer_id={task_id}; occurrence={occurrence}; error={error}"),
                    &delivery_key,
                    &["failed"],
                ),
                ScheduleReconcileAudit::ExistingTerminal {
                    action,
                    outcome,
                    detail,
                    delivery_key,
                } => append_schedule_audit_once(
                    &audit,
                    session_id,
                    action,
                    outcome,
                    detail,
                    &delivery_key,
                    if outcome == "cancelled" {
                        &["cancelled"]
                    } else {
                        &["completed", "failed"]
                    },
                ),
            }
        }
    }
}

fn load_worker_profile(
    project_root: &Path,
    profile_id: &str,
    sessions_dir: &Path,
    daemon_dir: &Path,
) -> Result<(Profile, Vec<String>), String> {
    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("failed to resolve worker project root: {error}"))?;
    let config =
        crate::config::load_nib_config_full(&project_root).map_err(|error| error.to_string())?;
    let sensitive_values = config.sensitive_values();
    let profiles = ProfileRegistry::load(&project_root, &config.profiles)
        .map_err(|error| error.to_string())?;
    let profile = profiles
        .get(profile_id)
        .ok_or_else(|| format!("durable task profile no longer exists: {profile_id}"))?
        .clone();
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;
    let expected_sessions = profile
        .sessions_dir()
        .canonicalize()
        .map_err(|error| format!("failed to resolve profile sessions: {error}"))?;
    let actual_sessions = sessions_dir
        .canonicalize()
        .map_err(|error| format!("failed to resolve task sessions: {error}"))?;
    if actual_sessions != expected_sessions {
        return Err(format!(
            "durable task session scope changed: expected {}, got {}",
            expected_sessions.display(),
            actual_sessions.display()
        ));
    }
    let expected_daemon = profile
        .daemon_dir()
        .canonicalize()
        .map_err(|error| format!("failed to resolve profile daemon state: {error}"))?;
    let actual_daemon = daemon_dir
        .canonicalize()
        .map_err(|error| format!("failed to resolve task daemon state: {error}"))?;
    if actual_daemon != expected_daemon {
        return Err(format!(
            "durable task daemon scope changed: expected {}, got {}",
            expected_daemon.display(),
            actual_daemon.display()
        ));
    }
    Ok((profile, sensitive_values))
}

fn validate_terminal_request(request: &DurableTerminalRequest) -> Result<(), String> {
    if request.command.trim().is_empty() {
        return Err("background command must not be empty".to_string());
    }
    if request.command.len() > MAX_TERMINAL_COMMAND_BYTES {
        return Err(format!(
            "background command exceeds the {MAX_TERMINAL_COMMAND_BYTES}-byte limit"
        ));
    }
    if request.timeout_secs == 0 || request.timeout_secs > MAX_TERMINAL_TIMEOUT_SECONDS {
        return Err(format!(
            "background command timeout must be between 1 and {MAX_TERMINAL_TIMEOUT_SECONDS} seconds"
        ));
    }
    if request.max_output_bytes == 0 || request.max_output_bytes > MAX_TERMINAL_OUTPUT_BYTES {
        return Err(format!(
            "background command output limit must be between 1 and {MAX_TERMINAL_OUTPUT_BYTES} bytes"
        ));
    }
    validate_scoped_worker_paths(
        &request.project_root,
        &request.cwd,
        &request.sessions_dir,
        &request.session_id,
    )
}

fn validate_schedule_request(request: &DurableScheduleRequest) -> Result<(), String> {
    if request.prompt.trim().is_empty() {
        return Err("scheduled prompt must not be empty".to_string());
    }
    if request.prompt.len() > MAX_SCHEDULE_PROMPT_BYTES {
        return Err(format!(
            "scheduled prompt exceeds the {MAX_SCHEDULE_PROMPT_BYTES}-byte limit"
        ));
    }
    if request.repeat_count == 0 || request.repeat_count > 100 {
        return Err("timer repeat_count must be between 1 and 100".to_string());
    }
    if request.initial_delay.is_zero() || request.interval.is_zero() {
        return Err("timer delays must be greater than zero".to_string());
    }
    if request.initial_delay.as_secs() > MAX_SCHEDULE_DELAY_SECONDS
        || request.interval.as_secs() > MAX_SCHEDULE_DELAY_SECONDS
    {
        return Err(format!(
            "timer delays must not exceed {MAX_SCHEDULE_DELAY_SECONDS} seconds"
        ));
    }
    validate_scoped_worker_paths(
        &request.project_root,
        &request.project_root,
        &request.sessions_dir,
        &request.session_id,
    )
}

fn validate_scoped_worker_paths(
    project_root: &Path,
    cwd: &Path,
    sessions_dir: &Path,
    session_id: &str,
) -> Result<(), String> {
    validate_task_id(session_id)?;
    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("invalid durable task project root: {error}"))?;
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("invalid durable task working directory: {error}"))?;
    if !cwd.starts_with(&project_root) || !cwd.is_dir() {
        return Err(format!(
            "durable task working directory escapes its project: {}",
            cwd.display()
        ));
    }
    if !sessions_dir.is_dir() {
        return Err(format!(
            "durable task sessions directory is unavailable: {}",
            sessions_dir.display()
        ));
    }
    if SessionStore::at_dir(sessions_dir.to_path_buf())
        .load_result(session_id)
        .map_err(|error| format!("failed to inspect durable task session: {error}"))?
        .is_none()
    {
        return Err(format!("originating session not found: {session_id}"));
    }
    Ok(())
}

fn validate_task_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 160
        || !id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(format!(
            "background task id contains unsupported characters: {id}"
        ));
    }
    Ok(())
}

fn legacy_execution_id(record: &DurableTaskRecord) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let created_at = record.created_at.to_rfc3339();
    for byte in record
        .id
        .as_bytes()
        .iter()
        .chain(record.kind.as_bytes())
        .chain(created_at.as_bytes())
    {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("legacy-{hash:016x}")
}

fn validate_task_record_size(path: &Path, bytes: u64) -> Result<(), String> {
    if bytes > MAX_TASK_RECORD_BYTES {
        return Err(format!(
            "task record {} is {bytes} bytes; maximum is {MAX_TASK_RECORD_BYTES} bytes",
            path.display()
        ));
    }
    Ok(())
}

fn validate_task_enumeration_size(
    path: &Path,
    bytes: u64,
    remaining_bytes: u64,
) -> Result<(), String> {
    if bytes > remaining_bytes {
        return Err(format!(
            "durable task enumeration exceeds its aggregate byte limit before reading {} ({bytes} bytes, {remaining_bytes} bytes remaining)",
            path.display(),
        ));
    }
    Ok(())
}

fn require_lease_token(
    task: &DurableTaskFile,
    task_id: &str,
    lease_token: &str,
) -> Result<(), String> {
    if task
        .worker_lease
        .as_ref()
        .is_some_and(|lease| lease.token == lease_token)
    {
        Ok(())
    } else {
        Err(worker_lease_lost(task_id))
    }
}

fn require_worker_owner(
    task: &DurableTaskFile,
    task_id: &str,
    owner: &WorkerOwner,
) -> Result<(), String> {
    require_lease_token(task, task_id, &owner.token)?;
    if task
        .record
        .worker_pid
        .is_none_or(|worker_pid| worker_pid == owner.pid)
    {
        Ok(())
    } else {
        Err(worker_lease_lost(task_id))
    }
}

fn worker_lease_lost(task_id: &str) -> String {
    format!("worker lease lost for background task {task_id}")
}

fn is_worker_lease_lost(error: &str) -> bool {
    error.starts_with("worker lease lost for background task ")
}

fn ensure_local_directory(path: &Path, label: &str) -> Result<(), String> {
    crate::fs_security::ensure_directory_without_symlinks(path)
        .map_err(|error| format!("failed to create {label} {}: {error}", path.display()))?;
    Ok(())
}

fn job_project_root(job: &DurableJob) -> &Path {
    match job {
        DurableJob::Terminal { project_root, .. } | DurableJob::Schedule { project_root, .. } => {
            project_root
        }
    }
}

fn finish_task_file(
    task: &mut DurableTaskFile,
    status: &str,
    result: Option<Value>,
    error: Option<String>,
) {
    task.record.status = status.to_string();
    task.record.result = result;
    task.record.error = error;
    task.record.worker_pid = None;
    task.record.updated_at = Utc::now();
    task.worker_lease = None;
    task.active_occurrence = None;
    scrub_completed_job(&mut task.job);
}

fn update_schedule_progress_file(
    task: &mut DurableTaskFile,
    occurrence: u32,
    next_run_at: Option<DateTime<Utc>>,
    run: Value,
) {
    let mut runs = task
        .record
        .result
        .as_ref()
        .and_then(|value| value.get("runs"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    runs.push(run);
    task.record.completed_occurrences = occurrence;
    task.record.next_run_at = next_run_at;
    task.record.result = Some(json!({
        "delivered_count": occurrence,
        "repeat_count": task.record.total_occurrences,
        "runs": runs,
        "execution_mode": "plan",
    }));
    task.record.updated_at = Utc::now();
    task.active_occurrence = None;
    if occurrence >= task.record.total_occurrences {
        task.record.status = "completed".to_string();
        task.record.worker_pid = None;
        task.worker_lease = None;
        scrub_completed_job(&mut task.job);
    }
}

fn scrub_completed_job(job: &mut DurableJob) {
    match job {
        DurableJob::Terminal { command, .. } => {
            *command = "[removed after task completion]".to_string();
        }
        DurableJob::Schedule { prompt, .. } => {
            *prompt = "[removed after schedule completion]".to_string();
        }
    }
}

fn worker_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("NIB_WORKER_EXECUTABLE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "NIB_WORKER_EXECUTABLE is not a file: {}",
            path.display()
        ));
    }
    let current = std::env::current_exe()
        .map_err(|error| format!("failed to locate nib worker executable: {error}"))?;
    if current
        .file_stem()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value == "nib")
    {
        return Ok(current);
    }
    if current.parent().and_then(Path::parent).is_some() {
        let debug_dir = current
            .parent()
            .and_then(Path::parent)
            .expect("checked parent");
        let candidate = debug_dir.join(if cfg!(windows) { "nib.exe" } else { "nib" });
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "cannot locate nib worker executable from {}; set NIB_WORKER_EXECUTABLE",
        current.display()
    ))
}

#[cfg(not(windows))]
static WORKER_REAPER: OnceLock<Result<Sender<Child>, String>> = OnceLock::new();

#[cfg(not(windows))]
fn worker_reaper_sender() -> Result<&'static Sender<Child>, String> {
    WORKER_REAPER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            std::thread::Builder::new()
                .name("nib-worker-reaper".to_string())
                .spawn(move || reap_worker_children(receiver))
                .map_err(|error| format!("failed to start durable worker reaper: {error}"))?;
            Ok(sender)
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(not(windows))]
fn reap_worker_children(receiver: Receiver<Child>) {
    let mut children: Vec<Child> = Vec::new();
    loop {
        match receiver.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(child) => children.push(child),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                for mut child in children {
                    let _ = child.wait();
                }
                return;
            }
        }

        reap_worker_children_once(&mut children, |child| child.try_wait());
    }
}

#[cfg(not(windows))]
fn reap_worker_children_once(
    children: &mut Vec<Child>,
    mut poll: impl FnMut(&mut Child) -> std::io::Result<Option<std::process::ExitStatus>>,
) {
    let mut index = 0;
    while index < children.len() {
        match poll(&mut children[index]) {
            Ok(Some(_)) => {
                let mut child = children.swap_remove(index);
                let _ = child.wait();
            }
            Ok(None) => index += 1,
            Err(_) => {
                let mut child = children.swap_remove(index);
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(not(windows))]
fn hand_off_worker(sender: &Sender<Child>, child: Child) -> Result<(), String> {
    sender.send(child).map_err(|error| {
        let mut child = error.0;
        let pid = child.id();
        let _ = child.kill();
        let wait_error = child.wait().err();
        match wait_error {
            Some(wait_error) => format!(
                "durable worker reaper stopped before accepting pid {pid}; failed to reap worker: {wait_error}"
            ),
            None => format!("durable worker reaper stopped before accepting pid {pid}"),
        }
    })
}

#[cfg(not(windows))]
fn configure_worker_process(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

fn task_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash % TASK_LOCK_STRIPES as u64) as usize
}

fn legacy_visible_lock_id(name: &std::ffi::OsStr) -> Option<String> {
    let name = name.to_str()?;
    if name == ".admission.lock" || name.starts_with(".task-stripe-") {
        return None;
    }
    let id = name.strip_suffix(".lock")?;
    validate_task_id(id).ok()?;
    Some(id.to_string())
}

fn legacy_anchor_lock_id(name: &std::ffi::OsStr) -> Option<String> {
    let name = name.to_str()?;
    if name == ".admission.task.lock.anchor" || name.starts_with(".task-stripe-") {
        return None;
    }
    let id = name.strip_prefix('.')?.strip_suffix(".task.lock.anchor")?;
    validate_task_id(id).ok()?;
    Some(id.to_string())
}

#[derive(Debug)]
struct TaskLock {
    _file: File,
    _lock_directory: crate::daemons::state::StableDirectory,
    _anchor_directory: crate::daemons::state::StableDirectory,
}

impl TaskLock {
    fn acquire(path: PathBuf, anchor_path: PathBuf) -> Result<Self, String> {
        Self::acquire_with_hook_and_retries(path, anchor_path, LOCK_RETRIES, || Ok(()))
    }

    #[cfg(test)]
    fn acquire_with_hook(
        path: PathBuf,
        anchor_path: PathBuf,
        after_open: impl FnOnce() -> Result<(), String>,
    ) -> Result<Self, String> {
        Self::acquire_with_hook_and_retries(path, anchor_path, LOCK_RETRIES, after_open)
    }

    #[cfg(test)]
    fn acquire_with_retries(
        path: PathBuf,
        anchor_path: PathBuf,
        retries: usize,
    ) -> Result<Self, String> {
        Self::acquire_with_hook_and_retries(path, anchor_path, retries, || Ok(()))
    }

    fn acquire_with_hook_and_retries(
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
fn open_task_lock_anchor_bound(
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
fn open_task_lock_anchor_bound(
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
fn cleanup_existing_task_lock_artifacts(
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
fn cleanup_existing_task_lock_artifacts(
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
fn verify_task_lock_paths_bound(
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
fn verify_task_lock_paths_bound(
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
fn task_file_identity(
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
fn task_file_identity(_file: &File, path: &Path) -> Result<(), String> {
    Err(format!(
        "stable task file identity is unsupported on this platform: {}",
        path.display()
    ))
}

fn append_compensation_error(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ToolCallRecord;
    #[cfg(unix)]
    use std::fs;
    use tempfile::tempdir;

    const TASK_LOCK_CHILD_DAEMON_DIR: &str = "NIB_TEST_TASK_LOCK_DAEMON_DIR";
    const TASK_LOCK_CHILD_KIND: &str = "NIB_TEST_TASK_LOCK_KIND";
    const TASK_LOCK_CHILD_ID: &str = "NIB_TEST_TASK_LOCK_ID";
    const TASK_LOCK_CHILD_EXPECTATION: &str = "NIB_TEST_TASK_LOCK_EXPECTATION";
    const RECONCILE_CHILD_DAEMON_DIR: &str = "NIB_TEST_RECONCILE_DAEMON_DIR";
    const RECONCILE_CHILD_TASK_ID: &str = "NIB_TEST_RECONCILE_TASK_ID";
    const RECONCILE_CHILD_POINT: &str = "NIB_TEST_RECONCILE_POINT";
    const RECONCILE_CHILD_READY: &str = "NIB_TEST_RECONCILE_READY";
    const PUBLICATION_CHILD_DAEMON_DIR: &str = "NIB_TEST_PUBLICATION_DAEMON_DIR";
    const PUBLICATION_CHILD_SESSIONS_DIR: &str = "NIB_TEST_PUBLICATION_SESSIONS_DIR";
    const PUBLICATION_CHILD_TASK_ID: &str = "NIB_TEST_PUBLICATION_TASK_ID";
    const PUBLICATION_CHILD_KIND: &str = "NIB_TEST_PUBLICATION_KIND";
    const PUBLICATION_CHILD_OWNER_TOKEN: &str = "NIB_TEST_PUBLICATION_OWNER_TOKEN";
    const PUBLICATION_CHILD_OWNER_PID: &str = "NIB_TEST_PUBLICATION_OWNER_PID";
    #[cfg(unix)]
    const TASK_COMMIT_CHILD_DAEMON_DIR: &str = "NIB_TASK_COMMIT_CHILD_DAEMON_DIR";
    #[cfg(unix)]
    const TASK_COMMIT_CHILD_ID: &str = "NIB_TASK_COMMIT_CHILD_ID";
    #[cfg(unix)]
    const TASK_COMMIT_CHILD_MODE: &str = "NIB_TASK_COMMIT_CHILD_MODE";
    #[cfg(unix)]
    const TASK_COMMIT_CHILD_READY: &str = "NIB_TASK_COMMIT_CHILD_READY";
    #[cfg(unix)]
    const TASK_COMMIT_CHILD_RELEASE: &str = "NIB_TASK_COMMIT_CHILD_RELEASE";

    fn fixture() -> (tempfile::TempDir, DurableTaskStore, SessionStore) {
        let directory = tempdir().expect("tempdir");
        let daemon = directory.path().join("state/daemons");
        let sessions = directory.path().join("state/sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let session_store = SessionStore::at_dir(sessions);
        session_store.create_session_with_id("origin");
        let store = DurableTaskStore::at_daemon_dir(daemon).expect("store");
        (directory, store, session_store)
    }

    fn prepare_schedule_fixture(
        directory: &tempfile::TempDir,
        store: &DurableTaskStore,
        session_store: &SessionStore,
        id: &str,
    ) {
        store
            .prepare_schedule(DurableScheduleRequest {
                id: id.to_string(),
                prompt: "scheduled plan".to_string(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                initial_delay: Duration::from_secs(1),
                interval: Duration::from_secs(1),
                repeat_count: 1,
            })
            .expect("prepare schedule");
    }

    fn scheduled_agent_fixture() -> (tempfile::TempDir, DurableTaskStore, SessionStore) {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join(".gitignore"), ".nib/\n").expect("gitignore");
        std::fs::write(
            directory.path().join("README.md"),
            "scheduled agent fixture\n",
        )
        .expect("readme");
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "nib-tests@example.invalid"],
            vec!["config", "user.name", "nib tests"],
            vec!["add", ".gitignore", "README.md"],
            vec!["commit", "--quiet", "-m", "initial"],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(directory.path())
                .status()
                .expect("git command");
            assert!(status.success());
        }

        let mut config = crate::config::NibConfig::default();
        config.llm.active_provider = Some("mock".to_string());
        config.llm.providers.insert(
            "mock".to_string(),
            crate::config::ProviderEntry {
                model: "mock-model".to_string(),
                ..crate::config::ProviderEntry::default()
            },
        );
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        crate::config::save_nib_config_full(directory.path(), &mut config).expect("runtime config");

        let session_store = SessionStore::for_project(directory.path()).expect("session store");
        session_store.create_session_with_id("origin");
        let store = DurableTaskStore::for_project(directory.path()).expect("durable task store");
        (directory, store, session_store)
    }

    fn prepare_owned_due_schedule(
        directory: &tempfile::TempDir,
        store: &DurableTaskStore,
        session_store: &SessionStore,
        task_id: &str,
    ) -> (WorkerOwner, ScheduleWorkerJob, DateTime<Utc>) {
        prepare_schedule_fixture(directory, store, session_store, task_id);
        let owner = WorkerOwner {
            token: format!("{task_id}-owner"),
            pid: std::process::id(),
        };
        let stale_heartbeat = Utc::now() - ChronoDuration::seconds(10);
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.record.updated_at = stale_heartbeat;
                task.record.next_run_at = Some(Utc::now() - ChronoDuration::seconds(1));
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed due schedule worker");
        let job = ScheduleWorkerJob {
            prompt: "scheduled plan".to_string(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            interval_secs: 1,
            repeat_count: 1,
        };
        (owner, job, stale_heartbeat)
    }

    async fn wait_for_worker_heartbeat(
        store: &DurableTaskStore,
        task_id: &str,
        prior_heartbeat: DateTime<Utc>,
    ) {
        timeout(Duration::from_secs(10), async {
            loop {
                let record = store
                    .get(task_id)
                    .expect("read schedule worker")
                    .expect("schedule worker remains present");
                if record.updated_at > prior_heartbeat {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("schedule worker heartbeat");
    }

    #[tokio::test]
    async fn due_schedule_waits_for_active_session_before_wake_and_completes_once() {
        let (directory, store, session_store) = scheduled_agent_fixture();
        let active_run = session_store
            .try_acquire_run_lease("origin")
            .expect("hold active session run");
        let task_id = "schedule-waits-for-active-run";
        let (owner, job, stale_heartbeat) =
            prepare_owned_due_schedule(&directory, &store, &session_store, task_id);
        let worker_store = store.clone();
        let worker_task_id = task_id.to_string();
        let mut worker = tokio::spawn(async move {
            run_schedule_worker(&worker_store, &owner, &worker_task_id, job).await
        });

        wait_for_worker_heartbeat(&store, task_id, stale_heartbeat).await;
        sleep(WORKER_POLL_INTERVAL).await;
        assert!(
            !worker.is_finished(),
            "due schedule must wait for the session run"
        );
        let deferred = store.get_file(task_id).expect("deferred schedule");
        assert_eq!(deferred.record.status, "running");
        assert_eq!(deferred.active_occurrence, None);
        let session = session_store.load("origin").expect("origin session");
        assert!(!session.events.iter().any(|event| {
            matches!(
                event.kind.as_str(),
                "timer_fired" | "scheduled_agent_run_completed" | "scheduled_agent_run_failed"
            )
        }));

        drop(active_run);
        timeout(Duration::from_secs(10), &mut worker)
            .await
            .expect("scheduled worker completes after session release")
            .expect("scheduled worker task")
            .expect("scheduled worker result");

        let completed = store.get_file(task_id).expect("completed schedule");
        assert_eq!(completed.record.status, "completed");
        assert_eq!(completed.record.completed_occurrences, 1);
        assert_eq!(completed.record.error, None);
        assert_eq!(completed.active_occurrence, None);
        let session = session_store.load("origin").expect("origin session");
        assert_eq!(
            session
                .events
                .iter()
                .filter(|event| event.kind == "timer_fired")
                .count(),
            1
        );
        assert_eq!(
            session
                .events
                .iter()
                .filter(|event| event.kind == "scheduled_agent_run_completed")
                .count(),
            1
        );
        assert!(!session
            .events
            .iter()
            .any(|event| event.kind == "scheduled_agent_run_failed"));
    }

    #[tokio::test]
    async fn scheduled_agent_run_keeps_the_exact_non_default_profile_scope() {
        let (directory, _default_task_store, default_session_store) = scheduled_agent_fixture();
        let mut config =
            crate::config::load_nib_config_full(directory.path()).expect("load runtime config");
        config.profiles.default = "default".to_string();
        config.profiles.active = vec![
            crate::config::ProfileConfig {
                id: "default".to_string(),
                root: PathBuf::from("."),
                state_dir: Some(PathBuf::from(".nib/profiles/default")),
                ..crate::config::ProfileConfig::default()
            },
            crate::config::ProfileConfig {
                id: "alternate".to_string(),
                root: PathBuf::from("."),
                state_dir: Some(PathBuf::from(".nib/profiles/alternate")),
                ..crate::config::ProfileConfig::default()
            },
        ];
        crate::config::save_nib_config_full(directory.path(), &mut config)
            .expect("save two-profile runtime config");
        let profiles = ProfileRegistry::load(directory.path(), &config.profiles)
            .expect("load profile registry");
        let alternate = profiles.get("alternate").expect("alternate profile");
        alternate.ensure_state_dirs().expect("alternate state");
        let alternate_session_store = SessionStore::at_dir(alternate.sessions_dir().to_path_buf());
        alternate_session_store.create_session_with_id("origin");
        let store = DurableTaskStore::at_daemon_dir(alternate.daemon_dir().to_path_buf())
            .expect("alternate durable task store");
        let task_id = "schedule-exact-alternate-profile";
        store
            .prepare_schedule(DurableScheduleRequest {
                id: task_id.to_string(),
                prompt: "scheduled plan".to_string(),
                project_root: directory.path().to_path_buf(),
                profile_id: "alternate".to_string(),
                sessions_dir: alternate_session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                initial_delay: Duration::from_secs(1),
                interval: Duration::from_secs(1),
                repeat_count: 1,
            })
            .expect("prepare alternate schedule");
        let owner = WorkerOwner {
            token: "schedule-exact-alternate-profile-owner".to_string(),
            pid: std::process::id(),
        };
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.record.next_run_at = Some(Utc::now() - ChronoDuration::seconds(1));
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed alternate schedule worker");
        let default_before = default_session_store
            .load("origin")
            .expect("default session before schedule");
        let _default_run = default_session_store
            .try_acquire_run_lease("origin")
            .expect("hold default profile run lease");

        timeout(
            Duration::from_secs(10),
            run_schedule_worker(
                &store,
                &owner,
                task_id,
                ScheduleWorkerJob {
                    prompt: "scheduled plan".to_string(),
                    project_root: directory.path().to_path_buf(),
                    profile_id: "alternate".to_string(),
                    sessions_dir: alternate_session_store.sessions_dir().to_path_buf(),
                    session_id: "origin".to_string(),
                    interval_secs: 1,
                    repeat_count: 1,
                },
            ),
        )
        .await
        .expect("alternate schedule completes")
        .expect("alternate schedule result");

        assert_eq!(
            default_session_store
                .load("origin")
                .expect("default session after schedule"),
            default_before
        );
        let alternate_session = alternate_session_store
            .load("origin")
            .expect("alternate session after schedule");
        assert!(alternate_session
            .messages
            .iter()
            .any(|message| { message.role == "user" && message.content == "scheduled plan" }));
        assert!(alternate_session.plan.is_some());
        assert_eq!(
            alternate_session
                .events
                .iter()
                .filter(|event| event.kind == "timer_fired")
                .count(),
            1
        );
        assert_eq!(
            alternate_session
                .events
                .iter()
                .filter(|event| event.kind == "scheduled_agent_run_completed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancelled_due_schedule_waiting_for_active_session_never_publishes_wake() {
        let (directory, store, session_store) = scheduled_agent_fixture();
        let _active_run = session_store
            .try_acquire_run_lease("origin")
            .expect("hold active session run");
        let task_id = "cancel-schedule-waiting-for-active-run";
        let (owner, job, stale_heartbeat) =
            prepare_owned_due_schedule(&directory, &store, &session_store, task_id);
        let worker_store = store.clone();
        let worker_task_id = task_id.to_string();
        let mut worker = tokio::spawn(async move {
            run_schedule_worker(&worker_store, &owner, &worker_task_id, job).await
        });

        wait_for_worker_heartbeat(&store, task_id, stale_heartbeat).await;
        sleep(WORKER_POLL_INTERVAL).await;
        assert!(!worker.is_finished(), "due schedule must still be waiting");
        store
            .update(task_id, |task| {
                task.record.cancel_requested = true;
                task.record.status = "cancelling".to_string();
                task.record.updated_at = Utc::now();
                Ok(())
            })
            .expect("request schedule cancellation");
        timeout(Duration::from_secs(10), &mut worker)
            .await
            .expect("deferred schedule observes cancellation")
            .expect("scheduled worker task")
            .expect("scheduled worker result");

        let cancelled = store.get_file(task_id).expect("cancelled schedule");
        assert_eq!(cancelled.record.status, "cancelled");
        assert_eq!(cancelled.active_occurrence, None);
        let session = session_store.load("origin").expect("origin session");
        assert_eq!(
            session
                .events
                .iter()
                .filter(|event| event.kind == "timer_cancelled")
                .count(),
            1
        );
        assert!(!session.events.iter().any(|event| {
            matches!(
                event.kind.as_str(),
                "timer_fired" | "scheduled_agent_run_completed" | "scheduled_agent_run_failed"
            )
        }));
    }

    fn terminal_request(
        directory: &tempfile::TempDir,
        session_store: &SessionStore,
        id: &str,
    ) -> DurableTerminalRequest {
        DurableTerminalRequest {
            id: id.to_string(),
            command: "printf ok".to_string(),
            cwd: directory.path().to_path_buf(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            execution: ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn point_reads_wait_for_an_in_flight_record_replacement() {
        let (directory, store, session_store) = fixture();
        let id = "read-during-replacement";
        store
            .prepare_terminal(terminal_request(&directory, &session_store, id))
            .expect("prepare durable task");
        let lock = store.acquire_task_lock(id).expect("hold task lock");
        let path = store.task_path(id);
        let evacuated = path.with_extension("json.evacuated");
        std::fs::rename(&path, &evacuated).expect("evacuate visible task record");

        let reader = store.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            sent.send(reader.get(id)).expect("return point read");
        });
        assert!(
            received.recv_timeout(Duration::from_millis(100)).is_err(),
            "point read must wait for the task publication lock"
        );

        std::fs::rename(&evacuated, &path).expect("restore visible task record");
        drop(lock);
        let record = received
            .recv_timeout(Duration::from_secs(2))
            .expect("point read resumed")
            .expect("point read succeeds")
            .expect("task remains present");
        assert_eq!(record.id, id);
        thread.join().expect("point reader");
    }

    #[cfg(unix)]
    #[test]
    fn enumeration_waits_for_a_live_atomic_writer_without_parsing_transaction_artifacts() {
        let (directory, store, session_store) = fixture();
        let id = "list-during-replacement";
        store
            .prepare_terminal(terminal_request(&directory, &session_store, id))
            .expect("prepare durable task");
        let path = store.task_path(id);
        let encoded = fs::read(&path).expect("task record");
        let writer_store = store.clone();
        let writer_path = path.clone();
        let (evacuated, evacuation_observed) = std::sync::mpsc::channel();

        let writer = std::thread::spawn(move || {
            let expected = writer_store
                .tasks_directory
                .open_read(&writer_path)
                .expect("open expected task record");
            writer_store
                .tasks_directory
                .save_bytes_atomically_expected_with_after_evacuation_hook(
                    &writer_path,
                    &encoded,
                    ".task-",
                    crate::daemons::state::FileExpectation::Present(&expected),
                    || {
                        evacuated.send(()).expect("signal target evacuation");
                        std::thread::sleep(Duration::from_millis(50));
                    },
                )
                .expect("complete atomic task publication");
        });

        evacuation_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("writer reached target evacuation");
        let records = store
            .list()
            .expect("enumeration waits for the live writer and resumes");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, id);
        writer.join().expect("atomic task writer");
    }

    #[cfg(unix)]
    #[test]
    fn constructor_migration_waits_for_atomic_record_evacuation() {
        let (directory, store, session_store) = fixture();
        let id = "migration-during-replacement";
        store
            .prepare_terminal(terminal_request(&directory, &session_store, id))
            .expect("prepare durable task");

        let migration_store = store.clone();
        let (enumerated, enumeration_observed) = std::sync::mpsc::sync_channel(0);
        let (continue_migration, migration_released) = std::sync::mpsc::sync_channel(0);
        let (migration_result, result_observed) = std::sync::mpsc::sync_channel(1);
        let migration = std::thread::spawn(move || {
            let result = migration_store.migrate_legacy_execution_ids_with_hook(|| {
                enumerated
                    .send(())
                    .map_err(|error| format!("signal task enumeration: {error}"))?;
                migration_released
                    .recv()
                    .map_err(|error| format!("wait to continue task migration: {error}"))?;
                Ok(())
            });
            migration_result
                .send(result)
                .expect("return migration result");
        });
        enumeration_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("migration enumerated the task record");

        let writer_store = store.clone();
        let path = store.task_path(id);
        let writer_path = path.clone();
        let (evacuated, evacuation_observed) = std::sync::mpsc::sync_channel(0);
        let (publish, publication_released) = std::sync::mpsc::sync_channel(0);
        let writer = std::thread::spawn(move || {
            let _lock = writer_store
                .acquire_task_lock(id)
                .expect("hold task publication lock");
            let opened = writer_store
                .read_path_opened(&writer_path)
                .expect("open task record for publication");
            let encoded = serde_json::to_vec_pretty(&opened.task).expect("encode task record");
            writer_store
                .tasks_directory
                .save_bytes_atomically_expected_with_after_evacuation_hook(
                    &writer_path,
                    &encoded,
                    ".task-",
                    crate::daemons::state::FileExpectation::Present(&opened.file),
                    || {
                        evacuated.send(()).expect("signal target evacuation");
                        publication_released
                            .recv()
                            .expect("wait to publish replacement");
                    },
                )
                .expect("publish replacement task record");
        });
        evacuation_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("writer evacuated the enumerated task record");
        assert!(!path.exists(), "writer must hold the target evacuated");

        continue_migration
            .send(())
            .expect("continue migration while target is evacuated");
        assert_eq!(
            result_observed.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "migration must wait for the task publication lock before its first read"
        );

        publish.send(()).expect("release task publication");
        writer.join().expect("atomic task writer");
        result_observed
            .recv_timeout(Duration::from_secs(2))
            .expect("migration resumed after publication")
            .expect("migration succeeds after publication");
        migration.join().expect("task migration");
    }

    fn prepare_reconcilable_terminal(
        directory: &tempfile::TempDir,
        store: &DurableTaskStore,
        session_store: &SessionStore,
        id: &str,
    ) -> WorkerOwner {
        let prepared = store
            .prepare_terminal(terminal_request(directory, session_store, id))
            .expect("prepare reconcilable terminal");
        session_store
            .record_tool_call(ToolCallRecord {
                id: Some(format!("call-{id}")),
                session_id: Some("origin".to_string()),
                tool_name: Some("terminal".to_string()),
                result: Some(json!({
                    "success": true,
                    "output": {
                        "task_id": id,
                        "execution_id": prepared.execution_id,
                        "status": "started",
                    },
                })),
                ..ToolCallRecord::default()
            })
            .expect("record originating background tool call");
        let owner = WorkerOwner {
            token: format!("lease-{id}"),
            pid: 42_100,
        };
        store
            .update(id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed stale worker");
        owner
    }

    #[cfg(unix)]
    #[test]
    fn real_child_task_commit_barrier_and_fsync_crash_recovery() {
        if let Some(daemon_dir) = std::env::var_os(TASK_COMMIT_CHILD_DAEMON_DIR) {
            run_task_commit_child(Path::new(&daemon_dir));
            return;
        }

        let (replacement_root, replacement_store, replacement_sessions) = fixture();
        let replacement_id = "child-task-commit-substitution";
        prepare_reconcilable_terminal(
            &replacement_root,
            &replacement_store,
            &replacement_sessions,
            replacement_id,
        );
        let replacement_path = replacement_store.task_path(replacement_id);
        let displaced = replacement_store
            .tasks_dir
            .join("child-task-commit-substitution.displaced");
        let ready = replacement_root.path().join("task-replacement.ready");
        let release = replacement_root.path().join("task-replacement.release");
        let mut child = spawn_task_commit_child(
            &replacement_store.daemon_dir,
            replacement_id,
            "replace",
            &ready,
            Some(&release),
        );
        wait_for_task_commit_child(&mut child, &ready);

        let mut replacement: DurableTaskFile =
            serde_json::from_slice(&fs::read(&replacement_path).expect("replacement task base"))
                .expect("decode replacement task base");
        replacement.record.error = Some("authoritative replacement".to_string());
        replacement.record.updated_at = Utc::now();
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize task");
        fs::rename(&replacement_path, &displaced).expect("displace expected task");
        fs::write(&replacement_path, &replacement_bytes).expect("install replacement task");
        fs::write(&release, b"release").expect("release task child");
        let status = child.wait().expect("wait for replacement child");
        assert!(status.success(), "replacement child failed: {status}");
        assert_eq!(
            fs::read(&replacement_path).expect("replacement task bytes"),
            replacement_bytes
        );
        assert!(displaced.exists(), "displaced expected task was lost");

        let (crash_root, crash_store, crash_sessions) = fixture();
        let crash_id = "child-task-fsync-crash";
        prepare_reconcilable_terminal(&crash_root, &crash_store, &crash_sessions, crash_id);
        let crash_path = crash_store.task_path(crash_id);
        let crash_before = fs::read(&crash_path).expect("task before crash");
        let crash_ready = crash_root.path().join("task-crash.ready");
        let mut crash_child = spawn_task_commit_child(
            &crash_store.daemon_dir,
            crash_id,
            "kill",
            &crash_ready,
            None,
        );
        wait_for_task_commit_child(&mut crash_child, &crash_ready);
        let temporary = task_temporary_paths(&crash_store.tasks_dir);
        assert_eq!(temporary.len(), 1, "expected one fsynced task temp");
        crash_child.kill().expect("kill task writer");
        crash_child.wait().expect("reap task writer");
        assert!(
            temporary[0].exists(),
            "killed writer temp disappeared early"
        );

        let daemon_dir = crash_store.daemon_dir.clone();
        let tasks_dir = crash_store.tasks_dir.clone();
        drop(crash_store);
        let recovered = DurableTaskStore::at_daemon_dir(&daemon_dir).expect("recover task store");
        assert_eq!(
            fs::read(&crash_path).expect("task after recovery"),
            crash_before
        );
        assert!(
            task_temporary_paths(&tasks_dir).is_empty(),
            "task recovery left the killed writer temp"
        );
        assert_eq!(
            recovered
                .get(crash_id)
                .expect("load recovered task")
                .expect("recovered task exists")
                .error,
            None
        );
    }

    fn background_delivery_counts(
        store: &DurableTaskStore,
        session_store: &SessionStore,
        task_id: &str,
    ) -> (usize, usize, usize) {
        let session = session_store.load("origin").expect("origin session");
        let events = session
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind.as_str(),
                    "background_task_completed" | "background_task_failed"
                ) && event.details.get("task_id").and_then(Value::as_str) == Some(task_id)
            })
            .count();
        let messages = session
            .messages
            .iter()
            .filter(|message| {
                serde_json::from_str::<Value>(&message.content).is_ok_and(|content| {
                    content.get("type").and_then(Value::as_str) == Some("background_task_result")
                        && content.get("task_id").and_then(Value::as_str) == Some(task_id)
                })
            })
            .count();
        let key = format!("task_id={task_id}");
        let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("daemon audit")
            .iter()
            .filter(|record| {
                record.daemon == "background_task"
                    && record.action == "deliver_observation"
                    && record
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.split(';').any(|field| field.trim() == key))
                    && matches!(record.outcome.as_str(), "completed" | "failed")
            })
            .count();
        (events, messages, audits)
    }

    fn background_delivery_counts_for_execution(
        store: &DurableTaskStore,
        session_store: &SessionStore,
        task_id: &str,
        execution_id: &str,
    ) -> (usize, usize, usize) {
        let session = session_store.load("origin").expect("origin session");
        let events = session
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind.as_str(),
                    "background_task_completed" | "background_task_failed"
                ) && event.details.get("task_id").and_then(Value::as_str) == Some(task_id)
                    && event.details.get("execution_id").and_then(Value::as_str)
                        == Some(execution_id)
            })
            .count();
        let messages = session
            .messages
            .iter()
            .filter(|message| {
                serde_json::from_str::<Value>(&message.content).is_ok_and(|content| {
                    content.get("type").and_then(Value::as_str) == Some("background_task_result")
                        && content.get("task_id").and_then(Value::as_str) == Some(task_id)
                        && content.get("execution_id").and_then(Value::as_str) == Some(execution_id)
                })
            })
            .count();
        let detail_key = format!("task_id={task_id}; execution_id={execution_id}");
        let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("daemon audit")
            .iter()
            .filter(|record| {
                record.daemon == "background_task"
                    && record.action == "deliver_observation"
                    && record.detail.as_deref().is_some_and(|detail| {
                        detail_key.split(';').all(|key_field| {
                            detail
                                .split(';')
                                .any(|detail_field| detail_field.trim() == key_field.trim())
                        })
                    })
                    && matches!(record.outcome.as_str(), "completed" | "failed")
            })
            .count();
        (events, messages, audits)
    }

    fn terminate_reconciler_at(store: &DurableTaskStore, task_id: &str, point: ReconcileHookPoint) {
        let ready = store
            .daemon_dir()
            .join(format!(".{task_id}.reconciler-ready"));
        let point = match point {
            ReconcileHookPoint::BeforeDelivery => "before_delivery",
            ReconcileHookPoint::AfterDelivery => "after_delivery",
        };
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "daemons::workload::tests::reconciliation_process_loss_child",
                "--nocapture",
            ])
            .env(RECONCILE_CHILD_DAEMON_DIR, store.daemon_dir())
            .env(RECONCILE_CHILD_TASK_ID, task_id)
            .env(RECONCILE_CHILD_POINT, point)
            .env(RECONCILE_CHILD_READY, &ready)
            .spawn()
            .expect("spawn reconciler child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect reconciler child") {
                panic!("reconciler child exited before pause: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "reconciler child did not reach {point}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().expect("terminate paused reconciler child");
        child.wait().expect("reap terminated reconciler child");
        std::fs::remove_file(ready).expect("remove reconciler readiness marker");
    }

    fn terminate_worker_publication_at(
        store: &DurableTaskStore,
        session_store: &SessionStore,
        task_id: &str,
        kind: &str,
        owner: &WorkerOwner,
        point: WorkerPublicationHookPoint,
    ) {
        let point_name = match point {
            WorkerPublicationHookPoint::BeforeEffects => "before",
            WorkerPublicationHookPoint::AfterEffects => "after",
        };
        let ready = store
            .daemon_dir()
            .join(format!(".{task_id}-{kind}-{point_name}.publication-ready"));
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "daemons::workload::tests::worker_publication_process_loss_child",
                "--nocapture",
            ])
            .env(PUBLICATION_CHILD_DAEMON_DIR, store.daemon_dir())
            .env(PUBLICATION_CHILD_SESSIONS_DIR, session_store.sessions_dir())
            .env(PUBLICATION_CHILD_TASK_ID, task_id)
            .env(PUBLICATION_CHILD_KIND, kind)
            .env(PUBLICATION_CHILD_OWNER_TOKEN, &owner.token)
            .env(PUBLICATION_CHILD_OWNER_PID, owner.pid.to_string())
            .env("NIB_TEST_WORKER_PUBLICATION_TASK_ID", task_id)
            .env("NIB_TEST_WORKER_PUBLICATION_POINT", point_name)
            .env("NIB_TEST_WORKER_PUBLICATION_READY", &ready)
            .spawn()
            .expect("spawn worker publication child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect publication child") {
                panic!("worker publication child exited before pause: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "worker publication child did not reach {point_name} effects"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().expect("terminate paused publication child");
        child.wait().expect("reap terminated publication child");
        std::fs::remove_file(ready).expect("remove publication readiness marker");
    }

    #[test]
    fn durable_admission_is_store_wide_and_exact_cap_remains_readable() {
        let (directory, store, session_store) = fixture();
        let first_store = store.clone().with_record_limit(1);
        let second_store = DurableTaskStore::at_daemon_dir(store.daemon_dir())
            .expect("second store handle")
            .with_record_limit(1);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut writers = Vec::new();
        for (id, store) in [
            ("bounded-a", first_store.clone()),
            ("bounded-b", second_store),
        ] {
            let barrier = barrier.clone();
            let request = terminal_request(&directory, &session_store, id);
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                store.prepare_terminal(request)
            }));
        }

        let results: Vec<_> = writers
            .into_iter()
            .map(|writer| writer.join().expect("admission writer"))
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let rejected = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("one admission is rejected");
        assert!(rejected.contains("1-record limit"), "{rejected}");
        assert_eq!(first_store.list().expect("list at exact cap").len(), 1);
        assert!(first_store
            .reconcile(Utc::now())
            .expect("reconcile at exact cap")
            .is_empty());
    }

    #[test]
    fn listing_bounds_aggregate_large_records_while_reconcile_streams_them() {
        let (directory, store, session_store) = fixture();
        let store = store
            .with_enumeration_byte_limit(3 * 1024 * 1024)
            .with_reconciliation_report_limit(1);
        for id in ["large-a", "large-b", "large-c"] {
            store
                .prepare_terminal(terminal_request(&directory, &session_store, id))
                .expect("prepare large record");
            store
                .update(id, |task| {
                    task.record.result = Some(json!({"output": "x".repeat(2 * 1024 * 1024)}));
                    task.record.status = "running".to_string();
                    task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                    Ok(())
                })
                .expect("persist large valid record");
            assert!(store.task_path(id).metadata().unwrap().len() < MAX_TASK_RECORD_BYTES);
            assert_eq!(
                store
                    .get(id)
                    .expect("read individual large record")
                    .and_then(|record| record.result)
                    .and_then(|result| result["output"].as_str().map(str::len)),
                Some(2 * 1024 * 1024)
            );
        }

        let error = store
            .list()
            .expect_err("materialized listing must enforce aggregate byte budget");
        assert!(error.contains("enumeration"), "{error}");
        let report = store
            .reconcile(Utc::now())
            .expect("streamed reconcile remains usable");
        assert_eq!(report.scanned_records, 3);
        assert_eq!(report.reconciled_records, 3);
        assert_eq!(report.tasks.len(), 1);
        assert_eq!(report.omitted_records, 2);
        for id in ["large-a", "large-b", "large-c"] {
            assert_eq!(store.get(id).unwrap().unwrap().status, "failed");
        }
    }

    #[test]
    fn terminal_capacity_evicts_oldest_with_audit_and_never_evicts_active() {
        let (directory, store, session_store) = fixture();
        let store = store.with_record_limit(2);
        for (id, age) in [("terminal-old", 120), ("terminal-new", 60)] {
            store
                .prepare_terminal(terminal_request(&directory, &session_store, id))
                .expect("prepare terminal candidate");
            store
                .update(id, |task| {
                    task.record.status = "failed".to_string();
                    task.record.updated_at = Utc::now() - ChronoDuration::seconds(age);
                    scrub_completed_job(&mut task.job);
                    Ok(())
                })
                .expect("terminalize candidate");
        }

        store
            .prepare_terminal(terminal_request(&directory, &session_store, "active-third"))
            .expect("old terminal record is evicted");
        assert!(store.get("terminal-old").unwrap().is_none());
        assert!(store.get("terminal-new").unwrap().is_some());
        assert_eq!(
            store.get("active-third").unwrap().unwrap().status,
            "prepared"
        );

        store
            .prepare_terminal(terminal_request(
                &directory,
                &session_store,
                "active-fourth",
            ))
            .expect("remaining terminal record is evicted before active work");
        assert!(store.get("terminal-new").unwrap().is_none());
        assert!(store.get("active-third").unwrap().is_some());
        assert!(store.get("active-fourth").unwrap().is_some());
        let error = store
            .prepare_terminal(terminal_request(&directory, &session_store, "active-fifth"))
            .expect_err("active records cannot be evicted");
        assert!(
            error.contains("no terminal record can be evicted"),
            "{error}"
        );

        let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
        let records = audit.read_all().expect("capacity audit");
        for id in ["terminal-old", "terminal-new"] {
            assert!(records.iter().any(|record| {
                record.action == "evict_terminal_task"
                    && record.target.as_deref() == Some(id)
                    && record.outcome == "planned"
            }));
            assert!(records.iter().any(|record| {
                record.action == "evict_terminal_task"
                    && record.target.as_deref() == Some(id)
                    && record.outcome == "evicted"
            }));
        }
    }

    #[test]
    fn terminal_eviction_does_not_remove_record_when_audit_cannot_start() {
        let (directory, store, session_store) = fixture();
        let store = store.with_record_limit(1);
        store
            .prepare_terminal(terminal_request(
                &directory,
                &session_store,
                "terminal-retained",
            ))
            .expect("prepare terminal candidate");
        store
            .fail_prepared("terminal-retained", "done".to_string())
            .expect("terminalize candidate");
        std::fs::create_dir(store.daemon_dir().join("audit.jsonl"))
            .expect("block audit publication");

        let error = store
            .prepare_terminal(terminal_request(
                &directory,
                &session_store,
                "replacement-rejected",
            ))
            .expect_err("unaudited eviction is rejected");
        assert!(error.contains("regular local file"), "{error}");
        assert!(store.get("terminal-retained").unwrap().is_some());
        assert!(store.get("replacement-rejected").unwrap().is_none());
    }

    #[test]
    fn prepared_rollback_removes_record_and_frees_admission_capacity() {
        let (directory, store, session_store) = fixture();
        let store = store.with_record_limit(1);
        store
            .prepare_terminal(terminal_request(
                &directory,
                &session_store,
                "rollback-first",
            ))
            .expect("first admission");

        assert!(store
            .remove_prepared("rollback-first")
            .expect("remove prepared record"));
        assert!(store.get("rollback-first").unwrap().is_none());
        store
            .prepare_terminal(terminal_request(
                &directory,
                &session_store,
                "rollback-second",
            ))
            .expect("capacity was released");
        assert_eq!(store.list().expect("list replacement").len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn detached_worker_configuration_uses_a_process_group_and_reaper() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_worker_process(&mut command);
        let child = command.spawn().expect("detached child");
        let pid = child.id();
        let process = Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .expect("inspect child process group");
        assert!(process.status.success());
        let process_group: u32 = String::from_utf8_lossy(&process.stdout)
            .trim()
            .parse()
            .expect("numeric process group");
        assert_eq!(process_group, pid);

        hand_off_worker(worker_reaper_sender().expect("worker reaper"), child)
            .expect("reaper accepts child");
        let started = Instant::now();
        while Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "child {pid} was not reaped"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[cfg(unix)]
    #[test]
    fn reaper_kills_and_removes_children_after_poll_errors() {
        let child = Command::new("sh")
            .args(["-c", "sleep 60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("long-running child");
        let pid = child.id();
        let mut children = vec![child];

        reap_worker_children_once(&mut children, |_| {
            Err(std::io::Error::other("forced poll failure"))
        });

        assert!(children.is_empty());
        assert!(!Command::new("ps")
            .args(["-p", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success()));
    }

    #[test]
    fn durable_records_roundtrip_and_reconcile_stale_workers() {
        let (directory, store, session_store) = fixture();
        let record = store
            .prepare_terminal(DurableTerminalRequest {
                id: "terminal-roundtrip".to_string(),
                command: "printf ok".to_string(),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                execution: ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
            .expect("prepare task");
        assert_eq!(record.status, "prepared");
        assert_eq!(store.list().unwrap().len(), 1);

        store
            .update("terminal-roundtrip", |task| {
                task.record.status = "running".to_string();
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                Ok(())
            })
            .unwrap();
        let reconciled = store.reconcile(Utc::now()).unwrap();
        assert_eq!(reconciled.reconciled_records, 1);
        assert_eq!(reconciled.tasks[0].status, "failed");
        assert!(reconciled.tasks[0]
            .error
            .as_deref()
            .unwrap()
            .contains("not replayed"));
    }

    #[test]
    fn prepared_task_can_be_cancelled_without_a_worker() {
        let (directory, store, session_store) = fixture();
        store
            .prepare_terminal(DurableTerminalRequest {
                id: "cancel-prepared".to_string(),
                command: "sleep 60".to_string(),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                execution: ExecutionConfig::default(),
                timeout_secs: 70,
                max_output_bytes: 1024,
            })
            .unwrap();
        let cancelled = store.cancel("cancel-prepared").unwrap();
        assert_eq!(cancelled.status, "cancelled");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_worker_rejects_transient_config_with_reduced_redaction_set() {
        let directory = tempdir().expect("tempdir");
        let project_root = directory.path();
        let secret = "canonical-worker-secret-value";
        let mut canonical = crate::config::NibConfig::default();
        canonical.llm.providers.insert(
            "credential-source".to_string(),
            crate::config::ProviderEntry {
                model: "model".to_string(),
                api_key: Some(secret.to_string()),
                ..crate::config::ProviderEntry::default()
            },
        );
        canonical.profiles.default = "default".to_string();
        canonical.profiles.active = vec![crate::config::ProfileConfig {
            id: "default".to_string(),
            root: PathBuf::from("."),
            state_dir: Some(PathBuf::from("state")),
            ..crate::config::ProfileConfig::default()
        }];
        crate::config::save_nib_config_full(project_root, &mut canonical)
            .expect("canonical worker config");

        let sessions_dir = project_root.join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions directory");
        let session_store = SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let store = DurableTaskStore::at_daemon_dir(project_root.join("state/daemons"))
            .expect("durable store");
        let task_id = "transient-config-redaction";
        let prepared = store
            .prepare_terminal(DurableTerminalRequest {
                id: task_id.to_string(),
                command: format!("printf %s {secret}"),
                cwd: project_root.to_path_buf(),
                project_root: project_root.to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: sessions_dir.clone(),
                session_id: "origin".to_string(),
                execution: ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
            .expect("prepare terminal worker");
        session_store
            .record_tool_call(ToolCallRecord {
                id: Some("call-transient-config-redaction".to_string()),
                session_id: Some("origin".to_string()),
                tool_name: Some("terminal".to_string()),
                result: Some(json!({
                    "success": true,
                    "output": {
                        "task_id": task_id,
                        "execution_id": prepared.execution_id,
                        "status": "started",
                    },
                })),
                ..ToolCallRecord::default()
            })
            .expect("originating tool call");
        let owner = WorkerOwner {
            token: "transient-config-owner".to_string(),
            pid: std::process::id(),
        };
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("bind worker owner");

        let paths = crate::config::config_paths(project_root);
        let displaced = paths.nib_dir.join("config.toml.canonical");
        std::fs::rename(&paths.toml, &displaced).expect("displace canonical config");
        let mut forged = canonical.clone();
        forged
            .llm
            .providers
            .get_mut("credential-source")
            .expect("credential provider")
            .api_key = None;
        std::fs::write(
            &paths.toml,
            toml::to_string_pretty(&forged).expect("forged config"),
        )
        .expect("publish forged config");
        let restore_path = paths.toml.clone();
        let restore_displaced = displaced.clone();
        let _hook = crate::config::install_config_read_hook(paths.toml.clone(), move |_| {
            std::fs::remove_file(&restore_path).map_err(|error| error.to_string())?;
            std::fs::rename(&restore_displaced, &restore_path).map_err(|error| error.to_string())
        });
        let worker_job = || TerminalWorkerJob {
            command: format!("printf %s {secret}"),
            cwd: project_root.to_path_buf(),
            project_root: project_root.to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: sessions_dir.clone(),
            session_id: "origin".to_string(),
            execution: ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        };

        let error = run_terminal_worker(&store, &owner, task_id, worker_job())
            .await
            .expect_err("transient config must fail before terminal execution");
        assert!(error.contains("identity changed"), "{error}");
        let still_running = store.get(task_id).unwrap().expect("running record");
        assert_eq!(still_running.status, "running");
        assert!(still_running.result.is_none());

        run_terminal_worker(&store, &owner, task_id, worker_job())
            .await
            .expect("canonical worker run");
        let completed = store.get(task_id).unwrap().expect("completed record");
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.result.as_ref().unwrap()["stdout"], "[REDACTED]");
        assert!(!serde_json::to_string(&completed).unwrap().contains(secret));
        assert!(
            !serde_json::to_string(&session_store.load("origin").unwrap())
                .unwrap()
                .contains(secret)
        );
    }

    #[tokio::test]
    async fn long_running_worker_heartbeats_before_concurrent_reconcile() {
        let (directory, store, session_store) = fixture();
        let task_id = "heartbeat-schedule";
        prepare_schedule_fixture(&directory, &store, &session_store, task_id);
        let owner = WorkerOwner {
            token: "heartbeat-lease".to_string(),
            pid: 42_001,
        };
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed stale running lease");

        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let monitor_store = store.clone();
        let monitor_owner = owner.clone();
        let monitor_started = started.clone();
        let cancellation = crate::agent::CancellationSignal::new();
        let monitor = tokio::spawn(async move {
            monitor_worker_future(
                &monitor_store,
                &monitor_owner,
                task_id,
                &cancellation,
                async move {
                    monitor_started.notify_one();
                    sleep(Duration::from_millis(300)).await;
                    json!({"outcome": "complete"})
                },
            )
            .await
        });

        started.notified().await;
        assert!(store
            .reconcile(Utc::now())
            .expect("concurrent reconcile")
            .is_empty());
        let run = monitor
            .await
            .expect("monitor joins")
            .expect("monitor result");
        let MonitoredRun::Completed(run) = run else {
            panic!("active worker lease was lost")
        };
        let completed = store
            .update_schedule_progress_owned(&owner, task_id, 1, None, run)
            .expect("lease owner commits progress");
        assert_eq!(completed.status, "completed");
    }

    #[test]
    fn reconciler_revokes_lease_and_fences_late_worker_commit() {
        let (directory, store, session_store) = fixture();
        let task_id = "fenced-schedule";
        prepare_schedule_fixture(&directory, &store, &session_store, task_id);
        let owner = WorkerOwner {
            token: "expired-lease".to_string(),
            pid: 42_002,
        };
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed expired lease");

        let reconciled = store
            .reconcile(Utc::now())
            .expect("reconcile expired lease");
        assert_eq!(reconciled.reconciled_records, 1);
        assert_eq!(reconciled.tasks[0].status, "failed");
        let late_commit = store.update_schedule_progress_owned(
            &owner,
            task_id,
            1,
            None,
            json!({"outcome": "late"}),
        );
        assert!(late_commit
            .expect_err("revoked owner must be fenced")
            .contains("worker lease lost"));
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
    }

    #[test]
    fn reconciliation_process_loss_child() {
        let Some(daemon_dir) = std::env::var_os(RECONCILE_CHILD_DAEMON_DIR) else {
            return;
        };
        let task_id = std::env::var(RECONCILE_CHILD_TASK_ID).expect("child task ID");
        let point = std::env::var(RECONCILE_CHILD_POINT).expect("child pause point");
        let ready =
            PathBuf::from(std::env::var_os(RECONCILE_CHILD_READY).expect("child readiness path"));
        let store = DurableTaskStore::at_daemon_dir(daemon_dir).expect("child durable store");
        let result = store.reconcile_with_hook(Utc::now(), |hook_point, id| {
            let selected = matches!(
                (point.as_str(), hook_point),
                ("before_delivery", ReconcileHookPoint::BeforeDelivery)
                    | ("after_delivery", ReconcileHookPoint::AfterDelivery)
            );
            if selected && id == task_id {
                std::fs::write(&ready, b"ready")
                    .map_err(|error| format!("failed to publish child readiness: {error}"))?;
                std::thread::sleep(Duration::from_secs(60));
            }
            Ok(())
        });
        panic!("reconciler child was not terminated at {point}: {result:?}");
    }

    #[test]
    fn worker_publication_process_loss_child() {
        let Some(daemon_dir) = std::env::var_os(PUBLICATION_CHILD_DAEMON_DIR) else {
            return;
        };
        let sessions_dir = PathBuf::from(
            std::env::var_os(PUBLICATION_CHILD_SESSIONS_DIR).expect("child sessions directory"),
        );
        let task_id = std::env::var(PUBLICATION_CHILD_TASK_ID).expect("child task ID");
        let kind = std::env::var(PUBLICATION_CHILD_KIND).expect("child publication kind");
        let owner = WorkerOwner {
            token: std::env::var(PUBLICATION_CHILD_OWNER_TOKEN).expect("child owner token"),
            pid: std::env::var(PUBLICATION_CHILD_OWNER_PID)
                .expect("child owner pid")
                .parse()
                .expect("numeric child owner pid"),
        };
        let store = DurableTaskStore::at_daemon_dir(daemon_dir).expect("child durable store");
        let session_store = SessionStore::at_dir(sessions_dir);
        let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
        let result = match kind.as_str() {
            "terminal" => publish_terminal_outcome_owned(
                &store,
                &owner,
                &task_id,
                &BackgroundTaskSession {
                    session_store,
                    session_id: "origin".to_string(),
                    audit_log: audit,
                },
                true,
                json!({"exit_code": 0, "stdout": "published before process loss"}),
                None,
            )
            .map(|_| ()),
            "schedule" => publish_schedule_completion_owned(
                &store,
                &owner,
                &task_id,
                &session_store,
                &audit,
                "origin",
                ScheduleCompletion {
                    occurrence: 1,
                    repeat_count: 1,
                    outcome: "plan_ready",
                    steps_taken: 1,
                    next_run_at: None,
                    run: json!({"occurrence": 1, "outcome": "plan_ready"}),
                },
            )
            .map(|_| ()),
            value => panic!("unsupported child publication kind: {value}"),
        };
        panic!("worker publication child was not terminated: {result:?}");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn actual_worker_publication_loss_is_reconciled_once_and_fences_late_owners() {
        for point in [
            WorkerPublicationHookPoint::BeforeEffects,
            WorkerPublicationHookPoint::AfterEffects,
        ] {
            let (directory, store, session_store) = fixture();
            let suffix = match point {
                WorkerPublicationHookPoint::BeforeEffects => "before",
                WorkerPublicationHookPoint::AfterEffects => "after",
            };
            let task_id = format!("terminal-worker-loss-{suffix}");
            let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, &task_id);
            let execution_id = store
                .get(&task_id)
                .expect("terminal record")
                .expect("terminal task")
                .execution_id;

            terminate_worker_publication_at(
                &store,
                &session_store,
                &task_id,
                "terminal",
                &owner,
                point,
            );
            let expected_before_reconcile = if point == WorkerPublicationHookPoint::BeforeEffects {
                (0, 0, 0)
            } else {
                (1, 1, 1)
            };
            assert_eq!(
                background_delivery_counts_for_execution(
                    &store,
                    &session_store,
                    &task_id,
                    &execution_id,
                ),
                expected_before_reconcile
            );

            let report = store
                .reconcile(Utc::now())
                .expect("reconcile killed terminal publisher");
            assert_eq!(report.reconciled_records, 1);
            assert_eq!(
                background_delivery_counts_for_execution(
                    &store,
                    &session_store,
                    &task_id,
                    &execution_id,
                ),
                (1, 1, 1)
            );
            let late = publish_terminal_outcome_owned(
                &store,
                &owner,
                &task_id,
                &BackgroundTaskSession {
                    session_store: session_store.clone(),
                    session_id: "origin".to_string(),
                    audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
                },
                true,
                json!({"exit_code": 0, "stdout": "late"}),
                None,
            )
            .expect_err("reconciler must fence the killed terminal owner");
            assert!(late.contains("worker lease lost"), "{late}");
        }

        for point in [
            WorkerPublicationHookPoint::BeforeEffects,
            WorkerPublicationHookPoint::AfterEffects,
        ] {
            let (directory, store, session_store) = fixture();
            let suffix = match point {
                WorkerPublicationHookPoint::BeforeEffects => "before",
                WorkerPublicationHookPoint::AfterEffects => "after",
            };
            let task_id = format!("schedule-worker-loss-{suffix}");
            prepare_schedule_fixture(&directory, &store, &session_store, &task_id);
            let owner = WorkerOwner {
                token: format!("schedule-worker-loss-{suffix}-lease"),
                pid: 42_600,
            };
            store
                .update(&task_id, |task| {
                    task.record.status = "running".to_string();
                    task.record.worker_pid = Some(owner.pid);
                    task.worker_lease = Some(WorkerLease {
                        token: owner.token.clone(),
                    });
                    Ok(())
                })
                .expect("seed schedule worker owner");
            let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
            let job = ScheduleWorkerJob {
                prompt: "scheduled plan".to_string(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                interval_secs: 1,
                repeat_count: 1,
            };
            assert_eq!(
                publish_schedule_wake_owned(
                    &store,
                    &owner,
                    &task_id,
                    &session_store,
                    &audit,
                    &job,
                    1,
                )
                .expect("publish schedule wake before terminal process loss"),
                ScheduleWakePublication::Started
            );
            store
                .update(&task_id, |task| {
                    task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                    Ok(())
                })
                .expect("age schedule worker after wake");
            let execution_id = store
                .get(&task_id)
                .expect("schedule record")
                .expect("schedule task")
                .execution_id;
            let terminal_key = schedule_delivery_key(&task_id, &execution_id, 1, "terminal");

            terminate_worker_publication_at(
                &store,
                &session_store,
                &task_id,
                "schedule",
                &owner,
                point,
            );
            let terminal_counts = || {
                let session = session_store.load("origin").expect("origin session");
                let events = session
                    .events
                    .iter()
                    .filter(|event| {
                        matches!(
                            event.kind.as_str(),
                            "scheduled_agent_run_completed" | "scheduled_agent_run_failed"
                        ) && event.details.get("delivery_key").and_then(Value::as_str)
                            == Some(terminal_key.as_str())
                    })
                    .count();
                let audits = audit
                    .read_all()
                    .expect("schedule audit")
                    .iter()
                    .filter(|record| {
                        record
                            .detail
                            .as_deref()
                            .is_some_and(|detail| detail.contains(&terminal_key))
                    })
                    .count();
                (events, audits)
            };
            assert_eq!(
                terminal_counts(),
                if point == WorkerPublicationHookPoint::BeforeEffects {
                    (0, 0)
                } else {
                    (1, 1)
                }
            );

            let report = store
                .reconcile(Utc::now())
                .expect("reconcile killed schedule publisher");
            assert_eq!(report.reconciled_records, 1);
            assert_eq!(terminal_counts(), (1, 1));
            let late = publish_schedule_completion_owned(
                &store,
                &owner,
                &task_id,
                &session_store,
                &audit,
                "origin",
                ScheduleCompletion {
                    occurrence: 1,
                    repeat_count: 1,
                    outcome: "late",
                    steps_taken: 1,
                    next_run_at: None,
                    run: json!({"occurrence": 1, "outcome": "late"}),
                },
            )
            .expect_err("reconciler must fence the killed schedule owner");
            assert!(late.contains("worker lease lost"), "{late}");
        }
    }

    #[test]
    fn reconciling_record_resumes_after_loss_before_delivery_and_keeps_worker_fenced() {
        let (directory, store, session_store) = fixture();
        let task_id = "resume-before-delivery";
        let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);

        terminate_reconciler_at(&store, task_id, ReconcileHookPoint::BeforeDelivery);
        let claimed = store
            .get_file(task_id)
            .expect("durable reconciliation claim");
        assert_eq!(claimed.record.status, "reconciling");
        assert!(claimed.worker_lease.is_none());
        assert_eq!(
            background_delivery_counts(&store, &session_store, task_id),
            (0, 0, 0)
        );

        let late_commit = store.finish_owned(
            &owner,
            task_id,
            "completed",
            Some(json!({"outcome": "late"})),
            None,
        );
        assert!(late_commit
            .expect_err("revoked worker remains fenced after reconciler loss")
            .contains("worker lease lost"));

        let report = store
            .reconcile(Utc::now())
            .expect("later reconciler resumes durable claim");
        assert_eq!(report.reconciled_records, 1);
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
        assert_eq!(
            background_delivery_counts(&store, &session_store, task_id),
            (1, 1, 1)
        );
    }

    #[test]
    fn reconciling_record_resumes_after_loss_without_repeating_delivered_side_effects() {
        let (directory, store, session_store) = fixture();
        let task_id = "resume-after-delivery";
        prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);

        terminate_reconciler_at(&store, task_id, ReconcileHookPoint::AfterDelivery);
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "reconciling");
        assert_eq!(
            background_delivery_counts(&store, &session_store, task_id),
            (1, 1, 1)
        );

        let report = store
            .reconcile(Utc::now())
            .expect("later reconciler resumes delivered claim");
        assert_eq!(report.reconciled_records, 1);
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
        assert_eq!(
            background_delivery_counts(&store, &session_store, task_id),
            (1, 1, 1)
        );
    }

    #[test]
    fn schedule_reconciliation_resumes_after_killed_delivery_without_duplicates() {
        let (directory, store, session_store) = fixture();
        let task_id = "resume-schedule-after-delivery";
        prepare_schedule_fixture(&directory, &store, &session_store, task_id);
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(42_200);
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                task.worker_lease = Some(WorkerLease {
                    token: "schedule-reconcile-lease".to_string(),
                });
                Ok(())
            })
            .expect("seed stale schedule worker");

        terminate_reconciler_at(&store, task_id, ReconcileHookPoint::AfterDelivery);
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "reconciling");
        let count_deliveries = || {
            let session = session_store.load("origin").expect("origin session");
            let events = session
                .events
                .iter()
                .filter(|event| {
                    event.kind == "scheduled_agent_run_failed"
                        && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                        && event.details.get("reconciled").and_then(Value::as_bool) == Some(true)
                })
                .count();
            let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
                .read_all()
                .expect("daemon audit")
                .iter()
                .filter(|record| {
                    record.action == "reconcile_expired_worker"
                        && record
                            .detail
                            .as_deref()
                            .is_some_and(|detail| detail.contains(&format!("timer_id={task_id}")))
                })
                .count();
            (events, audits)
        };
        assert_eq!(count_deliveries(), (1, 1));

        let report = store
            .reconcile(Utc::now())
            .expect("later reconciler resumes schedule claim");
        assert_eq!(report.reconciled_records, 1);
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
        assert_eq!(count_deliveries(), (1, 1));
    }

    #[test]
    fn schedule_reconciliation_resumes_after_loss_before_delivery_and_fences_owner() {
        let (directory, store, session_store) = fixture();
        let task_id = "resume-schedule-before-delivery";
        prepare_schedule_fixture(&directory, &store, &session_store, task_id);
        let owner = WorkerOwner {
            token: "schedule-before-reconcile-lease".to_string(),
            pid: 42_225,
        };
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed stale schedule worker");

        terminate_reconciler_at(&store, task_id, ReconcileHookPoint::BeforeDelivery);
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "reconciling");
        let session = session_store.load("origin").expect("origin session");
        assert!(!session.events.iter().any(|event| {
            event.kind == "scheduled_agent_run_failed"
                && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                && event.details.get("reconciled").and_then(Value::as_bool) == Some(true)
        }));
        let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
        assert!(!audit
            .read_all()
            .expect("daemon audit")
            .iter()
            .any(|record| {
                record.action == "reconcile_expired_worker"
                    && record
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.contains(&format!("timer_id={task_id}")))
            }));

        let late = publish_schedule_failure_owned(
            &store,
            &owner,
            task_id,
            &session_store,
            &audit,
            ScheduleFailure {
                session_id: "origin",
                occurrence: 1,
                repeat_count: 1,
                error: "late owner".to_string(),
            },
        )
        .expect_err("reconciliation claim must fence the prior schedule owner");
        assert!(late.contains("worker lease lost"), "{late}");

        let report = store
            .reconcile(Utc::now())
            .expect("later reconciler resumes schedule claim");
        assert_eq!(report.reconciled_records, 1);
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
        let session = session_store.load("origin").expect("origin session");
        assert_eq!(
            session
                .events
                .iter()
                .filter(|event| {
                    event.kind == "scheduled_agent_run_failed"
                        && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                        && event.details.get("reconciled").and_then(Value::as_bool) == Some(true)
                })
                .count(),
            1
        );
        assert_eq!(
            audit
                .read_all()
                .expect("daemon audit")
                .iter()
                .filter(|record| {
                    record.action == "reconcile_expired_worker"
                        && record
                            .detail
                            .as_deref()
                            .is_some_and(|detail| detail.contains(&format!("timer_id={task_id}")))
                })
                .count(),
            1
        );
    }

    #[test]
    fn schedule_reconciliation_preserves_a_prior_terminal_observation() {
        let (directory, store, session_store) = fixture();
        let task_id = "reconcile-after-schedule-completion";
        prepare_schedule_fixture(&directory, &store, &session_store, task_id);
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(42_250);
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                task.worker_lease = Some(WorkerLease {
                    token: "schedule-completed-before-loss".to_string(),
                });
                task.active_occurrence = Some(1);
                Ok(())
            })
            .expect("seed stale schedule worker");
        let execution_id = store
            .get_file(task_id)
            .expect("schedule execution")
            .record
            .execution_id;
        let delivery_key = schedule_delivery_key(task_id, &execution_id, 1, "terminal");
        record_schedule_event_once(
            &session_store,
            "origin",
            "scheduled_agent_run_completed",
            json!({
                "timer_id": task_id,
                "execution_id": execution_id,
                "occurrence": 1,
                "repeat_count": 1,
                "outcome": "plan_ready",
                "steps_taken": 1,
                "mode": "plan",
            }),
            &delivery_key,
        )
        .expect("seed prior keyed completion event");
        let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));

        let report = store
            .reconcile(Utc::now())
            .expect("reconcile after completion publication");
        assert_eq!(report.reconciled_records, 1);
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
        let session = session_store.load("origin").expect("origin session");
        let terminal_events = session
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind.as_str(),
                    "scheduled_agent_run_completed" | "scheduled_agent_run_failed"
                ) && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                    && event.details.get("occurrence").and_then(Value::as_u64) == Some(1)
            })
            .collect::<Vec<_>>();
        assert_eq!(terminal_events.len(), 1);
        assert_eq!(terminal_events[0].kind, "scheduled_agent_run_completed");
        let audits = audit.read_all().expect("daemon audit");
        assert_eq!(
            audits
                .iter()
                .filter(|record| {
                    record
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.contains(&delivery_key))
                })
                .count(),
            1
        );
        assert!(audits.iter().any(|record| {
            record.action == "wake_agent_loop"
                && record.outcome == "completed"
                && record
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains(&delivery_key))
        }));
        assert!(!audits.iter().any(|record| {
            record.action == "reconcile_expired_worker"
                && record
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains(&format!("timer_id={task_id}")))
        }));
    }

    #[test]
    fn terminal_publication_revalidates_owner_before_effect_and_transition() {
        let (directory, store, session_store) = fixture();
        let task_id = "revoked-terminal-publication";
        let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
        store
            .poll_worker_owned(task_id, &owner, true)
            .expect("worker performs its pre-publication poll");
        store
            .update(task_id, |task| {
                task.record.status = "reconciling".to_string();
                task.record.worker_pid = None;
                task.worker_lease = None;
                Ok(())
            })
            .expect("revoke worker after prior poll");
        let target = BackgroundTaskSession {
            session_store: session_store.clone(),
            session_id: "origin".to_string(),
            audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
        };

        let error = publish_terminal_outcome_owned(
            &store,
            &owner,
            task_id,
            &target,
            true,
            json!({"exit_code": 0, "stdout": "late"}),
            None,
        )
        .expect_err("revoked worker cannot enter publication closure");
        assert!(error.contains("worker lease lost"), "{error}");
        assert_eq!(
            background_delivery_counts(&store, &session_store, task_id),
            (0, 0, 0)
        );
        assert_eq!(store.get(task_id).unwrap().unwrap().status, "reconciling");
    }

    #[test]
    fn valid_terminal_success_and_cancel_publish_with_their_durable_transition() {
        let (directory, store, session_store) = fixture();
        let success_id = "owned-terminal-success";
        let success_owner =
            prepare_reconcilable_terminal(&directory, &store, &session_store, success_id);
        let target = BackgroundTaskSession {
            session_store: session_store.clone(),
            session_id: "origin".to_string(),
            audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
        };
        let completed = publish_terminal_outcome_owned(
            &store,
            &success_owner,
            success_id,
            &target,
            true,
            json!({"exit_code": 0, "stdout": "ok"}),
            None,
        )
        .expect("owned success publication");
        assert_eq!(completed.status, "completed");
        assert_eq!(
            background_delivery_counts(&store, &session_store, success_id),
            (1, 1, 1)
        );

        let cancel_id = "owned-terminal-cancel";
        let cancel_owner =
            prepare_reconcilable_terminal(&directory, &store, &session_store, cancel_id);
        store
            .update(cancel_id, |task| {
                task.record.cancel_requested = true;
                Ok(())
            })
            .expect("request terminal cancellation");
        let claimed = store
            .get_file(cancel_id)
            .expect("claimed cancellation task");
        let cancelled = publish_claimed_cancellation(&store, &cancel_owner, cancel_id, &claimed)
            .expect("owned cancellation publication");
        assert_eq!(cancelled.status, "cancelled");
        assert_eq!(
            background_delivery_counts(&store, &session_store, cancel_id),
            (1, 1, 1)
        );
    }

    #[test]
    fn reused_terminal_id_delivers_each_execution_exactly_once() {
        let (directory, store, session_store) = fixture();
        let task_id = "reused-terminal";
        let target = BackgroundTaskSession {
            session_store: session_store.clone(),
            session_id: "origin".to_string(),
            audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
        };

        let first_owner =
            prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
        let first_execution = store
            .get(task_id)
            .expect("first record")
            .expect("first task")
            .execution_id;
        publish_terminal_outcome_owned(
            &store,
            &first_owner,
            task_id,
            &target,
            true,
            json!({"exit_code": 0, "stdout": "first"}),
            None,
        )
        .expect("publish first execution");
        store
            .evict_terminal_record(task_id)
            .expect("evict first terminal execution");

        let second_owner =
            prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
        let second_execution = store
            .get(task_id)
            .expect("second record")
            .expect("second task")
            .execution_id;
        assert_ne!(first_execution, second_execution);
        publish_terminal_outcome_owned(
            &store,
            &second_owner,
            task_id,
            &target,
            true,
            json!({"exit_code": 0, "stdout": "second"}),
            None,
        )
        .expect("publish second execution");

        assert_eq!(
            background_delivery_counts_for_execution(
                &store,
                &session_store,
                task_id,
                &first_execution,
            ),
            (1, 1, 1)
        );
        assert_eq!(
            background_delivery_counts_for_execution(
                &store,
                &session_store,
                task_id,
                &second_execution,
            ),
            (1, 1, 1)
        );
        assert_eq!(
            background_delivery_counts(&store, &session_store, task_id),
            (2, 2, 2)
        );
    }

    #[test]
    fn legacy_execution_generation_is_deterministic_persisted_and_dedupes_old_evidence() {
        let (directory, store, session_store) = fixture();
        let task_id = "legacy-generation";
        prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
        let task_path = store.task_path(task_id);
        let mut encoded: Value =
            serde_json::from_slice(&std::fs::read(&task_path).expect("read modern task record"))
                .expect("decode modern task record");
        encoded
            .get_mut("record")
            .and_then(Value::as_object_mut)
            .expect("record object")
            .remove("execution_id");
        std::fs::write(
            &task_path,
            serde_json::to_vec_pretty(&encoded).expect("encode legacy task record"),
        )
        .expect("write legacy task record");

        session_store
            .update_session("origin", |session| {
                for tool_call in &mut session.tool_calls {
                    if let Some(output) = tool_call
                        .result
                        .as_mut()
                        .and_then(|result| result.get_mut("output"))
                        .and_then(Value::as_object_mut)
                    {
                        output.remove("execution_id");
                    }
                }
                session.messages.push(crate::session::SessionMessage {
                    index: session.messages.len(),
                    role: "user".to_string(),
                    content: "legacy background task context".to_string(),
                    timestamp: Some(Utc::now()),
                });
                session.messages.push(crate::session::SessionMessage {
                    index: session.messages.len(),
                    role: "assistant".to_string(),
                    content: "legacy boundary".to_string(),
                    timestamp: Some(Utc::now()),
                });
                session.messages.push(crate::session::SessionMessage {
                    index: session.messages.len(),
                    role: "tool".to_string(),
                    content: json!({
                        "type": "background_task_result",
                        "task_id": task_id,
                        "success": false,
                        "error": "legacy worker loss",
                    })
                    .to_string(),
                    timestamp: Some(Utc::now()),
                });
                session.events.push(SessionEvent {
                    index: session.events.len(),
                    kind: "background_task_failed".to_string(),
                    details: json!({
                        "task_id": task_id,
                        "success": false,
                        "error": "legacy worker loss",
                    }),
                    timestamp: Some(Utc::now()),
                });
                Ok(())
            })
            .expect("seed pre-generation session evidence");
        let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
        audit
            .append(&DaemonAuditRecord {
                timestamp: Utc::now(),
                daemon: "background_task".to_string(),
                action: "deliver_observation".to_string(),
                target: Some("origin".to_string()),
                outcome: "failed".to_string(),
                authorized: true,
                detail: Some(format!("task_id={task_id}; error=legacy worker loss")),
            })
            .expect("seed pre-generation daemon audit");

        let migrated =
            DurableTaskStore::at_daemon_dir(store.daemon_dir()).expect("migrate legacy generation");
        let first_generation = migrated
            .get(task_id)
            .expect("migrated record")
            .expect("migrated task")
            .execution_id;
        assert!(first_generation.starts_with("legacy-"));
        let persisted: Value =
            serde_json::from_slice(&std::fs::read(&task_path).expect("read migrated task record"))
                .expect("decode migrated task record");
        assert_eq!(
            persisted
                .get("record")
                .and_then(|record| record.get("execution_id"))
                .and_then(Value::as_str),
            Some(first_generation.as_str())
        );
        let restarted = DurableTaskStore::at_daemon_dir(store.daemon_dir())
            .expect("restart after generation migration");
        assert_eq!(
            restarted
                .get(task_id)
                .expect("restarted record")
                .expect("restarted task")
                .execution_id,
            first_generation
        );

        let report = restarted
            .reconcile(Utc::now())
            .expect("reconcile migrated legacy task");
        assert_eq!(report.reconciled_records, 1);
        assert_eq!(
            background_delivery_counts(&restarted, &session_store, task_id),
            (1, 1, 1)
        );
        assert_eq!(
            background_delivery_counts_for_execution(
                &restarted,
                &session_store,
                task_id,
                &first_generation,
            ),
            (0, 0, 0),
            "legacy evidence remains unmodified and is not duplicated"
        );
    }

    #[test]
    fn reused_schedule_id_keeps_wake_and_terminal_delivery_keys_generation_scoped() {
        fn publish_execution(
            directory: &tempfile::TempDir,
            store: &DurableTaskStore,
            session_store: &SessionStore,
            task_id: &str,
            owner_token: &str,
            stdout: &str,
        ) -> String {
            prepare_schedule_fixture(directory, store, session_store, task_id);
            let owner = WorkerOwner {
                token: owner_token.to_string(),
                pid: 42_700,
            };
            store
                .update(task_id, |task| {
                    task.record.status = "running".to_string();
                    task.record.worker_pid = Some(owner.pid);
                    task.worker_lease = Some(WorkerLease {
                        token: owner.token.clone(),
                    });
                    Ok(())
                })
                .expect("seed schedule owner");
            let execution_id = store
                .get(task_id)
                .expect("schedule record")
                .expect("schedule task")
                .execution_id;
            let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
            let job = ScheduleWorkerJob {
                prompt: "scheduled plan".to_string(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                interval_secs: 1,
                repeat_count: 1,
            };
            assert_eq!(
                publish_schedule_wake_owned(
                    store,
                    &owner,
                    task_id,
                    session_store,
                    &audit,
                    &job,
                    1,
                )
                .expect("publish schedule wake"),
                ScheduleWakePublication::Started
            );
            publish_schedule_completion_owned(
                store,
                &owner,
                task_id,
                session_store,
                &audit,
                "origin",
                ScheduleCompletion {
                    occurrence: 1,
                    repeat_count: 1,
                    outcome: "plan_ready",
                    steps_taken: 1,
                    next_run_at: None,
                    run: json!({"occurrence": 1, "outcome": "plan_ready", "stdout": stdout}),
                },
            )
            .expect("publish schedule completion");
            execution_id
        }

        let (directory, store, session_store) = fixture();
        let task_id = "reused-schedule";
        let first_execution = publish_execution(
            &directory,
            &store,
            &session_store,
            task_id,
            "first-schedule-owner",
            "first",
        );
        store
            .evict_terminal_record(task_id)
            .expect("evict first schedule execution");
        let second_execution = publish_execution(
            &directory,
            &store,
            &session_store,
            task_id,
            "second-schedule-owner",
            "second",
        );
        assert_ne!(first_execution, second_execution);

        let session = session_store.load("origin").expect("origin session");
        let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("daemon audit");
        for execution_id in [&first_execution, &second_execution] {
            for phase in ["start", "terminal"] {
                let key = schedule_delivery_key(task_id, execution_id, 1, phase);
                assert_eq!(
                    session
                        .events
                        .iter()
                        .filter(|event| {
                            event.details.get("delivery_key").and_then(Value::as_str)
                                == Some(key.as_str())
                        })
                        .count(),
                    1,
                    "missing generation-scoped {phase} event for {execution_id}"
                );
                assert_eq!(
                    audits
                        .iter()
                        .filter(|record| {
                            record
                                .detail
                                .as_deref()
                                .is_some_and(|detail| detail.contains(&key))
                        })
                        .count(),
                    1,
                    "missing generation-scoped {phase} audit for {execution_id}"
                );
            }
        }
    }

    #[test]
    fn schedule_publications_are_fenced_and_phase_consistent() {
        let (directory, store, session_store) = fixture();
        let task_id = "owned-schedule-success";
        prepare_schedule_fixture(&directory, &store, &session_store, task_id);
        let owner = WorkerOwner {
            token: "owned-schedule-lease".to_string(),
            pid: 42_300,
        };
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed owned schedule");
        let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
        let job = ScheduleWorkerJob {
            prompt: "scheduled plan".to_string(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            interval_secs: 1,
            repeat_count: 1,
        };
        assert_eq!(
            publish_schedule_wake_owned(&store, &owner, task_id, &session_store, &audit, &job, 1,)
                .expect("owned wake publication"),
            ScheduleWakePublication::Started
        );
        assert_eq!(store.get_file(task_id).unwrap().active_occurrence, Some(1));
        let completed = publish_schedule_completion_owned(
            &store,
            &owner,
            task_id,
            &session_store,
            &audit,
            "origin",
            ScheduleCompletion {
                occurrence: 1,
                repeat_count: 1,
                outcome: "plan_ready",
                steps_taken: 1,
                next_run_at: None,
                run: json!({"occurrence": 1, "outcome": "plan_ready"}),
            },
        )
        .expect("owned completion publication");
        assert_eq!(completed.status, "completed");
        assert!(store.get_file(task_id).unwrap().active_occurrence.is_none());

        let revoked_id = "revoked-schedule-wake";
        prepare_schedule_fixture(&directory, &store, &session_store, revoked_id);
        store
            .update(revoked_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed owned schedule before revocation");
        store
            .poll_worker_owned(revoked_id, &owner, true)
            .expect("schedule worker performs its pre-publication poll");
        store
            .update(revoked_id, |task| {
                task.record.status = "reconciling".to_string();
                task.record.worker_pid = None;
                task.worker_lease = None;
                Ok(())
            })
            .expect("revoke schedule after prior poll");
        let error = publish_schedule_wake_owned(
            &store,
            &owner,
            revoked_id,
            &session_store,
            &audit,
            &job,
            1,
        )
        .expect_err("revoked schedule cannot publish wake effects");
        assert!(error.contains("worker lease lost"), "{error}");
        let session = session_store.load("origin").expect("origin session");
        assert!(!session.events.iter().any(|event| {
            event.details.get("timer_id").and_then(Value::as_str) == Some(revoked_id)
        }));

        let cancelled_id = "owned-schedule-cancel";
        prepare_schedule_fixture(&directory, &store, &session_store, cancelled_id);
        store
            .update(cancelled_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.record.cancel_requested = true;
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed cancelled schedule");
        assert_eq!(
            publish_schedule_wake_owned(
                &store,
                &owner,
                cancelled_id,
                &session_store,
                &audit,
                &job,
                1,
            )
            .expect("owned cancellation publication"),
            ScheduleWakePublication::Finished
        );
        assert_eq!(
            store.get(cancelled_id).unwrap().unwrap().status,
            "cancelled"
        );

        let failed_id = "owned-schedule-failure";
        prepare_schedule_fixture(&directory, &store, &session_store, failed_id);
        store
            .update(failed_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed failed schedule");
        let failed = publish_schedule_failure_owned(
            &store,
            &owner,
            failed_id,
            &session_store,
            &audit,
            ScheduleFailure {
                session_id: "origin",
                occurrence: 1,
                repeat_count: 1,
                error: "agent failed".to_string(),
            },
        )
        .expect("owned failure publication");
        assert_eq!(failed.status, "failed");
        let session = session_store.load("origin").expect("origin session");
        for id in [task_id, cancelled_id, failed_id] {
            assert!(session.events.iter().any(|event| {
                event.details.get("timer_id").and_then(Value::as_str) == Some(id)
            }));
        }
    }

    #[test]
    fn task_lock_keeps_a_stable_inode_and_never_steals_live_ownership() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("task.lock");
        let anchor_path = directory.path().join(".task.lock.anchor");
        let first = TaskLock::acquire(path.clone(), anchor_path.clone()).expect("first owner");
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open stable lock inode");

        assert!(matches!(
            contender
                .try_lock()
                .expect_err("live owner cannot be displaced"),
            std::fs::TryLockError::WouldBlock
        ));
        assert!(path.exists(), "lock inode remains present while owned");

        drop(first);
        contender.try_lock().expect("dead owner releases OS lock");
        assert!(
            path.exists(),
            "stable lock inode is not unlinked on release"
        );
        assert!(
            anchor_path.exists(),
            "persistent lock anchor remains reachable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn task_lock_rejects_regular_file_replacement_during_open() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("task.lock");
        let anchor_path = directory.path().join(".task.lock.anchor");
        let displaced = directory.path().join("displaced.lock");
        let error = TaskLock::acquire_with_hook(path.clone(), anchor_path, || {
            std::fs::rename(&path, &displaced)
                .map_err(|error| format!("failed to displace lock: {error}"))?;
            std::fs::write(&path, b"replacement")
                .map_err(|error| format!("failed to replace lock: {error}"))
        })
        .expect_err("replacement lock inode must be rejected");
        assert!(error.contains("identity changed"), "{error}");
    }

    #[test]
    fn task_lock_replacement_child_process() {
        let Some(daemon_dir) = std::env::var_os(TASK_LOCK_CHILD_DAEMON_DIR) else {
            return;
        };
        let kind = std::env::var(TASK_LOCK_CHILD_KIND).expect("child lock kind");
        let id = std::env::var(TASK_LOCK_CHILD_ID).expect("child task ID");
        let expectation =
            std::env::var(TASK_LOCK_CHILD_EXPECTATION).expect("child lock expectation");
        let daemon_dir = PathBuf::from(daemon_dir);
        let tasks_dir = daemon_dir.join("tasks");
        let (path, anchor_path) = match kind.as_str() {
            "task" => {
                let stripe = task_lock_stripe(&id);
                (
                    tasks_dir.join(format!(".task-stripe-{stripe:02}.lock")),
                    daemon_dir.join(format!(".task-stripe-{stripe:02}.lock.anchor")),
                )
            }
            "admission" => (
                tasks_dir.join(".admission.lock"),
                daemon_dir.join(".admission.task.lock.anchor"),
            ),
            value => panic!("unsupported child lock kind: {value}"),
        };
        let error = TaskLock::acquire_with_retries(path, anchor_path, 10)
            .expect_err("replacement must not create a second task lock domain");
        match expectation.as_str() {
            "timeout" => assert!(error.contains("timed out acquiring task lock"), "{error}"),
            "identity" => assert!(
                error.contains("different identities") || error.contains("identity changed"),
                "{error}"
            ),
            value => panic!("unsupported child expectation: {value}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn persistent_anchors_protect_task_and_admission_locks_across_path_replacement() {
        let (_directory, store, _session_store) = fixture();
        let task_id = "anchored-owner";

        for kind in ["task", "admission"] {
            let (path, anchor_path) = match kind {
                "task" => (store.lock_path(task_id), store.lock_anchor_path(task_id)),
                "admission" => (
                    store.admission_lock_path(),
                    store.admission_lock_anchor_path(),
                ),
                _ => unreachable!(),
            };
            let held = TaskLock::acquire(path.clone(), anchor_path.clone())
                .expect("held persistent task lock");
            let run_child = |expectation: &str| {
                let output = Command::new(std::env::current_exe().expect("test binary"))
                    .args([
                        "--exact",
                        "daemons::workload::tests::task_lock_replacement_child_process",
                        "--nocapture",
                    ])
                    .env(TASK_LOCK_CHILD_DAEMON_DIR, store.daemon_dir())
                    .env(TASK_LOCK_CHILD_KIND, kind)
                    .env(TASK_LOCK_CHILD_ID, task_id)
                    .env(TASK_LOCK_CHILD_EXPECTATION, expectation)
                    .output()
                    .expect("run task lock child process");
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            };

            run_child("timeout");

            let displaced_path = path.with_extension("lock.displaced");
            std::fs::rename(&path, &displaced_path).expect("displace visible task lock");
            std::fs::write(&path, b"replacement").expect("replace visible task lock");
            run_child("identity");
            std::fs::remove_file(&path).expect("remove replacement task lock");
            std::fs::rename(&displaced_path, &path).expect("restore anchored task lock");

            let displaced_tasks = store.daemon_dir().join("tasks.displaced");
            std::fs::rename(&store.tasks_dir, &displaced_tasks)
                .expect("displace durable tasks directory");
            std::fs::create_dir(&store.tasks_dir).expect("replace durable tasks directory");
            run_child("timeout");
            std::fs::remove_dir_all(&store.tasks_dir)
                .expect("remove replacement durable tasks directory");
            std::fs::rename(&displaced_tasks, &store.tasks_dir)
                .expect("restore durable tasks directory");

            drop(held);
            TaskLock::acquire(path, anchor_path)
                .expect("restored anchored task lock remains usable");
        }
    }

    #[cfg(windows)]
    #[test]
    fn open_task_directory_capability_blocks_lock_parent_replacement() {
        let (_directory, store, _session_store) = fixture();
        let task_id = "anchored-windows-owner";
        for kind in ["task", "admission"] {
            let (path, anchor_path) = match kind {
                "task" => (store.lock_path(task_id), store.lock_anchor_path(task_id)),
                "admission" => (
                    store.admission_lock_path(),
                    store.admission_lock_anchor_path(),
                ),
                _ => unreachable!(),
            };
            let held = TaskLock::acquire(path.clone(), anchor_path.clone())
                .expect("held persistent task lock");
            assert!(
                std::fs::rename(&store.tasks_dir, store.daemon_dir().join("tasks.displaced"))
                    .is_err(),
                "Windows must deny replacement while the task directory capability is open"
            );
            let output = Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "daemons::workload::tests::task_lock_replacement_child_process",
                    "--nocapture",
                ])
                .env(TASK_LOCK_CHILD_DAEMON_DIR, store.daemon_dir())
                .env(TASK_LOCK_CHILD_KIND, kind)
                .env(TASK_LOCK_CHILD_ID, task_id)
                .env(TASK_LOCK_CHILD_EXPECTATION, "timeout")
                .output()
                .expect("run task lock child process");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            drop(held);
            TaskLock::acquire(path, anchor_path).expect("released lock remains usable");
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn fixed_stripes_bound_failed_id_artifacts_and_migrate_legacy_locks() {
        let (_directory, store, _session_store) = fixture();
        for index in 0..512 {
            let id = format!("missing-{index}");
            let error = store.cancel(&id).expect_err("missing task stays missing");
            assert!(error.contains("background task not found"), "{error}");
        }

        for index in 0..80 {
            let id = format!("legacy-{index}");
            let visible = store.tasks_dir.join(format!("{id}.lock"));
            let anchor = store.daemon_dir.join(format!(".{id}.task.lock.anchor"));
            std::fs::write(&visible, b"legacy").expect("legacy visible lock");
            std::fs::hard_link(&visible, &anchor).expect("legacy persistent anchor");
            let error = store
                .cancel(&id)
                .expect_err("legacy missing task stays missing");
            assert!(error.contains("background task not found"), "{error}");
            assert!(!visible.exists(), "legacy visible lock was removed");
            assert!(!anchor.exists(), "legacy persistent anchor was removed");
        }

        let live_id = "legacy-live-owner";
        let live_visible = store.tasks_dir.join(format!("{live_id}.lock"));
        let live_anchor = store
            .daemon_dir
            .join(format!(".{live_id}.task.lock.anchor"));
        std::fs::write(&live_visible, b"legacy live").expect("live legacy lock");
        std::fs::hard_link(&live_visible, &live_anchor).expect("live legacy anchor");
        let live_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&live_anchor)
            .expect("open live legacy anchor");
        live_file.try_lock().expect("hold legacy lock");
        let error = store
            .cancel(live_id)
            .expect_err("live legacy owner must fail migration closed");
        assert!(error.contains("still owned"), "{error}");
        assert!(live_visible.exists());
        assert!(live_anchor.exists());
        drop(live_file);
        store
            .cancel(live_id)
            .expect_err("released legacy lock migrates before missing result");
        assert!(!live_visible.exists());
        assert!(!live_anchor.exists());

        let visible_locks = std::fs::read_dir(&store.tasks_dir)
            .expect("task directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".task-stripe-")
            })
            .count();
        let persistent_anchors = std::fs::read_dir(&store.daemon_dir)
            .expect("daemon directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with(".task-stripe-") && name.ends_with(".lock.anchor")
            })
            .count();
        assert_eq!(visible_locks, TASK_LOCK_STRIPES);
        assert_eq!(persistent_anchors, TASK_LOCK_STRIPES);
        let admission = store.admission_lock_path();
        let admission_anchor = store.admission_lock_anchor_path();
        assert!(admission.exists());
        assert!(admission_anchor.exists());
        assert_eq!(
            task_file_identity(
                &OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&admission)
                    .expect("admission lock"),
                &admission,
            )
            .expect("admission identity"),
            task_file_identity(
                &OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&admission_anchor)
                    .expect("admission anchor"),
                &admission_anchor,
            )
            .expect("admission anchor identity")
        );
        assert_eq!(
            std::fs::read_dir(&store.tasks_dir)
                .expect("bounded task directory")
                .count(),
            TASK_LOCK_STRIPES + 1,
            "failed IDs must not grow task-directory inode use beyond the fixed controls"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn constructor_globally_migrates_untouched_legacy_locks_and_preserves_controls() {
        let root = tempdir().expect("tempdir");
        let daemon_dir = root.path().join("state/daemons");
        let tasks_dir = daemon_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).expect("task directory");

        let preserved_visible = tasks_dir.join(".task-stripe-00.lock");
        let preserved_anchor = daemon_dir.join(".task-stripe-00.lock.anchor");
        std::fs::write(&preserved_visible, b"current stripe").expect("current stripe");
        std::fs::hard_link(&preserved_visible, &preserved_anchor).expect("current stripe anchor");
        let preserved_identity = task_file_identity(
            &OpenOptions::new()
                .read(true)
                .write(true)
                .open(&preserved_anchor)
                .expect("current stripe anchor"),
            &preserved_anchor,
        )
        .expect("current stripe identity");

        let admission = tasks_dir.join(".admission.lock");
        let admission_anchor = daemon_dir.join(".admission.task.lock.anchor");
        std::fs::write(&admission, b"current admission").expect("current admission");
        std::fs::hard_link(&admission, &admission_anchor).expect("current admission anchor");
        let admission_identity = task_file_identity(
            &OpenOptions::new()
                .read(true)
                .write(true)
                .open(&admission_anchor)
                .expect("current admission anchor"),
            &admission_anchor,
        )
        .expect("current admission identity");

        for index in 0..128 {
            let id = format!("untouched-{index}");
            let visible = tasks_dir.join(format!("{id}.lock"));
            let anchor = daemon_dir.join(format!(".{id}.task.lock.anchor"));
            std::fs::write(&visible, b"legacy").expect("legacy lock");
            std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
        }

        let store = DurableTaskStore::at_daemon_dir(&daemon_dir)
            .expect("constructor performs global migration");
        for index in 0..128 {
            let id = format!("untouched-{index}");
            assert!(!tasks_dir.join(format!("{id}.lock")).exists());
            assert!(!daemon_dir.join(format!(".{id}.task.lock.anchor")).exists());
        }
        assert_eq!(
            task_file_identity(
                &OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&preserved_anchor)
                    .expect("preserved stripe anchor"),
                &preserved_anchor,
            )
            .expect("preserved stripe identity"),
            preserved_identity
        );
        assert_eq!(
            task_file_identity(
                &OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&admission_anchor)
                    .expect("preserved admission anchor"),
                &admission_anchor,
            )
            .expect("preserved admission identity"),
            admission_identity
        );
        assert_eq!(
            std::fs::read_dir(&store.tasks_dir)
                .expect("fixed task controls")
                .count(),
            TASK_LOCK_STRIPES + 1
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn global_legacy_lock_migration_fails_closed_for_a_live_owner() {
        let root = tempdir().expect("tempdir");
        let daemon_dir = root.path().join("state/daemons");
        let tasks_dir = daemon_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).expect("task directory");
        let visible = tasks_dir.join("untouched-live.lock");
        let anchor = daemon_dir.join(".untouched-live.task.lock.anchor");
        std::fs::write(&visible, b"live legacy").expect("legacy lock");
        std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&anchor)
            .expect("legacy owner");
        owner.try_lock().expect("hold legacy lock");

        let error = DurableTaskStore::at_daemon_dir(&daemon_dir)
            .expect_err("global migration must reject a live legacy owner");
        assert!(error.contains("still owned"), "{error}");
        assert!(visible.exists());
        assert!(anchor.exists());

        drop(owner);
        DurableTaskStore::at_daemon_dir(&daemon_dir)
            .expect("released legacy owner can be globally migrated");
        assert!(!visible.exists());
        assert!(!anchor.exists());
    }

    #[cfg(unix)]
    #[test]
    fn task_record_rejects_regular_file_replacement_during_open() {
        let (directory, store, session_store) = fixture();
        store
            .prepare_terminal(terminal_request(
                &directory,
                &session_store,
                "replace-record",
            ))
            .expect("prepare record");
        let path = store.task_path("replace-record");
        let displaced = store.tasks_dir.join("displaced-record");
        let replacement = std::fs::read(&path).expect("replacement contents");
        let error = store
            .read_path_bounded_with_hook(&path, MAX_TASK_RECORD_BYTES, || {
                std::fs::rename(&path, &displaced)
                    .map_err(|error| format!("failed to displace record: {error}"))?;
                std::fs::write(&path, replacement)
                    .map_err(|error| format!("failed to replace record: {error}"))
            })
            .expect_err("replacement record inode must be rejected");
        assert!(
            error.contains("identity changed while it was in use"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn effectful_worker_commit_targets_original_directory_after_replacement() {
        let (directory, store, session_store) = fixture();
        let task_id = "paired-directory-replacement";
        let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
        let task_path = store.task_path(task_id);
        let before = std::fs::read(&task_path).expect("record before replacement");
        let displaced_tasks = store.daemon_dir().join("tasks.displaced-after-effect");
        let replacement_task = store.tasks_dir.join(format!("{task_id}.json"));
        let effect_marker = directory.path().join("external-effect");

        let error = store
            .update_owned_with_hook(
                task_id,
                &owner,
                |task| {
                    std::fs::write(&effect_marker, b"published")
                        .map_err(|error| format!("failed to publish test effect: {error}"))?;
                    finish_task_file(task, "completed", Some(json!({"exit_code": 0})), None);
                    Ok(())
                },
                || {
                    std::fs::rename(&store.tasks_dir, &displaced_tasks)
                        .map_err(|error| format!("failed to detach task directory: {error}"))?;
                    std::fs::create_dir(&store.tasks_dir)
                        .map_err(|error| format!("failed to replace task directory: {error}"))?;
                    std::fs::write(&replacement_task, &before)
                        .map_err(|error| format!("failed to seed replacement task: {error}"))
                },
            )
            .expect_err("paired commit must report the detached task directory");
        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(
            std::fs::read(&effect_marker).expect("external effect marker"),
            b"published"
        );
        let committed: DurableTaskFile = serde_json::from_slice(
            &std::fs::read(displaced_tasks.join(format!("{task_id}.json")))
                .expect("original capability record"),
        )
        .expect("decode committed original record");
        assert_eq!(committed.record.status, "completed");
        let replacement: DurableTaskFile = serde_json::from_slice(
            &std::fs::read(&replacement_task).expect("replacement sentinel record"),
        )
        .expect("decode replacement record");
        assert_eq!(replacement.record.status, "running");
    }

    #[test]
    fn effectful_worker_commit_does_not_overwrite_replaced_task_record() {
        let (directory, store, session_store) = fixture();
        let task_id = "paired-file-replacement";
        let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
        let task_path = store.task_path(task_id);
        let displaced = store.tasks_dir.join("displaced-paired-task.json");
        let mut replacement: DurableTaskFile =
            serde_json::from_slice(&std::fs::read(&task_path).expect("task before replacement"))
                .expect("decode replacement base");
        replacement.record.status = "failed".to_string();
        replacement.record.error = Some("newer reconciler decision".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement task");
        let effect_marker = directory.path().join("paired-file-effect");

        let error = store
            .update_owned_with_hook(
                task_id,
                &owner,
                |task| {
                    std::fs::write(&effect_marker, b"published")
                        .map_err(|error| error.to_string())?;
                    finish_task_file(task, "completed", Some(json!({"exit_code": 0})), None);
                    Ok(())
                },
                || {
                    std::fs::rename(&task_path, &displaced).map_err(|error| error.to_string())?;
                    std::fs::write(&task_path, &replacement_bytes)
                        .map_err(|error| error.to_string())
                },
            )
            .expect_err("replaced task record must reject post-effect commit");

        assert!(error.contains("identity changed"), "{error}");
        assert_eq!(
            std::fs::read(&effect_marker).expect("external effect"),
            b"published"
        );
        let visible: DurableTaskFile =
            serde_json::from_slice(&std::fs::read(&task_path).expect("newer visible record"))
                .expect("decode newer record");
        assert_eq!(visible.record.status, "failed");
        assert_eq!(
            visible.record.error.as_deref(),
            Some("newer reconciler decision")
        );
        let displaced_record: DurableTaskFile =
            serde_json::from_slice(&std::fs::read(displaced).expect("displaced prior record"))
                .expect("decode displaced record");
        assert_eq!(displaced_record.record.status, "running");
    }

    #[test]
    fn worker_launch_compensation_failure_is_returned_and_audited() {
        let (directory, store, session_store) = fixture();
        let id = "worker-compensation";
        store
            .prepare_terminal(terminal_request(&directory, &session_store, id))
            .expect("prepare worker task");
        store
            .update(id, |task| {
                task.record.status = "starting".to_string();
                task.worker_lease = Some(WorkerLease {
                    token: "lease".to_string(),
                });
                Ok(())
            })
            .expect("seed worker lease");
        let path = store.task_path(id);
        std::fs::remove_file(&path).expect("remove worker record");
        std::fs::create_dir(&path).expect("inject terminalization failure");

        let error = store.finish_worker_launch_failure(
            id,
            "lease",
            "injected worker launch failure".to_string(),
        );
        assert!(error.contains("durable compensation failed"), "{error}");
        assert!(error.contains("daemon audit"), "{error}");
        let records = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("compensation audit");
        assert!(records.iter().any(|record| {
            record.action == "worker_launch"
                && record.target.as_deref() == Some(id)
                && record.outcome == "compensation_failed"
        }));
    }

    #[cfg(unix)]
    #[test]
    fn task_record_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let (directory, store, _) = fixture();
        let outside = directory.path().join("outside.json");
        std::fs::write(&outside, "{}").unwrap();
        symlink(&outside, store.task_path("linked")).unwrap();
        assert!(store.get("linked").is_err());
    }

    #[test]
    fn oversized_sparse_task_record_is_rejected_before_reading() {
        let (_directory, store, _) = fixture();
        let path = store.task_path("oversized");
        File::create(&path)
            .and_then(|file| file.set_len(MAX_TASK_RECORD_BYTES + 1))
            .expect("oversized sparse task record");

        let error = store
            .get("oversized")
            .expect_err("oversized task record must fail closed");

        assert!(error.contains("maximum is"));
    }

    #[cfg(unix)]
    #[test]
    fn task_store_rechecks_directory_after_constructor() {
        use std::os::unix::fs::symlink;

        let (directory, store, session_store) = fixture();
        let outside = directory.path().join("outside");
        std::fs::create_dir(&outside).expect("outside");
        let displaced = directory.path().join("tasks.displaced");
        std::fs::rename(&store.tasks_dir, &displaced).expect("displace task directory");
        symlink(&outside, &store.tasks_dir).expect("swap task directory");

        let error = store
            .prepare_terminal(DurableTerminalRequest {
                id: "blocked".to_string(),
                command: "printf blocked".to_string(),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                execution: ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
            .expect_err("swapped task directory must fail closed");

        assert!(error.contains("unsafe") || error.contains("symlink"));
        assert!(!outside.join("blocked.json").exists());
        assert!(!outside.join("blocked.lock").exists());
        std::fs::remove_file(&store.tasks_dir).expect("remove replacement symlink");
        std::fs::rename(displaced, &store.tasks_dir).expect("restore task directory");
    }

    #[test]
    fn direct_durable_requests_enforce_public_tool_bounds() {
        let (directory, store, session_store) = fixture();
        let terminal_error = store
            .prepare_terminal(DurableTerminalRequest {
                id: "oversized-command".to_string(),
                command: "x".repeat(MAX_TERMINAL_COMMAND_BYTES + 1),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                execution: ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
            .expect_err("oversized direct command must be rejected");
        assert!(terminal_error.contains("command exceeds"));

        let schedule_error = store
            .prepare_schedule(DurableScheduleRequest {
                id: "oversized-prompt".to_string(),
                prompt: "x".repeat(MAX_SCHEDULE_PROMPT_BYTES + 1),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                initial_delay: Duration::from_secs(1),
                interval: Duration::from_secs(1),
                repeat_count: 1,
            })
            .expect_err("oversized direct prompt must be rejected");
        assert!(schedule_error.contains("prompt exceeds"));
    }

    #[cfg(unix)]
    fn run_task_commit_child(daemon_dir: &Path) {
        let id =
            std::env::var(TASK_COMMIT_CHILD_ID).expect("task commit child id must be configured");
        let mode = std::env::var(TASK_COMMIT_CHILD_MODE)
            .expect("task commit child mode must be configured");
        let ready = PathBuf::from(
            std::env::var_os(TASK_COMMIT_CHILD_READY)
                .expect("task commit child ready path must be configured"),
        );
        let release = std::env::var_os(TASK_COMMIT_CHILD_RELEASE).map(PathBuf::from);
        let store = DurableTaskStore::at_daemon_dir(daemon_dir).expect("task commit child store");
        let _lock = store
            .acquire_task_lock(&id)
            .expect("task commit child lock");
        let path = store.task_path(&id);
        let mut opened = store
            .read_path_opened(&path)
            .expect("task commit child record");
        opened.task.record.error = Some("child must not publish".to_string());
        opened.task.record.updated_at = Utc::now();
        let result = store.write_path_after_effects_expected_with_commit_check(
            &path,
            &opened.task,
            crate::daemons::state::FileExpectation::Present(&opened.file),
            || {
                fs::write(&ready, b"ready").map_err(|error| error.to_string())?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    if release.as_ref().is_some_and(|path| path.exists()) {
                        return Ok(());
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err("task commit child timed out".to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            },
        );

        match mode.as_str() {
            "replace" => {
                let error = result.expect_err("commit-barrier replacement must fail closed");
                assert!(error.contains("identity changed"), "{error}");
            }
            "kill" => panic!("task crash child unexpectedly left its commit barrier"),
            value => panic!("unsupported task commit child mode: {value}"),
        }
    }

    #[cfg(unix)]
    fn spawn_task_commit_child(
        daemon_dir: &Path,
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
            std::env::current_exe().expect("current workload test binary"),
        );
        command
            .args([
                "--exact",
                "daemons::workload::tests::real_child_task_commit_barrier_and_fsync_crash_recovery",
                "--nocapture",
            ])
            .env(TASK_COMMIT_CHILD_DAEMON_DIR, daemon_dir)
            .env(TASK_COMMIT_CHILD_ID, id)
            .env(TASK_COMMIT_CHILD_MODE, mode)
            .env(TASK_COMMIT_CHILD_READY, ready);
        if let Some(release) = release {
            command.env(TASK_COMMIT_CHILD_RELEASE, release);
        }
        command.spawn().expect("spawn task commit child")
    }

    #[cfg(unix)]
    fn wait_for_task_commit_child(child: &mut std::process::Child, ready: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect task commit child") {
                panic!("task commit child exited before readiness: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "task commit child did not become ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn task_temporary_paths(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("list task directory")
            .map(|entry| entry.expect("task directory entry").path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".task-") && name.ends_with(".tmp")
                })
            })
            .collect()
    }
}
