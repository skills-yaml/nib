//! Split for T043 C02.

use super::*;

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const INTERRUPTED_ERROR: &str =
    "subagent runtime ended before a durable terminal result was recorded";

pub(crate) const MAX_SUBAGENT_RECORDS: usize = 10_000;

pub(crate) const MAX_SUBAGENT_RECORD_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) const SUBAGENT_RECORD_LOCK_STRIPES: usize = 64;

pub(crate) const MAX_LEGACY_RECORD_LOCK_ENTRIES: usize = MAX_SUBAGENT_RECORDS + 1_024;

pub(crate) const MAX_LEGACY_RECORD_LOCK_NAME_BYTES: usize = 4 * 1024 * 1024;

pub(crate) const MAX_LEGACY_RECORD_LOCK_MIGRATION_PASSES: usize = 8;

pub(crate) const LEGACY_RECORD_LOCK_MIGRATION_RECEIPT: &str = ".legacy-lock-migration-v1.json";

pub(crate) const NATIVE_RECORDS_STAGING_DIRECTORY: &str = ".subagents-native-origin-v1.staging";

pub(crate) const LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION: u32 = 1;

pub(crate) const MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) const MERGE_PENDING_STATUS: &str = "merge_pending";

pub(crate) const MERGE_FAILED_STATUS: &str = "merge_failed";

pub(crate) const NIB_EXCLUDE_PATHSPEC: &str = ":(exclude).nib";

pub(crate) const NIB_DESCENDANTS_EXCLUDE_PATHSPEC: &str = ":(exclude).nib/**";

pub(crate) const REPOSITORY_MERGE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) const SUBAGENT_RECORD_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) const SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
    pub(crate) static TEST_SPAWN_RECONCILIATION_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, windows))]
pub(crate) struct SpawnPositiveProgressTimeoutGuard {
    pub(crate) previous_preparation: Option<Duration>,
    pub(crate) previous_reconciliation: Option<Duration>,
    pub(crate) _not_send_or_sync: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(all(test, windows))]
impl SpawnPositiveProgressTimeoutGuard {
    pub(crate) fn set(timeout: Duration) -> Self {
        let previous_preparation = TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT.with(|slot| {
            let previous = slot.get();
            slot.set(Some(timeout));
            previous
        });
        let previous_reconciliation = TEST_SPAWN_RECONCILIATION_TIMEOUT.with(|slot| {
            let previous = slot.get();
            slot.set(Some(timeout));
            previous
        });
        Self {
            previous_preparation,
            previous_reconciliation,
            _not_send_or_sync: std::marker::PhantomData,
        }
    }
}

#[cfg(all(test, windows))]
impl Drop for SpawnPositiveProgressTimeoutGuard {
    fn drop(&mut self) {
        TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT.with(|slot| slot.set(self.previous_preparation));
        TEST_SPAWN_RECONCILIATION_TIMEOUT.with(|slot| slot.set(self.previous_reconciliation));
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct SubagentCancellationTimeoutGuard(pub(crate) Option<Duration>);

#[cfg(test)]
impl SubagentCancellationTimeoutGuard {
    pub(crate) fn set(timeout: Duration) -> Self {
        let previous = TEST_SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT.with(|slot| {
            let previous = slot.get();
            slot.set(Some(timeout));
            previous
        });
        Self(previous)
    }
}

#[cfg(test)]
impl Drop for SubagentCancellationTimeoutGuard {
    fn drop(&mut self) {
        TEST_SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT.with(|slot| slot.set(self.0));
    }
}

#[cfg(test)]
pub(crate) type SpawnPhaseHook = Option<Box<dyn FnMut(&'static str)>>;

#[cfg(test)]
thread_local! {
    pub(crate) static SPAWN_HANDOFF_PHASE_HOOK: std::cell::RefCell<SpawnPhaseHook> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn run_spawn_handoff_phase_hook(phase: &'static str) {
    SPAWN_HANDOFF_PHASE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(phase);
        }
    });
}

#[cfg(not(test))]
pub(crate) fn run_spawn_handoff_phase_hook(_phase: &'static str) {}

pub(crate) fn spawn_preparation_operation_timeout() -> Duration {
    #[cfg(test)]
    {
        TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT
            .with(|timeout| timeout.get())
            .unwrap_or(SUBAGENT_RECORD_LOCK_TIMEOUT)
    }
    #[cfg(not(test))]
    {
        SUBAGENT_RECORD_LOCK_TIMEOUT
    }
}

pub(crate) fn spawn_reconciliation_deadline_timeout() -> Duration {
    #[cfg(test)]
    {
        TEST_SPAWN_RECONCILIATION_TIMEOUT
            .with(|timeout| timeout.get())
            .unwrap_or(SUBAGENT_RECORD_LOCK_TIMEOUT)
    }
    #[cfg(not(test))]
    {
        SUBAGENT_RECORD_LOCK_TIMEOUT
    }
}

pub(crate) fn spawn_reconciliation_worktree_timeout() -> Duration {
    #[cfg(test)]
    {
        TEST_SPAWN_RECONCILIATION_TIMEOUT
            .with(|timeout| timeout.get())
            .unwrap_or(SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT)
    }
    #[cfg(not(test))]
    {
        SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT
    }
}

pub(crate) fn subagent_cancellation_reconciliation_timeout() -> Duration {
    #[cfg(test)]
    {
        TEST_SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT
            .with(|timeout| timeout.get())
            .unwrap_or(SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT)
    }
    #[cfg(not(test))]
    {
        SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT
    }
}

pub(crate) const SUBAGENT_CANCELLATION_RECONCILIATION_ATTEMPTS: usize = 500;

#[cfg(not(test))]
pub(crate) const SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT: Duration = Duration::from_secs(4);

#[cfg(test)]
pub(crate) const SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT: Duration =
    Duration::from_millis(250);

pub(crate) const MAX_SUBAGENT_DIRECTORY_ENTRIES: usize = MAX_SUBAGENT_RECORDS + 1_024;

pub(crate) const MAX_SUBAGENT_DIRECTORY_NAME_BYTES: usize = 4 * 1024 * 1024;

pub(crate) const OWNER_LEASE_DIRECTORY: &str = "subagent-owner-leases";

pub(crate) const OWNER_LEASE_SUFFIX: &str = ".lease";

pub(crate) const OWNER_LEASE_ANCHOR_PREFIX: &str = ".subagent-owner-";

pub(crate) const OWNER_LEASE_ANCHOR_SUFFIX: &str = ".anchor";

pub(crate) const OWNER_LOST_ERROR: &str =
    "subagent execution was interrupted because its owner process is no longer live";

pub(crate) const SUBAGENT_AUDIT_DESTINATION_ENCODING_ERROR: &str =
    "subagent audit destination cannot be represented without changing its filesystem identity";

pub(crate) const MAX_SUBAGENT_WORKER_REQUEST_BYTES: usize = 16 * 1024 * 1024;

pub(crate) const SUBAGENT_SUPERVISOR_READY_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(all(debug_assertions, not(test)))]
pub(crate) const SUBAGENT_LAUNCH_FAILPOINT_ENV: &str = "NIB_TEST_SUBAGENT_LAUNCH_FAILPOINT";

#[cfg(all(debug_assertions, not(test)))]
pub(crate) const SUBAGENT_READY_BEFORE_COMMIT_PATH_ENV: &str =
    "NIB_TEST_SUBAGENT_READY_BEFORE_COMMIT_PATH";

#[cfg(all(debug_assertions, not(test)))]
pub(crate) const SUBAGENT_WORKER_STARTED_PATH_ENV: &str = "NIB_TEST_SUBAGENT_WORKER_STARTED_PATH";

#[cfg(all(debug_assertions, not(test)))]
pub(crate) const SUBAGENT_WORKER_DELAY_MS_ENV: &str = "NIB_TEST_SUBAGENT_WORKER_DELAY_MS";

#[cfg(all(debug_assertions, not(test)))]
pub(crate) const SUBAGENT_SCOPE_PREPARED_PATH_ENV: &str = "NIB_TEST_SUBAGENT_SCOPE_PREPARED_PATH";

#[cfg(all(debug_assertions, not(test)))]
pub(crate) const SUBAGENT_SUPERVISOR_PRE_REGISTER_PATH_ENV: &str =
    "NIB_TEST_SUBAGENT_SUPERVISOR_PRE_REGISTER_PATH";

#[cfg(all(debug_assertions, not(test)))]
pub(crate) const SUBAGENT_SUPERVISOR_REGISTER_RELEASE_ENV: &str =
    "NIB_TEST_SUBAGENT_SUPERVISOR_REGISTER_RELEASE";

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SubagentWorkerRequest {
    pub(crate) prompt: String,
    pub(crate) max_steps: u32,
}

pub(crate) const SUBAGENT_SUPERVISOR_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubagentSupervisorRequest {
    pub(crate) version: u32,
    pub(crate) handoff_nonce: String,
    pub(crate) subagent_id: String,
    pub(crate) execution_generation: u64,
    pub(crate) owner_lease: String,
    pub(crate) cleanup_lease_id: String,
    pub(crate) worker: SubagentWorkerRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubagentSupervisorFrame {
    pub(crate) version: u32,
    pub(crate) phase: String,
    pub(crate) handoff_nonce: String,
    pub(crate) subagent_id: String,
    pub(crate) execution_generation: u64,
    pub(crate) owner_lease: String,
    pub(crate) process_scope: crate::sandbox::process::ProcessScopeRecord,
}

#[cfg(not(test))]
pub(crate) struct SubagentSupervisorHandoff {
    pub(crate) control: Arc<Mutex<Option<std::process::ChildStdin>>>,
    pub(crate) responses: std::sync::mpsc::Receiver<Result<SubagentSupervisorFrame, String>>,
    pub(crate) ready: SubagentSupervisorFrame,
}

pub(crate) struct LaunchedSubagentTask {
    pub(crate) response: Value,
    #[cfg(test)]
    pub(crate) precommit_process_scope: Option<crate::sandbox::process::ProcessScopeRecord>,
    #[cfg(not(test))]
    pub(crate) supervisor_handoff: Option<SubagentSupervisorHandoff>,
}

impl LaunchedSubagentTask {
    #[cfg(test)]
    pub(crate) fn precommit_process_scope(
        &self,
    ) -> Option<crate::sandbox::process::ProcessScopeRecord> {
        self.precommit_process_scope.clone()
    }

    #[cfg(not(test))]
    pub(crate) fn precommit_process_scope(
        &self,
    ) -> Option<crate::sandbox::process::ProcessScopeRecord> {
        self.supervisor_handoff
            .as_ref()
            .map(|handoff| handoff.ready.process_scope.clone())
    }

    #[cfg(test)]
    pub(crate) fn commit_supervisor_handoff(
        &mut self,
        authority: &SpawnPreparationAuthority,
    ) -> Result<(), String> {
        authority.verify_until(authority.operation_deadline())
    }

    #[cfg(not(test))]
    pub(crate) fn commit_supervisor_handoff(
        &mut self,
        authority: &SpawnPreparationAuthority,
    ) -> Result<(), String> {
        let deadline = authority.operation_deadline();
        authority.verify_until(deadline)?;
        let handoff = self
            .supervisor_handoff
            .take()
            .ok_or("subagent supervisor handoff authority was already consumed")?;
        let commit = SubagentSupervisorFrame {
            phase: "commit".to_string(),
            ..handoff.ready.clone()
        };
        let transmitted_commit = {
            #[cfg(debug_assertions)]
            {
                let mut transmitted_commit = commit;
                if subagent_launch_failpoint("commit-identity-mismatch") {
                    transmitted_commit.handoff_nonce = uuid::Uuid::new_v4().to_string();
                }
                transmitted_commit
            }
            #[cfg(not(debug_assertions))]
            {
                commit
            }
        };
        let encoded = serde_json::to_vec(&transmitted_commit)
            .map_err(|error| format!("failed to encode supervisor COMMIT frame: {error}"))?;
        if encoded.len() > MAX_SUBAGENT_WORKER_REQUEST_BYTES {
            return Err("subagent supervisor COMMIT frame exceeds its bound".to_string());
        }
        {
            let mut control_slot = handoff
                .control
                .lock()
                .map_err(|_| "subagent supervisor control lock is poisoned".to_string())?;
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("commit-eof") {
                control_slot.take();
                return Err("injected supervisor COMMIT EOF".to_string());
            }
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("commit-timeout") {
                drop(control_slot);
                let remaining = deadline.saturating_duration_since(Instant::now());
                if !remaining.is_zero() {
                    std::thread::sleep(remaining);
                }
                return Err("injected supervisor COMMIT timeout".to_string());
            }
            let control = control_slot
                .as_mut()
                .ok_or("subagent supervisor control pipe is unavailable")?;
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("commit-partial") {
                let split = encoded.len().max(2) / 2;
                control
                    .write_all(&encoded[..split])
                    .and_then(|()| control.flush())
                    .map_err(|error| {
                        format!("failed to send partial supervisor COMMIT frame: {error}")
                    })?;
                return Err("injected partial supervisor COMMIT frame before newline".to_string());
            }
            control
                .write_all(&encoded)
                .and_then(|()| control.write_all(b"\n"))
                .and_then(|()| control.flush())
                .map_err(|error| format!("failed to send supervisor COMMIT frame: {error}"))?;
        }
        authority.verify_until(deadline)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or("subagent supervisor STARTED acknowledgement exceeded its deadline")?;
        let started = handoff
            .responses
            .recv_timeout(remaining.min(SUBAGENT_SUPERVISOR_READY_TIMEOUT))
            .map_err(|error| {
                format!("subagent supervisor STARTED acknowledgement failed: {error}")
            })??;
        #[cfg(debug_assertions)]
        if subagent_launch_failpoint("commit-identity-mismatch") {
            return Err(
                "injected supervisor COMMIT identity mismatch was not rejected".to_string(),
            );
        }
        validate_subagent_supervisor_frame(&handoff.ready, &started, "started")?;
        if started.process_scope.launch_committed != Some(true) {
            return Err("subagent supervisor STARTED frame is not launch-committed".to_string());
        }
        authority.verify_until(deadline)?;
        Ok(())
    }
}

pub(crate) fn validate_subagent_supervisor_frame(
    expected: &SubagentSupervisorFrame,
    observed: &SubagentSupervisorFrame,
    phase: &str,
) -> Result<(), String> {
    let process_scope_matches = match phase {
        "commit" => observed.process_scope == expected.process_scope,
        "started" => {
            let mut normalized = observed.process_scope.clone();
            let updated_at_is_monotonic =
                normalized.updated_at >= expected.process_scope.updated_at;
            normalized.launch_committed = expected.process_scope.launch_committed;
            normalized.updated_at = expected.process_scope.updated_at;
            updated_at_is_monotonic && normalized == expected.process_scope
        }
        _ => true,
    };
    if observed.version != SUBAGENT_SUPERVISOR_PROTOCOL_VERSION
        || observed.phase != phase
        || observed.handoff_nonce != expected.handoff_nonce
        || observed.subagent_id != expected.subagent_id
        || observed.execution_generation != expected.execution_generation
        || observed.owner_lease != expected.owner_lease
        || observed.process_scope.scope_id != expected.process_scope.scope_id
        || observed.process_scope.execution_generation
            != expected.process_scope.execution_generation
        || observed.process_scope.cleanup_lease_id != expected.process_scope.cleanup_lease_id
        || observed.process_scope.supervisor != expected.process_scope.supervisor
        || observed.process_scope.direct_child != expected.process_scope.direct_child
        || !process_scope_matches
    {
        return Err(format!(
            "subagent supervisor {phase} frame does not match its exact execution authority"
        ));
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SubagentWorkerResponse {
    pub(crate) outcome: Result<crate::agent::AgentRunSummary, String>,
}

#[cfg(test)]
pub(crate) static CANCELLED_RECORD_WRITE_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) static RECOVERABLE_REVISION_PUBLICATION_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) static SPAWN_OWNER_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SPAWN_RECORD_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SPAWN_WORKTREE_CLEANUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SPAWN_OWNER_CLEANUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SPAWN_AUDIT_CLEANUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SPAWN_SESSION_CLEANUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SPAWN_SESSION_PUBLICATION_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SPAWN_POST_AUDIT_CANCELLATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn consume_spawn_failure(counter: &std::sync::atomic::AtomicUsize) -> bool {
    counter
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |remaining| remaining.checked_sub(1),
        )
        .is_ok()
}

#[derive(Debug)]
pub(crate) struct SpawnOwnerCreationError {
    pub(crate) message: String,
    pub(crate) mutation_indeterminate: bool,
}

pub(crate) fn create_spawn_owner_lease(
    project_root: &Path,
    plan: &SubagentOwnerPlan,
    authority: &SpawnPreparationAuthority,
) -> Result<SubagentOwnerLease, SpawnOwnerCreationError> {
    #[cfg(test)]
    if consume_spawn_failure(&SPAWN_OWNER_FAILURES) {
        return Err(SpawnOwnerCreationError {
            message: "injected subagent owner creation failure".to_string(),
            mutation_indeterminate: false,
        });
    }
    let deadline = authority.operation_deadline();
    authority
        .verify_until(deadline)
        .map_err(|message| SpawnOwnerCreationError {
            message,
            mutation_indeterminate: false,
        })?;
    let owner = SubagentOwnerLease::create_until_with_guard(project_root, deadline, plan, || {
        authority.verify_until(deadline)
    })
    .map_err(|message| SpawnOwnerCreationError {
        message,
        // The owner publisher can fail after either visible half is durable.
        // Only restart reconciliation can classify that exact pair safely.
        mutation_indeterminate: true,
    })?;
    authority
        .verify_until(deadline)
        .map_err(|message| SpawnOwnerCreationError {
            message,
            mutation_indeterminate: true,
        })?;
    Ok(owner)
}

pub(crate) fn write_spawn_subagent_record_locked(
    project_root: &Path,
    record: &SubagentRecord,
    authority: &SpawnPreparationAuthority,
) -> Result<InitialSubagentRecordPublication, InitialSubagentRecordPublicationError> {
    #[cfg(test)]
    if consume_spawn_failure(&SPAWN_RECORD_FAILURES) {
        return Err(InitialSubagentRecordPublicationError {
            message: "injected initial subagent record publication failure".to_string(),
            receipt: None,
            publication_attempted: false,
        });
    }
    let deadline = authority.operation_deadline();
    let path = record_path(project_root, &record.id).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
            publication_attempted: false,
        }
    })?;
    authority
        .verify_until(deadline)
        .map_err(|message| InitialSubagentRecordPublicationError {
            message,
            receipt: None,
            publication_attempted: false,
        })?;
    let mut before_namespace_step = || authority.verify_until(deadline);
    let receipt = write_subagent_record_unlocked_with_receipt_and_guard(
        project_root,
        &authority.records,
        &path,
        record,
        crate::daemons::state::FileExpectation::Missing,
        Some(deadline),
        &mut before_namespace_step,
    )
    .map_err(|error| InitialSubagentRecordPublicationError {
        message: error.message,
        receipt: error.receipt,
        publication_attempted: true,
    })?;
    if !receipt.exact_identity {
        return Err(InitialSubagentRecordPublicationError {
            message: format!(
                "initial subagent record {} was published without exact identity",
                record.id
            ),
            receipt: Some(receipt),
            publication_attempted: true,
        });
    }
    let clone_receipt =
        || {
            receipt.file.try_clone().ok().map(|file| {
                crate::daemons::state::FilePublicationReceipt {
                    file,
                    exact_identity: receipt.exact_identity,
                }
            })
        };
    authority
        .verify_until(deadline)
        .map_err(|message| InitialSubagentRecordPublicationError {
            message,
            receipt: clone_receipt(),
            publication_attempted: true,
        })?;
    let opened = read_opened_subagent_record_in(&authority.records, &path).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: clone_receipt(),
            publication_attempted: true,
        }
    })?;
    validate_reopened_subagent_record(record, &opened.record).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: clone_receipt(),
            publication_attempted: true,
        }
    })?;
    if !crate::daemons::state::same_open_file_identity(&receipt.file, &opened.file).map_err(
        |message| InitialSubagentRecordPublicationError {
            message,
            receipt: clone_receipt(),
            publication_attempted: true,
        },
    )? {
        return Err(InitialSubagentRecordPublicationError {
            message: "initial subagent record changed before locked readback".to_string(),
            receipt: Some(receipt),
            publication_attempted: true,
        });
    }
    authority
        .verify_until(deadline)
        .map_err(|message| InitialSubagentRecordPublicationError {
            message,
            receipt: clone_receipt(),
            publication_attempted: true,
        })?;
    Ok(InitialSubagentRecordPublication { receipt })
}

pub(crate) fn commit_spawn_handoff_record_locked(
    project_root: &Path,
    record: &mut SubagentRecord,
    publication: &mut InitialSubagentRecordPublication,
    intent: &SpawnPreparationIntent,
) -> Result<(), String> {
    let deadline = intent.authority.operation_deadline();
    intent.authority.verify_until(deadline)?;
    let result = record
        .result
        .as_mut()
        .and_then(Value::as_object_mut)
        .ok_or("pending subagent record lacks internal handoff authority")?;
    result.insert(
        SPAWN_HANDOFF_KEY.to_string(),
        spawn_handoff_evidence(&intent.data, "committed"),
    );
    let path = record_path(project_root, &record.id)?;
    let mut before_namespace_step = || intent.authority.verify_until(deadline);
    let receipt = match write_subagent_record_unlocked_with_receipt_and_guard(
        project_root,
        &intent.authority.records,
        &path,
        record,
        crate::daemons::state::FileExpectation::Present(&publication.receipt.file),
        Some(deadline),
        &mut before_namespace_step,
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            let opened = read_opened_subagent_record_in(&intent.authority.records, &path).map_err(
                |readback| format!("{}; handoff readback failed: {readback}", error.message),
            )?;
            validate_spawn_intent_record_identity(&intent.data, &opened.record)?;
            if opened.record.status == "running" {
                return Err(error.message);
            }
            // A gated worker can terminalize between durable handoff proof and
            // this CAS on production runtimes.  Its exact terminal record is
            // stronger authority than the pending running revision.
            *record = opened.record;
            publication.receipt = crate::daemons::state::FilePublicationReceipt {
                file: opened.file,
                exact_identity: true,
            };
            intent.authority.verify_until(deadline)?;
            return Ok(());
        }
    };
    if !receipt.exact_identity {
        return Err("committed subagent handoff lacks exact record identity".to_string());
    }
    intent.authority.verify_until(deadline)?;
    publication.receipt = receipt;
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn run_after_subagent_audit_preflight_hook() {
    AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
pub(crate) fn run_after_subagent_audit_preflight_hook() {}

#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_PREPARATION_INTENT_OPEN_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn run_after_preparation_intent_open_hook() {
    AFTER_PREPARATION_INTENT_OPEN_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
pub(crate) fn run_after_preparation_intent_open_hook() {}

#[cfg(test)]
thread_local! {
    pub(crate) static SPAWN_FORWARD_MUTATION_HOOK: std::cell::RefCell<SpawnPhaseHook> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn run_spawn_forward_mutation_hook(boundary: &'static str) {
    SPAWN_FORWARD_MUTATION_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(boundary);
        }
    });
}

#[cfg(not(test))]
pub(crate) fn run_spawn_forward_mutation_hook(_boundary: &'static str) {}

#[cfg(test)]
pub(crate) type SpawnAuthorityVerifyHook =
    std::sync::Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

#[cfg(test)]
pub(crate) static SPAWN_AUTHORITY_VERIFY_HOOK: std::sync::LazyLock<
    std::sync::Mutex<Option<SpawnAuthorityVerifyHook>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
pub(crate) fn run_spawn_authority_verify_hook(records_path: &Path) -> Result<(), String> {
    let hook = SPAWN_AUTHORITY_VERIFY_HOOK
        .lock()
        .map_err(|_| "spawn authority verify hook lock poisoned".to_string())?
        .clone();
    match hook {
        Some(hook) => hook(records_path),
        None => Ok(()),
    }
}

#[cfg(not(test))]
pub(crate) fn run_spawn_authority_verify_hook(_records_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(debug_assertions)]
pub(crate) fn pause_after_subagent_audit_preparation(subagent_id: &str) -> Result<(), String> {
    let Some(ready) = std::env::var_os("NIB_TEST_SUBAGENT_AUDIT_PREPARED_READY") else {
        return Ok(());
    };
    let ready = PathBuf::from(ready);
    std::fs::write(&ready, subagent_id.as_bytes())
        .map_err(|error| format!("failed to publish audit preparation readiness: {error}"))?;
    let resume = std::env::var_os("NIB_TEST_SUBAGENT_AUDIT_PREPARED_RESUME")
        .map(PathBuf::from)
        .ok_or_else(|| "missing audit preparation resume path".to_string())?;
    let started = Instant::now();
    while !resume.exists() {
        if started.elapsed() >= Duration::from_secs(30) {
            return Err("timed out waiting after audit preparation".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
pub(crate) fn pause_after_subagent_audit_preparation(_subagent_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(debug_assertions)]
pub(crate) fn pause_after_spawn_preparation_intent(subagent_id: &str) -> Result<(), String> {
    let Some(ready) = std::env::var_os("NIB_TEST_SUBAGENT_INTENT_PLANNED_READY") else {
        return Ok(());
    };
    let ready = PathBuf::from(ready);
    let staged_ready = ready.with_extension("pending");
    std::fs::write(&staged_ready, subagent_id.as_bytes())
        .map_err(|error| format!("failed to publish planned intent readiness: {error}"))?;
    std::fs::rename(&staged_ready, &ready)
        .map_err(|error| format!("failed to commit planned intent readiness: {error}"))?;
    let resume = std::env::var_os("NIB_TEST_SUBAGENT_INTENT_PLANNED_RESUME")
        .map(PathBuf::from)
        .ok_or_else(|| "missing planned intent resume path".to_string())?;
    let started = Instant::now();
    while !resume.exists() {
        if started.elapsed() >= Duration::from_secs(30) {
            return Err("timed out waiting after planned spawn intent".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(debug_assertions))]
pub(crate) fn pause_after_spawn_preparation_intent(_subagent_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(all(debug_assertions, not(test)))]
pub(crate) fn pause_after_supervisor_ready_before_commit(subagent_id: &str) -> Result<(), String> {
    let Some(path) = std::env::var_os(SUBAGENT_READY_BEFORE_COMMIT_PATH_ENV) else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    std::fs::write(&path, subagent_id.as_bytes())
        .map_err(|error| format!("failed to publish supervisor READY barrier: {error}"))?;
    let started = Instant::now();
    loop {
        if started.elapsed() >= Duration::from_secs(30) {
            return Err("timed out at the supervisor READY-before-COMMIT barrier".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(not(debug_assertions), test))]
pub(crate) fn pause_after_supervisor_ready_before_commit(_subagent_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(all(debug_assertions, not(test)))]
pub(crate) fn pause_after_scope_prepared_before_supervisor_spawn(
    subagent_id: &str,
) -> Result<(), String> {
    let Some(path) = std::env::var_os(SUBAGENT_SCOPE_PREPARED_PATH_ENV) else {
        return Ok(());
    };
    std::fs::write(PathBuf::from(path), subagent_id.as_bytes())
        .map_err(|error| format!("failed to publish prepared scope barrier: {error}"))?;
    loop {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(not(debug_assertions), not(test)))]
pub(crate) fn pause_after_scope_prepared_before_supervisor_spawn(
    _subagent_id: &str,
) -> Result<(), String> {
    Ok(())
}

#[cfg(all(debug_assertions, not(test)))]
pub(crate) fn pause_before_supervisor_self_registration(
    _subagent_id: &str,
    identity: &crate::sandbox::process::ProcessIdentity,
) -> Result<(), String> {
    let Some(path) = std::env::var_os(SUBAGENT_SUPERVISOR_PRE_REGISTER_PATH_ENV) else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let encoded = serde_json::to_vec(identity)
        .map_err(|error| format!("failed to encode supervisor registration barrier: {error}"))?;
    std::fs::write(&path, encoded)
        .map_err(|error| format!("failed to publish supervisor registration barrier: {error}"))?;
    let release = std::env::var_os(SUBAGENT_SUPERVISOR_REGISTER_RELEASE_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| "missing supervisor registration release path".to_string())?;
    let started = Instant::now();
    while !release.exists() {
        if started.elapsed() >= Duration::from_secs(30) {
            return Err("timed out before supervisor self-registration".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(any(not(debug_assertions), test))]
pub(crate) fn pause_before_supervisor_self_registration(
    _subagent_id: &str,
    _identity: &crate::sandbox::process::ProcessIdentity,
) -> Result<(), String> {
    Ok(())
}

#[cfg(debug_assertions)]
pub(crate) type MergeInterruptionBarrierMap =
    std::collections::HashMap<(PathBuf, String), tokio::sync::oneshot::Sender<Result<(), String>>>;

#[cfg(debug_assertions)]
pub(crate) static MERGE_INTERRUPTION_TEST_BARRIERS: std::sync::LazyLock<
    std::sync::Mutex<MergeInterruptionBarrierMap>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(debug_assertions)]
#[doc(hidden)]
pub struct MergeInterruptionTestBarrier {
    pub(crate) reached: tokio::sync::oneshot::Receiver<Result<(), String>>,
}

#[cfg(debug_assertions)]
impl MergeInterruptionTestBarrier {
    #[doc(hidden)]
    pub async fn wait_until_interrupted(self) -> Result<(), String> {
        self.reached
            .await
            .map_err(|_| "merge interruption test barrier was dropped before use".to_string())?
    }
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn install_merge_interruption_test_barrier(
    project_root: &Path,
    subagent_id: &str,
) -> Result<MergeInterruptionTestBarrier, String> {
    if !is_valid_subagent_id(subagent_id) {
        return Err("invalid subagent id".to_string());
    }
    let key = (
        canonical_project_root(project_root)?,
        subagent_id.to_string(),
    );
    let (reached, receiver) = tokio::sync::oneshot::channel();
    let mut barriers = MERGE_INTERRUPTION_TEST_BARRIERS
        .lock()
        .map_err(|_| "merge interruption test barrier registry is poisoned".to_string())?;
    match barriers.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(reached);
        }
        std::collections::hash_map::Entry::Occupied(_) => {
            return Err("merge interruption test barrier already exists".to_string());
        }
    }
    Ok(MergeInterruptionTestBarrier { reached: receiver })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerificationEvidence {
    pub tool_name: String,
    pub command: String,
    pub worktree_path: PathBuf,
    pub success: bool,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub approval_granted: bool,
    pub approval_source: Option<String>,
    pub duration_seconds: f64,
    pub configured_provider: String,
    pub sandbox_profile: String,
    pub boundaries: BoundaryConfig,
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_commit: Option<String>,
    pub executed_at: DateTime<Utc>,
}

pub(crate) struct VerificationTarget {
    pub worktree_path: PathBuf,
    pub snapshot_commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRecord {
    pub id: String,
    pub parent_session_id: Option<String>,
    pub child_session_id: String,
    pub prompt: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_lease: Option<String>,
    pub worktree_path: PathBuf,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_oid: Option<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationEvidence>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SubagentAuditTarget {
    pub(crate) sessions_dir: PathBuf,
    pub(crate) directory_identity: crate::fs_security::FileIdentitySnapshot,
}

pub(crate) const OWNERSHIP_AUDIT_TARGET_KEY: &str = "_ownership_audit_target";

pub(crate) const WORKTREE_PREPARATION_RECEIPT_KEY: &str = "_worktree_ownership_receipt";

pub(crate) const SPAWN_HANDOFF_KEY: &str = "_spawn_handoff";

pub(crate) const SPAWN_HANDOFF_VERSION: u32 = 1;

pub(crate) const SPAWN_PREPARATION_DIRECTORY: &str = ".preparations";

pub(crate) const LEGACY_SPAWN_PREPARATION_VERSION: u32 = 2;

pub(crate) const HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION: u32 = 3;

pub(crate) const SPAWN_PREPARATION_VERSION: u32 = 4;

pub(crate) const MAX_SPAWN_PREPARATION_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubagentProcessScopePlan {
    pub(crate) cleanup_lease_id: String,
    pub(crate) supervisor_registration_nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnPreparationIntentData {
    pub(crate) version: u32,
    pub(crate) revision: u64,
    pub(crate) phase: SpawnPreparationPhase,
    pub(crate) subagent_id: String,
    pub(crate) owner: SubagentOwnerPlan,
    pub(crate) worktree: crate::sandbox::worktree::WorktreePreparationAuthority,
    pub(crate) audit_session_id: String,
    pub(crate) audit_sessions_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) audit_namespace_plan: Option<crate::session::SessionNamespacePreparationPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) audit_target: Option<SubagentAuditTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) audit_receipt: Option<crate::session::SessionPreparationReceipt>,
    /// Exact authority published before any process-scope or supervisor
    /// mutation. Version-four launches must consume these values unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) process_scope_plan: Option<SubagentProcessScopePlan>,
    /// Exact supervisor READY authority. New production handoffs persist this
    /// before COMMIT so restart never infers execution from a filename or a
    /// legacy launch flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) handoff_process_scope: Option<crate::sandbox::process::ProcessScopeRecord>,
    pub(crate) created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SpawnPreparationPhase {
    Planned,
    ResourcesPrepared,
    AuditPlanned,
    AuditPublished,
    RecordPublished,
    ManagerRegistered,
    HandoffProven,
}

pub(crate) struct SpawnPreparationIntent {
    pub(crate) data: SpawnPreparationIntentData,
    pub(crate) authority: std::sync::Arc<SpawnPreparationAuthority>,
    pub(crate) directory: crate::daemons::state::StableDirectory,
    pub(crate) path: PathBuf,
    pub(crate) file: File,
    pub(crate) created_directory_parent: Option<crate::daemons::state::StableDirectory>,
}

pub(crate) struct SpawnPreparationAuthority {
    pub(crate) records: crate::daemons::state::StableDirectory,
    pub(crate) migration_fence: crate::daemons::state::HeldFileLock,
    pub(crate) record_stripe: crate::daemons::state::HeldFileLock,
    pub(crate) operation_deadline: Instant,
}

impl SpawnPreparationAuthority {
    pub(crate) fn operation_deadline(&self) -> Instant {
        self.operation_deadline
    }

    #[cfg(test)]
    pub(crate) fn verify(&self) -> Result<(), String> {
        self.migration_fence.verify()?;
        self.record_stripe.verify()?;
        self.records.verify_visible()
    }

    pub(crate) fn verify_until(&self, deadline: Instant) -> Result<(), String> {
        ensure_subagent_reconciliation_deadline(Some(deadline))?;
        self.migration_fence.verify_until(deadline)?;
        self.record_stripe.verify_until(deadline)?;
        self.records.verify_visible()?;
        run_spawn_authority_verify_hook(self.records.path())?;
        ensure_subagent_reconciliation_deadline(Some(deadline))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyRecordLockMigrationReceipt {
    pub(crate) version: u32,
    pub(crate) epoch_id: String,
    pub(crate) records_identity: crate::fs_security::DirectoryIdentity,
    pub(crate) phase: LegacyRecordLockMigrationPhase,
    pub(crate) attested_at: DateTime<Utc>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) artifacts: Vec<LegacyRecordLockMigrationArtifact>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LegacyRecordLockMigrationPhase {
    Pending,
    Completed,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyRecordLockMigrationArtifact {
    pub(crate) path: PathBuf,
    pub(crate) quarantine_path: Option<PathBuf>,
    pub(crate) identity: crate::fs_security::FileIdentitySnapshot,
}

pub(crate) struct OpenedSubagentRecord {
    pub(crate) record: SubagentRecord,
    pub(crate) file: File,
}

pub(crate) struct InitialSubagentRecordPublication {
    pub(crate) receipt: crate::daemons::state::FilePublicationReceipt,
}

pub(crate) struct InitialSubagentRecordPublicationError {
    pub(crate) message: String,
    pub(crate) receipt: Option<crate::daemons::state::FilePublicationReceipt>,
    pub(crate) publication_attempted: bool,
}

impl std::fmt::Debug for InitialSubagentRecordPublicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InitialSubagentRecordPublicationError")
            .field("message", &self.message)
            .field("has_receipt", &self.receipt.is_some())
            .field("publication_attempted", &self.publication_attempted)
            .field(
                "exact_identity",
                &self.receipt.as_ref().map(|receipt| receipt.exact_identity),
            )
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct SubagentOwnerLease {
    pub(crate) visible_directory: Option<crate::daemons::state::StableDirectory>,
    pub(crate) anchor_directory: crate::daemons::state::StableDirectory,
    pub(crate) visible_path: PathBuf,
    pub(crate) anchor_path: PathBuf,
    pub(crate) file: Option<File>,
    pub(crate) execution_generation: u64,
    pub(crate) lease_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubagentOwnerPlan {
    pub(crate) execution_generation: u64,
    pub(crate) lease_id: String,
}

pub(crate) enum OwnerLeaseProbe {
    Live,
    Acquired(SubagentOwnerLease),
}

pub(crate) struct OwnershipReconciliationWork {
    pub(crate) record: SubagentRecord,
    pub(crate) evidence: Option<Value>,
    pub(crate) acquired_owner_lease: Option<SubagentOwnerLease>,
    pub(crate) retry_persisted_owner_cleanup: bool,
}

impl SubagentOwnerLease {
    pub(crate) fn plan() -> SubagentOwnerPlan {
        SubagentOwnerPlan {
            execution_generation: new_execution_generation(),
            lease_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn create(project_root: &Path) -> Result<Self, String> {
        Self::create_from_plan(project_root, &Self::plan())
    }

    #[cfg(test)]
    pub(crate) fn create_from_plan(
        project_root: &Path,
        plan: &SubagentOwnerPlan,
    ) -> Result<Self, String> {
        validate_execution_ownership(plan.execution_generation, &plan.lease_id)?;
        Self::create_with_timeout_and_guard(
            project_root,
            OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT,
            plan,
            || Ok(()),
        )
    }

    #[cfg(test)]
    pub(crate) fn create_with_timeout_and_guard(
        project_root: &Path,
        timeout: Duration,
        plan: &SubagentOwnerPlan,
        before_namespace_step: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let started = Instant::now();
        let deadline = started.checked_add(timeout).unwrap_or(started);
        Self::create_until_with_guard(project_root, deadline, plan, before_namespace_step)
    }

    pub(crate) fn create_until_with_guard(
        project_root: &Path,
        deadline: Instant,
        plan: &SubagentOwnerPlan,
        mut before_namespace_step: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        ensure_subagent_reconciliation_deadline(Some(deadline))?;
        let timeout = deadline.saturating_duration_since(Instant::now());
        let visible = owner_lease_directory(project_root);
        let namespace_lock = owner_lease_namespace_lock_path(project_root);
        let project_directory = crate::daemons::state::StableDirectory::open(project_root)?;
        let mut deadline_guard = || {
            before_namespace_step()?;
            ensure_delegation_lock_deadline(deadline, &namespace_lock, Some(timeout))
        };
        let visible_directory = project_directory.open_or_create_descendant_directory_with_guard(
            &visible,
            &mut deadline_guard,
            |_| Ok(()),
        )?;
        deadline_guard()?;
        drop(visible_directory);
        with_delegation_lock_in_deadline(
            &namespace_lock,
            &project_root.join(".nib"),
            deadline,
            Some(timeout),
            |_, deadline| {
                Self::create_locked(project_root, deadline, plan, &mut before_namespace_step)
            },
        )
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub(crate) fn create_locked(
        project_root: &Path,
        deadline: Instant,
        plan: &SubagentOwnerPlan,
        before_namespace_step: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let (anchor_directory, visible_directory) =
            open_owner_lease_directories(project_root, false)?;
        visible_directory.for_each_entry_bounded(
            MAX_SUBAGENT_DIRECTORY_ENTRIES,
            MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
            |_| Ok(()),
        )?;
        let execution_generation = plan.execution_generation;
        let lease_id = plan.lease_id.clone();
        let visible_path = owner_lease_path(project_root, &lease_id)?;
        let anchor_path = owner_lease_anchor_path(project_root, &lease_id)?;
        if visible_directory.path_exists(&visible_path)?
            || anchor_directory.path_exists(&anchor_path)?
        {
            return Err("new subagent owner lease identifier unexpectedly exists".to_string());
        }
        let mut namespace_guard = || {
            before_namespace_step()?;
            ensure_subagent_reconciliation_deadline(Some(deadline))
        };
        let visible_file = visible_directory
            .open_read_write_create_with_guard(&visible_path, &mut namespace_guard)?;
        if let Err(error) = visible_directory.hard_link_to_with_guard(
            &visible_path,
            &anchor_directory,
            &anchor_path,
            &mut namespace_guard,
        ) {
            let cleanup = visible_directory
                .remove_file_if_matches_with_guard(
                    &visible_path,
                    &visible_file,
                    ".nib-subagent-owner-create-visible-delete-",
                    &mut namespace_guard,
                )
                .err()
                .map(|cleanup| format!("; exact visible cleanup failed: {cleanup}"))
                .unwrap_or_default();
            return Err(format!(
                "failed to anchor subagent owner lease: {error}{cleanup}"
            ));
        }
        let file = match anchor_directory.open_read_write(&anchor_path) {
            Ok(file) => file,
            Err(error) => {
                let cleanup = visible_directory
                    .remove_file_if_matches_with_guard(
                        &visible_path,
                        &visible_file,
                        ".nib-subagent-owner-create-visible-delete-",
                        &mut namespace_guard,
                    )
                    .err()
                    .map(|cleanup| format!("; exact visible cleanup failed: {cleanup}"))
                    .unwrap_or_default();
                return Err(format!("{error}; ambiguous anchor was preserved{cleanup}"));
            }
        };
        if let Err(error) = verify_owner_lease_pair(
            &visible_directory,
            &visible_path,
            &visible_file,
            &anchor_directory,
            &anchor_path,
            &file,
        ) {
            return Err(format!(
                "{error}; mismatched owner lease artifacts were preserved"
            ));
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(
                    "new subagent owner lease is unexpectedly already owned; exact artifacts were preserved"
                        .to_string(),
                );
            }
            Err(std::fs::TryLockError::Error(error)) => {
                let cleanup = cleanup_created_owner_lease_pair(
                    &visible_directory,
                    &visible_path,
                    &visible_file,
                    &anchor_directory,
                    &anchor_path,
                    &file,
                    &mut namespace_guard,
                )
                .err()
                .map(|cleanup| format!("; exact owner lease cleanup failed: {cleanup}"))
                .unwrap_or_default();
                return Err(format!(
                    "failed to acquire subagent owner lease: {error}{cleanup}"
                ));
            }
        }
        if let Err(error) = verify_owner_lease_pair_from_anchor(
            &visible_directory,
            &visible_path,
            &anchor_directory,
            &anchor_path,
            &file,
        ) {
            return Err(format!(
                "{error}; changed owner lease artifacts were preserved"
            ));
        }
        drop(visible_file);
        Ok(Self {
            visible_directory: Some(visible_directory),
            anchor_directory,
            visible_path,
            anchor_path,
            file: Some(file),
            execution_generation,
            lease_id,
        })
    }

    pub(crate) fn probe(
        project_root: &Path,
        execution_generation: u64,
        lease_id: &str,
    ) -> Result<OwnerLeaseProbe, String> {
        validate_execution_ownership(execution_generation, lease_id)?;
        let nib = project_root.join(".nib");
        let visible_root = owner_lease_directory(project_root);
        let anchor_directory = crate::daemons::state::StableDirectory::open(&nib)?;
        let visible_directory = match anchor_directory.entry_kind(&visible_root)? {
            Some(crate::daemons::state::StableEntryKind::Directory) => {
                Some(anchor_directory.open_child(&visible_root)?)
            }
            Some(crate::daemons::state::StableEntryKind::File) => {
                return Err(format!(
                    "subagent owner lease directory is not a directory: {}",
                    visible_root.display()
                ));
            }
            None => None,
        };
        let visible_path = owner_lease_path(project_root, lease_id)?;
        let anchor_path = owner_lease_anchor_path(project_root, lease_id)?;
        let visible_directory = match visible_directory {
            Some(directory) => match directory.entry_kind(&visible_path)? {
                Some(crate::daemons::state::StableEntryKind::File) => Some(directory),
                Some(crate::daemons::state::StableEntryKind::Directory) => {
                    return Err(format!(
                        "subagent owner lease is not a regular file: {}",
                        visible_path.display()
                    ));
                }
                None => None,
            },
            None => None,
        };
        let file = anchor_directory
            .open_read_write(&anchor_path)
            .map_err(|error| format!("subagent owner lease anchor is unavailable: {error}"))?;
        if let Some(visible_directory) = &visible_directory {
            let visible_file = visible_directory
                .open_read_write(&visible_path)
                .map_err(|error| format!("subagent owner lease is unavailable: {error}"))?;
            verify_owner_lease_pair(
                visible_directory,
                &visible_path,
                &visible_file,
                &anchor_directory,
                &anchor_path,
                &file,
            )?;
        } else {
            verify_owner_lease_anchor(&anchor_directory, &anchor_path, &file)?;
        }
        let probe = match file.try_lock() {
            Ok(()) => OwnerLeaseProbe::Acquired(Self {
                visible_directory,
                anchor_directory,
                visible_path,
                anchor_path,
                file: Some(file),
                execution_generation,
                lease_id: lease_id.to_string(),
            }),
            Err(std::fs::TryLockError::WouldBlock) => {
                if let Some(visible_directory) = &visible_directory {
                    verify_owner_lease_pair_from_anchor(
                        visible_directory,
                        &visible_path,
                        &anchor_directory,
                        &anchor_path,
                        &file,
                    )?;
                } else {
                    verify_owner_lease_anchor(&anchor_directory, &anchor_path, &file)?;
                }
                OwnerLeaseProbe::Live
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!("failed to inspect subagent owner lease: {error}"));
            }
        };
        if let OwnerLeaseProbe::Acquired(lease) = &probe {
            lease.verify_pair()?;
        }
        Ok(probe)
    }

    pub(crate) fn verify_pair(&self) -> Result<(), String> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| "subagent owner lease is already released".to_string())?;
        if let Some(visible_directory) = &self.visible_directory {
            verify_owner_lease_pair_from_anchor(
                visible_directory,
                &self.visible_path,
                &self.anchor_directory,
                &self.anchor_path,
                file,
            )
        } else {
            verify_owner_lease_anchor(&self.anchor_directory, &self.anchor_path, file)
        }
    }

    pub(crate) fn remove(self) -> Result<(), String> {
        self.remove_until(None)
    }

    pub(crate) fn remove_until(self, deadline: Option<Instant>) -> Result<(), String> {
        self.remove_until_with_guard(deadline, || Ok(()))
    }

    pub(crate) fn remove_until_with_guard(
        self,
        deadline: Option<Instant>,
        mut external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        external_guard()?;
        let nib = self
            .anchor_path
            .parent()
            .ok_or_else(|| "subagent owner lease anchor has no parent directory".to_string())?
            .to_path_buf();
        let project_root = nib
            .parent()
            .ok_or_else(|| "subagent owner lease has no project root".to_string())?
            .to_path_buf();
        let lock_path = owner_lease_namespace_lock_path(&project_root);
        match deadline {
            Some(deadline) => {
                with_bounded_delegation_lock_in_until(&lock_path, &nib, deadline, |_, deadline| {
                    external_guard()?;
                    self.remove_locked(Some(deadline), &mut external_guard)
                })
            }
            None => with_bounded_delegation_lock_in(
                &lock_path,
                &nib,
                OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT,
                |_, deadline| {
                    external_guard()?;
                    self.remove_locked(Some(deadline), &mut external_guard)
                },
            ),
        }
    }

    pub(crate) fn release_for_reconciliation(self) -> Result<(), String> {
        let verification = self.verify_pair();
        drop(self);
        verification
    }

    pub(crate) fn remove_locked(
        mut self,
        deadline: Option<Instant>,
        external_guard: &mut impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        external_guard()?;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| "subagent owner lease is already released".to_string())?;
        if let Some(visible_directory) = &self.visible_directory {
            match visible_directory.entry_kind(&self.visible_path)? {
                Some(crate::daemons::state::StableEntryKind::File) => {
                    verify_owner_lease_pair_from_anchor(
                        visible_directory,
                        &self.visible_path,
                        &self.anchor_directory,
                        &self.anchor_path,
                        file,
                    )?;
                    ensure_subagent_reconciliation_deadline(deadline)?;
                    visible_directory.remove_file_if_matches_with_guard(
                        &self.visible_path,
                        file,
                        ".nib-subagent-owner-visible-delete-",
                        || {
                            external_guard()?;
                            ensure_subagent_reconciliation_deadline(deadline)
                        },
                    )?;
                }
                Some(crate::daemons::state::StableEntryKind::Directory) => {
                    return Err(format!(
                        "subagent owner lease is not a regular file: {}",
                        self.visible_path.display()
                    ));
                }
                None => {
                    verify_owner_lease_anchor(&self.anchor_directory, &self.anchor_path, file)?;
                }
            }
        } else {
            verify_owner_lease_anchor(&self.anchor_directory, &self.anchor_path, file)?;
        }
        ensure_subagent_reconciliation_deadline(deadline)?;
        self.anchor_directory.remove_file_if_matches_with_guard(
            &self.anchor_path,
            file,
            ".nib-subagent-owner-anchor-delete-",
            || {
                external_guard()?;
                ensure_subagent_reconciliation_deadline(deadline)
            },
        )?;
        drop(self.file.take());
        external_guard()?;
        Ok(())
    }
}

pub(crate) fn remove_persisted_owner_lease_until(
    project_root: &Path,
    execution_generation: u64,
    lease_id: &str,
    deadline: Option<Instant>,
) -> Result<(), String> {
    remove_persisted_owner_lease_until_with_guard(
        project_root,
        execution_generation,
        lease_id,
        deadline,
        || Ok(()),
    )
}

pub(crate) fn remove_persisted_owner_lease_until_with_guard(
    project_root: &Path,
    execution_generation: u64,
    lease_id: &str,
    requested_deadline: Option<Instant>,
    mut before_namespace_step: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    validate_execution_ownership(execution_generation, lease_id)?;
    ensure_subagent_reconciliation_deadline(requested_deadline)?;
    let nib = project_root.join(".nib");
    let lock_path = owner_lease_namespace_lock_path(project_root);
    let remove = |anchor_directory: &crate::daemons::state::StableDirectory,
                  lock_deadline: Instant| {
        let deadline = Some(lock_deadline);
        ensure_subagent_reconciliation_deadline(deadline)?;
        let visible_root = owner_lease_directory(project_root);
        let visible_directory = match anchor_directory.entry_kind(&visible_root)? {
            Some(crate::daemons::state::StableEntryKind::Directory) => {
                Some(anchor_directory.open_child(&visible_root)?)
            }
            Some(crate::daemons::state::StableEntryKind::File) => {
                return Err(format!(
                    "subagent owner lease directory is not a directory: {}",
                    visible_root.display()
                ));
            }
            None => None,
        };
        let visible = owner_lease_path(project_root, lease_id)?;
        let anchor = owner_lease_anchor_path(project_root, lease_id)?;
        let visible_state = match &visible_directory {
            Some(directory) => open_owner_deletion_artifact(
                directory,
                &visible,
                ".nib-subagent-owner-visible-delete-",
            )?,
            None => OwnerDeletionArtifact {
                canonical: None,
                quarantine: None,
                quarantine_path: visible.clone(),
            },
        };
        let anchor_state = open_owner_deletion_artifact(
            anchor_directory,
            &anchor,
            ".nib-subagent-owner-anchor-delete-",
        )?;
        ensure_subagent_reconciliation_deadline(deadline)?;
        let Some(authority) = owner_deletion_authority(&visible_state, &anchor_state)? else {
            return Ok(());
        };
        match authority.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "terminal subagent owner lease is still live; artifacts were preserved: {lease_id}"
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to inspect terminal subagent owner lease: {error}"
                ));
            }
        }
        let mut namespace_guard = || {
            before_namespace_step()?;
            ensure_subagent_reconciliation_deadline(deadline)
        };
        if let Some(visible_directory) = &visible_directory {
            delete_owner_artifact_state(
                visible_directory,
                &visible,
                ".nib-subagent-owner-visible-delete-",
                &visible_state,
                &mut namespace_guard,
            )?;
        }
        delete_owner_artifact_state(
            anchor_directory,
            &anchor,
            ".nib-subagent-owner-anchor-delete-",
            &anchor_state,
            &mut namespace_guard,
        )?;
        ensure_subagent_reconciliation_deadline(deadline)?;
        Ok(())
    };
    match requested_deadline {
        Some(deadline) => with_bounded_delegation_lock_in_until(&lock_path, &nib, deadline, remove),
        None => with_bounded_delegation_lock_in(
            &lock_path,
            &nib,
            OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT,
            remove,
        ),
    }
}

#[cfg(test)]
pub(crate) struct TestSubagentOwnerLease(pub(crate) SubagentOwnerLease);

#[cfg(test)]
impl TestSubagentOwnerLease {
    pub(crate) fn execution_generation(&self) -> u64 {
        self.0.execution_generation
    }

    pub(crate) fn lease_id(&self) -> &str {
        &self.0.lease_id
    }
}

#[cfg(test)]
pub(crate) fn create_test_subagent_owner_lease(
    project_root: &Path,
) -> Result<TestSubagentOwnerLease, String> {
    SubagentOwnerLease::create(project_root).map(TestSubagentOwnerLease)
}

pub(crate) fn cleanup_created_owner_lease_pair(
    visible_directory: &crate::daemons::state::StableDirectory,
    visible_path: &Path,
    visible_file: &File,
    anchor_directory: &crate::daemons::state::StableDirectory,
    anchor_path: &Path,
    anchor_file: &File,
    namespace_guard: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let visible = visible_directory.remove_file_if_matches_with_guard(
        visible_path,
        visible_file,
        ".nib-subagent-owner-create-visible-delete-",
        &mut *namespace_guard,
    );
    let anchor = anchor_directory.remove_file_if_matches_with_guard(
        anchor_path,
        anchor_file,
        ".nib-subagent-owner-create-anchor-delete-",
        &mut *namespace_guard,
    );
    match (visible, anchor) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(visible), Ok(())) => Err(visible),
        (Ok(()), Err(anchor)) => Err(anchor),
        (Err(visible), Err(anchor)) => Err(format!("{visible}; {anchor}")),
    }
}

pub(crate) fn verify_owner_lease_pair(
    visible_directory: &crate::daemons::state::StableDirectory,
    visible_path: &Path,
    visible_file: &File,
    anchor_directory: &crate::daemons::state::StableDirectory,
    anchor_path: &Path,
    anchor_file: &File,
) -> Result<(), String> {
    visible_directory.verify_visible()?;
    anchor_directory.verify_visible()?;
    visible_directory.verify_file_identity(visible_path, visible_file)?;
    anchor_directory.verify_file_identity(anchor_path, anchor_file)?;
    let visible_identity = crate::fs_security::FileIdentity::from_file(
        visible_file
            .try_clone()
            .map_err(|error| format!("failed to clone subagent owner lease: {error}"))?,
    )
    .map_err(|error| format!("failed to identify subagent owner lease: {error}"))?;
    let anchor_identity = crate::fs_security::FileIdentity::from_file(
        anchor_file
            .try_clone()
            .map_err(|error| format!("failed to clone subagent owner lease anchor: {error}"))?,
    )
    .map_err(|error| format!("failed to identify subagent owner lease anchor: {error}"))?;
    if visible_identity != anchor_identity {
        return Err(
            "subagent owner lease and persistent anchor have different identities".to_string(),
        );
    }
    Ok(())
}

pub(crate) fn verify_owner_lease_pair_from_anchor(
    visible_directory: &crate::daemons::state::StableDirectory,
    visible_path: &Path,
    anchor_directory: &crate::daemons::state::StableDirectory,
    anchor_path: &Path,
    anchor_file: &File,
) -> Result<(), String> {
    let visible_file = visible_directory.open_read_write(visible_path)?;
    verify_owner_lease_pair(
        visible_directory,
        visible_path,
        &visible_file,
        anchor_directory,
        anchor_path,
        anchor_file,
    )
}

pub(crate) fn verify_owner_lease_anchor(
    anchor_directory: &crate::daemons::state::StableDirectory,
    anchor_path: &Path,
    anchor_file: &File,
) -> Result<(), String> {
    anchor_directory.verify_visible()?;
    anchor_directory.verify_file_identity(anchor_path, anchor_file)
}

pub(crate) struct OwnerDeletionArtifact {
    pub(crate) canonical: Option<File>,
    pub(crate) quarantine: Option<File>,
    pub(crate) quarantine_path: PathBuf,
}

pub(crate) fn open_owner_deletion_artifact(
    directory: &crate::daemons::state::StableDirectory,
    canonical_path: &Path,
    quarantine_prefix: &str,
) -> Result<OwnerDeletionArtifact, String> {
    let quarantine_path =
        directory.deterministic_artifact_path(canonical_path, quarantine_prefix, ".quarantine")?;
    let canonical = directory
        .path_exists(canonical_path)?
        .then(|| directory.open_read_write(canonical_path))
        .transpose()?;
    let quarantine = directory
        .path_exists(&quarantine_path)?
        .then(|| directory.open_read_write(&quarantine_path))
        .transpose()?;
    if let (Some(canonical), Some(quarantine)) = (&canonical, &quarantine) {
        if !crate::daemons::state::same_open_file_identity(canonical, quarantine)? {
            return Err(format!(
                "subagent owner artifact and its deletion quarantine have different identities; both were preserved: {}",
                canonical_path.display()
            ));
        }
    }
    Ok(OwnerDeletionArtifact {
        canonical,
        quarantine,
        quarantine_path,
    })
}

pub(crate) fn owner_deletion_authority<'a>(
    visible: &'a OwnerDeletionArtifact,
    anchor: &'a OwnerDeletionArtifact,
) -> Result<Option<&'a File>, String> {
    let mut authority = None;
    for candidate in [
        anchor.canonical.as_ref(),
        anchor.quarantine.as_ref(),
        visible.canonical.as_ref(),
        visible.quarantine.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(authority) = authority {
            if !crate::daemons::state::same_open_file_identity(authority, candidate)? {
                return Err(
                    "subagent owner lease artifacts have different identities; all were preserved"
                        .to_string(),
                );
            }
        } else {
            authority = Some(candidate);
        }
    }
    Ok(authority)
}

pub(crate) fn delete_owner_artifact_state(
    directory: &crate::daemons::state::StableDirectory,
    canonical_path: &Path,
    quarantine_prefix: &str,
    state: &OwnerDeletionArtifact,
    namespace_guard: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if let Some(canonical) = &state.canonical {
        directory.remove_file_if_matches_with_guard(
            canonical_path,
            canonical,
            quarantine_prefix,
            &mut *namespace_guard,
        )
    } else if let Some(quarantine) = &state.quarantine {
        directory.remove_visible_file_if_matches_direct_with_guard(
            &state.quarantine_path,
            quarantine,
            &mut *namespace_guard,
        )
    } else {
        Ok(())
    }
}

pub(crate) fn exact_deletion_quarantine_name(name: &std::ffi::OsStr, prefix: &str) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(digest) = name
        .strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(".quarantine"))
    else {
        return false;
    };
    digest.len() == 32 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn new_execution_generation() -> u64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let generation = u64::from_be_bytes(bytes[..8].try_into().expect("UUID half is eight bytes"));
    generation.max(1)
}

pub(crate) fn validate_execution_ownership(
    execution_generation: u64,
    lease_id: &str,
) -> Result<(), String> {
    if execution_generation == 0 {
        return Err("subagent execution generation must be non-zero".to_string());
    }
    let parsed = uuid::Uuid::parse_str(lease_id)
        .map_err(|_| "subagent owner lease identifier is invalid".to_string())?;
    if parsed.to_string() != lease_id {
        return Err("subagent owner lease identifier is not canonical".to_string());
    }
    Ok(())
}

pub(crate) fn owner_lease_directory(project_root: &Path) -> PathBuf {
    project_root.join(".nib").join(OWNER_LEASE_DIRECTORY)
}

pub(crate) fn owner_lease_namespace_lock_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".nib")
        .join(".nib-subagent-owner-namespace.lock")
}
