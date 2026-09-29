//! T043 split.

use super::*;

pub(crate) const TASK_FILE_VERSION: u32 = 1;
pub(crate) const LOCK_RETRIES: usize = 500;
pub(crate) const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
pub(crate) const TASK_LOCK_STRIPES: usize = 64;
pub(crate) const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(crate) const STALE_WORKER_AFTER_SECONDS: i64 = 15;
pub(crate) const MAX_TASK_RECORD_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_DURABLE_TASK_RECORDS: usize = 10_000;
pub(crate) const MAX_DURABLE_TASK_ENUMERATION_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_TASK_DIRECTORY_EXTRA_ENTRIES: usize = TASK_LOCK_STRIPES + 1_024;
pub(crate) const MAX_TASK_DIRECTORY_NAME_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_LEGACY_LOCK_MIGRATION_ENTRIES: usize = 100_000;
pub(crate) const MAX_RECONCILIATION_REPORT_TASKS: usize = 128;
pub(crate) const MAX_RECONCILIATION_ERROR_CHARS: usize = 4_096;
pub(crate) const MAX_TERMINAL_COMMAND_BYTES: usize = 65_536;
pub(crate) const MAX_TERMINAL_TIMEOUT_SECONDS: u64 = 3_600;
pub(crate) const MAX_TERMINAL_OUTPUT_BYTES: usize = 1_048_576;
pub(crate) const MAX_SCHEDULE_PROMPT_BYTES: usize = 20_000;
pub(crate) const MAX_SCHEDULE_DELAY_SECONDS: u64 = 31_536_000;
pub(crate) const MAX_COMPENSATION_AUDIT_DETAIL_CHARS: usize = 16_384;
pub(crate) const SESSION_SCOPED_TASK_UNAVAILABLE: &str =
    "background task is not available to the active session";

pub(crate) enum SessionCancellationFailure {
    Unavailable,
    NotRunning(String),
    Store(String),
}

impl SessionCancellationFailure {
    pub(crate) fn into_public_message(self) -> String {
        match self {
            Self::Unavailable => SESSION_SCOPED_TASK_UNAVAILABLE.to_string(),
            Self::NotRunning(status) => {
                format!("background task is not running (status: {status})")
            }
            Self::Store(error) => error,
        }
    }
}

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

/// Bounded, presentation-safe view of durable work owned by one session.
///
/// Command text, prompts, results, errors, worker identities, and cancellation
/// internals intentionally remain private to the authoritative workload store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionOwnedDurableTask {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<&DurableTaskRecord> for SessionOwnedDurableTask {
    fn from(record: &DurableTaskRecord) -> Self {
        Self {
            id: record.id.clone(),
            kind: record.kind.clone(),
            status: record.status.clone(),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

impl DurableTaskRecord {
    pub(crate) fn is_terminal(&self) -> bool {
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
pub(crate) enum DurableJob {
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

impl DurableJob {
    pub(crate) fn session_id(&self) -> &str {
        match self {
            Self::Terminal { session_id, .. } | Self::Schedule { session_id, .. } => session_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurableTaskFile {
    pub(crate) version: u32,
    pub(crate) record: DurableTaskRecord,
    pub(crate) job: DurableJob,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) worker_lease: Option<WorkerLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active_occurrence: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct OpenedDurableTaskFile {
    pub(crate) task: DurableTaskFile,
    pub(crate) file: File,
    pub(crate) bytes_read: u64,
    pub(crate) needs_execution_id_migration: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WorkerLease {
    pub(crate) token: String,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerOwner {
    pub(crate) token: String,
    pub(crate) pid: u32,
}

pub(crate) enum MonitoredRun<T> {
    Completed(T),
    Cancelled,
    LeaseLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReconcileHookPoint {
    BeforeDelivery,
    AfterDelivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerPublicationHookPoint {
    BeforeEffects,
    AfterEffects,
}

#[cfg(test)]
pub(crate) fn pause_worker_publication(
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
pub(crate) fn pause_worker_publication(
    _point: WorkerPublicationHookPoint,
    _task_id: &str,
) -> Result<(), String> {
    Ok(())
}

#[derive(Debug, Clone)]
pub struct DurableTaskStore {
    pub(crate) daemon_dir: PathBuf,
    pub(crate) tasks_dir: PathBuf,
    pub(crate) daemon_directory: Arc<crate::daemons::state::StableDirectory>,
    pub(crate) tasks_directory: Arc<crate::daemons::state::StableDirectory>,
    pub(crate) max_records: usize,
    pub(crate) max_enumeration_bytes: u64,
    pub(crate) max_reconciliation_report_tasks: usize,
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
    pub(crate) fn with_record_limit(mut self, max_records: usize) -> Self {
        self.max_records = max_records;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_enumeration_byte_limit(mut self, max_enumeration_bytes: u64) -> Self {
        self.max_enumeration_bytes = max_enumeration_bytes;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_reconciliation_report_limit(mut self, max_tasks: usize) -> Self {
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

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
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

    pub(crate) fn finish_worker_launch_failure(
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

    pub fn list_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionOwnedDurableTask>, String> {
        crate::session::validate_session_id(session_id).map_err(|error| error.to_string())?;
        let mut records = Vec::new();
        let mut remaining_bytes = self.max_enumeration_bytes;
        for path in self.record_paths()? {
            let (_task_id, _lock) = self.acquire_record_path_lock(&path)?;
            let (task, bytes_read, _) = self.read_path_bounded(&path, remaining_bytes)?;
            remaining_bytes = remaining_bytes
                .checked_sub(bytes_read)
                .ok_or_else(|| "durable task enumeration byte count underflowed".to_string())?;
            if task.job.session_id() == session_id {
                records.push(SessionOwnedDurableTask::from(&task.record));
            }
        }
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }

    pub fn cancel(&self, id: &str) -> Result<DurableTaskRecord, String> {
        self.cancel_scoped(id, None)
    }

    pub fn cancel_for_session(
        &self,
        id: &str,
        session_id: &str,
    ) -> Result<SessionOwnedDurableTask, String> {
        crate::session::validate_session_id(session_id).map_err(|error| error.to_string())?;
        self.cancel_session_scoped(id, session_id)
            .map(|record| SessionOwnedDurableTask::from(&record))
            .map_err(SessionCancellationFailure::into_public_message)
    }

    pub(crate) fn cancel_session_scoped(
        &self,
        id: &str,
        session_id: &str,
    ) -> Result<DurableTaskRecord, SessionCancellationFailure> {
        validate_task_id(id).map_err(SessionCancellationFailure::Store)?;
        let _lock = self
            .acquire_task_lock(id)
            .map_err(SessionCancellationFailure::Store)?;
        let path = self.task_path(id);
        if !self
            .tasks_directory
            .path_exists(&path)
            .map_err(SessionCancellationFailure::Store)?
        {
            return Err(SessionCancellationFailure::Unavailable);
        }
        // Until a complete record proves ownership, malformed or substituted state
        // is indistinguishable from a missing/foreign task to the session caller.
        let mut opened = self
            .read_path_opened(&path)
            .map_err(|_| SessionCancellationFailure::Unavailable)?;
        if opened.task.job.session_id() != session_id {
            return Err(SessionCancellationFailure::Unavailable);
        }
        if opened.task.record.is_terminal() {
            return Err(SessionCancellationFailure::NotRunning(
                opened.task.record.status.clone(),
            ));
        }
        opened.task.record.cancel_requested = true;
        opened.task.record.updated_at = Utc::now();
        if matches!(opened.task.record.status.as_str(), "prepared" | "starting")
            && opened.task.record.worker_pid.is_none()
        {
            opened.task.record.status = "cancelled".to_string();
            opened.task.record.error = Some("cancelled by user before start".to_string());
            opened.task.worker_lease = None;
            scrub_completed_job(&mut opened.task.job);
        } else {
            opened.task.record.status = "cancelling".to_string();
        }
        self.write_path_expected(
            &path,
            &opened.task,
            crate::daemons::state::FileExpectation::Present(&opened.file),
        )
        .map_err(SessionCancellationFailure::Store)?;
        let updated = opened.task.record;
        if updated.is_terminal() {
            return Ok(updated);
        }
        drop(_lock);
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(20));
            let Some(record) = self.get(id).map_err(SessionCancellationFailure::Store)? else {
                return Err(SessionCancellationFailure::Unavailable);
            };
            if record.is_terminal() {
                return Ok(record);
            }
        }
        self.get(id)
            .map_err(SessionCancellationFailure::Store)?
            .ok_or(SessionCancellationFailure::Unavailable)
    }

    pub(crate) fn cancel_scoped(
        &self,
        id: &str,
        required_session_id: Option<&str>,
    ) -> Result<DurableTaskRecord, String> {
        let updated = self.update(id, |task| {
            if required_session_id.is_some_and(|session_id| task.job.session_id() != session_id) {
                return Err(format!("{SESSION_SCOPED_TASK_UNAVAILABLE}: {id}"));
            }
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

    pub(crate) fn reconcile_with_hook(
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

    pub(crate) fn reconcile_record(
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

    pub(crate) fn create(&self, task: &DurableTaskFile) -> Result<(), String> {
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

    pub(crate) fn ensure_admission_capacity(&self) -> Result<(), String> {
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

    pub(crate) fn evict_terminal_record(&self, id: &str) -> Result<(), String> {
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

    pub(crate) fn get_file(&self, id: &str) -> Result<DurableTaskFile, String> {
        validate_task_id(id)?;
        let _lock = self.acquire_task_lock(id)?;
        let path = self.task_path(id);
        if !self.tasks_directory.path_exists(&path)? {
            return Err(format!("background task not found: {id}"));
        }
        self.read_path(&path)
    }

    pub(crate) fn record_paths(&self) -> Result<Vec<PathBuf>, String> {
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

    pub(crate) fn update<F>(&self, id: &str, mutate: F) -> Result<DurableTaskRecord, String>
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

    pub(crate) fn bind_worker(
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

    pub(crate) fn claim_worker(
        &self,
        id: &str,
        owner: &WorkerOwner,
    ) -> Result<DurableTaskFile, String> {
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

    pub(crate) fn update_owned<F>(
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

    pub(crate) fn update_owned_with<F, T>(
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

    pub(crate) fn update_owned_with_hook<F, T, H>(
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

    pub(crate) fn poll_worker_owned(
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

    pub(crate) fn finish_owned(
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

    pub(crate) fn finish_starting(
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
    pub(crate) fn update_schedule_progress_owned(
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

    pub(crate) fn task_path(&self, id: &str) -> PathBuf {
        self.tasks_dir.join(format!("{id}.json"))
    }

    pub(crate) fn lock_path(&self, id: &str) -> PathBuf {
        self.lock_path_for_stripe(task_lock_stripe(id))
    }

    pub(crate) fn lock_anchor_path(&self, id: &str) -> PathBuf {
        self.lock_anchor_path_for_stripe(task_lock_stripe(id))
    }

    pub(crate) fn acquire_record_path_lock(
        &self,
        path: &Path,
    ) -> Result<(String, TaskLock), String> {
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

    pub(crate) fn lock_path_for_stripe(&self, stripe: usize) -> PathBuf {
        self.tasks_dir
            .join(format!(".task-stripe-{stripe:02}.lock"))
    }

    pub(crate) fn lock_anchor_path_for_stripe(&self, stripe: usize) -> PathBuf {
        self.daemon_dir
            .join(format!(".task-stripe-{stripe:02}.lock.anchor"))
    }

    pub(crate) fn admission_lock_path(&self) -> PathBuf {
        self.tasks_dir.join(".admission.lock")
    }

    pub(crate) fn admission_lock_anchor_path(&self) -> PathBuf {
        self.daemon_dir.join(".admission.task.lock.anchor")
    }

    pub(crate) fn acquire_task_lock(&self, id: &str) -> Result<TaskLock, String> {
        let lock = TaskLock::acquire(self.lock_path(id), self.lock_anchor_path(id))?;
        self.tasks_directory.verify_visible()?;
        self.daemon_directory.verify_visible()?;
        self.cleanup_legacy_task_lock(id)?;
        Ok(lock)
    }

    pub(crate) fn acquire_admission_lock(&self) -> Result<TaskLock, String> {
        let lock = TaskLock::acquire(
            self.admission_lock_path(),
            self.admission_lock_anchor_path(),
        )?;
        self.tasks_directory.verify_visible()?;
        self.daemon_directory.verify_visible()?;
        Ok(lock)
    }

    pub(crate) fn cleanup_legacy_task_lock(&self, id: &str) -> Result<(), String> {
        let path = self.tasks_dir.join(format!("{id}.lock"));
        let anchor_path = self.daemon_dir.join(format!(".{id}.task.lock.anchor"));
        cleanup_existing_task_lock_artifacts(
            &path,
            &anchor_path,
            &self.tasks_directory,
            &self.daemon_directory,
        )
    }

    pub(crate) fn initialize_lock_namespace(&self) -> Result<(), String> {
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

    pub(crate) fn migrate_legacy_task_locks(&self) -> Result<(), String> {
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

    pub(crate) fn migrate_legacy_execution_ids(&self) -> Result<(), String> {
        self.migrate_legacy_execution_ids_with_hook(|| Ok(()))
    }

    pub(crate) fn migrate_legacy_execution_ids_with_hook(
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

    pub(crate) fn read_path(&self, path: &Path) -> Result<DurableTaskFile, String> {
        self.read_path_bounded(path, MAX_TASK_RECORD_BYTES)
            .map(|(task, _, _)| task)
    }

    pub(crate) fn read_path_opened(&self, path: &Path) -> Result<OpenedDurableTaskFile, String> {
        self.read_path_bounded_opened(path, MAX_TASK_RECORD_BYTES)
    }

    pub(crate) fn read_path_bounded(
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

    pub(crate) fn read_path_bounded_opened(
        &self,
        path: &Path,
        allocation_limit: u64,
    ) -> Result<OpenedDurableTaskFile, String> {
        self.read_path_bounded_with_hook(path, allocation_limit, || Ok(()))
    }

    pub(crate) fn read_path_bounded_with_hook(
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

    pub(crate) fn write_path_expected(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        expected: crate::daemons::state::FileExpectation<'_>,
    ) -> Result<(), String> {
        self.tasks_directory.verify_visible()?;
        self.write_path_with_mode(path, task, false, expected)
    }

    pub(crate) fn write_path_after_effects_expected(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        expected: crate::daemons::state::FileExpectation<'_>,
    ) -> Result<(), String> {
        self.write_path_with_mode(path, task, true, expected)
    }

    pub(crate) fn write_path_with_mode(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        after_effects: bool,
        expected: crate::daemons::state::FileExpectation<'_>,
    ) -> Result<(), String> {
        self.write_path_with_mode_and_commit_check(path, task, after_effects, expected, || Ok(()))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn write_path_after_effects_expected_with_commit_check(
        &self,
        path: &Path,
        task: &DurableTaskFile,
        expected: crate::daemons::state::FileExpectation<'_>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        self.write_path_with_mode_and_commit_check(path, task, true, expected, before_commit)
    }

    pub(crate) fn write_path_with_mode_and_commit_check(
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

pub(crate) fn publish_claimed_cancellation(
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
