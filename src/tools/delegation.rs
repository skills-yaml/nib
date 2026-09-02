use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
#[cfg(any(test, debug_assertions))]
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Output;
#[cfg(not(test))]
use std::process::Stdio;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::BoundaryConfig;
use crate::tools::executor::ApprovalHandler;
use crate::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};

#[cfg(test)]
const INTERRUPTED_ERROR: &str =
    "subagent runtime ended before a durable terminal result was recorded";
const MAX_SUBAGENT_RECORDS: usize = 10_000;
const MAX_SUBAGENT_RECORD_BYTES: u64 = 16 * 1024 * 1024;
const SUBAGENT_RECORD_LOCK_STRIPES: usize = 64;
const MAX_LEGACY_RECORD_LOCK_ENTRIES: usize = MAX_SUBAGENT_RECORDS + 1_024;
const MAX_LEGACY_RECORD_LOCK_NAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_LEGACY_RECORD_LOCK_MIGRATION_PASSES: usize = 8;
const LEGACY_RECORD_LOCK_MIGRATION_RECEIPT: &str = ".legacy-lock-migration-v1.json";
const NATIVE_RECORDS_STAGING_DIRECTORY: &str = ".subagents-native-origin-v1.staging";
const LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION: u32 = 1;
const MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES: u64 = 4 * 1024 * 1024;
const MERGE_PENDING_STATUS: &str = "merge_pending";
const MERGE_FAILED_STATUS: &str = "merge_failed";
const NIB_EXCLUDE_PATHSPEC: &str = ":(exclude).nib";
const NIB_DESCENDANTS_EXCLUDE_PATHSPEC: &str = ":(exclude).nib/**";
const REPOSITORY_MERGE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const SUBAGENT_RECORD_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(test)]
thread_local! {
    static TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
    static TEST_SPAWN_RECONCILIATION_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(all(test, windows))]
pub(crate) struct SpawnPositiveProgressTimeoutGuard {
    previous_preparation: Option<Duration>,
    previous_reconciliation: Option<Duration>,
    _not_send_or_sync: std::marker::PhantomData<std::rc::Rc<()>>,
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
    static TEST_SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) struct SubagentCancellationTimeoutGuard(Option<Duration>);

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
thread_local! {
    static SPAWN_HANDOFF_PHASE_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&'static str)>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_spawn_handoff_phase_hook(phase: &'static str) {
    SPAWN_HANDOFF_PHASE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(phase);
        }
    });
}

#[cfg(not(test))]
fn run_spawn_handoff_phase_hook(_phase: &'static str) {}

fn spawn_preparation_operation_timeout() -> Duration {
    #[cfg(test)]
    {
        return TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT
            .with(|timeout| timeout.get())
            .unwrap_or(SUBAGENT_RECORD_LOCK_TIMEOUT);
    }
    #[cfg(not(test))]
    {
        SUBAGENT_RECORD_LOCK_TIMEOUT
    }
}

fn spawn_reconciliation_deadline_timeout() -> Duration {
    #[cfg(test)]
    {
        return TEST_SPAWN_RECONCILIATION_TIMEOUT
            .with(|timeout| timeout.get())
            .unwrap_or(SUBAGENT_RECORD_LOCK_TIMEOUT);
    }
    #[cfg(not(test))]
    {
        SUBAGENT_RECORD_LOCK_TIMEOUT
    }
}

fn spawn_reconciliation_worktree_timeout() -> Duration {
    #[cfg(test)]
    {
        return TEST_SPAWN_RECONCILIATION_TIMEOUT
            .with(|timeout| timeout.get())
            .unwrap_or(SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT);
    }
    #[cfg(not(test))]
    {
        SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT
    }
}

fn subagent_cancellation_reconciliation_timeout() -> Duration {
    #[cfg(test)]
    {
        return TEST_SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT
            .with(|timeout| timeout.get())
            .unwrap_or(SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT);
    }
    #[cfg(not(test))]
    {
        SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT
    }
}

const SUBAGENT_CANCELLATION_RECONCILIATION_ATTEMPTS: usize = 500;
#[cfg(not(test))]
const SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT: Duration = Duration::from_secs(4);
#[cfg(test)]
pub(crate) const SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT: Duration =
    Duration::from_millis(250);
const MAX_SUBAGENT_DIRECTORY_ENTRIES: usize = MAX_SUBAGENT_RECORDS + 1_024;
const MAX_SUBAGENT_DIRECTORY_NAME_BYTES: usize = 4 * 1024 * 1024;
const OWNER_LEASE_DIRECTORY: &str = "subagent-owner-leases";
const OWNER_LEASE_SUFFIX: &str = ".lease";
const OWNER_LEASE_ANCHOR_PREFIX: &str = ".subagent-owner-";
const OWNER_LEASE_ANCHOR_SUFFIX: &str = ".anchor";
const OWNER_LOST_ERROR: &str =
    "subagent execution was interrupted because its owner process is no longer live";
const SUBAGENT_AUDIT_DESTINATION_ENCODING_ERROR: &str =
    "subagent audit destination cannot be represented without changing its filesystem identity";
const MAX_SUBAGENT_WORKER_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const SUBAGENT_SUPERVISOR_READY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(debug_assertions, not(test)))]
const SUBAGENT_LAUNCH_FAILPOINT_ENV: &str = "NIB_TEST_SUBAGENT_LAUNCH_FAILPOINT";
#[cfg(all(debug_assertions, not(test)))]
const SUBAGENT_READY_BEFORE_COMMIT_PATH_ENV: &str = "NIB_TEST_SUBAGENT_READY_BEFORE_COMMIT_PATH";
#[cfg(all(debug_assertions, not(test)))]
const SUBAGENT_WORKER_STARTED_PATH_ENV: &str = "NIB_TEST_SUBAGENT_WORKER_STARTED_PATH";
#[cfg(all(debug_assertions, not(test)))]
const SUBAGENT_WORKER_DELAY_MS_ENV: &str = "NIB_TEST_SUBAGENT_WORKER_DELAY_MS";
#[cfg(all(debug_assertions, not(test)))]
const SUBAGENT_SCOPE_PREPARED_PATH_ENV: &str = "NIB_TEST_SUBAGENT_SCOPE_PREPARED_PATH";
#[cfg(all(debug_assertions, not(test)))]
const SUBAGENT_SUPERVISOR_PRE_REGISTER_PATH_ENV: &str =
    "NIB_TEST_SUBAGENT_SUPERVISOR_PRE_REGISTER_PATH";
#[cfg(all(debug_assertions, not(test)))]
const SUBAGENT_SUPERVISOR_REGISTER_RELEASE_ENV: &str =
    "NIB_TEST_SUBAGENT_SUPERVISOR_REGISTER_RELEASE";

#[derive(Debug, Serialize, Deserialize)]
struct SubagentWorkerRequest {
    prompt: String,
    max_steps: u32,
}

const SUBAGENT_SUPERVISOR_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubagentSupervisorRequest {
    version: u32,
    handoff_nonce: String,
    subagent_id: String,
    execution_generation: u64,
    owner_lease: String,
    cleanup_lease_id: String,
    worker: SubagentWorkerRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SubagentSupervisorFrame {
    version: u32,
    phase: String,
    handoff_nonce: String,
    subagent_id: String,
    execution_generation: u64,
    owner_lease: String,
    process_scope: crate::sandbox::process::ProcessScopeRecord,
}

#[cfg(not(test))]
struct SubagentSupervisorHandoff {
    control: Arc<Mutex<Option<std::process::ChildStdin>>>,
    responses: std::sync::mpsc::Receiver<Result<SubagentSupervisorFrame, String>>,
    ready: SubagentSupervisorFrame,
}

struct LaunchedSubagentTask {
    response: Value,
    #[cfg(test)]
    precommit_process_scope: Option<crate::sandbox::process::ProcessScopeRecord>,
    #[cfg(not(test))]
    supervisor_handoff: Option<SubagentSupervisorHandoff>,
}

impl LaunchedSubagentTask {
    #[cfg(test)]
    fn precommit_process_scope(&self) -> Option<crate::sandbox::process::ProcessScopeRecord> {
        self.precommit_process_scope.clone()
    }

    #[cfg(not(test))]
    fn precommit_process_scope(&self) -> Option<crate::sandbox::process::ProcessScopeRecord> {
        self.supervisor_handoff
            .as_ref()
            .map(|handoff| handoff.ready.process_scope.clone())
    }

    #[cfg(test)]
    fn commit_supervisor_handoff(
        &mut self,
        authority: &SpawnPreparationAuthority,
    ) -> Result<(), String> {
        authority.verify_until(authority.operation_deadline())
    }

    #[cfg(not(test))]
    fn commit_supervisor_handoff(
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

fn validate_subagent_supervisor_frame(
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
struct SubagentWorkerResponse {
    outcome: Result<crate::agent::AgentRunSummary, String>,
}

#[cfg(test)]
static CANCELLED_RECORD_WRITE_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static RECOVERABLE_REVISION_PUBLICATION_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static SPAWN_OWNER_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SPAWN_RECORD_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SPAWN_WORKTREE_CLEANUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SPAWN_OWNER_CLEANUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SPAWN_AUDIT_CLEANUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SPAWN_SESSION_CLEANUP_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SPAWN_SESSION_PUBLICATION_FAILURES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static SPAWN_POST_AUDIT_CANCELLATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn consume_spawn_failure(counter: &std::sync::atomic::AtomicUsize) -> bool {
    counter
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |remaining| remaining.checked_sub(1),
        )
        .is_ok()
}

#[derive(Debug)]
struct SpawnOwnerCreationError {
    message: String,
    mutation_indeterminate: bool,
}

fn create_spawn_owner_lease(
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

fn write_spawn_subagent_record_locked(
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

fn commit_spawn_handoff_record_locked(
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
    static AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_after_subagent_audit_preflight_hook() {
    AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_after_subagent_audit_preflight_hook() {}

#[cfg(test)]
thread_local! {
    static AFTER_PREPARATION_INTENT_OPEN_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_after_preparation_intent_open_hook() {
    AFTER_PREPARATION_INTENT_OPEN_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_after_preparation_intent_open_hook() {}

#[cfg(test)]
thread_local! {
    static SPAWN_FORWARD_MUTATION_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(&'static str)>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_spawn_forward_mutation_hook(boundary: &'static str) {
    SPAWN_FORWARD_MUTATION_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(boundary);
        }
    });
}

#[cfg(not(test))]
fn run_spawn_forward_mutation_hook(_boundary: &'static str) {}

#[cfg(test)]
type SpawnAuthorityVerifyHook = std::sync::Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

#[cfg(test)]
static SPAWN_AUTHORITY_VERIFY_HOOK: std::sync::LazyLock<
    std::sync::Mutex<Option<SpawnAuthorityVerifyHook>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn run_spawn_authority_verify_hook(records_path: &Path) -> Result<(), String> {
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
fn run_spawn_authority_verify_hook(_records_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(debug_assertions)]
fn pause_after_subagent_audit_preparation(subagent_id: &str) -> Result<(), String> {
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
fn pause_after_subagent_audit_preparation(_subagent_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(debug_assertions)]
fn pause_after_spawn_preparation_intent(subagent_id: &str) -> Result<(), String> {
    let Some(ready) = std::env::var_os("NIB_TEST_SUBAGENT_INTENT_PLANNED_READY") else {
        return Ok(());
    };
    let ready = PathBuf::from(ready);
    std::fs::write(&ready, subagent_id.as_bytes())
        .map_err(|error| format!("failed to publish planned intent readiness: {error}"))?;
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
fn pause_after_spawn_preparation_intent(_subagent_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(all(debug_assertions, not(test)))]
fn pause_after_supervisor_ready_before_commit(subagent_id: &str) -> Result<(), String> {
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
fn pause_after_supervisor_ready_before_commit(_subagent_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(all(debug_assertions, not(test)))]
fn pause_after_scope_prepared_before_supervisor_spawn(subagent_id: &str) -> Result<(), String> {
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
fn pause_after_scope_prepared_before_supervisor_spawn(_subagent_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(all(debug_assertions, not(test)))]
fn pause_before_supervisor_self_registration(
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
fn pause_before_supervisor_self_registration(
    _subagent_id: &str,
    _identity: &crate::sandbox::process::ProcessIdentity,
) -> Result<(), String> {
    Ok(())
}

#[cfg(debug_assertions)]
type MergeInterruptionBarrierMap =
    std::collections::HashMap<(PathBuf, String), tokio::sync::oneshot::Sender<Result<(), String>>>;

#[cfg(debug_assertions)]
static MERGE_INTERRUPTION_TEST_BARRIERS: std::sync::LazyLock<
    std::sync::Mutex<MergeInterruptionBarrierMap>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(debug_assertions)]
#[doc(hidden)]
pub struct MergeInterruptionTestBarrier {
    reached: tokio::sync::oneshot::Receiver<Result<(), String>>,
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
    sessions_dir: PathBuf,
    directory_identity: crate::fs_security::FileIdentitySnapshot,
}

const OWNERSHIP_AUDIT_TARGET_KEY: &str = "_ownership_audit_target";
const WORKTREE_PREPARATION_RECEIPT_KEY: &str = "_worktree_ownership_receipt";
const SPAWN_HANDOFF_KEY: &str = "_spawn_handoff";
const SPAWN_HANDOFF_VERSION: u32 = 1;
const SPAWN_PREPARATION_DIRECTORY: &str = ".preparations";
const LEGACY_SPAWN_PREPARATION_VERSION: u32 = 2;
const HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION: u32 = 3;
const SPAWN_PREPARATION_VERSION: u32 = 4;
const MAX_SPAWN_PREPARATION_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SubagentProcessScopePlan {
    cleanup_lease_id: String,
    supervisor_registration_nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnPreparationIntentData {
    version: u32,
    revision: u64,
    phase: SpawnPreparationPhase,
    subagent_id: String,
    owner: SubagentOwnerPlan,
    worktree: crate::sandbox::worktree::WorktreePreparationAuthority,
    audit_session_id: String,
    audit_sessions_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audit_namespace_plan: Option<crate::session::SessionNamespacePreparationPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audit_target: Option<SubagentAuditTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audit_receipt: Option<crate::session::SessionPreparationReceipt>,
    /// Exact authority published before any process-scope or supervisor
    /// mutation. Version-four launches must consume these values unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_scope_plan: Option<SubagentProcessScopePlan>,
    /// Exact supervisor READY authority. New production handoffs persist this
    /// before COMMIT so restart never infers execution from a filename or a
    /// legacy launch flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff_process_scope: Option<crate::sandbox::process::ProcessScopeRecord>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SpawnPreparationPhase {
    Planned,
    ResourcesPrepared,
    AuditPlanned,
    AuditPublished,
    RecordPublished,
    ManagerRegistered,
    HandoffProven,
}

struct SpawnPreparationIntent {
    data: SpawnPreparationIntentData,
    authority: std::sync::Arc<SpawnPreparationAuthority>,
    directory: crate::daemons::state::StableDirectory,
    path: PathBuf,
    file: File,
    created_directory_parent: Option<crate::daemons::state::StableDirectory>,
}

struct SpawnPreparationAuthority {
    records: crate::daemons::state::StableDirectory,
    migration_fence: crate::daemons::state::HeldFileLock,
    record_stripe: crate::daemons::state::HeldFileLock,
    operation_deadline: Instant,
}

impl SpawnPreparationAuthority {
    fn operation_deadline(&self) -> Instant {
        self.operation_deadline
    }

    #[cfg(test)]
    fn verify(&self) -> Result<(), String> {
        self.migration_fence.verify()?;
        self.record_stripe.verify()?;
        self.records.verify_visible()
    }

    fn verify_until(&self, deadline: Instant) -> Result<(), String> {
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
struct LegacyRecordLockMigrationReceipt {
    version: u32,
    epoch_id: String,
    records_identity: crate::fs_security::DirectoryIdentity,
    phase: LegacyRecordLockMigrationPhase,
    attested_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    artifacts: Vec<LegacyRecordLockMigrationArtifact>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LegacyRecordLockMigrationPhase {
    Pending,
    Completed,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LegacyRecordLockMigrationArtifact {
    path: PathBuf,
    quarantine_path: Option<PathBuf>,
    identity: crate::fs_security::FileIdentitySnapshot,
}

struct OpenedSubagentRecord {
    record: SubagentRecord,
    file: File,
}

struct InitialSubagentRecordPublication {
    receipt: crate::daemons::state::FilePublicationReceipt,
}

struct InitialSubagentRecordPublicationError {
    message: String,
    receipt: Option<crate::daemons::state::FilePublicationReceipt>,
    publication_attempted: bool,
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
struct SubagentOwnerLease {
    visible_directory: Option<crate::daemons::state::StableDirectory>,
    anchor_directory: crate::daemons::state::StableDirectory,
    visible_path: PathBuf,
    anchor_path: PathBuf,
    file: Option<File>,
    execution_generation: u64,
    lease_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SubagentOwnerPlan {
    execution_generation: u64,
    lease_id: String,
}

enum OwnerLeaseProbe {
    Live,
    Acquired(SubagentOwnerLease),
}

struct OwnershipReconciliationWork {
    record: SubagentRecord,
    evidence: Option<Value>,
    acquired_owner_lease: Option<SubagentOwnerLease>,
    retry_persisted_owner_cleanup: bool,
}

impl SubagentOwnerLease {
    fn plan() -> SubagentOwnerPlan {
        SubagentOwnerPlan {
            execution_generation: new_execution_generation(),
            lease_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    #[cfg(test)]
    fn create(project_root: &Path) -> Result<Self, String> {
        Self::create_from_plan(project_root, &Self::plan())
    }

    #[cfg(test)]
    fn create_from_plan(project_root: &Path, plan: &SubagentOwnerPlan) -> Result<Self, String> {
        validate_execution_ownership(plan.execution_generation, &plan.lease_id)?;
        Self::create_with_timeout_and_guard(
            project_root,
            OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT,
            plan,
            || Ok(()),
        )
    }

    #[cfg(test)]
    fn create_with_timeout_and_guard(
        project_root: &Path,
        timeout: Duration,
        plan: &SubagentOwnerPlan,
        before_namespace_step: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        let started = Instant::now();
        let deadline = started.checked_add(timeout).unwrap_or(started);
        Self::create_until_with_guard(project_root, deadline, plan, before_namespace_step)
    }

    fn create_until_with_guard(
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

    fn create_locked(
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

    fn probe(
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

    fn verify_pair(&self) -> Result<(), String> {
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

    fn remove(self) -> Result<(), String> {
        self.remove_until(None)
    }

    fn remove_until(self, deadline: Option<Instant>) -> Result<(), String> {
        self.remove_until_with_guard(deadline, || Ok(()))
    }

    fn remove_until_with_guard(
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

    fn release_for_reconciliation(self) -> Result<(), String> {
        let verification = self.verify_pair();
        drop(self);
        verification
    }

    fn remove_locked(
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

fn remove_persisted_owner_lease_until(
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

fn remove_persisted_owner_lease_until_with_guard(
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
pub(crate) struct TestSubagentOwnerLease(SubagentOwnerLease);

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

fn cleanup_created_owner_lease_pair(
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

fn verify_owner_lease_pair(
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

fn verify_owner_lease_pair_from_anchor(
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

fn verify_owner_lease_anchor(
    anchor_directory: &crate::daemons::state::StableDirectory,
    anchor_path: &Path,
    anchor_file: &File,
) -> Result<(), String> {
    anchor_directory.verify_visible()?;
    anchor_directory.verify_file_identity(anchor_path, anchor_file)
}

struct OwnerDeletionArtifact {
    canonical: Option<File>,
    quarantine: Option<File>,
    quarantine_path: PathBuf,
}

fn open_owner_deletion_artifact(
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

fn owner_deletion_authority<'a>(
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

fn delete_owner_artifact_state(
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

fn exact_deletion_quarantine_name(name: &std::ffi::OsStr, prefix: &str) -> bool {
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

fn new_execution_generation() -> u64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let generation = u64::from_be_bytes(bytes[..8].try_into().expect("UUID half is eight bytes"));
    generation.max(1)
}

fn validate_execution_ownership(execution_generation: u64, lease_id: &str) -> Result<(), String> {
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

fn owner_lease_directory(project_root: &Path) -> PathBuf {
    project_root.join(".nib").join(OWNER_LEASE_DIRECTORY)
}

fn owner_lease_namespace_lock_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".nib")
        .join(".nib-subagent-owner-namespace.lock")
}

fn open_owner_lease_directories(
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

fn owner_lease_path(project_root: &Path, lease_id: &str) -> Result<PathBuf, String> {
    let parsed = uuid::Uuid::parse_str(lease_id)
        .map_err(|_| "subagent owner lease identifier is invalid".to_string())?;
    if parsed.to_string() != lease_id {
        return Err("subagent owner lease identifier is not canonical".to_string());
    }
    Ok(owner_lease_directory(project_root).join(format!("{lease_id}{OWNER_LEASE_SUFFIX}")))
}

fn owner_lease_anchor_path(project_root: &Path, lease_id: &str) -> Result<PathBuf, String> {
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

struct SubagentCancellationWorker {
    worker: Option<std::thread::JoinHandle<()>>,
}

impl SubagentCancellationWorker {
    fn new(worker: std::thread::JoinHandle<()>) -> Self {
        Self {
            worker: Some(worker),
        }
    }

    fn join(&mut self) -> Result<(), String> {
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

struct NonInteractiveSubagentApproval;

#[derive(Debug)]
struct RepositoryMergeLock {
    _anchor_file: File,
}

impl RepositoryMergeLock {
    async fn acquire(
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
    async fn acquire_with_timeout(project_root: &Path, timeout: Duration) -> Result<Self, String> {
        Self::acquire_with_timeout_and_cancellation(project_root, timeout, None).await
    }

    async fn acquire_with_timeout_and_cancellation(
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

fn ensure_repository_merge_lock_not_cancelled(
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<(), String> {
    if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
        return Err(repository_merge_lock_cancelled());
    }
    Ok(())
}

fn repository_merge_lock_cancelled() -> String {
    "repository merge lock acquisition was cancelled".to_string()
}

fn with_bounded_delegation_lock_in<T>(
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

fn with_bounded_delegation_lock_in_until<T>(
    lock_path: &Path,
    protected_directory: &Path,
    deadline: Instant,
    operation: impl FnOnce(&crate::daemons::state::StableDirectory, Instant) -> Result<T, String>,
) -> Result<T, String> {
    with_delegation_lock_in_deadline(lock_path, protected_directory, deadline, None, operation)
}

fn with_delegation_lock_in_deadline<T>(
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

fn with_delegation_lock_in_deadline_with_setup_hook<T>(
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

fn delegation_lock_project_root(lock_path: &Path) -> Result<PathBuf, String> {
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

fn with_delegation_lock_in_deadline_bound_to<T>(
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

fn ensure_delegation_lock_deadline(
    deadline: Instant,
    lock_path: &Path,
    timeout: Option<Duration>,
) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err(delegation_lock_deadline_error(lock_path, timeout));
    }
    Ok(())
}

fn delegation_lock_deadline_error(lock_path: &Path, timeout: Option<Duration>) -> String {
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

fn repository_merge_lock_timeout(path: &Path, timeout: Duration) -> String {
    format!(
        "timed out acquiring repository merge lock {} after {} seconds",
        path.display(),
        timeout.as_secs_f64()
    )
}

fn repository_merge_lock_anchor_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".nib")
        .join(".subagents.merge.lock.anchor")
}

#[cfg(all(test, any(unix, windows)))]
fn open_repository_merge_lock_anchor(lock_path: &Path, anchor_path: &Path) -> Result<File, String> {
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
fn open_repository_merge_lock_anchor(
    lock_path: &Path,
    _anchor_path: &Path,
) -> Result<File, String> {
    Err(format!(
        "repository merge lock anchors are unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(all(test, any(unix, windows)))]
fn repository_merge_lock_path_exists(path: &Path) -> Result<bool, String> {
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
fn create_repository_merge_lock_link(source: &Path, destination: &Path) -> Result<(), String> {
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
fn open_repository_merge_lock(path: &Path) -> Result<File, String> {
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
fn open_repository_merge_lock(path: &Path) -> Result<File, String> {
    Err(format!(
        "stable no-follow repository merge locks are unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
fn repository_lock_identity(
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
fn repository_lock_identity(_file: &File, path: &Path) -> Result<(), String> {
    Err(format!(
        "repository merge lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(all(test, any(unix, windows)))]
fn open_repository_lock_identity(path: &Path) -> Result<crate::fs_security::FileIdentity, String> {
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
fn open_repository_lock_identity(path: &Path) -> Result<(), String> {
    Err(format!(
        "repository merge lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(test)]
fn validate_repository_lock_path(file: &File, path: &Path) -> Result<(), String> {
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
fn open_repository_lock_probe(path: &Path) -> Result<File, String> {
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
struct SubagentRunGuard {
    project_root: PathBuf,
    id: String,
    execution_generation: u64,
    lease_id: String,
    owner_lease: Option<SubagentOwnerLease>,
    armed: bool,
    reason: String,
}

struct PreparedSubagentTask {
    record: SubagentRecord,
    worktree: crate::sandbox::worktree::Worktree,
    owner_lease: SubagentOwnerLease,
}

#[cfg(test)]
impl SubagentRunGuard {
    fn new(project_root: PathBuf, id: String, owner_lease: SubagentOwnerLease) -> Self {
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

    fn disarm(&mut self) {
        self.armed = false;
        self.cleanup_owner_lease();
    }

    fn cleanup_owner_lease(&mut self) {
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

enum SubagentAuditPreparationPlan {
    Provided {
        store: crate::session::SessionStore,
        target: SubagentAuditTarget,
        encoded: Value,
        runtime_config: crate::config::NibConfig,
    },
    Fallback(crate::session::SessionDirectoryPreflight),
}

impl SubagentAuditPreparationPlan {
    fn runtime_config(&self) -> &crate::config::NibConfig {
        match self {
            Self::Provided { runtime_config, .. } => runtime_config,
            Self::Fallback(preflight) => preflight.runtime_config(),
        }
    }

    fn verify_continuity(&self) -> Result<(), String> {
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
    fn fallback_sessions_dir(&self) -> Option<&Path> {
        match self {
            Self::Fallback(preflight) => Some(preflight.sessions_dir()),
            Self::Provided { .. } => None,
        }
    }

    fn durable_audit_destination(&self) -> (&Path, Option<SubagentAuditTarget>) {
        match self {
            Self::Fallback(preflight) => (preflight.sessions_dir(), None),
            Self::Provided { target, .. } => (&target.sessions_dir, Some(target.clone())),
        }
    }

    #[cfg(test)]
    fn fallback_namespace_plan(
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

    fn fallback_namespace_plan_after_records(
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

struct PreparedSubagentAudit {
    encoded: Value,
    fallback: Option<crate::session::SessionStorePreparation>,
}

impl PreparedSubagentAudit {
    #[cfg(test)]
    fn cleanup(self) -> Result<(), String> {
        match self.fallback {
            Some(preparation) => preparation.cleanup(
                Instant::now()
                    .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
                    .unwrap_or_else(Instant::now),
            ),
            None => Ok(()),
        }
    }

    fn cleanup_with_authority(
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

    fn disarm(self) {
        if let Some(preparation) = self.fallback {
            drop(preparation.disarm());
        }
    }
}

#[cfg(test)]
fn spawn_preparation_directory_path(project_root: &Path) -> PathBuf {
    records_dir(project_root).join(SPAWN_PREPARATION_DIRECTORY)
}

fn open_or_create_spawn_preparation_directory(
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
    fn create(
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

    fn revise(
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

    fn cleanup(self) -> Result<(), String> {
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

fn encode_spawn_preparation_intent(data: &SpawnPreparationIntentData) -> Result<Vec<u8>, String> {
    validate_spawn_preparation_intent_structure(data)?;
    let encoded = serde_json::to_vec_pretty(data).map_err(|error| error.to_string())?;
    if encoded.len() as u64 > MAX_SPAWN_PREPARATION_BYTES {
        return Err("subagent preparation intent exceeds its size bound".to_string());
    }
    Ok(encoded)
}

fn is_canonical_spawn_preparation_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

/// Validates the one immutable process authority chain carried by a spawn
/// preparation. Version four binds the preplanned cleanup and registration
/// values to the persisted READY snapshot and, when supplied, to the observed
/// scope. Older versions retain their existing absence-of-plan contract.
fn validate_spawn_preparation_process_scope_binding(
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

fn validate_spawn_preparation_intent_structure(
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

fn spawn_preparation_phase_revision(intent: &SpawnPreparationIntentData) -> Option<u64> {
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

fn spawn_handoff_evidence(intent: &SpawnPreparationIntentData, state: &str) -> Value {
    json!({
        "version": SPAWN_HANDOFF_VERSION,
        "state": state,
        "subagent_id": intent.subagent_id,
        "execution_generation": intent.owner.execution_generation,
        "owner_lease": intent.owner.lease_id,
        "worktree_receipt": intent.worktree.ownership_receipt_id,
    })
}

fn record_spawn_handoff_matches(
    intent: &SpawnPreparationIntentData,
    record: &SubagentRecord,
    expected_state: &str,
) -> bool {
    process_scope_retirement_result(record).and_then(|result| result.get(SPAWN_HANDOFF_KEY))
        == Some(&spawn_handoff_evidence(intent, expected_state))
}

#[cfg(test)]
fn record_has_valid_committed_spawn_handoff(record: &SubagentRecord) -> bool {
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

fn spawn_handoff_has_execution_evidence(
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

fn reconcile_pre_handoff_process_scope(
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

fn preserve_spawn_internal_authority(previous: Option<&Value>, next: &mut Value) {
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

fn adopt_spawn_preparation_publication_error(
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

fn reconcile_spawn_preparations(
    project_root: &Path,
    records: &crate::daemons::state::StableDirectory,
) -> Result<(), String> {
    records.verify_visible()?;
    let path = records.path().join(SPAWN_PREPARATION_DIRECTORY);
    let directory = match records.entry_kind(&path)? {
        Some(crate::daemons::state::StableEntryKind::Directory) => {
            records.open_owned_child(&path)?
        }
        Some(crate::daemons::state::StableEntryKind::File) => {
            return Err(format!(
                "subagent preparation namespace is unsafe and was preserved: {}",
                path.display()
            ));
        }
        None => return Ok(()),
    };
    let deadline = Instant::now()
        .checked_add(spawn_reconciliation_deadline_timeout())
        .ok_or_else(|| "subagent preparation reconciliation deadline overflow".to_string())?;
    let _preparation_fence = acquire_spawn_preparation_fence_until(records, deadline)?;
    let verify_records = || {
        ensure_subagent_reconciliation_deadline(Some(deadline))?;
        records.verify_visible()?;
        ensure_subagent_reconciliation_deadline(Some(deadline))
    };
    verify_records()?;
    recover_spawn_preparation_transactions(&directory, records, deadline)?;
    directory.recover_stale_temporary_files_strict_with_guard(
        ".nib-subagent-preparation-",
        MAX_SUBAGENT_RECORDS,
        MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
        &verify_records,
    )?;
    let mut names = Vec::new();
    directory.for_each_entry_bounded(
        MAX_SUBAGENT_RECORDS,
        MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
        |name| {
            let path = Path::new(&name);
            let bytes = name.as_encoded_bytes();
            let is_delete_quarantine = bytes.starts_with(b".nib-subagent-preparation-delete-")
                && bytes.ends_with(b".quarantine");
            if path.extension().and_then(|value| value.to_str()) != Some("json")
                && !is_delete_quarantine
            {
                return Err("subagent preparation namespace contains an unknown entry".to_string());
            }
            names.push(name);
            Ok(())
        },
    )?;
    // Directory iteration order is not stable across filesystems. Inspect every
    // deletion quarantine before reconciling any canonical intent so a
    // canonical/quarantine ambiguity cannot be discovered only after cleanup of
    // that intent's resources has begun.
    for name in &names {
        let name_bytes = name.as_encoded_bytes();
        if !name_bytes.starts_with(b".nib-subagent-preparation-delete-")
            || !name_bytes.ends_with(b".quarantine")
        {
            continue;
        }
        let quarantine_path = path.join(name);
        let quarantine_file = directory.open_read_write(&quarantine_path)?;
        let bytes = read_spawn_preparation_bytes(&quarantine_file, &quarantine_path)?;
        let intent: SpawnPreparationIntentData =
            serde_json::from_slice(&bytes).map_err(|error| {
                format!(
                    "invalid subagent preparation intent was preserved {}: {error}",
                    quarantine_path.display()
                )
            })?;
        if validate_spawn_preparation_intent_structure(&intent).is_err() {
            return Err(format!(
                "subagent preparation intent identity is invalid and was preserved: {}",
                quarantine_path.display()
            ));
        }
        let canonical_path = path.join(format!("{}.json", intent.subagent_id));
        let expected_quarantine = directory.deterministic_artifact_path(
            &canonical_path,
            ".nib-subagent-preparation-delete-",
            ".quarantine",
        )?;
        if quarantine_path != expected_quarantine {
            return Err(format!(
                "subagent preparation intent identity is invalid and was preserved: {}",
                quarantine_path.display()
            ));
        }
        if directory.path_exists(&canonical_path)? {
            return Err(format!(
                "subagent preparation intent has ambiguous canonical and quarantine state; both were preserved: {}",
                canonical_path.display()
            ));
        }
    }
    for name in names {
        let intent_path = path.join(&name);
        let file = directory.open_read_write(&intent_path)?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_SPAWN_PREPARATION_BYTES {
            return Err(format!(
                "subagent preparation intent is unsafe and was preserved: {}",
                intent_path.display()
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&file)
            .take(MAX_SPAWN_PREPARATION_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        let intent: SpawnPreparationIntentData =
            serde_json::from_slice(&bytes).map_err(|error| {
                format!(
                    "invalid subagent preparation intent was preserved {}: {error}",
                    intent_path.display()
                )
            })?;
        let expected_name = format!("{}.json", intent.subagent_id);
        let canonical_intent_path = path.join(&expected_name);
        let expected_quarantine = directory.deterministic_artifact_path(
            &canonical_intent_path,
            ".nib-subagent-preparation-delete-",
            ".quarantine",
        )?;
        let is_delete_quarantine = intent_path == expected_quarantine;
        if validate_spawn_preparation_intent_structure(&intent).is_err()
            || (!is_delete_quarantine && name != std::ffi::OsStr::new(&expected_name))
        {
            return Err(format!(
                "subagent preparation intent identity is invalid and was preserved: {}",
                intent_path.display()
            ));
        }
        if is_delete_quarantine && directory.path_exists(&canonical_intent_path)? {
            return Err(format!(
                "subagent preparation intent has ambiguous canonical and quarantine state; both were preserved: {}",
                canonical_intent_path.display()
            ));
        }
        run_after_preparation_intent_open_hook();
        records.verify_visible()?;
        let record_path = record_path(project_root, &intent.subagent_id)?;
        let record_quarantine = records.deterministic_artifact_path(
            &record_path,
            ".nib-subagent-precommit-delete-",
            ".quarantine",
        )?;
        let record_exists = records.path_exists(&record_path)?;
        let record_quarantine_exists = records.path_exists(&record_quarantine)?;
        if record_exists && record_quarantine_exists {
            return Err(format!(
                "subagent pre-handoff record has ambiguous canonical and quarantine state; both were preserved: {}",
                record_path.display()
            ));
        }
        let pending_record = if record_exists || record_quarantine_exists {
            let actual_path = if record_exists {
                &record_path
            } else {
                &record_quarantine
            };
            let opened = read_opened_subagent_record_in(records, actual_path)?;
            validate_spawn_intent_record_identity(&intent, &opened.record)?;
            let terminal = opened.record.status != "running";
            let execution_evidence = if intent.phase == SpawnPreparationPhase::HandoffProven {
                spawn_handoff_has_execution_evidence(
                    project_root,
                    records,
                    &intent,
                    &opened.record,
                    deadline,
                )?
            } else {
                false
            };
            if intent.phase == SpawnPreparationPhase::HandoffProven
                && (record_spawn_handoff_matches(&intent, &opened.record, "committed") || terminal)
                && execution_evidence
            {
                if terminal {
                    retire_terminal_process_scope_in_locked_records_until(
                        project_root,
                        records,
                        &opened.record,
                        deadline,
                    )?;
                }
                // Only a proven handoff (or its exact durable terminal result)
                // transfers authority from the preparation transaction.
                remove_spawn_preparation_entry(
                    &directory,
                    &canonical_intent_path,
                    &intent_path,
                    &file,
                    is_delete_quarantine,
                    records,
                    deadline,
                )?;
                continue;
            }
            if !(record_spawn_handoff_matches(&intent, &opened.record, "pending")
                || intent.phase == SpawnPreparationPhase::HandoffProven
                    && !execution_evidence
                    && record_spawn_handoff_matches(&intent, &opened.record, "committed"))
            {
                return Err(format!(
                    "subagent record does not prove or retain the pending handoff for {}; intent and resources were preserved",
                    intent.subagent_id
                ));
            }
            Some((opened, record_quarantine_exists))
        } else {
            None
        };
        reconcile_pre_handoff_process_scope(project_root, records, &intent, deadline)?;
        if crate::daemons::task::TASK_MANAGER
            .get_status(&intent.subagent_id)
            .is_some()
        {
            crate::daemons::task::TASK_MANAGER
                .rollback_unattached_task(&intent.subagent_id)
                .map_err(|error| {
                    format!(
                        "failed to roll back uncommitted subagent manager entry; preparation was preserved: {error}"
                    )
                })?;
        }
        match remove_persisted_owner_lease_until_with_guard(
            project_root,
            intent.owner.execution_generation,
            &intent.owner.lease_id,
            Some(deadline),
            &verify_records,
        ) {
            Ok(()) => {}
            Err(error) => {
                return Err(format!("failed to reconcile prepared owner: {error}"));
            }
        }
        if let Some(receipt) = &intent.audit_receipt {
            if receipt.sessions_dir != intent.audit_sessions_dir
                || receipt.session_id != intent.audit_session_id
            {
                return Err(format!(
                    "subagent preparation audit receipt is inconsistent and was preserved: {}",
                    intent_path.display()
                ));
            }
            crate::session::SessionStorePreparation::cleanup_durable_with_guard(
                receipt,
                deadline,
                &verify_records,
            )
            .map_err(|error| format!("failed to reconcile prepared audit namespace: {error}"))?;
        } else if let Some(namespace_plan) = &intent.audit_namespace_plan {
            crate::session::SessionStorePreparation::cleanup_planned_namespace_with_guard(
                namespace_plan,
                deadline,
                &verify_records,
            )
            .map_err(|error| format!("failed to reconcile planned audit namespace: {error}"))?;
        }
        crate::sandbox::worktree::Worktree::cleanup_preparation_authority_with_guard(
            project_root,
            &intent.worktree,
            spawn_reconciliation_worktree_timeout(),
            &verify_records,
        )
        .map_err(|error| format!("failed to reconcile prepared worktree: {error}"))?;
        if let Some((opened, quarantined)) = pending_record {
            if quarantined {
                records.remove_visible_file_if_matches_direct_with_guard(
                    &record_quarantine,
                    &opened.file,
                    &verify_records,
                )?;
            } else {
                records.remove_file_if_matches_with_guard(
                    &record_path,
                    &opened.file,
                    ".nib-subagent-precommit-delete-",
                    &verify_records,
                )?;
            }
        }
        remove_spawn_preparation_entry(
            &directory,
            &canonical_intent_path,
            &intent_path,
            &file,
            is_delete_quarantine,
            records,
            deadline,
        )
        .map_err(|error| format!("failed to reconcile preparation intent: {error}"))?;
    }
    Ok(())
}

fn recover_spawn_preparation_transactions(
    directory: &crate::daemons::state::StableDirectory,
    records: &crate::daemons::state::StableDirectory,
    deadline: Instant,
) -> Result<(), String> {
    validate_spawn_preparation_temporary_artifacts(directory, records, deadline)?;
    let mut previous_names = Vec::new();
    directory.for_each_entry_bounded(
        MAX_SUBAGENT_RECORDS,
        MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
        |name| {
            if crate::daemons::state::StableDirectory::atomic_previous_target_name(
                &name,
                ".nib-subagent-preparation-",
            )
            .is_some()
            {
                previous_names.push(name);
            }
            Ok(())
        },
    )?;
    for previous_name in previous_names {
        ensure_subagent_reconciliation_deadline(Some(deadline))?;
        records.verify_visible()?;
        let target_name = crate::daemons::state::StableDirectory::atomic_previous_target_name(
            &previous_name,
            ".nib-subagent-preparation-",
        )
        .ok_or_else(|| "invalid preparation previous artifact name".to_string())?;
        let target = directory.path().join(&target_name);
        let previous = directory.path().join(&previous_name);
        let previous_file = directory.open_read_write(&previous)?;
        let previous_bytes = read_spawn_preparation_bytes(&previous_file, &previous)?;
        let previous_intent: SpawnPreparationIntentData = serde_json::from_slice(&previous_bytes)
            .map_err(|error| {
            format!(
                "invalid prior preparation revision was preserved {}: {error}",
                previous.display()
            )
        })?;
        validate_spawn_preparation_intent_structure(&previous_intent).map_err(|error| {
            format!(
                "invalid prior preparation revision was preserved {}: {error}",
                previous.display()
            )
        })?;
        if target_name != std::ffi::OsStr::new(&format!("{}.json", previous_intent.subagent_id)) {
            return Err(format!(
                "prior preparation revision target is inconsistent and was preserved: {}",
                previous.display()
            ));
        }
        if directory.path_exists(&target)? {
            let target_file = directory.open_read_write(&target)?;
            let target_bytes = read_spawn_preparation_bytes(&target_file, &target)?;
            let target_intent: SpawnPreparationIntentData = serde_json::from_slice(&target_bytes)
                .map_err(|error| {
                format!(
                    "invalid published preparation revision was preserved {}: {error}",
                    target.display()
                )
            })?;
            validate_spawn_preparation_intent_structure(&target_intent).map_err(|error| {
                format!(
                    "invalid published preparation revision was preserved {}: {error}",
                    target.display()
                )
            })?;
            validate_spawn_preparation_revision_successor(&previous_intent, &target_intent)?;
            directory.remove_visible_file_if_matches_direct_with_guard(
                &previous,
                &previous_file,
                || {
                    ensure_subagent_reconciliation_deadline(Some(deadline))?;
                    records.verify_visible()
                },
            )?;
        } else {
            directory.restore_exact_previous_artifact_with_guard(
                &target,
                &previous,
                &previous_bytes,
                || {
                    ensure_subagent_reconciliation_deadline(Some(deadline))?;
                    records.verify_visible()
                },
            )?;
        }
        records.verify_visible()?;
    }
    Ok(())
}

fn validate_spawn_preparation_temporary_artifacts(
    directory: &crate::daemons::state::StableDirectory,
    records: &crate::daemons::state::StableDirectory,
    deadline: Instant,
) -> Result<(), String> {
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    records.verify_visible()?;
    let mut temporary_names = Vec::new();
    directory.for_each_entry_bounded(
        MAX_SUBAGENT_RECORDS,
        MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
        |name| {
            let bytes = name.as_encoded_bytes();
            if bytes.starts_with(b".nib-subagent-preparation-") && bytes.ends_with(b".tmp") {
                temporary_names.push(name);
            }
            Ok(())
        },
    )?;
    for name in temporary_names {
        ensure_subagent_reconciliation_deadline(Some(deadline))?;
        records.verify_visible()?;
        let path = directory.path().join(&name);
        let file = directory.open_read_write(&path)?;
        let bytes = read_spawn_preparation_bytes(&file, &path)?;
        // A writer can die during its bounded temporary write. Such incomplete
        // bytes retain the pre-existing stale-temporary recovery contract. A
        // complete intent, however, carries authority and must pass the same
        // version/field matrix before recovery may remove or adopt it.
        let Ok(intent) = serde_json::from_slice::<SpawnPreparationIntentData>(&bytes) else {
            continue;
        };
        validate_spawn_preparation_intent_structure(&intent).map_err(|error| {
            format!(
                "invalid temporary preparation revision was preserved {}: {error}",
                path.display()
            )
        })?;
        let target = directory
            .path()
            .join(format!("{}.json", intent.subagent_id));
        let expected =
            directory.deterministic_artifact_path(&target, ".nib-subagent-preparation-", ".tmp")?;
        if path != expected {
            return Err(format!(
                "temporary preparation revision target is inconsistent and was preserved: {}",
                path.display()
            ));
        }
    }
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    records.verify_visible()
}

fn read_spawn_preparation_bytes(file: &File, path: &Path) -> Result<Vec<u8>, String> {
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_SPAWN_PREPARATION_BYTES {
        return Err(format!(
            "subagent preparation transaction artifact is unsafe and was preserved: {}",
            path.display()
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SPAWN_PREPARATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

fn validate_spawn_preparation_revision_successor(
    previous: &SpawnPreparationIntentData,
    current: &SpawnPreparationIntentData,
) -> Result<(), String> {
    validate_spawn_preparation_intent_structure(previous)?;
    validate_spawn_preparation_intent_structure(current)?;
    let expected_phase = match previous.phase {
        SpawnPreparationPhase::Planned => SpawnPreparationPhase::ResourcesPrepared,
        SpawnPreparationPhase::ResourcesPrepared if previous.audit_namespace_plan.is_none() => {
            SpawnPreparationPhase::AuditPublished
        }
        SpawnPreparationPhase::ResourcesPrepared => SpawnPreparationPhase::AuditPlanned,
        SpawnPreparationPhase::AuditPlanned => SpawnPreparationPhase::AuditPublished,
        SpawnPreparationPhase::AuditPublished => SpawnPreparationPhase::RecordPublished,
        SpawnPreparationPhase::RecordPublished => SpawnPreparationPhase::ManagerRegistered,
        SpawnPreparationPhase::ManagerRegistered => SpawnPreparationPhase::HandoffProven,
        SpawnPreparationPhase::HandoffProven => {
            return Err(
                "terminal preparation revision has an unexpected prior artifact".to_string(),
            );
        }
    };
    let immutable_matches = previous.version == current.version
        && previous.subagent_id == current.subagent_id
        && previous.owner == current.owner
        && previous.worktree == current.worktree
        && previous.audit_session_id == current.audit_session_id
        && previous.audit_sessions_dir == current.audit_sessions_dir
        && previous.audit_namespace_plan == current.audit_namespace_plan
        && previous.process_scope_plan == current.process_scope_plan
        && previous.created_at == current.created_at;
    let handoff_transition_matches = match current.version {
        LEGACY_SPAWN_PREPARATION_VERSION => {
            previous.handoff_process_scope.is_none() && current.handoff_process_scope.is_none()
        }
        HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION | SPAWN_PREPARATION_VERSION => {
            if current.phase == SpawnPreparationPhase::HandoffProven {
                previous.phase == SpawnPreparationPhase::ManagerRegistered
                    && previous.handoff_process_scope.is_none()
                    && current.handoff_process_scope.is_some()
            } else {
                previous.handoff_process_scope == current.handoff_process_scope
            }
        }
        _ => false,
    };
    let receipt_transition_matches = match (previous.phase, current.phase) {
        (SpawnPreparationPhase::Planned, SpawnPreparationPhase::ResourcesPrepared) => {
            previous.audit_receipt.is_none() && current.audit_receipt.is_none()
        }
        (SpawnPreparationPhase::ResourcesPrepared, SpawnPreparationPhase::AuditPlanned) => {
            previous.audit_receipt.is_none() && current.audit_receipt.is_some()
        }
        (SpawnPreparationPhase::AuditPlanned, SpawnPreparationPhase::AuditPublished) => {
            match (&previous.audit_receipt, &current.audit_receipt) {
                (Some(previous), Some(current)) => current.is_exact_publication_successor(previous),
                _ => false,
            }
        }
        (SpawnPreparationPhase::ResourcesPrepared, SpawnPreparationPhase::AuditPublished) => {
            previous.audit_namespace_plan.is_none()
                && previous.audit_receipt.is_none()
                && current.audit_receipt.is_none()
                && previous.audit_target == current.audit_target
        }
        (SpawnPreparationPhase::AuditPublished, SpawnPreparationPhase::RecordPublished)
        | (SpawnPreparationPhase::RecordPublished, SpawnPreparationPhase::ManagerRegistered)
        | (SpawnPreparationPhase::ManagerRegistered, SpawnPreparationPhase::HandoffProven) => {
            previous.audit_receipt == current.audit_receipt
                && previous.audit_target == current.audit_target
        }
        _ => false,
    };
    if !immutable_matches
        || current.revision != previous.revision.saturating_add(1)
        || current.phase != expected_phase
        || !receipt_transition_matches
        || !handoff_transition_matches
        || (previous.audit_target.is_some() && previous.audit_target != current.audit_target)
    {
        return Err(
            "published and prior preparation revisions are ambiguous; both were preserved"
                .to_string(),
        );
    }
    Ok(())
}

fn remove_spawn_preparation_entry(
    directory: &crate::daemons::state::StableDirectory,
    canonical_path: &Path,
    actual_path: &Path,
    file: &File,
    is_delete_quarantine: bool,
    records: &crate::daemons::state::StableDirectory,
    deadline: Instant,
) -> Result<(), String> {
    if !is_delete_quarantine {
        return remove_spawn_preparation_intent(directory, canonical_path, file, records, deadline);
    }
    directory.remove_visible_file_if_matches_direct_with_guard(actual_path, file, || {
        ensure_subagent_reconciliation_deadline(Some(deadline))?;
        records.verify_visible()
    })
}

fn validate_spawn_intent_record_identity(
    intent: &SpawnPreparationIntentData,
    record: &SubagentRecord,
) -> Result<(), String> {
    let result = process_scope_retirement_result(record)
        .ok_or_else(|| "superseding subagent record lacks internal authority".to_string())?;
    let receipt_id = result
        .get(WORKTREE_PREPARATION_RECEIPT_KEY)
        .and_then(Value::as_str);
    let audit_target = subagent_audit_target(record)?
        .ok_or_else(|| "superseding subagent record lacks its audit target".to_string())?;
    let status_is_valid = matches!(
        record.status.as_str(),
        "running"
            | "completed"
            | "failed"
            | "cancelled"
            | "verification_failed"
            | MERGE_FAILED_STATUS
            | MERGE_PENDING_STATUS
            | "merged"
    );
    if !status_is_valid
        || spawn_preparation_phase_revision(intent) != Some(intent.revision)
        || record.id != intent.subagent_id
        || record.child_session_id != intent.subagent_id
        || record.execution_generation != Some(intent.owner.execution_generation)
        || record.owner_lease.as_deref() != Some(intent.owner.lease_id.as_str())
        || record
            .parent_session_id
            .as_deref()
            .unwrap_or(&record.child_session_id)
            != intent.audit_session_id
        || audit_target.sessions_dir != intent.audit_sessions_dir
        || intent
            .audit_target
            .as_ref()
            .is_some_and(|expected| expected != &audit_target)
        || intent.audit_receipt.as_ref().is_some_and(|receipt| {
            audit_target.directory_identity != receipt.audit_directory_identity()
        })
        || !crate::sandbox::worktree::Worktree::preparation_authority_matches(
            &intent.worktree,
            &record.id,
            &record.worktree_path,
            &record.branch,
            record.branch_oid.as_deref(),
            receipt_id,
        )
    {
        return Err(format!(
            "subagent record does not exactly match preparation intent {}; intent and resources were preserved",
            intent.subagent_id
        ));
    }
    Ok(())
}

fn remove_spawn_preparation_intent(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    file: &File,
    records: &crate::daemons::state::StableDirectory,
    deadline: Instant,
) -> Result<(), String> {
    directory.remove_file_if_matches_with_guard(
        path,
        file,
        ".nib-subagent-preparation-delete-",
        || {
            ensure_subagent_reconciliation_deadline(Some(deadline))?;
            records.verify_visible()
        },
    )
}

fn validate_subagent_audit_argument_pair(args: &Value) -> Result<Option<String>, String> {
    let parent = args.get("_parent_session_id");
    let target = args.get("_audit_sessions_dir");
    if parent.is_some() != target.is_some() {
        return Err(
            "internal subagent parent session and audit destination must be supplied together"
                .to_string(),
        );
    }
    parent
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| "invalid internal subagent parent session id".to_string())
        })
        .transpose()
}

fn preflight_subagent_audit_target(
    args: &Value,
    project_root: &Path,
) -> Result<SubagentAuditPreparationPlan, String> {
    let deadline = Instant::now()
        .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
        .ok_or_else(|| "subagent audit preflight deadline overflow".to_string())?;
    match args.get("_audit_sessions_dir") {
        Some(encoded) => {
            let mut runtime_config = crate::config::load_nib_config_full_preflight_read_only_until(
                project_root,
                deadline,
            )
            .map_err(|error| error.to_string())?;
            let (selected_profile_id, selected_sessions_dir) =
                crate::profile::ProfileRegistry::resolve_profile_sessions_without_migration_until(
                    project_root,
                    &runtime_config.profiles,
                    deadline,
                )
                .map_err(|error| error.to_string())?;
            let expected: SubagentAuditTarget = serde_json::from_value(encoded.clone())
                .map_err(|error| format!("invalid internal subagent audit target: {error}"))?;
            let expected_sessions_dir = crate::fs_security::absolute_path(&expected.sessions_dir)
                .map_err(|error| error.to_string())?;
            let selected_sessions_dir = crate::fs_security::absolute_path(&selected_sessions_dir)
                .map_err(|error| error.to_string())?;
            if expected_sessions_dir != selected_sessions_dir {
                return Err(
                    "provided subagent audit destination does not match the workspace-selected profile"
                        .to_string(),
                );
            }
            runtime_config.profiles.default = selected_profile_id;
            let encoded = serialize_exact_subagent_audit_destination(&expected)?;
            let store = crate::session::SessionStore::at_existing_dir_with_identity_until(
                &expected.sessions_dir,
                expected.directory_identity,
                deadline,
            )?;
            let observed = subagent_audit_target_for_store(&store)?;
            if observed != expected {
                return Err(
                    "provided subagent audit destination changed during validation".to_string(),
                );
            }
            Ok(SubagentAuditPreparationPlan::Provided {
                store,
                target: observed,
                encoded,
                runtime_config,
            })
        }
        None => {
            let preflight = crate::session::SessionStore::preflight_project_sessions_dir_until(
                project_root,
                deadline,
            )?;
            // PathBuf serialization is the only fallible encoding boundary on
            // Unix. It must run before profile migration, state-directory
            // creation, session recovery, or delegation-owned mutation.
            serialize_exact_subagent_audit_destination(preflight.sessions_dir())?;
            run_after_subagent_audit_preflight_hook();
            Ok(SubagentAuditPreparationPlan::Fallback(preflight))
        }
    }
}

fn commit_subagent_audit_target(
    plan: SubagentAuditPreparationPlan,
    session_id: &str,
    worktree: Option<&crate::sandbox::worktree::Worktree>,
    mut preparation_intent: Option<&mut SpawnPreparationIntent>,
) -> Result<PreparedSubagentAudit, String> {
    let cleanup_authority = preparation_intent
        .as_deref()
        .map(|intent| intent.authority.clone());
    let deadline = match cleanup_authority.as_deref() {
        Some(authority) => authority.operation_deadline(),
        None => Instant::now()
            .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
            .ok_or_else(|| "subagent audit preparation deadline overflow".to_string())?,
    };
    let verify_authority = || match cleanup_authority.as_deref() {
        Some(authority) => authority.verify_until(deadline),
        None => ensure_subagent_reconciliation_deadline(Some(deadline)),
    };
    verify_authority()?;
    match plan {
        SubagentAuditPreparationPlan::Provided {
            store,
            target,
            encoded,
            runtime_config: _,
        } => {
            verify_authority()?;
            if subagent_audit_target_for_store(&store)? != target {
                return Err("provided subagent audit destination changed before commit".to_string());
            }
            verify_authority()?;
            Ok(PreparedSubagentAudit {
                encoded,
                fallback: None,
            })
        }
        SubagentAuditPreparationPlan::Fallback(preflight) => {
            let durable_plan = preparation_intent
                .as_deref()
                .and_then(|intent| intent.data.audit_namespace_plan.clone());
            let mut preparation = match worktree {
                Some(worktree) => preflight.open_until_after_owned_worktree_with_guard(
                    deadline,
                    worktree,
                    durable_plan.as_ref(),
                    &verify_authority,
                )?,
                None => preflight.open_until_with_guard(deadline, &verify_authority)?,
            };
            if let Err(error) = preparation.plan_unpublished_session(session_id) {
                let cleanup = cleanup_session_preparation(
                    preparation,
                    deadline,
                    cleanup_authority.as_deref(),
                )
                .err();
                return Err(match cleanup {
                    Some(cleanup) => {
                        format!("{error}; audit preparation cleanup failed: {cleanup}")
                    }
                    None => error,
                });
            }
            if let Some(intent) = preparation_intent.as_deref_mut() {
                let receipt = match preparation.durable_receipt(session_id) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        let cleanup = cleanup_session_preparation(
                            preparation,
                            deadline,
                            cleanup_authority.as_deref(),
                        )
                        .err();
                        return Err(match cleanup {
                            Some(cleanup) => {
                                format!("{error}; audit preparation cleanup failed: {cleanup}")
                            }
                            None => error,
                        });
                    }
                };
                if let Err(error) = intent.revise(
                    SpawnPreparationPhase::AuditPlanned,
                    Some(receipt),
                    None,
                    None,
                ) {
                    let cleanup = cleanup_session_preparation(
                        preparation,
                        deadline,
                        cleanup_authority.as_deref(),
                    )
                    .err();
                    return Err(match cleanup {
                        Some(cleanup) => {
                            format!("{error}; audit preparation cleanup failed: {cleanup}")
                        }
                        None => error,
                    });
                }
            }
            let session_publication = {
                #[cfg(test)]
                {
                    if consume_spawn_failure(&SPAWN_SESSION_PUBLICATION_FAILURES) {
                        Err("injected subagent session publication failure".to_string())
                    } else {
                        preparation
                            .create_unpublished_session_with_guard(session_id, &verify_authority)
                    }
                }
                #[cfg(not(test))]
                {
                    preparation.create_unpublished_session_with_guard(session_id, &verify_authority)
                }
            };
            if let Err(error) = session_publication {
                let cleanup = cleanup_session_preparation(
                    preparation,
                    deadline,
                    cleanup_authority.as_deref(),
                )
                .err();
                return Err(match cleanup {
                    Some(cleanup) => {
                        format!("{error}; audit preparation cleanup failed: {cleanup}")
                    }
                    None => error,
                });
            }
            let target = subagent_audit_target_for_store(preparation.store())?;
            verify_authority()?;
            if let Some(intent) = preparation_intent {
                let receipt = match preparation.durable_receipt(session_id) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        let cleanup = cleanup_session_preparation(
                            preparation,
                            deadline,
                            cleanup_authority.as_deref(),
                        )
                        .err();
                        return Err(match cleanup {
                            Some(cleanup) => {
                                format!("{error}; audit preparation cleanup failed: {cleanup}")
                            }
                            None => error,
                        });
                    }
                };
                if let Err(error) = intent.revise(
                    SpawnPreparationPhase::AuditPublished,
                    Some(receipt),
                    Some(target.clone()),
                    None,
                ) {
                    let cleanup = cleanup_session_preparation(
                        preparation,
                        deadline,
                        cleanup_authority.as_deref(),
                    )
                    .err();
                    return Err(match cleanup {
                        Some(cleanup) => {
                            format!("{error}; audit preparation cleanup failed: {cleanup}")
                        }
                        None => error,
                    });
                }
            }
            let encoded = serialize_exact_subagent_audit_destination(&target)?;
            verify_authority()?;
            Ok(PreparedSubagentAudit {
                encoded,
                fallback: Some(preparation),
            })
        }
    }
}

fn cleanup_session_preparation(
    preparation: crate::session::SessionStorePreparation,
    deadline: Instant,
    authority: Option<&SpawnPreparationAuthority>,
) -> Result<(), String> {
    #[cfg(test)]
    if consume_spawn_failure(&SPAWN_SESSION_CLEANUP_FAILURES) {
        if authority.is_some() {
            preparation.preserve_for_durable_reconciliation();
        } else {
            drop(preparation);
        }
        return Err("injected subagent session cleanup failure".to_string());
    }
    match authority {
        Some(authority) => preparation
            .cleanup_with_guard_preserving_failure(deadline, || authority.verify_until(deadline)),
        None => preparation.cleanup(deadline),
    }
}

#[cfg(test)]
fn prepare_subagent_audit_target(
    args: &Value,
    project_root: &Path,
    session_id: &str,
) -> Result<Value, String> {
    validate_subagent_audit_argument_pair(args)?;
    let plan = preflight_subagent_audit_target(args, project_root)?;
    let preparation = commit_subagent_audit_target(plan, session_id, None, None)?;
    let encoded = preparation.encoded.clone();
    preparation.disarm();
    Ok(encoded)
}

fn subagent_audit_target_for_store(
    store: &crate::session::SessionStore,
) -> Result<SubagentAuditTarget, String> {
    let sessions_dir = store
        .sessions_dir()
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent audit directory: {error}"))?;
    let directory_identity = store
        .persistent_directory_identity()
        .map_err(|error| error.to_string())?;
    Ok(SubagentAuditTarget {
        sessions_dir,
        directory_identity,
    })
}

fn serialize_exact_subagent_audit_destination<T: Serialize + ?Sized>(
    destination: &T,
) -> Result<Value, String> {
    serde_json::to_value(destination)
        .map_err(|_| SUBAGENT_AUDIT_DESTINATION_ENCODING_ERROR.to_string())
}

pub(crate) fn serialize_subagent_audit_destination(
    store: &crate::session::SessionStore,
) -> Result<Value, String> {
    serialize_exact_subagent_audit_destination(&subagent_audit_target_for_store(store)?)
}

pub fn spawn_subagent(args: &Value, project_root: &Path) -> Result<Value, String> {
    if std::env::var_os("NIB_MANAGED_PROCESS_SCOPE").is_some() {
        return Err(
            "nested subagents are not supported inside a foreground managed-process scope"
                .to_string(),
        );
    }
    tokio::runtime::Handle::try_current()
        .map_err(|_| "spawn_subagent requires an active Tokio runtime".to_string())?;
    let project_root = canonical_project_root(project_root)?;
    #[cfg(not(test))]
    crate::sandbox::process::ProcessScopeBackend::production()?;
    let subagent_id = format!("sub-{}", uuid::Uuid::new_v4());
    let prompt = args
        .get("prompt")
        .and_then(|value| value.as_str())
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or("missing prompt")?
        .to_string();
    let max_steps = args
        .get("max_steps")
        .and_then(|value| value.as_u64())
        .map(|value| value.clamp(1, 100) as u32)
        .unwrap_or(0);
    let parent_session_id = validate_subagent_audit_argument_pair(args)?;
    let audit_plan = preflight_subagent_audit_target(args, &project_root)?;
    audit_plan.verify_continuity()?;
    let runtime_config = audit_plan.runtime_config().clone();
    let worktree_plan = crate::sandbox::worktree::Worktree::plan_preparation_authority(
        &project_root,
        &subagent_id,
    )?;
    let records = ensure_records_directory_capability_until(&project_root, None)?;
    reconcile_spawn_preparations(&project_root, &records)?;
    let audit_namespace_plan =
        audit_plan.fallback_namespace_plan_after_records(&subagent_id, &records)?;
    let owner_plan = SubagentOwnerLease::plan();
    let audit_session_id = parent_session_id.as_deref().unwrap_or(&subagent_id);
    let (audit_sessions_dir, initial_audit_target) = audit_plan.durable_audit_destination();
    let mut preparation_intent = Some(SpawnPreparationIntent::create(
        &records,
        &subagent_id,
        owner_plan.clone(),
        worktree_plan.clone(),
        audit_session_id,
        audit_sessions_dir,
        audit_namespace_plan.clone(),
        initial_audit_target,
    )?);
    let preparation_authority = preparation_intent
        .as_ref()
        .ok_or_else(|| "spawn preparation authority was not retained".to_string())?
        .authority
        .clone();
    pause_after_spawn_preparation_intent(&subagent_id)?;

    run_spawn_forward_mutation_hook("worktree");
    let worktree_guard = {
        let authority = preparation_authority.clone();
        let deadline = authority.operation_deadline();
        std::sync::Arc::new(move || authority.verify_until(deadline))
            as std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>
    };
    let worktree =
        match crate::sandbox::worktree::Worktree::create_from_preparation_authority_with_guard(
            &project_root,
            &worktree_plan,
            worktree_guard,
        ) {
            Ok(worktree) => worktree,
            Err(error) => {
                // The constructor may already have published a durable
                // reservation, branch, path, or Git registration. Its exact
                // extent is authoritative only in the retained intent, so a
                // constructor error never retires that intent in-process.
                drop(preparation_intent);
                return Err(error);
            }
        };
    run_spawn_forward_mutation_hook("child_config");
    if let Err(error) = prepare_child_runtime_config_with_authority(
        &runtime_config,
        &worktree.path,
        &preparation_authority,
    ) {
        let worktree_cleanup = cleanup_precommit_worktree_sync_with_authority(
            &project_root,
            &worktree,
            &preparation_authority,
        );
        let cleanup = worktree_cleanup
            .as_ref()
            .err()
            .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
            .unwrap_or_default();
        let intent_cleanup = if worktree_cleanup.is_ok() {
            preparation_intent
                .take()
                .map(SpawnPreparationIntent::cleanup)
                .transpose()
                .err()
                .map(|value| format!("; preparation intent cleanup failed: {value}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        return Err(format!(
            "failed to prepare subagent runtime: {error}{cleanup}{intent_cleanup}"
        ));
    }
    run_spawn_forward_mutation_hook("owner");
    let owner_lease =
        match create_spawn_owner_lease(&project_root, &owner_plan, &preparation_authority) {
            Ok(owner) => owner,
            Err(error) => {
                let worktree_cleanup = cleanup_precommit_worktree_sync_with_authority(
                    &project_root,
                    &worktree,
                    &preparation_authority,
                );
                let cleanup = worktree_cleanup
                    .as_ref()
                    .err()
                    .map(|value| format!("; worktree cleanup failed: {value}"))
                    .unwrap_or_default();
                let intent_cleanup = if !error.mutation_indeterminate && worktree_cleanup.is_ok() {
                    preparation_intent
                        .take()
                        .map(SpawnPreparationIntent::cleanup)
                        .transpose()
                        .err()
                        .map(|value| format!("; preparation intent cleanup failed: {value}"))
                        .unwrap_or_default()
                } else {
                    // Owner creation can fail after publishing either half of its
                    // exact pair. Preserve the durable intent until restart can
                    // classify and remove the planned owner.
                    drop(preparation_intent.take());
                    String::new()
                };
                return Err(format!(
                    "failed to establish subagent execution ownership: {}{cleanup}{intent_cleanup}",
                    error.message
                ));
            }
        };
    if let Some(intent) = preparation_intent.as_mut() {
        if let Err(error) =
            intent.revise(SpawnPreparationPhase::ResourcesPrepared, None, None, None)
        {
            let worktree_cleanup = cleanup_precommit_worktree_sync_with_authority(
                &project_root,
                &worktree,
                &preparation_authority,
            )
            .err()
            .map(|value| format!("; worktree cleanup failed: {value}"))
            .unwrap_or_default();
            let owner_cleanup = compensate_owner_lease_with_authority(
                owner_lease,
                OwnerLeaseCompensation::Remove,
                &preparation_authority,
            )
            .err()
            .map(|value| format!("; owner cleanup failed: {value}"))
            .unwrap_or_default();
            return Err(format!("{error}{worktree_cleanup}{owner_cleanup}"));
        }
    }
    run_spawn_forward_mutation_hook("audit");
    let prepared_audit = match commit_subagent_audit_target(
        audit_plan,
        audit_session_id,
        Some(&worktree),
        preparation_intent.as_mut(),
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            let mut cleanup = Vec::new();
            if let Err(cleanup_error) = cleanup_precommit_worktree_sync_with_authority(
                &project_root,
                &worktree,
                &preparation_authority,
            ) {
                cleanup.push(format!("worktree cleanup failed: {cleanup_error}"));
            }
            if let Err(cleanup_error) = compensate_owner_lease_with_authority(
                owner_lease,
                OwnerLeaseCompensation::Remove,
                &preparation_authority,
            ) {
                cleanup.push(format!("owner lease cleanup failed: {cleanup_error}"));
            }
            if let Some(intent) = preparation_intent {
                // Audit initialization reports a single error that can include
                // an indeterminate session cleanup. Preserve the intent for
                // bounded restart reconciliation instead of guessing that the
                // audit namespace is clean.
                drop(intent);
            }
            return if cleanup.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", cleanup.join("; ")))
            };
        }
    };
    if let Some(intent) = preparation_intent.as_mut() {
        if intent.data.phase == SpawnPreparationPhase::ResourcesPrepared {
            if let Err(error) =
                intent.revise(SpawnPreparationPhase::AuditPublished, None, None, None)
            {
                let cleanup_errors = collect_spawn_compensation_sync_with_audit(
                    || Ok(()),
                    || {
                        cleanup_precommit_worktree_sync_with_authority(
                            &project_root,
                            &worktree,
                            &preparation_authority,
                        )
                    },
                    |action| {
                        compensate_owner_lease_with_authority(
                            owner_lease,
                            action,
                            &preparation_authority,
                        )
                    },
                    prepared_audit,
                    &preparation_authority,
                );
                // The intent revision itself is indeterminate.  Its durable
                // transaction remains the restart authority even when all
                // exact external cleanup happened to finish.
                drop(preparation_intent);
                return if cleanup_errors.is_empty() {
                    Err(error)
                } else {
                    Err(format!("{error}; {}", cleanup_errors.join("; ")))
                };
            }
        }
    }
    if let Err(error) = pause_after_subagent_audit_preparation(&subagent_id) {
        let mut cleanup_errors = collect_spawn_compensation_sync_with_audit(
            || Ok(()),
            || {
                cleanup_precommit_worktree_sync_with_authority(
                    &project_root,
                    &worktree,
                    &preparation_authority,
                )
            },
            |action| {
                compensate_owner_lease_with_authority(owner_lease, action, &preparation_authority)
            },
            prepared_audit,
            &preparation_authority,
        );
        if cleanup_errors.is_empty() {
            if let Some(intent) = preparation_intent {
                if let Err(cleanup) = intent.cleanup() {
                    cleanup_errors.push(format!("preparation intent cleanup failed: {cleanup}"));
                }
            }
        } else {
            drop(preparation_intent);
        }
        return if cleanup_errors.is_empty() {
            Err(error)
        } else {
            Err(format!("{error}; {}", cleanup_errors.join("; ")))
        };
    }
    let audit_target = prepared_audit.encoded.clone();
    let handoff_evidence = preparation_intent
        .as_ref()
        .map(|intent| spawn_handoff_evidence(&intent.data, "pending"))
        .ok_or("spawn preparation authority was lost before record construction")?;
    let mut record = SubagentRecord {
        id: subagent_id.clone(),
        parent_session_id: parent_session_id.clone(),
        child_session_id: subagent_id.clone(),
        prompt: prompt.clone(),
        status: "running".to_string(),
        execution_generation: Some(owner_lease.execution_generation),
        owner_lease: Some(owner_lease.lease_id.clone()),
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        branch_oid: Some(worktree.branch_oid.clone()),
        result: Some(json!({
            "_ownership_audit_target": audit_target,
            "_worktree_ownership_receipt": worktree.preparation_authority().ownership_receipt_id,
            "_spawn_handoff": handoff_evidence,
        })),
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let publication_result = match preparation_intent.as_ref() {
        Some(intent) => {
            write_spawn_subagent_record_locked(&project_root, &record, &intent.authority)
        }
        None => Err(InitialSubagentRecordPublicationError {
            message: "spawn preparation authority was lost before record publication".to_string(),
            receipt: None,
            publication_attempted: false,
        }),
    };
    let mut publication = match publication_result {
        Ok(publication) => publication,
        Err(error) => {
            let message = error.message.clone();
            let compensation_errors = collect_spawn_compensation_sync_with_audit(
                || {
                    cleanup_record_after_publication_failure_locked(
                        &project_root,
                        &record,
                        &error,
                        &preparation_authority,
                    )
                },
                || {
                    cleanup_precommit_worktree_sync_with_authority(
                        &project_root,
                        &worktree,
                        &preparation_authority,
                    )
                },
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            );
            let mut compensation_errors = compensation_errors;
            if compensation_errors.is_empty() {
                if let Some(intent) = preparation_intent {
                    if let Err(cleanup) = intent.cleanup() {
                        compensation_errors
                            .push(format!("preparation intent compensation failed: {cleanup}"));
                    }
                }
            }
            return if compensation_errors.is_empty() {
                Err(message)
            } else {
                Err(format!("{message}; {}", compensation_errors.join("; ")))
            };
        }
    };
    run_spawn_handoff_phase_hook("record_published");
    if let Some(intent) = preparation_intent.as_mut() {
        if let Err(error) = intent.revise(SpawnPreparationPhase::RecordPublished, None, None, None)
        {
            let compensation_errors = collect_spawn_compensation_sync_with_audit(
                || {
                    cleanup_record_after_registration_failure_locked(
                        &project_root,
                        &record,
                        &publication,
                        &preparation_authority,
                    )
                },
                || {
                    cleanup_precommit_worktree_sync_with_authority(
                        &project_root,
                        &worktree,
                        &preparation_authority,
                    )
                },
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            );
            drop(preparation_intent);
            return if compensation_errors.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", compensation_errors.join("; ")))
            };
        }
    }
    let start_gate = match crate::daemons::task::TASK_MANAGER
        .register_paused_task(subagent_id.clone(), "subagent")
    {
        Ok(start_gate) => start_gate,
        Err(error) => {
            let compensation_errors = collect_spawn_compensation_sync_with_audit(
                || {
                    cleanup_record_after_registration_failure_locked(
                        &project_root,
                        &record,
                        &publication,
                        &preparation_authority,
                    )
                },
                || {
                    cleanup_precommit_worktree_sync_with_authority(
                        &project_root,
                        &worktree,
                        &preparation_authority,
                    )
                },
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            );
            let mut compensation_errors = compensation_errors;
            if compensation_errors.is_empty() {
                if let Some(intent) = preparation_intent {
                    if let Err(cleanup) = intent.cleanup() {
                        compensation_errors
                            .push(format!("preparation intent compensation failed: {cleanup}"));
                    }
                }
            }
            return if compensation_errors.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", compensation_errors.join("; ")))
            };
        }
    };
    run_spawn_handoff_phase_hook("manager_registered");
    if let Some(intent) = preparation_intent.as_mut() {
        if let Err(error) =
            intent.revise(SpawnPreparationPhase::ManagerRegistered, None, None, None)
        {
            let mut compensation_errors = Vec::new();
            if let Err(rollback) =
                crate::daemons::task::TASK_MANAGER.rollback_unattached_task(&subagent_id)
            {
                compensation_errors.push(format!("task registration rollback failed: {rollback}"));
            }
            compensation_errors.extend(collect_spawn_compensation_sync_with_audit(
                || {
                    cleanup_record_after_registration_failure_locked(
                        &project_root,
                        &record,
                        &publication,
                        &preparation_authority,
                    )
                },
                || {
                    cleanup_precommit_worktree_sync_with_authority(
                        &project_root,
                        &worktree,
                        &preparation_authority,
                    )
                },
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            ));
            drop(preparation_intent);
            return if compensation_errors.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", compensation_errors.join("; ")))
            };
        }
    }
    prepared_audit.disarm();
    let launch = launch_subagent_task(
        project_root.clone(),
        subagent_id.clone(),
        prompt,
        max_steps,
        parent_session_id,
        PreparedSubagentTask {
            record: record.clone(),
            worktree,
            owner_lease,
        },
        start_gate,
        &preparation_authority,
        preparation_intent
            .as_ref()
            .and_then(|intent| intent.data.process_scope_plan.clone()),
    );
    let mut launched = match launch {
        Ok(launched) => launched,
        Err(error) => {
            let deadline = preparation_authority.operation_deadline();
            let execution_generation = record.execution_generation;
            let owner_lease = record.owner_lease.clone();
            let rollback = crate::daemons::task::TASK_MANAGER
                .rollback_unattached_task(&subagent_id)
                .err()
                .map(|value| format!("; task registration rollback failed: {value}"))
                .unwrap_or_default();
            drop(preparation_intent);
            drop(publication);
            drop(preparation_authority);
            let persistence = persist_unstarted_after_preparation_unlock(
                &project_root,
                &subagent_id,
                execution_generation,
                owner_lease.as_deref(),
                &error,
                deadline,
            )
            .err()
            .map(|value| format!("; unstarted outcome persistence failed: {value}"))
            .unwrap_or_default();
            return Err(format!("{error}{rollback}{persistence}"));
        }
    };
    let response = launched.response.clone();
    run_spawn_handoff_phase_hook("handoff_established");
    if let Err(error) = pause_after_supervisor_ready_before_commit(&subagent_id) {
        let cancellation = crate::daemons::task::TASK_MANAGER
            .cancel(&subagent_id)
            .err()
            .map(|value| format!("; gated task cancellation failed: {value}"))
            .unwrap_or_default();
        drop(preparation_intent);
        drop(publication);
        drop(preparation_authority);
        return Err(format!(
            "subagent supervisor READY barrier failed: {error}{cancellation}"
        ));
    }
    let intent = preparation_intent
        .as_mut()
        .ok_or("spawn preparation authority was lost before handoff commit")?;
    if let Err(error) = intent.revise(
        SpawnPreparationPhase::HandoffProven,
        None,
        None,
        launched.precommit_process_scope(),
    ) {
        let cancellation = crate::daemons::task::TASK_MANAGER
            .cancel(&subagent_id)
            .err()
            .map(|value| format!("; launched task cancellation failed: {value}"))
            .unwrap_or_default();
        drop(preparation_intent);
        drop(publication);
        drop(preparation_authority);
        return Err(format!(
            "failed to persist subagent handoff: {error}{cancellation}"
        ));
    }
    run_spawn_handoff_phase_hook("handoff_proven");
    if let Err(error) =
        commit_spawn_handoff_record_locked(&project_root, &mut record, &mut publication, intent)
    {
        let cancellation = crate::daemons::task::TASK_MANAGER
            .cancel(&subagent_id)
            .err()
            .map(|value| format!("; launched task cancellation failed: {value}"))
            .unwrap_or_default();
        drop(preparation_intent);
        drop(publication);
        drop(preparation_authority);
        return Err(format!(
            "failed to commit subagent handoff: {error}{cancellation}"
        ));
    }
    run_spawn_handoff_phase_hook("handoff_committed");
    if let Err(error) = launched.commit_supervisor_handoff(&preparation_authority) {
        let cancellation = crate::daemons::task::TASK_MANAGER
            .cancel(&subagent_id)
            .err()
            .map(|value| format!("; handed-off task cancellation failed: {value}"))
            .unwrap_or_default();
        drop(publication);
        drop(preparation_authority);
        return Err(format!(
            "failed to acknowledge committed subagent supervisor handoff: {error}{cancellation}"
        ));
    }
    run_spawn_handoff_phase_hook("supervisor_started");
    if let Err(error) =
        preparation_authority.verify_until(preparation_authority.operation_deadline())
    {
        let cancellation = crate::daemons::task::TASK_MANAGER
            .cancel(&subagent_id)
            .err()
            .map(|value| format!("; handed-off task cancellation failed: {value}"))
            .unwrap_or_default();
        drop(publication);
        drop(preparation_authority);
        return Err(format!(
            "subagent handoff deadline expired before start-gate release: {error}{cancellation}"
        ));
    }
    if let Err(error) = crate::daemons::task::TASK_MANAGER.start_task(&subagent_id) {
        let cancellation = crate::daemons::task::TASK_MANAGER
            .cancel(&subagent_id)
            .err()
            .map(|value| format!("; handed-off task cancellation failed: {value}"))
            .unwrap_or_default();
        drop(publication);
        drop(preparation_authority);
        return Err(format!(
            "failed to release committed subagent start gate: {error}{cancellation}"
        ));
    }
    run_spawn_handoff_phase_hook("launch_released");
    run_spawn_handoff_phase_hook("before_intent_retirement");
    let intent = preparation_intent
        .take()
        .ok_or("spawn preparation authority was lost before final retirement")?;
    if let Err(error) = intent.cleanup() {
        let cancellation = crate::daemons::task::TASK_MANAGER
            .cancel(&subagent_id)
            .err()
            .map(|value| format!("; launched task cancellation failed: {value}"))
            .unwrap_or_default();
        drop(publication);
        drop(preparation_authority);
        return Err(format!(
            "failed to retire authoritative spawn preparation after handoff: {error}{cancellation}"
        ));
    }
    run_spawn_handoff_phase_hook("intent_retired");
    drop(publication);
    drop(preparation_authority);
    Ok(response)
}

pub fn spawn_subagent_cancellable<'a>(
    args: &'a Value,
    project_root: &'a Path,
    cancellation: Option<&'a crate::agent::CancellationSignal>,
) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
    Box::pin(async move {
        if std::env::var_os("NIB_MANAGED_PROCESS_SCOPE").is_some() {
            return Err(
                "nested subagents are not supported inside a foreground managed-process scope"
                    .to_string(),
            );
        }
        tokio::runtime::Handle::try_current()
            .map_err(|_| "spawn_subagent requires an active Tokio runtime".to_string())?;
        let project_root = canonical_project_root(project_root)?;
        #[cfg(not(test))]
        crate::sandbox::process::ProcessScopeBackend::production()?;
        let subagent_id = format!("sub-{}", uuid::Uuid::new_v4());
        let prompt = args
            .get("prompt")
            .and_then(|value| value.as_str())
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or("missing prompt")?
            .to_string();
        let max_steps = args
            .get("max_steps")
            .and_then(|value| value.as_u64())
            .map(|value| value.clamp(1, 100) as u32)
            .unwrap_or(0);
        let parent_session_id = validate_subagent_audit_argument_pair(args)?;
        let audit_plan = preflight_subagent_audit_target(args, &project_root)?;
        audit_plan.verify_continuity()?;
        let runtime_config = audit_plan.runtime_config().clone();
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            return Err("subagent spawn cancelled before mutation".to_string());
        }
        let worktree_plan =
            crate::sandbox::worktree::Worktree::plan_preparation_authority_cancellable(
                &project_root,
                &subagent_id,
                cancellation,
            )
            .await?;
        let records = ensure_records_directory_capability_until(&project_root, None)?;
        reconcile_spawn_preparations(&project_root, &records)?;
        let audit_namespace_plan =
            audit_plan.fallback_namespace_plan_after_records(&subagent_id, &records)?;
        let owner_plan = SubagentOwnerLease::plan();
        let audit_session_id = parent_session_id.as_deref().unwrap_or(&subagent_id);
        let (audit_sessions_dir, initial_audit_target) = audit_plan.durable_audit_destination();
        let mut preparation_intent = Some(SpawnPreparationIntent::create(
            &records,
            &subagent_id,
            owner_plan.clone(),
            worktree_plan.clone(),
            audit_session_id,
            audit_sessions_dir,
            audit_namespace_plan.clone(),
            initial_audit_target,
        )?);
        let preparation_authority = preparation_intent
            .as_ref()
            .ok_or_else(|| "spawn preparation authority was not retained".to_string())?
            .authority
            .clone();
        pause_after_spawn_preparation_intent(&subagent_id)?;
        run_spawn_forward_mutation_hook("worktree");
        let worktree_guard = {
            let authority = preparation_authority.clone();
            let deadline = authority.operation_deadline();
            std::sync::Arc::new(move || authority.verify_until(deadline))
                as std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>
        };
        let worktree =
            match crate::sandbox::worktree::Worktree::create_cancellable_from_preparation_authority_with_guard(
                &project_root,
                &worktree_plan,
                cancellation,
                worktree_guard,
            )
            .await
            {
                Ok(worktree) => worktree,
                Err(error) => {
                    // See the synchronous path: a constructor error can own
                    // partial durable worktree state and therefore retains the
                    // preparation intent for restart reconciliation.
                    drop(preparation_intent.take());
                    return Err(error);
                }
            };
        run_spawn_forward_mutation_hook("child_config");
        if let Err(error) = prepare_child_runtime_config_with_authority(
            &runtime_config,
            &worktree.path,
            &preparation_authority,
        ) {
            let worktree_cleanup = cleanup_precommit_worktree_with_authority(
                &project_root,
                &worktree,
                preparation_authority.clone(),
            )
            .await;
            let cleanup = worktree_cleanup
                .as_ref()
                .err()
                .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
                .unwrap_or_default();
            let intent_cleanup = if worktree_cleanup.is_ok() {
                preparation_intent
                    .take()
                    .map(SpawnPreparationIntent::cleanup)
                    .transpose()
                    .err()
                    .map(|value| format!("; preparation intent cleanup failed: {value}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            return Err(format!(
                "failed to prepare subagent runtime: {error}{cleanup}{intent_cleanup}"
            ));
        }
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            cleanup_precommit_worktree_with_authority(
                &project_root,
                &worktree,
                preparation_authority.clone(),
            )
            .await?;
            if let Some(intent) = preparation_intent.take() {
                intent.cleanup()?;
            }
            return Err("subagent spawn cancelled before commit".to_string());
        }
        run_spawn_forward_mutation_hook("owner");
        let owner_lease =
            match create_spawn_owner_lease(&project_root, &owner_plan, &preparation_authority) {
                Ok(owner_lease) => owner_lease,
                Err(error) => {
                    let worktree_cleanup = cleanup_precommit_worktree_with_authority(
                        &project_root,
                        &worktree,
                        preparation_authority.clone(),
                    )
                    .await;
                    let cleanup = worktree_cleanup
                        .as_ref()
                        .err()
                        .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
                        .unwrap_or_default();
                    let intent_cleanup = if !error.mutation_indeterminate
                        && worktree_cleanup.is_ok()
                    {
                        preparation_intent
                            .take()
                            .map(SpawnPreparationIntent::cleanup)
                            .transpose()
                            .err()
                            .map(|value| format!("; preparation intent cleanup failed: {value}"))
                            .unwrap_or_default()
                    } else {
                        drop(preparation_intent.take());
                        String::new()
                    };
                    return Err(format!(
                    "failed to establish subagent execution ownership: {}{cleanup}{intent_cleanup}",
                    error.message
                ));
                }
            };
        if let Some(intent) = preparation_intent.as_mut() {
            if let Err(error) =
                intent.revise(SpawnPreparationPhase::ResourcesPrepared, None, None, None)
            {
                let worktree_cleanup = cleanup_precommit_worktree_with_authority(
                    &project_root,
                    &worktree,
                    preparation_authority.clone(),
                )
                .await
                .err()
                .map(|value| format!("; worktree cleanup failed: {value}"))
                .unwrap_or_default();
                let owner_cleanup = compensate_owner_lease_with_authority(
                    owner_lease,
                    OwnerLeaseCompensation::Remove,
                    &preparation_authority,
                )
                .err()
                .map(|value| format!("; owner cleanup failed: {value}"))
                .unwrap_or_default();
                return Err(format!("{error}{worktree_cleanup}{owner_cleanup}"));
            }
        }
        run_spawn_forward_mutation_hook("audit");
        let prepared_audit = match commit_subagent_audit_target(
            audit_plan,
            audit_session_id,
            Some(&worktree),
            preparation_intent.as_mut(),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                let cleanup = cleanup_precommit_worktree_with_authority(
                    &project_root,
                    &worktree,
                    preparation_authority.clone(),
                )
                .await
                .err()
                .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
                .unwrap_or_default();
                let owner_cleanup = compensate_owner_lease_with_authority(
                    owner_lease,
                    OwnerLeaseCompensation::Remove,
                    &preparation_authority,
                )
                .err()
                .map(|cleanup| format!("; owner lease cleanup failed: {cleanup}"))
                .unwrap_or_default();
                // Audit initialization can fail after an indeterminate session
                // cleanup. Retain the intent regardless of the other exact
                // cleanup outcomes.
                drop(preparation_intent);
                return Err(format!("{error}{cleanup}{owner_cleanup}"));
            }
        };
        if let Some(intent) = preparation_intent.as_mut() {
            if intent.data.phase == SpawnPreparationPhase::ResourcesPrepared {
                if let Err(error) =
                    intent.revise(SpawnPreparationPhase::AuditPublished, None, None, None)
                {
                    let cleanup_errors = collect_spawn_compensation_async_with_audit(
                        || Ok(()),
                        cleanup_precommit_worktree_with_authority(
                            &project_root,
                            &worktree,
                            preparation_authority.clone(),
                        ),
                        |action| {
                            compensate_owner_lease_with_authority(
                                owner_lease,
                                action,
                                &preparation_authority,
                            )
                        },
                        prepared_audit,
                        &preparation_authority,
                    )
                    .await;
                    // The failed revision can have recoverable publication
                    // artifacts, so durable restart authority must remain.
                    drop(preparation_intent);
                    return if cleanup_errors.is_empty() {
                        Err(error)
                    } else {
                        Err(format!("{error}; {}", cleanup_errors.join("; ")))
                    };
                }
            }
        }
        if let Err(error) = pause_after_subagent_audit_preparation(&subagent_id) {
            let mut cleanup_errors = collect_spawn_compensation_async_with_audit(
                || Ok(()),
                cleanup_precommit_worktree_with_authority(
                    &project_root,
                    &worktree,
                    preparation_authority.clone(),
                ),
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            )
            .await;
            if cleanup_errors.is_empty() {
                if let Some(intent) = preparation_intent {
                    if let Err(cleanup) = intent.cleanup() {
                        cleanup_errors
                            .push(format!("preparation intent cleanup failed: {cleanup}"));
                    }
                }
            } else {
                drop(preparation_intent);
            }
            return if cleanup_errors.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", cleanup_errors.join("; ")))
            };
        }
        let audit_target = prepared_audit.encoded.clone();
        #[cfg(test)]
        if consume_spawn_failure(&SPAWN_POST_AUDIT_CANCELLATIONS) {
            if let Some(cancellation) = cancellation {
                cancellation.cancel();
            }
        }
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            let mut cleanup_errors = Vec::new();
            if let Err(cleanup) = prepared_audit.cleanup_with_authority(&preparation_authority) {
                cleanup_errors.push(format!("audit cleanup failed: {cleanup}"));
            }
            if let Err(cleanup) = cleanup_precommit_worktree_with_authority(
                &project_root,
                &worktree,
                preparation_authority.clone(),
            )
            .await
            {
                cleanup_errors.push(format!("worktree cleanup failed: {cleanup}"));
            }
            if let Err(cleanup) = compensate_owner_lease_with_authority(
                owner_lease,
                OwnerLeaseCompensation::Remove,
                &preparation_authority,
            ) {
                cleanup_errors.push(format!("owner lease cleanup failed: {cleanup}"));
            }
            if cleanup_errors.is_empty() {
                if let Some(intent) = preparation_intent {
                    if let Err(cleanup) = intent.cleanup() {
                        cleanup_errors
                            .push(format!("preparation intent cleanup failed: {cleanup}"));
                    }
                }
            } else {
                drop(preparation_intent);
            }
            return if cleanup_errors.is_empty() {
                Err("subagent spawn cancelled before commit".to_string())
            } else {
                Err(format!(
                    "subagent spawn cancelled before commit; {}",
                    cleanup_errors.join("; ")
                ))
            };
        }
        let handoff_evidence = preparation_intent
            .as_ref()
            .map(|intent| spawn_handoff_evidence(&intent.data, "pending"))
            .ok_or("spawn preparation authority was lost before record construction")?;
        let mut record = SubagentRecord {
            id: subagent_id.clone(),
            parent_session_id: parent_session_id.clone(),
            child_session_id: subagent_id.clone(),
            prompt: prompt.clone(),
            status: "running".to_string(),
            execution_generation: Some(owner_lease.execution_generation),
            owner_lease: Some(owner_lease.lease_id.clone()),
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: Some(worktree.branch_oid.clone()),
            result: Some(json!({
                "_ownership_audit_target": audit_target,
                "_worktree_ownership_receipt": worktree.preparation_authority().ownership_receipt_id,
                "_spawn_handoff": handoff_evidence,
            })),
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let publication_result = match preparation_intent.as_ref() {
            Some(intent) => {
                write_spawn_subagent_record_locked(&project_root, &record, &intent.authority)
            }
            None => Err(InitialSubagentRecordPublicationError {
                message: "spawn preparation authority was lost before record publication"
                    .to_string(),
                receipt: None,
                publication_attempted: false,
            }),
        };
        let mut publication = match publication_result {
            Ok(publication) => publication,
            Err(error) => {
                let message = error.message.clone();
                let compensation_errors = collect_spawn_compensation_async_with_audit(
                    || {
                        cleanup_record_after_publication_failure_locked(
                            &project_root,
                            &record,
                            &error,
                            &preparation_authority,
                        )
                    },
                    cleanup_precommit_worktree_with_authority(
                        &project_root,
                        &worktree,
                        preparation_authority.clone(),
                    ),
                    |action| {
                        compensate_owner_lease_with_authority(
                            owner_lease,
                            action,
                            &preparation_authority,
                        )
                    },
                    prepared_audit,
                    &preparation_authority,
                )
                .await;
                let mut compensation_errors = compensation_errors;
                if compensation_errors.is_empty() {
                    if let Some(intent) = preparation_intent {
                        if let Err(cleanup) = intent.cleanup() {
                            compensation_errors
                                .push(format!("preparation intent compensation failed: {cleanup}"));
                        }
                    }
                }
                return if compensation_errors.is_empty() {
                    Err(message)
                } else {
                    Err(format!("{message}; {}", compensation_errors.join("; ")))
                };
            }
        };
        run_spawn_handoff_phase_hook("record_published");
        if let Some(intent) = preparation_intent.as_mut() {
            if let Err(error) =
                intent.revise(SpawnPreparationPhase::RecordPublished, None, None, None)
            {
                let compensation_errors = collect_spawn_compensation_async_with_audit(
                    || {
                        cleanup_record_after_registration_failure_locked(
                            &project_root,
                            &record,
                            &publication,
                            &preparation_authority,
                        )
                    },
                    cleanup_precommit_worktree_with_authority(
                        &project_root,
                        &worktree,
                        preparation_authority.clone(),
                    ),
                    |action| {
                        compensate_owner_lease_with_authority(
                            owner_lease,
                            action,
                            &preparation_authority,
                        )
                    },
                    prepared_audit,
                    &preparation_authority,
                )
                .await;
                drop(preparation_intent);
                return if compensation_errors.is_empty() {
                    Err(error)
                } else {
                    Err(format!("{error}; {}", compensation_errors.join("; ")))
                };
            }
        }
        let start_gate = match crate::daemons::task::TASK_MANAGER
            .register_paused_task(subagent_id.clone(), "subagent")
        {
            Ok(start_gate) => start_gate,
            Err(error) => {
                let compensation_errors = collect_spawn_compensation_async_with_audit(
                    || {
                        cleanup_record_after_registration_failure_locked(
                            &project_root,
                            &record,
                            &publication,
                            &preparation_authority,
                        )
                    },
                    cleanup_precommit_worktree_with_authority(
                        &project_root,
                        &worktree,
                        preparation_authority.clone(),
                    ),
                    |action| {
                        compensate_owner_lease_with_authority(
                            owner_lease,
                            action,
                            &preparation_authority,
                        )
                    },
                    prepared_audit,
                    &preparation_authority,
                )
                .await;
                let mut compensation_errors = compensation_errors;
                if compensation_errors.is_empty() {
                    if let Some(intent) = preparation_intent {
                        if let Err(cleanup) = intent.cleanup() {
                            compensation_errors
                                .push(format!("preparation intent compensation failed: {cleanup}"));
                        }
                    }
                }
                return if compensation_errors.is_empty() {
                    Err(error)
                } else {
                    Err(format!("{error}; {}", compensation_errors.join("; ")))
                };
            }
        };
        run_spawn_handoff_phase_hook("manager_registered");
        if let Some(intent) = preparation_intent.as_mut() {
            if let Err(error) =
                intent.revise(SpawnPreparationPhase::ManagerRegistered, None, None, None)
            {
                let mut compensation_errors = Vec::new();
                if let Err(rollback) =
                    crate::daemons::task::TASK_MANAGER.rollback_unattached_task(&subagent_id)
                {
                    compensation_errors
                        .push(format!("task registration rollback failed: {rollback}"));
                }
                compensation_errors.extend(
                    collect_spawn_compensation_async_with_audit(
                        || {
                            cleanup_record_after_registration_failure_locked(
                                &project_root,
                                &record,
                                &publication,
                                &preparation_authority,
                            )
                        },
                        cleanup_precommit_worktree_with_authority(
                            &project_root,
                            &worktree,
                            preparation_authority.clone(),
                        ),
                        |action| {
                            compensate_owner_lease_with_authority(
                                owner_lease,
                                action,
                                &preparation_authority,
                            )
                        },
                        prepared_audit,
                        &preparation_authority,
                    )
                    .await,
                );
                drop(preparation_intent);
                return if compensation_errors.is_empty() {
                    Err(error)
                } else {
                    Err(format!("{error}; {}", compensation_errors.join("; ")))
                };
            }
        }
        prepared_audit.disarm();
        let launch = launch_subagent_task(
            project_root.clone(),
            subagent_id.clone(),
            prompt,
            max_steps,
            parent_session_id,
            PreparedSubagentTask {
                record: record.clone(),
                worktree,
                owner_lease,
            },
            start_gate,
            &preparation_authority,
            preparation_intent
                .as_ref()
                .and_then(|intent| intent.data.process_scope_plan.clone()),
        );
        let mut launched = match launch {
            Ok(launched) => launched,
            Err(error) => {
                let deadline = preparation_authority.operation_deadline();
                let execution_generation = record.execution_generation;
                let owner_lease = record.owner_lease.clone();
                let rollback = crate::daemons::task::TASK_MANAGER
                    .rollback_unattached_task(&subagent_id)
                    .err()
                    .map(|value| format!("; task registration rollback failed: {value}"))
                    .unwrap_or_default();
                drop(preparation_intent);
                drop(publication);
                drop(preparation_authority);
                let persistence = persist_unstarted_after_preparation_unlock(
                    &project_root,
                    &subagent_id,
                    execution_generation,
                    owner_lease.as_deref(),
                    &error,
                    deadline,
                )
                .err()
                .map(|value| format!("; unstarted outcome persistence failed: {value}"))
                .unwrap_or_default();
                return Err(format!("{error}{rollback}{persistence}"));
            }
        };
        let response = launched.response.clone();
        run_spawn_handoff_phase_hook("handoff_established");
        if let Err(error) = pause_after_supervisor_ready_before_commit(&subagent_id) {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; gated task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(preparation_intent);
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "subagent supervisor READY barrier failed: {error}{cancellation}"
            ));
        }
        let intent = preparation_intent
            .as_mut()
            .ok_or("spawn preparation authority was lost before handoff commit")?;
        if let Err(error) = intent.revise(
            SpawnPreparationPhase::HandoffProven,
            None,
            None,
            launched.precommit_process_scope(),
        ) {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; launched task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(preparation_intent);
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to persist subagent handoff: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("handoff_proven");
        if let Err(error) =
            commit_spawn_handoff_record_locked(&project_root, &mut record, &mut publication, intent)
        {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; launched task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(preparation_intent);
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to commit subagent handoff: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("handoff_committed");
        if let Err(error) = launched.commit_supervisor_handoff(&preparation_authority) {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; handed-off task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to acknowledge committed subagent supervisor handoff: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("supervisor_started");
        if let Err(error) =
            preparation_authority.verify_until(preparation_authority.operation_deadline())
        {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; handed-off task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "subagent handoff deadline expired before start-gate release: {error}{cancellation}"
            ));
        }
        if let Err(error) = crate::daemons::task::TASK_MANAGER.start_task(&subagent_id) {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; handed-off task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to release committed subagent start gate: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("launch_released");
        run_spawn_handoff_phase_hook("before_intent_retirement");
        let intent = preparation_intent
            .take()
            .ok_or("spawn preparation authority was lost before final retirement")?;
        if let Err(error) = intent.cleanup() {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; launched task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to retire authoritative spawn preparation after handoff: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("intent_retired");
        drop(publication);
        drop(preparation_authority);
        Ok(response)
    })
}

#[cfg(test)]
// The test launcher mirrors the production lifecycle boundary, whose request,
// durable authorities, and start gate must remain independently inspectable.
#[allow(clippy::too_many_arguments)]
fn launch_subagent_task(
    project_root: PathBuf,
    subagent_id: String,
    prompt: String,
    max_steps: u32,
    parent_session_id: Option<String>,
    prepared: PreparedSubagentTask,
    start_gate: tokio::sync::oneshot::Receiver<()>,
    authority: &SpawnPreparationAuthority,
    process_scope_plan: Option<SubagentProcessScopePlan>,
) -> Result<LaunchedSubagentTask, String> {
    let deadline = authority.operation_deadline();
    authority.verify_until(deadline)?;
    let process_scope_plan = process_scope_plan
        .ok_or_else(|| "test subagent launch lacks preplanned process authority".to_string())?;
    let PreparedSubagentTask {
        record,
        worktree,
        owner_lease,
    } = prepared;
    let record_id = subagent_id.clone();
    let child_session_id = subagent_id.clone();
    let worktree_path = worktree.path;
    let record_root = project_root.clone();
    let execution_generation = owner_lease.execution_generation;
    let lease_id = owner_lease.lease_id.clone();
    let task_lease_id = lease_id.clone();
    let process_identity = crate::sandbox::process::ProcessIdentity::current()?;
    #[cfg(target_os = "linux")]
    let process_backend = crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace;
    #[cfg(windows)]
    let process_backend = crate::sandbox::process::ProcessScopeBackend::WindowsJobObject;
    #[cfg(target_os = "macos")]
    let process_backend = crate::sandbox::process::ProcessScopeBackend::MacosProcessGroup;
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    let process_backend =
        return Err("test subagent process scope is unsupported on this platform".to_string());
    let now = Utc::now();
    let precommit_process_scope = crate::sandbox::process::ProcessScopeRecord {
        version: 2,
        scope_id: subagent_id.clone(),
        workload_kind: "subagent".to_string(),
        execution_generation,
        cleanup_lease_id: process_scope_plan.cleanup_lease_id,
        supervisor_registration_nonce: Some(process_scope_plan.supervisor_registration_nonce),
        owner: process_identity.clone(),
        backend: process_backend,
        status: crate::sandbox::process::ProcessScopeStatus::Running,
        launch_committed: Some(false),
        supervisor: Some(process_identity.clone()),
        direct_child: Some(process_identity),
        cleanup_reason: None,
        cleanup_proof: None,
        launch_abort_proof: None,
        created_at: now,
        updated_at: now,
    };
    let guard = SubagentRunGuard::new(record_root.clone(), record_id.clone(), owner_lease);
    let session_lock_policy = crate::session::SessionStore::current_lock_policy();
    let handle = tokio::spawn(crate::session::SessionStore::with_optional_lock_policy(
        session_lock_policy,
        async move {
            let mut guard = guard;
            if start_gate.await.is_err() {
                guard.reason =
                    "subagent execution handoff was cancelled before durable commit".to_string();
                return;
            }
            let config = crate::agent::AgentLoopConfig {
                max_steps,
                auto_approve: false,
                approval_handler: Some(Arc::new(NonInteractiveSubagentApproval)),
                ..Default::default()
            };
            let outcome =
                crate::agent::run_agent_loop(worktree_path, &child_session_id, &prompt, config)
                    .await;
            match persist_subagent_outcome(
                &record_root,
                &record_id,
                execution_generation,
                &task_lease_id,
                outcome,
            ) {
                Ok(()) => guard.disarm(),
                Err(error) => {
                    guard.reason = format!("{INTERRUPTED_ERROR}: {error}");
                }
            }
        },
    ));
    if let Err(error) =
        crate::daemons::task::TASK_MANAGER.attach_abort_handle(&subagent_id, handle.abort_handle())
    {
        handle.abort();
        persist_interrupted_subagent(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            &error,
        );
        return Err(error);
    }
    if let Err(error) = authority.verify_until(deadline) {
        handle.abort();
        return Err(format!(
            "subagent execution handoff exceeded its preparation deadline: {error}"
        ));
    }

    Ok(LaunchedSubagentTask {
        response: public_subagent_start_response(&record, parent_session_id),
        precommit_process_scope: Some(precommit_process_scope),
    })
}

fn public_subagent_start_response(
    record: &SubagentRecord,
    parent_session_id: Option<String>,
) -> Value {
    json!({
        "status": "started",
        "subagent_id": record.id,
        "parent_session_id": parent_session_id,
        "child_session_id": record.child_session_id,
    })
}

#[cfg(not(test))]
fn monitor_subagent_supervisor_exit(
    mut child: std::process::Child,
    owner_lease: SubagentOwnerLease,
    project_root: PathBuf,
    subagent_id: String,
    execution_generation: u64,
    lease_id: String,
    wait_tx: tokio::sync::oneshot::Sender<Result<std::process::ExitStatus, String>>,
) {
    let status = child.wait().map_err(|error| error.to_string());
    let initial_record = get_subagent_record_unreconciled(&project_root, &subagent_id).ok();
    let completion_verified = initial_record.as_ref().is_some_and(|record| {
        record.status != "running"
            && process_scope_retirement_result(record)
                .is_none_or(|result| !result.contains_key("ownership_reconciliation"))
            && has_direct_terminal_process_scope_authority(record)
            && matches!(terminal_process_scope_authority(record), Ok(Some(_)))
            && retire_terminal_process_scope(&project_root, record).unwrap_or(false)
    });
    let (lease_cleanup, record) = if completion_verified {
        (owner_lease.remove(), initial_record)
    } else {
        let cleanup = owner_lease.release_for_reconciliation();
        let reconciled = match reconcile_subagent_ownership_with_owner_state(
            &project_root,
            &subagent_id,
            true,
        ) {
            Ok(record) => Some(record),
            Err(_)
                if initial_record.as_ref().is_some_and(|record| {
                    process_scope_retirement_result(record)
                        .is_some_and(|result| result.contains_key("ownership_reconciliation"))
                }) =>
            {
                None
            }
            Err(_) => initial_record,
        };
        (cleanup, reconciled)
    };
    let lease_cleanup_succeeded = lease_cleanup.is_ok();
    if let Err(error) = lease_cleanup {
        persist_owner_lease_cleanup_error(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            &error,
        );
    }
    if let Some(record) = record {
        if lease_cleanup_succeeded {
            let public_result = record
                .result
                .clone()
                .and_then(project_public_subagent_result);
            match record.status.as_str() {
                "completed" => {
                    crate::daemons::task::TASK_MANAGER.complete(&subagent_id, public_result)
                }
                "failed" => crate::daemons::task::TASK_MANAGER.fail(
                    &subagent_id,
                    record
                        .error
                        .clone()
                        .unwrap_or_else(|| "subagent supervisor failed".to_string()),
                    public_result,
                ),
                _ => {}
            }
        }
    }
    let _ = wait_tx.send(status);
}

fn retire_terminal_process_scope(
    project_root: &Path,
    expected: &SubagentRecord,
) -> Result<bool, String> {
    retire_terminal_process_scope_until(project_root, expected, None)
}

fn retire_terminal_process_scope_until(
    project_root: &Path,
    expected: &SubagentRecord,
    deadline: Option<Instant>,
) -> Result<bool, String> {
    if !status_retains_process_scope_retirement_authority(&expected.status) {
        return Ok(false);
    }
    ensure_subagent_reconciliation_deadline(deadline)?;
    let project_root = canonical_project_root(project_root)?;
    let path = record_path(&project_root, &expected.id)?;
    let records_directory = ensure_records_directory_until(&project_root, deadline)?;
    with_subagent_reconciliation_lock_in(
        &project_root,
        &expected.id,
        &records_directory,
        deadline,
        |directory, deadline| {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let opened = read_opened_subagent_record_in(directory, &path)?;
            validate_reopened_subagent_record(expected, &opened.record)?;
            let Some((execution_generation, authority)) =
                terminal_process_scope_authority(&opened.record)?
            else {
                return Ok(false);
            };
            directory.verify_file_identity(&path, &opened.file)?;
            ensure_subagent_reconciliation_deadline(deadline)?;
            let retired = retire_process_scope_authority_in_locked_records_until(
                &project_root,
                directory,
                &opened.record.id,
                execution_generation,
                &authority,
                deadline.ok_or_else(subagent_reconciliation_deadline_elapsed)?,
            )?;
            ensure_subagent_reconciliation_deadline(deadline)?;
            directory.verify_file_identity(&path, &opened.file)?;
            Ok(retired)
        },
    )
}

fn retire_terminal_process_scope_in_locked_records_until(
    project_root: &Path,
    records: &crate::daemons::state::StableDirectory,
    record: &SubagentRecord,
    deadline: Instant,
) -> Result<(), String> {
    let Some((generation, authority)) = terminal_process_scope_authority(record)? else {
        return Err(
            "terminal subagent has no exact process-scope retirement authority".to_string(),
        );
    };
    let _retired = retire_process_scope_authority_in_locked_records_until(
        project_root,
        records,
        &record.id,
        generation,
        &authority,
        deadline,
    )?;
    Ok(())
}

fn retire_process_scope_authority_in_locked_records_until(
    project_root: &Path,
    records: &crate::daemons::state::StableDirectory,
    scope_id: &str,
    execution_generation: u64,
    authority: &TerminalProcessScopeAuthority,
    deadline: Instant,
) -> Result<bool, String> {
    let Some(store) = crate::sandbox::process::ProcessScopeStore::open_existing_bound_to_records(
        project_root,
        records,
        deadline,
    )?
    else {
        return Ok(false);
    };
    match authority {
        TerminalProcessScopeAuthority::Cleanup(proof) => {
            store.retire_complete(scope_id, execution_generation, proof)
        }
        TerminalProcessScopeAuthority::LaunchAbort(proof) => {
            store.retire_launch_abort(scope_id, execution_generation, proof)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalProcessScopeAuthority {
    Cleanup(crate::sandbox::process::CleanupProof),
    LaunchAbort(crate::sandbox::process::LaunchAbortProof),
}

fn terminal_process_scope_authority(
    record: &SubagentRecord,
) -> Result<Option<(u64, TerminalProcessScopeAuthority)>, String> {
    if !status_retains_process_scope_retirement_authority(&record.status) {
        return Ok(None);
    }
    let execution_generation = record.execution_generation.ok_or_else(|| {
        format!(
            "terminal subagent {} has no execution generation for scope retirement",
            record.id
        )
    })?;
    let owner_lease = record.owner_lease.as_deref().ok_or_else(|| {
        format!(
            "terminal subagent {} has no owner lease for scope retirement",
            record.id
        )
    })?;
    validate_execution_ownership(execution_generation, owner_lease)?;
    let Some(result) = process_scope_retirement_result(record) else {
        return Ok(None);
    };

    let direct_cleanup_proof = match (
        result.get("cleanup_verified").and_then(Value::as_bool),
        result.get("cleanup_proof"),
    ) {
        (Some(true), Some(proof)) => Some(parse_terminal_cleanup_proof(proof)?),
        (Some(true), None) => {
            return Err("verified terminal subagent result has no cleanup proof".to_string());
        }
        (_, Some(_)) => {
            return Err(
                "terminal subagent result has a cleanup proof without verified cleanup".to_string(),
            );
        }
        _ => None,
    };
    let direct_launch_abort_proof = parse_terminal_launch_abort_authority(result)?;

    let (reconciled_cleanup_proof, reconciled_launch_abort_proof) =
        match result.get("ownership_reconciliation") {
            None => None,
            Some(evidence) => {
                let evidence = evidence
                    .as_object()
                    .ok_or("terminal ownership reconciliation is not an object")?;
                if evidence.get("subagent_id").and_then(Value::as_str) != Some(record.id.as_str())
                    || evidence.get("execution_generation").and_then(Value::as_u64)
                        != Some(execution_generation)
                    || evidence.get("owner_lease").and_then(Value::as_str) != Some(owner_lease)
                    || !retirement_terminal_status_matches(
                        &record.status,
                        evidence.get("terminal_status").and_then(Value::as_str),
                    )
                {
                    return Err(
                        "terminal ownership reconciliation does not match subagent execution ownership"
                            .to_string(),
                    );
                }
                let expected_reconciliation_id = subagent_reconciliation_id(
                    &record.id,
                    execution_generation,
                    owner_lease,
                )?;
                if evidence
                    .get("reconciliation_id")
                    .is_some_and(|observed| {
                        observed.as_str() != Some(expected_reconciliation_id.as_str())
                    })
                {
                    return Err(
                        "terminal ownership reconciliation has an invalid reconciliation identity"
                            .to_string(),
                    );
                }
                let cleanup_proof = match (
                    evidence.get("cleanup_verified").and_then(Value::as_bool),
                    evidence.get("cleanup_proof"),
                ) {
                    (Some(true), Some(proof)) => Some(parse_terminal_cleanup_proof(proof)?),
                    (Some(true), None) => {
                        return Err(
                            "verified terminal ownership reconciliation has no cleanup proof"
                                .to_string(),
                        );
                    }
                    (_, Some(_)) => {
                        return Err(
                            "terminal ownership reconciliation has a cleanup proof without verified cleanup"
                                .to_string(),
                        );
                    }
                    _ => None,
                };
                let launch_abort_proof = parse_terminal_launch_abort_authority(evidence)?;
                Some((cleanup_proof, launch_abort_proof))
            }
        }
        .unwrap_or((None, None));

    let cleanup_proof = match (direct_cleanup_proof, reconciled_cleanup_proof) {
        (Some(direct), Some(reconciled)) if direct != reconciled => {
            return Err("terminal subagent cleanup proofs conflict".to_string());
        }
        (Some(proof), _) | (_, Some(proof)) => Some(proof),
        (None, None) => None,
    };
    let launch_abort_proof = match (direct_launch_abort_proof, reconciled_launch_abort_proof) {
        (Some(direct), Some(reconciled)) if direct != reconciled => {
            return Err("terminal subagent launch-abort proofs conflict".to_string());
        }
        (Some(proof), _) | (_, Some(proof)) => Some(proof),
        (None, None) => None,
    };
    match (cleanup_proof, launch_abort_proof) {
        (Some(_), Some(_)) => {
            Err("terminal subagent carries conflicting managed-process authorities".to_string())
        }
        (Some(proof), None) => {
            if proof.execution_generation != execution_generation || !proof.descendants_reaped {
                return Err(
                    "terminal subagent cleanup proof does not own its execution".to_string()
                );
            }
            Ok(Some((
                execution_generation,
                TerminalProcessScopeAuthority::Cleanup(proof),
            )))
        }
        (None, Some(proof)) => {
            if proof.execution_generation != execution_generation || !proof.workload_never_launched
            {
                return Err(
                    "terminal subagent launch-abort proof does not own its execution".to_string(),
                );
            }
            Ok(Some((
                execution_generation,
                TerminalProcessScopeAuthority::LaunchAbort(proof),
            )))
        }
        (None, None) => Ok(None),
    }
}

fn has_direct_terminal_process_scope_authority(record: &SubagentRecord) -> bool {
    process_scope_retirement_result(record).is_some_and(|result| {
        result.contains_key("cleanup_proof") || result.contains_key("launch_abort_proof")
    })
}

fn parse_terminal_launch_abort_authority(
    result: &serde_json::Map<String, Value>,
) -> Result<Option<crate::sandbox::process::LaunchAbortProof>, String> {
    match (
        result.get("launch_abort_verified").and_then(Value::as_bool),
        result
            .get("workload_never_launched")
            .and_then(Value::as_bool),
        result.get("launch_abort_proof"),
    ) {
        (Some(true), Some(true), Some(proof)) => {
            if result.get("cleanup_verified").and_then(Value::as_bool) == Some(true)
                || result.get("cleanup_proof").is_some()
            {
                return Err(
                    "terminal launch-abort authority conflicts with cleanup verification"
                        .to_string(),
                );
            }
            serde_json::from_value(proof.clone())
                .map(Some)
                .map_err(|error| {
                    format!("invalid terminal managed-process launch-abort proof: {error}")
                })
        }
        (Some(true), _, _) => {
            Err("verified terminal launch abort has incomplete proof evidence".to_string())
        }
        (_, _, Some(_)) => {
            Err("terminal launch-abort proof is present without verified launch abort".to_string())
        }
        _ => Ok(None),
    }
}

fn status_retains_process_scope_retirement_authority(status: &str) -> bool {
    matches!(
        status,
        "completed"
            | "failed"
            | "cancelled"
            | "verification_failed"
            | MERGE_FAILED_STATUS
            | MERGE_PENDING_STATUS
            | "merged"
    )
}

fn process_scope_retirement_result(
    record: &SubagentRecord,
) -> Option<&serde_json::Map<String, Value>> {
    let result = record.result.as_ref()?.as_object()?;
    if matches!(record.status.as_str(), MERGE_PENDING_STATUS | "merged") {
        result.get("subagent_result")?.as_object()
    } else {
        Some(result)
    }
}

fn retirement_terminal_status_matches(record_status: &str, terminal_status: Option<&str>) -> bool {
    if matches!(
        record_status,
        "verification_failed" | MERGE_FAILED_STATUS | MERGE_PENDING_STATUS | "merged"
    ) {
        terminal_status == Some("completed")
    } else {
        terminal_status == Some(record_status)
    }
}

fn parse_terminal_cleanup_proof(
    proof: &Value,
) -> Result<crate::sandbox::process::CleanupProof, String> {
    serde_json::from_value(proof.clone())
        .map_err(|error| format!("invalid terminal managed-process cleanup proof: {error}"))
}

#[cfg(not(test))]
// The production launcher keeps request data, durable ownership, the start
// gate, and process authority explicit across the handoff protocol.
#[allow(clippy::too_many_arguments)]
fn launch_subagent_task(
    project_root: PathBuf,
    subagent_id: String,
    prompt: String,
    max_steps: u32,
    parent_session_id: Option<String>,
    prepared: PreparedSubagentTask,
    start_gate: tokio::sync::oneshot::Receiver<()>,
    authority: &SpawnPreparationAuthority,
    process_scope_plan: Option<SubagentProcessScopePlan>,
) -> Result<LaunchedSubagentTask, String> {
    let deadline = authority.operation_deadline();
    authority.verify_until(deadline)?;
    let PreparedSubagentTask {
        record,
        worktree,
        owner_lease,
    } = prepared;
    let execution_generation = owner_lease.execution_generation;
    let lease_id = owner_lease.lease_id.clone();
    let process_scope_plan = match process_scope_plan {
        Some(plan) => plan,
        None => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                "subagent supervisor launch has no preplanned process authority".to_string(),
                deadline,
            );
        }
    };
    let scope_store = match crate::sandbox::process::ProcessScopeStore::open_with_lock_deadline(
        &project_root,
        deadline,
    ) {
        Ok(store) => store,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                error,
                deadline,
            );
        }
    };
    let backend = match crate::sandbox::process::ProcessScopeBackend::production() {
        Ok(backend) => backend,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                error,
                deadline,
            );
        }
    };
    let owner_identity = match crate::sandbox::process::ProcessIdentity::current() {
        Ok(identity) => identity,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                error,
                deadline,
            );
        }
    };
    let mut process_scope = match scope_store.prepare_subagent_launch(
        &subagent_id,
        execution_generation,
        &process_scope_plan.cleanup_lease_id,
        &process_scope_plan.supervisor_registration_nonce,
        owner_identity,
        backend,
    ) {
        Ok(scope) => scope,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                error,
                deadline,
            );
        }
    };
    if let Err(error) = authority.verify_until(deadline) {
        return fail_unstarted_subagent(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            owner_lease,
            Some((&scope_store, &process_scope)),
            format!("subagent process-scope preparation exceeded its deadline: {error}"),
            deadline,
        );
    }
    let executable = match resolve_nib_executable() {
        Ok(executable) => executable,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                Some((&scope_store, &process_scope)),
                error,
                deadline,
            );
        }
    };
    if let Err(error) = pause_after_scope_prepared_before_supervisor_spawn(&subagent_id) {
        return fail_unstarted_subagent(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            owner_lease,
            Some((&scope_store, &process_scope)),
            error,
            deadline,
        );
    }
    let handoff_nonce = process_scope_plan.supervisor_registration_nonce.clone();
    let request = SubagentSupervisorRequest {
        version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
        handoff_nonce: handoff_nonce.clone(),
        subagent_id: subagent_id.clone(),
        execution_generation,
        owner_lease: lease_id.clone(),
        cleanup_lease_id: process_scope.cleanup_lease_id.clone(),
        worker: SubagentWorkerRequest { prompt, max_steps },
    };
    let mut encoded_request = match serde_json::to_vec(&request) {
        Ok(request) => request,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                Some((&scope_store, &process_scope)),
                format!("failed to encode subagent worker request: {error}"),
                deadline,
            );
        }
    };
    encoded_request.push(b'\n');

    let mut command = std::process::Command::new(&executable);
    command
        .arg("subagent-supervisor")
        .arg("--project-root")
        .arg(&project_root)
        .arg("--subagent-id")
        .arg(&subagent_id)
        .arg("--execution-generation")
        .arg(execution_generation.to_string())
        .arg("--owner-lease")
        .arg(&lease_id)
        .arg("--cleanup-lease-id")
        .arg(&process_scope.cleanup_lease_id)
        .arg("--supervisor-registration-nonce")
        .arg(&handoff_nonce)
        .arg("--worktree")
        .arg(&worktree.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    if let Err(error) = authority.verify_until(deadline) {
        return fail_unstarted_subagent(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            owner_lease,
            Some((&scope_store, &process_scope)),
            format!("subagent supervisor launch exceeded its deadline: {error}"),
            deadline,
        );
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("failed to start subagent supervisor: {error}");
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                Some((&scope_store, &process_scope)),
                message,
                deadline,
            );
        }
    };
    if let Err(error) = authority.verify_until(deadline) {
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            format!("subagent supervisor launch exceeded its deadline: {error}"),
        );
    }
    let supervisor_identity = match crate::sandbox::process::ProcessIdentity::capture(child.id()) {
        Ok(identity) => identity,
        Err(error) => {
            return fail_pre_delivery_subagent(
                PreDeliverySubagent {
                    project_root: &project_root,
                    id: &subagent_id,
                    execution_generation,
                    lease_id: &lease_id,
                    scope_store: &scope_store,
                    process_scope: &process_scope,
                    deadline,
                },
                owner_lease,
                child,
                format!("failed to identify the subagent supervisor: {error}"),
            );
        }
    };
    process_scope = loop {
        match scope_store.observe_registered_launch_supervisor(
            &subagent_id,
            execution_generation,
            &process_scope.cleanup_lease_id,
            &handoff_nonce,
            &supervisor_identity,
        ) {
            Ok(Some(scope)) => break scope,
            Ok(None) if Instant::now() < deadline => match child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Ok(Some(status)) => {
                    return fail_pre_delivery_subagent(
                        PreDeliverySubagent {
                            project_root: &project_root,
                            id: &subagent_id,
                            execution_generation,
                            lease_id: &lease_id,
                            scope_store: &scope_store,
                            process_scope: &process_scope,
                            deadline,
                        },
                        owner_lease,
                        child,
                        format!("subagent supervisor exited before self-registration: {status}"),
                    );
                }
                Err(error) => {
                    return fail_pre_delivery_subagent(
                        PreDeliverySubagent {
                            project_root: &project_root,
                            id: &subagent_id,
                            execution_generation,
                            lease_id: &lease_id,
                            scope_store: &scope_store,
                            process_scope: &process_scope,
                            deadline,
                        },
                        owner_lease,
                        child,
                        format!("failed to observe subagent supervisor: {error}"),
                    );
                }
            },
            Ok(None) => {
                return fail_pre_delivery_subagent(
                    PreDeliverySubagent {
                        project_root: &project_root,
                        id: &subagent_id,
                        execution_generation,
                        lease_id: &lease_id,
                        scope_store: &scope_store,
                        process_scope: &process_scope,
                        deadline,
                    },
                    owner_lease,
                    child,
                    "subagent supervisor did not self-register before its deadline".to_string(),
                );
            }
            Err(error) => {
                return fail_pre_delivery_subagent(
                    PreDeliverySubagent {
                        project_root: &project_root,
                        id: &subagent_id,
                        execution_generation,
                        lease_id: &lease_id,
                        scope_store: &scope_store,
                        process_scope: &process_scope,
                        deadline,
                    },
                    owner_lease,
                    child,
                    format!("failed to validate the self-registered subagent supervisor: {error}"),
                );
            }
        }
    };
    if let Err(error) = authority.verify_until(deadline) {
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            format!("subagent supervisor registration exceeded its deadline: {error}"),
        );
    }
    let supervisor_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            return fail_pre_delivery_subagent(
                PreDeliverySubagent {
                    project_root: &project_root,
                    id: &subagent_id,
                    execution_generation,
                    lease_id: &lease_id,
                    scope_store: &scope_store,
                    process_scope: &process_scope,
                    deadline,
                },
                owner_lease,
                child,
                "subagent supervisor stdin is unavailable".to_string(),
            );
        }
    };
    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("missing-stdout") {
        child.stdout.take();
        drop(supervisor_stdin);
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            "injected missing subagent supervisor stdout".to_string(),
        );
    }
    let supervisor_stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            drop(supervisor_stdin);
            return fail_pre_delivery_subagent(
                PreDeliverySubagent {
                    project_root: &project_root,
                    id: &subagent_id,
                    execution_generation,
                    lease_id: &lease_id,
                    scope_store: &scope_store,
                    process_scope: &process_scope,
                    deadline,
                },
                owner_lease,
                child,
                "subagent supervisor stdout is unavailable".to_string(),
            );
        }
    };
    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("readiness-monitor-failure") {
        drop(supervisor_stdout);
        drop(supervisor_stdin);
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            "injected subagent readiness-monitor failure".to_string(),
        );
    }
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(2);
    if let Err(error) = std::thread::Builder::new()
        .name(format!("nib-subagent-ready-{subagent_id}"))
        .spawn(move || {
            let mut reader = BufReader::new(supervisor_stdout);
            for phase in ["READY", "STARTED"] {
                let result = read_subagent_supervisor_frame(&mut reader, phase);
                let terminal = result.is_err();
                if ready_tx.send(result).is_err() || terminal {
                    break;
                }
            }
        })
    {
        let message = format!("failed to monitor subagent supervisor readiness: {error}");
        drop(supervisor_stdin);
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            message,
        );
    }

    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("wait-monitor-failure") {
        drop(supervisor_stdin);
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            "injected subagent exit-monitor failure".to_string(),
        );
    }

    let control = Arc::new(Mutex::new(Some(supervisor_stdin)));
    let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
    let wait_root = project_root.clone();
    let wait_id = subagent_id.clone();
    let wait_lease_id = lease_id.clone();
    let (ownership_tx, ownership_rx) = std::sync::mpsc::sync_channel(1);
    let wait_thread = std::thread::Builder::new()
        .name(format!("nib-subagent-supervisor-{subagent_id}"))
        .spawn(move || match ownership_rx.recv() {
            Ok((child, owner_lease)) => monitor_subagent_supervisor_exit(
                child,
                owner_lease,
                wait_root,
                wait_id,
                execution_generation,
                wait_lease_id,
                wait_tx,
            ),
            Err(error) => {
                let _ = wait_tx.send(Err(format!(
                    "subagent supervisor ownership handoff failed: {error}"
                )));
            }
        });
    if let Err(error) = wait_thread {
        let message = format!("failed to monitor subagent supervisor: {error}");
        if let Ok(mut stdin) = control.lock() {
            stdin.take();
        }
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            message,
        );
    }
    if let Err(error) = ownership_tx.send((child, owner_lease)) {
        let (child, owner_lease) = error.0;
        if let Ok(mut stdin) = control.lock() {
            stdin.take();
        }
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            "subagent supervisor monitor stopped before ownership handoff".to_string(),
        );
    }

    if let Err(error) = authority.verify_until(deadline) {
        signal_supervisor_cancellation(&control);
        return Err(format!(
            "subagent supervisor ownership handoff exceeded its deadline: {error}"
        ));
    }

    authority.verify_until(deadline)?;
    let initialization = control
        .lock()
        .map_err(|_| "subagent supervisor control lock is poisoned".to_string())
        .and_then(|mut control| {
            let supervisor_stdin = control
                .as_mut()
                .ok_or("subagent supervisor control pipe is unavailable".to_string())?;
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("after-first-byte") {
                supervisor_stdin
                    .write_all(&encoded_request[..1])
                    .and_then(|()| supervisor_stdin.flush())
                    .map_err(|error| {
                        format!("failed to write the subagent launch failpoint byte: {error}")
                    })?;
                return Err("injected subagent launch failure after first request byte".to_string());
            }
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("after-request-body") {
                supervisor_stdin
                    .write_all(&encoded_request[..encoded_request.len() - 1])
                    .and_then(|()| supervisor_stdin.flush())
                    .map_err(|error| {
                        format!("failed to write the subagent launch failpoint body: {error}")
                    })?;
                return Err(
                    "injected subagent launch failure after request body before newline"
                        .to_string(),
                );
            }
            supervisor_stdin
                .write_all(&encoded_request)
                .map_err(|error| format!("failed to initialize subagent supervisor: {error}"))?;
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("after-request-write") {
                return Err("injected subagent launch failure after request write".to_string());
            }
            supervisor_stdin
                .flush()
                .map_err(|error| format!("failed to initialize subagent supervisor: {error}"))?;
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("after-request-flush") {
                return Err("injected subagent launch failure after request flush".to_string());
            }
            Ok(())
        });
    if let Err(error) = initialization {
        signal_supervisor_cancellation(&control);
        return Err(error);
    }
    if let Err(error) = authority.verify_until(deadline) {
        signal_supervisor_cancellation(&control);
        return Err(format!(
            "subagent supervisor initialization exceeded its deadline: {error}"
        ));
    }

    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("readiness-timeout") {
        signal_supervisor_cancellation(&control);
        return Err("injected subagent supervisor readiness timeout".to_string());
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        signal_supervisor_cancellation(&control);
        return Err("subagent supervisor readiness exceeded its preparation deadline".to_string());
    }
    let ready = match ready_rx.recv_timeout(remaining.min(SUBAGENT_SUPERVISOR_READY_TIMEOUT)) {
        Ok(Ok(frame)) => frame,
        Ok(Err(error)) => {
            signal_supervisor_cancellation(&control);
            return Err(format!(
                "failed to read subagent supervisor readiness: {error}"
            ));
        }
        Err(error) => {
            signal_supervisor_cancellation(&control);
            return Err(format!("subagent supervisor did not become ready: {error}"));
        }
    };
    if ready.version != SUBAGENT_SUPERVISOR_PROTOCOL_VERSION
        || ready.phase != "ready"
        || ready.handoff_nonce != handoff_nonce
        || ready.subagent_id != subagent_id
        || ready.execution_generation != execution_generation
        || ready.owner_lease != lease_id
        || ready.process_scope.scope_id != subagent_id
        || ready.process_scope.execution_generation != execution_generation
        || ready.process_scope.cleanup_lease_id != process_scope.cleanup_lease_id
        || ready.process_scope.status != crate::sandbox::process::ProcessScopeStatus::Running
        || ready.process_scope.launch_committed != Some(false)
    {
        signal_supervisor_cancellation(&control);
        return Err(
            "subagent supervisor READY frame does not match its gated execution authority"
                .to_string(),
        );
    }
    let persisted_ready_scope = scope_store.load(&subagent_id)?;
    if persisted_ready_scope != ready.process_scope {
        signal_supervisor_cancellation(&control);
        return Err(
            "subagent supervisor READY frame is not the exact durable process scope".to_string(),
        );
    }
    if let Err(error) = authority.verify_until(deadline) {
        signal_supervisor_cancellation(&control);
        return Err(format!(
            "subagent supervisor readiness exceeded its deadline: {error}"
        ));
    }

    let supervisor_guard = SupervisorControlGuard::new(Arc::clone(&control));
    let handle = tokio::spawn(async move {
        let mut guard = supervisor_guard;
        if start_gate.await.is_err() {
            return;
        }
        let _ = wait_rx.await;
        guard.disarm();
    });

    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("abort-handle-failure") {
        handle.abort();
        return Err("injected subagent abort-handle attachment failure".to_string());
    }

    if let Err(error) =
        crate::daemons::task::TASK_MANAGER.attach_abort_handle(&subagent_id, handle.abort_handle())
    {
        handle.abort();
        return Err(error);
    }
    if let Err(error) = authority.verify_until(deadline) {
        handle.abort();
        return Err(format!(
            "subagent execution handoff exceeded its preparation deadline: {error}"
        ));
    }

    Ok(LaunchedSubagentTask {
        response: public_subagent_start_response(&record, parent_session_id),
        supervisor_handoff: Some(SubagentSupervisorHandoff {
            control,
            responses: ready_rx,
            ready,
        }),
    })
}

#[cfg(not(test))]
struct PreDeliverySubagent<'a> {
    project_root: &'a Path,
    id: &'a str,
    execution_generation: u64,
    lease_id: &'a str,
    scope_store: &'a crate::sandbox::process::ProcessScopeStore,
    process_scope: &'a crate::sandbox::process::ProcessScopeRecord,
    deadline: Instant,
}

#[cfg(not(test))]
fn fail_pre_delivery_subagent<T>(
    context: PreDeliverySubagent<'_>,
    owner_lease: SubagentOwnerLease,
    mut supervisor: std::process::Child,
    error: String,
) -> Result<T, String> {
    match terminate_unstarted_supervisor(&mut supervisor, context.deadline) {
        Ok(()) => fail_unstarted_subagent(
            context.project_root,
            context.id,
            context.execution_generation,
            context.lease_id,
            owner_lease,
            Some((context.scope_store, context.process_scope)),
            error,
            context.deadline,
        ),
        Err(cleanup_error) => {
            let _ = context.scope_store.mark_recovery_required(
                context.id,
                context.execution_generation,
                &context.process_scope.cleanup_lease_id,
                format!("unstarted supervisor cleanup was not proven: {cleanup_error}"),
            );
            let lease_cleanup = owner_lease.release_for_reconciliation().err();
            let lease_detail = lease_cleanup
                .map(|detail| format!("; owner-lease release failed: {detail}"))
                .unwrap_or_default();
            Err(format!(
                "{error}; supervisor cleanup remains unproven: {cleanup_error}{lease_detail}"
            ))
        }
    }
}

#[cfg(not(test))]
fn terminate_unstarted_supervisor(
    supervisor: &mut std::process::Child,
    deadline: Instant,
) -> Result<(), String> {
    let kill_error = supervisor
        .kill()
        .err()
        .filter(|error| error.kind() != std::io::ErrorKind::InvalidInput);
    loop {
        match supervisor.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                return Err(match kill_error {
                    Some(error) => format!(
                        "failed to terminate supervisor ({error}); it remained live for {} seconds",
                        SUBAGENT_SUPERVISOR_READY_TIMEOUT.as_secs()
                    ),
                    None => format!(
                        "supervisor remained live for {} seconds after termination",
                        SUBAGENT_SUPERVISOR_READY_TIMEOUT.as_secs()
                    ),
                });
            }
            Err(error) => {
                return Err(format!("failed to reap unstarted supervisor: {error}"));
            }
        }
    }
}

#[cfg(not(test))]
// Cleanup requires each exact persisted authority plus the original operation
// deadline; keeping them separate documents the fail-closed compensation set.
#[allow(clippy::too_many_arguments)]
fn fail_unstarted_subagent<T>(
    _project_root: &Path,
    _id: &str,
    _execution_generation: u64,
    _lease_id: &str,
    owner_lease: SubagentOwnerLease,
    process_scope: Option<(
        &crate::sandbox::process::ProcessScopeStore,
        &crate::sandbox::process::ProcessScopeRecord,
    )>,
    error: String,
    deadline: Instant,
) -> Result<T, String> {
    let mut details = Vec::new();
    if let Some((store, scope)) = process_scope {
        if let Err(cleanup) = store.remove_prepared(scope) {
            details.push(format!("process-scope cleanup failed: {cleanup}"));
        }
    }
    if let Err(cleanup) = owner_lease.remove_until(Some(deadline)) {
        details.push(format!("owner-lease cleanup failed: {cleanup}"));
    }
    if details.is_empty() {
        Err(error)
    } else {
        Err(format!("{error}; {}", details.join("; ")))
    }
}

#[cfg(not(test))]
struct SupervisorControlGuard {
    control: Arc<Mutex<Option<std::process::ChildStdin>>>,
    armed: bool,
}

#[cfg(not(test))]
impl SupervisorControlGuard {
    fn new(control: Arc<Mutex<Option<std::process::ChildStdin>>>) -> Self {
        Self {
            control,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        if let Ok(mut control) = self.control.lock() {
            control.take();
        }
    }
}

#[cfg(not(test))]
impl Drop for SupervisorControlGuard {
    fn drop(&mut self) {
        if self.armed {
            signal_supervisor_cancellation(&self.control);
        }
    }
}

#[cfg(not(test))]
fn signal_supervisor_cancellation(control: &Arc<Mutex<Option<std::process::ChildStdin>>>) {
    if let Ok(mut control) = control.lock() {
        if let Some(mut stdin) = control.take() {
            let _ = stdin.write_all(b"cancel\n");
            let _ = stdin.flush();
        }
    }
}

#[cfg(all(debug_assertions, not(test)))]
fn subagent_launch_failpoint(expected: &str) -> bool {
    std::env::var(SUBAGENT_LAUNCH_FAILPOINT_ENV).as_deref() == Ok(expected)
}

#[doc(hidden)]
pub fn run_subagent_supervisor(
    project_root: &Path,
    subagent_id: &str,
    execution_generation: u64,
    owner_lease_id: &str,
    cleanup_lease_id: &str,
    supervisor_registration_nonce: &str,
    worktree: &Path,
) -> Result<(), String> {
    if !is_valid_subagent_id(subagent_id) {
        return Err("invalid subagent id".to_string());
    }
    validate_execution_ownership(execution_generation, owner_lease_id)?;
    let supervisor_identity = crate::sandbox::process::ProcessIdentity::current()?;
    pause_before_supervisor_self_registration(subagent_id, &supervisor_identity)?;
    let scope_store =
        crate::sandbox::process::ProcessScopeStore::open_existing_for_supervisor(project_root)?;
    let _registered_scope = scope_store.self_register_launch_supervisor(
        subagent_id,
        execution_generation,
        cleanup_lease_id,
        supervisor_registration_nonce,
        supervisor_identity.clone(),
    )?;
    scope_store.verify_visible()?;
    let project_root = canonical_project_root(project_root)?;
    scope_store.verify_visible()?;
    let record = get_subagent_record_unreconciled(&project_root, subagent_id)?;
    if record.status != "running"
        || !record_matches_execution(&record, execution_generation, owner_lease_id)?
    {
        return Err("subagent supervisor does not own the running record".to_string());
    }
    validate_record_worktree(&project_root, &record)?;
    let worktree = worktree
        .canonicalize()
        .map_err(|error| format!("subagent supervisor worktree is unavailable: {error}"))?;
    let record_worktree = record
        .worktree_path
        .canonicalize()
        .map_err(|error| format!("subagent record worktree is unavailable: {error}"))?;
    if worktree != record_worktree {
        return Err("subagent supervisor worktree does not match its record".to_string());
    }

    let stdin = std::io::stdin();
    let mut owner_input = BufReader::new(stdin);
    let request = read_supervisor_request(&mut owner_input)?;
    if request.version != SUBAGENT_SUPERVISOR_PROTOCOL_VERSION
        || uuid::Uuid::parse_str(&request.handoff_nonce)
            .map(|nonce| nonce.to_string())
            .ok()
            .as_deref()
            != Some(request.handoff_nonce.as_str())
        || request.subagent_id != subagent_id
        || request.execution_generation != execution_generation
        || request.owner_lease != owner_lease_id
        || request.cleanup_lease_id != cleanup_lease_id
        || request.handoff_nonce != supervisor_registration_nonce
        || request.worker.prompt != record.prompt
        || request.worker.max_steps > 100
    {
        return Err("subagent supervisor request does not match its record".to_string());
    }
    let process_scope = scope_store.load(subagent_id)?;
    if process_scope.execution_generation != execution_generation
        || process_scope.workload_kind != "subagent"
        || process_scope.status != crate::sandbox::process::ProcessScopeStatus::Prepared
        || process_scope.supervisor.as_ref() != Some(&supervisor_identity)
        || process_scope.supervisor_registration_nonce.as_deref()
            != Some(supervisor_registration_nonce)
        || process_scope.direct_child.is_some()
    {
        return Err(
            "subagent supervisor process scope changed before cleanup ownership".to_string(),
        );
    }
    let cleanup_lease = scope_store.acquire_cleanup_lease(&process_scope)?;
    let worker_input = serde_json::to_vec(&request.worker)
        .map_err(|error| format!("failed to encode subagent worker request: {error}"))?;
    let (commit_rx, owner_signal) = start_subagent_owner_protocol_reader(owner_input)?;
    let protocol_identity = request;
    let executable = resolve_nib_executable()?;
    let mut environment: Vec<_> = std::env::vars_os().collect();
    environment.push(("NIB_MANAGED_PROCESS_SCOPE".into(), subagent_id.into()));
    let ready_identity = protocol_identity.handoff_nonce.clone();
    let ready_subagent = protocol_identity.subagent_id.clone();
    let ready_owner = protocol_identity.owner_lease.clone();
    let commit_identity = protocol_identity.handoff_nonce.clone();
    let commit_subagent = protocol_identity.subagent_id.clone();
    let commit_owner = protocol_identity.owner_lease.clone();
    let started_identity = protocol_identity.handoff_nonce;
    let started_subagent = protocol_identity.subagent_id;
    let started_owner = protocol_identity.owner_lease;
    let output = crate::sandbox::process::supervise_foreground_with_claimed_cleanup_and_commit(
        &scope_store,
        &process_scope,
        cleanup_lease,
        owner_signal,
        crate::sandbox::process::SupervisedCommand {
            program: executable,
            args: vec![
                "subagent-worker".into(),
                "--worktree".into(),
                worktree.as_os_str().to_owned(),
                "--subagent-id".into(),
                subagent_id.into(),
            ],
            cwd: worktree.clone(),
            stdin: worker_input,
            environment,
        },
        |running| {
            write_subagent_supervisor_frame(&SubagentSupervisorFrame {
                version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
                phase: "ready".to_string(),
                handoff_nonce: ready_identity,
                subagent_id: ready_subagent,
                execution_generation,
                owner_lease: ready_owner,
                process_scope: running.clone(),
            })
        },
        |running| {
            let commit = commit_rx
                .recv_timeout(SUBAGENT_SUPERVISOR_READY_TIMEOUT)
                .map_err(|error| format!("subagent supervisor COMMIT wait failed: {error}"))??;
            let expected = SubagentSupervisorFrame {
                version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
                phase: "ready".to_string(),
                handoff_nonce: commit_identity,
                subagent_id: commit_subagent,
                execution_generation,
                owner_lease: commit_owner,
                process_scope: running.clone(),
            };
            validate_subagent_supervisor_frame(&expected, &commit, "commit")
        },
        |committed| {
            write_subagent_supervisor_frame(&SubagentSupervisorFrame {
                version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
                phase: "started".to_string(),
                handoff_nonce: started_identity,
                subagent_id: started_subagent,
                execution_generation,
                owner_lease: started_owner,
                process_scope: committed.clone(),
            })
        },
    )?;

    let persistence = if output.cancelled || output.owner_lost {
        persist_supervised_interruption(
            &project_root,
            subagent_id,
            execution_generation,
            owner_lease_id,
            &output.cleanup_proof,
            output.cancelled,
        )
    } else {
        let outcome = serde_json::from_slice::<SubagentWorkerResponse>(&output.stdout)
            .map(|response| response.outcome)
            .unwrap_or_else(|error| {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!(
                    "subagent worker returned an invalid response: {error}; stderr: {}",
                    stderr.trim()
                ))
            });
        persist_subagent_outcome_with_cleanup(
            &project_root,
            subagent_id,
            execution_generation,
            owner_lease_id,
            outcome,
            &output.cleanup_proof,
        )
    };
    persistence?;
    if output.owner_lost {
        if let OwnerLeaseProbe::Acquired(owner_lease) =
            SubagentOwnerLease::probe(&project_root, execution_generation, owner_lease_id)?
        {
            owner_lease.remove()?;
            let record = get_subagent_record_unreconciled(&project_root, subagent_id)?;
            let _ = retire_terminal_process_scope(&project_root, &record)?;
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn run_subagent_worker(worktree: &Path, subagent_id: &str) -> Result<(), String> {
    if !is_valid_subagent_id(subagent_id) {
        return Err("invalid subagent id".to_string());
    }
    #[cfg(all(debug_assertions, not(test)))]
    if let Some(path) = std::env::var_os(SUBAGENT_WORKER_STARTED_PATH_ENV) {
        std::fs::write(PathBuf::from(path), subagent_id.as_bytes()).map_err(|error| {
            format!("failed to publish subagent worker start sentinel: {error}")
        })?;
    }
    #[cfg(all(debug_assertions, not(test)))]
    if let Some(delay) = std::env::var_os(SUBAGENT_WORKER_DELAY_MS_ENV) {
        let delay = delay
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value <= 15_000)
            .ok_or("invalid debug subagent worker delay")?;
        std::thread::sleep(Duration::from_millis(delay));
    }
    let worktree = worktree
        .canonicalize()
        .map_err(|error| format!("subagent worker worktree is unavailable: {error}"))?;
    if !worktree.is_dir() {
        return Err("subagent worker worktree is not a directory".to_string());
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((MAX_SUBAGENT_WORKER_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read subagent worker request: {error}"))?;
    if bytes.len() > MAX_SUBAGENT_WORKER_REQUEST_BYTES {
        return Err("subagent worker request exceeds its size limit".to_string());
    }
    let request: SubagentWorkerRequest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid subagent worker request: {error}"))?;
    if request.prompt.trim().is_empty() || request.max_steps > 100 {
        return Err("subagent worker request is invalid".to_string());
    }
    let runtime = crate::agent::build_agent_runtime("failed to start subagent worker runtime")?;
    let config = crate::agent::AgentLoopConfig {
        max_steps: request.max_steps,
        auto_approve: false,
        approval_handler: Some(Arc::new(NonInteractiveSubagentApproval)),
        ..Default::default()
    };
    let session_lock_policy = crate::session::SessionStore::current_lock_policy();
    let worker_subagent_id = subagent_id.to_string();
    let worker_prompt = request.prompt;
    let outcome = crate::agent::block_on_agent_runtime_worker(
        &runtime,
        async move {
            crate::session::SessionStore::with_optional_lock_policy(
                session_lock_policy,
                crate::agent::run_agent_loop(worktree, &worker_subagent_id, &worker_prompt, config),
            )
            .await
        },
        "subagent runtime worker",
    )?;
    serde_json::to_writer(
        std::io::stdout().lock(),
        &SubagentWorkerResponse { outcome },
    )
    .map_err(|error| format!("failed to write subagent worker response: {error}"))
}

fn read_supervisor_request<R: BufRead>(
    reader: &mut R,
) -> Result<SubagentSupervisorRequest, String> {
    let mut bytes = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|error| format!("failed to read subagent supervisor request: {error}"))?;
        if available.is_empty() {
            return Err("subagent owner closed before sending a complete request".to_string());
        }
        let delimiter = available.iter().position(|byte| *byte == b'\n');
        let count = delimiter.map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(count) > MAX_SUBAGENT_WORKER_REQUEST_BYTES + 1 {
            return Err("subagent supervisor request exceeds its size limit".to_string());
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if delimiter.is_some() {
            bytes.pop();
            break;
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid subagent supervisor request: {error}"))
}

fn read_subagent_supervisor_frame<R: BufRead>(
    reader: &mut R,
    label: &str,
) -> Result<SubagentSupervisorFrame, String> {
    let bytes = read_bounded_supervisor_line(reader, label)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid subagent supervisor {label} frame: {error}"))
}

fn read_bounded_supervisor_line<R: BufRead>(
    reader: &mut R,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|error| {
            format!("failed to read subagent supervisor {label} frame: {error}")
        })?;
        if available.is_empty() {
            return Err(format!(
                "subagent supervisor closed before a complete {label} frame"
            ));
        }
        let delimiter = available.iter().position(|byte| *byte == b'\n');
        let count = delimiter.map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(count) > MAX_SUBAGENT_WORKER_REQUEST_BYTES + 1 {
            return Err(format!(
                "subagent supervisor {label} frame exceeds its size limit"
            ));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if delimiter.is_some() {
            bytes.pop();
            return Ok(bytes);
        }
    }
}

struct SubagentOwnerSignalReader {
    receiver: std::sync::mpsc::Receiver<bool>,
}

impl Read for SubagentOwnerSignalReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        match self.receiver.recv() {
            Ok(true) => {
                buffer[0] = b'!';
                Ok(1)
            }
            Ok(false) | Err(_) => Ok(0),
        }
    }
}

fn start_subagent_owner_protocol_reader<R: BufRead + Send + 'static>(
    mut reader: R,
) -> Result<
    (
        std::sync::mpsc::Receiver<Result<SubagentSupervisorFrame, String>>,
        SubagentOwnerSignalReader,
    ),
    String,
> {
    let (commit_tx, commit_rx) = std::sync::mpsc::sync_channel(1);
    let (signal_tx, signal_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("nib-subagent-owner-protocol".to_string())
        .spawn(move || {
            let commit = read_subagent_supervisor_frame(&mut reader, "COMMIT");
            let valid = commit.is_ok();
            let _ = commit_tx.send(commit);
            if !valid {
                let _ = signal_tx.send(false);
                return;
            }
            let mut byte = [0_u8; 1];
            loop {
                match reader.read(&mut byte) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(0) | Err(_) => {
                        let _ = signal_tx.send(false);
                        return;
                    }
                    Ok(_) => {
                        let _ = signal_tx.send(true);
                        return;
                    }
                }
            }
        })
        .map_err(|error| format!("failed to start subagent owner protocol reader: {error}"))?;
    Ok((
        commit_rx,
        SubagentOwnerSignalReader {
            receiver: signal_rx,
        },
    ))
}

fn write_subagent_supervisor_frame(frame: &SubagentSupervisorFrame) -> Result<(), String> {
    let encoded = serde_json::to_vec(frame)
        .map_err(|error| format!("failed to encode subagent supervisor frame: {error}"))?;
    if encoded.len() > MAX_SUBAGENT_WORKER_REQUEST_BYTES {
        return Err("subagent supervisor response frame exceeds its size limit".to_string());
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&encoded)
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("failed to write subagent supervisor frame: {error}"))
}

fn resolve_nib_executable() -> Result<PathBuf, String> {
    for variable in ["NIB_EXECUTABLE", "CARGO_BIN_EXE_nib"] {
        if let Some(path) = std::env::var_os(variable) {
            let path = PathBuf::from(path);
            let canonical = path.canonicalize().map_err(|error| {
                format!("{variable} does not name an available executable: {error}")
            })?;
            if canonical.is_file() {
                return Ok(canonical);
            }
        }
    }
    let current = std::env::current_exe()
        .map_err(|error| format!("failed to resolve the nib executable: {error}"))?;
    let expected_name = if cfg!(windows) { "nib.exe" } else { "nib" };
    if current.file_name().and_then(|name| name.to_str()) == Some(expected_name) {
        return current
            .canonicalize()
            .map_err(|error| format!("failed to canonicalize the nib executable: {error}"));
    }
    if current.parent().and_then(Path::parent).is_some() {
        let candidate = current
            .parent()
            .and_then(Path::parent)
            .expect("checked parent")
            .join(expected_name);
        if candidate.is_file() {
            return candidate.canonicalize().map_err(|error| {
                format!("failed to canonicalize the test nib executable: {error}")
            });
        }
    }
    Err("could not locate the nib executable for subagent supervision".to_string())
}

#[cfg(test)]
fn collect_spawn_compensation_sync(
    record_cleanup: impl FnOnce() -> Result<(), String>,
    worktree_cleanup: impl FnOnce() -> Result<(), String>,
    owner_lease_cleanup: impl FnOnce(OwnerLeaseCompensation) -> Result<(), String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let record_removed = match record_cleanup() {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!("record compensation failed: {error}"));
            false
        }
    };
    if let Err(error) = worktree_cleanup() {
        errors.push(format!("worktree compensation failed: {error}"));
    }
    let lease_action = if record_removed {
        OwnerLeaseCompensation::Remove
    } else {
        OwnerLeaseCompensation::ReleaseForReconciliation
    };
    if let Err(error) = owner_lease_cleanup(lease_action) {
        errors.push(format!("owner lease compensation failed: {error}"));
    }
    errors
}

fn collect_spawn_compensation_sync_with_audit(
    record_cleanup: impl FnOnce() -> Result<(), String>,
    worktree_cleanup: impl FnOnce() -> Result<(), String>,
    owner_lease_cleanup: impl FnOnce(OwnerLeaseCompensation) -> Result<(), String>,
    audit: PreparedSubagentAudit,
    authority: &SpawnPreparationAuthority,
) -> Vec<String> {
    let mut errors = Vec::new();
    let record_removed = match record_cleanup() {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!("record compensation failed: {error}"));
            false
        }
    };
    if let Err(error) = worktree_cleanup() {
        errors.push(format!("worktree compensation failed: {error}"));
    }
    let lease_action = if record_removed {
        OwnerLeaseCompensation::Remove
    } else {
        OwnerLeaseCompensation::ReleaseForReconciliation
    };
    if let Err(error) = owner_lease_cleanup(lease_action) {
        errors.push(format!("owner lease compensation failed: {error}"));
    }
    if record_removed {
        if let Err(error) = audit.cleanup_with_authority(authority) {
            errors.push(format!("audit preparation compensation failed: {error}"));
        }
    } else {
        audit.disarm();
    }
    errors
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerLeaseCompensation {
    Remove,
    ReleaseForReconciliation,
}

#[cfg(test)]
fn compensate_owner_lease(
    owner_lease: SubagentOwnerLease,
    action: OwnerLeaseCompensation,
) -> Result<(), String> {
    match action {
        OwnerLeaseCompensation::Remove => owner_lease.remove(),
        OwnerLeaseCompensation::ReleaseForReconciliation => {
            owner_lease.release_for_reconciliation()
        }
    }
}

#[cfg(test)]
async fn collect_spawn_compensation_async(
    record_cleanup: impl FnOnce() -> Result<(), String>,
    worktree_cleanup: impl std::future::Future<Output = Result<(), String>>,
    owner_lease_cleanup: impl FnOnce(OwnerLeaseCompensation) -> Result<(), String>,
) -> Vec<String> {
    let (record_removed, record_error) = match record_cleanup() {
        Ok(()) => (true, None),
        Err(error) => (false, Some(error)),
    };
    let lease_action = if record_removed {
        OwnerLeaseCompensation::Remove
    } else {
        OwnerLeaseCompensation::ReleaseForReconciliation
    };
    // Lease compensation must run before the first suspension point. The
    // worktree cleanup future continues its blocking cleanup task if this
    // caller is dropped while awaiting it.
    let owner_lease_error = owner_lease_cleanup(lease_action).err();
    let worktree_error = worktree_cleanup.await.err();
    let mut errors = Vec::new();
    if let Some(error) = record_error {
        errors.push(format!("record compensation failed: {error}"));
    }
    if let Some(error) = worktree_error {
        errors.push(format!("worktree compensation failed: {error}"));
    }
    if let Some(error) = owner_lease_error {
        errors.push(format!("owner lease compensation failed: {error}"));
    }
    errors
}

async fn collect_spawn_compensation_async_with_audit(
    record_cleanup: impl FnOnce() -> Result<(), String>,
    worktree_cleanup: impl std::future::Future<Output = Result<(), String>>,
    owner_lease_cleanup: impl FnOnce(OwnerLeaseCompensation) -> Result<(), String>,
    audit: PreparedSubagentAudit,
    authority: &SpawnPreparationAuthority,
) -> Vec<String> {
    let (record_removed, record_error) = match record_cleanup() {
        Ok(()) => (true, None),
        Err(error) => (false, Some(error)),
    };
    let lease_action = if record_removed {
        OwnerLeaseCompensation::Remove
    } else {
        OwnerLeaseCompensation::ReleaseForReconciliation
    };
    let owner_lease_error = owner_lease_cleanup(lease_action).err();
    let audit_error = if record_removed {
        audit.cleanup_with_authority(authority).err()
    } else {
        audit.disarm();
        None
    };
    let worktree_error = worktree_cleanup.await.err();
    let mut errors = Vec::new();
    if let Some(error) = record_error {
        errors.push(format!("record compensation failed: {error}"));
    }
    if let Some(error) = worktree_error {
        errors.push(format!("worktree compensation failed: {error}"));
    }
    if let Some(error) = owner_lease_error {
        errors.push(format!("owner lease compensation failed: {error}"));
    }
    if let Some(error) = audit_error {
        errors.push(format!("audit preparation compensation failed: {error}"));
    }
    errors
}

#[cfg(test)]
fn cleanup_precommit_worktree_sync(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
) -> Result<(), String> {
    crate::sandbox::worktree::Worktree::remove_precommit(project_root, worktree)
}

#[cfg(test)]
async fn cleanup_precommit_worktree(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
) -> Result<(), String> {
    let project_root = project_root.to_path_buf();
    let worktree = worktree.clone();
    tokio::task::spawn_blocking(move || {
        crate::sandbox::worktree::Worktree::remove_precommit_bounded_sync(
            &project_root,
            &worktree,
            SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT,
        )
    })
    .await
    .map_err(|error| format!("subagent worktree cleanup worker failed: {error}"))?
}

fn cleanup_precommit_worktree_sync_with_authority(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    #[cfg(test)]
    if consume_spawn_failure(&SPAWN_WORKTREE_CLEANUP_FAILURES) {
        return Err("injected subagent worktree cleanup failure".to_string());
    }
    let preparation = worktree.preparation_authority();
    let deadline = authority.operation_deadline();
    crate::sandbox::worktree::Worktree::cleanup_preparation_authority_until_with_guard(
        project_root,
        &preparation,
        deadline,
        SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT,
        || authority.verify_until(deadline),
    )
}

async fn cleanup_precommit_worktree_with_authority(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
    authority: std::sync::Arc<SpawnPreparationAuthority>,
) -> Result<(), String> {
    let project_root = project_root.to_path_buf();
    let worktree = worktree.clone();
    tokio::task::spawn_blocking(move || {
        cleanup_precommit_worktree_sync_with_authority(&project_root, &worktree, &authority)
    })
    .await
    .map_err(|error| format!("subagent worktree cleanup worker failed: {error}"))?
}

fn compensate_owner_lease_with_authority(
    owner_lease: SubagentOwnerLease,
    action: OwnerLeaseCompensation,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    #[cfg(test)]
    if consume_spawn_failure(&SPAWN_OWNER_CLEANUP_FAILURES) {
        return Err("injected subagent owner cleanup failure".to_string());
    }
    match action {
        OwnerLeaseCompensation::Remove => {
            let deadline = authority.operation_deadline();
            owner_lease.remove_until_with_guard(Some(deadline), || authority.verify_until(deadline))
        }
        OwnerLeaseCompensation::ReleaseForReconciliation => {
            let verification = owner_lease.verify_pair();
            drop(owner_lease);
            authority.verify_until(authority.operation_deadline())?;
            verification
        }
    }
}

#[cfg(test)]
fn cleanup_precommit_record(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
) -> Result<(), String> {
    cleanup_precommit_record_with_hook(project_root, attempted_record, expected_publication, || {
        Ok(())
    })
}

#[cfg(test)]
fn cleanup_record_after_registration_failure(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    publication: &InitialSubagentRecordPublication,
) -> Result<(), String> {
    let exact_publication = publication
        .receipt
        .exact_identity
        .then_some(&publication.receipt.file);
    cleanup_precommit_record(project_root, attempted_record, exact_publication)
}

#[cfg(test)]
fn cleanup_record_after_publication_failure(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    error: &InitialSubagentRecordPublicationError,
) -> Result<(), String> {
    if !error.publication_attempted {
        return Ok(());
    }
    let exact_publication = error
        .receipt
        .as_ref()
        .filter(|receipt| receipt.exact_identity)
        .map(|receipt| &receipt.file);
    cleanup_precommit_record(project_root, attempted_record, exact_publication)
}

fn cleanup_record_after_publication_failure_locked(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    error: &InitialSubagentRecordPublicationError,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    if !error.publication_attempted {
        return authority.verify_until(authority.operation_deadline());
    }
    let exact = error
        .receipt
        .as_ref()
        .filter(|receipt| receipt.exact_identity)
        .map(|receipt| &receipt.file);
    cleanup_precommit_record_locked(project_root, attempted_record, exact, authority)
}

fn cleanup_record_after_registration_failure_locked(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    publication: &InitialSubagentRecordPublication,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    let exact = publication
        .receipt
        .exact_identity
        .then_some(&publication.receipt.file);
    cleanup_precommit_record_locked(project_root, attempted_record, exact, authority)
}

fn cleanup_precommit_record_locked(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    let deadline = authority.operation_deadline();
    authority.verify_until(deadline)?;
    let path = record_path(project_root, &attempted_record.id)?;
    let quarantine = authority.records.deterministic_artifact_path(
        &path,
        ".nib-subagent-precommit-delete-",
        ".quarantine",
    )?;
    let canonical_exists = authority.records.path_exists(&path)?;
    let quarantine_exists = authority.records.path_exists(&quarantine)?;
    if !canonical_exists && !quarantine_exists {
        return authority.verify_until(deadline);
    }
    let expected = expected_publication.ok_or_else(|| {
        format!(
            "exact precommit subagent record publication identity is unavailable; preserved {}",
            path.display()
        )
    })?;
    let encoded = serde_json::to_vec_pretty(attempted_record)
        .map_err(|error| format!("failed to encode precommit subagent record: {error}"))?;
    let mut guard = || {
        authority.verify_until(deadline)?;
        verify_open_subagent_record_bytes(expected, &encoded)
    };
    if !canonical_exists {
        let quarantined = authority.records.open_read_write(&quarantine)?;
        if !crate::daemons::state::same_open_file_identity(expected, &quarantined)? {
            return Err(format!(
                "precommit subagent deletion quarantine has an unexpected identity; preserved {}",
                quarantine.display()
            ));
        }
        verify_open_subagent_record_bytes(&quarantined, &encoded)?;
        authority
            .records
            .remove_visible_file_if_matches_direct_with_guard(
                &quarantine,
                &quarantined,
                &mut guard,
            )?;
    } else {
        authority.records.verify_file_identity(&path, expected)?;
        verify_open_subagent_record_bytes(expected, &encoded)?;
        authority.records.remove_file_if_matches_with_guard(
            &path,
            expected,
            ".nib-subagent-precommit-delete-",
            &mut guard,
        )?;
    }
    authority.verify_until(deadline)
}

#[cfg(test)]
fn cleanup_precommit_record_with_hook(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    before_quarantine: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    cleanup_precommit_record_with_timeout_and_hook(
        project_root,
        attempted_record,
        expected_publication,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        before_quarantine,
    )
}

#[cfg(test)]
fn cleanup_precommit_record_with_timeout_and_hook(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    timeout: Duration,
    before_quarantine: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    cleanup_precommit_record_with_timeout_and_hooks(
        project_root,
        attempted_record,
        expected_publication,
        timeout,
        before_quarantine,
        || Ok(()),
    )
}

#[cfg(test)]
fn cleanup_precommit_record_with_timeout_and_hooks(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    timeout: Duration,
    before_quarantine: impl FnOnce() -> Result<(), String>,
    mut before_namespace_step: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let project_root = canonical_project_root(project_root)?;
    let attempted_bytes = serde_json::to_vec_pretty(attempted_record)
        .map_err(|error| format!("failed to encode precommit subagent record: {error}"))?;
    let records = records_dir(&project_root);
    let metadata = match std::fs::symlink_metadata(&records) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "failed to inspect precommit subagent record directory: {error}"
            ));
        }
    };
    validate_records_directory(&project_root, &records, &metadata)?;
    let path = record_path(&project_root, &attempted_record.id)?;
    with_subagent_record_lock_in_timeout(
        &project_root,
        &attempted_record.id,
        &records,
        timeout,
        |directory, deadline| {
            let quarantine = directory.deterministic_artifact_path(
                &path,
                ".nib-subagent-precommit-delete-",
                ".quarantine",
            )?;
            let canonical_exists = directory.path_exists(&path)?;
            let quarantine_exists = directory.path_exists(&quarantine)?;
            if !canonical_exists && !quarantine_exists {
                return Ok(());
            }
            let expected_publication = expected_publication.ok_or_else(|| {
                format!(
                    "exact precommit subagent record publication identity is unavailable; preserved {}",
                    path.display()
                )
            })?;
            if !canonical_exists {
                let quarantined = directory.open_read_write(&quarantine)?;
                if !crate::daemons::state::same_open_file_identity(
                    expected_publication,
                    &quarantined,
                )? {
                    return Err(format!(
                        "precommit subagent deletion quarantine has an unexpected identity; preserved {}",
                        quarantine.display()
                    ));
                }
                verify_open_subagent_record_bytes(&quarantined, &attempted_bytes)?;
                return directory.remove_visible_file_if_matches_direct_with_guard(
                    &quarantine,
                    &quarantined,
                    || {
                        before_namespace_step()?;
                        ensure_subagent_reconciliation_deadline(Some(deadline))?;
                        verify_open_subagent_record_bytes(&quarantined, &attempted_bytes)
                    },
                );
            }
            directory
                .verify_file_identity(&path, expected_publication)
                .map_err(|error| {
                    format!(
                        "precommit subagent record no longer has the attempted publication identity; preserved {}: {error}",
                        path.display()
                    )
                })?;
            let opened = read_opened_subagent_record_in(directory, &path)
                .map_err(|error| format!("precommit subagent record is unsafe: {error}"))?;
            let attempted =
                serde_json::to_value(attempted_record).map_err(|error| error.to_string())?;
            let durable =
                serde_json::to_value(&opened.record).map_err(|error| error.to_string())?;
            if durable != attempted {
                return Err(format!(
                    "precommit subagent record no longer matches the attempted generation; preserved {}",
                    path.display()
                ));
            }
            directory
                .verify_file_identity(&path, expected_publication)
                .map_err(|error| {
                    format!(
                        "precommit subagent record publication identity changed before deletion; preserved {}: {error}",
                        path.display()
                    )
                })?;
            verify_open_subagent_record_bytes(expected_publication, &attempted_bytes)?;
            let mut before_quarantine = Some(before_quarantine);
            directory.remove_file_if_matches_with_guard(
                &path,
                expected_publication,
                ".nib-subagent-precommit-delete-",
                || {
                    if let Some(before_quarantine) = before_quarantine.take() {
                        before_quarantine()?;
                    }
                    before_namespace_step()?;
                    ensure_subagent_reconciliation_deadline(Some(deadline))?;
                    verify_open_subagent_record_bytes(expected_publication, &attempted_bytes)
                },
            )
        },
    )
}

fn verify_open_subagent_record_bytes(file: &File, expected: &[u8]) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect retained subagent record: {error}"))?;
    if metadata.len() != expected.len() as u64 {
        return Err("retained subagent record bytes changed; preserving it".to_string());
    }
    let mut file = file
        .try_clone()
        .map_err(|error| format!("failed to clone retained subagent record: {error}"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to seek retained subagent record: {error}"))?;
    let mut actual = Vec::with_capacity(expected.len().saturating_add(1));
    file.take(expected.len() as u64 + 1)
        .read_to_end(&mut actual)
        .map_err(|error| format!("failed to read retained subagent record: {error}"))?;
    if actual != expected {
        return Err("retained subagent record bytes changed; preserving it".to_string());
    }
    Ok(())
}

pub async fn merge_subagent_worktree(_args: &Value, _project_root: &Path) -> Result<Value, String> {
    Err("merge_subagent_worktree must be executed through ToolExecutor so verification is sandboxed and audited".to_string())
}

pub(crate) async fn merge_verified_subagent_worktree(
    args: &Value,
    project_root: &Path,
    evidence: VerificationEvidence,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<Value, String> {
    let project_root = canonical_project_root(project_root)?;
    let subagent_id = args
        .get("subagent_id")
        .and_then(|value| value.as_str())
        .ok_or("missing subagent_id")?;
    let verification_command = args
        .get("verification_command")
        .and_then(|value| value.as_str())
        .filter(|command| !command.trim().is_empty())
        .ok_or("verification_command is required before merge")?;
    let verified_commit = evidence
        .snapshot_commit
        .as_deref()
        .filter(|commit| valid_git_object_id(commit))
        .ok_or("verification evidence is missing a valid immutable snapshot commit")?
        .to_string();
    let _merge_lock = RepositoryMergeLock::acquire(&project_root, cancellation).await?;
    let OpenedSubagentRecord {
        mut record,
        file: mut record_file,
    } = get_opened_subagent_record(&project_root, subagent_id)?;
    if !matches!(
        record.status.as_str(),
        "completed" | "verification_failed" | MERGE_FAILED_STATUS | MERGE_PENDING_STATUS
    ) {
        return Err(format!(
            "subagent {} is not ready to merge (status: {})",
            record.id, record.status
        ));
    }

    if record.status == MERGE_PENDING_STATUS {
        let mut intent = pending_merge_intent(&record)?;
        require_record_branch_oid(&record, &intent.branch_commit)?;
        if let Err(error) =
            recover_interrupted_merge(&project_root, &mut record, &mut record_file, &intent).await
        {
            return persist_pending_merge_failure(
                &project_root,
                &mut record,
                &mut record_file,
                error,
            );
        }
        intent = pending_merge_intent(&record)?;
        if intent.branch_commit != verified_commit {
            return persist_pending_merge_failure(
                &project_root,
                &mut record,
                &mut record_file,
                "verification evidence does not match the pending immutable child commit"
                    .to_string(),
            );
        }
        if intent.verification_command != verification_command {
            return Err("verification command does not match the pending merge intent".to_string());
        }
        let verification_root = pending_verification_root(&project_root, &record)?;
        validate_verification_evidence(
            verification_command,
            &evidence,
            &verification_root,
            verification_root == project_root,
        )?;
        record.verification = Some(evidence.clone());
        record.updated_at = Utc::now();
        if !evidence.success {
            let error = format!(
                "verification command failed while reconciling pending merge: {}",
                evidence
                    .error
                    .as_deref()
                    .unwrap_or("unknown verification error")
            );
            return persist_pending_merge_failure(
                &project_root,
                &mut record,
                &mut record_file,
                error,
            );
        }
        if verification_root != project_root {
            if let Err(error) = ensure_child_snapshot_unchanged(
                &verification_root,
                &record.branch,
                &intent.branch_commit,
                "after pending verification",
            )
            .await
            {
                return persist_pending_merge_failure(
                    &project_root,
                    &mut record,
                    &mut record_file,
                    error,
                );
            }
        }
        return reconcile_pending_merge(
            &project_root,
            &mut record,
            &mut record_file,
            &intent,
            &evidence,
        )
        .await;
    }

    require_record_branch_oid(&record, &verified_commit)?;
    validate_record_worktree(&project_root, &record)?;
    validate_verification_evidence(
        verification_command,
        &evidence,
        &record.worktree_path,
        false,
    )?;
    record.verification = Some(evidence.clone());
    record.updated_at = Utc::now();
    if !evidence.success {
        let error = format!(
            "verification command failed: {}",
            evidence
                .error
                .as_deref()
                .unwrap_or("unknown verification error")
        );
        record.status = "verification_failed".to_string();
        record.error = Some(error.clone());
        persist_subagent_record_revision(&project_root, &record, &mut record_file)?;
        return Err(error);
    }
    if let Err(error) = ensure_child_snapshot_unchanged(
        &record.worktree_path,
        &record.branch,
        &verified_commit,
        "after verification",
    )
    .await
    {
        record.status = "verification_failed".to_string();
        record.error = Some(error.clone());
        persist_subagent_record_revision(&project_root, &record, &mut record_file)?;
        return Err(error);
    }
    record.status = "completed".to_string();
    record.error = None;
    persist_subagent_record_revision(&project_root, &record, &mut record_file)?;

    if let Err(error) = ensure_parent_clean(&project_root).await {
        return persist_merge_failure(&project_root, &mut record, &mut record_file, error);
    }
    let parent_head = match ensure_parent_clean(&project_root).await {
        Ok(head) => head,
        Err(error) => {
            return persist_merge_failure(&project_root, &mut record, &mut record_file, error);
        }
    };
    begin_pending_merge(
        &mut record,
        verification_command,
        &verified_commit,
        &parent_head,
    );
    persist_subagent_record_revision(&project_root, &record, &mut record_file)?;

    let intent = pending_merge_intent(&record)?;
    reconcile_pending_merge(
        &project_root,
        &mut record,
        &mut record_file,
        &intent,
        &evidence,
    )
    .await
}

pub fn send_message_to_subagent(args: &Value, project_root: &Path) -> Result<Value, String> {
    let subagent_id = args
        .get("subagent_id")
        .and_then(|value| value.as_str())
        .ok_or("missing subagent_id")?;
    let message = args
        .get("message")
        .and_then(|value| value.as_str())
        .filter(|message| !message.trim().is_empty())
        .ok_or("missing message")?;
    let record = get_subagent_record_internal(project_root, subagent_id)?;
    validate_record_worktree(&canonical_project_root(project_root)?, &record)?;
    let store = crate::session::SessionStore::for_project(&record.worktree_path)?;
    store
        .try_append_message(&record.child_session_id, "user", message)
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "status": "sent",
        "subagent_id": record.id,
        "child_session_id": record.child_session_id,
    }))
}

pub fn list_subagents(project_root: &Path) -> Result<Vec<Value>, String> {
    let project_root = canonical_project_root(project_root)?;
    let stable_directory = ensure_records_directory_capability_until(&project_root, None)?;
    reconcile_spawn_preparations(&project_root, &stable_directory)?;
    let mut record_ids = Vec::new();
    stable_directory.for_each_entry_bounded(
        MAX_SUBAGENT_DIRECTORY_ENTRIES,
        MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
        |name| {
            if name == std::ffi::OsStr::new(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT) {
                return Ok(());
            }
            let path = Path::new(&name);
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                return Ok(());
            }
            let id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .filter(|id| is_valid_subagent_id(id))
                .ok_or_else(|| "subagent record has an invalid filename".to_string())?;
            if record_ids.len() >= MAX_SUBAGENT_RECORDS {
                return Err(format!(
                    "subagent records exceed the {MAX_SUBAGENT_RECORDS}-record limit"
                ));
            }
            record_ids.push(id.to_string());
            Ok(())
        },
    )?;
    let mut records = Vec::new();
    let mut running_leases = std::collections::HashSet::new();
    for id in record_ids {
        let record = reconcile_subagent_ownership(&project_root, &id)
            .map_err(|error| format!("subagent record must be a regular file: {error}"))?;
        if record.status == "running" {
            if let Some(lease_id) = &record.owner_lease {
                running_leases.insert(lease_id.clone());
            }
        }
        sync_subagent_task_manager(&record);
        records.push(json!(public_subagent_record(record)));
    }
    sweep_owner_lease_artifacts(&project_root, &running_leases)?;
    records.sort_by(|left, right| {
        left["created_at"]
            .as_str()
            .cmp(&right["created_at"].as_str())
    });
    Ok(records)
}

fn sweep_owner_lease_artifacts(
    project_root: &Path,
    running_leases: &std::collections::HashSet<String>,
) -> Result<(), String> {
    sweep_owner_lease_artifacts_with_timeout_and_guard(
        project_root,
        running_leases,
        OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT,
        || Ok(()),
    )
}

fn sweep_owner_lease_artifacts_with_timeout_and_guard(
    project_root: &Path,
    running_leases: &std::collections::HashSet<String>,
    timeout: Duration,
    mut before_namespace_step: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let nib_path = project_root.join(".nib");
    match std::fs::symlink_metadata(&nib_path) {
        Ok(metadata)
            if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() =>
        {
            return Err(format!(
                "subagent owner lease namespace is unsafe: {}",
                nib_path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "failed to inspect subagent owner lease namespace: {error}"
            ));
        }
    }
    with_bounded_delegation_lock_in(
        &owner_lease_namespace_lock_path(project_root),
        &nib_path,
        timeout,
        |anchor_directory, deadline| {
            let mut namespace_guard = || {
                before_namespace_step()?;
                ensure_subagent_reconciliation_deadline(Some(deadline))
            };
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
            let mut lease_ids = std::collections::HashSet::new();
            let mut retained_anchor_quarantines = Vec::new();
            anchor_directory.for_each_entry_bounded(
                MAX_SUBAGENT_DIRECTORY_ENTRIES,
                MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
                |name| {
                    if [
                        ".nib-subagent-owner-anchor-delete-",
                        ".nib-subagent-owner-create-anchor-delete-",
                    ]
                    .iter()
                    .any(|prefix| exact_deletion_quarantine_name(&name, prefix))
                    {
                        retained_anchor_quarantines.push(nib_path.join(&name));
                        return Ok(());
                    }
                    if name.to_str().is_some_and(|name| {
                        name.starts_with(".nib-subagent-owner-anchor-delete-")
                            || name.starts_with(".nib-subagent-owner-create-anchor-delete-")
                    }) {
                        return Err(
                            "subagent owner anchor namespace contains an invalid deletion quarantine"
                                .to_string(),
                        );
                    }
                    if !name
                        .as_encoded_bytes()
                        .starts_with(OWNER_LEASE_ANCHOR_PREFIX.as_bytes())
                    {
                        return Ok(());
                    }
                    let Some(name) = name.to_str() else {
                        return Err(
                            "subagent owner anchor namespace contains a non-UTF-8 filename"
                                .to_string(),
                        );
                    };
                    let suffix = name
                        .strip_prefix(OWNER_LEASE_ANCHOR_PREFIX)
                        .expect("byte prefix was checked");
                    let lease_id = suffix.strip_suffix(OWNER_LEASE_ANCHOR_SUFFIX).ok_or_else(|| {
                        "subagent owner anchor namespace contains an invalid anchor filename"
                            .to_string()
                    })?;
                    let parsed = uuid::Uuid::parse_str(lease_id).map_err(|_| {
                        "subagent owner anchor namespace contains an invalid anchor filename"
                            .to_string()
                    })?;
                    if parsed.to_string() != lease_id {
                        return Err(
                            "subagent owner anchor namespace contains a non-canonical anchor filename"
                                .to_string(),
                        );
                    }
                    lease_ids.insert(lease_id.to_string());
                    Ok(())
                },
            )?;
            let mut retained_visible_quarantines = Vec::new();
            if let Some(visible_directory) = &visible_directory {
                visible_directory.for_each_entry_bounded(
                    MAX_SUBAGENT_DIRECTORY_ENTRIES,
                    MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
                    |name| {
                        if [
                            ".nib-subagent-owner-visible-delete-",
                            ".nib-subagent-owner-create-visible-delete-",
                        ]
                        .iter()
                        .any(|prefix| exact_deletion_quarantine_name(&name, prefix))
                        {
                            retained_visible_quarantines.push(visible_root.join(&name));
                            return Ok(());
                        }
                        let Some(name) = name.to_str() else {
                            return Err(
                                "subagent owner lease directory contains a non-UTF-8 filename"
                                    .to_string(),
                            );
                        };
                        if name.starts_with(".nib-subagent-owner-visible-delete-")
                            || name.starts_with(".nib-subagent-owner-create-visible-delete-")
                        {
                            return Err(
                                "subagent owner lease directory contains an invalid deletion quarantine"
                                    .to_string(),
                            );
                        }
                        let Some(lease_id) = name.strip_suffix(OWNER_LEASE_SUFFIX) else {
                            return Ok(());
                        };
                        let parsed = uuid::Uuid::parse_str(lease_id).map_err(|_| {
                            "subagent owner lease directory contains an invalid lease filename"
                                .to_string()
                        })?;
                        if parsed.to_string() != lease_id {
                            return Err(
                                "subagent owner lease directory contains a non-canonical lease filename"
                                    .to_string(),
                            );
                        }
                        lease_ids.insert(lease_id.to_string());
                        if lease_ids.len() > MAX_SUBAGENT_DIRECTORY_ENTRIES {
                            return Err(format!(
                                "subagent owner artifacts exceed the {MAX_SUBAGENT_DIRECTORY_ENTRIES}-entry limit"
                            ));
                        }
                        Ok(())
                    },
                )?;
            }

            let mut lease_ids = lease_ids.into_iter().collect::<Vec<_>>();
            lease_ids.sort();
            for lease_id in lease_ids {
                let record_is_running = running_leases.contains(&lease_id);
                let visible = owner_lease_path(project_root, &lease_id)?;
                let anchor = owner_lease_anchor_path(project_root, &lease_id)?;
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
                let Some(authority) = owner_deletion_authority(&visible_state, &anchor_state)?
                else {
                    continue;
                };
                let has_visible =
                    visible_state.canonical.is_some() || visible_state.quarantine.is_some();
                let has_anchor =
                    anchor_state.canonical.is_some() || anchor_state.quarantine.is_some();
                match authority.try_lock() {
                    Ok(()) if record_is_running => {
                        return Err(format!(
                            "running subagent owner lease is unlocked; artifacts were preserved: {lease_id}"
                        ));
                    }
                    Ok(()) => {
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
                    }
                    Err(std::fs::TryLockError::WouldBlock)
                        if record_is_running || (has_visible && has_anchor) => {}
                    Err(std::fs::TryLockError::WouldBlock) => {
                        let description = if has_anchor {
                            "live subagent owner anchor has no visible lease"
                        } else {
                            "live visible subagent owner lease has no anchor"
                        };
                        return Err(format!("{description}; artifact was preserved: {lease_id}"));
                    }
                    Err(std::fs::TryLockError::Error(error)) => {
                        return Err(format!(
                            "failed to inspect subagent owner lease artifact: {error}"
                        ));
                    }
                }
            }
            for (directory, quarantine) in retained_visible_quarantines
                .iter()
                .map(|path| (visible_directory.as_ref(), path))
                .chain(
                    retained_anchor_quarantines
                        .iter()
                        .map(|path| (Some(anchor_directory), path)),
                )
            {
                let Some(directory) = directory else {
                    continue;
                };
                if !directory.path_exists(quarantine)? {
                    continue;
                }
                let file = directory.open_read_write(quarantine)?;
                match file.try_lock() {
                    Ok(()) => directory.remove_visible_file_if_matches_direct_with_guard(
                        quarantine,
                        &file,
                        &mut namespace_guard,
                    )?,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        return Err(format!(
                            "unassociated subagent owner deletion quarantine is live and was preserved: {}",
                            quarantine.display()
                        ));
                    }
                    Err(std::fs::TryLockError::Error(error)) => {
                        return Err(format!(
                            "failed to inspect subagent owner deletion quarantine {}: {error}",
                            quarantine.display()
                        ));
                    }
                }
            }
            ensure_subagent_reconciliation_deadline(Some(deadline))?;
            Ok(())
        },
    )
}

pub fn get_subagent_record(project_root: &Path, id: &str) -> Result<SubagentRecord, String> {
    get_subagent_record_internal(project_root, id).map(public_subagent_record)
}

fn get_subagent_record_internal(project_root: &Path, id: &str) -> Result<SubagentRecord, String> {
    let project_root = canonical_project_root(project_root)?;
    let records = ensure_records_directory_capability_until(&project_root, None)?;
    reconcile_spawn_preparations(&project_root, &records)?;
    let record = reconcile_subagent_ownership(&project_root, id)?;
    records.verify_visible()?;
    sync_subagent_task_manager(&record);
    Ok(record)
}

fn public_subagent_record(mut record: SubagentRecord) -> SubagentRecord {
    record.execution_generation = None;
    record.owner_lease = None;
    record.result = record
        .result
        .take()
        .and_then(project_public_subagent_result);
    record
}

fn project_public_subagent_result(result: Value) -> Option<Value> {
    let Value::Object(mut result) = result else {
        return Some(result);
    };
    let nested_result = result
        .remove("subagent_result")
        .map(|nested| project_public_subagent_result(nested).unwrap_or(Value::Null));
    result.retain(|key, _| {
        !key.starts_with('_')
            && !matches!(
                key.as_str(),
                "cleanup_verified"
                    | "cleanup_proof"
                    | "cleanup_scope"
                    | "cleanup_unverified"
                    | "owner_lease_cleanup"
                    | "launch_abort_verified"
                    | "workload_never_launched"
                    | "launch_abort_proof"
                    | "ownership_reconciliation"
                    | "process_scope"
            )
    });
    if let Some(nested_result) = nested_result {
        result.insert("subagent_result".to_string(), nested_result);
    }
    (!result.is_empty()).then_some(Value::Object(result))
}

fn sync_subagent_task_manager(record: &SubagentRecord) {
    let public_result = record
        .result
        .clone()
        .and_then(project_public_subagent_result);
    match record.status.as_str() {
        "completed" => crate::daemons::task::TASK_MANAGER.complete(&record.id, public_result),
        "failed" => crate::daemons::task::TASK_MANAGER.fail(
            &record.id,
            record
                .error
                .clone()
                .unwrap_or_else(|| "subagent failed".to_string()),
            public_result,
        ),
        "cancelled"
            if crate::daemons::task::TASK_MANAGER
                .get_status(&record.id)
                .as_deref()
                == Some("running") =>
        {
            let _ = crate::daemons::task::TASK_MANAGER.cancel(&record.id);
        }
        _ => {}
    }
}

fn get_opened_subagent_record(
    project_root: &Path,
    id: &str,
) -> Result<OpenedSubagentRecord, String> {
    reconcile_subagent_ownership(project_root, id)?;
    let records = ensure_records_directory(project_root)?;
    let directory = crate::daemons::state::StableDirectory::open(&records)?;
    let path = record_path(project_root, id)?;
    read_opened_subagent_record_in(&directory, &path)
}

fn get_subagent_record_unreconciled(
    project_root: &Path,
    id: &str,
) -> Result<SubagentRecord, String> {
    let directory = records_dir(project_root);
    let metadata = std::fs::symlink_metadata(&directory)
        .map_err(|error| format!("subagent records are unavailable: {error}"))?;
    validate_records_directory(project_root, &directory, &metadata)?;
    let path = record_path(project_root, id)?;
    read_subagent_record(&path)
}

fn subagent_audit_session_id(record: &SubagentRecord) -> &str {
    record
        .parent_session_id
        .as_deref()
        .unwrap_or(&record.child_session_id)
}

fn resolve_legacy_subagent_audit_target(
    project_root: &Path,
    record: &SubagentRecord,
    deadline: Option<Instant>,
) -> Result<SubagentAuditTarget, String> {
    let store = match deadline {
        Some(deadline) => crate::session::SessionStore::for_existing_project_with_lock_deadline(
            project_root,
            deadline,
        )?,
        None => crate::session::SessionStore::for_project(project_root)?,
    };
    ensure_subagent_reconciliation_deadline(deadline)?;
    let session_id = subagent_audit_session_id(record);
    if deadline.is_none()
        && store
            .load_result(session_id)
            .map_err(|error| error.to_string())?
            .is_none()
    {
        store
            .try_create_session_with_id(session_id)
            .map_err(|error| error.to_string())?;
    }
    ensure_subagent_reconciliation_deadline(deadline)?;
    Ok(SubagentAuditTarget {
        sessions_dir: store
            .sessions_dir()
            .canonicalize()
            .map_err(|error| error.to_string())?,
        directory_identity: store
            .persistent_directory_identity()
            .map_err(|error| error.to_string())?,
    })
}

fn subagent_reconciliation_id(
    subagent_id: &str,
    execution_generation: u64,
    lease_id: &str,
) -> Result<String, String> {
    if !is_valid_subagent_id(subagent_id) {
        return Err("invalid subagent id for ownership reconciliation".to_string());
    }
    validate_execution_ownership(execution_generation, lease_id)?;
    Ok(format!(
        "nib.subagent-ownership-reconciliation.v1|{}:{subagent_id}|{execution_generation}|{}:{lease_id}",
        subagent_id.len(),
        lease_id.len(),
    ))
}

fn ownership_reconciliation_evidence(
    record: &SubagentRecord,
) -> Result<Option<(Value, bool)>, String> {
    let Some(result) = process_scope_retirement_result(record) else {
        return Ok(None);
    };
    let Some(original) = result.get("ownership_reconciliation") else {
        return Ok(None);
    };
    let mut evidence = original
        .as_object()
        .cloned()
        .ok_or("terminal ownership reconciliation is not an object")?;
    let execution_generation = record.execution_generation.ok_or_else(|| {
        format!(
            "terminal subagent {} has no reconciliation execution generation",
            record.id
        )
    })?;
    let lease_id = record.owner_lease.as_deref().ok_or_else(|| {
        format!(
            "terminal subagent {} has no reconciliation owner lease",
            record.id
        )
    })?;
    let expected_id = subagent_reconciliation_id(&record.id, execution_generation, lease_id)?;
    if evidence.get("subagent_id").and_then(Value::as_str) != Some(record.id.as_str())
        || evidence.get("execution_generation").and_then(Value::as_u64)
            != Some(execution_generation)
        || evidence.get("owner_lease").and_then(Value::as_str) != Some(lease_id)
        || !retirement_terminal_status_matches(
            &record.status,
            evidence.get("terminal_status").and_then(Value::as_str),
        )
    {
        return Err(
            "terminal ownership reconciliation does not match subagent execution ownership"
                .to_string(),
        );
    }
    let legacy = match evidence.get("reconciliation_id") {
        Some(Value::String(observed)) if observed == &expected_id => false,
        Some(_) => {
            return Err(
                "terminal ownership reconciliation has an invalid reconciliation identity"
                    .to_string(),
            );
        }
        None => true,
    };
    evidence.insert("reconciliation_id".to_string(), Value::String(expected_id));
    let (_, authority) = terminal_process_scope_authority(record)?.ok_or_else(|| {
        "terminal ownership reconciliation has no validated process authority".to_string()
    })?;
    let terminal_status = evidence
        .get("terminal_status")
        .and_then(Value::as_str)
        .ok_or("terminal ownership reconciliation has no terminal status")?;
    let expected_outcome = match (&authority, terminal_status) {
        (TerminalProcessScopeAuthority::Cleanup(_), "cancelled") => {
            "cancelled_after_verified_cleanup"
        }
        (TerminalProcessScopeAuthority::Cleanup(_), "failed") => {
            "supervisor_result_lost_after_verified_cleanup"
        }
        (TerminalProcessScopeAuthority::LaunchAbort(_), "failed") => {
            "supervisor_lost_before_gated_workload_launch"
        }
        _ => {
            return Err(
                "terminal ownership reconciliation has an invalid terminal status".to_string(),
            );
        }
    };
    if evidence.get("outcome").and_then(Value::as_str) != Some(expected_outcome)
        || evidence
            .get("reconciled_at")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .is_none()
        || !matches!(
            evidence.get("manager_status"),
            None | Some(Value::Null) | Some(Value::String(_))
        )
        || matches!(authority, TerminalProcessScopeAuthority::Cleanup(_))
            && evidence.get("cleanup_scope").and_then(Value::as_str)
                != Some("foreground_descendant_process_tree")
    {
        return Err("terminal ownership reconciliation has invalid stable evidence".to_string());
    }
    Ok(Some((Value::Object(evidence), legacy)))
}

fn open_subagent_audit_store_until(
    record: &SubagentRecord,
    deadline: Option<Instant>,
) -> Result<crate::session::SessionStore, String> {
    let target = subagent_audit_target(record)?.ok_or_else(|| {
        format!(
            "subagent {} has no pinned ownership audit destination",
            record.id
        )
    })?;
    let deadline = deadline.unwrap_or_else(|| {
        Instant::now()
            .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
            .unwrap_or_else(Instant::now)
    });
    crate::session::SessionStore::at_existing_dir_with_identity_until(
        &target.sessions_dir,
        target.directory_identity,
        deadline,
    )
}

fn subagent_audit_target(record: &SubagentRecord) -> Result<Option<SubagentAuditTarget>, String> {
    let Some(target) = process_scope_retirement_result(record)
        .and_then(|result| result.get(OWNERSHIP_AUDIT_TARGET_KEY))
    else {
        return Ok(None);
    };
    serde_json::from_value(target.clone())
        .map(Some)
        .map_err(|error| format!("subagent ownership audit target is invalid: {error}"))
}

fn set_subagent_audit_target(
    record: &mut SubagentRecord,
    target: SubagentAuditTarget,
) -> Result<(), String> {
    if record.result.is_none() {
        record.result = Some(Value::Object(serde_json::Map::new()));
    }
    process_scope_retirement_result_mut(record)
        .ok_or("subagent result cannot retain its ownership audit target")?
        .insert(
            OWNERSHIP_AUDIT_TARGET_KEY.to_string(),
            serde_json::to_value(target).map_err(|error| error.to_string())?,
        );
    Ok(())
}

fn legacy_running_reconciliation_evidence(
    record: &SubagentRecord,
    fresh_evidence: &Value,
    completion_authority: &TerminalProcessScopeAuthority,
    deadline: Option<Instant>,
) -> Result<Option<Value>, String> {
    let store = open_subagent_audit_store_until(record, deadline)?;
    let session_id = subagent_audit_session_id(record);
    let session = match deadline {
        Some(deadline) => store.load_result_with_deadline(session_id, deadline),
        None => store.load_result(session_id),
    }
    .map_err(|error| format!("failed to inspect legacy ownership audit: {error}"))?;
    let Some(session) = session else {
        return Ok(None);
    };
    ensure_subagent_reconciliation_deadline(deadline)?;
    let expected_id = fresh_evidence
        .get("reconciliation_id")
        .and_then(Value::as_str)
        .ok_or("fresh ownership reconciliation has no identity")?;
    let mut candidate = None;
    for event in &session.events {
        if event.kind != "subagent_execution_reconciled"
            || event.details.get("subagent_id").and_then(Value::as_str) != Some(record.id.as_str())
            || event
                .details
                .get("execution_generation")
                .and_then(Value::as_u64)
                != record.execution_generation
            || event.details.get("owner_lease").and_then(Value::as_str)
                != record.owner_lease.as_deref()
        {
            continue;
        }
        if event.details.get("reconciliation_id").is_some() {
            return Err(format!(
                "running subagent {} already has identified terminal audit evidence ({expected_id})",
                record.id
            ));
        }
        if candidate.replace(event.details.clone()).is_some() {
            return Err(format!(
                "running subagent {} has multiple legacy ownership reconciliation events",
                record.id
            ));
        }
    }
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    let mut comparable_candidate = candidate
        .as_object()
        .cloned()
        .ok_or("legacy ownership reconciliation audit is not an object")?;
    let mut comparable_fresh = fresh_evidence
        .as_object()
        .cloned()
        .ok_or("fresh ownership reconciliation audit is not an object")?;
    comparable_fresh.remove("reconciliation_id");
    for field in [
        "manager_status",
        "terminal_status",
        "outcome",
        "reconciled_at",
    ] {
        comparable_candidate.remove(field);
        comparable_fresh.remove(field);
    }
    if comparable_candidate != comparable_fresh {
        return Err(
            "legacy ownership reconciliation audit conflicts with process authority".to_string(),
        );
    }
    let terminal_status = candidate
        .get("terminal_status")
        .and_then(Value::as_str)
        .ok_or("legacy ownership reconciliation audit has no terminal status")?;
    let expected_outcome = match (completion_authority, terminal_status) {
        (TerminalProcessScopeAuthority::Cleanup(_), "cancelled") => {
            "cancelled_after_verified_cleanup"
        }
        (TerminalProcessScopeAuthority::Cleanup(_), "failed") => {
            "supervisor_result_lost_after_verified_cleanup"
        }
        (TerminalProcessScopeAuthority::LaunchAbort(_), "failed") => {
            "supervisor_lost_before_gated_workload_launch"
        }
        _ => {
            return Err(
                "legacy ownership reconciliation audit has an invalid terminal status".to_string(),
            );
        }
    };
    if candidate.get("outcome").and_then(Value::as_str) != Some(expected_outcome)
        || candidate
            .get("reconciled_at")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .is_none()
        || !matches!(
            candidate.get("manager_status"),
            None | Some(Value::Null) | Some(Value::String(_))
        )
    {
        return Err(
            "legacy ownership reconciliation audit has invalid stable evidence".to_string(),
        );
    }
    Ok(Some(candidate))
}

fn no_ownership_reconciliation_work(record: &SubagentRecord) -> OwnershipReconciliationWork {
    OwnershipReconciliationWork {
        record: record.clone(),
        evidence: None,
        acquired_owner_lease: None,
        retry_persisted_owner_cleanup: false,
    }
}

fn process_scope_retirement_result_mut(
    record: &mut SubagentRecord,
) -> Option<&mut serde_json::Map<String, Value>> {
    let result = record.result.as_mut()?.as_object_mut()?;
    if matches!(record.status.as_str(), MERGE_PENDING_STATUS | "merged") {
        result.get_mut("subagent_result")?.as_object_mut()
    } else {
        Some(result)
    }
}

fn reconcile_subagent_ownership(project_root: &Path, id: &str) -> Result<SubagentRecord, String> {
    reconcile_subagent_ownership_with_owner_state(project_root, id, false)
}

fn reconcile_subagent_ownership_until(
    project_root: &Path,
    id: &str,
    deadline: Instant,
) -> Result<SubagentRecord, String> {
    reconcile_subagent_ownership_with_owner_state_until(project_root, id, false, Some(deadline))
}

fn reconcile_subagent_ownership_with_owner_state(
    project_root: &Path,
    id: &str,
    owner_confirmed_stopped: bool,
) -> Result<SubagentRecord, String> {
    reconcile_subagent_ownership_with_owner_state_until(
        project_root,
        id,
        owner_confirmed_stopped,
        None,
    )
}

fn reconcile_subagent_ownership_with_owner_state_until(
    project_root: &Path,
    id: &str,
    owner_confirmed_stopped: bool,
    deadline: Option<Instant>,
) -> Result<SubagentRecord, String> {
    let path = record_path(project_root, id)?;
    let records_directory = ensure_records_directory_until(project_root, deadline)?;
    let mut work = with_subagent_reconciliation_lock_in(
        project_root,
        id,
        &records_directory,
        deadline,
        |directory, deadline| {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let mut opened = read_opened_subagent_record_in(directory, &path)?;
            let needs_audit_target = process_scope_retirement_result(&opened.record)
                .is_some_and(|result| result.contains_key("ownership_reconciliation"));
            if needs_audit_target && subagent_audit_target(&opened.record)?.is_none() {
                let audit_target =
                    resolve_legacy_subagent_audit_target(project_root, &opened.record, deadline)?;
                ensure_subagent_reconciliation_deadline(deadline)?;
                set_subagent_audit_target(&mut opened.record, audit_target)?;
                opened.record.updated_at = Utc::now();
                let receipt = write_subagent_record_unlocked_until(
                    project_root,
                    directory,
                    &path,
                    &opened.record,
                    crate::daemons::state::FileExpectation::Present(&opened.file),
                    deadline,
                )?;
                opened.file = receipt.file;
            }
            let record = &mut opened.record;
            if record.status != "running" {
                let Some((evidence, legacy)) = ownership_reconciliation_evidence(record)? else {
                    if has_direct_terminal_process_scope_authority(record)
                        && terminal_process_scope_authority(record)?.is_some()
                    {
                        retire_terminal_process_scope_in_locked_records_until(
                            project_root,
                            directory,
                            record,
                            deadline.ok_or_else(subagent_reconciliation_deadline_elapsed)?,
                        )?;
                        return Ok(OwnershipReconciliationWork {
                            record: record.clone(),
                            evidence: None,
                            acquired_owner_lease: None,
                            retry_persisted_owner_cleanup: true,
                        });
                    }
                    return Ok(no_ownership_reconciliation_work(record));
                };
                if legacy {
                    process_scope_retirement_result_mut(record)
                        .and_then(|result| result.get_mut("ownership_reconciliation"))
                        .ok_or("terminal ownership reconciliation disappeared during upgrade")?
                        .clone_from(&evidence);
                    ensure_subagent_reconciliation_deadline(deadline)?;
                    write_subagent_record_unlocked_until(
                        project_root,
                        directory,
                        &path,
                        record,
                        crate::daemons::state::FileExpectation::Present(&opened.file),
                        deadline,
                    )?;
                }
                retire_terminal_process_scope_in_locked_records_until(
                    project_root,
                    directory,
                    record,
                    deadline.ok_or_else(subagent_reconciliation_deadline_elapsed)?,
                )?;
                return Ok(OwnershipReconciliationWork {
                    record: record.clone(),
                    evidence: Some(evidence),
                    acquired_owner_lease: None,
                    retry_persisted_owner_cleanup: true,
                });
            }

            let execution_generation = record.execution_generation.ok_or_else(|| {
                format!(
                    "running subagent {} has legacy execution ownership and cannot be reconciled safely",
                    record.id
                )
            })?;
            let lease_id = record.owner_lease.clone().ok_or_else(|| {
                format!(
                    "running subagent {} has legacy execution ownership and cannot be reconciled safely",
                    record.id
                )
            })?;
            validate_execution_ownership(execution_generation, &lease_id)?;
            let manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
            let scope_store = open_process_scope_store_until(project_root, directory, deadline)?;
            let Some(mut process_scope) = scope_store
                .as_ref()
                .map(|store| store.try_load(id))
                .transpose()?
                .flatten()
            else {
                if !owner_confirmed_stopped {
                    match SubagentOwnerLease::probe(project_root, execution_generation, &lease_id)?
                    {
                        OwnerLeaseProbe::Live => {
                            return Ok(no_ownership_reconciliation_work(record));
                        }
                        OwnerLeaseProbe::Acquired(lease) => {
                            ensure_subagent_reconciliation_deadline(deadline)?;
                            lease.release_for_reconciliation()?;
                        }
                    }
                }
                #[cfg(test)]
                if record_has_valid_committed_spawn_handoff(record) {
                    // The cfg(test) launcher is an in-process Tokio task and
                    // cannot leave an OS descendant after process loss.  Its
                    // committed start gate is therefore sufficient to record
                    // a truthful interrupted terminal rather than the
                    // production missing-scope recovery posture.
                    let previous_result = record.result.clone();
                    let mut result = json!({
                        "outcome": "owner_process_lost_after_committed_handoff",
                        "cleanup_verified": true,
                        "cleanup_scope": "in_process_test_task",
                    });
                    preserve_spawn_internal_authority(previous_result.as_ref(), &mut result);
                    record.status = "failed".to_string();
                    record.error = Some(INTERRUPTED_ERROR.to_string());
                    record.result = Some(result);
                    record.updated_at = Utc::now();
                    write_subagent_record_unlocked_until(
                        project_root,
                        directory,
                        &path,
                        record,
                        crate::daemons::state::FileExpectation::Present(&opened.file),
                        deadline,
                    )?;
                    return Ok(no_ownership_reconciliation_work(record));
                }
                if record.result.as_ref().and_then(|result| {
                    result
                        .get("process_scope")
                        .and_then(|scope| scope.get("status"))
                        .and_then(Value::as_str)
                }) != Some("missing")
                {
                    ensure_subagent_reconciliation_deadline(deadline)?;
                    let audit_target = subagent_audit_target(record)?;
                    record.result = Some(json!({
                        "outcome": "recovery_required",
                        "process_scope": {
                            "status": "missing",
                            "execution_generation": execution_generation,
                        },
                        "cleanup_verified": false,
                    }));
                    if let Some(audit_target) = audit_target {
                        set_subagent_audit_target(record, audit_target)?;
                    }
                    record.updated_at = Utc::now();
                    write_subagent_record_unlocked_until(
                        project_root,
                        directory,
                        &path,
                        record,
                        crate::daemons::state::FileExpectation::Present(&opened.file),
                        deadline,
                    )?;
                }
                return Ok(no_ownership_reconciliation_work(record));
            };
            let scope_store = scope_store.expect("loaded scope has a retained store");
            if process_scope.execution_generation != execution_generation
                || process_scope.workload_kind != "subagent"
            {
                return Err("running subagent has a mismatched managed-process scope".to_string());
            }

            if process_scope.status != crate::sandbox::process::ProcessScopeStatus::Complete {
                if manager_status.as_deref() == Some("running") && !owner_confirmed_stopped {
                    return Ok(no_ownership_reconciliation_work(record));
                }
                let cleanup_state = scope_store.cleanup_lease_state(&process_scope)?;
                if cleanup_state == crate::sandbox::process::CleanupLeaseState::Live {
                    return Ok(no_ownership_reconciliation_work(record));
                }
                if !owner_confirmed_stopped {
                    if process_scope.status == crate::sandbox::process::ProcessScopeStatus::Prepared
                        && Utc::now()
                            .signed_duration_since(process_scope.updated_at)
                            .num_seconds()
                            < 5
                    {
                        return Ok(no_ownership_reconciliation_work(record));
                    }
                    match SubagentOwnerLease::probe(project_root, execution_generation, &lease_id)?
                    {
                        OwnerLeaseProbe::Live => {
                            return Ok(no_ownership_reconciliation_work(record));
                        }
                        OwnerLeaseProbe::Acquired(lease) => {
                            ensure_subagent_reconciliation_deadline(deadline)?;
                            lease.release_for_reconciliation()?;
                        }
                    }
                }
                let previous_status = process_scope.status;
                let recoverable_launch = cleanup_state
                    == crate::sandbox::process::CleanupLeaseState::Recoverable
                    || (cleanup_state == crate::sandbox::process::CleanupLeaseState::Missing
                        && process_scope.status
                            == crate::sandbox::process::ProcessScopeStatus::Prepared
                        && process_scope.supervisor.is_some()
                        && process_scope.direct_child.is_none());
                let recovery = if recoverable_launch
                    && process_scope.backend
                        == crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace
                    && matches!(
                        process_scope.status,
                        crate::sandbox::process::ProcessScopeStatus::Prepared
                            | crate::sandbox::process::ProcessScopeStatus::Running
                            | crate::sandbox::process::ProcessScopeStatus::CleanupInProgress
                            | crate::sandbox::process::ProcessScopeStatus::RecoveryRequired
                    ) {
                    ensure_subagent_reconciliation_deadline(deadline)?;
                    scope_store.recover_linux_supervisor_loss(&process_scope)
                } else {
                    Err(format!(
                        "automatic cleanup recovery is unavailable for backend {:?}, status {:?}, and lease state {:?}",
                        process_scope.backend, process_scope.status, cleanup_state
                    ))
                };
                match recovery {
                    Ok(recovered) => process_scope = recovered,
                    Err(recovery_error) => {
                        if previous_status != crate::sandbox::process::ProcessScopeStatus::Prepared
                        {
                            ensure_subagent_reconciliation_deadline(deadline)?;
                            process_scope = scope_store.mark_recovery_required(
                                id,
                                execution_generation,
                                &process_scope.cleanup_lease_id,
                                format!(
                                    "supervisor stopped before cleanup proof (previous status: {previous_status:?}): {recovery_error}"
                                ),
                            )?;
                        }
                        ensure_subagent_reconciliation_deadline(deadline)?;
                        let audit_target = subagent_audit_target(record)?;
                        record.result = Some(json!({
                            "outcome": "recovery_required",
                            "process_scope": process_scope,
                            "cleanup_verified": false,
                            "error": recovery_error,
                        }));
                        if let Some(audit_target) = audit_target {
                            set_subagent_audit_target(record, audit_target)?;
                        }
                        record.updated_at = Utc::now();
                        write_subagent_record_unlocked_until(
                            project_root,
                            directory,
                            &path,
                            record,
                            crate::daemons::state::FileExpectation::Present(&opened.file),
                            deadline,
                        )?;
                        return Ok(no_ownership_reconciliation_work(record));
                    }
                }
            }

            let completion_authority = match (
                process_scope.cleanup_proof.clone(),
                process_scope.launch_abort_proof.clone(),
            ) {
                (Some(proof), None) => TerminalProcessScopeAuthority::Cleanup(proof),
                (None, Some(proof)) => TerminalProcessScopeAuthority::LaunchAbort(proof),
                (Some(_), Some(_)) => {
                    return Err(
                        "completed subagent process scope has conflicting completion proofs"
                            .to_string(),
                    );
                }
                (None, None) => {
                    return Err(
                        "completed subagent process scope has no completion proof".to_string()
                    );
                }
            };
            match scope_store.cleanup_lease_state(&process_scope)? {
                crate::sandbox::process::CleanupLeaseState::Live => {
                    return Ok(no_ownership_reconciliation_work(record));
                }
                crate::sandbox::process::CleanupLeaseState::Recoverable => {
                    ensure_subagent_reconciliation_deadline(deadline)?;
                    let cleanup_lease = scope_store.acquire_cleanup_lease(&process_scope)?;
                    match &completion_authority {
                        TerminalProcessScopeAuthority::Cleanup(proof) => {
                            ensure_subagent_reconciliation_deadline(deadline)?;
                            cleanup_lease.release_after_proof(proof)?;
                        }
                        TerminalProcessScopeAuthority::LaunchAbort(proof) => {
                            ensure_subagent_reconciliation_deadline(deadline)?;
                            cleanup_lease.release_after_launch_abort(proof)?;
                        }
                    }
                }
                crate::sandbox::process::CleanupLeaseState::Missing => {}
            }
            let owner_lease =
                match SubagentOwnerLease::probe(project_root, execution_generation, &lease_id)? {
                    OwnerLeaseProbe::Live => {
                        return Ok(no_ownership_reconciliation_work(record));
                    }
                    OwnerLeaseProbe::Acquired(owner_lease) => owner_lease,
                };

            if subagent_audit_target(record)?.is_none() {
                let audit_target =
                    resolve_legacy_subagent_audit_target(project_root, record, deadline)?;
                ensure_subagent_reconciliation_deadline(deadline)?;
                set_subagent_audit_target(record, audit_target)?;
                record.updated_at = Utc::now();
                let receipt = write_subagent_record_unlocked_until(
                    project_root,
                    directory,
                    &path,
                    record,
                    crate::daemons::state::FileExpectation::Present(&opened.file),
                    deadline,
                )?;
                opened.file = receipt.file;
            }

            ensure_subagent_reconciliation_deadline(deadline)?;
            let mut reconciled_at = Utc::now();
            let cancelled = matches!(
                &completion_authority,
                TerminalProcessScopeAuthority::Cleanup(proof)
                    if manager_status.as_deref() == Some("cancelled")
                        || proof.outcome == "cancelled"
            );
            let outcome = match (&completion_authority, cancelled) {
                (_, true) => "cancelled_after_verified_cleanup",
                (TerminalProcessScopeAuthority::Cleanup(_), false) => {
                    "supervisor_result_lost_after_verified_cleanup"
                }
                (TerminalProcessScopeAuthority::LaunchAbort(_), false) => {
                    "supervisor_lost_before_gated_workload_launch"
                }
            };
            let mut terminal_status = if cancelled { "cancelled" } else { "failed" }.to_string();
            let mut error = if cancelled {
                "subagent cancellation was reconciled after its execution owner stopped"
            } else if matches!(
                completion_authority,
                TerminalProcessScopeAuthority::LaunchAbort(_)
            ) {
                "subagent supervisor stopped before the gated workload launched"
            } else {
                OWNER_LOST_ERROR
            };
            let reconciliation_id =
                subagent_reconciliation_id(&record.id, execution_generation, &lease_id)?;
            let mut evidence = json!({
                "reconciliation_id": reconciliation_id.clone(),
                "outcome": outcome,
                "subagent_id": record.id,
                "execution_generation": execution_generation,
                "owner_lease": lease_id.clone(),
                "manager_status": manager_status,
                "terminal_status": terminal_status.clone(),
                "reconciled_at": reconciled_at,
            });
            let evidence_object = evidence
                .as_object_mut()
                .expect("ownership reconciliation evidence is an object");
            match &completion_authority {
                TerminalProcessScopeAuthority::Cleanup(proof) => {
                    evidence_object.insert("cleanup_verified".to_string(), Value::Bool(true));
                    evidence_object.insert(
                        "cleanup_scope".to_string(),
                        Value::String("foreground_descendant_process_tree".to_string()),
                    );
                    evidence_object.insert(
                        "cleanup_proof".to_string(),
                        serde_json::to_value(proof).map_err(|error| {
                            format!("failed to encode managed-process cleanup proof: {error}")
                        })?,
                    );
                }
                TerminalProcessScopeAuthority::LaunchAbort(proof) => {
                    evidence_object.insert("cleanup_verified".to_string(), Value::Bool(false));
                    evidence_object.insert("launch_abort_verified".to_string(), Value::Bool(true));
                    evidence_object
                        .insert("workload_never_launched".to_string(), Value::Bool(true));
                    evidence_object.insert(
                        "launch_abort_proof".to_string(),
                        serde_json::to_value(proof).map_err(|error| {
                            format!("failed to encode managed-process launch-abort proof: {error}")
                        })?,
                    );
                }
            }
            if let Some(legacy_evidence) = legacy_running_reconciliation_evidence(
                record,
                &evidence,
                &completion_authority,
                deadline,
            )? {
                evidence = legacy_evidence;
                evidence
                    .as_object_mut()
                    .expect("validated legacy evidence is an object")
                    .insert(
                        "reconciliation_id".to_string(),
                        Value::String(reconciliation_id),
                    );
                terminal_status = evidence
                    .get("terminal_status")
                    .and_then(Value::as_str)
                    .expect("validated legacy terminal status")
                    .to_string();
                reconciled_at = DateTime::parse_from_rfc3339(
                    evidence
                        .get("reconciled_at")
                        .and_then(Value::as_str)
                        .expect("validated legacy reconciliation timestamp"),
                )
                .expect("validated legacy reconciliation timestamp")
                .with_timezone(&Utc);
                error = if terminal_status == "cancelled" {
                    "subagent cancellation was reconciled after its execution owner stopped"
                } else if matches!(
                    completion_authority,
                    TerminalProcessScopeAuthority::LaunchAbort(_)
                ) {
                    "subagent supervisor stopped before the gated workload launched"
                } else {
                    OWNER_LOST_ERROR
                };
            }
            ensure_subagent_reconciliation_deadline(deadline)?;
            record.status = terminal_status;
            let audit_target = subagent_audit_target(record)?
                .ok_or("terminal ownership reconciliation lost its pinned audit destination")?;
            record.result = Some(json!({
                "outcome": "interrupted",
                "ownership_reconciliation": evidence.clone(),
            }));
            set_subagent_audit_target(record, audit_target)?;
            record.error = Some(error.to_string());
            record.updated_at = reconciled_at;
            write_subagent_record_unlocked_until(
                project_root,
                directory,
                &path,
                record,
                crate::daemons::state::FileExpectation::Present(&opened.file),
                deadline,
            )?;
            retire_terminal_process_scope_in_locked_records_until(
                project_root,
                directory,
                record,
                deadline.ok_or_else(subagent_reconciliation_deadline_elapsed)?,
            )?;
            Ok(OwnershipReconciliationWork {
                record: record.clone(),
                evidence: Some(evidence),
                acquired_owner_lease: Some(owner_lease),
                retry_persisted_owner_cleanup: false,
            })
        },
    )?;

    if work.evidence.is_none() && !work.retry_persisted_owner_cleanup {
        return Ok(work.record);
    }
    let execution_generation = work.record.execution_generation.ok_or_else(|| {
        "terminal ownership reconciliation lost its execution generation".to_string()
    })?;
    let lease_id = work
        .record
        .owner_lease
        .clone()
        .ok_or_else(|| "terminal ownership reconciliation lost its owner lease".to_string())?;
    let cleanup = if let Some(owner_lease) = work.acquired_owner_lease.take() {
        ensure_subagent_reconciliation_deadline(deadline)?;
        owner_lease.remove_until(deadline)
    } else if work.retry_persisted_owner_cleanup {
        remove_persisted_owner_lease_until(project_root, execution_generation, &lease_id, deadline)
    } else {
        Ok(())
    };
    if let Err(error) = cleanup {
        persist_owner_lease_cleanup_error_until(
            project_root,
            id,
            execution_generation,
            &lease_id,
            &error,
            deadline,
        );
        return Err(format!(
            "subagent owner lease cleanup did not complete: {error}"
        ));
    }
    if work.retry_persisted_owner_cleanup
        && work
            .record
            .result
            .as_ref()
            .and_then(|result| result.get("cleanup_unverified"))
            .and_then(Value::as_bool)
            == Some(true)
    {
        work.record = persist_owner_lease_cleanup_success_until(
            project_root,
            id,
            execution_generation,
            &lease_id,
            deadline,
        )?;
    }
    if let Some(evidence) = work.evidence.as_ref() {
        record_subagent_ownership_reconciliation_event(&work.record, evidence, deadline)?;
    }
    Ok(work.record)
}

fn with_subagent_reconciliation_lock_in<T>(
    project_root: &Path,
    id: &str,
    protected_directory: &Path,
    deadline: Option<Instant>,
    operation: impl FnOnce(
        &crate::daemons::state::StableDirectory,
        Option<Instant>,
    ) -> Result<T, String>,
) -> Result<T, String> {
    match deadline {
        Some(deadline) => with_subagent_record_lock_bridge_in_deadline(
            project_root,
            id,
            protected_directory,
            deadline,
            None,
            || Ok(()),
            operation,
        ),
        None => with_subagent_record_lock_in_timeout(
            project_root,
            id,
            protected_directory,
            SUBAGENT_RECORD_LOCK_TIMEOUT,
            |directory, deadline| operation(directory, Some(deadline)),
        ),
    }
}

fn with_subagent_record_lock_in_timeout<T>(
    project_root: &Path,
    id: &str,
    protected_directory: &Path,
    timeout: Duration,
    operation: impl FnOnce(&crate::daemons::state::StableDirectory, Instant) -> Result<T, String>,
) -> Result<T, String> {
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    with_subagent_record_lock_bridge_in_deadline(
        project_root,
        id,
        protected_directory,
        deadline,
        Some(timeout),
        || Ok(()),
        |directory, deadline| {
            operation(
                directory,
                deadline.expect("record bridge always supplies its absolute deadline"),
            )
        },
    )
}

fn with_subagent_record_lock_bridge_in_deadline<T>(
    project_root: &Path,
    id: &str,
    protected_directory: &Path,
    deadline: Instant,
    timeout: Option<Duration>,
    after_migration: impl FnOnce() -> Result<(), String>,
    operation: impl FnOnce(
        &crate::daemons::state::StableDirectory,
        Option<Instant>,
    ) -> Result<T, String>,
) -> Result<T, String> {
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    let migration_lock = project_root
        .join(".nib")
        .join(".subagent-legacy-lock-migration.lock");
    let modern_lock = record_lock_path(project_root, id)?;
    let mut after_migration = Some(after_migration);
    let mut operation = Some(operation);
    with_delegation_lock_in_deadline(
        &migration_lock,
        protected_directory,
        deadline,
        timeout,
        |records_directory, deadline| {
            let mut after_scan = |_| Ok(());
            reconcile_legacy_record_lock_migration_locked(
                project_root,
                records_directory,
                Some(deadline),
                &mut after_scan,
            )?;
            ensure_subagent_reconciliation_deadline(Some(deadline))?;
            after_migration
                .take()
                .expect("record bridge migration hook runs once")()?;
            ensure_subagent_reconciliation_deadline(Some(deadline))?;
            records_directory.verify_visible()?;
            let mut after_rescan = |_| Ok(());
            reconcile_legacy_record_lock_migration_locked(
                project_root,
                records_directory,
                Some(deadline),
                &mut after_rescan,
            )?;
            with_delegation_lock_in_deadline_bound_to(
                &modern_lock,
                records_directory,
                deadline,
                timeout,
                |directory, deadline| {
                    operation.take().expect("record bridge operation runs once")(
                        directory,
                        Some(deadline),
                    )
                },
            )
        },
    )
}

fn with_modern_subagent_record_lock_in<T>(
    lock_path: &Path,
    protected_directory: &crate::daemons::state::StableDirectory,
    deadline: Option<Instant>,
    operation: impl FnOnce(
        &crate::daemons::state::StableDirectory,
        Option<Instant>,
    ) -> Result<T, String>,
) -> Result<T, String> {
    match deadline {
        Some(deadline) => with_delegation_lock_in_deadline_bound_to(
            lock_path,
            protected_directory,
            deadline,
            None,
            |directory, deadline| operation(directory, Some(deadline)),
        ),
        None => with_delegation_lock_in_deadline_bound_to(
            lock_path,
            protected_directory,
            Instant::now() + SUBAGENT_RECORD_LOCK_TIMEOUT,
            Some(SUBAGENT_RECORD_LOCK_TIMEOUT),
            |directory, deadline| operation(directory, Some(deadline)),
        ),
    }
}

fn ensure_subagent_reconciliation_deadline(deadline: Option<Instant>) -> Result<(), String> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(subagent_reconciliation_deadline_elapsed());
    }
    Ok(())
}

fn subagent_reconciliation_deadline_elapsed() -> String {
    "subagent cancellation reconciliation deadline elapsed".to_string()
}

fn open_process_scope_store_until(
    project_root: &Path,
    records: &crate::daemons::state::StableDirectory,
    deadline: Option<Instant>,
) -> Result<Option<crate::sandbox::process::ProcessScopeStore>, String> {
    let deadline = deadline.ok_or_else(subagent_reconciliation_deadline_elapsed)?;
    crate::sandbox::process::ProcessScopeStore::open_existing_bound_to_records(
        project_root,
        records,
        deadline,
    )
}

#[cfg(test)]
pub(crate) fn hold_subagent_record_lock_for_test(
    project_root: &Path,
    id: &str,
) -> Result<File, String> {
    let records = ensure_records_directory(project_root)?;
    let record_lock = record_lock_path(project_root, id)?;
    let record_anchor = crate::daemons::state::daemon_lock_anchor_path(&record_lock)?;
    crate::fs_security::ensure_directory_without_symlinks(
        record_anchor
            .parent()
            .ok_or_else(|| "subagent record lock anchor has no parent".to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let held = open_repository_merge_lock_anchor(&record_lock, &record_anchor)?;
    held.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => {
            "subagent record lock is already held by another test".to_string()
        }
        std::fs::TryLockError::Error(error) => {
            format!("failed to hold subagent record lock for test: {error}")
        }
    })?;
    crate::fs_security::verify_directory_without_symlinks(&records)
        .map_err(|error| error.to_string())?;
    Ok(held)
}

fn record_subagent_ownership_reconciliation_event(
    record: &SubagentRecord,
    evidence: &Value,
    deadline: Option<Instant>,
) -> Result<(), String> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    let session_id = subagent_audit_session_id(record);
    let reconciliation_id = evidence
        .get("reconciliation_id")
        .and_then(Value::as_str)
        .ok_or("ownership reconciliation audit has no stable identity")?;
    let expected_id = subagent_reconciliation_id(
        &record.id,
        record.execution_generation.unwrap_or_default(),
        record.owner_lease.as_deref().unwrap_or_default(),
    )?;
    if reconciliation_id != expected_id {
        return Err("ownership reconciliation audit identity is invalid".to_string());
    }
    let mut legacy_evidence = evidence
        .as_object()
        .cloned()
        .ok_or("ownership reconciliation audit is not an object")?;
    legacy_evidence.remove("reconciliation_id");
    let audit_deadline = deadline.unwrap_or_else(|| {
        Instant::now()
            .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
            .unwrap_or_else(Instant::now)
    });
    let store = open_subagent_audit_store_until(record, Some(audit_deadline))?;
    store
        .record_event_once_with_deadline(
            session_id,
            "subagent_execution_reconciled",
            reconciliation_id,
            evidence.clone(),
            Value::Object(legacy_evidence),
            audit_deadline,
        )
        .map_err(|error| format!("failed to audit subagent ownership reconciliation: {error}"))
}

pub(crate) async fn prepare_subagent_verification_target(
    project_root: &Path,
    id: &str,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<VerificationTarget, String> {
    let project_root = canonical_project_root(project_root)?;
    let _merge_lock = RepositoryMergeLock::acquire(&project_root, cancellation).await?;
    let OpenedSubagentRecord {
        mut record,
        file: mut record_file,
    } = get_opened_subagent_record(&project_root, id)?;
    if !matches!(
        record.status.as_str(),
        "completed" | "verification_failed" | MERGE_FAILED_STATUS | MERGE_PENDING_STATUS
    ) {
        return Err(format!(
            "subagent {} is not ready to verify (status: {})",
            record.id, record.status
        ));
    }
    if record.status == MERGE_PENDING_STATUS {
        let intent = pending_merge_intent(&record)?;
        require_record_branch_oid(&record, &intent.branch_commit)?;
        let worktree_path = pending_verification_root(&project_root, &record)?;
        if worktree_path != project_root {
            if let Err(error) = ensure_child_snapshot_unchanged(
                &worktree_path,
                &record.branch,
                &intent.branch_commit,
                "before pending verification",
            )
            .await
            {
                record.error = Some(error.clone());
                record.updated_at = Utc::now();
                persist_subagent_record_revision(&project_root, &record, &mut record_file)?;
                return Err(error);
            }
            if let Err(error) = crate::sandbox::worktree::Worktree::adopt_branch_revision(
                &project_root,
                &record.id,
                &intent.branch_commit,
            ) {
                record.error = Some(error.clone());
                record.updated_at = Utc::now();
                persist_subagent_record_revision(&project_root, &record, &mut record_file)?;
                return Err(error);
            }
        }
        return Ok(VerificationTarget {
            worktree_path,
            snapshot_commit: intent.branch_commit,
        });
    }
    validate_record_worktree(&project_root, &record)?;
    let worktree_path = record
        .worktree_path
        .canonicalize()
        .map_err(|error| format!("subagent worktree is unavailable: {error}"))?;
    let snapshot_commit = match create_immutable_subagent_snapshot(&worktree_path, &record).await {
        Ok(commit) => {
            if let Err(error) = crate::sandbox::worktree::Worktree::adopt_branch_revision(
                &project_root,
                &record.id,
                &commit,
            ) {
                record.status = MERGE_FAILED_STATUS.to_string();
                record.error = Some(error.clone());
                record.updated_at = Utc::now();
                persist_subagent_record_revision(&project_root, &record, &mut record_file)?;
                return Err(error);
            }
            commit
        }
        Err(error) => {
            record.status = MERGE_FAILED_STATUS.to_string();
            record.error = Some(error.clone());
            record.updated_at = Utc::now();
            persist_subagent_record_revision(&project_root, &record, &mut record_file)?;
            return Err(error);
        }
    };
    record.branch_oid = Some(snapshot_commit.clone());
    record.updated_at = Utc::now();
    persist_subagent_record_revision(&project_root, &record, &mut record_file)?;
    Ok(VerificationTarget {
        worktree_path,
        snapshot_commit,
    })
}

pub fn cancel_subagent(project_root: &Path, id: &str) -> Result<Value, String> {
    cancellation_result(resolve_subagent_cancellation(project_root, id))
}

pub(crate) async fn cancel_subagent_async(project_root: &Path, id: &str) -> Result<Value, String> {
    cancellation_result(resolve_subagent_cancellation_async(project_root, id).await)
}

fn cancellation_result(resolution: CancelSubagentResolution) -> Result<Value, String> {
    match resolution {
        CancelSubagentResolution::Cancelled { record } => Ok(json!({
            "status": record.status,
            "subagent_id": record.id,
            "child_session_id": record.child_session_id,
            "worktree_path": record.worktree_path,
        })),
        CancelSubagentResolution::Terminal { record } => Err(format!(
            "subagent {} is not running (status: {})",
            record.id, record.status
        )),
        CancelSubagentResolution::Unresolved {
            manager_stopped,
            observed_status,
            error,
        } => Err(format!(
            "subagent cancellation is unresolved (manager_stopped: {manager_stopped}, observed_status: {}): {error}",
            observed_status.as_deref().unwrap_or("unavailable")
        )),
    }
}

pub(crate) fn resolve_subagent_cancellation_async(
    project_root: &Path,
    id: &str,
) -> impl std::future::Future<Output = CancelSubagentResolution> + Send + 'static {
    resolve_subagent_cancellation_async_with_start_hook(
        project_root,
        id,
        subagent_cancellation_reconciliation_timeout(),
        || {},
    )
}

fn resolve_subagent_cancellation_async_with_start_hook(
    project_root: &Path,
    id: &str,
    timeout: Duration,
    before_reconciliation: impl FnOnce() + Send + 'static,
) -> impl std::future::Future<Output = CancelSubagentResolution> + Send + 'static {
    // This deadline is intentionally derived before constructing the future. A
    // caller that cannot poll the future or start its worker promptly must not
    // receive a fresh reconciliation budget later.
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    let project_root = project_root.to_path_buf();
    let id = id.to_string();
    let reconciliation_id = id.clone();
    async move {
        run_subagent_cancellation_worker(id, move || {
            before_reconciliation();
            resolve_subagent_cancellation_until(&project_root, &reconciliation_id, deadline)
        })
        .await
    }
}

async fn run_subagent_cancellation_worker(
    id: String,
    reconcile: impl FnOnce() -> CancelSubagentResolution + Send + 'static,
) -> CancelSubagentResolution {
    let (resolution_tx, resolution_rx) = tokio::sync::oneshot::channel();
    let worker = match std::thread::Builder::new()
        .name("nib-subagent-cancellation".to_string())
        .spawn(move || {
            let resolution = reconcile();
            let _ = resolution_tx.send(resolution);
        }) {
        Ok(worker) => worker,
        Err(error) => {
            return subagent_cancellation_worker_failure(
                &id,
                format!("failed to start reconciliation worker: {error}"),
            );
        }
    };
    let mut worker = SubagentCancellationWorker::new(worker);
    let resolution = resolution_rx.await;
    let joined = worker.join();
    match (resolution, joined) {
        (_, Err(error)) => subagent_cancellation_worker_failure(&id, error),
        (Err(error), Ok(())) => subagent_cancellation_worker_failure(
            &id,
            format!("reconciliation worker stopped without a resolution: {error}"),
        ),
        (Ok(resolution), Ok(())) => resolution,
    }
}

fn subagent_cancellation_worker_failure(id: &str, error: String) -> CancelSubagentResolution {
    let manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
    CancelSubagentResolution::Unresolved {
        manager_stopped: manager_status
            .as_deref()
            .is_some_and(is_stopped_task_status),
        observed_status: None,
        error: format!("subagent cancellation reconciliation worker failed: {error}"),
    }
}

pub(crate) fn resolve_subagent_cancellation(
    project_root: &Path,
    id: &str,
) -> CancelSubagentResolution {
    let started = Instant::now();
    let deadline = started
        .checked_add(subagent_cancellation_reconciliation_timeout())
        .unwrap_or(started);
    resolve_subagent_cancellation_until(project_root, id, deadline)
}

fn resolve_subagent_cancellation_until(
    project_root: &Path,
    id: &str,
    deadline: Instant,
) -> CancelSubagentResolution {
    let project_root = match canonical_project_root(project_root) {
        Ok(project_root) => project_root,
        Err(error) => {
            return CancelSubagentResolution::Unresolved {
                manager_stopped: false,
                observed_status: None,
                error,
            };
        }
    };
    let initial = match reconcile_subagent_ownership_until(&project_root, id, deadline) {
        Ok(record) => record,
        Err(error) => {
            let manager_before = crate::daemons::task::TASK_MANAGER.get_status(id);
            let manager_error = match manager_before.as_deref() {
                Some("running") => crate::daemons::task::TASK_MANAGER.cancel(id).err(),
                Some(status) if is_stopped_task_status(status) => None,
                Some(status) => Some(format!("background task has unknown status '{status}'")),
                None => Some(
                    "background task is untracked after process restart or state loss".to_string(),
                ),
            };
            let manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
            return CancelSubagentResolution::Unresolved {
                manager_stopped: manager_status
                    .as_deref()
                    .is_some_and(is_stopped_task_status),
                observed_status: None,
                error: [
                    Some(format!(
                        "subagent cancellation target is untracked: {error}"
                    )),
                    manager_error,
                    Some(format!(
                        "cancellation has no durable stopped record yet (manager status: {})",
                        manager_status.as_deref().unwrap_or("untracked")
                    )),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; "),
            };
        }
    };
    if initial.status != "running"
        && initial.status != "cancelled"
        && !is_terminal_subagent_status(&initial.status)
    {
        return CancelSubagentResolution::Unresolved {
            manager_stopped: false,
            observed_status: Some(initial.status.clone()),
            error: format!("subagent has unknown status '{}'", initial.status),
        };
    }

    let manager_before = crate::daemons::task::TASK_MANAGER.get_status(id);
    let manager_error = match manager_before.as_deref() {
        Some("running") if initial.status == "running" || initial.status == "cancelled" => {
            crate::daemons::task::TASK_MANAGER.cancel(id).err()
        }
        Some("running") => None,
        Some(status) if is_stopped_task_status(status) => None,
        Some(status) => Some(format!("background task has unknown status '{status}'")),
        None if initial.status == "running" => {
            Some("background task is untracked after process restart or state loss".to_string())
        }
        None => None,
    };
    let mut reconciliation_errors = Vec::new();
    let mut observed_status = None;
    let mut manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
    for attempt in 0..SUBAGENT_CANCELLATION_RECONCILIATION_ATTEMPTS {
        if Instant::now() >= deadline {
            reconciliation_errors.push(subagent_reconciliation_deadline_elapsed());
            break;
        }
        manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
        match reconcile_subagent_ownership_until(&project_root, id, deadline) {
            Ok(record) if is_terminal_subagent_status(&record.status) => {
                if terminal_manager_status_matches(&record.status, manager_status.as_deref()) {
                    return CancelSubagentResolution::Terminal { record };
                }
                reconciliation_errors.push(format!(
                    "terminal subagent record '{}' contradicts background task status '{}'",
                    record.status,
                    manager_status.as_deref().unwrap_or("untracked")
                ));
                observed_status = Some(record.status);
            }
            Ok(record) if record.status == "cancelled" => {
                if manager_status
                    .as_deref()
                    .is_none_or(|status| status == "cancelled")
                {
                    return CancelSubagentResolution::Cancelled { record };
                }
                reconciliation_errors.push(format!(
                    "cancelled subagent record contradicts background task status '{}'",
                    manager_status.as_deref().unwrap_or("untracked")
                ));
                observed_status = Some(record.status);
            }
            Ok(record) => observed_status = Some(record.status),
            Err(error) => reconciliation_errors.push(format!(
                "authoritative record reconciliation failed: {error}"
            )),
        }
        if attempt + 1 < SUBAGENT_CANCELLATION_RECONCILIATION_ATTEMPTS
            && matches!(manager_status.as_deref(), Some("running" | "cancelled"))
        {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                reconciliation_errors.push(subagent_reconciliation_deadline_elapsed());
                break;
            };
            std::thread::sleep(Duration::from_millis(10).min(remaining));
        }
    }
    CancelSubagentResolution::Unresolved {
        manager_stopped: manager_status
            .as_deref()
            .is_some_and(is_stopped_task_status),
        observed_status,
        error: [
            manager_error,
            (!reconciliation_errors.is_empty()).then(|| reconciliation_errors.join("; ")),
            Some(format!(
                "cancellation has no durable stopped record yet (manager status: {})",
                manager_status.as_deref().unwrap_or("untracked")
            )),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; "),
    }
}

fn terminal_manager_status_matches(record_status: &str, manager_status: Option<&str>) -> bool {
    match manager_status {
        None => true,
        Some("failed") => record_status == "failed",
        Some("completed") => record_status != "failed",
        Some(_) => false,
    }
}

fn is_terminal_subagent_status(status: &str) -> bool {
    matches!(
        status,
        "completed"
            | "failed"
            | "verification_failed"
            | "merged"
            | MERGE_PENDING_STATUS
            | MERGE_FAILED_STATUS
    )
}

fn is_stopped_task_status(status: &str) -> bool {
    matches!(status, "cancelled" | "completed" | "failed")
}

pub fn write_subagent_record(project_root: &Path, record: &SubagentRecord) -> Result<(), String> {
    write_subagent_record_with_refresh_hook(project_root, record, || Ok(()))
        .map(drop)
        .map_err(|error| error.message)
}

fn write_subagent_record_with_refresh_hook(
    project_root: &Path,
    record: &SubagentRecord,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<InitialSubagentRecordPublication, InitialSubagentRecordPublicationError> {
    write_subagent_record_with_refresh_hook_and_timeout(
        project_root,
        record,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        after_publication,
    )
}

fn write_subagent_record_with_refresh_hook_and_timeout(
    project_root: &Path,
    record: &SubagentRecord,
    timeout: Duration,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<InitialSubagentRecordPublication, InitialSubagentRecordPublicationError> {
    write_subagent_record_with_refresh_hooks_and_timeout(
        project_root,
        record,
        timeout,
        || Ok(()),
        after_publication,
    )
}

fn write_subagent_record_with_refresh_hooks_and_timeout(
    project_root: &Path,
    record: &SubagentRecord,
    timeout: Duration,
    mut before_namespace_step: impl FnMut() -> Result<(), String>,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<InitialSubagentRecordPublication, InitialSubagentRecordPublicationError> {
    let project_root = canonical_project_root(project_root).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
            publication_attempted: false,
        }
    })?;
    let path = record_path(&project_root, &record.id).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
            publication_attempted: false,
        }
    })?;
    let records_directory = ensure_records_directory(&project_root).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
            publication_attempted: false,
        }
    })?;
    let mut rescued_receipt = None;
    let result = with_subagent_record_lock_in_timeout(
        &project_root,
        &record.id,
        &records_directory,
        timeout,
        |directory, deadline| {
            let receipt = match write_subagent_record_unlocked_with_receipt_and_guard(
                &project_root,
                directory,
                &path,
                record,
                crate::daemons::state::FileExpectation::Missing,
                Some(deadline),
                &mut before_namespace_step,
            ) {
                Ok(receipt) => receipt,
                Err(error) => {
                    rescued_receipt = error.receipt;
                    return Err(error.message);
                }
            };
            let rescue_file = match receipt.file.try_clone() {
                Ok(file) => file,
                Err(error) => {
                    rescued_receipt = Some(receipt);
                    return Err(format!(
                        "failed to retain the initial subagent publication receipt: {error}"
                    ));
                }
            };
            rescued_receipt = Some(crate::daemons::state::FilePublicationReceipt {
                file: rescue_file,
                exact_identity: receipt.exact_identity,
            });
            if !receipt.exact_identity {
                return Err(format!(
                    "initial subagent record {} was published without an exact file-identity receipt; the visible generation was preserved",
                    record.id
                ));
            }
            after_publication()?;
            let opened = read_opened_subagent_record_in(directory, &path)?;
            validate_reopened_subagent_record(record, &opened.record)?;
            if !crate::daemons::state::same_open_file_identity(&receipt.file, &opened.file)? {
                return Err(format!(
                    "published subagent record {} was replaced by an identity-distinct generation before refresh; the visible generation was preserved",
                    record.id
                ));
            }
            directory.verify_file_identity(&path, &receipt.file)?;
            Ok(InitialSubagentRecordPublication { receipt })
        },
    );
    match result {
        Ok(publication) => Ok(publication),
        Err(message) => Err(InitialSubagentRecordPublicationError {
            message,
            receipt: rescued_receipt,
            publication_attempted: true,
        }),
    }
}

fn write_subagent_record_unlocked_until(
    project_root: &Path,
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    record: &SubagentRecord,
    expected: crate::daemons::state::FileExpectation<'_>,
    deadline: Option<Instant>,
) -> Result<crate::daemons::state::FilePublicationReceipt, String> {
    let publication = write_subagent_record_unlocked_with_receipt(
        project_root,
        directory,
        path,
        record,
        expected,
        deadline,
    );
    let error = match publication {
        Ok(receipt) => return Ok(receipt),
        Err(error) => error,
    };
    let Some(receipt) = error.receipt.filter(|receipt| receipt.exact_identity) else {
        return Err(error.message);
    };
    ensure_subagent_reconciliation_deadline(deadline)?;
    let previous_expected = match expected {
        crate::daemons::state::FileExpectation::Present(file) => Some(file),
        crate::daemons::state::FileExpectation::Missing => None,
        #[cfg(test)]
        crate::daemons::state::FileExpectation::Any => None,
    };
    let expected_bytes = serde_json::to_vec_pretty(record).map_err(|recovery| {
        format!(
            "{}; record recovery encoding failed: {recovery}",
            error.message
        )
    })?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    let mut finalization_guard = || {
        pause_subagent_record_finalization(&record.id)?;
        ensure_subagent_reconciliation_deadline(deadline)
    };
    if let Err(recovery) = directory.finalize_failed_exact_publication_with_guard(
        path,
        previous_expected,
        &receipt,
        ".nib-subagent-",
        &expected_bytes,
        &mut finalization_guard,
    ) {
        return Err(format!(
            "{}; exact publication recovery failed and all ambiguous state was preserved: {recovery}",
            error.message
        ));
    }
    ensure_subagent_reconciliation_deadline(deadline)?;
    let reopened = read_opened_subagent_record_in(directory, path).map_err(|recovery| {
        format!(
            "{}; finalized publication readback failed: {recovery}",
            error.message
        )
    })?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    validate_reopened_subagent_record(record, &reopened.record).map_err(|recovery| {
        format!(
            "{}; finalized publication record validation failed: {recovery}",
            error.message
        )
    })?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    if !crate::daemons::state::same_open_file_identity(&receipt.file, &reopened.file)? {
        return Err(format!(
            "{}; finalized publication was replaced before its authority could be adopted",
            error.message
        ));
    }
    ensure_subagent_reconciliation_deadline(deadline)?;
    directory.verify_file_identity(path, &receipt.file)?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    Ok(receipt)
}

fn write_subagent_record_unlocked_with_receipt(
    project_root: &Path,
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    record: &SubagentRecord,
    expected: crate::daemons::state::FileExpectation<'_>,
    deadline: Option<Instant>,
) -> Result<
    crate::daemons::state::FilePublicationReceipt,
    crate::daemons::state::FilePublicationError,
> {
    write_subagent_record_unlocked_with_receipt_and_guard(
        project_root,
        directory,
        path,
        record,
        expected,
        deadline,
        &mut || Ok(()),
    )
}

fn write_subagent_record_unlocked_with_receipt_and_guard(
    project_root: &Path,
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    record: &SubagentRecord,
    expected: crate::daemons::state::FileExpectation<'_>,
    deadline: Option<Instant>,
    before_namespace_step: &mut impl FnMut() -> Result<(), String>,
) -> Result<
    crate::daemons::state::FilePublicationReceipt,
    crate::daemons::state::FilePublicationError,
> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    let parent = path
        .parent()
        .ok_or_else(|| "subagent record has no parent".to_string())?;
    crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent records: {error}"))?;
    let within_project =
        crate::fs_security::canonical_path_starts_with(&canonical_parent, project_root)
            .map_err(|error| format!("failed to resolve subagent project root: {error}"))?;
    if crate::fs_security::metadata_is_link_or_reparse(&metadata)
        || !metadata.is_dir()
        || !within_project
    {
        return Err(format!(
            "subagent records path must be a local project directory: {}",
            parent.display()
        )
        .into());
    }
    let contents = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    if contents.len() as u64 > MAX_SUBAGENT_RECORD_BYTES {
        return Err(
            format!("subagent record exceeds the {MAX_SUBAGENT_RECORD_BYTES}-byte limit").into(),
        );
    }
    pause_subagent_record_write(&record.id)?;
    fail_cancelled_record_write(&record.id, &record.status)?;
    let publication = directory.save_bytes_atomically_expected_with_receipt_and_guard(
        path,
        &contents,
        ".nib-subagent-",
        expected,
        || {
            before_namespace_step()?;
            ensure_subagent_reconciliation_deadline(deadline)
        },
    );
    #[cfg(test)]
    {
        fail_recoverable_revision_publication(&record.id, expected, publication)
    }
    #[cfg(not(test))]
    {
        publication
    }
}

fn persist_subagent_record_revision(
    project_root: &Path,
    record: &SubagentRecord,
    expected_file: &mut File,
) -> Result<(), String> {
    persist_subagent_record_revision_with_refresh_hook(project_root, record, expected_file, || {
        Ok(())
    })
}

fn persist_subagent_record_revision_with_refresh_hook(
    project_root: &Path,
    record: &SubagentRecord,
    expected_file: &mut File,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    persist_subagent_record_revision_with_refresh_hook_and_timeout(
        project_root,
        record,
        expected_file,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        after_publication,
    )
}

fn persist_subagent_record_revision_with_refresh_hook_and_timeout(
    project_root: &Path,
    record: &SubagentRecord,
    expected_file: &mut File,
    timeout: Duration,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    persist_subagent_record_revision_with_refresh_hooks_and_timeout(
        project_root,
        record,
        expected_file,
        timeout,
        || Ok(()),
        after_publication,
    )
}

fn persist_subagent_record_revision_with_refresh_hooks_and_timeout(
    project_root: &Path,
    record: &SubagentRecord,
    expected_file: &mut File,
    timeout: Duration,
    mut before_namespace_step: impl FnMut() -> Result<(), String>,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let path = record_path(project_root, &record.id)?;
    let records_directory = ensure_records_directory(project_root)?;
    let expected_bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| format!("failed to encode revised subagent record: {error}"))?;
    let mut rescued_receipt = None;
    let result = with_subagent_record_lock_in_timeout(
        project_root,
        &record.id,
        &records_directory,
        timeout,
        |directory, deadline| {
            let receipt = match write_subagent_record_unlocked_with_receipt_and_guard(
                project_root,
                directory,
                &path,
                record,
                crate::daemons::state::FileExpectation::Present(expected_file),
                Some(deadline),
                &mut before_namespace_step,
            ) {
                Ok(receipt) => receipt,
                Err(mut error) => {
                    let Some(receipt) = error.receipt.take() else {
                        return Err(error.message);
                    };
                    if !receipt.exact_identity {
                        rescued_receipt = Some(receipt);
                        return Err(error.message);
                    }
                    let mut namespace_guard =
                        || ensure_subagent_reconciliation_deadline(Some(deadline));
                    if let Err(recovery) = directory.finalize_failed_exact_publication_with_guard(
                        &path,
                        Some(expected_file),
                        &receipt,
                        ".nib-subagent-",
                        &expected_bytes,
                        &mut namespace_guard,
                    ) {
                        rescued_receipt = Some(receipt);
                        return Err(format!(
                            "{}; revised publication recovery failed and ambiguous state was preserved: {recovery}",
                            error.message
                        ));
                    }
                    receipt
                }
            };
            let rescue_file = match receipt.file.try_clone() {
                Ok(file) => file,
                Err(error) => {
                    rescued_receipt = Some(receipt);
                    return Err(format!(
                        "failed to retain the revised subagent publication receipt: {error}"
                    ));
                }
            };
            rescued_receipt = Some(crate::daemons::state::FilePublicationReceipt {
                file: rescue_file,
                exact_identity: receipt.exact_identity,
            });
            if !receipt.exact_identity {
                return Err(format!(
                    "revised subagent record {} was published without an exact file-identity receipt; the visible generation was preserved",
                    record.id
                ));
            }
            after_publication()?;
            let opened = read_opened_subagent_record_in(directory, &path)?;
            validate_reopened_subagent_record(record, &opened.record)?;
            if !crate::daemons::state::same_open_file_identity(&receipt.file, &opened.file)? {
                return Err(format!(
                    "published subagent record {} was replaced by an identity-distinct generation before refresh; the visible generation was preserved",
                    record.id
                ));
            }
            directory.verify_file_identity(&path, &receipt.file)?;
            Ok(receipt.file)
        },
    );
    match result {
        Ok(next_file) => {
            *expected_file = next_file;
            Ok(())
        }
        Err(error) => {
            if let Some(receipt) = rescued_receipt.filter(|receipt| receipt.exact_identity) {
                *expected_file = receipt.file;
            }
            Err(error)
        }
    }
}

fn validate_reopened_subagent_record(
    expected: &SubagentRecord,
    reopened: &SubagentRecord,
) -> Result<(), String> {
    let expected_value = serde_json::to_value(expected).map_err(|error| error.to_string())?;
    let reopened_value = serde_json::to_value(reopened).map_err(|error| error.to_string())?;
    if reopened.id != expected.id
        || reopened.created_at != expected.created_at
        || reopened.updated_at != expected.updated_at
        || reopened.execution_generation != expected.execution_generation
        || reopened.owner_lease != expected.owner_lease
        || reopened_value != expected_value
    {
        return Err(format!(
            "published subagent record {} was substituted before its committed revision handle could be refreshed; the visible generation was preserved",
            expected.id
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn inject_cancelled_record_write_failures(id: &str, count: usize) {
    let mut failures = CANCELLED_RECORD_WRITE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if count == 0 {
        failures.remove(id);
    } else {
        failures.insert(id.to_string(), count);
    }
}

#[cfg(test)]
fn inject_recoverable_revision_publication_failures(id: &str, count: usize) {
    let mut failures = RECOVERABLE_REVISION_PUBLICATION_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if count == 0 {
        failures.remove(id);
    } else {
        failures.insert(id.to_string(), count);
    }
}

#[cfg(test)]
fn fail_recoverable_revision_publication(
    record_id: &str,
    expected: crate::daemons::state::FileExpectation<'_>,
    publication: Result<
        crate::daemons::state::FilePublicationReceipt,
        crate::daemons::state::FilePublicationError,
    >,
) -> Result<
    crate::daemons::state::FilePublicationReceipt,
    crate::daemons::state::FilePublicationError,
> {
    let receipt = publication?;
    if !matches!(expected, crate::daemons::state::FileExpectation::Present(_)) {
        return Ok(receipt);
    }
    let mut failures = RECOVERABLE_REVISION_PUBLICATION_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(remaining) = failures.get_mut(record_id) else {
        return Ok(receipt);
    };
    *remaining = remaining.saturating_sub(1);
    if *remaining == 0 {
        failures.remove(record_id);
    }
    Err(crate::daemons::state::FilePublicationError {
        message: "injected recoverable revised subagent publication failure".to_string(),
        receipt: Some(receipt),
    })
}

#[cfg(test)]
fn fail_cancelled_record_write(record_id: &str, status: &str) -> Result<(), String> {
    if status != "cancelled" {
        return Ok(());
    }
    let mut failures = CANCELLED_RECORD_WRITE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(remaining) = failures.get_mut(record_id) else {
        return Ok(());
    };
    *remaining = remaining.saturating_sub(1);
    if *remaining == 0 {
        failures.remove(record_id);
    }
    Err("injected cancelled subagent record write failure".to_string())
}

#[cfg(not(test))]
fn fail_cancelled_record_write(_record_id: &str, _status: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
fn pause_subagent_record_write(record_id: &str) -> Result<(), String> {
    let Some(expected_id) = std::env::var_os("NIB_TEST_SUBAGENT_WRITE_ID") else {
        return Ok(());
    };
    if expected_id != std::ffi::OsStr::new(record_id) {
        return Ok(());
    }
    let ready = PathBuf::from(
        std::env::var_os("NIB_TEST_SUBAGENT_WRITE_READY")
            .ok_or_else(|| "missing subagent write readiness path".to_string())?,
    );
    let resume = PathBuf::from(
        std::env::var_os("NIB_TEST_SUBAGENT_WRITE_RESUME")
            .ok_or_else(|| "missing subagent write resume path".to_string())?,
    );
    std::fs::write(&ready, b"ready")
        .map_err(|error| format!("failed to publish subagent write readiness: {error}"))?;
    let started = Instant::now();
    while !resume.exists() {
        if started.elapsed() >= Duration::from_secs(10) {
            return Err("timed out waiting to resume subagent record write".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(test))]
fn pause_subagent_record_write(_record_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
fn pause_subagent_record_finalization(record_id: &str) -> Result<(), String> {
    let Some(expected_id) = std::env::var_os("NIB_TEST_SUBAGENT_FINALIZE_ID") else {
        return Ok(());
    };
    if expected_id != std::ffi::OsStr::new(record_id) {
        return Ok(());
    }
    let ready = PathBuf::from(
        std::env::var_os("NIB_TEST_SUBAGENT_FINALIZE_READY")
            .ok_or_else(|| "missing subagent finalization readiness path".to_string())?,
    );
    let resume = PathBuf::from(
        std::env::var_os("NIB_TEST_SUBAGENT_FINALIZE_RESUME")
            .ok_or_else(|| "missing subagent finalization resume path".to_string())?,
    );
    std::fs::write(&ready, b"ready")
        .map_err(|error| format!("failed to publish subagent finalization readiness: {error}"))?;
    let started = Instant::now();
    while !resume.exists() {
        if started.elapsed() >= Duration::from_secs(10) {
            return Err("timed out waiting to resume subagent record finalization".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(test))]
fn pause_subagent_record_finalization(_record_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
fn prepare_child_runtime_config(
    config: &crate::config::NibConfig,
    worktree_path: &Path,
) -> Result<(), String> {
    let mut config = selected_child_runtime_config(config, worktree_path)?;
    crate::config::save_nib_config_full_new_unpublished_root(worktree_path, &mut config)
        .map_err(|error| error.to_string())
}

fn prepare_child_runtime_config_with_authority(
    config: &crate::config::NibConfig,
    worktree_path: &Path,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    let deadline = authority.operation_deadline();
    authority.verify_until(deadline)?;
    let mut config = selected_child_runtime_config(config, worktree_path)?;

    crate::config::save_nib_config_full_new_unpublished_root_with_guard(
        worktree_path,
        &mut config,
        deadline,
        || authority.verify_until(deadline),
    )
    .map_err(|error| error.to_string())?;
    authority.verify_until(deadline)
}

fn selected_child_runtime_config(
    config: &crate::config::NibConfig,
    worktree_path: &Path,
) -> Result<crate::config::NibConfig, String> {
    let mut config = config.clone();

    if !config.profiles.active.is_empty() {
        let selected = config
            .profiles
            .active
            .iter()
            .find(|profile| profile.id == config.profiles.default)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "default profile {} is missing from active profiles",
                    config.profiles.default
                )
            })?;
        let mut selected = selected;
        selected.root = PathBuf::from(".");
        selected.state_dir = Some(PathBuf::from(".nib").join("profiles").join(&selected.id));
        selected.env_file = selected
            .env_file
            .filter(|path| worktree_path.join(path).is_file());
        selected
            .skill_paths
            .retain(|path| worktree_path.join(path).is_dir());
        config.profiles.default = selected.id.clone();
        config.profiles.active = vec![selected];
    }

    // A child always owns only its linked worktree. Parent-specific writable
    // exceptions must not silently expand that boundary.
    config.execution.boundaries.allow_write.clear();
    // The worktree is a new configuration root. Do not treat the parent's
    // snapshot revision as authoritative for a path that has no config yet.
    config.revision = 0;
    Ok(config)
}

#[cfg(test)]
fn persist_subagent_outcome(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    outcome: Result<crate::agent::AgentRunSummary, String>,
) -> Result<(), String> {
    persist_subagent_outcome_internal(
        project_root,
        id,
        execution_generation,
        lease_id,
        outcome,
        None,
    )
}

fn persist_subagent_outcome_with_cleanup(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    outcome: Result<crate::agent::AgentRunSummary, String>,
    cleanup_proof: &crate::sandbox::process::CleanupProof,
) -> Result<(), String> {
    persist_subagent_outcome_internal(
        project_root,
        id,
        execution_generation,
        lease_id,
        outcome,
        Some(cleanup_proof),
    )
}

fn persist_subagent_outcome_internal(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    outcome: Result<crate::agent::AgentRunSummary, String>,
    cleanup_proof: Option<&crate::sandbox::process::CleanupProof>,
) -> Result<(), String> {
    enum TaskUpdate {
        Complete(Value),
        Fail(String, Option<Value>),
    }

    let cleanup_evidence = cleanup_proof
        .map(|proof| {
            if proof.execution_generation != execution_generation || !proof.descendants_reaped {
                return Err("subagent cleanup proof does not own this execution".to_string());
            }
            serde_json::to_value(proof)
                .map_err(|error| format!("failed to encode subagent cleanup proof: {error}"))
        })
        .transpose()?;
    let task_update = update_subagent_record(project_root, id, |record| {
        if record.status != "running" {
            return Ok(None);
        }
        if !record_matches_execution(record, execution_generation, lease_id)? {
            return Ok(None);
        }
        let previous_result = record.result.clone();
        let update = match outcome {
            Ok(summary) => {
                let completed = summary.outcome == "completed" && !summary.bound_reached;
                let mut result = json!({
                    "session_id": summary.session_id,
                    "steps_taken": summary.steps_taken,
                    "last_message": summary.last_message,
                    "tool_call_count": summary.tool_call_count,
                    "final_state": summary.final_state.as_str(),
                    "outcome": summary.outcome,
                    "failure": summary.failure,
                    "bound_reached": summary.bound_reached,
                    "trace": summary.trace,
                });
                if let (Some(result), Some(cleanup)) =
                    (result.as_object_mut(), cleanup_evidence.clone())
                {
                    result.insert("cleanup_verified".to_string(), Value::Bool(true));
                    result.insert("cleanup_proof".to_string(), cleanup);
                }
                preserve_spawn_internal_authority(previous_result.as_ref(), &mut result);
                record.result = Some(result.clone());
                if completed {
                    record.status = "completed".to_string();
                    record.error = None;
                    TaskUpdate::Complete(result)
                } else {
                    let error = format!(
                        "subagent ended without completion (outcome: {})",
                        result["outcome"].as_str().unwrap_or("unknown")
                    );
                    record.status = "failed".to_string();
                    record.error = Some(error.clone());
                    TaskUpdate::Fail(error, Some(result))
                }
            }
            Err(error) => {
                record.status = "failed".to_string();
                record.error = Some(error.clone());
                let mut result = cleanup_evidence.clone().map_or_else(
                    || json!({}),
                    |cleanup| {
                        json!({
                            "outcome": "worker_failed",
                            "cleanup_verified": true,
                            "cleanup_proof": cleanup,
                        })
                    },
                );
                preserve_spawn_internal_authority(previous_result.as_ref(), &mut result);
                record.result = Some(result.clone());
                TaskUpdate::Fail(error, Some(result))
            }
        };
        record.updated_at = Utc::now();
        Ok(Some(update))
    })?;

    match task_update {
        Some(TaskUpdate::Complete(result)) => {
            crate::daemons::task::TASK_MANAGER.complete(id, project_public_subagent_result(result));
        }
        Some(TaskUpdate::Fail(error, result)) => {
            crate::daemons::task::TASK_MANAGER.fail(
                id,
                error,
                result.and_then(project_public_subagent_result),
            );
        }
        None => {}
    }
    Ok(())
}

fn persist_supervised_interruption(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    cleanup_proof: &crate::sandbox::process::CleanupProof,
    cancelled: bool,
) -> Result<(), String> {
    if cleanup_proof.execution_generation != execution_generation
        || !cleanup_proof.descendants_reaped
    {
        return Err("subagent cleanup proof does not own this execution".to_string());
    }
    let cleanup = serde_json::to_value(cleanup_proof)
        .map_err(|error| format!("failed to encode subagent cleanup proof: {error}"))?;
    let updated = update_subagent_record(project_root, id, |record| {
        if record.status != "running"
            || !record_matches_execution(record, execution_generation, lease_id)?
        {
            return Ok(false);
        }
        record.status = if cancelled { "cancelled" } else { "failed" }.to_string();
        let reason = if cancelled {
            "cancelled by manage_subagents"
        } else {
            OWNER_LOST_ERROR
        };
        record.error = Some(reason.to_string());
        let previous_result = record.result.clone();
        let mut result = json!({
            "outcome": if cancelled { "cancelled" } else { "owner_process_lost" },
            "cleanup_verified": true,
            "cleanup_scope": "foreground_descendant_process_tree",
            "cleanup_proof": cleanup,
        });
        preserve_spawn_internal_authority(previous_result.as_ref(), &mut result);
        record.result = Some(result);
        record.updated_at = Utc::now();
        Ok(true)
    })?;
    if updated {
        if cancelled {
            let _ = crate::daemons::task::TASK_MANAGER.cancel(id);
        } else {
            crate::daemons::task::TASK_MANAGER.fail(id, OWNER_LOST_ERROR.to_string(), None);
        }
    }
    Ok(())
}

#[cfg(test)]
fn persist_interrupted_subagent(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    reason: &str,
) -> bool {
    persist_interrupted_subagent_until(
        project_root,
        id,
        execution_generation,
        lease_id,
        reason,
        None,
    )
}

fn persist_interrupted_subagent_until(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    reason: &str,
    deadline: Option<Instant>,
) -> bool {
    let updated = update_subagent_record_until(project_root, id, deadline, |record| {
        if record.status != "running" {
            return Ok(false);
        }
        if !record_matches_execution(record, execution_generation, lease_id)? {
            return Ok(false);
        }
        let manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
        if manager_status.as_deref() == Some("cancelled") {
            record.status = "cancelled".to_string();
            record.error = Some("cancelled by manage_subagents".to_string());
        } else {
            record.status = "failed".to_string();
            record.error = Some(reason.to_string());
        }
        record.updated_at = Utc::now();
        Ok(record.status == "failed")
    });
    match updated {
        Ok(true) => {
            crate::daemons::task::TASK_MANAGER.fail(id, reason.to_string(), None);
            true
        }
        Ok(false) => true,
        Err(_) => {
            crate::daemons::task::TASK_MANAGER.fail(id, reason.to_string(), None);
            false
        }
    }
}

fn persist_unstarted_after_preparation_unlock(
    project_root: &Path,
    id: &str,
    execution_generation: Option<u64>,
    lease_id: Option<&str>,
    reason: &str,
    deadline: Instant,
) -> Result<(), String> {
    let (Some(execution_generation), Some(lease_id)) = (execution_generation, lease_id) else {
        return Err("pending subagent record lost its execution ownership".to_string());
    };
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    let nib_path = project_root.join(".nib");
    let nib = crate::daemons::state::StableDirectory::open(&nib_path)?;
    let scope_directory = nib_path.join("process-scopes");
    let scope = match nib.entry_kind(&scope_directory)? {
        Some(crate::daemons::state::StableEntryKind::Directory) => {
            crate::sandbox::process::ProcessScopeStore::open_with_lock_deadline(
                project_root,
                deadline,
            )?
            .try_load(id)?
        }
        Some(crate::daemons::state::StableEntryKind::File) => {
            return Err("managed-process scope namespace is not a directory".to_string());
        }
        None => None,
    };
    if let Some(scope) = scope {
        if scope.execution_generation != execution_generation {
            return Err(
                "unstarted subagent process scope belongs to another execution generation"
                    .to_string(),
            );
        }
        // A supervisor monitor owns terminalization once its process-scope
        // record is durable. Publishing a plain failure here would erase the
        // distinction between verified launch abort and descendant cleanup.
        return Ok(());
    }
    if persist_interrupted_subagent_until(
        project_root,
        id,
        execution_generation,
        lease_id,
        reason,
        Some(deadline),
    ) {
        Ok(())
    } else {
        Err("failed to persist exact unstarted subagent outcome".to_string())
    }
}

fn persist_owner_lease_cleanup_error(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    cleanup_error: &str,
) {
    persist_owner_lease_cleanup_error_until(
        project_root,
        id,
        execution_generation,
        lease_id,
        cleanup_error,
        None,
    );
}

fn persist_owner_lease_cleanup_error_until(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    cleanup_error: &str,
    deadline: Option<Instant>,
) {
    let _ = update_subagent_record_until(project_root, id, deadline, |record| {
        if !record_matches_execution(record, execution_generation, lease_id)? {
            return Ok(());
        }
        let evidence = json!({
            "status": "failed",
            "error": cleanup_error,
            "cleanup_unverified": true,
            "recorded_at": Utc::now(),
        });
        match record.result.as_mut() {
            Some(Value::Object(result)) => {
                result.insert("cleanup_unverified".to_string(), Value::Bool(true));
                result.insert("owner_lease_cleanup".to_string(), evidence);
            }
            _ => {
                record.result = Some(json!({
                    "cleanup_unverified": true,
                    "owner_lease_cleanup": evidence,
                }));
            }
        }
        record.updated_at = Utc::now();
        Ok(())
    });
}

fn persist_owner_lease_cleanup_success_until(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    deadline: Option<Instant>,
) -> Result<SubagentRecord, String> {
    update_subagent_record_until(project_root, id, deadline, |record| {
        if !record_matches_execution(record, execution_generation, lease_id)? {
            return Err("terminal owner lease cleanup lost execution ownership".to_string());
        }
        let result = record
            .result
            .as_mut()
            .and_then(Value::as_object_mut)
            .ok_or("terminal owner lease cleanup result is not an object")?;
        result.insert("cleanup_unverified".to_string(), Value::Bool(false));
        result.insert(
            "owner_lease_cleanup".to_string(),
            json!({
                "status": "completed",
                "cleanup_unverified": false,
                "recorded_at": Utc::now(),
            }),
        );
        record.updated_at = Utc::now();
        Ok(record.clone())
    })
}

fn record_matches_execution(
    record: &SubagentRecord,
    execution_generation: u64,
    lease_id: &str,
) -> Result<bool, String> {
    validate_execution_ownership(execution_generation, lease_id)?;
    match (record.execution_generation, record.owner_lease.as_deref()) {
        (Some(record_generation), Some(record_lease)) => {
            validate_execution_ownership(record_generation, record_lease)?;
            Ok(record_generation == execution_generation && record_lease == lease_id)
        }
        _ => Err(format!(
            "running subagent {} has legacy execution ownership and cannot be mutated safely",
            record.id
        )),
    }
}

#[derive(Debug, Clone)]
struct PendingMergeIntent {
    branch_commit: String,
    parent_head: String,
    verification_command: String,
    active_merge_base: Option<String>,
}

fn validate_verification_evidence(
    command: &str,
    evidence: &VerificationEvidence,
    expected_worktree: &Path,
    allow_session_worktree: bool,
) -> Result<(), String> {
    if evidence.tool_name != "run_terminal" {
        return Err("verification evidence did not originate from run_terminal".to_string());
    }
    let audited_command = crate::tools::executor::redact_text(command);
    if evidence.command != audited_command {
        return Err("verification evidence command does not match merge request".to_string());
    }
    let evidence_worktree = evidence
        .worktree_path
        .canonicalize()
        .map_err(|error| format!("verification evidence worktree is unavailable: {error}"))?;
    let expected_worktree = expected_worktree
        .canonicalize()
        .map_err(|error| format!("verification worktree is unavailable: {error}"))?;
    if evidence_worktree != expected_worktree {
        return Err("verification evidence was produced in a different worktree".to_string());
    }
    if evidence.success {
        let output = evidence
            .output
            .as_ref()
            .ok_or("successful verification evidence is missing terminal output")?;
        if output.get("command").and_then(Value::as_str) != Some(audited_command.as_str())
            || output.get("exit_code").and_then(Value::as_i64) != Some(0)
        {
            return Err(
                "successful verification evidence has inconsistent terminal output".to_string(),
            );
        }
        let output_cwd = output
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or("successful verification evidence is missing terminal cwd")?
            .canonicalize()
            .map_err(|error| format!("verification terminal cwd is unavailable: {error}"))?;
        let allowed_session_root = expected_worktree
            .join(".nib")
            .join("worktrees")
            .join("sessions");
        let isolated_session_worktree = allow_session_worktree
            && output_cwd.starts_with(&allowed_session_root)
            && crate::fs_security::verify_directory_without_symlinks(&output_cwd).is_ok();
        if output_cwd != expected_worktree && !isolated_session_worktree {
            return Err("verification terminal executed outside the subagent worktree".to_string());
        }
    }
    Ok(())
}

fn begin_pending_merge(
    record: &mut SubagentRecord,
    verification_command: &str,
    branch_commit: &str,
    parent_head: &str,
) {
    let subagent_result = record.result.take();
    record.result = Some(json!({
        "subagent_result": subagent_result,
        "verification_command": verification_command,
        "merge_commit": branch_commit,
        "parent_head_before": parent_head,
        "active_merge_base": Value::Null,
        "merge_stdout": Value::Null,
    }));
    record.status = MERGE_PENDING_STATUS.to_string();
    record.error = None;
    record.updated_at = Utc::now();
}

fn pending_merge_intent(record: &SubagentRecord) -> Result<PendingMergeIntent, String> {
    let result = record
        .result
        .as_ref()
        .and_then(Value::as_object)
        .ok_or("pending merge record is missing structured integration evidence")?;
    let branch_commit = result
        .get("merge_commit")
        .and_then(Value::as_str)
        .filter(|commit| valid_git_object_id(commit))
        .ok_or("pending merge record has an invalid merge commit")?;
    let parent_head = result
        .get("parent_head_before")
        .and_then(Value::as_str)
        .filter(|commit| valid_git_object_id(commit))
        .ok_or("pending merge record has an invalid pre-merge parent HEAD")?;
    let verification_command = result
        .get("verification_command")
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
        .ok_or("pending merge record is missing its verification command")?;
    let active_merge_base = result
        .get("active_merge_base")
        .and_then(Value::as_str)
        .map(str::to_string);
    if active_merge_base
        .as_deref()
        .is_some_and(|commit| !valid_git_object_id(commit))
    {
        return Err("pending merge record has an invalid active merge base".to_string());
    }
    Ok(PendingMergeIntent {
        branch_commit: branch_commit.to_string(),
        parent_head: parent_head.to_string(),
        verification_command: verification_command.to_string(),
        active_merge_base,
    })
}

fn set_active_merge_base(record: &mut SubagentRecord, base: Option<&str>) {
    if let Some(result) = record.result.as_mut().and_then(Value::as_object_mut) {
        result.insert(
            "active_merge_base".to_string(),
            base.map_or(Value::Null, |value| Value::String(value.to_string())),
        );
    }
    record.updated_at = Utc::now();
}

fn valid_git_object_id(value: &str) -> bool {
    (40..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn require_record_branch_oid(record: &SubagentRecord, expected: &str) -> Result<(), String> {
    let branch_oid = record
        .branch_oid
        .as_deref()
        .filter(|oid| valid_git_object_id(oid))
        .ok_or_else(|| {
            format!(
                "subagent {} has no valid persisted branch ownership OID; branch cleanup is unsafe",
                record.id
            )
        })?;
    if branch_oid != expected {
        return Err(format!(
            "subagent {} branch ownership changed: persisted {branch_oid}, expected {expected}",
            record.id
        ));
    }
    let expected_branch = format!("nib/subagent/{}", record.id);
    if record.branch != expected_branch {
        return Err(format!(
            "subagent {} branch name is not owned by this record: {}",
            record.id, record.branch
        ));
    }
    Ok(())
}

fn pending_verification_root(
    project_root: &Path,
    record: &SubagentRecord,
) -> Result<PathBuf, String> {
    let intent = pending_merge_intent(record)?;
    if git_is_ancestor_sync(project_root, &intent.branch_commit, "HEAD")? {
        return Ok(project_root.to_path_buf());
    }
    match std::fs::symlink_metadata(&record.worktree_path) {
        Ok(_) => {
            validate_record_worktree(project_root, record)?;
            record
                .worktree_path
                .canonicalize()
                .map_err(|error| format!("subagent worktree is unavailable: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "pending subagent commit {} is not integrated and its worktree is unavailable",
            intent.branch_commit
        )),
        Err(error) => Err(format!("subagent worktree is unavailable: {error}")),
    }
}

async fn stage_subagent_changes(worktree: &Path) -> Result<(), String> {
    unstage_nib_runtime_state(worktree).await?;
    git_checked(worktree, ["add", "--all"]).await?;
    unstage_nib_runtime_state(worktree).await
}

async fn stage_and_commit_subagent_snapshot(worktree: &Path, id: &str) -> Result<(), String> {
    stage_subagent_changes(worktree).await?;
    let staged = git_output(worktree, ["diff", "--cached", "--quiet"]).await?;
    match staged.status.code() {
        Some(0) => Ok(()),
        Some(1) => {
            git_checked(
                worktree,
                [
                    "commit",
                    "--no-verify",
                    "--no-gpg-sign",
                    "-m",
                    &format!("nib subagent {id} changes"),
                ],
            )
            .await?;
            Ok(())
        }
        _ => Err(git_failure(&staged, "inspect staged subagent changes")),
    }
}

async fn create_immutable_subagent_snapshot(
    worktree: &Path,
    record: &SubagentRecord,
) -> Result<String, String> {
    stage_and_commit_subagent_snapshot(worktree, &record.id).await?;
    let snapshot_commit = git_stdout(worktree, ["rev-parse", "HEAD"]).await?;
    ensure_child_snapshot_unchanged(
        worktree,
        &record.branch,
        &snapshot_commit,
        "before verification",
    )
    .await?;
    Ok(snapshot_commit)
}

async fn ensure_child_snapshot_unchanged(
    worktree: &Path,
    branch: &str,
    snapshot_commit: &str,
    phase: &str,
) -> Result<(), String> {
    let head = git_stdout(worktree, ["rev-parse", "HEAD"]).await?;
    let branch_head = git_stdout(worktree, ["rev-parse", branch]).await?;
    if head != snapshot_commit || branch_head != snapshot_commit {
        return Err(format!(
            "child immutable snapshot changed {phase}: verified {snapshot_commit}, HEAD {head}, branch {branch_head}; fresh verification is required"
        ));
    }
    let status = git_output(
        worktree,
        [
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--",
            ".",
            NIB_EXCLUDE_PATHSPEC,
            NIB_DESCENDANTS_EXCLUDE_PATHSPEC,
        ],
    )
    .await?;
    require_git_success(&status, "inspect immutable child snapshot")?;
    if !status.stdout.is_empty() {
        return Err(format!(
            "child worktree has mergeable changes {phase} relative to immutable snapshot {snapshot_commit}: {}; fresh verification is required",
            String::from_utf8_lossy(&status.stdout).trim()
        ));
    }
    Ok(())
}

async fn unstage_nib_runtime_state(worktree: &Path) -> Result<(), String> {
    let staged_nib = git_output(
        worktree,
        ["diff", "--cached", "--name-only", "-z", "--", ".nib"],
    )
    .await?;
    require_git_success(&staged_nib, "inspect staged .nib paths")?;
    if !staged_nib.stdout.is_empty() {
        git_checked(worktree, ["reset", "-q", "HEAD", "--", ".nib"]).await?;
        let remaining = git_output(
            worktree,
            ["diff", "--cached", "--name-only", "-z", "--", ".nib"],
        )
        .await?;
        require_git_success(&remaining, "verify staged .nib exclusion")?;
        if !remaining.stdout.is_empty() {
            return Err("refusing to commit child runtime state from .nib".to_string());
        }
    }
    Ok(())
}

async fn ensure_parent_clean(project_root: &Path) -> Result<String, String> {
    let status = git_output(
        project_root,
        [
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--",
            ".",
            NIB_EXCLUDE_PATHSPEC,
            NIB_DESCENDANTS_EXCLUDE_PATHSPEC,
        ],
    )
    .await?;
    require_git_success(&status, "inspect parent worktree")?;
    if !status.stdout.is_empty() {
        return Err(format!(
            "parent worktree and index must be clean before subagent merge: {}",
            String::from_utf8_lossy(&status.stdout).trim()
        ));
    }
    git_stdout(project_root, ["rev-parse", "HEAD"]).await
}

async fn recover_interrupted_merge(
    project_root: &Path,
    record: &mut SubagentRecord,
    record_file: &mut File,
    intent: &PendingMergeIntent,
) -> Result<(), String> {
    let Some(active_merge_base) = intent.active_merge_base.as_deref() else {
        if let Some(merge_head) = git_optional_object_id(project_root, "MERGE_HEAD").await? {
            return Err(format!(
                "repository has an in-progress merge ({merge_head}) that is not owned by this subagent; nib left it untouched"
            ));
        }
        return Ok(());
    };
    let recovery =
        recover_owned_merge_state(project_root, active_merge_base, &intent.branch_commit).await?;
    set_active_merge_base(record, None);
    record.error = Some(match recovery {
        OwnedMergeRecovery::NoMerge => {
            "interrupted merge left no Git merge state and the recorded base is clean; retry will reconcile"
                .to_string()
        }
        OwnedMergeRecovery::Aborted => {
            "owned interrupted merge was aborted and restored before retry".to_string()
        }
        OwnedMergeRecovery::Integrated => {
            "interrupted merge commit is already integrated; retry will reconcile cleanup"
                .to_string()
        }
    });
    persist_subagent_record_revision(project_root, record, record_file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnedMergeRecovery {
    NoMerge,
    Aborted,
    Integrated,
}

async fn recover_owned_merge_state(
    project_root: &Path,
    active_merge_base: &str,
    branch_commit: &str,
) -> Result<OwnedMergeRecovery, String> {
    let merge_head = git_optional_object_id(project_root, "MERGE_HEAD").await?;
    let current_head = git_stdout(project_root, ["rev-parse", "HEAD"]).await?;
    let Some(merge_head) = merge_head else {
        if git_is_ancestor(project_root, branch_commit, "HEAD").await? {
            ensure_parent_clean(project_root).await.map_err(|error| {
                format!(
                    "recorded subagent commit is integrated but parent state is not clean: {error}"
                )
            })?;
            return Ok(OwnedMergeRecovery::Integrated);
        }
        if current_head != active_merge_base {
            return Err(format!(
                "interrupted subagent merge started at {active_merge_base}, but repository HEAD is {current_head} without MERGE_HEAD; nib left the repository untouched"
            ));
        }
        ensure_parent_clean(project_root).await.map_err(|error| {
            format!(
                "interrupted subagent merge has no MERGE_HEAD but parent state is ambiguous: {error}; nib left it untouched"
            )
        })?;
        return Ok(OwnedMergeRecovery::NoMerge);
    };

    if merge_head != branch_commit {
        return Err(format!(
            "repository MERGE_HEAD {merge_head} does not match this subagent's recorded merge commit {branch_commit}; nib left the unrelated merge untouched"
        ));
    }
    if current_head != active_merge_base {
        return Err(format!(
            "interrupted subagent merge started at {active_merge_base}, but repository HEAD is {current_head}; refusing to abort"
        ));
    }
    ensure_owned_merge_has_no_user_changes(project_root, active_merge_base, branch_commit).await?;

    let abort = git_output(project_root, ["merge", "--abort"]).await?;
    if !abort.status.success() {
        let failure = git_failure(&abort, "recover owned subagent merge");
        if failure.contains("index.lock") {
            return Err(format!(
                "{failure}; nib did not remove Git index.lock because ownership cannot be proven; ensure no Git process is active, remove the stale lock manually, then retry"
            ));
        }
        return Err(failure);
    }
    let restored_head = ensure_parent_clean(project_root).await?;
    if restored_head != active_merge_base {
        return Err(format!(
            "interrupted merge abort restored HEAD to {restored_head}, expected {active_merge_base}"
        ));
    }
    Ok(OwnedMergeRecovery::Aborted)
}

async fn git_optional_object_id(cwd: &Path, revision: &str) -> Result<Option<String>, String> {
    let output = git_output(cwd, ["rev-parse", "-q", "--verify", revision]).await?;
    match output.status.code() {
        Some(0) => {
            let object = String::from_utf8(output.stdout)
                .map_err(|error| format!("git {revision} output was not UTF-8: {error}"))?;
            let object = object.trim();
            if valid_git_object_id(object) {
                Ok(Some(object.to_string()))
            } else {
                Err(format!("git {revision} returned an invalid object ID"))
            }
        }
        Some(1) | Some(128) => Ok(None),
        _ => Err(git_failure(&output, &format!("inspect {revision}"))),
    }
}

async fn ensure_owned_merge_has_no_user_changes(
    project_root: &Path,
    active_merge_base: &str,
    branch_commit: &str,
) -> Result<(), String> {
    let common_base = git_stdout(
        project_root,
        ["merge-base", active_merge_base, branch_commit],
    )
    .await?;
    let expected = git_output(
        project_root,
        [
            "diff",
            "--name-only",
            "-z",
            common_base.as_str(),
            branch_commit,
            "--",
            ".",
            NIB_EXCLUDE_PATHSPEC,
            NIB_DESCENDANTS_EXCLUDE_PATHSPEC,
        ],
    )
    .await?;
    require_git_success(&expected, "inspect interrupted merge paths")?;
    let expected_paths = expected
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    let status = git_output(
        project_root,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--",
            ".",
            NIB_EXCLUDE_PATHSPEC,
            NIB_DESCENDANTS_EXCLUDE_PATHSPEC,
        ],
    )
    .await?;
    require_git_success(&status, "inspect interrupted merge state")?;

    let entries = status.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut index = 0;
    while index < entries.len() {
        let entry = entries[index];
        index += 1;
        if entry.is_empty() {
            continue;
        }
        if entry.len() < 4 || entry[2] != b' ' {
            return Err(
                "git returned malformed status while recovering interrupted merge".to_string(),
            );
        }
        let x = entry[0];
        let y = entry[1];
        let path = &entry[3..];
        let second_path = if matches!(x, b'R' | b'C') {
            let original = entries
                .get(index)
                .copied()
                .filter(|path| !path.is_empty())
                .ok_or("git returned malformed rename status while recovering interrupted merge")?;
            index += 1;
            Some(original)
        } else {
            None
        };
        let expected_path = expected_paths
            .iter()
            .any(|candidate| candidate.as_slice() == path)
            || second_path.is_some_and(|original| {
                expected_paths
                    .iter()
                    .any(|candidate| candidate.as_slice() == original)
            });
        let unmerged = matches!(
            (x, y),
            (b'D', b'D')
                | (b'A', b'U')
                | (b'U', b'D')
                | (b'U', b'A')
                | (b'D', b'U')
                | (b'A', b'A')
                | (b'U', b'U')
        );
        let merge_owned = expected_path && (unmerged || (x != b' ' && y == b' '));
        if !merge_owned {
            return Err(format!(
                "parent contains changes not proven to belong to the interrupted subagent merge ({x_char}{y_char} {path}); refusing to abort",
                x_char = char::from(x),
                y_char = char::from(y),
                path = String::from_utf8_lossy(path)
            ));
        }
    }
    Ok(())
}

async fn reconcile_pending_merge(
    project_root: &Path,
    record: &mut SubagentRecord,
    record_file: &mut File,
    intent: &PendingMergeIntent,
    evidence: &VerificationEvidence,
) -> Result<Value, String> {
    let premerge_head = match ensure_parent_clean(project_root).await {
        Ok(head) => head,
        Err(error) => {
            return persist_pending_merge_failure(project_root, record, record_file, error);
        }
    };
    let parent_advanced_since_intent = premerge_head != intent.parent_head;
    let already_integrated =
        match git_is_ancestor(project_root, &intent.branch_commit, "HEAD").await {
            Ok(integrated) => integrated,
            Err(error) => {
                return persist_pending_merge_failure(project_root, record, record_file, error);
            }
        };

    let mut merge_stdout = if already_integrated {
        set_active_merge_base(record, None);
        format!(
            "subagent commit {} is already integrated",
            intent.branch_commit
        )
    } else {
        set_active_merge_base(record, Some(&premerge_head));
        record.error = None;
        persist_subagent_record_revision(project_root, record, record_file)?;
        let merge = merge_recorded_commit(project_root, &record.id, &intent.branch_commit).await;
        let merge = match merge {
            Ok(output) => output,
            Err(error) => {
                let restored = restore_after_failed_merge(
                    project_root,
                    &premerge_head,
                    &intent.branch_commit,
                    error,
                )
                .await;
                if restored.clear_active {
                    set_active_merge_base(record, None);
                }
                return persist_pending_merge_failure(
                    project_root,
                    record,
                    record_file,
                    restored.error,
                );
            }
        };
        if !merge.status.success() {
            let restored = restore_after_failed_merge(
                project_root,
                &premerge_head,
                &intent.branch_commit,
                git_failure(&merge, "merge"),
            )
            .await;
            if restored.clear_active {
                set_active_merge_base(record, None);
            }
            return persist_pending_merge_failure(
                project_root,
                record,
                record_file,
                restored.error,
            );
        }
        set_active_merge_base(record, None);
        match git_is_ancestor(project_root, &intent.branch_commit, "HEAD").await {
            Ok(true) => {}
            Ok(false) => {
                return persist_pending_merge_failure(
                    project_root,
                    record,
                    record_file,
                    "git merge returned success without integrating the recorded subagent commit"
                        .to_string(),
                );
            }
            Err(error) => {
                return persist_pending_merge_failure(project_root, record, record_file, error);
            }
        }
        String::from_utf8_lossy(&merge.stdout).trim().to_string()
    };
    if parent_advanced_since_intent {
        merge_stdout.push_str(&format!(
            "\nparent HEAD advanced from {} to {} after merge intent was persisted",
            intent.parent_head, premerge_head
        ));
    }

    set_merge_stdout(record, &merge_stdout);
    record.error = None;
    record.updated_at = Utc::now();
    if let Err(error) = require_record_branch_oid(record, &intent.branch_commit) {
        return persist_pending_merge_failure(project_root, record, record_file, error);
    }
    if let Err(error) = crate::sandbox::worktree::Worktree::remove_reconciled_async(
        project_root,
        &record.id,
        &intent.branch_commit,
    )
    .await
    {
        return persist_pending_merge_failure(
            project_root,
            record,
            record_file,
            format!("worktree cleanup failed after merge: {error}"),
        );
    }

    record.status = "merged".to_string();
    record.error = None;
    record.updated_at = Utc::now();
    persist_subagent_record_revision(project_root, record, record_file)?;
    Ok(json!({
        "success": true,
        "subagent_id": record.id,
        "status": record.status,
        "verification_command": intent.verification_command,
        "verification_provider": evidence
            .output
            .as_ref()
            .and_then(|output| output.get("provider"))
            .cloned()
            .unwrap_or_else(|| Value::String(evidence.configured_provider.clone())),
        "stdout": merge_stdout,
    }))
}

async fn merge_recorded_commit(
    project_root: &Path,
    _subagent_id: &str,
    branch_commit: &str,
) -> Result<Output, String> {
    #[cfg(debug_assertions)]
    interrupt_recorded_merge_for_test(project_root, _subagent_id, branch_commit).await?;
    git_output(
        project_root,
        ["merge", "--no-edit", "--no-verify", branch_commit],
    )
    .await
}

#[cfg(debug_assertions)]
async fn interrupt_recorded_merge_for_test(
    project_root: &Path,
    subagent_id: &str,
    branch_commit: &str,
) -> Result<(), String> {
    let key = (project_root.to_path_buf(), subagent_id.to_string());
    let reached = MERGE_INTERRUPTION_TEST_BARRIERS
        .lock()
        .map_err(|_| "merge interruption test barrier registry is poisoned".to_string())?
        .remove(&key);
    let Some(reached) = reached else {
        return Ok(());
    };

    let setup = async {
        let merge = git_output(
            project_root,
            [
                "merge",
                "--no-commit",
                "--no-edit",
                "--no-verify",
                branch_commit,
            ],
        )
        .await?;
        require_git_success(&merge, "establish interrupted merge test fixture")?;
        let merge_head = git_optional_object_id(project_root, "MERGE_HEAD").await?;
        if merge_head.as_deref() != Some(branch_commit) {
            return Err(
                "interrupted merge test fixture did not retain the expected MERGE_HEAD".to_string(),
            );
        }
        let git_directory = git_stdout(project_root, ["rev-parse", "--absolute-git-dir"]).await?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(PathBuf::from(git_directory).join("index.lock"))
            .map_err(|error| {
                format!("failed to create interrupted merge test index lock: {error}")
            })?;
        Ok(())
    }
    .await;

    match setup {
        Ok(()) => {
            let _ = reached.send(Ok(()));
            std::future::pending::<Result<(), String>>().await
        }
        Err(error) => {
            let _ = reached.send(Err(error.clone()));
            Err(error)
        }
    }
}

struct MergeRestoreOutcome {
    error: String,
    clear_active: bool,
}

async fn restore_after_failed_merge(
    project_root: &Path,
    premerge_head: &str,
    branch_commit: &str,
    merge_error: String,
) -> MergeRestoreOutcome {
    let (error, clear_active) = match recover_owned_merge_state(
        project_root,
        premerge_head,
        branch_commit,
    )
    .await
    {
        Ok(OwnedMergeRecovery::Aborted) => (
            format!("{merge_error}; owned parent merge was aborted and restored; retry is allowed"),
            true,
        ),
        Ok(OwnedMergeRecovery::NoMerge) => (
            format!(
                "{merge_error}; no merge state remained and the parent is clean at the pre-merge HEAD; retry is allowed"
            ),
            true,
        ),
        Ok(OwnedMergeRecovery::Integrated) => (
            format!(
                "{merge_error}; the recorded subagent commit is already integrated; retry will reconcile cleanup"
            ),
            true,
        ),
        Err(recovery_error) => (
            format!(
                "{merge_error}; parent recovery failed closed without aborting ambiguous state: {recovery_error}"
            ),
            false,
        ),
    };
    MergeRestoreOutcome {
        error,
        clear_active,
    }
}

fn set_merge_stdout(record: &mut SubagentRecord, merge_stdout: &str) {
    if let Some(result) = record.result.as_mut().and_then(Value::as_object_mut) {
        result.insert(
            "merge_stdout".to_string(),
            Value::String(merge_stdout.to_string()),
        );
    }
}

async fn git_is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let output = git_output(cwd, ["merge-base", "--is-ancestor", ancestor, descendant]).await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(&output, "merge-base --is-ancestor")),
    }
}

fn git_is_ancestor_sync(cwd: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let output = crate::sandbox::worktree::run_git_bounded_sync(
        cwd,
        ["merge-base", "--is-ancestor", ancestor, descendant],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure(&output, "merge-base --is-ancestor")),
    }
}

fn persist_pending_merge_failure(
    project_root: &Path,
    record: &mut SubagentRecord,
    record_file: &mut File,
    error: String,
) -> Result<Value, String> {
    record.status = MERGE_PENDING_STATUS.to_string();
    record.error = Some(error.clone());
    record.updated_at = Utc::now();
    match persist_subagent_record_revision(project_root, record, record_file) {
        Ok(()) => Err(error),
        Err(persist_error) => Err(format!(
            "{error}; failed to persist pending merge evidence: {persist_error}"
        )),
    }
}

fn persist_merge_failure(
    project_root: &Path,
    record: &mut SubagentRecord,
    record_file: &mut File,
    error: String,
) -> Result<Value, String> {
    record.status = MERGE_FAILED_STATUS.to_string();
    record.error = Some(error.clone());
    record.updated_at = Utc::now();
    persist_subagent_record_revision(project_root, record, record_file)?;
    Err(error)
}

fn records_dir(project_root: &Path) -> PathBuf {
    project_root.join(".nib").join("subagents")
}

fn ensure_records_directory(project_root: &Path) -> Result<PathBuf, String> {
    ensure_records_directory_until(project_root, None)
}

fn ensure_records_directory_until(
    project_root: &Path,
    deadline: Option<Instant>,
) -> Result<PathBuf, String> {
    ensure_records_directory_until_with_phase_hook(
        project_root,
        deadline,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        |_| Ok(()),
    )
}

fn ensure_records_directory_capability_until(
    project_root: &Path,
    deadline: Option<Instant>,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let started = Instant::now();
    let effective_deadline = deadline.unwrap_or_else(|| {
        started
            .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
            .unwrap_or(started)
    });
    let directory = open_or_create_records_directory_capability_with_setup_hook(
        project_root,
        Some(effective_deadline),
        |_| Ok(()),
    )?;
    migrate_legacy_record_locks(project_root, directory.path(), Some(effective_deadline))?;
    directory.verify_visible()?;
    Ok(directory)
}

fn ensure_records_directory_until_with_phase_hook(
    project_root: &Path,
    deadline: Option<Instant>,
    default_timeout: Duration,
    after_open: impl FnOnce(Instant) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let started = Instant::now();
    let effective_deadline =
        deadline.unwrap_or_else(|| started.checked_add(default_timeout).unwrap_or(started));
    ensure_subagent_reconciliation_deadline(Some(effective_deadline))?;
    let directory = open_or_create_records_directory(project_root, Some(effective_deadline))?;
    after_open(effective_deadline)?;
    ensure_subagent_reconciliation_deadline(Some(effective_deadline))?;
    migrate_legacy_record_locks(project_root, &directory, Some(effective_deadline))?;
    Ok(directory)
}

fn open_or_create_records_directory(
    project_root: &Path,
    deadline: Option<Instant>,
) -> Result<PathBuf, String> {
    open_or_create_records_directory_with_setup_hook(project_root, deadline, |_| Ok(()))
}

fn open_or_create_records_directory_with_setup_hook(
    project_root: &Path,
    deadline: Option<Instant>,
    before_setup_mutation: impl FnMut(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    open_or_create_records_directory_capability_with_setup_hook(
        project_root,
        deadline,
        before_setup_mutation,
    )
    .map(|directory| directory.path().to_path_buf())
}

fn open_or_create_records_directory_capability_with_setup_hook(
    project_root: &Path,
    deadline: Option<Instant>,
    mut before_setup_mutation: impl FnMut(&Path) -> Result<(), String>,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let started = Instant::now();
    let setup_deadline = deadline.unwrap_or_else(|| {
        started
            .checked_add(SUBAGENT_RECORD_LOCK_TIMEOUT)
            .unwrap_or(started)
    });
    ensure_subagent_reconciliation_deadline(Some(setup_deadline))?;
    let nib = project_root.join(".nib");
    let project_directory = crate::daemons::state::StableDirectory::open(project_root)?;
    let mut setup_guard = || ensure_subagent_reconciliation_deadline(Some(setup_deadline));
    let nib_directory = project_directory.open_or_create_descendant_directory_with_guard(
        &nib,
        &mut setup_guard,
        &mut before_setup_mutation,
    )?;
    setup_guard()?;
    drop(nib_directory);
    let directory = records_dir(project_root);
    let lock_path = nib.join(".subagent-legacy-lock-migration.lock");
    let initialize = |nib_directory: &crate::daemons::state::StableDirectory,
                      lock_deadline: Instant| {
        ensure_subagent_reconciliation_deadline(Some(lock_deadline))?;
        match nib_directory.entry_kind(&directory)? {
            Some(crate::daemons::state::StableEntryKind::Directory) => {
                nib_directory.open_owned_child(&directory)
            }
            Some(crate::daemons::state::StableEntryKind::File) => Err(format!(
                "subagent records path must be a local project directory: {}",
                directory.display()
            )),
            None => {
                let staging = nib.join(NATIVE_RECORDS_STAGING_DIRECTORY);
                let records_directory = match nib_directory.entry_kind(&staging)? {
                    Some(crate::daemons::state::StableEntryKind::Directory) => {
                        open_valid_native_records_staging(nib_directory, &staging)?
                    }
                    Some(crate::daemons::state::StableEntryKind::File) => {
                        return Err(format!(
                            "native subagent namespace staging is unsafe and was preserved for inspection; verify it and remove only this exact entry before retrying `nib doctor --fix --confirm-no-legacy-processes`: {}",
                            staging.display()
                        ));
                    }
                    None => create_native_records_staging(nib_directory, &staging, lock_deadline)?,
                };
                ensure_subagent_reconciliation_deadline(Some(lock_deadline))?;
                if nib_directory.entry_kind(&directory)?.is_some() {
                    return Err(format!(
                        "subagent records namespace appeared during native-origin publication and was preserved: {}",
                        directory.display()
                    ));
                }
                nib_directory.rename_child_directory_until(
                    &staging,
                    &records_directory,
                    &directory,
                    lock_deadline,
                )?;
                ensure_subagent_reconciliation_deadline(Some(lock_deadline))?;
                nib_directory.open_owned_child(&directory)
            }
        }
    };
    let records = with_delegation_lock_in_deadline_with_setup_hook(
        &lock_path,
        &nib,
        setup_deadline,
        deadline.is_none().then_some(SUBAGENT_RECORD_LOCK_TIMEOUT),
        initialize,
        &mut before_setup_mutation,
    )?;
    let metadata = std::fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    validate_records_directory(project_root, &directory, &metadata)?;
    records.verify_visible()?;
    Ok(records)
}

fn create_native_records_staging(
    nib_directory: &crate::daemons::state::StableDirectory,
    staging: &Path,
    deadline: Instant,
) -> Result<crate::daemons::state::StableDirectory, String> {
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    let staged = nib_directory.create_owned_child_directory_until(staging, deadline)?;
    let now = Utc::now();
    let receipt = LegacyRecordLockMigrationReceipt {
        version: LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION,
        epoch_id: uuid::Uuid::new_v4().to_string(),
        records_identity: records_directory_identity(&staged)?,
        phase: LegacyRecordLockMigrationPhase::Completed,
        attested_at: now,
        completed_at: Some(now),
        artifacts: Vec::new(),
    };
    save_legacy_record_lock_migration_receipt(&staged, &receipt, Some(deadline))?;
    Ok(staged)
}

fn open_valid_native_records_staging(
    nib_directory: &crate::daemons::state::StableDirectory,
    staging: &Path,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let staged = nib_directory.open_owned_child(staging)?;
    let mut saw_receipt = false;
    staged
        .for_each_entry_bounded(2, 1024, |name| {
            if name != std::ffi::OsStr::new(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT) || saw_receipt {
                return Err(format!(
                    "native-origin staging contains an unowned or ambiguous entry: {}",
                    staging.join(name).display()
                ));
            }
            saw_receipt = true;
            Ok(())
        })
        .map_err(|error| {
            format!(
                "incomplete or ambiguous native subagent namespace staging was preserved for inspection; verify its contents and remove only this exact staging directory before retrying `nib doctor --fix --confirm-no-legacy-processes`: {}: {error}",
                staging.display()
            )
        })?;
    if !saw_receipt {
        return Err(format!(
            "incomplete native subagent namespace staging has no exact creation receipt and was preserved for inspection; verify its contents and remove only this exact staging directory before retrying `nib doctor --fix --confirm-no-legacy-processes`: {}",
            staging.display(),
        ));
    }
    let receipt = load_legacy_record_lock_migration_receipt(&staged)
        .and_then(|receipt| receipt.ok_or_else(|| "native-origin receipt is absent".to_string()))
        .and_then(|receipt| {
            validate_legacy_record_lock_migration_receipt(&staged, &receipt)?;
            if receipt.phase != LegacyRecordLockMigrationPhase::Completed
                || !receipt.artifacts.is_empty()
            {
                return Err("native-origin receipt is not complete".to_string());
            }
            Ok(receipt)
        });
    receipt.map(|_| staged).map_err(|error| {
        format!(
            "incomplete or ambiguous native subagent namespace staging was preserved for inspection; verify its contents and remove only this exact staging directory before retrying `nib doctor --fix --confirm-no-legacy-processes`: {}: {error}",
            staging.display()
        )
    })
}

fn validate_records_directory(
    project_root: &Path,
    directory: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), String> {
    crate::fs_security::verify_directory_without_symlinks(directory)
        .map_err(|error| format!("subagent records path is unsafe: {error}"))?;
    let canonical = directory
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent records: {error}"))?;
    let within_project =
        crate::fs_security::canonical_path_starts_with(&canonical, project_root)
            .map_err(|error| format!("failed to resolve subagent project root: {error}"))?;
    if crate::fs_security::metadata_is_link_or_reparse(metadata)
        || !metadata.is_dir()
        || !within_project
    {
        return Err(format!(
            "subagent records path must be a local project directory: {}",
            directory.display()
        ));
    }
    Ok(())
}

fn record_lock_path(project_root: &Path, id: &str) -> Result<PathBuf, String> {
    if !is_valid_subagent_id(id) {
        return Err("invalid subagent id".to_string());
    }
    let directory = project_root.join(".nib");
    crate::fs_security::ensure_directory_without_symlinks(&directory)
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    let canonical = directory
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent record locks: {error}"))?;
    let within_project =
        crate::fs_security::canonical_path_starts_with(&canonical, project_root)
            .map_err(|error| format!("failed to resolve subagent project root: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !within_project {
        return Err(format!(
            "subagent record lock path must be a local project directory: {}",
            directory.display()
        ));
    }
    Ok(directory.join(format!(
        ".subagent-record-stripe-{:02}.lock",
        subagent_record_lock_stripe(id)
    )))
}

fn record_lock_path_for_stripe(
    records: &crate::daemons::state::StableDirectory,
    stripe: usize,
) -> Result<PathBuf, String> {
    if stripe >= SUBAGENT_RECORD_LOCK_STRIPES {
        return Err("subagent record lock stripe is out of range".to_string());
    }
    let nib = records.path().parent().ok_or_else(|| {
        format!(
            "subagent records directory has no state parent: {}",
            records.path().display()
        )
    })?;
    Ok(nib.join(format!(".subagent-record-stripe-{stripe:02}.lock")))
}

fn acquire_spawn_preparation_authority_until(
    records: &crate::daemons::state::StableDirectory,
    id: &str,
    deadline: Instant,
) -> Result<std::sync::Arc<SpawnPreparationAuthority>, String> {
    if !is_valid_subagent_id(id) {
        return Err("invalid subagent id".to_string());
    }
    records.verify_visible()?;
    let migration_fence = acquire_spawn_preparation_fence_until(records, deadline)?;
    let lock_path = record_lock_path_for_stripe(records, subagent_record_lock_stripe(id))?;
    let record_stripe =
        crate::daemons::state::acquire_file_lock_in_until_bound(&lock_path, records, deadline)?;
    let authority = std::sync::Arc::new(SpawnPreparationAuthority {
        records: records.try_clone()?,
        migration_fence,
        record_stripe,
        operation_deadline: deadline,
    });
    authority.verify_until(deadline)?;
    Ok(authority)
}

fn acquire_spawn_preparation_fence_until(
    records: &crate::daemons::state::StableDirectory,
    deadline: Instant,
) -> Result<crate::daemons::state::HeldFileLock, String> {
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    let nib = records.path().parent().ok_or_else(|| {
        format!(
            "subagent records directory has no state parent: {}",
            records.path().display()
        )
    })?;
    let path = nib.join(".subagent-legacy-lock-migration.lock");
    let lock = crate::daemons::state::acquire_file_lock_in_until_bound(&path, records, deadline)?;
    records.verify_visible()?;
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    Ok(lock)
}

#[cfg(test)]
fn legacy_record_lock_path(records: &Path, id: &str) -> Result<PathBuf, String> {
    if !is_valid_subagent_id(id) {
        return Err("invalid subagent id".to_string());
    }
    Ok(records.join(".locks").join(format!("{id}.lock")))
}

fn subagent_record_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash % SUBAGENT_RECORD_LOCK_STRIPES as u64) as usize
}

fn migrate_legacy_record_locks(
    project_root: &Path,
    records: &Path,
    deadline: Option<Instant>,
) -> Result<(), String> {
    migrate_legacy_record_locks_with_scan_hook(project_root, records, deadline, |_| Ok(()))
}

/// Performs the one explicitly attested offline migration from legacy per-record
/// locks to the fixed modern stripe namespace.
///
/// The caller's confirmation is the external quiescence proof: every prior nib
/// binary must already be stopped and disabled before this function is entered.
pub fn confirm_no_legacy_subagent_processes(project_root: &Path) -> Result<usize, String> {
    confirm_no_legacy_subagent_processes_with_scan_hook(project_root, |_| Ok(()))
}

fn confirm_no_legacy_subagent_processes_with_scan_hook(
    project_root: &Path,
    mut after_scan: impl FnMut(usize) -> Result<(), String>,
) -> Result<usize, String> {
    let project_root = canonical_project_root(project_root)?;
    let deadline = Instant::now() + SUBAGENT_RECORD_LOCK_TIMEOUT;
    let records = open_or_create_records_directory(&project_root, Some(deadline))?;
    let lock_path = project_root
        .join(".nib")
        .join(".subagent-legacy-lock-migration.lock");
    with_bounded_delegation_lock_in_until(
        &lock_path,
        &records,
        deadline,
        |records_directory, deadline| {
            let scan = scan_legacy_record_lock_namespaces(records_directory, Some(deadline))?;
            let existing = load_legacy_record_lock_migration_receipt(records_directory)?;
            let mut receipt = match existing {
                Some(receipt)
                    if receipt.phase == LegacyRecordLockMigrationPhase::Pending
                        && validate_legacy_record_lock_migration_receipt(
                            records_directory,
                            &receipt,
                        )
                        .is_ok()
                        && validate_legacy_record_lock_migration_artifacts(
                            records_directory,
                            &receipt,
                            &scan,
                        )
                        .is_ok() =>
                {
                    receipt
                }
                _ => LegacyRecordLockMigrationReceipt {
                    version: LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION,
                    epoch_id: uuid::Uuid::new_v4().to_string(),
                    records_identity: records_directory_identity(records_directory)?,
                    phase: LegacyRecordLockMigrationPhase::Pending,
                    attested_at: Utc::now(),
                    completed_at: None,
                    artifacts: scan.artifacts.clone(),
                },
            };
            save_legacy_record_lock_migration_receipt(records_directory, &receipt, Some(deadline))?;
            let artifact_count = receipt.artifacts.len();
            if let Err(error) = migrate_legacy_record_locks_locked_with_scan_hook(
                &project_root,
                records_directory,
                Some(deadline),
                &receipt,
                &mut after_scan,
            ) {
                receipt.phase = LegacyRecordLockMigrationPhase::Rejected;
                let rejection = save_legacy_record_lock_migration_receipt(
                    records_directory,
                    &receipt,
                    Some(deadline),
                )
                .err();
                return Err(match rejection {
                    Some(rejection) => format!(
                        "offline legacy-lock migration failed: {error}; its receipt could not be rejected safely: {rejection}"
                    ),
                    None => format!(
                        "offline legacy-lock migration failed and requires a fresh operator confirmation: {error}"
                    ),
                });
            }
            receipt.phase = LegacyRecordLockMigrationPhase::Completed;
            receipt.completed_at = Some(Utc::now());
            receipt.artifacts.clear();
            save_legacy_record_lock_migration_receipt(records_directory, &receipt, Some(deadline))?;
            Ok(artifact_count)
        },
    )
}

fn migrate_legacy_record_locks_with_scan_hook(
    project_root: &Path,
    records: &Path,
    deadline: Option<Instant>,
    mut after_scan: impl FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    let lock_path = project_root
        .join(".nib")
        .join(".subagent-legacy-lock-migration.lock");
    match deadline {
        Some(deadline) => with_bounded_delegation_lock_in_until(
            &lock_path,
            records,
            deadline,
            |records_directory, deadline| {
                reconcile_legacy_record_lock_migration_locked(
                    project_root,
                    records_directory,
                    Some(deadline),
                    &mut after_scan,
                )
            },
        ),
        None => with_bounded_delegation_lock_in(
            &lock_path,
            records,
            SUBAGENT_RECORD_LOCK_TIMEOUT,
            |records_directory, deadline| {
                reconcile_legacy_record_lock_migration_locked(
                    project_root,
                    records_directory,
                    Some(deadline),
                    &mut after_scan,
                )
            },
        ),
    }
}

struct LegacyRecordLockScan {
    records_directory: crate::daemons::state::StableDirectory,
    legacy_directory: Option<crate::daemons::state::StableDirectory>,
    ids: Vec<String>,
    retained_quarantines: Vec<PathBuf>,
    artifacts: Vec<LegacyRecordLockMigrationArtifact>,
}

impl LegacyRecordLockScan {
    fn is_clean(&self) -> bool {
        self.ids.is_empty() && self.retained_quarantines.is_empty()
    }

    fn verify_namespace_identity(&self) -> Result<(), String> {
        self.records_directory.verify_visible()?;
        if let Some(legacy_directory) = &self.legacy_directory {
            legacy_directory.verify_visible()?;
        }
        Ok(())
    }
}

fn scan_legacy_record_lock_namespaces(
    records_directory: &crate::daemons::state::StableDirectory,
    deadline: Option<Instant>,
) -> Result<LegacyRecordLockScan, String> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    records_directory.verify_visible()?;
    let records = records_directory.path();
    let legacy_locks = records.join(".locks");
    let legacy_directory = match records_directory.entry_kind(&legacy_locks)? {
        Some(crate::daemons::state::StableEntryKind::Directory) => {
            Some(records_directory.open_child(&legacy_locks)?)
        }
        Some(crate::daemons::state::StableEntryKind::File) => {
            return Err(format!(
                "legacy subagent lock path is unsafe: {}",
                legacy_locks.display()
            ));
        }
        None => None,
    };
    let mut ids = std::collections::HashSet::new();
    let mut retained_quarantines = Vec::new();
    let mut artifacts = Vec::new();
    if let Some(legacy_directory) = &legacy_directory {
        legacy_directory.for_each_entry_bounded(
            MAX_LEGACY_RECORD_LOCK_ENTRIES,
            MAX_LEGACY_RECORD_LOCK_NAME_BYTES,
            |name| {
                ensure_subagent_reconciliation_deadline(deadline)?;
                if exact_deletion_quarantine_name(&name, ".nib-legacy-lock-delete-") {
                    let path = legacy_locks.join(&name);
                    retained_quarantines.push(path.clone());
                    artifacts.push(snapshot_legacy_migration_artifact(
                        records_directory,
                        legacy_directory,
                        &path,
                        None,
                    )?);
                    return Ok(());
                }
                let Some(name) = name.to_str() else {
                    return Err("legacy lock namespace contains a non-UTF-8 filename".to_string());
                };
                if name.starts_with(".nib-legacy-lock-delete-") {
                    return Err(
                        "legacy lock namespace contains an invalid deletion quarantine".to_string(),
                    );
                }
                let Some(id) = name
                    .strip_suffix(".lock")
                    .filter(|id| is_valid_subagent_id(id))
                else {
                    return Ok(());
                };
                ids.insert(id.to_string());
                let path = legacy_locks.join(name);
                let quarantine_path = legacy_directory.deterministic_artifact_path(
                    &path,
                    ".nib-legacy-lock-delete-",
                    ".quarantine",
                )?;
                artifacts.push(snapshot_legacy_migration_artifact(
                    records_directory,
                    legacy_directory,
                    &path,
                    Some(quarantine_path),
                )?);
                Ok(())
            },
        )?;
    }
    let anchor_prefix = ".nib-lock-6-.locks-";
    let anchor_suffix = ".lock.anchor";
    records_directory.for_each_entry_bounded(
        MAX_SUBAGENT_DIRECTORY_ENTRIES,
        MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
        |name| {
            ensure_subagent_reconciliation_deadline(deadline)?;
            if exact_deletion_quarantine_name(&name, ".nib-legacy-lock-delete-") {
                let path = records.join(name);
                retained_quarantines.push(path.clone());
                artifacts.push(snapshot_legacy_migration_artifact(
                    records_directory,
                    records_directory,
                    &path,
                    None,
                )?);
                return Ok(());
            }
            let Some(name) = name.to_str() else {
                return Ok(());
            };
            if name.starts_with(".nib-legacy-lock-delete-") {
                return Err(
                    "subagent record namespace contains an invalid legacy-lock deletion quarantine"
                        .to_string(),
                );
            }
            if let Some(id) = name
                .strip_prefix(anchor_prefix)
                .and_then(|name| name.strip_suffix(anchor_suffix))
                .filter(|id| is_valid_subagent_id(id))
            {
                ids.insert(id.to_string());
                let path = records.join(name);
                let quarantine_path = records_directory.deterministic_artifact_path(
                    &path,
                    ".nib-legacy-lock-delete-",
                    ".quarantine",
                )?;
                artifacts.push(snapshot_legacy_migration_artifact(
                    records_directory,
                    records_directory,
                    &path,
                    Some(quarantine_path),
                )?);
            }
            Ok(())
        },
    )?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    let mut ids = ids.into_iter().collect::<Vec<_>>();
    ids.sort();
    retained_quarantines.sort();
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    artifacts.dedup_by(|left, right| left.path == right.path && left.identity == right.identity);
    Ok(LegacyRecordLockScan {
        records_directory: records_directory.try_clone()?,
        legacy_directory,
        ids,
        retained_quarantines,
        artifacts,
    })
}

fn snapshot_legacy_migration_artifact(
    records_directory: &crate::daemons::state::StableDirectory,
    artifact_directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    quarantine_path: Option<PathBuf>,
) -> Result<LegacyRecordLockMigrationArtifact, String> {
    let relative = path
        .strip_prefix(records_directory.path())
        .map_err(|_| {
            format!(
                "legacy lock artifact escaped records state: {}",
                path.display()
            )
        })?
        .to_path_buf();
    let quarantine_path = quarantine_path
        .map(|path| {
            path.strip_prefix(records_directory.path())
                .map(Path::to_path_buf)
                .map_err(|_| {
                    format!(
                        "legacy lock quarantine escaped records state: {}",
                        path.display()
                    )
                })
        })
        .transpose()?;
    let file = artifact_directory.open_read_write(path)?;
    let identity = crate::fs_security::file_identity_snapshot(&file)
        .map_err(|error| format!("failed to identify legacy lock {}: {error}", path.display()))?;
    artifact_directory.verify_file_identity(path, &file)?;
    Ok(LegacyRecordLockMigrationArtifact {
        path: relative,
        quarantine_path,
        identity,
    })
}

fn reconcile_legacy_record_lock_migration_locked(
    _project_root: &Path,
    records_directory: &crate::daemons::state::StableDirectory,
    deadline: Option<Instant>,
    after_scan: &mut impl FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    let scan = scan_legacy_record_lock_namespaces(records_directory, deadline)?;
    drop(scan);
    after_scan(0)?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    records_directory.verify_visible()?;
    let scan = scan_legacy_record_lock_namespaces(records_directory, deadline)?;
    let receipt = load_legacy_record_lock_migration_receipt(records_directory)?;
    let receipt = receipt.ok_or_else(legacy_record_lock_offline_migration_required)?;
    validate_legacy_record_lock_migration_receipt(records_directory, &receipt)?;
    if receipt.phase != LegacyRecordLockMigrationPhase::Completed || !scan.is_clean() {
        return Err(legacy_record_lock_offline_migration_required());
    }
    Ok(())
}

fn legacy_record_lock_offline_migration_required() -> String {
    "legacy per-ID subagent locks require an offline migration; stop and disable every prior nib binary, then run `nib doctor --fix --confirm-no-legacy-processes` from this project before retrying"
        .to_string()
}

fn legacy_record_lock_migration_receipt_path(
    records_directory: &crate::daemons::state::StableDirectory,
) -> PathBuf {
    records_directory
        .path()
        .join(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT)
}

fn records_directory_identity(
    records_directory: &crate::daemons::state::StableDirectory,
) -> Result<crate::fs_security::DirectoryIdentity, String> {
    records_directory
        .directory_removal_receipt()
        .map(|receipt| receipt.identity())
}

fn load_legacy_record_lock_migration_receipt(
    records_directory: &crate::daemons::state::StableDirectory,
) -> Result<Option<LegacyRecordLockMigrationReceipt>, String> {
    let path = legacy_record_lock_migration_receipt_path(records_directory);
    if !records_directory.path_exists(&path)? {
        return Ok(None);
    }
    let file = records_directory.open_read(&path)?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES {
        return Err(format!(
            "legacy lock migration receipt is invalid or exceeds {} bytes: {}",
            MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES,
            path.display()
        ));
    }
    records_directory.verify_file_identity(&path, &file)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&file)
        .take(MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES {
        return Err(format!(
            "legacy lock migration receipt exceeds {} bytes: {}",
            MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES,
            path.display()
        ));
    }
    records_directory.verify_file_identity(&path, &file)?;
    let receipt = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid legacy lock migration receipt: {error}"))?;
    Ok(Some(receipt))
}

fn validate_legacy_record_lock_migration_receipt(
    records_directory: &crate::daemons::state::StableDirectory,
    receipt: &LegacyRecordLockMigrationReceipt,
) -> Result<(), String> {
    if receipt.version != LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION
        || uuid::Uuid::parse_str(&receipt.epoch_id).is_err()
        || receipt.records_identity != records_directory_identity(records_directory)?
        || receipt.artifacts.len() > MAX_LEGACY_RECORD_LOCK_ENTRIES * 2
        || (receipt.phase == LegacyRecordLockMigrationPhase::Completed
            && (!receipt.artifacts.is_empty() || receipt.completed_at.is_none()))
        || (receipt.phase != LegacyRecordLockMigrationPhase::Completed
            && receipt.completed_at.is_some())
    {
        return Err("legacy lock migration receipt is invalid or belongs to a replaced records directory; run the offline doctor migration again".to_string());
    }
    let mut paths = std::collections::HashSet::new();
    for artifact in &receipt.artifacts {
        validate_legacy_record_lock_receipt_relative_path(&artifact.path)?;
        if let Some(path) = &artifact.quarantine_path {
            validate_legacy_record_lock_receipt_relative_path(path)?;
        }
        if !paths.insert(artifact.path.clone()) {
            return Err("legacy lock migration receipt contains duplicate artifacts".to_string());
        }
    }
    Ok(())
}

fn validate_legacy_record_lock_receipt_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("legacy lock migration receipt contains an unsafe artifact path".to_string());
    }
    Ok(())
}

fn validate_legacy_record_lock_migration_artifacts(
    records_directory: &crate::daemons::state::StableDirectory,
    receipt: &LegacyRecordLockMigrationReceipt,
    scan: &LegacyRecordLockScan,
) -> Result<(), String> {
    validate_legacy_record_lock_migration_receipt(records_directory, receipt)?;
    for current in &scan.artifacts {
        let matches = receipt.artifacts.iter().any(|expected| {
            expected.identity == current.identity
                && (expected.path == current.path
                    || expected.quarantine_path.as_ref() == Some(&current.path))
        });
        if !matches {
            return Err(format!(
                "legacy lock state changed after offline quiescence attestation; artifacts were preserved and a fresh `nib doctor --fix --confirm-no-legacy-processes` run is required: {}",
                current.path.display()
            ));
        }
    }
    Ok(())
}

fn save_legacy_record_lock_migration_receipt(
    records_directory: &crate::daemons::state::StableDirectory,
    receipt: &LegacyRecordLockMigrationReceipt,
    deadline: Option<Instant>,
) -> Result<(), String> {
    validate_legacy_record_lock_migration_receipt(records_directory, receipt)?;
    let path = legacy_record_lock_migration_receipt_path(records_directory);
    let encoded = serde_json::to_vec_pretty(receipt).map_err(|error| error.to_string())?;
    if encoded.len() as u64 > MAX_LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_BYTES {
        return Err("legacy lock migration receipt exceeds its publication bound".to_string());
    }
    for attempt in 0..2 {
        ensure_subagent_reconciliation_deadline(deadline)?;
        let expected_file = if records_directory.path_exists(&path)? {
            Some(records_directory.open_read(&path)?)
        } else {
            None
        };
        let expected = expected_file
            .as_ref()
            .map_or(crate::daemons::state::FileExpectation::Missing, |file| {
                crate::daemons::state::FileExpectation::Present(file)
            });
        let result = records_directory.save_bytes_atomically_expected_with_guard_and_hook(
            &path,
            &encoded,
            ".nib-subagent-legacy-migration-",
            true,
            expected,
            || {
                ensure_subagent_reconciliation_deadline(deadline)?;
                records_directory.verify_visible()
            },
            || ensure_subagent_reconciliation_deadline(deadline),
        );
        match result {
            Ok(()) => return Ok(()),
            Err(_) if attempt == 0 => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded migration receipt save loop always returns")
}

fn migrate_legacy_record_locks_locked_with_scan_hook(
    project_root: &Path,
    records_directory: &crate::daemons::state::StableDirectory,
    deadline: Option<Instant>,
    receipt: &LegacyRecordLockMigrationReceipt,
    after_scan: &mut impl FnMut(usize) -> Result<(), String>,
) -> Result<(), String> {
    let records = records_directory.path();
    let legacy_locks = records.join(".locks");
    for pass in 0..MAX_LEGACY_RECORD_LOCK_MIGRATION_PASSES {
        ensure_subagent_reconciliation_deadline(deadline)?;
        records_directory.verify_visible()?;
        let scan = scan_legacy_record_lock_namespaces(records_directory, deadline)?;
        drop(scan);
        after_scan(pass)?;
        let scan = scan_legacy_record_lock_namespaces(records_directory, deadline)?;
        validate_legacy_record_lock_migration_artifacts(records_directory, receipt, &scan)?;
        let LegacyRecordLockScan {
            records_directory,
            legacy_directory,
            ids,
            retained_quarantines,
            artifacts: _,
        } = scan;
        ensure_subagent_reconciliation_deadline(deadline)?;
        records_directory.verify_visible()?;
        if let Some(legacy_directory) = &legacy_directory {
            legacy_directory.verify_visible()?;
        }
        let initial_legacy_present = legacy_directory.is_some();
        for id in ids {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let name = format!("{id}.lock");
            let legacy_path = legacy_locks.join(&name);
            let anchor_path = crate::daemons::state::daemon_lock_anchor_path(&legacy_path)?;
            with_modern_subagent_record_lock_in(
                &record_lock_path(project_root, &id)?,
                &records_directory,
                deadline,
                |protected_records, deadline| {
                    ensure_subagent_reconciliation_deadline(deadline)?;
                    let protected_legacy = match protected_records.entry_kind(&legacy_locks)? {
                        Some(crate::daemons::state::StableEntryKind::Directory) => {
                            Some(protected_records.open_child(&legacy_locks)?)
                        }
                        Some(crate::daemons::state::StableEntryKind::File) => {
                            return Err(format!(
                                "legacy subagent lock path is unsafe: {}",
                                legacy_locks.display()
                            ));
                        }
                        None => None,
                    };
                    if let Some(protected_legacy) = protected_legacy.as_ref() {
                        crate::daemons::state::cleanup_legacy_lock_pair_with_guard(
                            protected_legacy,
                            &legacy_path,
                            protected_records,
                            &anchor_path,
                            || {
                                ensure_subagent_reconciliation_deadline(deadline)?;
                                protected_legacy.verify_visible()
                            },
                        )
                    } else {
                        crate::daemons::state::cleanup_legacy_lock_pair_optional_with_guard(
                            None,
                            &legacy_path,
                            protected_records,
                            &anchor_path,
                            || {
                                ensure_subagent_reconciliation_deadline(deadline)?;
                                if protected_records.entry_kind(&legacy_locks)?.is_some() {
                                    return Err(format!(
                                "legacy subagent lock namespace appeared during anchor cleanup; artifacts were preserved: {}",
                                legacy_locks.display()
                            ));
                                }
                                Ok(())
                            },
                        )
                    }
                },
            )?;
        }
        for quarantine in retained_quarantines {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let directory = if quarantine.parent() == Some(legacy_locks.as_path()) {
                legacy_directory
                    .as_ref()
                    .ok_or("legacy lock deletion quarantine lost its containing directory")?
            } else {
                &records_directory
            };
            if !directory.path_exists(&quarantine)? {
                continue;
            }
            let file = directory.open_read_write(&quarantine)?;
            match file.try_lock() {
                Ok(()) => directory.remove_visible_file_if_matches_direct_with_guard(
                    &quarantine,
                    &file,
                    || ensure_subagent_reconciliation_deadline(deadline),
                )?,
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "legacy lock deletion quarantine is still owned and was preserved: {}",
                        quarantine.display()
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to inspect legacy lock deletion quarantine {}: {error}",
                        quarantine.display()
                    ));
                }
            }
        }
        ensure_subagent_reconciliation_deadline(deadline)?;
        records_directory.verify_visible()?;
        if let Some(legacy_directory) = &legacy_directory {
            legacy_directory.verify_visible()?;
        }
        let final_scan = scan_legacy_record_lock_namespaces(&records_directory, deadline)?;
        validate_legacy_record_lock_migration_artifacts(&records_directory, receipt, &final_scan)?;
        if final_scan.is_clean() && final_scan.legacy_directory.is_some() == initial_legacy_present
        {
            final_scan.verify_namespace_identity()?;
            ensure_subagent_reconciliation_deadline(deadline)?;
            return Ok(());
        }
    }
    Err(format!(
        "legacy subagent lock namespace did not stabilize within {MAX_LEGACY_RECORD_LOCK_MIGRATION_PASSES} passes"
    ))
}

fn is_valid_subagent_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn read_subagent_record(path: &Path) -> Result<SubagentRecord, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("subagent record has no parent: {}", path.display()))?;
    let directory = crate::daemons::state::StableDirectory::open(parent)?;
    read_subagent_record_in(&directory, path)
}

fn read_subagent_record_in(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<SubagentRecord, String> {
    read_opened_subagent_record_in(directory, path).map(|opened| opened.record)
}

fn read_opened_subagent_record_in(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<OpenedSubagentRecord, String> {
    if !directory.path_exists(path)? {
        return Err(format!("subagent record {} not found", path.display()));
    }
    let file = directory.open_read(path)?;
    let opened_metadata = file.metadata().map_err(|error| error.to_string())?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_SUBAGENT_RECORD_BYTES {
        return Err(format!(
            "subagent record {} exceeds the {MAX_SUBAGENT_RECORD_BYTES}-byte limit or is not a regular file",
            path.display()
        ));
    }
    directory.verify_file_identity(path, &file)?;
    let mut contents = Vec::with_capacity(opened_metadata.len() as usize);
    (&file)
        .take(MAX_SUBAGENT_RECORD_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| error.to_string())?;
    if contents.len() as u64 > MAX_SUBAGENT_RECORD_BYTES {
        return Err(format!(
            "subagent record {} exceeds the {MAX_SUBAGENT_RECORD_BYTES}-byte limit",
            path.display()
        ));
    }
    directory.verify_file_identity(path, &file)?;
    let record = serde_json::from_slice(&contents)
        .map_err(|error| format!("invalid subagent record {}: {error}", path.display()))?;
    Ok(OpenedSubagentRecord { record, file })
}

fn update_subagent_record<T>(
    project_root: &Path,
    id: &str,
    update: impl FnOnce(&mut SubagentRecord) -> Result<T, String>,
) -> Result<T, String> {
    update_subagent_record_until(project_root, id, None, update)
}

fn update_subagent_record_until<T>(
    project_root: &Path,
    id: &str,
    deadline: Option<Instant>,
    update: impl FnOnce(&mut SubagentRecord) -> Result<T, String>,
) -> Result<T, String> {
    let project_root = canonical_project_root(project_root)?;
    let path = record_path(&project_root, id)?;
    let records_directory = ensure_records_directory_until(&project_root, deadline)?;
    with_subagent_reconciliation_lock_in(
        &project_root,
        id,
        &records_directory,
        deadline,
        |directory, deadline| {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let mut opened = read_opened_subagent_record_in(directory, &path)?;
            let result = update(&mut opened.record)?;
            ensure_subagent_reconciliation_deadline(deadline)?;
            write_subagent_record_unlocked_until(
                &project_root,
                directory,
                &path,
                &opened.record,
                crate::daemons::state::FileExpectation::Present(&opened.file),
                deadline,
            )?;
            Ok(result)
        },
    )
}

fn record_path(project_root: &Path, id: &str) -> Result<PathBuf, String> {
    if !is_valid_subagent_id(id) {
        return Err("invalid subagent id".to_string());
    }
    Ok(records_dir(project_root).join(format!("{id}.json")))
}

fn canonical_project_root(project_root: &Path) -> Result<PathBuf, String> {
    let root = project_root
        .canonicalize()
        .map_err(|error| format!("invalid project root {}: {error}", project_root.display()))?;
    if !root.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            root.display()
        ));
    }
    Ok(root)
}

fn validate_record_worktree(project_root: &Path, record: &SubagentRecord) -> Result<(), String> {
    let worktree = record
        .worktree_path
        .canonicalize()
        .map_err(|error| format!("subagent worktree is unavailable: {error}"))?;
    let allowed = project_root
        .join(".nib")
        .join("worktrees")
        .join("subagents")
        .canonicalize()
        .map_err(|error| format!("subagent worktree root is unavailable: {error}"))?;
    if !worktree.starts_with(&allowed) {
        return Err(format!(
            "subagent worktree {} is outside {}",
            worktree.display(),
            allowed.display()
        ));
    }
    Ok(())
}

async fn git_output<I, S>(cwd: &Path, args: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    crate::sandbox::worktree::run_git_bounded(cwd, args).await
}

async fn git_checked<I, S>(cwd: &Path, args: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = git_output(cwd, args).await?;
    if !output.status.success() {
        return Err(git_failure(&output, "command"));
    }
    Ok(output)
}

async fn git_stdout<I, S>(cwd: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = git_checked(cwd, args).await?;
    let value = String::from_utf8(output.stdout)
        .map_err(|error| format!("git output was not UTF-8: {error}"))?;
    let value = value.trim();
    if value.is_empty() {
        Err("git command returned empty output".to_string())
    } else {
        Ok(value.to_string())
    }
}

fn require_git_success(output: &Output, operation: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(output, operation))
    }
}

fn git_failure(output: &Output, operation: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    format!("git {operation} failed: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SpawnPreparationTimeoutGuard {
        previous: Option<Duration>,
        _not_send_or_sync: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    impl SpawnPreparationTimeoutGuard {
        fn set(timeout: Duration) -> Self {
            let previous = TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT.with(|slot| {
                let previous = slot.get();
                slot.set(Some(timeout));
                previous
            });
            Self {
                previous,
                _not_send_or_sync: std::marker::PhantomData,
            }
        }
    }

    impl Drop for SpawnPreparationTimeoutGuard {
        fn drop(&mut self) {
            TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT.with(|slot| slot.set(self.previous));
        }
    }

    struct SpawnReconciliationTimeoutGuard {
        previous: Option<Duration>,
        _not_send_or_sync: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    impl SpawnReconciliationTimeoutGuard {
        fn set(timeout: Duration) -> Self {
            let previous = TEST_SPAWN_RECONCILIATION_TIMEOUT.with(|slot| {
                let previous = slot.get();
                slot.set(Some(timeout));
                previous
            });
            Self {
                previous,
                _not_send_or_sync: std::marker::PhantomData,
            }
        }
    }

    impl Drop for SpawnReconciliationTimeoutGuard {
        fn drop(&mut self) {
            TEST_SPAWN_RECONCILIATION_TIMEOUT.with(|slot| slot.set(self.previous));
        }
    }

    struct SpawnAuthorityVerifyHookGuard;

    impl SpawnAuthorityVerifyHookGuard {
        fn install(hook: SpawnAuthorityVerifyHook) -> Self {
            assert!(
                SPAWN_AUTHORITY_VERIFY_HOOK
                    .lock()
                    .expect("spawn authority verify hook lock")
                    .replace(hook)
                    .is_none(),
                "spawn authority verify hook already installed"
            );
            Self
        }
    }

    impl Drop for SpawnAuthorityVerifyHookGuard {
        fn drop(&mut self) {
            SPAWN_AUTHORITY_VERIFY_HOOK
                .lock()
                .expect("spawn authority verify hook lock")
                .take();
        }
    }

    struct SpawnHandoffPhaseHookGuard;

    impl SpawnHandoffPhaseHookGuard {
        fn install(hook: impl FnMut(&'static str) + 'static) -> Self {
            SPAWN_HANDOFF_PHASE_HOOK.with(|slot| {
                assert!(
                    slot.borrow_mut().replace(Box::new(hook)).is_none(),
                    "spawn handoff hook already installed"
                );
            });
            Self
        }
    }

    impl Drop for SpawnHandoffPhaseHookGuard {
        fn drop(&mut self) {
            SPAWN_HANDOFF_PHASE_HOOK.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    const MERGE_LOCK_CHILD_PROJECT_ROOT: &str = "NIB_TEST_MERGE_LOCK_PROJECT_ROOT";
    const MERGE_LOCK_CHILD_EXPECTATION: &str = "NIB_TEST_MERGE_LOCK_EXPECTATION";
    const RECORD_WRITE_CHILD_PROJECT_ROOT: &str = "NIB_TEST_SUBAGENT_WRITE_PROJECT_ROOT";
    const LEGACY_OPEN_CHILD_PROJECT_ROOT: &str = "NIB_TEST_LEGACY_OPEN_PROJECT_ROOT";
    const LEGACY_OPEN_CHILD_READY: &str = "NIB_TEST_LEGACY_OPEN_READY";
    const LEGACY_OPEN_CHILD_RESUME: &str = "NIB_TEST_LEGACY_OPEN_RESUME";
    const LEGACY_OPEN_CHILD_LOCKED: &str = "NIB_TEST_LEGACY_OPEN_LOCKED";
    const LEGACY_OPEN_CHILD_RELEASE: &str = "NIB_TEST_LEGACY_OPEN_RELEASE";

    #[test]
    fn supervisor_protocol_rejects_partial_eof_on_every_platform() {
        let mut partial = std::io::Cursor::new(br#"{"version":1,"phase":"commit"}"#.to_vec());
        let error = read_subagent_supervisor_frame(&mut partial, "COMMIT")
            .expect_err("a frame without its newline is incomplete");
        assert!(
            error.contains("closed before a complete COMMIT frame"),
            "{error}"
        );
    }

    #[test]
    fn supervisor_protocol_rejects_exact_identity_mismatch_on_every_platform() {
        #[cfg(target_os = "linux")]
        let backend = crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace;
        #[cfg(windows)]
        let backend = crate::sandbox::process::ProcessScopeBackend::WindowsJobObject;
        #[cfg(target_os = "macos")]
        let backend = crate::sandbox::process::ProcessScopeBackend::MacosProcessGroup;
        let now = Utc::now();
        let scope = crate::sandbox::process::ProcessScopeRecord {
            version: 2,
            scope_id: "sub-protocol-identity".to_string(),
            workload_kind: "subagent".to_string(),
            execution_generation: 17,
            cleanup_lease_id: uuid::Uuid::new_v4().to_string(),
            supervisor_registration_nonce: Some(uuid::Uuid::new_v4().to_string()),
            owner: crate::sandbox::process::ProcessIdentity::current().expect("process identity"),
            backend,
            status: crate::sandbox::process::ProcessScopeStatus::Prepared,
            launch_committed: Some(false),
            supervisor: None,
            direct_child: None,
            cleanup_reason: None,
            cleanup_proof: None,
            launch_abort_proof: None,
            created_at: now,
            updated_at: now,
        };
        let expected = SubagentSupervisorFrame {
            version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
            phase: "ready".to_string(),
            handoff_nonce: uuid::Uuid::new_v4().to_string(),
            subagent_id: scope.scope_id.clone(),
            execution_generation: scope.execution_generation,
            owner_lease: uuid::Uuid::new_v4().to_string(),
            process_scope: scope,
        };
        let mut mismatched = expected.clone();
        mismatched.phase = "commit".to_string();
        mismatched.process_scope.cleanup_lease_id = uuid::Uuid::new_v4().to_string();
        let error = validate_subagent_supervisor_frame(&expected, &mismatched, "commit")
            .expect_err("a mismatched process-scope authority must be rejected");
        assert!(error.contains("exact execution authority"), "{error}");
    }
    const LEGACY_OPEN_CHILD_ID: &str = "open-before-lock-contender";
    const OWNER_LOSS_CHILD_PROJECT_ROOT: &str = "NIB_TEST_OWNER_LOSS_PROJECT_ROOT";
    const OWNER_LOSS_CHILD_READY: &str = "NIB_TEST_OWNER_LOSS_READY";
    const OWNER_LOSS_SUBAGENT_ID: &str = "sub-owner-process-loss";
    const PREPARATION_CRASH_CHILD_PROJECT_ROOT: &str = "NIB_TEST_PREPARATION_CRASH_PROJECT_ROOT";
    const HANDOFF_CRASH_CHILD_PROJECT_ROOT: &str = "NIB_TEST_HANDOFF_CRASH_PROJECT_ROOT";
    const HANDOFF_CRASH_CHILD_PHASE: &str = "NIB_TEST_HANDOFF_CRASH_PHASE";
    const HANDOFF_CRASH_CHILD_READY: &str = "NIB_TEST_HANDOFF_CRASH_READY";

    fn subagent_namespace_snapshot(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(root: &Path, current: &Path, snapshot: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries = std::fs::read_dir(current)
                .expect("read subagent namespace")
                .map(|entry| entry.expect("subagent namespace entry"))
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let entry_path = entry.path();
                let relative = entry_path
                    .strip_prefix(root)
                    .expect("subagent namespace entry is below root")
                    .to_path_buf();
                if entry
                    .file_type()
                    .expect("subagent namespace entry type")
                    .is_dir()
                {
                    snapshot.push((relative, Vec::new()));
                    visit(root, &entry_path, snapshot);
                } else {
                    snapshot.push((
                        relative,
                        crate::fs_security::read_namespace_snapshot_file(&entry_path)
                            .expect("subagent namespace bytes"),
                    ));
                }
            }
        }

        let mut snapshot = Vec::new();
        visit(path, path, &mut snapshot);
        snapshot
    }

    fn assert_spawn_cleanup_snapshot(
        before: &[(PathBuf, Vec<u8>)],
        after: &[(PathBuf, Vec<u8>)],
        context: &str,
    ) {
        let before_map = before
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let after_map = after
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let changed = before_map
            .keys()
            .chain(after_map.keys())
            .filter(|path| before_map.get(*path) != after_map.get(*path))
            .filter(|path| {
                let rendered = path.to_string_lossy().replace('\\', "/");
                let complete_worktree_proof = rendered.starts_with(".nib/worktree-ownership/")
                    && rendered.ends_with(".json")
                    && before_map.get(*path).is_none()
                    && after_map.get(*path).is_some_and(|bytes| {
                        serde_json::from_slice::<Value>(bytes).is_ok_and(|record| {
                            record.get("phase").and_then(Value::as_str) == Some("complete")
                                && record.get("path_cleanup").and_then(Value::as_str)
                                    == Some("removed")
                                && record.get("registration_cleanup").and_then(Value::as_str)
                                    == Some("removed")
                                && record.get("branch_cleanup").and_then(Value::as_str)
                                    == Some("removed")
                        })
                    });
                rendered != ".nib/subagents/.preparations"
                    && !(rendered.starts_with(".nib/.subagent-record-stripe-")
                        && rendered.ends_with(".lock"))
                    && !(rendered
                        .starts_with(".git/nib/locks/.nib-lock-4-.nib-.subagent-record-stripe-")
                        && rendered.ends_with(".lock.anchor"))
                    && !complete_worktree_proof
            })
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert!(changed.is_empty(), "{context}: {changed:?}");
    }

    fn assert_subagent_namespace_unchanged(
        before: &[(PathBuf, Vec<u8>)],
        after: &[(PathBuf, Vec<u8>)],
        context: &str,
    ) {
        let before = before
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let after = after
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let added = after
            .keys()
            .filter(|path| !before.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        let removed = before
            .keys()
            .filter(|path| !after.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();
        let changed = before
            .iter()
            .filter_map(|(path, bytes)| {
                after
                    .get(path)
                    .filter(|after| *after != bytes)
                    .map(|_| path.clone())
            })
            .collect::<Vec<_>>();
        assert!(
            added.is_empty() && removed.is_empty() && changed.is_empty(),
            "{context}: added={added:?}, removed={removed:?}, changed={changed:?}"
        );
    }

    fn initialize_spawn_test_repository(root: &Path) {
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "nib-tests@example.invalid"],
            vec!["config", "user.name", "nib tests"],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .expect("run git fixture command");
            assert!(status.success());
        }
        std::fs::write(root.join("README.md"), b"spawn fixture\n").expect("fixture file");
        let status = std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(root)
            .status()
            .expect("stage fixture");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args(["commit", "--quiet", "-m", "fixture"])
            .current_dir(root)
            .status()
            .expect("commit fixture");
        assert!(status.success());
    }

    fn prime_fixed_subagent_record_lock_namespace(project_root: &Path) {
        let records = ensure_records_directory_capability_until(project_root, None)
            .expect("authorized records for fixed lock priming");
        let timeout = if cfg!(windows) {
            Duration::from_secs(15)
        } else {
            SUBAGENT_RECORD_LOCK_TIMEOUT
        };
        let deadline = Instant::now() + timeout;
        let _fence = acquire_spawn_preparation_fence_until(&records, deadline)
            .expect("global preparation fence for fixed lock priming");
        for stripe in 0..SUBAGENT_RECORD_LOCK_STRIPES {
            let path = record_lock_path_for_stripe(&records, stripe).expect("fixed stripe path");
            let lock =
                crate::daemons::state::acquire_file_lock_in_until_bound(&path, &records, deadline)
                    .expect("prime fixed record stripe");
            lock.verify_until(deadline)
                .expect("verify primed fixed record stripe");
        }
    }

    #[test]
    fn fallback_preparation_crash_child() {
        let Some(project_root) = std::env::var_os(PREPARATION_CRASH_CHILD_PROJECT_ROOT) else {
            return;
        };
        #[cfg(windows)]
        let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(15));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("child runtime");
        runtime.block_on(async {
            let _ = spawn_subagent(
                &json!({"prompt": "pause after durable fallback audit preparation"}),
                &PathBuf::from(project_root),
            );
        });
        panic!("crash child unexpectedly passed the preparation boundary");
    }

    #[test]
    fn spawn_handoff_crash_child() {
        let Some(project_root) = std::env::var_os(HANDOFF_CRASH_CHILD_PROJECT_ROOT) else {
            return;
        };
        #[cfg(windows)]
        let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(10));
        let phase = std::env::var(HANDOFF_CRASH_CHILD_PHASE).expect("handoff crash phase");
        let ready = PathBuf::from(
            std::env::var_os(HANDOFF_CRASH_CHILD_READY).expect("handoff crash ready path"),
        );
        let _hook = SpawnHandoffPhaseHookGuard::install(move |observed| {
            if observed == phase {
                std::fs::write(&ready, observed).expect("publish handoff crash readiness");
                loop {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("handoff crash child runtime");
        let result = runtime.block_on(async {
            spawn_subagent(
                &json!({"prompt": "pause at durable handoff boundary"}),
                &PathBuf::from(project_root),
            )
        });
        panic!("handoff crash child unexpectedly passed its boundary: {result:?}");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn killed_spawn_handoff_boundaries_never_leave_unlaunchable_running_state() {
        for phase in [
            "record_published",
            "manager_registered",
            "handoff_established",
            "handoff_proven",
            "handoff_committed",
            "launch_released",
            "before_intent_retirement",
            "intent_retired",
        ] {
            let root = tempfile::tempdir().expect("handoff crash project");
            initialize_spawn_test_repository(root.path());
            let ready = root.path().join(format!("handoff-{phase}.ready"));
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args([
                        "--exact",
                        "tools::delegation::tests::spawn_handoff_crash_child",
                        "--nocapture",
                    ])
                    .env(HANDOFF_CRASH_CHILD_PROJECT_ROOT, root.path())
                    .env(HANDOFF_CRASH_CHILD_PHASE, phase)
                    .env(HANDOFF_CRASH_CHILD_READY, &ready)
                    .spawn()
                    .expect("spawn handoff crash child");
            let started = Instant::now();
            while !ready.exists() {
                if let Some(status) = child.try_wait().expect("inspect handoff crash child") {
                    panic!("handoff crash child exited before {phase}: {status}");
                }
                assert!(
                    started.elapsed() < Duration::from_secs(20),
                    "handoff crash child did not reach {phase}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            child.kill().expect("kill handoff crash child");
            child.wait().expect("reap handoff crash child");

            let listed = list_subagents(root.path())
                .unwrap_or_else(|error| panic!("{phase}: restart reconciliation failed: {error}"));
            assert!(
                listed.iter().all(|record| !matches!(
                    record.get("status").and_then(Value::as_str).unwrap_or(""),
                    "running" | "recovery_required"
                )),
                "{phase}: restart exposed an unlaunchable record: {listed:?}"
            );
            let preparations = spawn_preparation_directory_path(root.path());
            assert!(
                !preparations.exists()
                    || std::fs::read_dir(&preparations)
                        .expect("read reconciled handoff preparations")
                        .next()
                        .is_none(),
                "{phase}: restart did not retire preparation last"
            );
            assert!(
                !owner_lease_directory(root.path())
                    .read_dir()
                    .is_ok_and(|mut entries| entries.next().is_some()),
                "{phase}: restart left owner authority"
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn killed_fallback_preparation_is_reconciled_without_orphans() {
        let root = tempfile::tempdir().expect("project root");
        initialize_spawn_test_repository(root.path());
        let ready = root.path().join("audit-preparation.ready");
        let resume = root.path().join("audit-preparation.resume");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::fallback_preparation_crash_child",
                "--nocapture",
            ])
            .env(PREPARATION_CRASH_CHILD_PROJECT_ROOT, root.path())
            .env("NIB_TEST_SUBAGENT_AUDIT_PREPARED_READY", &ready)
            .env("NIB_TEST_SUBAGENT_AUDIT_PREPARED_RESUME", &resume)
            .spawn()
            .expect("spawn preparation child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect preparation child") {
                panic!("preparation child exited before crash boundary: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "preparation child did not reach crash boundary"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let subagent_id = std::fs::read_to_string(&ready).expect("prepared subagent id");
        let intent =
            spawn_preparation_directory_path(root.path()).join(format!("{subagent_id}.json"));
        assert!(
            intent.is_file(),
            "write-ahead intent precedes crash boundary"
        );
        let audit_session = root
            .path()
            .join(".nib/profiles/default/sessions")
            .join(format!("{subagent_id}.json"));
        assert!(
            audit_session.is_file(),
            "audit leaf is published before crash"
        );

        child.kill().expect("kill preparation child");
        child.wait().expect("reap preparation child");
        let listed = list_subagents(root.path()).expect("restart reconciliation");
        assert!(
            listed.is_empty(),
            "preparing intent is never public workload"
        );
        assert!(!intent.exists(), "reconciled intent removed last");
        assert!(!audit_session.exists(), "exact prepared audit leaf removed");
        assert!(!owner_lease_directory(root.path())
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some()));
        assert!(!root
            .path()
            .join(".nib/worktrees/subagents")
            .join(&subagent_id)
            .exists());

        SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("retry runtime");
        let retry = runtime.block_on(async {
            spawn_subagent(
                &json!({"prompt": "fresh retry after preparation recovery"}),
                root.path(),
            )
        });
        let error = retry.expect_err("injected retry publication failure");
        assert!(error.contains("injected initial subagent record publication failure"));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn killed_planned_intent_precedes_every_spawn_resource_and_reconciles() {
        let root = tempfile::tempdir().expect("project root");
        initialize_spawn_test_repository(root.path());
        let ready = root.path().join("spawn-intent-planned.ready");
        let resume = root.path().join("spawn-intent-planned.resume");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::fallback_preparation_crash_child",
                "--nocapture",
            ])
            .env(PREPARATION_CRASH_CHILD_PROJECT_ROOT, root.path())
            .env("NIB_TEST_SUBAGENT_INTENT_PLANNED_READY", &ready)
            .env("NIB_TEST_SUBAGENT_INTENT_PLANNED_RESUME", &resume)
            .spawn()
            .expect("spawn planned-intent child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect planned child") {
                panic!("planned-intent child exited before boundary: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "planned-intent child did not reach boundary"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let id = std::fs::read_to_string(&ready).expect("planned subagent id");
        let intent = spawn_preparation_directory_path(root.path()).join(format!("{id}.json"));
        assert!(intent.is_file(), "planned intent is durable");
        assert!(
            !root
                .path()
                .join(".nib/worktrees/subagents")
                .join(&id)
                .exists(),
            "planned boundary precedes worktree mutation"
        );
        assert!(
            !owner_lease_directory(root.path())
                .read_dir()
                .is_ok_and(|mut entries| entries.next().is_some()),
            "planned boundary precedes owner mutation"
        );
        assert!(
            !root
                .path()
                .join(".nib/profiles/default/sessions")
                .join(format!("{id}.json"))
                .exists(),
            "planned boundary precedes audit mutation"
        );
        child.kill().expect("kill planned child");
        child.wait().expect("reap planned child");
        assert!(list_subagents(root.path())
            .expect("restart reconciliation")
            .is_empty());
        assert!(!intent.exists(), "planned intent reconciled last");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn spawn_intent_and_session_atomic_phase_crashes_reconcile_exactly() {
        let _reconciliation_timeout = SpawnReconciliationTimeoutGuard::set(Duration::from_secs(15));
        let categories = [
            ("intent-initial", ".preparations", "\"revision\": 0"),
            ("intent-resources", ".preparations", "\"revision\": 1"),
            ("intent-audit-planned", ".preparations", "\"revision\": 2"),
            ("intent-audit-published", ".preparations", "\"revision\": 3"),
            ("session-leaf", "sessions", "\"messages\": []"),
        ];
        for (category, component, content) in categories {
            for phase in [
                "temporary_create",
                "after_evacuation",
                "canonical_publish",
                "directory_sync",
                "receipt_return",
            ] {
                let root = tempfile::tempdir().expect("project root");
                initialize_spawn_test_repository(root.path());
                let ready = root.path().join(format!("{category}-{phase}.ready"));
                let resume = root.path().join(format!("{category}-{phase}.resume"));
                let mut child =
                    std::process::Command::new(std::env::current_exe().expect("test binary"))
                        .args([
                            "--exact",
                            "tools::delegation::tests::fallback_preparation_crash_child",
                            "--nocapture",
                        ])
                        .env(PREPARATION_CRASH_CHILD_PROJECT_ROOT, root.path())
                        .env("NIB_TEST_ATOMIC_PUBLICATION_PHASE", phase)
                        .env("NIB_TEST_ATOMIC_PUBLICATION_PATH_COMPONENT", component)
                        .env("NIB_TEST_ATOMIC_PUBLICATION_CONTENT", content)
                        .env("NIB_TEST_ATOMIC_PUBLICATION_READY", &ready)
                        .env("NIB_TEST_ATOMIC_PUBLICATION_RESUME", &resume)
                        .spawn()
                        .expect("spawn atomic-phase child");
                let started = Instant::now();
                while !ready.exists() {
                    if let Some(status) = child.try_wait().expect("inspect atomic child") {
                        panic!("atomic child exited before {category}/{phase} boundary: {status}");
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(20),
                        "atomic child did not reach {category}/{phase} boundary"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                let target = PathBuf::from(
                    std::fs::read_to_string(&ready).expect("UTF-8 atomic target path"),
                );
                let id = target
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .expect("atomic target id")
                    .to_string();
                child.kill().expect("kill atomic child");
                child.wait().expect("reap atomic child");
                assert!(list_subagents(root.path())
                    .unwrap_or_else(|error| {
                        panic!("reconcile {category}/{phase} restart: {error}")
                    })
                    .is_empty());
                assert!(
                    !spawn_preparation_directory_path(root.path())
                        .join(format!("{id}.json"))
                        .exists(),
                    "{category}/{phase} preparation remained"
                );
                assert!(
                    !root
                        .path()
                        .join(".nib/worktrees/subagents")
                        .join(&id)
                        .exists(),
                    "{category}/{phase} worktree remained"
                );
                assert!(
                    !root
                        .path()
                        .join(".nib/profiles/default/sessions")
                        .join(format!("{id}.json"))
                        .exists(),
                    "{category}/{phase} audit session remained"
                );
                assert!(
                    !root.path().join(".nib/profiles/default").exists(),
                    "{category}/{phase} left the transaction-owned profile/session directory tree"
                );
                let namespace = subagent_namespace_snapshot(root.path());
                assert!(
                    namespace.iter().all(|(path, _)| {
                        !path
                            .file_name()
                            .is_some_and(|name| name.as_encoded_bytes().starts_with(b".nib-session-"))
                            && path.file_name()
                                != Some(std::ffi::OsStr::new(".session-directory.identity"))
                    }),
                    "{category}/{phase} left a session transaction, marker, or anchor artifact: {namespace:?}"
                );
            }
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn killed_fallback_namespace_phases_reconcile_exact_planned_artifacts() {
        for phase in ["directory", "marker", "anchor", "sync", "final"] {
            let root = tempfile::tempdir().expect("project root");
            initialize_spawn_test_repository(root.path());
            let ready = root
                .path()
                .join(format!("session-preparation-{phase}.ready"));
            let resume = root
                .path()
                .join(format!("session-preparation-{phase}.resume"));
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args([
                        "--exact",
                        "tools::delegation::tests::fallback_preparation_crash_child",
                        "--nocapture",
                    ])
                    .env(PREPARATION_CRASH_CHILD_PROJECT_ROOT, root.path())
                    .env("NIB_TEST_SESSION_PREPARATION_PHASE", phase)
                    .env("NIB_TEST_SESSION_PREPARATION_READY", &ready)
                    .env("NIB_TEST_SESSION_PREPARATION_RESUME", &resume)
                    .spawn()
                    .expect("spawn phase child");
            let started = Instant::now();
            while !ready.exists() {
                if let Some(status) = child.try_wait().expect("inspect phase child") {
                    panic!("phase child exited before {phase} boundary: {status}");
                }
                assert!(
                    started.elapsed() < Duration::from_secs(20),
                    "phase child did not reach {phase} boundary"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(
                std::fs::read_to_string(&ready).expect("phase readiness"),
                phase
            );
            let preparation_dir = spawn_preparation_directory_path(root.path());
            let intents = std::fs::read_dir(&preparation_dir)
                .expect("durable preparation namespace")
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.path().extension().and_then(|value| value.to_str()) == Some("json")
                })
                .collect::<Vec<_>>();
            assert_eq!(intents.len(), 1, "{phase}: intent precedes mutation");
            let subagent_id = intents[0]
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .expect("intent id")
                .to_string();

            child.kill().expect("kill phase child");
            child.wait().expect("reap phase child");
            assert!(
                list_subagents(root.path())
                    .unwrap_or_else(|error| panic!("{phase}: restart reconciliation: {error}"))
                    .is_empty(),
                "{phase}: preparing state stays private"
            );
            assert!(
                !intents[0].path().exists(),
                "{phase}: intent is removed last"
            );
            assert!(
                !root.path().join(".nib/profiles").exists(),
                "{phase}: exact planned audit hierarchy is removed"
            );
            assert!(!root
                .path()
                .join(".nib/worktrees/subagents")
                .join(&subagent_id)
                .exists());
            assert!(!owner_lease_directory(root.path())
                .read_dir()
                .is_ok_and(|mut entries| entries.next().is_some()));
        }
    }

    fn directory_tree_snapshot(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(root: &Path, current: &Path, snapshot: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries = std::fs::read_dir(current)
                .expect("read namespace tree")
                .map(|entry| entry.expect("namespace tree entry"))
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let entry_path = entry.path();
                let relative = entry_path
                    .strip_prefix(root)
                    .expect("namespace entry is below root")
                    .to_path_buf();
                let file_type = entry.file_type().expect("namespace entry type");
                if file_type.is_dir() {
                    snapshot.push((relative.clone(), Vec::new()));
                    visit(root, &entry_path, snapshot);
                } else if file_type.is_symlink() {
                    snapshot.push((
                        relative,
                        std::fs::read_link(entry_path)
                            .expect("namespace symlink target")
                            .as_os_str()
                            .as_encoded_bytes()
                            .to_vec(),
                    ));
                } else {
                    snapshot.push((
                        relative,
                        std::fs::read(entry_path).expect("namespace tree bytes"),
                    ));
                }
            }
        }

        let mut snapshot = Vec::new();
        visit(path, path, &mut snapshot);
        snapshot
    }

    fn record_fixture(root: &Path, id: &str, status: &str) -> SubagentRecord {
        SubagentRecord {
            id: id.to_string(),
            parent_session_id: Some("parent".to_string()),
            child_session_id: format!("child-{id}"),
            prompt: "fixture".to_string(),
            status: status.to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: root.join("worktree"),
            branch: format!("nib/subagent/{id}"),
            branch_oid: None,
            result: None,
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn attach_execution_ownership(record: &mut SubagentRecord, owner_lease: &SubagentOwnerLease) {
        record.execution_generation = Some(owner_lease.execution_generation);
        record.owner_lease = Some(owner_lease.lease_id.clone());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn direct_spawn_variants_reject_non_utf8_audit_target_before_any_partial_state() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("UTF-8 project root");
        let non_utf8_state = root
            .path()
            .join(OsString::from_vec(b"profile-state-\xff".to_vec()));
        std::fs::create_dir(&non_utf8_state).expect("non-UTF-8 state target");
        let nib = root.path().join(".nib");
        std::fs::create_dir(&nib).expect("nib state root");
        symlink(&non_utf8_state, nib.join("selected-state")).expect("in-root state symlink");
        let mut config = crate::config::NibConfig::default();
        config.profiles = crate::config::ProfilesConfig {
            default: "selected".to_string(),
            active: vec![crate::config::ProfileConfig {
                id: "selected".to_string(),
                root: PathBuf::from("."),
                state_dir: Some(PathBuf::from(".nib/selected-state")),
                ..crate::config::ProfileConfig::default()
            }],
        };
        crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");
        assert!(
            non_utf8_state.join("sessions").to_str().is_none(),
            "fixture audit destination must contain non-UTF-8"
        );
        let namespace_before = directory_tree_snapshot(root.path());

        let args = json!({"prompt": "must fail before direct delegation state"});
        let sync_error = spawn_subagent(&args, root.path())
            .expect_err("sync spawn must reject a non-serializable audit target");
        assert_eq!(sync_error, SUBAGENT_AUDIT_DESTINATION_ENCODING_ERROR);
        let cancellable_error = spawn_subagent_cancellable(&args, root.path(), None)
            .await
            .expect_err("cancellable spawn must reject a non-serializable audit target");
        assert_eq!(cancellable_error, SUBAGENT_AUDIT_DESTINATION_ENCODING_ERROR);

        assert_eq!(directory_tree_snapshot(root.path()), namespace_before);
        for path in [
            non_utf8_state.join("sessions"),
            non_utf8_state.join("daemons"),
            non_utf8_state.join("managed-skills"),
            root.path().join(".nib/subagents"),
            root.path().join(".nib/subagent-owner-leases"),
            root.path().join(".nib/worktrees/subagents"),
        ] {
            assert!(
                !path.exists(),
                "direct audit-target failure created delegation state at {}",
                path.display()
            );
        }
    }

    #[tokio::test]
    async fn already_cancelled_cancellable_spawn_is_a_whole_namespace_noop() {
        let root = tempfile::tempdir().expect("project root");
        let before = subagent_namespace_snapshot(root.path());
        let cancellation = crate::agent::CancellationSignal::new();
        assert!(cancellation.cancel());

        let error = spawn_subagent_cancellable(
            &json!({"prompt": "must not mutate"}),
            root.path(),
            Some(&cancellation),
        )
        .await
        .expect_err("already-cancelled spawn must stop before mutation");
        assert!(error.contains("cancelled before mutation"));
        assert_eq!(subagent_namespace_snapshot(root.path()), before);
    }

    #[test]
    fn fallback_child_bootstrap_uses_the_workspace_selected_profile_snapshot() {
        let root = tempfile::tempdir().expect("project root");
        let default_workspace = root.path().join("default-workspace");
        std::fs::create_dir(&default_workspace).expect("default workspace");
        let mut config = crate::config::NibConfig::default();
        config.profiles = crate::config::ProfilesConfig {
            default: "default-profile".to_string(),
            active: vec![
                crate::config::ProfileConfig {
                    id: "default-profile".to_string(),
                    root: PathBuf::from("default-workspace"),
                    env_file: Some(PathBuf::from("default.env")),
                    active_skills: vec!["default-secret-skill".to_string()],
                    skill_paths: vec![PathBuf::from("default-skills")],
                    ..crate::config::ProfileConfig::default()
                },
                crate::config::ProfileConfig {
                    id: "selected-profile".to_string(),
                    root: PathBuf::from("."),
                    env_file: Some(PathBuf::from("selected.env")),
                    active_skills: vec!["selected-skill".to_string()],
                    skill_paths: vec![PathBuf::from("selected-skills")],
                    ..crate::config::ProfileConfig::default()
                },
            ],
        };
        std::fs::write(
            default_workspace.join("default.env"),
            "TOKEN=default-secret\n",
        )
        .expect("default env");
        std::fs::create_dir(default_workspace.join("default-skills")).expect("default skills");
        std::fs::write(root.path().join("selected.env"), "TOKEN=selected-secret\n")
            .expect("selected env");
        std::fs::create_dir(root.path().join("selected-skills")).expect("selected skills");
        crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");

        let deadline = Instant::now() + Duration::from_secs(5);
        let preflight = crate::session::SessionStore::preflight_project_sessions_dir_until(
            root.path(),
            deadline,
        )
        .expect("selected profile preflight");
        assert_eq!(
            preflight.runtime_config().profiles.default,
            "selected-profile"
        );

        let child = root.path().join("child-bootstrap");
        std::fs::create_dir(&child).expect("child root");
        std::fs::write(child.join("selected.env"), "TOKEN=selected-secret\n")
            .expect("child selected env");
        std::fs::create_dir(child.join("selected-skills")).expect("child selected skills");
        prepare_child_runtime_config(preflight.runtime_config(), &child)
            .expect("child runtime config");
        let child_config = crate::config::load_nib_config_full_preflight_read_only_until(
            &child,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("child config");
        assert_eq!(child_config.profiles.default, "selected-profile");
        assert_eq!(child_config.profiles.active.len(), 1);
        let selected = &child_config.profiles.active[0];
        assert_eq!(selected.id, "selected-profile");
        assert_eq!(
            selected.env_file.as_deref(),
            Some(Path::new("selected.env"))
        );
        assert_eq!(selected.active_skills, ["selected-skill"]);
        assert!(!serde_json::to_string(&child_config)
            .expect("encoded child config")
            .contains("default-secret"));
    }

    #[tokio::test]
    async fn worktree_and_owner_failures_leave_no_fallback_audit_preparation() {
        let non_git = tempfile::tempdir().expect("non-git project");
        let before = subagent_namespace_snapshot(non_git.path());
        let error = spawn_subagent(&json!({"prompt": "worktree failure"}), non_git.path())
            .expect_err("non-git worktree creation must fail");
        assert!(error.contains("git"));
        assert_subagent_namespace_unchanged(
            &before,
            &subagent_namespace_snapshot(non_git.path()),
            "non-Git failure namespace",
        );

        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        ensure_records_directory(root.path()).expect("prime authoritative records namespace");
        prime_fixed_subagent_record_lock_namespace(root.path());
        let primed = crate::sandbox::worktree::Worktree::create(root.path(), "prime-owner")
            .expect("prime worktree namespace");
        crate::sandbox::worktree::Worktree::remove(root.path(), &primed.id)
            .expect("remove priming worktree");
        let before = subagent_namespace_snapshot(root.path());
        SPAWN_OWNER_FAILURES.store(1, std::sync::atomic::Ordering::Release);
        let error = spawn_subagent(&json!({"prompt": "owner failure"}), root.path())
            .expect_err("injected owner creation must fail");
        assert!(error.contains("injected subagent owner creation failure"));
        assert_subagent_namespace_unchanged(
            &before,
            &subagent_namespace_snapshot(root.path()),
            "owner failure namespace",
        );
    }

    #[tokio::test]
    async fn sync_and_cancellable_record_failures_rollback_exact_fallback_audit_preparation() {
        let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(30));
        for cancellable in [false, true] {
            let root = tempfile::tempdir().expect("git project");
            initialize_spawn_test_repository(root.path());
            ensure_records_directory(root.path()).expect("prime records namespace");
            prime_fixed_subagent_record_lock_namespace(root.path());
            let owner = SubagentOwnerLease::create(root.path()).expect("prime owner namespace");
            owner.remove().expect("remove priming owner");
            let worktree = crate::sandbox::worktree::Worktree::create(root.path(), "prime-record")
                .expect("prime worktree namespace");
            crate::sandbox::worktree::Worktree::remove(root.path(), &worktree.id)
                .expect("remove priming worktree");
            crate::session::SessionStore::for_project(root.path())
                .expect("prime profile session namespace");
            let before = subagent_namespace_snapshot(root.path());

            SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
            let args = json!({"prompt": "record publication failure"});
            let error = if cancellable {
                spawn_subagent_cancellable(&args, root.path(), None)
                    .await
                    .expect_err("cancellable record publication must fail")
            } else {
                spawn_subagent(&args, root.path()).expect_err("sync record publication must fail")
            };
            assert!(
                error.contains("injected initial subagent record publication failure"),
                "unexpected record-publication failure (cancellable={cancellable}): {error}"
            );
            assert_subagent_namespace_unchanged(
                &before,
                &subagent_namespace_snapshot(root.path()),
                &format!(
                    "record failure left fallback audit/delegation artifacts (cancellable={cancellable})"
                ),
            );
        }
    }

    #[tokio::test]
    async fn preparation_supersession_requires_every_persisted_authority_field() {
        // This fixture stages every durable spawn resource before exercising
        // authority mismatch validation. Keep loaded Windows runners from
        // expiring the unrelated positive-progress setup deadline.
        let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(30));
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let args = json!({"prompt": "exact supersession fixture"});
        let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
        let namespace_plan = audit_plan
            .fallback_namespace_plan(&id, None)
            .expect("namespace plan")
            .expect("fallback plan");
        let worktree_plan =
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan");
        let owner_plan = SubagentOwnerLease::plan();
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let sessions_dir = audit_plan
            .fallback_sessions_dir()
            .expect("fallback sessions")
            .to_path_buf();
        let mut intent = SpawnPreparationIntent::create(
            &records,
            &id,
            owner_plan.clone(),
            worktree_plan.clone(),
            &id,
            &sessions_dir,
            Some(namespace_plan),
            None,
        )
        .expect("planned intent");
        let worktree = crate::sandbox::worktree::Worktree::create_from_preparation_authority(
            &project_root,
            &worktree_plan,
        )
        .expect("planned worktree");
        let owner = create_spawn_owner_lease(&project_root, &owner_plan, &intent.authority)
            .expect("planned owner");
        intent
            .revise(SpawnPreparationPhase::ResourcesPrepared, None, None, None)
            .expect("resource revision");
        let prepared =
            commit_subagent_audit_target(audit_plan, &id, Some(&worktree), Some(&mut intent))
                .expect("audit preparation");
        let target = prepared.encoded.clone();
        let base = SubagentRecord {
            id: id.clone(),
            parent_session_id: None,
            child_session_id: id.clone(),
            prompt: "fixture".to_string(),
            status: "running".to_string(),
            execution_generation: Some(owner_plan.execution_generation),
            owner_lease: Some(owner_plan.lease_id.clone()),
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: Some(worktree.branch_oid.clone()),
            result: Some(json!({
                "_ownership_audit_target": target,
                "_worktree_ownership_receipt": worktree.preparation_authority().ownership_receipt_id,
            })),
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        validate_spawn_intent_record_identity(&intent.data, &base).expect("exact record identity");

        let mut mismatches = Vec::new();
        let mut changed = base.clone();
        changed.execution_generation = Some(owner_plan.execution_generation.saturating_add(1));
        mismatches.push(changed);
        let mut changed = base.clone();
        changed.owner_lease = Some(uuid::Uuid::new_v4().to_string());
        mismatches.push(changed);
        let mut changed = base.clone();
        changed.worktree_path = project_root.join("replacement-worktree");
        mismatches.push(changed);
        let mut changed = base.clone();
        changed.branch.push_str("-replacement");
        mismatches.push(changed);
        let mut changed = base.clone();
        changed.branch_oid = Some("0".repeat(40));
        mismatches.push(changed);
        let mut changed = base.clone();
        changed.child_session_id.push_str("-replacement");
        mismatches.push(changed);
        let mut changed = base.clone();
        changed.status = "preparing".to_string();
        mismatches.push(changed);
        let mut changed = base.clone();
        changed.result.as_mut().expect("result")[WORKTREE_PREPARATION_RECEIPT_KEY] =
            Value::String(uuid::Uuid::new_v4().to_string());
        mismatches.push(changed);
        let mut changed = base.clone();
        changed.result.as_mut().expect("result")[OWNERSHIP_AUDIT_TARGET_KEY]["sessions_dir"] =
            Value::String("replacement".to_string());
        mismatches.push(changed);
        for changed in mismatches {
            let error = validate_spawn_intent_record_identity(&intent.data, &changed)
                .expect_err("mismatched authority must preserve preparation");
            assert!(error.contains("does not exactly match"), "{error}");
        }

        prepared.cleanup().expect("audit cleanup");
        cleanup_precommit_worktree(&project_root, &worktree)
            .await
            .expect("worktree cleanup");
        owner.remove().expect("owner cleanup");
        intent.cleanup().expect("intent cleanup");
    }

    #[test]
    fn handoff_execution_evidence_requires_exact_committed_scope_authority() {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let generation = 7_001;
        let owner_plan = SubagentOwnerPlan {
            execution_generation: generation,
            lease_id: uuid::Uuid::new_v4().to_string(),
        };
        let worktree =
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan");
        let store =
            crate::sandbox::process::ProcessScopeStore::open(&project_root).expect("scope store");
        let process_scope_plan = SubagentProcessScopePlan {
            cleanup_lease_id: uuid::Uuid::new_v4().to_string(),
            supervisor_registration_nonce: uuid::Uuid::new_v4().to_string(),
        };
        let prepared = store
            .prepare_subagent_launch(
                &id,
                generation,
                &process_scope_plan.cleanup_lease_id,
                &process_scope_plan.supervisor_registration_nonce,
                crate::sandbox::process::ProcessIdentity::current().expect("owner identity"),
                crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepared scope");
        let identity =
            crate::sandbox::process::ProcessIdentity::current().expect("execution identity");
        let mut ready = prepared.clone();
        ready.status = crate::sandbox::process::ProcessScopeStatus::Running;
        ready.launch_committed = Some(false);
        ready.supervisor = Some(identity.clone());
        ready.direct_child = Some(identity);
        ready.updated_at = Utc::now();
        let intent = SpawnPreparationIntentData {
            version: SPAWN_PREPARATION_VERSION,
            revision: 5,
            phase: SpawnPreparationPhase::HandoffProven,
            subagent_id: id.clone(),
            owner: owner_plan.clone(),
            worktree,
            audit_session_id: id.clone(),
            audit_sessions_dir: project_root.join(".nib/test-audit-sessions"),
            audit_namespace_plan: None,
            audit_target: None,
            audit_receipt: None,
            process_scope_plan: Some(process_scope_plan),
            handoff_process_scope: Some(ready.clone()),
            created_at: Utc::now(),
        };
        let mut record = record_fixture(&project_root, &id, "running");
        record.execution_generation = Some(generation);
        record.owner_lease = Some(owner_plan.lease_id.clone());
        let scope_path = project_root
            .join(".nib/process-scopes")
            .join(format!("{id}.json"));
        let install = |scope: &crate::sandbox::process::ProcessScopeRecord| {
            std::fs::write(
                &scope_path,
                serde_json::to_vec_pretty(scope).expect("encode scope fixture"),
            )
            .expect("install scope fixture");
        };
        let evidence_for = |candidate: &SpawnPreparationIntentData, record: &SubagentRecord| {
            spawn_handoff_has_execution_evidence(
                &project_root,
                &records,
                candidate,
                record,
                Instant::now() + Duration::from_secs(2),
            )
        };
        let evidence = |record: &SubagentRecord| evidence_for(&intent, record);

        let mut committed_control = ready.clone();
        committed_control.launch_committed = Some(true);
        committed_control.updated_at = Utc::now();
        install(&committed_control);
        assert!(evidence(&record).expect("matching v4 plan and READY scope"));
        let mut changed_ready_authorities = Vec::new();
        let mut changed = intent.clone();
        changed
            .handoff_process_scope
            .as_mut()
            .expect("READY scope")
            .cleanup_lease_id = uuid::Uuid::new_v4().to_string();
        changed_ready_authorities.push(changed);
        let mut changed = intent.clone();
        changed
            .handoff_process_scope
            .as_mut()
            .expect("READY scope")
            .supervisor_registration_nonce = Some(uuid::Uuid::new_v4().to_string());
        changed_ready_authorities.push(changed);
        let mut changed = intent.clone();
        changed
            .handoff_process_scope
            .as_mut()
            .expect("READY scope")
            .supervisor_registration_nonce = None;
        changed_ready_authorities.push(changed);
        let committed_namespace = subagent_namespace_snapshot(root.path());
        for changed in &changed_ready_authorities {
            assert!(
                evidence_for(changed, &record).is_err(),
                "changed or missing READY plan authority must fail closed"
            );
            assert_eq!(
                subagent_namespace_snapshot(root.path()),
                committed_namespace,
                "rejected READY authority changed intent or resource bytes"
            );
        }

        for status in [
            crate::sandbox::process::ProcessScopeStatus::Running,
            crate::sandbox::process::ProcessScopeStatus::CleanupInProgress,
            crate::sandbox::process::ProcessScopeStatus::RecoveryRequired,
        ] {
            let mut committed = ready.clone();
            committed.status = status;
            committed.launch_committed = Some(true);
            committed.cleanup_reason = (status
                != crate::sandbox::process::ProcessScopeStatus::Running)
                .then(|| "committed recovery fixture".to_string());
            committed.updated_at = Utc::now();
            install(&committed);
            assert!(evidence(&record).expect("exact committed scope evidence"));
        }

        for launch_committed in [Some(false), None] {
            let mut uncommitted = ready.clone();
            uncommitted.launch_committed = launch_committed;
            uncommitted.updated_at = Utc::now();
            install(&uncommitted);
            assert!(
                !matches!(evidence(&record), Ok(true)),
                "uncommitted or legacy scope must never supersede its intent"
            );
        }
        install(&prepared);
        assert!(
            !matches!(evidence(&record), Ok(true)),
            "Prepared must never supersede its intent"
        );
        let mut legacy_prepared = prepared.clone();
        legacy_prepared.launch_committed = None;
        install(&legacy_prepared);
        assert!(
            !matches!(evidence(&record), Ok(true)),
            "legacy Prepared must never supersede its intent"
        );
        let mut recovery_uncommitted = ready.clone();
        recovery_uncommitted.status = crate::sandbox::process::ProcessScopeStatus::RecoveryRequired;
        recovery_uncommitted.launch_committed = Some(false);
        recovery_uncommitted.cleanup_reason = Some("uncommitted recovery fixture".to_string());
        install(&recovery_uncommitted);
        assert!(
            !matches!(evidence(&record), Ok(true)),
            "uncommitted RecoveryRequired must never supersede its intent"
        );

        for mut mismatched in [
            {
                let mut scope = ready.clone();
                scope.workload_kind = "daemon".to_string();
                scope
            },
            {
                let mut scope = ready.clone();
                scope.execution_generation += 1;
                scope
            },
            {
                let mut scope = ready.clone();
                scope.cleanup_lease_id = uuid::Uuid::new_v4().to_string();
                scope
            },
            {
                let mut scope = ready.clone();
                scope.owner.start_marker.push_str("-other");
                scope
            },
        ] {
            mismatched.launch_committed = Some(true);
            install(&mismatched);
            assert!(
                evidence(&record).is_err(),
                "mismatched execution authority must fail closed"
            );
        }

        let mut wrong_id = ready.clone();
        wrong_id.scope_id = format!("{id}-wrong");
        wrong_id.launch_committed = Some(true);
        install(&wrong_id);
        assert!(
            evidence(&record).is_err(),
            "wrong scope id must fail closed"
        );

        let mut complete = ready.clone();
        complete.status = crate::sandbox::process::ProcessScopeStatus::Complete;
        complete.launch_committed = Some(true);
        complete.cleanup_reason = Some("completed evidence fixture".to_string());
        let proof = crate::sandbox::process::CleanupProof {
            execution_generation: generation,
            cleanup_lease_id: complete.cleanup_lease_id.clone(),
            backend: complete.backend,
            direct_child: complete.direct_child.clone().expect("direct child"),
            outcome: "completed evidence fixture".to_string(),
            descendants_reaped: true,
            completed_at: Utc::now(),
        };
        complete.cleanup_proof = Some(proof.clone());
        complete.updated_at = Utc::now();
        install(&complete);
        record.status = "failed".to_string();
        record.result = Some(json!({
            "cleanup_verified": true,
            "cleanup_proof": proof,
        }));
        assert!(evidence(&record).expect("exact terminal cleanup evidence"));
        record.result.as_mut().expect("terminal result")["cleanup_proof"]["outcome"] =
            Value::String("different proof".to_string());
        assert!(
            evidence(&record).is_err(),
            "terminal proof mismatch must fail closed"
        );
    }

    #[test]
    fn legacy_preparation_schema_and_evidence_reject_v2_ready_without_mutation() {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let audit_store =
            crate::session::SessionStore::for_project(&project_root).expect("audit store");
        let audit_target = subagent_audit_target_for_store(&audit_store).expect("audit target");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let generation = 7_101;
        let owner = SubagentOwnerPlan {
            execution_generation: generation,
            lease_id: uuid::Uuid::new_v4().to_string(),
        };
        let worktree =
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan");
        let process_store =
            crate::sandbox::process::ProcessScopeStore::open(&project_root).expect("scope store");
        let prepared = process_store
            .prepare_subagent_launch(
                &id,
                generation,
                &uuid::Uuid::new_v4().to_string(),
                &uuid::Uuid::new_v4().to_string(),
                crate::sandbox::process::ProcessIdentity::current().expect("owner identity"),
                crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepared scope");
        let execution =
            crate::sandbox::process::ProcessIdentity::current().expect("execution identity");
        let mut ready = prepared;
        ready.status = crate::sandbox::process::ProcessScopeStatus::Running;
        ready.launch_committed = Some(false);
        ready.supervisor = Some(execution.clone());
        ready.direct_child = Some(execution);
        ready.updated_at = Utc::now();
        let valid_v2 = SpawnPreparationIntentData {
            version: LEGACY_SPAWN_PREPARATION_VERSION,
            revision: 5,
            phase: SpawnPreparationPhase::HandoffProven,
            subagent_id: id.clone(),
            owner: owner.clone(),
            worktree,
            audit_session_id: id.clone(),
            audit_sessions_dir: audit_target.sessions_dir.clone(),
            audit_namespace_plan: None,
            audit_target: Some(audit_target),
            audit_receipt: None,
            process_scope_plan: None,
            handoff_process_scope: None,
            created_at: Utc::now(),
        };
        validate_spawn_preparation_intent_structure(&valid_v2).expect("valid v2 structure");
        encode_spawn_preparation_intent(&valid_v2).expect("valid v2 encoding");

        let mut record = record_fixture(&project_root, &id, "running");
        record.execution_generation = Some(generation);
        record.owner_lease = Some(owner.lease_id.clone());
        let mut committed = ready.clone();
        committed.launch_committed = Some(true);
        committed.updated_at = Utc::now();
        std::fs::write(
            project_root
                .join(".nib/process-scopes")
                .join(format!("{id}.json")),
            serde_json::to_vec_pretty(&committed).expect("encode committed scope"),
        )
        .expect("install committed scope");
        let before = subagent_namespace_snapshot(root.path());
        assert!(
            spawn_handoff_has_execution_evidence(
                &project_root,
                &records,
                &valid_v2,
                &record,
                Instant::now() + Duration::from_secs(2),
            )
            .is_err(),
            "v2 cannot infer exact handoff evidence from a matching committed scope"
        );
        assert_eq!(subagent_namespace_snapshot(root.path()), before);

        let mut v2_with_ready = valid_v2.clone();
        v2_with_ready.handoff_process_scope = Some(ready.clone());
        assert!(validate_spawn_preparation_intent_structure(&v2_with_ready).is_err());
        assert!(encode_spawn_preparation_intent(&v2_with_ready).is_err());
        assert!(
            spawn_handoff_has_execution_evidence(
                &project_root,
                &records,
                &v2_with_ready,
                &record,
                Instant::now() + Duration::from_secs(2),
            )
            .is_err(),
            "matching disk evidence cannot legitimize a forbidden v2 READY field"
        );
        assert_eq!(subagent_namespace_snapshot(root.path()), before);

        let mut valid_v3 = valid_v2.clone();
        valid_v3.version = HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION;
        valid_v3.handoff_process_scope = Some(ready);
        validate_spawn_preparation_intent_structure(&valid_v3).expect("valid v3 READY structure");
        encode_spawn_preparation_intent(&valid_v3).expect("valid v3 encoding");
        assert!(
            spawn_handoff_has_execution_evidence(
                &project_root,
                &records,
                &valid_v3,
                &record,
                Instant::now() + Duration::from_secs(2),
            )
            .expect("exact v3 evidence"),
            "valid v3 READY must retain its exact legacy evidence semantics"
        );
        assert_eq!(subagent_namespace_snapshot(root.path()), before);
        let mut mismatched_v3 = valid_v3.clone();
        mismatched_v3
            .handoff_process_scope
            .as_mut()
            .expect("v3 READY")
            .cleanup_lease_id = uuid::Uuid::new_v4().to_string();
        assert!(
            spawn_handoff_has_execution_evidence(
                &project_root,
                &records,
                &mismatched_v3,
                &record,
                Instant::now() + Duration::from_secs(2),
            )
            .is_err(),
            "v3 evidence must fail closed when its exact READY authority differs"
        );
        assert_eq!(subagent_namespace_snapshot(root.path()), before);

        let mut v3_without_ready = valid_v3.clone();
        v3_without_ready.handoff_process_scope = None;
        assert!(validate_spawn_preparation_intent_structure(&v3_without_ready).is_err());
        let mut v3_ready_too_early = valid_v3;
        v3_ready_too_early.phase = SpawnPreparationPhase::ManagerRegistered;
        v3_ready_too_early.revision = 4;
        assert!(validate_spawn_preparation_intent_structure(&v3_ready_too_early).is_err());
        assert_eq!(subagent_namespace_snapshot(root.path()), before);
    }

    #[test]
    fn atomic_preparation_revision_rejects_process_plan_substitution_byte_exactly() {
        for variant in ["cleanup-lease", "registration-nonce", "matching-control"] {
            let root = tempfile::tempdir().expect("git project");
            initialize_spawn_test_repository(root.path());
            let project_root = canonical_project_root(root.path()).expect("canonical root");
            let id = format!("sub-{}", uuid::Uuid::new_v4());
            let args = json!({"prompt": "atomic process plan fixture"});
            let audit_plan =
                preflight_subagent_audit_target(&args, &project_root).expect("audit preflight");
            let records = ensure_records_directory_capability_until(&project_root, None)
                .expect("authorized records");
            let namespace_plan = audit_plan
                .fallback_namespace_plan_after_records(&id, &records)
                .expect("namespace plan")
                .expect("fallback namespace plan");
            let intent = SpawnPreparationIntent::create(
                &records,
                &id,
                SubagentOwnerLease::plan(),
                crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                    .expect("worktree plan"),
                &id,
                audit_plan.fallback_sessions_dir().expect("sessions"),
                Some(namespace_plan),
                None,
            )
            .expect("planned intent");
            let previous = intent
                .directory
                .deterministic_previous_artifact_path(&intent.path, ".nib-subagent-preparation-")
                .expect("previous revision path");
            let mut successor = intent.data.clone();
            successor.revision = 1;
            successor.phase = SpawnPreparationPhase::ResourcesPrepared;
            match variant {
                "cleanup-lease" => {
                    successor
                        .process_scope_plan
                        .as_mut()
                        .expect("v4 plan")
                        .cleanup_lease_id = uuid::Uuid::new_v4().to_string();
                }
                "registration-nonce" => {
                    successor
                        .process_scope_plan
                        .as_mut()
                        .expect("v4 plan")
                        .supervisor_registration_nonce = uuid::Uuid::new_v4().to_string();
                }
                "matching-control" => {}
                _ => unreachable!(),
            }
            let successor_bytes =
                encode_spawn_preparation_intent(&successor).expect("successor bytes");
            std::fs::rename(&intent.path, &previous).expect("evacuate prior revision");
            std::fs::write(&intent.path, &successor_bytes).expect("publish successor fixture");
            let before = subagent_namespace_snapshot(root.path());
            let result = recover_spawn_preparation_transactions(
                &intent.directory,
                &records,
                Instant::now() + Duration::from_secs(2),
            );
            if variant == "matching-control" {
                result.expect("matching immutable plan finalizes prior revision");
                assert!(!previous.exists(), "matching prior revision was finalized");
                assert_eq!(
                    std::fs::read(&intent.path).expect("matching successor"),
                    successor_bytes
                );
            } else {
                let error = result.expect_err("substituted v4 plan must fail closed");
                assert!(error.contains("ambiguous"), "{error}");
                assert_eq!(
                    subagent_namespace_snapshot(root.path()),
                    before,
                    "atomic {variant} substitution changed intent or resource bytes"
                );
            }
            drop(intent);
        }
    }

    #[test]
    fn atomic_legacy_preparation_recovery_rejects_v2_ready_in_every_artifact() {
        for variant in [
            "temporary-v2-ready",
            "previous-v2-ready",
            "target-v2-ready",
            "valid-v2-control",
            "valid-v3-control",
        ] {
            let root = tempfile::tempdir().expect("git project");
            initialize_spawn_test_repository(root.path());
            let project_root = canonical_project_root(root.path()).expect("canonical root");
            let records = ensure_records_directory_capability_until(&project_root, None)
                .expect("authorized records");
            let audit_store =
                crate::session::SessionStore::for_project(&project_root).expect("audit store");
            let audit_target = subagent_audit_target_for_store(&audit_store).expect("audit target");
            let audit_sessions_dir = audit_target.sessions_dir.clone();
            let id = format!("sub-{}", uuid::Uuid::new_v4());
            let intent = SpawnPreparationIntent::create(
                &records,
                &id,
                SubagentOwnerLease::plan(),
                crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                    .expect("worktree plan"),
                &id,
                &audit_sessions_dir,
                None,
                Some(audit_target),
            )
            .expect("planned intent");
            let mut legacy = intent.data.clone();
            legacy.version = if variant == "valid-v3-control" {
                HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION
            } else {
                LEGACY_SPAWN_PREPARATION_VERSION
            };
            legacy.process_scope_plan = None;
            legacy.handoff_process_scope = None;
            let valid_planned = encode_spawn_preparation_intent(&legacy).expect("legacy planned");
            std::fs::write(&intent.path, &valid_planned).expect("install legacy planned intent");

            #[cfg(target_os = "linux")]
            let backend = crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace;
            #[cfg(windows)]
            let backend = crate::sandbox::process::ProcessScopeBackend::WindowsJobObject;
            #[cfg(target_os = "macos")]
            let backend = crate::sandbox::process::ProcessScopeBackend::MacosProcessGroup;
            let identity =
                crate::sandbox::process::ProcessIdentity::current().expect("process identity");
            let now = Utc::now();
            let ready = crate::sandbox::process::ProcessScopeRecord {
                version: 2,
                scope_id: id.clone(),
                workload_kind: "subagent".to_string(),
                execution_generation: legacy.owner.execution_generation,
                cleanup_lease_id: uuid::Uuid::new_v4().to_string(),
                supervisor_registration_nonce: None,
                owner: identity.clone(),
                backend,
                status: crate::sandbox::process::ProcessScopeStatus::Running,
                launch_committed: Some(false),
                supervisor: Some(identity.clone()),
                direct_child: Some(identity),
                cleanup_reason: None,
                cleanup_proof: None,
                launch_abort_proof: None,
                created_at: now,
                updated_at: now,
            };
            let previous = intent
                .directory
                .deterministic_previous_artifact_path(&intent.path, ".nib-subagent-preparation-")
                .expect("previous artifact");
            let temporary = intent
                .directory
                .deterministic_artifact_path(&intent.path, ".nib-subagent-preparation-", ".tmp")
                .expect("temporary artifact");
            let mut manager = legacy.clone();
            manager.phase = SpawnPreparationPhase::ManagerRegistered;
            manager.revision = 4;
            let mut proven = manager.clone();
            proven.phase = SpawnPreparationPhase::HandoffProven;
            proven.revision = 5;

            match variant {
                "temporary-v2-ready" => {
                    let mut invalid = legacy.clone();
                    invalid.handoff_process_scope = Some(ready.clone());
                    std::fs::write(
                        &temporary,
                        serde_json::to_vec_pretty(&invalid).expect("invalid temp bytes"),
                    )
                    .expect("install invalid temporary");
                }
                "previous-v2-ready" => {
                    let mut invalid = manager.clone();
                    invalid.handoff_process_scope = Some(ready.clone());
                    std::fs::rename(&intent.path, &previous).expect("evacuate prior intent");
                    std::fs::write(
                        &previous,
                        serde_json::to_vec_pretty(&invalid).expect("invalid previous bytes"),
                    )
                    .expect("install invalid previous");
                }
                "target-v2-ready" => {
                    std::fs::write(
                        &intent.path,
                        encode_spawn_preparation_intent(&manager).expect("valid v2 prior"),
                    )
                    .expect("install valid v2 prior");
                    std::fs::rename(&intent.path, &previous).expect("evacuate valid prior");
                    proven.handoff_process_scope = Some(ready.clone());
                    std::fs::write(
                        &intent.path,
                        serde_json::to_vec_pretty(&proven).expect("invalid target bytes"),
                    )
                    .expect("install invalid target");
                }
                "valid-v2-control" => {
                    std::fs::write(
                        &intent.path,
                        encode_spawn_preparation_intent(&manager).expect("valid v2 prior"),
                    )
                    .expect("install valid v2 prior");
                    std::fs::rename(&intent.path, &previous).expect("evacuate valid v2 prior");
                    std::fs::write(
                        &intent.path,
                        encode_spawn_preparation_intent(&proven).expect("valid v2 target"),
                    )
                    .expect("install valid v2 target");
                }
                "valid-v3-control" => {
                    std::fs::write(
                        &intent.path,
                        encode_spawn_preparation_intent(&manager).expect("valid v3 prior"),
                    )
                    .expect("install valid v3 prior");
                    std::fs::rename(&intent.path, &previous).expect("evacuate valid v3 prior");
                    proven.handoff_process_scope = Some(ready);
                    std::fs::write(
                        &intent.path,
                        encode_spawn_preparation_intent(&proven).expect("valid v3 target"),
                    )
                    .expect("install valid v3 target");
                }
                _ => unreachable!(),
            }

            let before = subagent_namespace_snapshot(root.path());
            let result = recover_spawn_preparation_transactions(
                &intent.directory,
                &records,
                Instant::now() + Duration::from_secs(2),
            );
            if variant.starts_with("valid-") {
                result.expect("valid legacy atomic successor");
                assert!(!previous.exists(), "valid prior artifact was finalized");
                assert!(intent.path.is_file(), "valid target remains canonical");
            } else {
                let error = result.expect_err("v2 READY injection must fail closed");
                assert!(error.contains("preserved"), "{variant}: {error}");
                assert_eq!(
                    subagent_namespace_snapshot(root.path()),
                    before,
                    "{variant} changed the project namespace"
                );
            }
            drop(intent);
        }
    }

    #[test]
    fn restart_adopts_exact_intent_delete_quarantine_and_rejects_ambiguity() {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let args = json!({"prompt": "quarantine retry fixture"});
        let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
        let worktree_plan =
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan");
        let owner_plan = SubagentOwnerLease::plan();
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let namespace_plan = audit_plan
            .fallback_namespace_plan_after_records(&id, &records)
            .expect("namespace plan")
            .expect("fallback plan");
        let sessions_dir = audit_plan
            .fallback_sessions_dir()
            .expect("fallback sessions")
            .to_path_buf();
        let intent = SpawnPreparationIntent::create(
            &records,
            &id,
            owner_plan,
            worktree_plan,
            &id,
            &sessions_dir,
            Some(namespace_plan),
            None,
        )
        .expect("planned intent");
        let quarantine = intent
            .directory
            .deterministic_artifact_path(
                &intent.path,
                ".nib-subagent-preparation-delete-",
                ".quarantine",
            )
            .expect("quarantine path");
        std::fs::rename(&intent.path, &quarantine).expect("simulate interrupted quarantine delete");
        drop(intent);
        assert!(list_subagents(&project_root)
            .expect("fresh retry adopts exact quarantine")
            .is_empty());
        assert!(!quarantine.exists(), "exact quarantine removed on retry");

        let root = tempfile::tempdir().expect("ambiguous git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
        let namespace_plan = audit_plan
            .fallback_namespace_plan_after_records(&id, &records)
            .expect("namespace plan")
            .expect("fallback plan");
        let worktree_plan =
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan");
        let intent = SpawnPreparationIntent::create(
            &records,
            &id,
            SubagentOwnerLease::plan(),
            worktree_plan,
            &id,
            audit_plan.fallback_sessions_dir().expect("sessions"),
            Some(namespace_plan),
            None,
        )
        .expect("second planned intent");
        let quarantine = intent
            .directory
            .deterministic_artifact_path(
                &intent.path,
                ".nib-subagent-preparation-delete-",
                ".quarantine",
            )
            .expect("quarantine path");
        std::fs::copy(&intent.path, &quarantine).expect("install ambiguous quarantine");
        drop(intent);
        let before = subagent_namespace_snapshot(root.path());
        let error = list_subagents(&project_root).expect_err("ambiguity must fail closed");
        assert!(
            error.contains("ambiguous canonical and quarantine"),
            "{error}"
        );
        assert_subagent_namespace_unchanged(
            &before,
            &subagent_namespace_snapshot(root.path()),
            "ambiguous intent quarantine",
        );
    }

    #[test]
    fn live_planned_intent_blocks_list_and_get_reconciliation_until_writer_retires() {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let args = json!({"prompt": "live planned intent fixture"});
        let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let namespace_plan = audit_plan
            .fallback_namespace_plan_after_records(&id, &records)
            .expect("namespace plan")
            .expect("fallback plan");
        let intent = SpawnPreparationIntent::create(
            &records,
            &id,
            SubagentOwnerLease::plan(),
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan"),
            &id,
            audit_plan.fallback_sessions_dir().expect("sessions"),
            Some(namespace_plan),
            None,
        )
        .expect("planned intent");

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let list_root = project_root.clone();
        let list_started = started_tx.clone();
        let list_results = result_tx.clone();
        let list_thread = std::thread::spawn(move || {
            list_started.send("list").expect("announce list start");
            list_results
                .send((
                    "list",
                    list_subagents(&list_root).map(|records| records.len()),
                ))
                .expect("send list result");
        });
        let get_root = project_root.clone();
        let get_id = id.clone();
        let get_thread = std::thread::spawn(move || {
            started_tx.send("get").expect("announce get start");
            result_tx
                .send((
                    "get",
                    get_subagent_record(&get_root, &get_id).map(|_| 1_usize),
                ))
                .expect("send get result");
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first reader started");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second reader started");
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            matches!(
                result_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "a reader entered reconciliation while the planned writer held its lifetime authority"
        );
        assert!(intent.path.exists(), "live planned intent was preserved");

        intent.cleanup().expect("writer retires intent");
        let first = result_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("first reader completes after retirement");
        let second = result_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("second reader completes after retirement");
        for (kind, result) in [first, second] {
            match kind {
                "list" => assert_eq!(result.expect("list after retirement"), 0),
                "get" => assert!(result.is_err(), "missing record must remain missing"),
                _ => panic!("unexpected reader kind"),
            }
        }
        list_thread.join().expect("join list reader");
        get_thread.join().expect("join get reader");
    }

    #[test]
    fn live_revision_and_intent_quarantine_block_reconcile_and_preserve_record() {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let args = json!({"prompt": "serialized revision fixture"});
        let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let namespace_plan = audit_plan
            .fallback_namespace_plan_after_records(&id, &records)
            .expect("namespace plan")
            .expect("fallback plan");
        let intent = SpawnPreparationIntent::create(
            &records,
            &id,
            SubagentOwnerLease::plan(),
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan"),
            &id,
            audit_plan.fallback_sessions_dir().expect("sessions"),
            Some(namespace_plan),
            None,
        )
        .expect("planned intent");
        let mut record = record_fixture(&project_root, &id, "completed");
        record.parent_session_id = None;
        let publication =
            write_spawn_subagent_record_locked(&project_root, &record, &intent.authority)
                .expect("authoritative record publication");
        let record_path = records.path().join(format!("{id}.json"));
        let record_before = std::fs::read(&record_path).expect("record bytes before race");

        let previous = intent
            .directory
            .deterministic_previous_artifact_path(&intent.path, ".nib-subagent-preparation-")
            .expect("revision previous path");
        std::fs::rename(&intent.path, &previous).expect("pause revision after evacuation");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let list_root = project_root.clone();
        let reader = std::thread::spawn(move || {
            started_tx.send(()).expect("announce reader");
            result_tx
                .send(list_subagents(&list_root))
                .expect("send list result");
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reader started");
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            matches!(
                result_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "reconciler entered while a revision held the lifetime authority"
        );
        assert!(
            previous.exists(),
            "live evacuated revision was not rolled back"
        );
        let encoded = encode_spawn_preparation_intent(&intent.data).expect("intent bytes");
        intent
            .directory
            .restore_exact_previous_artifact_with_guard(&intent.path, &previous, &encoded, || {
                intent.authority.verify()
            })
            .expect("writer restores evacuated revision");

        let quarantine = intent
            .directory
            .deterministic_artifact_path(
                &intent.path,
                ".nib-subagent-preparation-delete-",
                ".quarantine",
            )
            .expect("intent quarantine");
        std::fs::rename(&intent.path, &quarantine).expect("pause cleanup after quarantine");
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            matches!(
                result_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "reconciler entered while exact cleanup held the lifetime authority"
        );
        intent
            .directory
            .remove_visible_file_if_matches_direct_with_guard(&quarantine, &intent.file, || {
                intent.authority.verify()
            })
            .expect("writer completes exact intent deletion");
        drop(publication);
        drop(intent);

        let listed = result_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("reader completes after cleanup")
            .expect("list after cleanup");
        assert_eq!(listed.len(), 1, "authoritative record remains visible");
        assert_eq!(listed[0]["id"], id);
        assert_eq!(
            std::fs::read(&record_path).expect("record bytes after race"),
            record_before,
            "reconciliation changed the authoritative record"
        );
        assert!(!previous.exists(), "revision artifact was finalized");
        assert!(!quarantine.exists(), "cleanup artifact was finalized");
        reader.join().expect("join reader");
    }

    #[cfg(unix)]
    #[test]
    fn preparation_reconciliation_rejects_records_replacement_after_intent_open() {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let args = json!({"prompt": "records replacement fixture"});
        let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
        let worktree_plan =
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan");
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let namespace_plan = audit_plan
            .fallback_namespace_plan_after_records(&id, &records)
            .expect("namespace plan")
            .expect("fallback plan");
        let intent = SpawnPreparationIntent::create(
            &records,
            &id,
            SubagentOwnerLease::plan(),
            worktree_plan,
            &id,
            audit_plan.fallback_sessions_dir().expect("sessions"),
            Some(namespace_plan),
            None,
        )
        .expect("planned intent");
        drop(intent);
        let original_before = directory_tree_snapshot(records.path());
        let records_path = records.path().to_path_buf();
        let displaced = project_root.join(".nib/subagents.displaced-preparation");
        let displaced_for_hook = displaced.clone();
        let replacement_before = std::sync::Arc::new(std::sync::Mutex::new(None));
        let replacement_capture = replacement_before.clone();
        AFTER_PREPARATION_INTENT_OPEN_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&records_path, &displaced_for_hook).expect("displace records");
                std::fs::create_dir(&records_path).expect("create replacement records");
                std::fs::write(records_path.join("replacement.sentinel"), b"replacement")
                    .expect("replacement sentinel");
                *replacement_capture
                    .lock()
                    .expect("replacement snapshot lock") =
                    Some(directory_tree_snapshot(&records_path));
            }));
        });
        let error = list_subagents(&project_root)
            .expect_err("detached preparation capability must fail closed");
        assert!(
            error.contains("identity changed") || error.contains("no longer attached"),
            "{error}"
        );
        assert_eq!(directory_tree_snapshot(&displaced), original_before);
        assert_eq!(
            directory_tree_snapshot(&project_root.join(".nib/subagents")),
            replacement_before
                .lock()
                .expect("replacement snapshot lock")
                .clone()
                .expect("replacement snapshot")
        );
        assert!(displaced
            .join(".preparations")
            .join(format!("{id}.json"))
            .exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_forward_mutations_stop_at_detached_preparation_authority() {
        for cancellable in [false, true] {
            for boundary in ["worktree", "child_config", "owner", "audit"] {
                let root = tempfile::tempdir().expect("git project");
                initialize_spawn_test_repository(root.path());
                ensure_records_directory(root.path()).expect("authorized records namespace");
                let records_path = records_dir(root.path());
                let displaced = root
                    .path()
                    .join(format!(".records-displaced-{boundary}-{cancellable}"));
                let expected_after_detach = std::sync::Arc::new(std::sync::Mutex::new(None));
                let expected_capture = expected_after_detach.clone();
                let root_path = root.path().to_path_buf();
                let requested = boundary;
                let mut replaced = false;
                SPAWN_FORWARD_MUTATION_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |observed| {
                        if replaced || observed != requested {
                            return;
                        }
                        replaced = true;
                        std::fs::rename(&records_path, &displaced)
                            .expect("detach authoritative records directory");
                        std::fs::create_dir(&records_path)
                            .expect("create replacement records directory");
                        std::fs::write(records_path.join("replacement.sentinel"), b"replacement")
                            .expect("write replacement records sentinel");
                        *expected_capture.lock().expect("snapshot lock") =
                            Some(directory_tree_snapshot(&root_path));
                    }));
                });
                let args = json!({"prompt": format!("detach before {boundary}")});
                let result = if cancellable {
                    spawn_subagent_cancellable(&args, root.path(), None).await
                } else {
                    spawn_subagent(&args, root.path())
                };
                SPAWN_FORWARD_MUTATION_HOOK.with(|hook| hook.borrow_mut().take());
                let error = result.expect_err("detached preparation authority must fail closed");
                assert!(
                    error.contains("identity changed")
                        || error.contains("no longer attached")
                        || error.contains("state directory changed"),
                    "{boundary} cancellable={cancellable}: {error}"
                );
                let expected = expected_after_detach
                    .lock()
                    .expect("snapshot lock")
                    .clone()
                    .expect("replacement boundary ran");
                assert_eq!(
                    directory_tree_snapshot(root.path()),
                    expected,
                    "{boundary} cancellable={cancellable} mutated a resource after records detachment"
                );
            }
        }
    }

    #[tokio::test]
    async fn partial_cleanup_failures_retain_intent_until_restart_reconciles_last() {
        #[cfg(windows)]
        let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(10));
        for cancellable in [false, true] {
            for failure in ["session", "worktree", "owner", "audit"] {
                let root = tempfile::tempdir().expect("git project");
                initialize_spawn_test_repository(root.path());
                ensure_records_directory(root.path()).expect("authorized records namespace");
                let primed_owner = SubagentOwnerLease::create(root.path()).expect("prime owner");
                primed_owner.remove().expect("remove primed owner");
                let primed_worktree =
                    crate::sandbox::worktree::Worktree::create(root.path(), "cleanup-prime")
                        .expect("prime worktree");
                crate::sandbox::worktree::Worktree::remove(root.path(), &primed_worktree.id)
                    .expect("remove primed worktree");
                crate::session::SessionStore::for_project(root.path())
                    .expect("prime fallback session namespace");
                let before = subagent_namespace_snapshot(root.path());

                match failure {
                    "session" => {
                        SPAWN_SESSION_PUBLICATION_FAILURES
                            .store(1, std::sync::atomic::Ordering::Release);
                        SPAWN_SESSION_CLEANUP_FAILURES
                            .store(1, std::sync::atomic::Ordering::Release);
                    }
                    "worktree" => {
                        SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                        SPAWN_WORKTREE_CLEANUP_FAILURES
                            .store(1, std::sync::atomic::Ordering::Release);
                    }
                    "owner" => {
                        SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                        SPAWN_OWNER_CLEANUP_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                    }
                    "audit" => {
                        SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                        SPAWN_AUDIT_CLEANUP_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                    }
                    _ => unreachable!(),
                }
                let args = json!({"prompt": format!("{failure} cleanup failure")});
                let error = if cancellable {
                    spawn_subagent_cancellable(&args, root.path(), None)
                        .await
                        .expect_err("cancellable partial cleanup must fail")
                } else {
                    spawn_subagent(&args, root.path()).expect_err("sync partial cleanup must fail")
                };
                SPAWN_RECORD_FAILURES.store(0, std::sync::atomic::Ordering::Release);
                SPAWN_WORKTREE_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
                SPAWN_OWNER_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
                SPAWN_AUDIT_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
                SPAWN_SESSION_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
                SPAWN_SESSION_PUBLICATION_FAILURES.store(0, std::sync::atomic::Ordering::Release);
                assert!(error.contains("failure"), "{failure}: {error}");
                let preparations = spawn_preparation_directory_path(root.path());
                assert!(
                    preparations.exists()
                        && std::fs::read_dir(&preparations)
                            .expect("read retained preparations")
                            .next()
                            .is_some(),
                    "{failure} cancellable={cancellable} retired intent before exact cleanup"
                );
                if failure == "audit" {
                    let intent_path = std::fs::read_dir(&preparations)
                        .expect("read retained audit intent")
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .find(|path| {
                            path.extension().and_then(|value| value.to_str()) == Some("json")
                        })
                        .expect("retained canonical audit intent");
                    let intent: SpawnPreparationIntentData = serde_json::from_slice(
                        &std::fs::read(&intent_path).expect("read retained audit intent"),
                    )
                    .expect("decode retained audit intent");
                    assert_eq!(intent.phase, SpawnPreparationPhase::AuditPublished);
                    assert!(
                        intent
                            .audit_sessions_dir
                            .join(format!("{}.json", intent.audit_session_id))
                            .is_file(),
                        "audit cleanup failure was retried by Drop instead of restart"
                    );
                }

                assert!(list_subagents(root.path())
                    .expect("restart reconciliation")
                    .is_empty());
                assert!(
                    !preparations.exists()
                        || std::fs::read_dir(&preparations)
                            .expect("read reconciled preparations")
                            .next()
                            .is_none(),
                    "{failure} cancellable={cancellable} left a durable preparation intent"
                );
                let after = subagent_namespace_snapshot(root.path());
                assert_spawn_cleanup_snapshot(
                    &before,
                    &after,
                    &format!(
                        "{failure} cancellable={cancellable} restart changed namespace entries"
                    ),
                );
            }
        }
    }

    #[tokio::test]
    async fn post_audit_cancellation_cleanup_failure_retains_intent_until_restart() {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        ensure_records_directory(root.path()).expect("authorized records namespace");
        let primed_owner = SubagentOwnerLease::create(root.path()).expect("prime owner");
        primed_owner.remove().expect("remove primed owner");
        let primed_worktree =
            crate::sandbox::worktree::Worktree::create(root.path(), "post-audit-prime")
                .expect("prime worktree");
        crate::sandbox::worktree::Worktree::remove(root.path(), &primed_worktree.id)
            .expect("remove primed worktree");
        crate::session::SessionStore::for_project(root.path())
            .expect("prime fallback session namespace");
        let before = subagent_namespace_snapshot(root.path());
        let cancellation = crate::agent::CancellationSignal::new();
        SPAWN_POST_AUDIT_CANCELLATIONS.store(1, std::sync::atomic::Ordering::Release);
        SPAWN_AUDIT_CLEANUP_FAILURES.store(1, std::sync::atomic::Ordering::Release);

        let error = spawn_subagent_cancellable(
            &json!({"prompt": "cancel after audit publication"}),
            root.path(),
            Some(&cancellation),
        )
        .await
        .expect_err("post-audit cancellation cleanup failure must fail closed");
        SPAWN_POST_AUDIT_CANCELLATIONS.store(0, std::sync::atomic::Ordering::Release);
        SPAWN_AUDIT_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
        assert!(error.contains("cancelled before commit"), "{error}");
        assert!(error.contains("audit cleanup failed"), "{error}");
        let preparations = spawn_preparation_directory_path(root.path());
        assert!(
            preparations.exists()
                && std::fs::read_dir(&preparations)
                    .expect("read retained post-audit preparation")
                    .next()
                    .is_some(),
            "post-audit cancellation retired intent after partial cleanup"
        );
        let intent_path = std::fs::read_dir(&preparations)
            .expect("read retained post-audit preparation")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .expect("retained post-audit intent");
        let intent: SpawnPreparationIntentData = serde_json::from_slice(
            &std::fs::read(&intent_path).expect("read retained post-audit intent"),
        )
        .expect("decode retained post-audit intent");
        assert_eq!(intent.phase, SpawnPreparationPhase::AuditPublished);
        assert!(
            intent
                .audit_sessions_dir
                .join(format!("{}.json", intent.audit_session_id))
                .is_file(),
            "post-audit cleanup failure was retried by Drop instead of restart"
        );

        assert!(list_subagents(root.path())
            .expect("restart post-audit reconciliation")
            .is_empty());
        assert!(
            std::fs::read_dir(&preparations)
                .expect("read reconciled post-audit preparations")
                .next()
                .is_none(),
            "restart did not retire post-audit intent last"
        );
        assert_spawn_cleanup_snapshot(
            &before,
            &subagent_namespace_snapshot(root.path()),
            "post-audit restart left an external artifact",
        );
    }

    #[tokio::test]
    async fn final_intent_retirement_expiry_is_fail_closed_for_sync_and_async_handoffs() {
        for cancellable in [false, true] {
            let root = tempfile::tempdir().expect("handoff expiry project");
            initialize_spawn_test_repository(root.path());
            let operation_timeout = if cfg!(windows) {
                Duration::from_secs(15)
            } else {
                Duration::from_secs(5)
            };
            let expiry_delay = operation_timeout + Duration::from_millis(100);
            let cancellation_timeout = if cfg!(windows) {
                Duration::from_secs(15)
            } else {
                Duration::from_secs(2)
            };
            let _timeout = SpawnPreparationTimeoutGuard::set(operation_timeout);
            let _cancellation_timeout = SubagentCancellationTimeoutGuard::set(cancellation_timeout);
            let _hook = SpawnHandoffPhaseHookGuard::install(move |phase| {
                if phase == "before_intent_retirement" {
                    std::thread::sleep(expiry_delay);
                }
            });
            let args = json!({"prompt": "expire only at final intent retirement"});
            let error = if cancellable {
                spawn_subagent_cancellable(&args, root.path(), None)
                    .await
                    .expect_err("async final retirement expiry must fail closed")
            } else {
                spawn_subagent(&args, root.path())
                    .expect_err("sync final retirement expiry must fail closed")
            };
            assert!(
                error.contains("retire authoritative spawn preparation after handoff"),
                "cancellable={cancellable}: {error}"
            );
            drop(_hook);
            drop(_timeout);
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(25)).await;

            let listed = list_subagents(root.path()).unwrap_or_else(|reconcile| {
                panic!("cancellable={cancellable}: restart reconciliation: {reconcile}")
            });
            assert!(
                listed.iter().all(|record| !matches!(
                    record.get("status").and_then(Value::as_str).unwrap_or(""),
                    "running" | "recovery_required"
                )),
                "cancellable={cancellable}: final expiry exposed unlaunchable state: {listed:?}"
            );
            let preparations = spawn_preparation_directory_path(root.path());
            assert!(
                !preparations.exists()
                    || std::fs::read_dir(&preparations)
                        .expect("read final-expiry preparations")
                        .next()
                        .is_none(),
                "cancellable={cancellable}: restart did not retire intent last"
            );
        }
    }

    #[tokio::test]
    async fn manager_rollback_failure_preserves_intent_until_restart_compensates() {
        for cancellable in [false, true] {
            let root = tempfile::tempdir().expect("manager rollback project");
            initialize_spawn_test_repository(root.path());
            let operation_timeout = if cfg!(windows) {
                Duration::from_secs(15)
            } else {
                Duration::from_secs(5)
            };
            let expiry_delay = operation_timeout + Duration::from_millis(100);
            let cancellation_timeout = if cfg!(windows) {
                Duration::from_secs(15)
            } else {
                Duration::from_secs(2)
            };
            let _timeout = SpawnPreparationTimeoutGuard::set(operation_timeout);
            let _cancellation_timeout = SubagentCancellationTimeoutGuard::set(cancellation_timeout);
            let _hook = SpawnHandoffPhaseHookGuard::install(move |phase| {
                if phase == "manager_registered" {
                    std::thread::sleep(expiry_delay);
                }
            });
            crate::daemons::task::inject_rollback_unattached_failures(1);
            let args = json!({"prompt": "expire after manager registration"});
            let error = if cancellable {
                spawn_subagent_cancellable(&args, root.path(), None)
                    .await
                    .expect_err("async manager rollback failure must fail closed")
            } else {
                spawn_subagent(&args, root.path())
                    .expect_err("sync manager rollback failure must fail closed")
            };
            assert!(
                error.contains("task registration rollback failed"),
                "{error}"
            );
            drop(_hook);
            drop(_timeout);
            crate::daemons::task::inject_rollback_unattached_failures(0);
            let preparation_dir = spawn_preparation_directory_path(root.path());
            let intent_path = std::fs::read_dir(&preparation_dir)
                .expect("retained manager-failure intent directory")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
                .expect("retained manager-failure intent");
            let intent: SpawnPreparationIntentData = serde_json::from_slice(
                &std::fs::read(&intent_path).expect("read manager-failure intent"),
            )
            .expect("decode manager-failure intent");
            assert_eq!(
                crate::daemons::task::TASK_MANAGER.get_status(&intent.subagent_id),
                Some("running".to_string()),
                "failed rollback did not retain the unattached manager entry"
            );

            let listed = list_subagents(root.path()).unwrap_or_else(|reconcile| {
                panic!("cancellable={cancellable}: rollback restart: {reconcile}")
            });
            assert!(
                listed.is_empty(),
                "rollback restart retained workload: {listed:?}"
            );
            assert_eq!(
                crate::daemons::task::TASK_MANAGER.get_status(&intent.subagent_id),
                None
            );
        }
    }

    #[tokio::test]
    async fn expired_spawn_preparation_retains_intent_until_fresh_restart_cleanup() {
        const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
        const EXPIRY_DELAY: Duration = Duration::from_millis(5_100);

        for cancellable in [false, true] {
            for phase in ["worktree_reservation", "session_temp", "session_canonical"] {
                let root = tempfile::tempdir().expect("git project");
                initialize_spawn_test_repository(root.path());
                ensure_records_directory(root.path()).expect("authorized records namespace");
                let primed_owner = SubagentOwnerLease::create(root.path()).expect("prime owner");
                primed_owner.remove().expect("remove primed owner");
                let primed_worktree =
                    crate::sandbox::worktree::Worktree::create(root.path(), "deadline-prime")
                        .expect("prime worktree");
                crate::sandbox::worktree::Worktree::remove(root.path(), &primed_worktree.id)
                    .expect("remove primed worktree");
                let sessions = crate::session::SessionStore::for_project(root.path())
                    .expect("prime fallback session namespace")
                    .sessions_dir()
                    .to_path_buf();
                let records = records_dir(root.path());
                let ownership = root.path().join(".nib/worktree-ownership");
                let ownership_before = std::fs::read_dir(&ownership)
                    .expect("read primed worktree ownership")
                    .map(|entry| entry.expect("worktree ownership entry").file_name())
                    .collect::<std::collections::BTreeSet<_>>();
                let before = subagent_namespace_snapshot(root.path());
                let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let fired_for_hook = fired.clone();
                let records_for_hook = records.clone();
                let sessions_for_hook = sessions.clone();
                let ownership_for_hook = ownership.clone();
                let ownership_before_for_hook = ownership_before.clone();
                let hook = std::sync::Arc::new(move |observed_records: &Path| {
                    if !crate::fs_security::canonical_paths_match(
                        observed_records,
                        &records_for_hook,
                    ) || fired_for_hook.load(std::sync::atomic::Ordering::Acquire)
                    {
                        return Ok(());
                    }
                    let boundary_reached = match phase {
                        "worktree_reservation" => std::fs::read_dir(&ownership_for_hook)
                            .map(|entries| {
                                entries.filter_map(Result::ok).any(|entry| {
                                    !ownership_before_for_hook.contains(&entry.file_name())
                                })
                            })
                            .unwrap_or(false),
                        "session_temp" => std::fs::read_dir(&sessions_for_hook)
                            .map(|entries| {
                                entries.filter_map(Result::ok).any(|entry| {
                                    let name = entry.file_name();
                                    let name = name.to_string_lossy();
                                    name.starts_with(".nib-session-")
                                        && name.ends_with(".tmp")
                                        && std::fs::metadata(entry.path())
                                            .is_ok_and(|metadata| metadata.len() > 0)
                                })
                            })
                            .unwrap_or(false),
                        "session_canonical" => std::fs::read_dir(&sessions_for_hook)
                            .map(|entries| {
                                entries.filter_map(Result::ok).any(|entry| {
                                    entry.file_name().to_string_lossy().ends_with(".json")
                                })
                            })
                            .unwrap_or(false),
                        _ => unreachable!(),
                    };
                    if boundary_reached
                        && fired_for_hook
                            .compare_exchange(
                                false,
                                true,
                                std::sync::atomic::Ordering::AcqRel,
                                std::sync::atomic::Ordering::Acquire,
                            )
                            .is_ok()
                    {
                        std::thread::sleep(EXPIRY_DELAY);
                    }
                    Ok(())
                });
                let timeout_guard = SpawnPreparationTimeoutGuard::set(OPERATION_TIMEOUT);
                let hook_guard = SpawnAuthorityVerifyHookGuard::install(hook);
                let args = json!({"prompt": format!("expire after {phase}")});
                let result = if cancellable {
                    spawn_subagent_cancellable(&args, root.path(), None).await
                } else {
                    spawn_subagent(&args, root.path())
                };
                drop(hook_guard);
                drop(timeout_guard);
                let error = result.expect_err("expired spawn preparation must fail closed");
                assert!(
                    error.contains("deadline") || error.contains("timed out"),
                    "{phase} cancellable={cancellable}: {error}"
                );
                assert!(
                    fired.load(std::sync::atomic::Ordering::Acquire),
                    "{phase} cancellable={cancellable} never reached the durable boundary"
                );
                let preparations = spawn_preparation_directory_path(root.path());
                assert!(
                    preparations.exists()
                        && std::fs::read_dir(&preparations)
                            .expect("read retained deadline preparation")
                            .next()
                            .is_some(),
                    "{phase} cancellable={cancellable} retired an expired preparation"
                );

                assert!(list_subagents(root.path())
                    .expect("fresh-deadline restart reconciliation")
                    .is_empty());
                assert!(
                    std::fs::read_dir(&preparations)
                        .expect("read reconciled deadline preparations")
                        .next()
                        .is_none(),
                    "{phase} cancellable={cancellable} did not retire intent last"
                );
                let after = subagent_namespace_snapshot(root.path());
                assert_spawn_cleanup_snapshot(
                    &before,
                    &after,
                    &format!("{phase} cancellable={cancellable} restart changed namespace entries"),
                );
            }
        }
    }

    #[tokio::test]
    async fn parent_only_internal_audit_argument_is_rejected_before_legacy_session_migration() {
        let root = tempfile::tempdir().expect("project root");
        let legacy = crate::session::SessionStore::new(root.path());
        legacy
            .try_create_session_with_id("legacy-parent")
            .expect("legacy parent session");
        let before = subagent_namespace_snapshot(root.path());

        let args = json!({
            "prompt": "must reject incomplete reserved authority",
            "_parent_session_id": "legacy-parent",
        });
        let sync_error = spawn_subagent(&args, root.path())
            .expect_err("sync parent-only authority must be rejected");
        assert!(sync_error.contains("must be supplied together"));
        let cancellable_error = spawn_subagent_cancellable(&args, root.path(), None)
            .await
            .expect_err("cancellable parent-only authority must be rejected");
        assert!(cancellable_error.contains("must be supplied together"));
        assert_eq!(subagent_namespace_snapshot(root.path()), before);

        let migrated = crate::session::SessionStore::for_project(root.path())
            .expect("normal profile migration remains available");
        assert!(migrated
            .load_result("legacy-parent")
            .expect("load migrated parent")
            .is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_spawn_rejects_selected_state_replacement_after_audit_preflight_without_mutation(
    ) {
        let root = tempfile::tempdir().expect("project root");
        let state = root.path().join(".nib/selected-state");
        let displaced = root.path().join(".nib/selected-state.displaced");
        let mut config = crate::config::NibConfig::default();
        config.profiles = crate::config::ProfilesConfig {
            default: "selected".to_string(),
            active: vec![crate::config::ProfileConfig {
                id: "selected".to_string(),
                root: PathBuf::from("."),
                state_dir: Some(PathBuf::from(".nib/selected-state")),
                ..crate::config::ProfileConfig::default()
            }],
        };
        crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");
        let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
        store
            .try_create_session_with_id("existing-audit")
            .expect("existing audit session");
        let original_snapshot = directory_tree_snapshot(&state);

        let state_for_hook = state.clone();
        let displaced_for_hook = displaced.clone();
        AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK.with(|slot| {
            assert!(slot
                .borrow_mut()
                .replace(Box::new(move || {
                    std::fs::rename(&state_for_hook, &displaced_for_hook)
                        .expect("displace selected state");
                    std::fs::create_dir(&state_for_hook).expect("replacement selected state");
                    std::fs::write(state_for_hook.join("sentinel"), b"replacement")
                        .expect("replacement sentinel");
                }))
                .is_none());
        });

        let error = spawn_subagent(
            &json!({"prompt": "must not mutate replacement"}),
            root.path(),
        )
        .expect_err("selected state replacement must fail closed");
        assert!(
            error.contains("identity changed") || error.contains("changed while"),
            "unexpected replacement error: {error}"
        );
        assert_eq!(directory_tree_snapshot(&displaced), original_snapshot);
        assert_eq!(
            directory_tree_snapshot(&state),
            vec![(PathBuf::from("sentinel"), b"replacement".to_vec())]
        );
        for path in [
            root.path().join(".nib/subagents"),
            root.path().join(".nib/subagent-owner-leases"),
            root.path().join(".nib/worktrees/subagents"),
        ] {
            assert!(
                !path.exists(),
                "replacement failure created {}",
                path.display()
            );
        }

        std::fs::remove_dir_all(&state).expect("remove replacement state");
        std::fs::rename(&displaced, &state).expect("restore selected state");
        prepare_subagent_audit_target(&json!({}), root.path(), "fresh-state-retry")
            .expect("fresh retry binds the restored state capability");
        assert!(state.join("sessions/fresh-state-retry.json").is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellable_spawn_rejects_sessions_replacement_after_audit_preflight_without_mutation()
    {
        let root = tempfile::tempdir().expect("project root");
        let state = root.path().join(".nib/selected-state");
        let sessions = state.join("sessions");
        let displaced = state.join("sessions.displaced");
        let mut config = crate::config::NibConfig::default();
        config.profiles = crate::config::ProfilesConfig {
            default: "selected".to_string(),
            active: vec![crate::config::ProfileConfig {
                id: "selected".to_string(),
                root: PathBuf::from("."),
                state_dir: Some(PathBuf::from(".nib/selected-state")),
                ..crate::config::ProfileConfig::default()
            }],
        };
        crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");
        let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
        store
            .try_create_session_with_id("existing-audit")
            .expect("existing audit session");
        let original_snapshot = directory_tree_snapshot(&sessions);

        let sessions_for_hook = sessions.clone();
        let displaced_for_hook = displaced.clone();
        AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK.with(|slot| {
            assert!(slot
                .borrow_mut()
                .replace(Box::new(move || {
                    std::fs::rename(&sessions_for_hook, &displaced_for_hook)
                        .expect("displace sessions directory");
                    std::fs::create_dir(&sessions_for_hook).expect("replacement sessions");
                    std::fs::write(sessions_for_hook.join("sentinel"), b"replacement")
                        .expect("replacement sentinel");
                }))
                .is_none());
        });

        let error = spawn_subagent_cancellable(
            &json!({"prompt": "must not mutate replacement"}),
            root.path(),
            None,
        )
        .await
        .expect_err("sessions replacement must fail closed");
        assert!(
            error.contains("identity changed") || error.contains("changed while"),
            "unexpected replacement error: {error}"
        );
        assert_eq!(directory_tree_snapshot(&displaced), original_snapshot);
        assert_eq!(
            directory_tree_snapshot(&sessions),
            vec![(PathBuf::from("sentinel"), b"replacement".to_vec())]
        );
        for path in [
            root.path().join(".nib/subagents"),
            root.path().join(".nib/subagent-owner-leases"),
            root.path().join(".nib/worktrees/subagents"),
        ] {
            assert!(
                !path.exists(),
                "replacement failure created {}",
                path.display()
            );
        }

        std::fs::remove_dir_all(&sessions).expect("remove replacement sessions");
        std::fs::rename(&displaced, &sessions).expect("restore sessions directory");
        prepare_subagent_audit_target(&json!({}), root.path(), "fresh-sessions-retry")
            .expect("fresh retry binds the restored sessions capability");
        assert!(sessions.join("fresh-sessions-retry.json").is_file());
    }

    #[tokio::test]
    async fn direct_spawn_rejects_missing_profile_parent_appearance_after_audit_preflight() {
        let root = tempfile::tempdir().expect("project root");
        let mut config = crate::config::NibConfig::default();
        crate::config::save_nib_config_full(root.path(), &mut config).expect("default config");
        let profiles = root.path().join(".nib/profiles");
        assert!(!profiles.exists(), "fixture profile parent starts absent");

        let profiles_for_hook = profiles.clone();
        AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK.with(|slot| {
            assert!(slot
                .borrow_mut()
                .replace(Box::new(move || {
                    std::fs::create_dir(&profiles_for_hook).expect("appearing profile parent");
                    std::fs::write(profiles_for_hook.join("sentinel"), b"appeared")
                        .expect("appearing parent sentinel");
                }))
                .is_none());
        });

        let error = spawn_subagent(
            &json!({"prompt": "must preserve appearing parent"}),
            root.path(),
        )
        .expect_err("post-preflight profile parent must fail closed");
        assert!(
            error.contains("appeared after its absence was proven"),
            "unexpected appearing-parent error: {error}"
        );
        assert_eq!(
            std::fs::read(profiles.join("sentinel")).expect("preserved sentinel"),
            b"appeared"
        );
        assert_eq!(
            std::fs::read_dir(&profiles)
                .expect("preserved profile parent")
                .count(),
            1,
            "failed preflight must not add profile descendants"
        );
        for path in [
            root.path().join(".nib/subagents"),
            root.path().join(".nib/subagent-owner-leases"),
            root.path().join(".nib/worktrees/subagents"),
        ] {
            assert!(
                !path.exists(),
                "appearing parent created {}",
                path.display()
            );
        }

        std::fs::remove_dir_all(&profiles).expect("remove appearing profile parent");
        prepare_subagent_audit_target(&json!({}), root.path(), "fresh-missing-parent-retry")
            .expect("fresh retry creates the preflighted profile hierarchy");
        assert!(root
            .path()
            .join(".nib/profiles/default/sessions/fresh-missing-parent-retry.json")
            .is_file());
    }

    #[test]
    fn direct_list_projects_running_authority_and_preserves_terminal_and_merge_results() {
        let root = tempfile::tempdir().expect("root");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("live owner lease");
        let mut running = record_fixture(root.path(), "sub-public-running", "running");
        attach_execution_ownership(&mut running, &owner_lease);
        let private_path = root.path().join("private-audit-sessions");
        running.result = Some(json!({
            "_ownership_audit_target": {
                "sessions_dir": private_path,
                "directory_identity": "private-file-identity",
            }
        }));
        write_subagent_record(root.path(), &running).expect("running record");

        let listed = list_subagents(root.path()).expect("public subagent list");
        assert_eq!(listed.len(), 1);
        let public = &listed[0];
        assert_eq!(public["id"], running.id);
        assert!(public["result"].is_null());
        assert!(public.get("execution_generation").is_none());
        assert!(public.get("owner_lease").is_none());
        let encoded = serde_json::to_string(public).expect("public record");
        assert!(!encoded.contains(OWNERSHIP_AUDIT_TARGET_KEY));
        assert!(!encoded.contains("private-audit-sessions"));
        assert!(!encoded.contains("private-file-identity"));

        let persisted =
            get_subagent_record_unreconciled(root.path(), &running.id).expect("internal record");
        assert!(persisted.execution_generation.is_some());
        assert!(persisted.owner_lease.is_some());
        assert!(subagent_audit_target(&persisted).is_err());

        let terminal = project_public_subagent_result(json!({
            "outcome": "completed",
            "summary": "public result",
            "cleanup_verified": true,
            "cleanup_proof": {"private": "authority"},
            "_ownership_audit_target": {"private": "target"},
        }))
        .expect("terminal public result");
        assert_eq!(
            terminal,
            json!({"outcome": "completed", "summary": "public result"})
        );
        let manager_id = format!("sub-public-manager-{}", uuid::Uuid::new_v4());
        crate::daemons::task::TASK_MANAGER
            .register_task(manager_id.clone(), "subagent")
            .expect("manager record");
        let mut terminal_record = record_fixture(root.path(), &manager_id, "completed");
        terminal_record.result = Some(json!({
            "outcome": "completed",
            "summary": "public result",
            "cleanup_verified": true,
            "cleanup_proof": {"private": "authority"},
            "_ownership_audit_target": {"private": "target"},
        }));
        sync_subagent_task_manager(&terminal_record);
        let manager_public = crate::daemons::task::TASK_MANAGER
            .get_task(&manager_id)
            .expect("public manager record");
        assert_eq!(manager_public["result"], terminal);

        let merged = project_public_subagent_result(json!({
            "subagent_result": {
                "summary": "done",
                "ownership_reconciliation": {"private": "authority"},
                "_ownership_audit_target": {"private": "target"},
            },
            "merge_commit": "abc123",
            "merge_stdout": "merged",
        }))
        .expect("merge public result");
        assert_eq!(merged["subagent_result"], json!({"summary": "done"}));
        assert_eq!(merged["merge_commit"], "abc123");
        assert_eq!(merged["merge_stdout"], "merged");
    }

    fn remove_visible_owner_lease(owner_lease: &SubagentOwnerLease) {
        let owner_file = owner_lease.file.as_ref().expect("owner handle");
        owner_lease
            .visible_directory
            .as_ref()
            .expect("visible directory")
            .remove_file_if_matches(
                &owner_lease.visible_path,
                owner_file,
                ".nib-test-owner-visible-delete-",
            )
            .expect("remove visible owner lease");
    }

    fn install_completed_process_scope(
        root: &Path,
        id: &str,
        execution_generation: u64,
    ) -> crate::sandbox::process::CleanupProof {
        install_completed_process_scope_with_outcome(root, id, execution_generation, "fixture")
    }

    fn install_completed_process_scope_with_outcome(
        root: &Path,
        id: &str,
        execution_generation: u64,
        outcome: &str,
    ) -> crate::sandbox::process::CleanupProof {
        let store = crate::sandbox::process::ProcessScopeStore::open(root).expect("scope store");
        let identity = crate::sandbox::process::ProcessIdentity::current().expect("identity");
        let mut scope = store
            .prepare(
                id,
                "subagent",
                execution_generation,
                identity.clone(),
                crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepared scope");
        let proof = crate::sandbox::process::CleanupProof {
            execution_generation,
            cleanup_lease_id: scope.cleanup_lease_id.clone(),
            backend: scope.backend,
            direct_child: identity.clone(),
            outcome: outcome.to_string(),
            descendants_reaped: true,
            completed_at: Utc::now(),
        };
        scope.status = crate::sandbox::process::ProcessScopeStatus::Complete;
        scope.launch_committed = Some(true);
        scope.supervisor = Some(identity.clone());
        scope.direct_child = Some(identity);
        scope.cleanup_reason = Some(outcome.to_string());
        scope.cleanup_proof = Some(proof.clone());
        scope.updated_at = Utc::now();
        std::fs::write(
            root.join(".nib/process-scopes").join(format!("{id}.json")),
            serde_json::to_vec_pretty(&scope).expect("encode complete scope"),
        )
        .expect("write complete scope");
        proof
    }

    fn install_completed_launch_abort_scope(
        root: &Path,
        id: &str,
        execution_generation: u64,
    ) -> crate::sandbox::process::LaunchAbortProof {
        let store = crate::sandbox::process::ProcessScopeStore::open(root).expect("scope store");
        let identity = crate::sandbox::process::ProcessIdentity::current().expect("identity");
        let prepared = store
            .prepare(
                id,
                "subagent",
                execution_generation,
                identity.clone(),
                crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepared scope");
        let mut scope = store
            .register_launch_supervisor(
                id,
                execution_generation,
                &prepared.cleanup_lease_id,
                identity.clone(),
            )
            .expect("registered launch supervisor");
        let proof = crate::sandbox::process::LaunchAbortProof {
            execution_generation,
            cleanup_lease_id: scope.cleanup_lease_id.clone(),
            backend: scope.backend,
            supervisor: identity,
            namespace_root: None,
            outcome: "gate_eof_before_running".to_string(),
            workload_never_launched: true,
            completed_at: Utc::now(),
        };
        scope.status = crate::sandbox::process::ProcessScopeStatus::Complete;
        scope.cleanup_reason = Some(proof.outcome.clone());
        scope.launch_abort_proof = Some(proof.clone());
        scope.updated_at = Utc::now();
        std::fs::write(
            root.join(".nib/process-scopes").join(format!("{id}.json")),
            serde_json::to_vec_pretty(&scope).expect("encode launch-abort scope"),
        )
        .expect("write launch-abort scope");
        proof
    }

    fn install_completed_process_scope_with_live_lease(
        root: &Path,
        id: &str,
        execution_generation: u64,
    ) -> (
        crate::sandbox::process::CleanupProof,
        crate::sandbox::process::CleanupLease,
    ) {
        let store = crate::sandbox::process::ProcessScopeStore::open(root).expect("scope store");
        let identity = crate::sandbox::process::ProcessIdentity::current().expect("identity");
        let mut scope = store
            .prepare(
                id,
                "subagent",
                execution_generation,
                identity.clone(),
                crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepared scope");
        let lease = store
            .acquire_cleanup_lease(&scope)
            .expect("live cleanup lease");
        let proof = crate::sandbox::process::CleanupProof {
            execution_generation,
            cleanup_lease_id: scope.cleanup_lease_id.clone(),
            backend: scope.backend,
            direct_child: identity.clone(),
            outcome: "fixture".to_string(),
            descendants_reaped: true,
            completed_at: Utc::now(),
        };
        scope.status = crate::sandbox::process::ProcessScopeStatus::Complete;
        scope.launch_committed = Some(true);
        scope.supervisor = Some(identity.clone());
        scope.direct_child = Some(identity);
        scope.cleanup_reason = Some("fixture".to_string());
        scope.cleanup_proof = Some(proof.clone());
        scope.updated_at = Utc::now();
        std::fs::write(
            root.join(".nib/process-scopes").join(format!("{id}.json")),
            serde_json::to_vec_pretty(&scope).expect("encode complete scope"),
        )
        .expect("write complete scope");
        (proof, lease)
    }

    fn terminal_record_fixture(
        root: &Path,
        id: &str,
        execution_generation: u64,
        owner_lease: &str,
        proof: &crate::sandbox::process::CleanupProof,
    ) -> SubagentRecord {
        let mut record = record_fixture(root, id, "completed");
        record.execution_generation = Some(execution_generation);
        record.owner_lease = Some(owner_lease.to_string());
        record.result = Some(json!({
            "cleanup_verified": true,
            "cleanup_proof": proof,
        }));
        record
    }

    fn launch_abort_terminal_record_fixture(
        root: &Path,
        id: &str,
        execution_generation: u64,
        owner_lease: &str,
        proof: &crate::sandbox::process::LaunchAbortProof,
    ) -> SubagentRecord {
        let mut record = record_fixture(root, id, "failed");
        record.execution_generation = Some(execution_generation);
        record.owner_lease = Some(owner_lease.to_string());
        record.result = Some(json!({
            "cleanup_verified": false,
            "launch_abort_verified": true,
            "workload_never_launched": true,
            "launch_abort_proof": proof,
        }));
        record
    }

    #[test]
    fn terminal_scope_retirement_requires_a_locked_full_record_and_exact_authority() {
        let root = tempfile::tempdir().expect("root");
        let execution_generation = 7001;
        let id = "sub-terminal-retirement";
        let proof = install_completed_process_scope(root.path(), id, execution_generation);
        let owner_lease = uuid::Uuid::new_v4().to_string();
        let record =
            terminal_record_fixture(root.path(), id, execution_generation, &owner_lease, &proof);
        let _records = ensure_records_directory(root.path()).expect("records directory");
        let path = record_path(root.path(), id).expect("record path");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "id": id,
                "status": "completed",
                "result": record.result.clone(),
            }))
            .expect("partial record"),
        )
        .expect("write partial record");
        assert!(retire_terminal_process_scope(root.path(), &record).is_err());
        assert!(root
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json"))
            .exists());
        std::fs::remove_file(&path).expect("remove partial record");
        write_subagent_record(root.path(), &record).expect("full terminal record");
        assert!(retire_terminal_process_scope(root.path(), &record).expect("retire exact scope"));
        assert!(!root
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json"))
            .exists());
    }

    #[test]
    fn delegated_provider_failure_preserves_typed_private_terminal_evidence() {
        let root = tempfile::tempdir().expect("root");
        let id = "sub-typed-provider-failure";
        let execution_generation = 81_337;
        let lease_id = uuid::Uuid::new_v4().to_string();
        let mut record = record_fixture(root.path(), id, "running");
        record.execution_generation = Some(execution_generation);
        record.owner_lease = Some(lease_id.clone());
        write_subagent_record(root.path(), &record).expect("running subagent record");
        crate::daemons::task::TASK_MANAGER
            .register_task(id.to_string(), "subagent")
            .expect("register delegated task");

        let secret = "delegated-provider-private-secret".to_string();
        let sensitive_values = vec![secret.clone()];
        let failure = crate::llm::LlmError::new(
            crate::llm::LlmErrorClass::Authentication,
            crate::llm::LlmErrorPhase::HttpResponse,
            crate::llm::RetryDisposition::NotRetryable,
            crate::llm::LlmErrorMetadata::new(
                "openai",
                "responses",
                Some("fixture-model"),
                Some(401),
                &sensitive_values,
            ),
            format!("provider rejected private credential {secret}"),
        );
        let summary = crate::agent::AgentRunSummary {
            session_id: format!("child-{id}"),
            run_id: "0123456789abcdef0123456789abcdef".to_string(),
            steps_taken: 1,
            last_message: None,
            tool_call_count: 0,
            final_state: crate::agent::state::AgentState::Done,
            outcome: "llm_stream_failed".to_string(),
            failure: Some(failure),
            bound_reached: false,
            trace: vec!["reconciliation".to_string(), "done".to_string()],
        };

        persist_subagent_outcome(
            root.path(),
            id,
            execution_generation,
            &lease_id,
            Ok(summary),
        )
        .expect("persist delegated provider failure");

        let persisted =
            get_subagent_record_unreconciled(root.path(), id).expect("persisted delegated record");
        assert_eq!(persisted.status, "failed");
        let result = persisted.result.expect("delegated failure result");
        assert_eq!(result["outcome"], "llm_stream_failed");
        assert!(result["last_message"].is_null());
        assert_eq!(result["failure"]["class"], "authentication");
        assert_eq!(result["failure"]["incident_code"], "LLM-AUTH");
        assert_eq!(result["failure"]["provider"], "openai");
        assert_eq!(result["failure"]["transport"], "responses");
        assert_eq!(result["failure"]["http_status"], 401);

        let observed = crate::daemons::task::TASK_MANAGER
            .get_task(id)
            .expect("delegated task observer");
        assert_eq!(observed["status"], "failed");
        assert_eq!(
            observed["result"]["failure"]["class"],
            result["failure"]["class"]
        );
        assert_eq!(
            observed["result"]["failure"]["incident_code"],
            result["failure"]["incident_code"]
        );
        let encoded = format!(
            "{}\n{}",
            serde_json::to_string(&result).unwrap(),
            serde_json::to_string(&observed).unwrap()
        );
        assert!(!encoded.contains(&secret));
        assert!(!encoded.contains("provider rejected private credential"));
    }

    #[test]
    fn terminal_scope_retirement_rejects_mismatched_ownership_and_stale_snapshots() {
        let root = tempfile::tempdir().expect("root");
        let execution_generation = 7002;
        let id = "sub-terminal-stale";
        let proof = install_completed_process_scope(root.path(), id, execution_generation);
        let owner_lease = uuid::Uuid::new_v4().to_string();
        let mut record =
            terminal_record_fixture(root.path(), id, execution_generation, &owner_lease, &proof);

        let mut mismatched = record.clone();
        mismatched.execution_generation = Some(execution_generation + 1);
        assert!(terminal_process_scope_authority(&mismatched).is_err());
        mismatched = record.clone();
        mismatched.result = Some(json!({
            "outcome": "interrupted",
            "ownership_reconciliation": {
                "subagent_id": id,
                "execution_generation": execution_generation,
                "owner_lease": uuid::Uuid::new_v4().to_string(),
                "terminal_status": "completed",
                "cleanup_verified": true,
                "cleanup_proof": proof,
            }
        }));
        assert!(terminal_process_scope_authority(&mismatched).is_err());

        write_subagent_record(root.path(), &record).expect("terminal record");
        let stale = record.clone();
        record.error = Some("replacement revision".to_string());
        record.updated_at = Utc::now() + chrono::Duration::milliseconds(1);
        update_subagent_record(root.path(), id, |current| {
            *current = record.clone();
            Ok(())
        })
        .expect("replacement record");
        assert!(retire_terminal_process_scope(root.path(), &stale).is_err());
        assert!(root
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json"))
            .exists());
    }

    #[test]
    fn terminal_scope_retirement_retries_after_merge_status_advances() {
        let root = tempfile::tempdir().expect("root");
        let execution_generation = 7003;
        let id = "sub-terminal-retirement-retry";
        let (proof, cleanup_lease) =
            install_completed_process_scope_with_live_lease(root.path(), id, execution_generation);
        let owner_lease = uuid::Uuid::new_v4().to_string();
        let mut record =
            terminal_record_fixture(root.path(), id, execution_generation, &owner_lease, &proof);
        write_subagent_record(root.path(), &record).expect("terminal record");

        let error = retire_terminal_process_scope(root.path(), &record)
            .expect_err("live cleanup lease must defer retirement");
        assert!(error.contains("cleanup lease exists"), "{error}");

        let subagent_result = record.result.take();
        record.status = MERGE_PENDING_STATUS.to_string();
        record.result = Some(json!({
            "subagent_result": subagent_result,
            "verification_command": "task check",
            "merge_commit": "a".repeat(40),
            "parent_head_before": "b".repeat(40),
            "active_merge_base": Value::Null,
            "merge_stdout": Value::Null,
        }));
        record.updated_at = Utc::now() + chrono::Duration::milliseconds(1);
        update_subagent_record(root.path(), id, |current| {
            *current = record.clone();
            Ok(())
        })
        .expect("advance terminal record to merge pending");
        cleanup_lease
            .release_after_proof(&proof)
            .expect("release transient cleanup lease");

        let advanced =
            get_subagent_record_unreconciled(root.path(), id).expect("advanced terminal record");
        assert!(retire_terminal_process_scope(root.path(), &advanced)
            .expect("retry retirement from merge-pending authority"));
        assert!(!root
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json"))
            .exists());
    }

    #[test]
    fn terminal_scope_retirement_authority_survives_every_supported_status_shape() {
        let root = tempfile::tempdir().expect("root");
        let execution_generation = 7004;
        let owner_lease = uuid::Uuid::new_v4().to_string();
        let proof = install_completed_process_scope(
            root.path(),
            "sub-terminal-authority-statuses",
            execution_generation,
        );
        let base = terminal_record_fixture(
            root.path(),
            "sub-terminal-authority-statuses",
            execution_generation,
            &owner_lease,
            &proof,
        );

        for status in [
            "completed",
            "failed",
            "cancelled",
            "verification_failed",
            MERGE_FAILED_STATUS,
        ] {
            let mut record = base.clone();
            record.status = status.to_string();
            let authority = terminal_process_scope_authority(&record)
                .expect("direct authority")
                .expect("direct proof");
            assert!(matches!(
                authority.1,
                TerminalProcessScopeAuthority::Cleanup(ref observed) if observed == &proof
            ));
        }

        for status in [MERGE_PENDING_STATUS, "merged"] {
            let mut record = base.clone();
            record.status = status.to_string();
            record.result = Some(json!({
                "subagent_result": record.result.take(),
            }));
            let authority = terminal_process_scope_authority(&record)
                .expect("nested authority")
                .expect("nested proof");
            assert!(matches!(
                authority.1,
                TerminalProcessScopeAuthority::Cleanup(ref observed) if observed == &proof
            ));
        }
    }

    #[test]
    fn terminal_scope_retirement_accepts_exact_launch_abort_authority() {
        let root = tempfile::tempdir().expect("root");
        let execution_generation = 7005;
        let id = "sub-terminal-launch-abort";
        let owner_lease = uuid::Uuid::new_v4().to_string();
        let proof = install_completed_launch_abort_scope(root.path(), id, execution_generation);
        let base = launch_abort_terminal_record_fixture(
            root.path(),
            id,
            execution_generation,
            &owner_lease,
            &proof,
        );

        for status in [
            "completed",
            "failed",
            "cancelled",
            "verification_failed",
            MERGE_FAILED_STATUS,
        ] {
            let mut record = base.clone();
            record.status = status.to_string();
            let authority = terminal_process_scope_authority(&record)
                .expect("direct launch-abort authority")
                .expect("direct launch-abort proof");
            assert!(matches!(
                authority.1,
                TerminalProcessScopeAuthority::LaunchAbort(ref observed) if observed == &proof
            ));
        }

        for status in [MERGE_PENDING_STATUS, "merged"] {
            let mut record = base.clone();
            record.status = status.to_string();
            record.result = Some(json!({
                "subagent_result": record.result.take(),
            }));
            let authority = terminal_process_scope_authority(&record)
                .expect("nested launch-abort authority")
                .expect("nested launch-abort proof");
            assert!(matches!(
                authority.1,
                TerminalProcessScopeAuthority::LaunchAbort(ref observed) if observed == &proof
            ));
        }

        write_subagent_record(root.path(), &base).expect("terminal launch-abort record");
        assert!(
            retire_terminal_process_scope(root.path(), &base).expect("retire launch-aborted scope")
        );
        assert!(!root
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json"))
            .exists());
    }

    #[test]
    fn terminal_scope_retirement_rejects_forged_launch_abort_authority() {
        let root = tempfile::tempdir().expect("root");
        let execution_generation = 7006;
        let id = "sub-terminal-launch-abort-forgery";
        let owner_lease = uuid::Uuid::new_v4().to_string();
        let proof = install_completed_launch_abort_scope(root.path(), id, execution_generation);
        let base = launch_abort_terminal_record_fixture(
            root.path(),
            id,
            execution_generation,
            &owner_lease,
            &proof,
        );

        let mut forged = base.clone();
        forged.result.as_mut().expect("result")["launch_abort_proof"]["execution_generation"] =
            json!(execution_generation + 1);
        let error = terminal_process_scope_authority(&forged)
            .expect_err("mismatched launch-abort generation must be rejected");
        assert!(error.contains("does not own its execution"), "{error}");

        forged = base.clone();
        forged.result.as_mut().expect("result")["workload_never_launched"] = Value::Bool(false);
        let error = terminal_process_scope_authority(&forged)
            .expect_err("unverified workload launch state must be rejected");
        assert!(error.contains("incomplete proof evidence"), "{error}");

        forged = base.clone();
        forged.result.as_mut().expect("result")["launch_abort_verified"] = Value::Bool(false);
        let error = terminal_process_scope_authority(&forged)
            .expect_err("proof without verified launch abort must be rejected");
        assert!(error.contains("without verified launch abort"), "{error}");

        let cleanup_proof = crate::sandbox::process::CleanupProof {
            execution_generation,
            cleanup_lease_id: proof.cleanup_lease_id.clone(),
            backend: proof.backend,
            direct_child: proof.supervisor.clone(),
            outcome: "forged_cleanup".to_string(),
            descendants_reaped: true,
            completed_at: Utc::now(),
        };
        forged = base.clone();
        let result = forged
            .result
            .as_mut()
            .and_then(Value::as_object_mut)
            .expect("result object");
        result.insert("cleanup_verified".to_string(), Value::Bool(true));
        result.insert(
            "cleanup_proof".to_string(),
            serde_json::to_value(cleanup_proof).expect("cleanup proof"),
        );
        let error = terminal_process_scope_authority(&forged)
            .expect_err("cleanup and launch-abort authority must be mutually exclusive");
        assert!(
            error.contains("conflicts with cleanup verification"),
            "{error}"
        );

        for (field, value) in [
            (
                "owner_lease",
                Value::String(uuid::Uuid::new_v4().to_string()),
            ),
            ("terminal_status", Value::String("completed".to_string())),
        ] {
            forged = base.clone();
            forged.result = Some(json!({
                "ownership_reconciliation": {
                    "subagent_id": id,
                    "execution_generation": execution_generation,
                    "owner_lease": owner_lease,
                    "terminal_status": "failed",
                    "cleanup_verified": false,
                    "launch_abort_verified": true,
                    "workload_never_launched": true,
                    "launch_abort_proof": proof,
                }
            }));
            forged.result.as_mut().expect("result")["ownership_reconciliation"][field] = value;
            let error = terminal_process_scope_authority(&forged)
                .expect_err("mismatched reconciliation ownership must be rejected");
            assert!(
                error.contains("does not match subagent execution ownership"),
                "{error}"
            );
        }

        assert!(root
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json"))
            .exists());
    }

    #[test]
    fn subagent_owner_process_loss_child() {
        let Some(project_root) = std::env::var_os(OWNER_LOSS_CHILD_PROJECT_ROOT) else {
            return;
        };
        let ready = PathBuf::from(
            std::env::var_os(OWNER_LOSS_CHILD_READY).expect("owner-loss child ready path"),
        );
        let project_root = PathBuf::from(project_root);
        let owner_lease =
            SubagentOwnerLease::create(&project_root).expect("child owner lease is acquired");
        let mut record = record_fixture(&project_root, OWNER_LOSS_SUBAGENT_ID, "running");
        record.parent_session_id = Some("parent-owner-process-loss".to_string());
        attach_execution_ownership(&mut record, &owner_lease);
        write_subagent_record(&project_root, &record).expect("child running record is durable");
        std::fs::write(ready, b"ready").expect("child readiness is published");
        let _owner_lease = owner_lease;
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    fn legacy_open_before_lock_child_process() {
        let Some(project_root) = std::env::var_os(LEGACY_OPEN_CHILD_PROJECT_ROOT) else {
            return;
        };
        let project_root = PathBuf::from(project_root);
        let ready = PathBuf::from(
            std::env::var_os(LEGACY_OPEN_CHILD_READY).expect("legacy child ready path"),
        );
        let resume = PathBuf::from(
            std::env::var_os(LEGACY_OPEN_CHILD_RESUME).expect("legacy child resume path"),
        );
        let locked = PathBuf::from(
            std::env::var_os(LEGACY_OPEN_CHILD_LOCKED).expect("legacy child locked path"),
        );
        let release = PathBuf::from(
            std::env::var_os(LEGACY_OPEN_CHILD_RELEASE).expect("legacy child release path"),
        );
        let visible = records_dir(&project_root)
            .join(".locks")
            .join(format!("{LEGACY_OPEN_CHILD_ID}.lock"));
        let anchor = crate::daemons::state::daemon_lock_anchor_path(&visible)
            .expect("legacy child anchor path");
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&anchor)
            .expect("legacy child opens the exact anchor before locking");
        std::fs::write(&ready, b"opened").expect("publish opened readiness");
        let started = Instant::now();
        while !resume.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "legacy child was not resumed after opening"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        owner
            .try_lock()
            .expect("the exact opened legacy anchor remains lockable");
        std::fs::write(&locked, b"locked").expect("publish legacy lock acquisition");
        let started = Instant::now();
        while !release.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "legacy child was not released"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn killed_owner_without_process_scope_requires_recovery_and_preserves_leases() {
        let root = tempfile::tempdir().expect("root");
        let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
        store.create_session_with_id("parent-owner-process-loss");
        let ready = root.path().join("owner-loss.ready");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::subagent_owner_process_loss_child",
                "--nocapture",
            ])
            .env(OWNER_LOSS_CHILD_PROJECT_ROOT, root.path())
            .env(OWNER_LOSS_CHILD_READY, &ready)
            .spawn()
            .expect("spawn owner process");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect owner process") {
                panic!("owner process exited before readiness: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "owner process did not publish readiness"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let live = get_subagent_record_internal(root.path(), OWNER_LOSS_SUBAGENT_ID)
            .expect("cross-process live owner remains running");
        assert_eq!(live.status, "running");
        assert!(
            live.result.is_none(),
            "live owner probing must not publish missing-scope recovery evidence"
        );
        let lease_id = live.owner_lease.clone().expect("owner lease id");
        child.kill().expect("kill owner process");
        child.wait().expect("reap owner process");

        let reconciled = get_subagent_record_internal(root.path(), OWNER_LOSS_SUBAGENT_ID)
            .expect("process loss requires recovery");
        assert_eq!(reconciled.status, "running");
        let result = reconciled.result.expect("recovery evidence");
        assert_eq!(result["outcome"], "recovery_required");
        assert_eq!(result["process_scope"]["status"], "missing");
        assert_eq!(result["cleanup_verified"], false);
        assert!(reconciled.error.is_none());
        let session = store
            .load_result("parent-owner-process-loss")
            .expect("load audit session")
            .expect("audit session");
        assert!(!session
            .events
            .iter()
            .any(|event| event.kind == "subagent_execution_reconciled"));
        assert!(owner_lease_path(root.path(), &lease_id)
            .expect("visible lease path")
            .exists());
        assert!(owner_lease_anchor_path(root.path(), &lease_id)
            .expect("lease anchor path")
            .exists());
    }

    #[test]
    fn expired_legacy_audit_resolution_does_not_migrate_or_create_profile_state() {
        let root = tempfile::tempdir().expect("root");
        let nib = root.path().join(".nib");
        std::fs::create_dir(&nib).expect("legacy nib directory");
        std::fs::write(
            nib.join("config.json"),
            serde_json::to_vec_pretty(&crate::config::LlmConfig::default()).expect("legacy config"),
        )
        .expect("write legacy config");
        std::fs::create_dir(nib.join("sessions")).expect("legacy session source");
        let record = record_fixture(root.path(), "sub-expired-audit-target", "running");

        let error = resolve_legacy_subagent_audit_target(
            root.path(),
            &record,
            Some(Instant::now() - Duration::from_millis(1)),
        )
        .expect_err("expired audit setup must fail closed");

        assert!(error.contains("deadline elapsed"), "{error}");
        assert!(nib.join("config.json").is_file());
        assert!(!nib.join("config.toml").exists());
        assert!(!nib.join("config.json.bak").exists());
        assert!(!nib.join("profiles").exists());
        assert!(!nib.join(".legacy-state-migration-v1.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_audit_target_waits_for_evacuated_config_before_terminal_cas() {
        let root = tempfile::tempdir().expect("root");
        let nib = root.path().join(".nib");
        let wrong_sessions = nib.join("profiles/default/sessions");
        let selected_sessions = nib.join("profiles/selected/sessions");
        let wrong_store = crate::session::SessionStore::at_dir(wrong_sessions.clone());
        wrong_store.create_session_with_id("parent");
        let selected_store = crate::session::SessionStore::at_dir(selected_sessions.clone());
        selected_store.create_session_with_id("parent");

        let mut initial = crate::config::NibConfig::default();
        crate::config::save_nib_config_full(root.path(), &mut initial)
            .expect("initial TOML configuration");
        std::fs::write(
            nib.join("config.json"),
            serde_json::to_vec_pretty(&crate::config::LlmConfig::default())
                .expect("legacy JSON configuration"),
        )
        .expect("legacy JSON fallback");
        let wrong_namespace = directory_tree_snapshot(&wrong_sessions);

        let mut replacement = initial.clone();
        replacement.revision = replacement
            .revision
            .checked_add(1)
            .expect("replacement revision");
        replacement.profiles = crate::config::ProfilesConfig {
            default: "selected".to_string(),
            active: vec![crate::config::ProfileConfig {
                id: "selected".to_string(),
                root: PathBuf::from("."),
                state_dir: Some(PathBuf::from(".nib/profiles/selected")),
                ..crate::config::ProfileConfig::default()
            }],
        };
        let replacement_bytes = toml::to_string_pretty(&replacement)
            .expect("encode replacement configuration")
            .into_bytes();

        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let execution_generation = owner_lease.execution_generation;
        let id = "sub-config-evacuation-audit";
        let mut record = record_fixture(root.path(), id, "running");
        attach_execution_ownership(&mut record, &owner_lease);
        install_completed_process_scope(root.path(), id, execution_generation);
        write_subagent_record(root.path(), &record).expect("legacy running record");
        drop(owner_lease);

        let (evacuated_tx, evacuated_rx) = std::sync::mpsc::sync_channel(1);
        let (publish_tx, publish_rx) = std::sync::mpsc::sync_channel(1);
        let writer_nib = nib.clone();
        let config_path = nib.join("config.toml");
        let writer_config_path = config_path.clone();
        let writer = std::thread::spawn(move || {
            let directory = crate::daemons::state::StableDirectory::open(&writer_nib)
                .expect("stable config directory");
            let previous = directory
                .open_read(&writer_config_path)
                .expect("opened prior config");
            directory
                .save_bytes_atomically_expected_with_after_evacuation_hook(
                    &writer_config_path,
                    &replacement_bytes,
                    ".config.toml.tmp-",
                    crate::daemons::state::FileExpectation::Present(&previous),
                    || {
                        evacuated_tx.send(()).expect("publish evacuation pause");
                        publish_rx.recv().expect("resume config publication");
                    },
                )
                .expect("publish replacement config")
        });
        evacuated_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("config writer evacuated its target");
        assert!(
            !config_path.exists(),
            "config target is intentionally evacuated"
        );
        assert!(
            nib.join("config.json").is_file(),
            "wrong legacy fallback exists"
        );

        let reconcile_root = root.path().to_path_buf();
        let reconciler = std::thread::spawn(move || {
            reconcile_subagent_ownership_until(
                &reconcile_root,
                id,
                Instant::now() + Duration::from_secs(2),
            )
        });
        std::thread::sleep(Duration::from_millis(75));
        let still_running =
            get_subagent_record_unreconciled(root.path(), id).expect("record during evacuation");
        assert_eq!(still_running.status, "running");
        assert!(
            subagent_audit_target(&still_running)
                .expect("valid running metadata")
                .is_none(),
            "an evacuated config must not pin the legacy/default target"
        );
        publish_tx.send(()).expect("resume config writer");
        writer.join().expect("config writer");
        let terminal = reconciler
            .join()
            .expect("reconciliation worker")
            .expect("terminal reconciliation");

        let target = subagent_audit_target(&terminal)
            .expect("valid terminal target")
            .expect("pinned terminal target");
        assert_eq!(
            target.sessions_dir,
            selected_sessions.canonicalize().expect("selected sessions")
        );
        assert_ne!(
            target.sessions_dir,
            wrong_sessions.canonicalize().expect("wrong sessions")
        );
        assert_eq!(
            directory_tree_snapshot(&wrong_sessions),
            wrong_namespace,
            "coherent resolution must not read, pin, or mutate the legacy/default store"
        );
        assert!(nib.join("config.json").is_file());
        assert!(!nib.join("config.json.bak").exists());
        assert!(!nib.join(".legacy-state-migration-v1.json").exists());
        let audit = selected_store
            .load_result("parent")
            .expect("load selected audit session")
            .expect("selected audit session");
        assert_eq!(
            audit
                .events
                .iter()
                .filter(|event| event.kind == "subagent_execution_reconciled")
                .count(),
            1
        );
    }

    #[test]
    fn terminal_retry_finishes_owner_cleanup_and_audits_exactly_once() {
        let root = tempfile::tempdir().expect("root");
        let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
        store.create_session_with_id("parent");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let lease_id = owner_lease.lease_id.clone();
        let execution_generation = owner_lease.execution_generation;
        let id = "sub-terminal-cleanup-retry";
        let mut record = record_fixture(root.path(), id, "running");
        attach_execution_ownership(&mut record, &owner_lease);
        install_completed_process_scope(root.path(), id, execution_generation);
        write_subagent_record(root.path(), &record).expect("running record");
        drop(owner_lease);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let project_root = root.path().to_path_buf();
        let holder = std::thread::spawn(move || {
            with_bounded_delegation_lock_in(
                &owner_lease_namespace_lock_path(&project_root),
                &project_root.join(".nib"),
                Duration::from_secs(10),
                |_, _| {
                    ready_tx.send(()).expect("publish held owner namespace");
                    release_rx.recv().expect("release owner namespace");
                    Ok(())
                },
            )
            .expect("hold owner namespace")
        });
        ready_rx.recv().expect("owner namespace is held");

        let error = reconcile_subagent_ownership_until(
            root.path(),
            id,
            Instant::now() + Duration::from_secs(2),
        )
        .expect_err("owner cleanup deadline must fail closed");
        assert!(error.contains("owner lease cleanup"), "{error}");
        let terminal =
            get_subagent_record_unreconciled(root.path(), id).expect("terminal record first");
        assert_eq!(terminal.status, "failed");
        assert!(terminal.result.as_ref().is_some_and(|result| {
            result["ownership_reconciliation"]["reconciliation_id"]
                .as_str()
                .is_some()
        }));
        assert!(owner_lease_path(root.path(), &lease_id)
            .expect("visible owner lease")
            .exists());
        assert!(owner_lease_anchor_path(root.path(), &lease_id)
            .expect("owner anchor")
            .exists());
        assert!(store
            .load_result("parent")
            .expect("audit session")
            .expect("parent session")
            .events
            .is_empty());

        release_tx.send(()).expect("release owner namespace");
        holder.join().expect("owner namespace holder");
        let visible = owner_lease_path(root.path(), &lease_id).expect("visible owner lease");
        let anchor = owner_lease_anchor_path(root.path(), &lease_id).expect("owner anchor");
        let visible_directory = crate::daemons::state::StableDirectory::open(
            visible.parent().expect("visible owner parent"),
        )
        .expect("visible owner directory");
        let visible_quarantine = visible_directory
            .deterministic_artifact_path(
                &visible,
                ".nib-subagent-owner-visible-delete-",
                ".quarantine",
            )
            .expect("visible owner quarantine");
        let (quarantined_tx, quarantined_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let cleanup_root = root.path().to_path_buf();
        let cleanup_lease_id = lease_id.clone();
        let cleanup_visible = visible.clone();
        let cleanup_quarantine = visible_quarantine.clone();
        let cleanup = std::thread::spawn(move || {
            let mut paused = false;
            remove_persisted_owner_lease_until_with_guard(
                &cleanup_root,
                execution_generation,
                &cleanup_lease_id,
                Some(Instant::now() + Duration::from_millis(150)),
                || {
                    if !paused && cleanup_quarantine.exists() && !cleanup_visible.exists() {
                        paused = true;
                        quarantined_tx
                            .send(())
                            .expect("publish terminal owner quarantine pause");
                        resume_rx.recv().expect("resume terminal owner cleanup");
                    }
                    Ok(())
                },
            )
        });
        quarantined_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("terminal cleanup reached retained quarantine");
        std::thread::sleep(Duration::from_millis(200));
        resume_tx.send(()).expect("resume expired terminal cleanup");
        cleanup
            .join()
            .expect("terminal cleanup worker")
            .expect_err("terminal cleanup expiry retains quarantine");
        assert!(!visible.exists());
        assert!(visible_quarantine.exists());
        assert!(anchor.exists());
        assert!(store
            .load_result("parent")
            .expect("audit session")
            .expect("parent session")
            .events
            .is_empty());

        let (audit_lock_tx, audit_lock_rx) = std::sync::mpsc::sync_channel(1);
        let (release_audit_tx, release_audit_rx) = std::sync::mpsc::sync_channel(1);
        let audit_root = root.path().to_path_buf();
        let audit_holder = std::thread::spawn(move || {
            let audit_store =
                crate::session::SessionStore::for_project(&audit_root).expect("audit store");
            audit_store
                .with_session_lock_for_testing("parent", || {
                    audit_lock_tx.send(()).expect("publish held audit lock");
                    release_audit_rx.recv().expect("release audit lock");
                    Ok(())
                })
                .expect("hold audit session lock")
        });
        audit_lock_rx.recv().expect("audit session lock is held");
        let retry_root = root.path().to_path_buf();
        let retry = std::thread::spawn(move || {
            reconcile_subagent_ownership_until(
                &retry_root,
                id,
                Instant::now() + Duration::from_secs(2),
            )
        });
        let cleanup_observation_deadline = Instant::now() + Duration::from_secs(1);
        while (visible.exists() || visible_quarantine.exists() || anchor.exists())
            && Instant::now() < cleanup_observation_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!visible.exists(), "terminal retry removed visible owner");
        assert!(
            !visible_quarantine.exists(),
            "terminal retry removed retained quarantine before audit publication"
        );
        assert!(!anchor.exists(), "terminal retry removed owner anchor");
        release_audit_tx
            .send(())
            .expect("release audit session lock");
        audit_holder.join().expect("audit session holder");
        let reconciled = retry
            .join()
            .expect("terminal reconciliation worker")
            .expect("terminal retry finalizes ownership");
        assert_eq!(reconciled.status, "failed");
        assert!(!visible.exists());
        assert!(!anchor.exists());

        reconcile_subagent_ownership_until(
            root.path(),
            id,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("idempotent terminal retry");
        let session = store
            .load_result("parent")
            .expect("audit session")
            .expect("parent session");
        let events = session
            .events
            .iter()
            .filter(|event| event.kind == "subagent_execution_reconciled")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].details["reconciliation_id"],
            terminal.result.expect("terminal result")["ownership_reconciliation"]
                ["reconciliation_id"]
        );
    }

    #[test]
    fn direct_terminal_cancellation_waits_for_live_owner_cleanup_before_resolution() {
        let root = tempfile::tempdir().expect("root");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("live owner lease");
        let lease_id = owner_lease.lease_id.clone();
        let execution_generation = owner_lease.execution_generation;
        let id = "sub-direct-terminal-cancel";
        let proof = install_completed_process_scope_with_outcome(
            root.path(),
            id,
            execution_generation,
            "cancelled",
        );
        let mut record =
            terminal_record_fixture(root.path(), id, execution_generation, &lease_id, &proof);
        record.status = "cancelled".to_string();
        record.error = Some("cancelled by manage_subagents".to_string());
        write_subagent_record(root.path(), &record)
            .expect("supervisor persists direct terminal cancellation before monitor cleanup");

        let live_resolution = resolve_subagent_cancellation_until(
            root.path(),
            id,
            Instant::now() + Duration::from_millis(250),
        );
        assert!(
            matches!(
                live_resolution,
                CancelSubagentResolution::Unresolved {
                    observed_status: None,
                    ..
                }
            ),
            "a live exact owner must keep direct cancellation unresolved: {live_resolution:?}"
        );
        assert!(owner_lease_path(root.path(), &lease_id)
            .expect("visible owner lease")
            .exists());
        assert!(owner_lease_anchor_path(root.path(), &lease_id)
            .expect("owner lease anchor")
            .exists());

        owner_lease
            .release_for_reconciliation()
            .expect("monitor releases exact ownership after supervisor exit");
        let resolved = resolve_subagent_cancellation_until(
            root.path(),
            id,
            Instant::now() + Duration::from_secs(2),
        );
        let CancelSubagentResolution::Cancelled { record } = resolved else {
            panic!("released direct cancellation must reconcile successfully: {resolved:?}");
        };
        assert_eq!(record.status, "cancelled");
        assert_eq!(
            record
                .result
                .as_ref()
                .and_then(|result| result.get("cleanup_unverified"))
                .and_then(Value::as_bool),
            Some(false)
        );
        assert!(!owner_lease_path(root.path(), &lease_id)
            .expect("visible owner lease")
            .exists());
        assert!(!owner_lease_anchor_path(root.path(), &lease_id)
            .expect("owner lease anchor")
            .exists());
    }

    #[test]
    fn legacy_precommit_audit_is_adopted_and_upgraded_without_duplication() {
        let root = tempfile::tempdir().expect("root");
        let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
        store.create_session_with_id("parent");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let execution_generation = owner_lease.execution_generation;
        let lease_id = owner_lease.lease_id.clone();
        let id = "sub-legacy-precommit-audit";
        let mut record = record_fixture(root.path(), id, "running");
        attach_execution_ownership(&mut record, &owner_lease);
        let proof = install_completed_process_scope(root.path(), id, execution_generation);
        write_subagent_record(root.path(), &record).expect("running record");
        drop(owner_lease);
        let reconciled_at = Utc::now() - chrono::Duration::seconds(1);
        store
            .record_event(
                "parent",
                "subagent_execution_reconciled",
                json!({
                    "outcome": "supervisor_result_lost_after_verified_cleanup",
                    "subagent_id": id,
                    "execution_generation": execution_generation,
                    "owner_lease": lease_id,
                    "manager_status": Value::Null,
                    "terminal_status": "failed",
                    "reconciled_at": reconciled_at,
                    "cleanup_verified": true,
                    "cleanup_scope": "foreground_descendant_process_tree",
                    "cleanup_proof": proof,
                }),
            )
            .expect("legacy audit-first event");

        let terminal = reconcile_subagent_ownership_until(
            root.path(),
            id,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("adopt legacy audit");
        assert_eq!(
            terminal.result.as_ref().expect("result")["ownership_reconciliation"]["reconciled_at"],
            json!(reconciled_at)
        );
        let session = store
            .load_result("parent")
            .expect("session")
            .expect("parent");
        let events = session
            .events
            .iter()
            .filter(|event| event.kind == "subagent_execution_reconciled")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert!(events[0].details["reconciliation_id"].is_string());
    }

    #[test]
    fn no_parent_reconciliation_creates_the_pinned_child_audit_session() {
        let root = tempfile::tempdir().expect("root");
        crate::session::SessionStore::for_project(root.path()).expect("session namespace");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let execution_generation = owner_lease.execution_generation;
        let id = "sub-no-parent-audit";
        let mut record = record_fixture(root.path(), id, "running");
        record.parent_session_id = None;
        attach_execution_ownership(&mut record, &owner_lease);
        install_completed_process_scope(root.path(), id, execution_generation);
        write_subagent_record(root.path(), &record).expect("running record");
        drop(owner_lease);

        reconcile_subagent_ownership_until(
            root.path(),
            id,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("no-parent reconciliation");
        let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
        let child = store
            .load_result(&record.child_session_id)
            .expect("child audit session")
            .expect("created child audit session");
        assert_eq!(
            child
                .events
                .iter()
                .filter(|event| event.kind == "subagent_execution_reconciled")
                .count(),
            1
        );
    }

    #[test]
    fn merge_wrapped_record_retains_its_pinned_audit_target() {
        let root = tempfile::tempdir().expect("root");
        let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
        let target = SubagentAuditTarget {
            sessions_dir: store.sessions_dir().canonicalize().expect("sessions dir"),
            directory_identity: store
                .persistent_directory_identity()
                .expect("session identity"),
        };
        let mut record = record_fixture(root.path(), "sub-merge-target", MERGE_PENDING_STATUS);
        record.result = Some(json!({
            "subagent_result": {
                "_ownership_audit_target": target.clone(),
            }
        }));

        assert_eq!(
            subagent_audit_target(&record)
                .expect("valid target")
                .expect("nested target"),
            target
        );
    }

    #[test]
    fn stale_generation_guard_cannot_overwrite_reused_running_record() {
        let root = tempfile::tempdir().expect("root");
        let old_lease = SubagentOwnerLease::create(root.path()).expect("old owner lease");
        let old_lease_id = old_lease.lease_id.clone();
        let mut record = record_fixture(root.path(), "sub-generation-fence", "running");
        attach_execution_ownership(&mut record, &old_lease);
        write_subagent_record(root.path(), &record).expect("old generation record");
        let guard = SubagentRunGuard::new(root.path().to_path_buf(), record.id.clone(), old_lease);

        let new_lease = SubagentOwnerLease::create(root.path()).expect("new owner lease");
        attach_execution_ownership(&mut record, &new_lease);
        update_subagent_record(root.path(), &record.id, |current| {
            *current = record.clone();
            Ok(())
        })
        .expect("reused generation record");
        drop(guard);

        let persisted = get_subagent_record_internal(root.path(), &record.id)
            .expect("new live generation remains authoritative");
        assert_eq!(persisted.status, "running");
        assert_eq!(
            persisted.execution_generation,
            Some(new_lease.execution_generation)
        );
        assert_eq!(
            persisted.owner_lease.as_deref(),
            Some(new_lease.lease_id.as_str())
        );
        assert!(!owner_lease_path(root.path(), &old_lease_id)
            .expect("old visible lease")
            .exists());
        update_subagent_record(root.path(), &record.id, |record| {
            record.status = "failed".to_string();
            record.error = Some("test cleanup".to_string());
            record.updated_at = Utc::now();
            Ok(())
        })
        .expect("terminalize new generation fixture");
        new_lease.remove().expect("clean new owner lease");
    }

    #[cfg(unix)]
    #[test]
    fn live_owner_is_not_reconciled_through_replaced_lease_paths() {
        let root = tempfile::tempdir().expect("root");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let lease_id = owner_lease.lease_id.clone();
        let mut record = record_fixture(root.path(), "sub-live-owner-replacement", "running");
        attach_execution_ownership(&mut record, &owner_lease);
        write_subagent_record(root.path(), &record).expect("running record");

        let visible_directory = owner_lease_directory(root.path());
        let displaced_directory = root.path().join(".nib/subagent-owner-leases.displaced");
        std::fs::rename(&visible_directory, &displaced_directory)
            .expect("displace live lease directory");
        std::fs::create_dir(&visible_directory).expect("replacement lease directory");
        std::fs::copy(
            displaced_directory.join(format!("{lease_id}{OWNER_LEASE_SUFFIX}")),
            visible_directory.join(format!("{lease_id}{OWNER_LEASE_SUFFIX}")),
        )
        .expect("copy unlocked replacement lease");
        let error = get_subagent_record(root.path(), &record.id)
            .expect_err("replacement directory must not form a second lease domain");
        assert!(error.contains("different identities"), "{error}");
        assert_eq!(
            get_subagent_record_unreconciled(root.path(), &record.id)
                .expect("record after directory replacement")
                .status,
            "running"
        );
        std::fs::remove_dir_all(&visible_directory).expect("remove replacement directory");
        std::fs::rename(&displaced_directory, &visible_directory).expect("restore lease directory");

        let visible = owner_lease_path(root.path(), &lease_id).expect("visible lease");
        let displaced_file = visible.with_extension("lease.displaced");
        std::fs::rename(&visible, &displaced_file).expect("displace live lease file");
        std::fs::write(&visible, b"replacement").expect("replacement lease file");
        let error = get_subagent_record(root.path(), &record.id)
            .expect_err("replacement file must not form a second lease domain");
        assert!(error.contains("different identities"), "{error}");
        assert_eq!(
            get_subagent_record_unreconciled(root.path(), &record.id)
                .expect("record after file replacement")
                .status,
            "running"
        );
        std::fs::remove_file(&visible).expect("remove replacement lease file");
        std::fs::rename(&displaced_file, &visible).expect("restore lease file");
        update_subagent_record(root.path(), &record.id, |record| {
            record.status = "failed".to_string();
            record.updated_at = Utc::now();
            Ok(())
        })
        .expect("terminalize replacement fixture");
        owner_lease.remove().expect("clean owner lease");
    }

    #[test]
    fn anchor_only_owner_artifacts_are_reported_while_live_and_removed_when_stale() {
        let root = tempfile::tempdir().expect("root");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let lease_id = owner_lease.lease_id.clone();
        let visible_path = owner_lease.visible_path.clone();
        let anchor_path = owner_lease.anchor_path.clone();
        remove_visible_owner_lease(&owner_lease);

        let error = sweep_owner_lease_artifacts(root.path(), &std::collections::HashSet::new())
            .expect_err("a locked anchor-only owner must be reported");
        assert!(error.contains("live subagent owner anchor"), "{error}");
        assert!(anchor_path.exists());
        assert!(!visible_path.exists());

        drop(owner_lease);
        sweep_owner_lease_artifacts(root.path(), &std::collections::HashSet::new())
            .expect("stale anchor-only owner cleanup");
        assert!(!anchor_path.exists());
        assert!(!owner_lease_path(root.path(), &lease_id)
            .expect("visible owner path")
            .exists());
    }

    #[test]
    fn live_anchor_only_running_record_supports_status_list_and_cancel_reconciliation() {
        let root = tempfile::tempdir().expect("root");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let lease_id = owner_lease.lease_id.clone();
        let mut record = record_fixture(root.path(), "sub-live-anchor-only", "running");
        attach_execution_ownership(&mut record, &owner_lease);
        write_subagent_record(root.path(), &record).expect("running record");
        remove_visible_owner_lease(&owner_lease);

        let status = get_subagent_record(root.path(), &record.id).expect("status reconciliation");
        assert_eq!(status.status, "running");
        let listed = list_subagents(root.path()).expect("list reconciliation");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["status"], "running");
        let cancel_error = cancel_subagent(root.path(), &record.id)
            .expect_err("an untracked but live owner cannot be reported cancelled");
        assert!(
            cancel_error.contains("observed_status: running"),
            "{cancel_error}"
        );
        assert!(
            !cancel_error.contains("owner lease is unavailable"),
            "{cancel_error}"
        );
        assert!(
            owner_lease_anchor_path(root.path(), &lease_id)
                .expect("anchor path")
                .exists(),
            "the live anchor remains authoritative"
        );

        update_subagent_record(root.path(), &record.id, |current| {
            current.status = "failed".to_string();
            current.updated_at = Utc::now();
            Ok(())
        })
        .expect("terminalize fixture");
        owner_lease
            .remove()
            .expect("clean anchor-only owner through normal owner cleanup");
        assert!(
            !owner_lease_anchor_path(root.path(), &lease_id)
                .expect("anchor path")
                .exists(),
            "normal owner cleanup removes the anchor"
        );
    }

    #[test]
    fn dead_anchor_only_record_without_process_scope_requires_recovery() {
        let root = tempfile::tempdir().expect("root");
        crate::session::SessionStore::for_project(root.path())
            .expect("session store")
            .create_session_with_id("parent");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let lease_id = owner_lease.lease_id.clone();
        let mut record = record_fixture(root.path(), "sub-dead-anchor-only", "running");
        attach_execution_ownership(&mut record, &owner_lease);
        write_subagent_record(root.path(), &record).expect("running record");
        remove_visible_owner_lease(&owner_lease);
        drop(owner_lease);

        let cancel_error = cancel_subagent(root.path(), &record.id)
            .expect_err("cleanup cannot be inferred from owner loss");
        assert!(
            cancel_error.contains("observed_status: running"),
            "{cancel_error}"
        );
        assert!(cancel_error.contains("untracked"), "{cancel_error}");
        let reconciled =
            get_subagent_record_internal(root.path(), &record.id).expect("reconciled record");
        assert_eq!(reconciled.status, "running");
        let result = reconciled.result.as_ref().expect("recovery evidence");
        assert_eq!(result["outcome"], "recovery_required");
        assert_eq!(result["process_scope"]["status"], "missing");
        assert_eq!(result["cleanup_verified"], false);
        assert!(
            owner_lease_anchor_path(root.path(), &lease_id)
                .expect("anchor path")
                .exists(),
            "the unverified owner anchor remains available for explicit recovery"
        );
        assert!(owner_lease_directory(root.path()).is_dir());
    }

    #[test]
    fn legacy_running_record_and_orphan_cancellation_fail_closed() {
        #[cfg(windows)]
        let _timeout = SubagentCancellationTimeoutGuard::set(Duration::from_secs(10));
        let root = tempfile::tempdir().expect("root");
        let legacy = record_fixture(root.path(), "sub-legacy-owner", "running");
        write_subagent_record(root.path(), &legacy).expect("legacy running record");
        let error = get_subagent_record(root.path(), &legacy.id)
            .expect_err("legacy ownership must not be guessed");
        assert!(error.contains("legacy execution ownership"), "{error}");
        assert_eq!(
            get_subagent_record_unreconciled(root.path(), &legacy.id)
                .expect("legacy record remains durable")
                .status,
            "running"
        );

        let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
        store.create_session_with_id("parent");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("orphan lease");
        let mut orphan = record_fixture(root.path(), "sub-cancel-orphan", "running");
        attach_execution_ownership(&mut orphan, &owner_lease);
        write_subagent_record(root.path(), &orphan).expect("orphan running record");
        drop(owner_lease);
        match resolve_subagent_cancellation(root.path(), &orphan.id) {
            CancelSubagentResolution::Unresolved {
                manager_stopped,
                observed_status,
                error,
            } => {
                assert!(!manager_stopped);
                assert_eq!(observed_status.as_deref(), Some("running"));
                assert!(error.contains("untracked"), "{error}");
                let persisted = get_subagent_record_internal(root.path(), &orphan.id)
                    .expect("orphan remains recoverable");
                assert_eq!(persisted.status, "running");
                let result = persisted.result.expect("recovery evidence");
                assert_eq!(result["outcome"], "recovery_required");
                assert_eq!(result["cleanup_verified"], false);
            }
            resolution => panic!("orphan cancellation must fail closed, got {resolution:?}"),
        }
    }

    #[tokio::test]
    async fn contradictory_terminal_record_and_manager_states_fail_closed() {
        let root = tempfile::tempdir().expect("root");
        let completed_id = format!("sub-terminal-cancelled-{}", uuid::Uuid::new_v4());
        let completed = record_fixture(root.path(), &completed_id, "completed");
        write_subagent_record(root.path(), &completed).expect("completed record");
        crate::daemons::task::TASK_MANAGER
            .register_task(completed_id.clone(), "subagent")
            .expect("completed manager task");
        let pending = tokio::spawn(std::future::pending::<()>());
        crate::daemons::task::TASK_MANAGER
            .attach_abort_handle(&completed_id, pending.abort_handle())
            .expect("completed manager abort handle");
        crate::daemons::task::TASK_MANAGER
            .cancel(&completed_id)
            .expect("force contradictory cancelled manager state");

        match resolve_subagent_cancellation(root.path(), &completed_id) {
            CancelSubagentResolution::Unresolved { error, .. } => {
                assert!(error.contains("contradicts"), "{error}");
                assert!(error.contains("cancelled"), "{error}");
            }
            resolution => panic!("contradictory completion must fail closed: {resolution:?}"),
        }
        assert!(pending
            .await
            .expect_err("cancelled manager task")
            .is_cancelled());

        let cancelled_id = format!("sub-cancelled-completed-{}", uuid::Uuid::new_v4());
        let cancelled = record_fixture(root.path(), &cancelled_id, "cancelled");
        write_subagent_record(root.path(), &cancelled).expect("cancelled record");
        crate::daemons::task::TASK_MANAGER
            .register_task(cancelled_id.clone(), "subagent")
            .expect("cancelled manager task");
        crate::daemons::task::TASK_MANAGER.complete(&cancelled_id, None);

        match resolve_subagent_cancellation(root.path(), &cancelled_id) {
            CancelSubagentResolution::Unresolved { error, .. } => {
                assert!(error.contains("contradicts"), "{error}");
                assert!(error.contains("completed"), "{error}");
            }
            resolution => panic!("contradictory cancellation must fail closed: {resolution:?}"),
        }
    }

    #[tokio::test]
    async fn async_cancellation_reconciliation_persists_an_aborted_run_on_one_worker() {
        let root = tempfile::tempdir().expect("root");
        let _timeout = SubagentCancellationTimeoutGuard::set(Duration::from_secs(2));
        let id = format!("sub-async-cancel-{}", uuid::Uuid::new_v4());
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let mut record = record_fixture(root.path(), &id, "running");
        attach_execution_ownership(&mut record, &owner_lease);
        write_subagent_record(root.path(), &record).expect("running record");
        crate::daemons::task::TASK_MANAGER
            .register_task(id.clone(), "subagent")
            .expect("background task");

        let guard = SubagentRunGuard::new(root.path().to_path_buf(), id.clone(), owner_lease);
        let running = tokio::spawn(async move {
            let guard = guard;
            std::future::pending::<()>().await;
            drop(guard);
        });
        crate::daemons::task::TASK_MANAGER
            .attach_abort_handle(&id, running.abort_handle())
            .expect("abort handle");

        match resolve_subagent_cancellation_async(root.path(), &id).await {
            CancelSubagentResolution::Cancelled { record } => {
                assert_eq!(record.status, "cancelled");
                assert_eq!(
                    record.error.as_deref(),
                    Some("cancelled by manage_subagents")
                );
            }
            resolution => panic!("async cancellation must persist stopped truth: {resolution:?}"),
        }
        assert!(running
            .await
            .expect_err("subagent task is aborted")
            .is_cancelled());
        assert_eq!(
            get_subagent_record_unreconciled(root.path(), &id)
                .expect("durable cancelled record")
                .status,
            "cancelled"
        );
    }

    #[tokio::test]
    async fn async_cancellation_worker_start_delay_cannot_renew_the_api_deadline() {
        let root = tempfile::tempdir().expect("root");
        let id = format!("sub-delayed-cancel-{}", uuid::Uuid::new_v4());
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let mut record = record_fixture(root.path(), &id, "running");
        attach_execution_ownership(&mut record, &owner_lease);
        write_subagent_record(root.path(), &record).expect("running record");
        drop(owner_lease);
        let namespace_before = subagent_namespace_snapshot(root.path());

        let (worker_ready_tx, worker_ready_rx) = tokio::sync::oneshot::channel();
        let (worker_release_tx, worker_release_rx) = std::sync::mpsc::channel();
        let timeout = Duration::from_millis(50);
        let api_started = Instant::now();
        let cancellation = resolve_subagent_cancellation_async_with_start_hook(
            root.path(),
            &id,
            timeout,
            move || {
                let _ = worker_ready_tx.send(());
                worker_release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release delayed reconciliation worker");
            },
        );
        let resolver = tokio::spawn(cancellation);
        tokio::time::timeout(Duration::from_secs(1), worker_ready_rx)
            .await
            .expect("worker start gate timeout")
            .expect("worker reached start gate");
        tokio::time::sleep(timeout * 2).await;

        let released = Instant::now();
        worker_release_tx
            .send(())
            .expect("resume delayed reconciliation worker");
        let resolution = tokio::time::timeout(Duration::from_secs(1), resolver)
            .await
            .expect("expired worker must join within its original budget")
            .expect("resolver task");
        match resolution {
            CancelSubagentResolution::Unresolved { error, .. } => {
                assert!(error.contains("deadline elapsed"), "{error}");
            }
            resolution => panic!("expired API-entry deadline must fail closed: {resolution:?}"),
        }
        assert!(
            api_started.elapsed() >= timeout,
            "fixture did not hold the worker beyond the API-entry deadline"
        );
        assert!(
            released.elapsed() < Duration::from_secs(1),
            "worker gained a new reconciliation budget after its start gate"
        );
        assert_eq!(subagent_namespace_snapshot(root.path()), namespace_before);
        let persisted =
            get_subagent_record_unreconciled(root.path(), &id).expect("unchanged record");
        assert_eq!(persisted.status, "running");
        assert!(persisted.result.is_none());
        tokio::time::sleep(timeout * 2).await;
        assert_eq!(subagent_namespace_snapshot(root.path()), namespace_before);
    }

    #[tokio::test]
    async fn async_cancellation_worker_abort_joins_a_held_lock_reconciler() {
        let root = tempfile::tempdir().expect("root");
        let id = format!("sub-joined-cancel-{}", uuid::Uuid::new_v4());
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let mut record = record_fixture(root.path(), &id, "running");
        attach_execution_ownership(&mut record, &owner_lease);
        write_subagent_record(root.path(), &record).expect("running record");
        drop(owner_lease);
        let held = hold_subagent_record_lock_for_test(root.path(), &id).expect("held record lock");

        let project_root = root.path().to_path_buf();
        let cancellation_id = id.clone();
        let resolver = tokio::spawn(async move {
            resolve_subagent_cancellation_async(&project_root, &cancellation_id).await
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        let started = Instant::now();
        resolver.abort();
        assert!(resolver
            .await
            .expect_err("resolver task is aborted")
            .is_cancelled());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "worker join exceeded its absolute reconciliation deadline"
        );

        drop(held);
        tokio::time::sleep(SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT * 2).await;
        let persisted =
            get_subagent_record_unreconciled(root.path(), &id).expect("unreconciled record");
        assert_eq!(persisted.status, "running");
        assert!(
            persisted.result.is_none(),
            "an aborted reconciliation worker mutated the record after its owner returned"
        );
    }

    #[tokio::test]
    async fn async_cancellation_worker_panic_is_fail_closed() {
        let id = format!("sub-panicked-cancel-{}", uuid::Uuid::new_v4());
        match run_subagent_cancellation_worker(id, || panic!("worker panic fixture")).await {
            CancelSubagentResolution::Unresolved {
                manager_stopped,
                observed_status,
                error,
            } => {
                assert!(!manager_stopped);
                assert!(observed_status.is_none());
                assert!(error.contains("worker panicked before shutdown"), "{error}");
            }
            resolution => panic!("worker panic must fail closed: {resolution:?}"),
        }
    }

    #[test]
    fn subagent_record_directory_replacement_child() {
        let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
            return;
        };
        let project_root = PathBuf::from(project_root);
        let record = record_fixture(&project_root, "replacement-record", "completed");
        let error = write_subagent_record(&project_root, &record)
            .expect_err("detached records directory must reject a pure record commit");
        assert!(error.contains("identity changed"), "{error}");
    }

    #[test]
    fn subagent_record_destination_replacement_child() {
        let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
            return;
        };
        let project_root = PathBuf::from(project_root);
        let record = record_fixture(&project_root, "replacement-file", "completed");
        let error = write_subagent_record(&project_root, &record)
            .expect_err("a replacement record must defeat Missing publication");
        assert!(error.contains("appeared"), "{error}");
    }

    #[test]
    fn subagent_record_revision_replacement_child() {
        let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
            return;
        };
        let project_root = PathBuf::from(project_root);
        let records = ensure_records_directory(&project_root).expect("records");
        let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
        let path = record_path(&project_root, "replacement-revision").expect("record path");
        let mut opened = read_opened_subagent_record_in(&directory, &path).expect("revision");
        opened.record.status = "completed".to_string();
        opened.record.updated_at = Utc::now();
        let error =
            persist_subagent_record_revision(&project_root, &opened.record, &mut opened.file)
                .expect_err("replacement must defeat Present publication");
        assert!(
            error.contains("identity") || error.contains("changed"),
            "{error}"
        );
    }

    #[test]
    fn bounded_subagent_record_deadline_child() {
        let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
            return;
        };
        let project_root = PathBuf::from(project_root);
        let error = update_subagent_record_until(
            &project_root,
            "expired-reconciliation-write",
            Some(Instant::now() + Duration::from_millis(100)),
            |record| {
                record.status = "failed".to_string();
                record.error = Some("must not publish".to_string());
                record.updated_at = Utc::now();
                Ok(())
            },
        )
        .expect_err("expired bounded record publication must fail");
        assert!(
            error.contains("delegation state lock deadline elapsed"),
            "{error}"
        );
    }

    #[test]
    fn bounded_subagent_finalization_deadline_child() {
        let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
            return;
        };
        let project_root = PathBuf::from(project_root);
        inject_recoverable_revision_publication_failures("expired-finalization-write", 1);
        let error = update_subagent_record_until(
            &project_root,
            "expired-finalization-write",
            Some(Instant::now() + Duration::from_secs(3)),
            |record| {
                record.status = "failed".to_string();
                record.error = Some("published before finalization pause".to_string());
                record.updated_at = Utc::now();
                Ok(())
            },
        )
        .expect_err("expired exact-receipt finalization must fail closed");
        assert!(
            error.contains("delegation state lock deadline elapsed"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn subagent_record_write_never_redirects_to_replaced_records_directory() {
        let root = tempfile::tempdir().expect("root");
        let record = record_fixture(root.path(), "replacement-record", "running");
        write_subagent_record(root.path(), &record).expect("initial record");
        let path = record_path(root.path(), &record.id).expect("record path");
        let initial = std::fs::read(&path).expect("initial record bytes");
        let ready = root.path().join("record-write.ready");
        let resume = root.path().join("record-write.resume");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::subagent_record_directory_replacement_child",
                "--nocapture",
            ])
            .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
            .env("NIB_TEST_SUBAGENT_WRITE_ID", &record.id)
            .env("NIB_TEST_SUBAGENT_WRITE_READY", &ready)
            .env("NIB_TEST_SUBAGENT_WRITE_RESUME", &resume)
            .spawn()
            .expect("spawn record writer child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect record writer child") {
                panic!("record writer exited before pause: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "record writer did not pause before commit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let records = records_dir(root.path());
        let displaced = root.path().join(".nib/subagents.displaced-write");
        std::fs::rename(&records, &displaced).expect("detach records directory");
        std::fs::create_dir(&records).expect("replacement records directory");
        std::fs::write(records.join("replacement-record.json"), &initial)
            .expect("replacement record sentinel");
        std::fs::write(&resume, b"resume").expect("resume record writer");
        let status = child.wait().expect("wait for record writer child");
        assert!(status.success(), "record writer child failed: {status}");
        assert_eq!(
            std::fs::read(displaced.join("replacement-record.json"))
                .expect("original record after aborted write"),
            initial
        );
        assert_eq!(
            std::fs::read(records.join("replacement-record.json"))
                .expect("replacement record sentinel"),
            initial
        );
    }

    #[test]
    fn missing_record_creation_preserves_a_destination_replacement() {
        let root = tempfile::tempdir().expect("root");
        let ready = root.path().join("record-create.ready");
        let resume = root.path().join("record-create.resume");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::subagent_record_destination_replacement_child",
                "--nocapture",
            ])
            .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
            .env("NIB_TEST_SUBAGENT_WRITE_ID", "replacement-file")
            .env("NIB_TEST_SUBAGENT_WRITE_READY", &ready)
            .env("NIB_TEST_SUBAGENT_WRITE_RESUME", &resume)
            .spawn()
            .expect("spawn record creator child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect record creator child") {
                panic!("record creator exited before pause: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "record creator did not pause before publication"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut replacement = record_fixture(root.path(), "replacement-file", "failed");
        replacement.error = Some("replacement sentinel".to_string());
        let path = record_path(root.path(), &replacement.id).expect("replacement path");
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");
        std::fs::write(&path, &replacement_bytes).expect("publish replacement record");
        std::fs::write(&resume, b"resume").expect("resume record creator");
        let status = child.wait().expect("wait for record creator child");
        assert!(status.success(), "record creator child failed: {status}");
        assert_eq!(
            std::fs::read(&path).expect("replacement record remains"),
            replacement_bytes
        );
    }

    #[test]
    fn expected_record_revision_rechecks_identity_after_the_precommit_pause() {
        let root = tempfile::tempdir().expect("root");
        let initial = record_fixture(root.path(), "replacement-revision", "running");
        write_subagent_record(root.path(), &initial).expect("initial record");
        let ready = root.path().join("record-revision.ready");
        let resume = root.path().join("record-revision.resume");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::subagent_record_revision_replacement_child",
                "--nocapture",
            ])
            .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
            .env("NIB_TEST_SUBAGENT_WRITE_ID", "replacement-revision")
            .env("NIB_TEST_SUBAGENT_WRITE_READY", &ready)
            .env("NIB_TEST_SUBAGENT_WRITE_RESUME", &resume)
            .spawn()
            .expect("spawn record revision child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect record revision child") {
                panic!("record revision child exited before pause: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "record revision child did not pause before publication"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let path = record_path(root.path(), &initial.id).expect("record path");
        let displaced = path.with_extension("old-revision");
        std::fs::rename(&path, &displaced).expect("displace expected revision");
        let mut replacement = initial.clone();
        replacement.status = "failed".to_string();
        replacement.error = Some("replacement sentinel".to_string());
        replacement.updated_at = Utc::now();
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");
        std::fs::write(&path, &replacement_bytes).expect("publish replacement revision");
        std::fs::write(&resume, b"resume").expect("resume record revision child");
        let status = child.wait().expect("wait for record revision child");
        assert!(status.success(), "record revision child failed: {status}");
        assert_eq!(
            std::fs::read(&path).expect("replacement revision remains"),
            replacement_bytes
        );
        assert!(displaced.exists(), "expected revision remains displaced");
    }

    #[test]
    fn bounded_reconciliation_record_does_not_publish_after_precommit_expiry() {
        let root = tempfile::tempdir().expect("root");
        let initial = record_fixture(root.path(), "expired-reconciliation-write", "running");
        write_subagent_record(root.path(), &initial).expect("initial record");
        let path = record_path(root.path(), &initial.id).expect("record path");
        let original = std::fs::read(&path).expect("initial record bytes");
        let ready = root.path().join("record-deadline.ready");
        let resume = root.path().join("record-deadline.resume");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::bounded_subagent_record_deadline_child",
                "--nocapture",
            ])
            .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
            .env("NIB_TEST_SUBAGENT_WRITE_ID", &initial.id)
            .env("NIB_TEST_SUBAGENT_WRITE_READY", &ready)
            .env("NIB_TEST_SUBAGENT_WRITE_RESUME", &resume)
            .spawn()
            .expect("spawn bounded record writer child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect record writer child") {
                panic!("bounded record writer exited before pause: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "bounded record writer did not pause before publication"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let paused_namespace = subagent_namespace_snapshot(&records_dir(root.path()));
        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(&resume, b"resume").expect("resume bounded record writer");

        let status = child.wait().expect("wait for bounded record writer child");
        assert!(
            status.success(),
            "bounded record writer child failed: {status}"
        );
        assert_eq!(
            std::fs::read(&path).expect("record remains readable"),
            original,
            "bounded reconciliation published after its absolute deadline"
        );
        assert_eq!(
            subagent_namespace_snapshot(&records_dir(root.path())),
            paused_namespace,
            "expired record save mutated its transaction namespace"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            subagent_namespace_snapshot(&records_dir(root.path())),
            paused_namespace,
            "expired record save mutated its transaction namespace later"
        );
    }

    #[test]
    fn exact_receipt_finalization_preserves_its_namespace_after_expiry() {
        let root = tempfile::tempdir().expect("root");
        let initial = record_fixture(root.path(), "expired-finalization-write", "running");
        write_subagent_record(root.path(), &initial).expect("initial record");
        let records = ensure_records_directory(root.path()).expect("records directory");
        let directory = crate::daemons::state::StableDirectory::open(&records)
            .expect("stable records directory");
        let path = record_path(root.path(), &initial.id).expect("record path");
        let temporary = directory
            .deterministic_artifact_path(&path, ".nib-subagent-", ".tmp")
            .expect("temporary artifact path");
        let ready = root.path().join("record-finalization.ready");
        let resume = root.path().join("record-finalization.resume");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::bounded_subagent_finalization_deadline_child",
                "--nocapture",
            ])
            .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
            .env("NIB_TEST_SUBAGENT_FINALIZE_ID", &initial.id)
            .env("NIB_TEST_SUBAGENT_FINALIZE_READY", &ready)
            .env("NIB_TEST_SUBAGENT_FINALIZE_RESUME", &resume)
            .spawn()
            .expect("spawn exact-receipt finalizer child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect finalizer child") {
                panic!("exact-receipt finalizer exited before pause: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "exact-receipt finalizer did not pause"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(
            &temporary,
            b"preserve post-publication transaction artifact",
        )
        .expect("temporary finalization fixture");
        let paused_namespace = subagent_namespace_snapshot(&records);
        std::thread::sleep(Duration::from_millis(3_200));
        std::fs::write(&resume, b"resume").expect("resume exact-receipt finalizer");

        let status = child.wait().expect("wait for exact-receipt finalizer");
        assert!(
            status.success(),
            "exact-receipt finalizer child failed: {status}"
        );
        assert_eq!(
            subagent_namespace_snapshot(&records),
            paused_namespace,
            "expired exact-receipt finalization mutated its namespace"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            subagent_namespace_snapshot(&records),
            paused_namespace,
            "expired exact-receipt finalization mutated its namespace later"
        );
        assert!(
            temporary.exists(),
            "expired finalizer removed its transaction artifact"
        );
    }

    #[test]
    fn post_publication_refresh_rejects_a_substituted_record_generation() {
        let root = tempfile::tempdir().expect("root");
        let initial = record_fixture(root.path(), "post-publish-substitution", "running");
        write_subagent_record(root.path(), &initial).expect("initial record");
        let records = ensure_records_directory(root.path()).expect("records");
        let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
        let path = record_path(root.path(), &initial.id).expect("record path");
        let mut opened = read_opened_subagent_record_in(&directory, &path).expect("opened record");
        let mut committed = initial.clone();
        committed.status = "completed".to_string();
        committed.updated_at = Utc::now();
        let replacement = committed.clone();
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");
        let displaced = path.with_extension("published-displaced");

        let error = persist_subagent_record_revision_with_refresh_hook(
            root.path(),
            &committed,
            &mut opened.file,
            || {
                std::fs::rename(&path, &displaced).map_err(|error| error.to_string())?;
                std::fs::write(&path, &replacement_bytes).map_err(|error| error.to_string())
            },
        )
        .expect_err("a substituted post-publication generation must not be adopted");
        assert!(error.contains("identity-distinct"), "{error}");
        assert_eq!(
            std::fs::read(&path).expect("replacement remains visible"),
            replacement_bytes
        );
        assert!(displaced.exists(), "committed generation remains displaced");

        committed.status = "verification_failed".to_string();
        let error = persist_subagent_record_revision(root.path(), &committed, &mut opened.file)
            .expect_err("the prior handle must not have adopted the replacement");
        assert!(
            error.contains("identity") || error.contains("changed"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&path).expect("replacement remains after stale retry"),
            replacement_bytes
        );
    }

    #[test]
    fn recoverable_revision_publication_error_finalizes_and_adopts_exact_receipt() {
        let root = tempfile::tempdir().expect("root");
        let initial = record_fixture(root.path(), "recoverable-revision-receipt", "running");
        write_subagent_record(root.path(), &initial).expect("initial record");
        let records = ensure_records_directory(root.path()).expect("records");
        let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
        let path = record_path(root.path(), &initial.id).expect("record path");
        let mut opened = read_opened_subagent_record_in(&directory, &path).expect("opened record");
        let mut revision = opened.record.clone();
        revision.status = "completed".to_string();
        revision.result = Some(json!({"publication": "recovered"}));
        revision.updated_at = Utc::now();
        inject_recoverable_revision_publication_failures(&revision.id, 1);

        persist_subagent_record_revision(root.path(), &revision, &mut opened.file)
            .expect("exact publication receipt must support finalization and adoption");

        let authoritative =
            read_opened_subagent_record_in(&directory, &path).expect("authoritative revision");
        assert_eq!(
            serde_json::to_value(&authoritative.record).expect("authoritative JSON"),
            serde_json::to_value(&revision).expect("expected JSON")
        );
        assert!(
            crate::daemons::state::same_open_file_identity(&opened.file, &authoritative.file)
                .expect("compare adopted receipt with authoritative revision")
        );
        directory
            .verify_file_identity(&path, &opened.file)
            .expect("adopted revision receipt remains authoritative");
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn initial_refresh_rejects_an_identity_distinct_json_equivalent_replacement() {
        let root = tempfile::tempdir().expect("root");
        let attempted = record_fixture(root.path(), "initial-equivalent-refresh", "running");
        let path = record_path(root.path(), &attempted.id).expect("record path");
        let displaced = path.with_extension("initial-publication-displaced");
        let attempted_bytes = serde_json::to_vec_pretty(&attempted).expect("attempted JSON");

        let error = match write_subagent_record_with_refresh_hook(root.path(), &attempted, || {
            std::fs::rename(&path, &displaced).map_err(|error| error.to_string())?;
            std::fs::write(&path, &attempted_bytes).map_err(|error| error.to_string())
        }) {
            Ok(_) => panic!("JSON equality cannot authorize adopting a distinct file generation"),
            Err(error) => error,
        };

        assert!(error.message.contains("identity-distinct"), "{error:?}");
        assert!(
            error
                .receipt
                .as_ref()
                .is_some_and(|receipt| receipt.exact_identity),
            "{error:?}"
        );
        assert_eq!(
            std::fs::read(&path).expect("equivalent replacement"),
            attempted_bytes
        );
        assert!(displaced.exists(), "original publication remains displaced");
    }

    #[test]
    fn post_publication_failure_receipt_preserves_a_substituted_record() {
        let root = tempfile::tempdir().expect("root");
        let attempted = record_fixture(root.path(), "post-publish-failure", "running");
        let path = record_path(root.path(), &attempted.id).expect("record path");
        let displaced = path.with_extension("failed-publication-displaced");
        let attempted_bytes = serde_json::to_vec_pretty(&attempted).expect("attempted JSON");

        let error = match write_subagent_record_with_refresh_hook(root.path(), &attempted, || {
            std::fs::rename(&path, &displaced).map_err(|error| error.to_string())?;
            std::fs::write(&path, &attempted_bytes).map_err(|error| error.to_string())?;
            Err("injected post-publication validation failure".to_string())
        }) {
            Ok(_) => panic!("post-publication failure must retain its publication receipt"),
            Err(error) => error,
        };
        assert!(error.receipt.is_some(), "{error:?}");
        let cleanup = cleanup_record_after_publication_failure(root.path(), &attempted, &error)
            .expect_err("the original receipt cannot authorize deleting the replacement");
        if error
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.exact_identity)
        {
            assert!(cleanup.contains("publication identity"), "{cleanup}");
        } else {
            assert!(cleanup.contains("identity is unavailable"), "{cleanup}");
        }
        assert_eq!(
            std::fs::read(&path).expect("substituted record remains"),
            attempted_bytes
        );
        assert!(displaced.exists(), "original publication remains displaced");
    }

    #[test]
    fn precommit_cleanup_preserves_a_record_replaced_before_quarantine() {
        let root = tempfile::tempdir().expect("root");
        let attempted = record_fixture(root.path(), "precommit-replacement", "running");
        let _publication =
            write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
                .expect("attempted record");
        let path = record_path(root.path(), &attempted.id).expect("record path");
        let records = ensure_records_directory(root.path()).expect("records");
        let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
        let opened = read_opened_subagent_record_in(&directory, &path).expect("opened record");
        let displaced = path.with_extension("displaced");
        let mut replacement = attempted.clone();
        replacement.status = "failed".to_string();
        replacement.error = Some("replacement sentinel".to_string());
        replacement.updated_at = Utc::now();
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");

        let error =
            cleanup_precommit_record_with_hook(root.path(), &attempted, Some(&opened.file), || {
                std::fs::rename(&path, &displaced).map_err(|error| error.to_string())?;
                std::fs::write(&path, &replacement_bytes).map_err(|error| error.to_string())?;
                Ok(())
            })
            .expect_err("replacement must defeat conditional precommit deletion");
        assert!(
            error.contains("changed") || error.contains("identity"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&path).expect("replacement remains"),
            replacement_bytes
        );
        assert!(displaced.exists(), "attempted generation remains displaced");
    }

    #[test]
    fn precommit_cleanup_preserves_an_in_place_record_mutation() {
        let root = tempfile::tempdir().expect("root");
        let attempted = record_fixture(root.path(), "precommit-in-place", "running");
        let publication =
            write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
                .expect("attempted record");
        let path = record_path(root.path(), &attempted.id).expect("record path");
        let mut replacement = attempted.clone();
        replacement.status = "failed".to_string();
        replacement.error = Some("in-place replacement".to_string());
        replacement.updated_at = Utc::now();
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");

        let error = cleanup_precommit_record_with_hook(
            root.path(),
            &attempted,
            Some(&publication.receipt.file),
            || std::fs::write(&path, &replacement_bytes).map_err(|error| error.to_string()),
        )
        .expect_err("in-place mutation must defeat semantic cleanup");

        assert!(error.contains("bytes changed"), "{error}");
        assert_eq!(
            std::fs::read(&path).expect("mutated record remains"),
            replacement_bytes
        );
    }

    #[test]
    fn precommit_cleanup_preserves_an_identity_distinct_json_equivalent_replacement() {
        let root = tempfile::tempdir().expect("root");
        let attempted = record_fixture(root.path(), "precommit-equivalent", "running");
        let _publication =
            write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
                .expect("attempted record");
        let path = record_path(root.path(), &attempted.id).expect("record path");
        let records = ensure_records_directory(root.path()).expect("records");
        let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
        let opened = read_opened_subagent_record_in(&directory, &path).expect("opened record");
        let displaced = path.with_extension("attempted-displaced");
        let attempted_bytes = serde_json::to_vec_pretty(&attempted).expect("attempted JSON");
        std::fs::rename(&path, &displaced).expect("displace attempted publication");
        std::fs::write(&path, &attempted_bytes).expect("publish JSON-equivalent replacement");

        let error = cleanup_precommit_record(root.path(), &attempted, Some(&opened.file))
            .expect_err("JSON equality cannot authorize deletion of a distinct file identity");
        assert!(error.contains("publication identity"), "{error}");
        assert_eq!(
            std::fs::read(&path).expect("equivalent replacement remains"),
            attempted_bytes
        );
        assert!(
            displaced.exists(),
            "attempted publication remains displaced"
        );
    }

    #[test]
    fn registration_failure_receipt_preserves_a_json_equivalent_substitution() {
        let root = tempfile::tempdir().expect("root");
        let attempted = record_fixture(root.path(), "registration-equivalent", "running");
        let publication =
            write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
                .expect("attempted record");
        let path = record_path(root.path(), &attempted.id).expect("record path");
        let displaced = path.with_extension("registered-publication-displaced");
        let attempted_bytes = serde_json::to_vec_pretty(&attempted).expect("attempted JSON");
        std::fs::rename(&path, &displaced).expect("displace registered publication");
        std::fs::write(&path, &attempted_bytes).expect("publish JSON-equivalent replacement");

        let error =
            cleanup_record_after_registration_failure(root.path(), &attempted, &publication)
                .expect_err("registration compensation cannot delete a substituted record");
        if publication.receipt.exact_identity {
            assert!(error.contains("publication identity"), "{error}");
        } else {
            assert!(error.contains("identity is unavailable"), "{error}");
        }
        assert_eq!(
            std::fs::read(&path).expect("registration replacement remains"),
            attempted_bytes
        );
        assert!(
            displaced.exists(),
            "the exact attempted publication remains displaced"
        );
    }

    #[test]
    fn registration_failure_uses_only_an_exact_publication_receipt_for_cleanup() {
        let root = tempfile::tempdir().expect("root");
        let attempted = record_fixture(root.path(), "registration-cleanup", "running");
        let publication =
            write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
                .expect("attempted record");
        let path = record_path(root.path(), &attempted.id).expect("record path");

        let cleanup =
            cleanup_record_after_registration_failure(root.path(), &attempted, &publication);
        if publication.receipt.exact_identity {
            cleanup.expect("exact receipt authorizes conditional cleanup");
            assert!(!path.exists());
        } else {
            let error = cleanup.expect_err("non-exact receipt must preserve the publication");
            assert!(error.contains("identity is unavailable"), "{error}");
            assert!(path.exists());
        }
    }

    #[test]
    fn stale_record_revision_cannot_overwrite_a_newer_generation() {
        let root = tempfile::tempdir().expect("root");
        let record = record_fixture(root.path(), "stale-revision", "running");
        write_subagent_record(root.path(), &record).expect("initial record");
        let records = ensure_records_directory(root.path()).expect("records");
        let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
        let path = record_path(root.path(), &record.id).expect("record path");
        let mut stale = read_opened_subagent_record_in(&directory, &path).expect("stale revision");
        update_subagent_record(root.path(), &record.id, |current| {
            current.status = "completed".to_string();
            current.updated_at = Utc::now();
            Ok(())
        })
        .expect("newer revision");
        stale.record.status = "failed".to_string();
        let error = persist_subagent_record_revision(root.path(), &stale.record, &mut stale.file)
            .expect_err("stale expected handle must fail");
        assert!(
            error.contains("identity") || error.contains("changed"),
            "{error}"
        );
        assert_eq!(
            get_subagent_record(root.path(), &record.id)
                .expect("authoritative revision")
                .status,
            "completed"
        );
    }

    #[test]
    fn records_namespace_requires_native_origin_or_explicit_offline_attestation() {
        let existing = tempfile::tempdir().expect("existing root");
        let existing_records = records_dir(existing.path());
        std::fs::create_dir_all(&existing_records).expect("pre-existing clean records namespace");
        let before = directory_tree_snapshot(&existing_records);

        let error = ensure_records_directory(existing.path())
            .expect_err("a clean but unmarked existing namespace is not proof of quiescence");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert_eq!(directory_tree_snapshot(&existing_records), before);
        assert!(!existing_records
            .join(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT)
            .exists());

        assert_eq!(
            confirm_no_legacy_subagent_processes(existing.path())
                .expect("operator attestation authorizes the existing clean namespace"),
            0
        );
        ensure_records_directory(existing.path()).expect("completed epoch authorizes ordinary use");
        let existing_directory =
            crate::daemons::state::StableDirectory::open(&existing_records).expect("records");
        let receipt = load_legacy_record_lock_migration_receipt(&existing_directory)
            .expect("receipt load")
            .expect("completed operator receipt");
        assert_eq!(receipt.phase, LegacyRecordLockMigrationPhase::Completed);
        assert_eq!(
            receipt.records_identity,
            records_directory_identity(&existing_directory).expect("records identity")
        );

        let native = tempfile::tempdir().expect("native root");
        let native_records = ensure_records_directory(native.path())
            .expect("current build creates a marked records namespace");
        let native_directory =
            crate::daemons::state::StableDirectory::open(&native_records).expect("native records");
        let receipt = load_legacy_record_lock_migration_receipt(&native_directory)
            .expect("native receipt load")
            .expect("native-origin receipt");
        assert_eq!(receipt.phase, LegacyRecordLockMigrationPhase::Completed);
        assert!(receipt.artifacts.is_empty());
        assert_eq!(
            receipt.records_identity,
            records_directory_identity(&native_directory).expect("native records identity")
        );

        let interrupted = tempfile::tempdir().expect("interrupted native root");
        let staging = interrupted
            .path()
            .join(".nib")
            .join(NATIVE_RECORDS_STAGING_DIRECTORY);
        std::fs::create_dir_all(&staging).expect("interrupted native staging");
        std::fs::write(staging.join("interrupted.sentinel"), b"incomplete")
            .expect("interrupted staging sentinel");
        let interrupted_before = directory_tree_snapshot(&staging);
        let error = ensure_records_directory(interrupted.path())
            .expect_err("ordinary operation preserves incomplete native staging");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert_eq!(directory_tree_snapshot(&staging), interrupted_before);
        assert!(!records_dir(interrupted.path()).exists());
        let error = confirm_no_legacy_subagent_processes(interrupted.path())
            .expect_err("explicit attestation cannot claim ownership of hostile staging");
        assert!(error.contains("preserved for inspection"), "{error}");
        assert_eq!(
            directory_tree_snapshot(&staging),
            interrupted_before,
            "doctor must preserve every hostile staging byte"
        );
        assert!(!records_dir(interrupted.path()).exists());

        let resumable = tempfile::tempdir().expect("resumable native root");
        let nib = resumable.path().join(".nib");
        std::fs::create_dir(&nib).expect("resumable nib directory");
        let nib_directory =
            crate::daemons::state::StableDirectory::open(&nib).expect("resumable nib capability");
        let resumable_staging = nib.join(NATIVE_RECORDS_STAGING_DIRECTORY);
        let staged = create_native_records_staging(
            &nib_directory,
            &resumable_staging,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("valid exact native staging");
        drop(staged);
        let exact_receipt =
            std::fs::read(resumable_staging.join(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT))
                .expect("exact native receipt");
        assert_eq!(
            confirm_no_legacy_subagent_processes(resumable.path())
                .expect("valid exact staging resumes safely"),
            0
        );
        assert!(!resumable_staging.exists());
        ensure_records_directory(resumable.path())
            .expect("resumed native publication authorizes ordinary use");

        let mismatch = tempfile::tempdir().expect("mismatched native root");
        let mismatch_staging = mismatch
            .path()
            .join(".nib")
            .join(NATIVE_RECORDS_STAGING_DIRECTORY);
        std::fs::create_dir_all(&mismatch_staging).expect("mismatched native staging");
        std::fs::write(
            mismatch_staging.join(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT),
            exact_receipt,
        )
        .expect("foreign native receipt");
        let mismatch_before = directory_tree_snapshot(&mismatch_staging);
        let error = confirm_no_legacy_subagent_processes(mismatch.path())
            .expect_err("foreign identity receipt must not authorize staging");
        assert!(error.contains("preserved for inspection"), "{error}");
        assert_eq!(directory_tree_snapshot(&mismatch_staging), mismatch_before);
        assert!(!records_dir(mismatch.path()).exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn pending_offline_epoch_is_resumable_only_by_explicit_doctor_attestation() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("native records");
        let legacy = legacy_record_lock_path(&records, "pending-epoch").expect("legacy path");
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&legacy).expect("legacy anchor");
        std::fs::create_dir(legacy.parent().expect("legacy parent")).expect("legacy directory");
        std::fs::write(&legacy, b"pending epoch").expect("legacy visible");
        std::fs::hard_link(&legacy, &anchor).expect("legacy anchor");
        let directory =
            crate::daemons::state::StableDirectory::open(&records).expect("records directory");
        let scan = scan_legacy_record_lock_namespaces(&directory, None).expect("legacy scan");
        let receipt = LegacyRecordLockMigrationReceipt {
            version: LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION,
            epoch_id: uuid::Uuid::new_v4().to_string(),
            records_identity: records_directory_identity(&directory).expect("records identity"),
            phase: LegacyRecordLockMigrationPhase::Pending,
            attested_at: Utc::now(),
            completed_at: None,
            artifacts: scan.artifacts,
        };
        save_legacy_record_lock_migration_receipt(&directory, &receipt, None)
            .expect("persist interrupted pending epoch");

        let error = ensure_records_directory(root.path())
            .expect_err("pending epoch never authorizes an ordinary operation");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert!(legacy.exists());
        assert!(anchor.exists());

        assert_eq!(
            confirm_no_legacy_subagent_processes(root.path())
                .expect("explicit renewed attestation resumes the exact pending manifest"),
            2
        );
        assert!(!legacy.exists());
        assert!(!anchor.exists());
        ensure_records_directory(root.path()).expect("completed resumed epoch authorizes use");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn global_subagent_legacy_lock_migration_is_bounded_and_fails_closed_for_live_owner() {
        let root = tempfile::tempdir().expect("root");
        let records = records_dir(root.path());
        let legacy = records.join(".locks");
        std::fs::create_dir_all(&legacy).expect("legacy lock directory");
        let mut legacy_pairs = Vec::new();
        let legacy_pair_count = if cfg!(windows) {
            SUBAGENT_RECORD_LOCK_STRIPES + 1
        } else {
            96
        };
        for index in 0..legacy_pair_count {
            let visible = legacy.join(format!("untouched-{index}.lock"));
            let anchor = crate::daemons::state::daemon_lock_anchor_path(&visible)
                .expect("legacy anchor path");
            std::fs::write(&visible, b"legacy").expect("legacy lock");
            std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
            legacy_pairs.push((visible, anchor));
        }

        let error = ensure_records_directory(root.path())
            .expect_err("ordinary startup must not consume an unattested legacy namespace");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert!(legacy_pairs
            .iter()
            .all(|(visible, anchor)| visible.exists() && anchor.exists()));

        assert_eq!(
            confirm_no_legacy_subagent_processes(root.path())
                .expect("explicit offline legacy migration"),
            legacy_pairs.len() * 2
        );
        for (visible, anchor) in &legacy_pairs {
            assert!(!visible.exists());
            assert!(!anchor.exists());
        }
        let fixed_visible = std::fs::read_dir(root.path().join(".nib"))
            .expect("fixed lock directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".subagent-record-stripe-")
            })
            .count();
        let fixed_anchors = std::fs::read_dir(root.path())
            .expect("fixed anchor directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.contains("subagent-record-stripe-") && name.ends_with(".anchor")
            })
            .count();
        assert!(fixed_visible <= SUBAGENT_RECORD_LOCK_STRIPES);
        assert_eq!(fixed_visible, fixed_anchors);

        let live_visible = legacy.join("untouched-live.lock");
        let live_anchor = crate::daemons::state::daemon_lock_anchor_path(&live_visible)
            .expect("live legacy anchor path");
        std::fs::write(&live_visible, b"live legacy").expect("live legacy lock");
        std::fs::hard_link(&live_visible, &live_anchor).expect("live legacy anchor");
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&live_anchor)
            .expect("legacy owner");
        owner.try_lock().expect("hold legacy lock");
        let error = ensure_records_directory(root.path())
            .expect_err("ordinary operation must reject post-epoch legacy state");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert!(live_visible.exists());
        assert!(live_anchor.exists());
        let error = confirm_no_legacy_subagent_processes(root.path())
            .expect_err("attested migration must still fail closed for a live owner");
        assert!(error.contains("still owned"), "{error}");
        drop(owner);
        assert_eq!(
            confirm_no_legacy_subagent_processes(root.path())
                .expect("fresh attestation migrates the released legacy lock"),
            2
        );
        ensure_records_directory(root.path()).expect("completed receipt authorizes ordinary use");
        assert!(!live_visible.exists());
        assert!(!live_anchor.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn missing_legacy_directory_still_reconciles_live_and_dead_canonical_anchors() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let legacy_locks = records.join(".locks");
        assert!(!legacy_locks.exists(), "fixture has no legacy directory");
        let visible = legacy_locks.join("anchor-only.lock");
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
        std::fs::write(&anchor, b"anchor-only").expect("anchor-only legacy lock");
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&anchor)
            .expect("legacy anchor owner");
        owner.try_lock().expect("hold legacy anchor");

        let error = migrate_legacy_record_locks(
            root.path(),
            &records,
            Some(Instant::now() + Duration::from_secs(1)),
        )
        .expect_err("ordinary operation must reject an anchor introduced after the epoch");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert!(anchor.exists(), "live legacy anchor was preserved");
        assert!(!legacy_locks.exists(), "migration did not create .locks");

        let error = confirm_no_legacy_subagent_processes(root.path())
            .expect_err("explicit migration still rejects a live anchor owner");
        assert!(error.contains("still owned"), "{error}");

        drop(owner);
        assert_eq!(
            confirm_no_legacy_subagent_processes(root.path())
                .expect("fresh attestation cleans the dead anchor-only legacy lock"),
            1
        );
        assert!(!anchor.exists(), "dead legacy anchor was removed");
        assert!(!legacy_locks.exists(), "cleanup did not create .locks");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn missing_legacy_directory_retries_anchor_quarantine_without_creating_state() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let legacy_locks = records.join(".locks");
        let visible = legacy_locks.join("anchor-quarantine.lock");
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
        std::fs::write(&anchor, b"anchor-quarantine").expect("legacy anchor");
        let anchor_directory =
            crate::daemons::state::StableDirectory::open(&records).expect("records directory");
        let quarantine = anchor_directory
            .deterministic_artifact_path(&anchor, ".nib-legacy-lock-delete-", ".quarantine")
            .expect("anchor quarantine");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let deadline = Instant::now() + Duration::from_millis(150);
        let worker_anchor = anchor.clone();
        let worker_quarantine = quarantine.clone();
        let worker = std::thread::spawn(move || {
            let mut paused = false;
            crate::daemons::state::cleanup_legacy_lock_pair_optional_with_guard(
                None,
                &visible,
                &anchor_directory,
                &worker_anchor,
                || {
                    if !paused && worker_quarantine.exists() && !worker_anchor.exists() {
                        paused = true;
                        ready_tx.send(()).expect("publish anchor quarantine pause");
                        resume_rx.recv().expect("resume anchor cleanup");
                    }
                    ensure_subagent_reconciliation_deadline(Some(deadline))
                },
            )
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("anchor cleanup reached its quarantine boundary");
        std::thread::sleep(Duration::from_millis(200));
        resume_tx.send(()).expect("resume expired anchor cleanup");
        let error = worker
            .join()
            .expect("anchor cleanup worker")
            .expect_err("expired cleanup retains anchor quarantine");
        assert!(error.contains("deadline elapsed"), "{error}");
        assert!(quarantine.exists());
        assert!(!anchor.exists());
        assert!(!legacy_locks.exists());

        assert_eq!(
            confirm_no_legacy_subagent_processes(root.path())
                .expect("fresh attestation removes retained anchor quarantine"),
            1
        );
        assert!(!anchor.exists());
        assert!(!quarantine.exists());
        assert!(!legacy_locks.exists(), "retry did not create .locks");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn missing_legacy_directory_preserves_ambiguous_anchor_quarantine() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let legacy_locks = records.join(".locks");
        let visible = legacy_locks.join("ambiguous-anchor.lock");
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
        std::fs::write(&anchor, b"canonical-anchor").expect("canonical anchor");
        let directory =
            crate::daemons::state::StableDirectory::open(&records).expect("records directory");
        let quarantine = directory
            .deterministic_artifact_path(&anchor, ".nib-legacy-lock-delete-", ".quarantine")
            .expect("anchor quarantine");
        std::fs::write(&quarantine, b"mismatched-quarantine").expect("ambiguous anchor quarantine");

        let error = confirm_no_legacy_subagent_processes(root.path())
            .expect_err("mismatched anchor quarantine must fail closed");
        assert!(error.contains("different identities"), "{error}");
        assert_eq!(
            std::fs::read(&anchor).expect("canonical anchor"),
            b"canonical-anchor"
        );
        assert_eq!(
            std::fs::read(&quarantine).expect("ambiguous quarantine"),
            b"mismatched-quarantine"
        );
        assert!(!legacy_locks.exists(), "ambiguity did not create .locks");
    }

    #[cfg(any(unix, windows))]
    fn assert_post_scan_live_legacy_publication_is_rejected(anchor_only: bool) {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let legacy_locks = records.join(".locks");
        let id = if anchor_only {
            "post-scan-live-anchor"
        } else {
            "post-scan-live-pair"
        };
        let visible = legacy_locks.join(format!("{id}.lock"));
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
        let mut owner = None;

        let error = confirm_no_legacy_subagent_processes_with_scan_hook(root.path(), |pass| {
            if pass != 0 {
                return Ok(());
            }
            if !anchor_only {
                std::fs::create_dir(&legacy_locks)
                    .map_err(|error| format!("create legacy directory: {error}"))?;
                std::fs::write(&visible, b"post-scan-live")
                    .map_err(|error| format!("write legacy lock: {error}"))?;
                std::fs::hard_link(&visible, &anchor)
                    .map_err(|error| format!("link legacy anchor: {error}"))?;
            } else {
                std::fs::write(&anchor, b"post-scan-live-anchor")
                    .map_err(|error| format!("write legacy anchor: {error}"))?;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&anchor)
                .map_err(|error| format!("open legacy owner: {error}"))?;
            file.try_lock()
                .map_err(|error| format!("hold legacy owner: {error}"))?;
            owner = Some(file);
            Ok(())
        })
        .expect_err("a live legacy publication after the initial scan must fail closed");

        assert!(error.contains("fresh operator confirmation"), "{error}");
        assert!(anchor.exists(), "live legacy anchor was preserved");
        if anchor_only {
            assert!(
                !legacy_locks.exists(),
                "anchor-only scan did not create .locks"
            );
        } else {
            assert!(visible.exists(), "live legacy lock was preserved");
        }

        drop(owner.take());
        confirm_no_legacy_subagent_processes(root.path())
            .expect("a fresh attestation cleans the released legacy publication");
        assert!(!visible.exists(), "released legacy lock was removed");
        assert!(!anchor.exists(), "released legacy anchor was removed");
        if anchor_only {
            assert!(
                !legacy_locks.exists(),
                "anchor-only retry did not create .locks"
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn post_scan_live_legacy_pair_and_anchor_are_detected_before_success() {
        assert_post_scan_live_legacy_publication_is_rejected(false);
        assert_post_scan_live_legacy_publication_is_rejected(true);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn post_scan_legacy_directory_replacement_is_rejected_before_mutation() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let legacy_locks = records.join(".locks");
        std::fs::create_dir(&legacy_locks).expect("initial legacy directory");
        let displaced = records.join(".locks.displaced");
        let visible = legacy_locks.join("post-scan-replacement.lock");
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
        let mut owner = None;

        let error = confirm_no_legacy_subagent_processes_with_scan_hook(root.path(), |pass| {
            if pass != 0 {
                return Ok(());
            }
            std::fs::rename(&legacy_locks, &displaced)
                .map_err(|error| format!("displace legacy directory: {error}"))?;
            std::fs::create_dir(&legacy_locks)
                .map_err(|error| format!("create replacement legacy directory: {error}"))?;
            std::fs::write(&visible, b"replacement-live")
                .map_err(|error| format!("write replacement legacy lock: {error}"))?;
            std::fs::hard_link(&visible, &anchor)
                .map_err(|error| format!("link replacement legacy anchor: {error}"))?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&anchor)
                .map_err(|error| format!("open replacement legacy owner: {error}"))?;
            file.try_lock()
                .map_err(|error| format!("hold replacement legacy owner: {error}"))?;
            owner = Some(file);
            Ok(())
        })
        .expect_err("a replaced legacy namespace must fail before mutation");

        assert!(
            error.contains("state changed") || error.contains("identity changed"),
            "{error}"
        );
        assert!(visible.is_file(), "replacement legacy lock was preserved");
        assert!(anchor.is_file(), "replacement legacy anchor was preserved");
        assert!(
            displaced.is_dir(),
            "the retained original directory was preserved"
        );

        drop(owner.take());
        assert_eq!(
            std::fs::read(&visible).expect("replacement legacy lock"),
            b"replacement-live"
        );
        assert_eq!(
            std::fs::read(&anchor).expect("replacement legacy anchor"),
            b"replacement-live"
        );
        confirm_no_legacy_subagent_processes(root.path())
            .expect("a fresh attestation cleans the released replacement artifacts");
        assert!(!visible.exists(), "released replacement lock was removed");
        assert!(!anchor.exists(), "released replacement anchor was removed");
        assert!(
            displaced.is_dir(),
            "unrelated displaced state was untouched"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn record_bridge_blocks_a_live_legacy_contender_published_after_migration() {
        let initial_bridge_timeout = if cfg!(windows) {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(200)
        };
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let id = "post-migration-legacy-contender";
        let legacy = legacy_record_lock_path(&records, id).expect("legacy lock path");
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&legacy).expect("legacy anchor");
        let modern = record_lock_path(root.path(), id).expect("modern lock path");
        let mut owner = None;
        let mut entered = false;

        let error = with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            id,
            &records,
            Instant::now() + initial_bridge_timeout,
            None,
            || {
                std::fs::create_dir(legacy.parent().expect("legacy lock parent"))
                    .map_err(|error| format!("create legacy lock directory: {error}"))?;
                std::fs::write(&legacy, b"live-old-writer")
                    .map_err(|error| format!("write legacy lock: {error}"))?;
                std::fs::hard_link(&legacy, &anchor)
                    .map_err(|error| format!("link legacy anchor: {error}"))?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&anchor)
                    .map_err(|error| format!("open legacy owner: {error}"))?;
                file.try_lock()
                    .map_err(|error| format!("hold legacy owner: {error}"))?;
                owner = Some(file);
                Ok(())
            },
            |_, _| {
                entered = true;
                Ok(())
            },
        )
        .expect_err("post-migration legacy contender must fence the modern operation");

        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert!(
            !entered,
            "modern critical section did not overlap the old writer"
        );
        assert!(
            !modern.exists(),
            "modern stripe was not acquired after legacy contention"
        );
        assert!(legacy.is_file(), "live legacy lock was preserved");
        assert!(anchor.is_file(), "live legacy anchor was preserved");

        drop(owner.take());
        assert_eq!(
            std::fs::read(&legacy).expect("preserved legacy lock"),
            b"live-old-writer"
        );
        assert_eq!(
            std::fs::read(&anchor).expect("preserved legacy anchor"),
            b"live-old-writer"
        );
        let error = with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            id,
            &records,
            Instant::now() + Duration::from_secs(2),
            None,
            || Ok(()),
            |_, _| Ok(()),
        )
        .expect_err("a completed epoch never consumes newly introduced legacy state");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        confirm_no_legacy_subagent_processes(root.path())
            .expect("fresh offline attestation cleans the released contender");
        let mut observed_cleanup = false;
        with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            id,
            &records,
            Instant::now() + Duration::from_secs(2),
            None,
            || Ok(()),
            |_, _| {
                observed_cleanup = !legacy.exists() && !anchor.exists();
                Ok(())
            },
        )
        .expect("fresh bridge cleans the dead contender and enters safely");
        assert!(
            observed_cleanup,
            "dead legacy identity was removed before ordinary fixed-stripe use resumed"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn offline_epoch_preserves_an_open_before_lock_contender_until_operator_quiescence() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("native records namespace");
        let legacy =
            legacy_record_lock_path(&records, LEGACY_OPEN_CHILD_ID).expect("legacy lock path");
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&legacy).expect("legacy anchor");
        std::fs::create_dir(legacy.parent().expect("legacy parent"))
            .expect("legacy lock directory");
        std::fs::write(&legacy, b"prior-version-contender").expect("legacy lock");
        std::fs::hard_link(&legacy, &anchor).expect("legacy anchor");
        let opened_before = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&anchor)
            .expect("observe exact legacy anchor");
        let identity_before = crate::fs_security::file_identity_snapshot(&opened_before)
            .expect("legacy identity before child open");
        let ready = root.path().join("legacy-open.ready");
        let resume = root.path().join("legacy-open.resume");
        let locked = root.path().join("legacy-open.locked");
        let release = root.path().join("legacy-open.release");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::legacy_open_before_lock_child_process",
                "--nocapture",
            ])
            .env(LEGACY_OPEN_CHILD_PROJECT_ROOT, root.path())
            .env(LEGACY_OPEN_CHILD_READY, &ready)
            .env(LEGACY_OPEN_CHILD_RESUME, &resume)
            .env(LEGACY_OPEN_CHILD_LOCKED, &locked)
            .env(LEGACY_OPEN_CHILD_RELEASE, &release)
            .spawn()
            .expect("spawn paused prior-version contender");
        let wait_for = |path: &Path, child: &mut std::process::Child| {
            let started = Instant::now();
            while !path.exists() {
                if let Some(status) = child.try_wait().expect("inspect legacy child") {
                    panic!("legacy child exited before {}: {status}", path.display());
                }
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "legacy child did not publish {}",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for(&ready, &mut child);

        let mut entered = false;
        let error = with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            LEGACY_OPEN_CHILD_ID,
            &records,
            Instant::now() + Duration::from_secs(1),
            None,
            || Ok(()),
            |_, _| {
                entered = true;
                Ok(())
            },
        )
        .expect_err("ordinary operation must preserve an open-but-unlocked legacy inode");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert!(!entered);
        let reopened = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&anchor)
            .expect("preserved legacy anchor");
        assert_eq!(
            crate::fs_security::file_identity_snapshot(&reopened)
                .expect("legacy identity after refusal"),
            identity_before,
            "ordinary refusal neither deletes nor recreates the inode already opened by the child"
        );

        std::fs::write(&resume, b"lock exact inode").expect("resume legacy child");
        wait_for(&locked, &mut child);
        std::fs::write(&release, b"operator stopped old binary").expect("release legacy child");
        let status = child.wait().expect("reap legacy child");
        assert!(status.success(), "legacy child failed: {status}");
        drop(reopened);
        drop(opened_before);

        assert_eq!(
            confirm_no_legacy_subagent_processes(root.path())
                .expect("attestation is accepted only after the prior binary exits"),
            2
        );
        with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            LEGACY_OPEN_CHILD_ID,
            &records,
            Instant::now() + Duration::from_secs(1),
            None,
            || Ok(()),
            |_, _| Ok(()),
        )
        .expect("completed epoch enables the fixed modern stripe");
        assert!(!legacy.exists());
        assert!(!anchor.exists());

        std::fs::write(&legacy, b"late-prior-version-state").expect("late legacy lock");
        std::fs::hard_link(&legacy, &anchor).expect("late legacy anchor");
        let error = with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            LEGACY_OPEN_CHILD_ID,
            &records,
            Instant::now() + Duration::from_secs(1),
            None,
            || Ok(()),
            |_, _| Ok(()),
        )
        .expect_err("a stale completed epoch cannot authorize newly introduced legacy state");
        assert!(error.contains("confirm-no-legacy-processes"), "{error}");
        assert!(legacy.exists());
        assert!(anchor.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn record_bridge_uses_only_the_modern_stripe_and_obeys_one_deadline() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let id = "record-bridge-lock-order";
        let modern = record_lock_path(root.path(), id).expect("modern lock path");
        let modern_anchor =
            crate::daemons::state::daemon_lock_anchor_path(&modern).expect("modern anchor");
        let held_modern =
            open_repository_merge_lock_anchor(&modern, &modern_anchor).expect("modern lock pair");
        held_modern.try_lock().expect("hold modern stripe");
        let legacy = legacy_record_lock_path(&records, id).expect("legacy lock path");
        let legacy_anchor =
            crate::daemons::state::daemon_lock_anchor_path(&legacy).expect("legacy anchor");
        let marker = records.join("record-bridge-operation.marker");
        let started = Instant::now();

        let error = with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            id,
            &records,
            Instant::now() + Duration::from_millis(200),
            None,
            || Ok(()),
            |_, _| std::fs::write(&marker, b"late mutation").map_err(|error| error.to_string()),
        )
        .expect_err("held modern stripe must expire after the legacy fence is acquired");

        assert!(
            error.contains("delegation state lock deadline elapsed"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!legacy.exists(), "modern bridges never create legacy locks");
        assert!(
            !legacy_anchor.exists(),
            "modern bridges never create legacy anchors"
        );
        assert!(
            !marker.exists(),
            "expired bridge did not enter record mutation"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(!marker.exists(), "no record mutation occurred after return");

        drop(held_modern);
        with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            id,
            &records,
            Instant::now() + Duration::from_secs(2),
            None,
            || Ok(()),
            |_, _| std::fs::write(&marker, b"fresh").map_err(|error| error.to_string()),
        )
        .expect("fresh deadline acquires the fixed modern stripe");
        assert_eq!(std::fs::read(&marker).expect("fresh mutation"), b"fresh");
    }

    #[cfg(unix)]
    #[test]
    fn record_bridge_rejects_whole_records_replacement_before_sensitive_mutation() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let original_sentinel = records.join("original.sentinel");
        std::fs::write(&original_sentinel, b"original records").expect("original records sentinel");
        let process_state = root.path().join(".nib").join("process-scopes");
        let audit_state = root.path().join(".nib").join("sessions");
        std::fs::create_dir(&process_state).expect("process state directory");
        std::fs::create_dir(&audit_state).expect("audit state directory");
        std::fs::write(process_state.join("scope.sentinel"), b"process state")
            .expect("process sentinel");
        std::fs::write(audit_state.join("audit.sentinel"), b"audit state").expect("audit sentinel");
        let original_before = directory_tree_snapshot(&records);
        let process_before = directory_tree_snapshot(&process_state);
        let audit_before = directory_tree_snapshot(&audit_state);
        let displaced = root.path().join(".nib").join("subagents.displaced");
        let record = record_fixture(root.path(), "records-capability-replacement", "running");
        let path = record_path(root.path(), &record.id).expect("record path");
        let mut replacement_before = None;
        let mut entered = false;

        let error = with_subagent_record_lock_bridge_in_deadline(
            root.path(),
            &record.id,
            &records,
            Instant::now() + Duration::from_secs(2),
            None,
            || {
                std::fs::rename(&records, &displaced)
                    .map_err(|error| format!("displace records directory: {error}"))?;
                std::fs::create_dir(&records)
                    .map_err(|error| format!("create replacement records directory: {error}"))?;
                std::fs::write(records.join("replacement.sentinel"), b"replacement records")
                    .map_err(|error| format!("write replacement sentinel: {error}"))?;
                replacement_before = Some(directory_tree_snapshot(&records));
                Ok(())
            },
            |directory, deadline| {
                entered = true;
                write_subagent_record_unlocked_until(
                    root.path(),
                    directory,
                    &path,
                    &record,
                    crate::daemons::state::FileExpectation::Missing,
                    deadline,
                )?;
                std::fs::write(process_state.join("scope.sentinel"), b"redirected")
                    .map_err(|error| error.to_string())?;
                std::fs::write(audit_state.join("audit.sentinel"), b"redirected")
                    .map_err(|error| error.to_string())
            },
        )
        .expect_err("whole records replacement must detach the retained capability");

        assert!(error.contains("identity changed"), "{error}");
        assert!(!entered, "the sensitive record callback was not entered");
        assert_eq!(
            directory_tree_snapshot(&displaced),
            original_before,
            "the retained original namespace was not mutated after displacement"
        );
        assert_eq!(
            directory_tree_snapshot(&records),
            replacement_before.expect("replacement snapshot"),
            "the replacement namespace did not receive redirected lock or record state"
        );
        assert_eq!(directory_tree_snapshot(&process_state), process_before);
        assert_eq!(directory_tree_snapshot(&audit_state), audit_before);
        assert!(!displaced.join(format!("{}.json", record.id)).exists());
        assert!(!path.exists());
    }

    #[test]
    fn pending_merge_intent_preserves_subagent_result_and_integration_identity() {
        let root = tempfile::tempdir().expect("root");
        let mut record = SubagentRecord {
            id: "sub-pending".to_string(),
            parent_session_id: Some("parent".to_string()),
            child_session_id: "child-pending".to_string(),
            prompt: "fixture".to_string(),
            status: "completed".to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: root.path().join("worktree"),
            branch: "nib/subagent/sub-pending".to_string(),
            branch_oid: Some("a".repeat(40)),
            result: Some(json!({"summary": "verified"})),
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        begin_pending_merge(&mut record, "task test", &"a".repeat(40), &"b".repeat(40));

        let intent = pending_merge_intent(&record).expect("pending intent");
        assert_eq!(record.status, MERGE_PENDING_STATUS);
        assert_eq!(intent.branch_commit, "a".repeat(40));
        assert_eq!(intent.parent_head, "b".repeat(40));
        assert_eq!(intent.verification_command, "task test");
        assert_eq!(
            record.result.as_ref().unwrap()["subagent_result"]["summary"],
            "verified"
        );
        assert!(record.result.as_ref().unwrap()["merge_stdout"].is_null());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_merge_lock_times_out_when_another_holder_does_not_release() {
        let root = tempfile::tempdir().expect("root");
        let _held = RepositoryMergeLock::acquire_with_timeout(root.path(), Duration::from_secs(1))
            .await
            .expect("first lock");
        let started = Instant::now();

        let error =
            RepositoryMergeLock::acquire_with_timeout(root.path(), Duration::from_millis(75))
                .await
                .expect_err("second lock must time out");

        assert!(error.contains("timed out acquiring repository merge lock"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn repository_merge_lock_default_timeout_is_thirty_seconds() {
        assert_eq!(REPOSITORY_MERGE_LOCK_TIMEOUT, Duration::from_secs(30));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn expired_absolute_delegation_lock_rejects_free_lock_before_operation() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let lock_path = record_lock_path(root.path(), "sub-expired-free").expect("record lock");
        let mut operation_ran = false;

        let error = with_bounded_delegation_lock_in_until(
            &lock_path,
            &records,
            Instant::now() - Duration::from_millis(1),
            |_, _| {
                operation_ran = true;
                Ok(())
            },
        )
        .expect_err("an expired absolute deadline must reject an uncontended lock");

        assert!(error.contains("delegation state lock deadline elapsed"));
        assert!(!operation_ran, "expired delegation lock entered mutation");
        assert!(!lock_path.exists(), "expired delegation lock created state");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn records_initialization_deadline_guards_nib_creation_and_retries_safely() {
        let root = tempfile::tempdir().expect("root");
        let nib = root.path().join(".nib");
        let deadline = Instant::now() + Duration::from_millis(300);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let worker_root = root.path().to_path_buf();
        let worker_nib = nib.clone();
        let worker = std::thread::spawn(move || {
            let mut paused = false;
            open_or_create_records_directory_with_setup_hook(&worker_root, Some(deadline), |path| {
                if !paused && path == worker_nib {
                    paused = true;
                    ready_tx.send(()).expect("signal nib create boundary");
                    resume_rx.recv().expect("resume nib create boundary");
                }
                Ok(())
            })
            .expect_err("expired nib creation must fail closed")
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("records initialization reached nib creation");
        let paused = subagent_namespace_snapshot(root.path());
        while Instant::now() < deadline {
            std::thread::yield_now();
        }
        resume_tx.send(()).expect("release nib create boundary");
        let error = worker.join().expect("join records initializer");
        assert!(error.contains("deadline elapsed"), "{error}");
        assert_eq!(subagent_namespace_snapshot(root.path()), paused);
        assert!(!nib.exists());

        let records = open_or_create_records_directory(
            root.path(),
            Some(Instant::now() + Duration::from_secs(2)),
        )
        .expect("fresh deadline retries exact records initialization");
        assert!(records.is_dir());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn records_setup_and_migration_share_one_default_absolute_deadline() {
        let root = tempfile::tempdir().expect("root");
        let mut paused_namespace = None;
        #[cfg(windows)]
        let default_timeout = Duration::from_secs(2);
        #[cfg(not(windows))]
        let default_timeout = Duration::from_millis(80);
        let error = ensure_records_directory_until_with_phase_hook(
            root.path(),
            None,
            default_timeout,
            |effective_deadline| {
                paused_namespace = Some(subagent_namespace_snapshot(root.path()));
                while Instant::now() < effective_deadline {
                    std::thread::yield_now();
                }
                Ok(())
            },
        )
        .expect_err("migration must not receive a renewed default budget");
        assert!(error.contains("deadline elapsed"), "{error}");
        let paused_namespace = paused_namespace.expect("snapshot after records setup");
        assert_eq!(
            subagent_namespace_snapshot(root.path()),
            paused_namespace,
            "expired second phase mutated the records namespace"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            subagent_namespace_snapshot(root.path()),
            paused_namespace,
            "expired migration mutated the records namespace later"
        );

        ensure_records_directory_until(root.path(), Some(Instant::now() + Duration::from_secs(2)))
            .expect("fresh explicit deadline completes authorization and migration");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn delegation_lock_setup_deadline_guards_every_namespace_mutation_and_retries() {
        #[derive(Clone, Copy)]
        enum Boundary {
            LockParent,
            AnchorParent,
            VisibleLock,
            AnchorLink,
        }

        for boundary in [
            Boundary::LockParent,
            Boundary::AnchorParent,
            Boundary::VisibleLock,
            Boundary::AnchorLink,
        ] {
            let root = tempfile::tempdir().expect("root");
            let status = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root.path())
                .status()
                .expect("initialize git repository");
            assert!(status.success());
            let nib = root.path().join(".nib");
            std::fs::create_dir(&nib).expect("nib fixture");
            let lock_parent = nib.join("deadline-locks");
            let lock_path = match boundary {
                Boundary::LockParent => lock_parent.join("setup.lock"),
                Boundary::AnchorParent | Boundary::VisibleLock | Boundary::AnchorLink => {
                    nib.join("setup.lock")
                }
            };
            let anchor_path =
                crate::daemons::state::daemon_lock_anchor_path(&lock_path).expect("anchor path");
            let target = match boundary {
                Boundary::LockParent => lock_parent.clone(),
                Boundary::AnchorParent => {
                    anchor_path.parent().expect("anchor parent").to_path_buf()
                }
                Boundary::VisibleLock => lock_path.clone(),
                Boundary::AnchorLink => anchor_path.clone(),
            };
            let deadline = Instant::now() + Duration::from_millis(300);
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
            let worker_nib = nib.clone();
            let worker_lock = lock_path.clone();
            let worker_target = target.clone();
            let worker = std::thread::spawn(move || {
                let mut paused = false;
                with_delegation_lock_in_deadline_with_setup_hook(
                    &worker_lock,
                    &worker_nib,
                    deadline,
                    None,
                    |_, _| -> Result<(), String> {
                        panic!("expired setup must not enter protected operation")
                    },
                    |path| {
                        if !paused && path == worker_target {
                            paused = true;
                            ready_tx.send(()).expect("signal setup boundary");
                            resume_rx.recv().expect("resume setup boundary");
                        }
                        Ok(())
                    },
                )
                .expect_err("expired setup boundary must fail closed")
            });
            ready_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("delegation setup reached mutation boundary");
            let paused = subagent_namespace_snapshot(root.path());
            while Instant::now() < deadline {
                std::thread::yield_now();
            }
            resume_tx.send(()).expect("release setup boundary");
            let error = worker.join().expect("join setup worker");
            assert!(error.contains("deadline elapsed"), "{error}");
            assert_eq!(
                subagent_namespace_snapshot(root.path()),
                paused,
                "expired setup mutated its namespace after the final boundary"
            );

            let mut entered = false;
            with_bounded_delegation_lock_in_until(
                &lock_path,
                &nib,
                Instant::now() + Duration::from_secs(2),
                |_, _| {
                    entered = true;
                    Ok(())
                },
            )
            .expect("fresh deadline repairs and acquires exact setup artifacts");
            assert!(entered);
            let visible = open_repository_merge_lock(&lock_path).expect("visible lock");
            let anchor = open_repository_merge_lock(&anchor_path).expect("anchor lock");
            assert_eq!(
                repository_lock_identity(&visible, &lock_path).expect("visible identity"),
                repository_lock_identity(&anchor, &anchor_path).expect("anchor identity")
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn expired_owner_lease_removal_preserves_exact_artifacts() {
        let root = tempfile::tempdir().expect("root");
        let lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let visible_path = lease.visible_path.clone();
        let anchor_path = lease.anchor_path.clone();

        let error = lease
            .remove_until(Some(Instant::now() - Duration::from_millis(1)))
            .expect_err("expired cleanup must preserve the owner lease");

        assert!(error.contains("delegation state lock deadline elapsed"));
        assert!(
            visible_path.is_file(),
            "visible owner lease was removed late"
        );
        assert!(anchor_path.is_file(), "owner lease anchor was removed late");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn delegation_lock_rejects_success_when_its_operation_crosses_the_deadline() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let lock_path =
            record_lock_path(root.path(), "sub-post-operation-deadline").expect("record lock");

        let error = with_bounded_delegation_lock_in_until(
            &lock_path,
            &records,
            Instant::now() + Duration::from_millis(40),
            |_, _| {
                std::thread::sleep(Duration::from_millis(75));
                Ok(())
            },
        )
        .expect_err("post-operation deadline expiry must reject success");

        assert!(
            error.contains("delegation state lock deadline elapsed"),
            "{error}"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn owner_creation_stops_before_anchor_publication_when_its_deadline_expires() {
        let operation_timeout = if cfg!(windows) {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(150)
        };
        let expiry_delay = operation_timeout + Duration::from_millis(50);
        let boundary_wait = if cfg!(windows) {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(2)
        };
        let root = tempfile::tempdir().expect("root");
        let owner_directory = owner_lease_directory(root.path());
        let anchor_directory = root.path().join(".nib");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let project_root = root.path().to_path_buf();
        let owner_plan = SubagentOwnerLease::plan();
        let worker_owner_directory = owner_directory.clone();
        let worker_anchor_directory = anchor_directory.clone();
        let worker = std::thread::spawn(move || {
            let mut paused = false;
            SubagentOwnerLease::create_with_timeout_and_guard(
                &project_root,
                operation_timeout,
                &owner_plan,
                || {
                    let visible_count = std::fs::read_dir(&worker_owner_directory)
                        .map(|entries| entries.filter_map(Result::ok).count())
                        .unwrap_or(0);
                    let anchor_count = std::fs::read_dir(&worker_anchor_directory)
                        .map(|entries| {
                            entries
                                .filter_map(Result::ok)
                                .filter(|entry| {
                                    entry
                                        .file_name()
                                        .as_encoded_bytes()
                                        .starts_with(OWNER_LEASE_ANCHOR_PREFIX.as_bytes())
                                })
                                .count()
                        })
                        .unwrap_or(0);
                    if !paused && visible_count == 1 && anchor_count == 0 {
                        paused = true;
                        ready_tx.send(()).expect("publish owner creation pause");
                        resume_rx.recv().expect("resume owner creation");
                    }
                    Ok(())
                },
            )
        });

        ready_rx
            .recv_timeout(boundary_wait)
            .expect("owner creation reached its anchor publication boundary");
        let before_expiry = subagent_namespace_snapshot(&owner_directory);
        assert_eq!(
            before_expiry.len(),
            1,
            "one recoverable visible lease exists"
        );
        assert!(
            std::fs::read_dir(&anchor_directory)
                .expect("anchor namespace")
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(OWNER_LEASE_ANCHOR_PREFIX.as_bytes())),
            "owner anchor was not published before the pause"
        );
        std::thread::sleep(expiry_delay);
        resume_tx.send(()).expect("resume expired owner creation");
        let error = worker
            .join()
            .expect("owner creation worker")
            .expect_err("expired owner creation must fail closed");
        assert!(
            error.contains("timed out acquiring delegation state lock"),
            "{error}"
        );
        assert_eq!(subagent_namespace_snapshot(&owner_directory), before_expiry);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(subagent_namespace_snapshot(&owner_directory), before_expiry);
    }

    #[cfg(any(unix, windows))]
    fn sweep_owner_quarantine_expiry_fixture(half: Option<bool>) {
        let root = tempfile::tempdir().expect("root");
        let lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let visible = lease.visible_path.clone();
        let anchor = lease.anchor_path.clone();
        drop(lease);
        let anchor_only = half == Some(true);
        let visible_only = half == Some(false);
        if anchor_only {
            std::fs::remove_file(&visible).expect("create anchor-only sweep fixture");
        } else if visible_only {
            std::fs::remove_file(&anchor).expect("create visible-only sweep fixture");
        }
        let source = if anchor_only {
            anchor.clone()
        } else {
            visible.clone()
        };
        let source_bytes = std::fs::read(&source).expect("sweep source bytes");
        let paired = half
            .is_none()
            .then(|| std::fs::read(&anchor).expect("paired sweep anchor"));
        let directory = crate::daemons::state::StableDirectory::open(
            source.parent().expect("sweep source parent"),
        )
        .expect("sweep source directory");
        let quarantine_prefix = if anchor_only {
            ".nib-subagent-owner-anchor-delete-"
        } else {
            ".nib-subagent-owner-visible-delete-"
        };
        let quarantine = directory
            .deterministic_artifact_path(&source, quarantine_prefix, ".quarantine")
            .expect("sweep quarantine");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let project_root = root.path().to_path_buf();
        let worker_source = source.clone();
        let worker_quarantine = quarantine.clone();
        let worker = std::thread::spawn(move || {
            let mut paused = false;
            sweep_owner_lease_artifacts_with_timeout_and_guard(
                &project_root,
                &std::collections::HashSet::new(),
                Duration::from_millis(150),
                || {
                    if !paused && worker_quarantine.exists() && !worker_source.exists() {
                        paused = true;
                        ready_tx.send(()).expect("publish sweep quarantine pause");
                        resume_rx.recv().expect("resume owner sweep");
                    }
                    Ok(())
                },
            )
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("sweep reached its final deletion boundary");
        let quarantined = source_bytes;
        std::thread::sleep(Duration::from_millis(200));
        resume_tx.send(()).expect("resume expired owner sweep");
        let error = worker
            .join()
            .expect("owner sweep worker")
            .expect_err("expired owner sweep must fail closed");
        assert!(
            error.contains("timed out acquiring delegation state lock"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&quarantine).expect("retained sweep quarantine"),
            quarantined
        );
        if let Some(paired) = paired {
            assert_eq!(
                std::fs::read(&anchor).expect("retained sweep anchor"),
                paired
            );
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            std::fs::read(&quarantine).expect("later sweep quarantine"),
            quarantined
        );
        sweep_owner_lease_artifacts_with_timeout_and_guard(
            root.path(),
            &std::collections::HashSet::new(),
            Duration::from_secs(2),
            || Ok(()),
        )
        .expect("fresh sweep finishes retained quarantine deletion");
        assert!(!visible.exists(), "fresh sweep removed visible owner state");
        assert!(!anchor.exists(), "fresh sweep removed anchor owner state");
        assert!(
            !quarantine.exists(),
            "fresh sweep removed the retained quarantine"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn sweep_pair_and_both_half_deletions_stop_at_expired_quarantines() {
        sweep_owner_quarantine_expiry_fixture(None);
        sweep_owner_quarantine_expiry_fixture(Some(true));
        sweep_owner_quarantine_expiry_fixture(Some(false));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn precommit_record_cleanup_stops_at_its_quarantine_after_expiry() {
        let root = tempfile::tempdir().expect("root");
        let record = record_fixture(root.path(), "sub-precommit-expiry", "running");
        write_subagent_record(root.path(), &record).expect("initial record");
        let path = record_path(root.path(), &record.id).expect("record path");
        let expected = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("expected record");
        let directory =
            crate::daemons::state::StableDirectory::open(path.parent().expect("record parent"))
                .expect("record directory");
        let quarantine = directory
            .deterministic_artifact_path(&path, ".nib-subagent-precommit-delete-", ".quarantine")
            .expect("precommit quarantine");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let project_root = root.path().to_path_buf();
        let worker_path = path.clone();
        let worker_quarantine = quarantine.clone();
        let worker_record = record.clone();
        let worker_expected = expected.try_clone().expect("clone expected record");
        let worker = std::thread::spawn(move || {
            cleanup_precommit_record_with_timeout_and_hooks(
                &project_root,
                &worker_record,
                Some(&worker_expected),
                Duration::from_millis(150),
                || Ok(()),
                || {
                    if worker_quarantine.exists() && !worker_path.exists() {
                        ready_tx
                            .send(())
                            .expect("publish precommit quarantine pause");
                        resume_rx.recv().expect("resume precommit cleanup");
                    }
                    Ok(())
                },
            )
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("precommit cleanup reached its final deletion boundary");
        let quarantined = std::fs::read(&quarantine).expect("precommit quarantine bytes");
        std::thread::sleep(Duration::from_millis(200));
        resume_tx
            .send(())
            .expect("resume expired precommit cleanup");
        let error = worker
            .join()
            .expect("precommit cleanup worker")
            .expect_err("expired precommit cleanup must fail closed");
        assert!(
            error.contains("timed out acquiring delegation state lock"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&quarantine).expect("retained precommit quarantine"),
            quarantined
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            std::fs::read(&quarantine).expect("later precommit quarantine"),
            quarantined
        );
        cleanup_precommit_record_with_timeout_and_hooks(
            root.path(),
            &record,
            Some(&expected),
            Duration::from_secs(2),
            || Ok(()),
            || Ok(()),
        )
        .expect("fresh precommit cleanup finishes retained quarantine deletion");
        assert!(!path.exists(), "fresh cleanup leaves no canonical record");
        assert!(
            !quarantine.exists(),
            "fresh cleanup removes the retained precommit quarantine"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn initial_and_revision_publications_stop_before_namespace_mutation_after_expiry() {
        let operation_timeout = if cfg!(windows) {
            Duration::from_secs(2)
        } else {
            Duration::from_millis(150)
        };
        let expiry_delay = operation_timeout + Duration::from_millis(50);
        let boundary_wait = if cfg!(windows) {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(2)
        };
        let root = tempfile::tempdir().expect("root");
        let initial = record_fixture(root.path(), "sub-initial-publication-expiry", "running");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let project_root = root.path().to_path_buf();
        let worker_initial = initial.clone();
        let worker = std::thread::spawn(move || {
            let mut paused = false;
            write_subagent_record_with_refresh_hooks_and_timeout(
                &project_root,
                &worker_initial,
                operation_timeout,
                || {
                    if !paused {
                        paused = true;
                        ready_tx.send(()).expect("publish initial record pause");
                        resume_rx.recv().expect("resume initial publication");
                    }
                    Ok(())
                },
                || Ok(()),
            )
        });
        ready_rx
            .recv_timeout(boundary_wait)
            .expect("initial publication reached atomic namespace boundary");
        let records = records_dir(root.path());
        let before_initial = subagent_namespace_snapshot(&records);
        std::thread::sleep(expiry_delay);
        resume_tx
            .send(())
            .expect("resume expired initial publication");
        let error = match worker.join().expect("initial publication worker") {
            Ok(_) => panic!("expired initial publication must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .message
                .contains("timed out acquiring delegation state lock"),
            "{}",
            error.message
        );
        assert_eq!(subagent_namespace_snapshot(&records), before_initial);
        assert!(!record_path(root.path(), &initial.id)
            .expect("initial path")
            .exists());

        let mut revised = record_fixture(root.path(), "sub-revision-publication-expiry", "running");
        write_subagent_record(root.path(), &revised).expect("revision base record");
        let revised_path = record_path(root.path(), &revised.id).expect("revision path");
        let expected = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&revised_path)
            .expect("revision identity");
        revised.status = "failed".to_string();
        revised.error = Some("must not publish".to_string());
        revised.updated_at = Utc::now();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let project_root = root.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            let mut expected = expected;
            let mut paused = false;
            persist_subagent_record_revision_with_refresh_hooks_and_timeout(
                &project_root,
                &revised,
                &mut expected,
                operation_timeout,
                || {
                    if !paused {
                        paused = true;
                        ready_tx.send(()).expect("publish revision record pause");
                        resume_rx.recv().expect("resume revision publication");
                    }
                    Ok(())
                },
                || Ok(()),
            )
        });
        ready_rx
            .recv_timeout(boundary_wait)
            .expect("revision publication reached atomic namespace boundary");
        let before_revision = subagent_namespace_snapshot(&records);
        std::thread::sleep(expiry_delay);
        resume_tx
            .send(())
            .expect("resume expired revision publication");
        let error = worker
            .join()
            .expect("revision publication worker")
            .expect_err("expired revision publication must fail closed");
        assert!(
            error.contains("timed out acquiring delegation state lock"),
            "{error}"
        );
        assert_eq!(subagent_namespace_snapshot(&records), before_revision);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(subagent_namespace_snapshot(&records), before_revision);
    }

    #[cfg(any(unix, windows))]
    fn owner_cleanup_quarantine_expiry_fixture(half: Option<bool>) {
        let root = tempfile::tempdir().expect("root");
        let lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let execution_generation = lease.execution_generation;
        let lease_id = lease.lease_id.clone();
        let visible = lease.visible_path.clone();
        let anchor = lease.anchor_path.clone();
        drop(lease);
        let anchor_only = half == Some(true);
        let visible_only = half == Some(false);
        if anchor_only {
            std::fs::remove_file(&visible).expect("create anchor-only fixture");
        } else if visible_only {
            std::fs::remove_file(&anchor).expect("create visible-only fixture");
        }
        let source = if anchor_only {
            anchor.clone()
        } else {
            visible.clone()
        };
        let source_bytes = std::fs::read(&source).expect("owner source bytes");
        let paired = half
            .is_none()
            .then(|| std::fs::read(&anchor).expect("paired anchor bytes"));
        let directory = crate::daemons::state::StableDirectory::open(
            source.parent().expect("owner artifact parent"),
        )
        .expect("owner artifact directory");
        let quarantine_prefix = if anchor_only {
            ".nib-subagent-owner-anchor-delete-"
        } else {
            ".nib-subagent-owner-visible-delete-"
        };
        let quarantine = directory
            .deterministic_artifact_path(&source, quarantine_prefix, ".quarantine")
            .expect("owner deletion quarantine");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let project_root = root.path().to_path_buf();
        let cleanup_quarantine = quarantine.clone();
        let cleanup_source = source.clone();
        let worker_lease_id = lease_id.clone();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(150);
            let mut paused = false;
            remove_persisted_owner_lease_until_with_guard(
                &project_root,
                execution_generation,
                &worker_lease_id,
                Some(deadline),
                || {
                    if !paused && cleanup_quarantine.exists() && !cleanup_source.exists() {
                        paused = true;
                        ready_tx.send(()).expect("publish owner quarantine pause");
                        resume_rx.recv().expect("resume owner cleanup");
                    }
                    Ok(())
                },
            )
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("owner cleanup reached its final quarantine");
        assert!(!source.exists(), "owner source was quarantined");
        assert!(quarantine.is_file(), "recoverable quarantine is retained");
        if half.is_none() {
            assert!(anchor.is_file(), "the paired anchor remains authoritative");
        }
        std::thread::sleep(Duration::from_millis(200));
        let quarantined = source_bytes;
        resume_tx.send(()).expect("resume expired owner cleanup");

        let error = worker
            .join()
            .expect("owner cleanup worker")
            .expect_err("expired owner cleanup must fail closed");
        assert!(error.contains("deadline elapsed"), "{error}");
        assert_eq!(
            std::fs::read(&quarantine).expect("retained owner quarantine"),
            quarantined
        );
        if let Some(paired) = paired {
            assert_eq!(
                std::fs::read(&anchor).expect("retained paired anchor"),
                paired
            );
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            std::fs::read(&quarantine).expect("later retained owner quarantine"),
            quarantined,
            "expired cleanup mutated its recoverable quarantine after returning"
        );
        remove_persisted_owner_lease_until(
            root.path(),
            execution_generation,
            &lease_id,
            Some(Instant::now() + Duration::from_secs(2)),
        )
        .expect("fresh owner cleanup finishes retained quarantine deletion");
        assert!(
            !visible.exists(),
            "fresh cleanup removes visible owner state"
        );
        assert!(!anchor.exists(), "fresh cleanup removes anchor owner state");
        assert!(
            !quarantine.exists(),
            "fresh cleanup removes the retained owner quarantine"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn paired_owner_cleanup_stops_at_its_quarantine_when_deadline_expires() {
        owner_cleanup_quarantine_expiry_fixture(None);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn half_owner_cleanup_stops_at_its_quarantine_when_deadline_expires() {
        owner_cleanup_quarantine_expiry_fixture(Some(true));
        owner_cleanup_quarantine_expiry_fixture(Some(false));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn owner_cleanup_rejects_ambiguous_and_live_retained_quarantines() {
        let root = tempfile::tempdir().expect("root");
        let lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let execution_generation = lease.execution_generation;
        let lease_id = lease.lease_id.clone();
        let visible = lease.visible_path.clone();
        let anchor = lease.anchor_path.clone();
        let visible_directory =
            crate::daemons::state::StableDirectory::open(visible.parent().expect("visible parent"))
                .expect("visible directory");
        let quarantine = visible_directory
            .deterministic_artifact_path(
                &visible,
                ".nib-subagent-owner-visible-delete-",
                ".quarantine",
            )
            .expect("visible quarantine");
        std::fs::rename(&visible, &quarantine).expect("retain live visible quarantine");

        let error = remove_persisted_owner_lease_until(
            root.path(),
            execution_generation,
            &lease_id,
            Some(Instant::now() + Duration::from_secs(1)),
        )
        .expect_err("a live quarantined owner must remain unresolved");
        assert!(error.contains("still live"), "{error}");
        assert!(quarantine.exists());
        assert!(anchor.exists());

        drop(lease);
        std::fs::write(&visible, b"replacement").expect("ambiguous visible replacement");
        let error = remove_persisted_owner_lease_until(
            root.path(),
            execution_generation,
            &lease_id,
            Some(Instant::now() + Duration::from_secs(1)),
        )
        .expect_err("a mismatched canonical/quarantine pair must fail closed");
        assert!(error.contains("different identities"), "{error}");
        assert_eq!(
            std::fs::read(&visible).expect("replacement visible"),
            b"replacement"
        );
        assert!(quarantine.exists());
        assert!(anchor.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn legacy_lock_migration_retries_a_retained_deletion_quarantine() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let legacy_locks = records.join(".locks");
        std::fs::create_dir(&legacy_locks).expect("legacy locks");
        let id = "sub-legacy-quarantine-retry";
        let visible = legacy_locks.join(format!("{id}.lock"));
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor path");
        std::fs::write(&visible, b"legacy-lock").expect("legacy visible");
        std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
        let visible_directory =
            crate::daemons::state::StableDirectory::open(&legacy_locks).expect("legacy directory");
        let anchor_directory =
            crate::daemons::state::StableDirectory::open(&records).expect("record directory");
        let quarantine = visible_directory
            .deterministic_artifact_path(&visible, ".nib-legacy-lock-delete-", ".quarantine")
            .expect("legacy quarantine");
        let deadline = Instant::now() + Duration::from_millis(150);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let worker_visible = visible.clone();
        let worker_quarantine = quarantine.clone();
        let worker = std::thread::spawn(move || {
            let mut paused = false;
            crate::daemons::state::cleanup_legacy_lock_pair_with_guard(
                &visible_directory,
                &worker_visible,
                &anchor_directory,
                &anchor,
                || {
                    if !paused && worker_quarantine.exists() && !worker_visible.exists() {
                        paused = true;
                        ready_tx.send(()).expect("publish legacy quarantine pause");
                        resume_rx.recv().expect("resume legacy cleanup");
                    }
                    ensure_subagent_reconciliation_deadline(Some(deadline))
                },
            )
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("legacy cleanup reached its quarantine boundary");
        std::thread::sleep(Duration::from_millis(200));
        resume_tx.send(()).expect("resume expired legacy cleanup");
        let error = worker
            .join()
            .expect("legacy cleanup worker")
            .expect_err("expired legacy cleanup must retain quarantine");
        assert!(error.contains("deadline elapsed"), "{error}");
        assert!(quarantine.exists());

        confirm_no_legacy_subagent_processes(root.path())
            .expect("fresh attestation finishes retained legacy cleanup");
        assert!(!visible.exists());
        assert!(!quarantine.exists());
        assert!(!crate::daemons::state::daemon_lock_anchor_path(&visible)
            .expect("legacy anchor")
            .exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn record_and_owner_namespace_locks_have_absolute_deadlines() {
        let root = tempfile::tempdir().expect("root");
        let records = ensure_records_directory(root.path()).expect("records");
        let record_lock = record_lock_path(root.path(), "sub-held-lock").expect("record lock");
        let record_anchor = crate::daemons::state::daemon_lock_anchor_path(&record_lock)
            .expect("record lock anchor");
        crate::fs_security::ensure_directory_without_symlinks(
            record_anchor.parent().expect("record anchor parent"),
        )
        .expect("record anchor directory");
        let held_record = open_repository_merge_lock_anchor(&record_lock, &record_anchor)
            .expect("record lock pair");
        held_record.try_lock().expect("hold record stripe");
        let started = Instant::now();
        let error = with_bounded_delegation_lock_in(
            &record_lock,
            &records,
            Duration::from_millis(75),
            |_, _| -> Result<(), String> {
                panic!("held record stripe must not run its operation")
            },
        )
        .expect_err("held record stripe must time out");
        assert!(error.contains("timed out acquiring delegation state lock"));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held_record);

        let owner_lock = owner_lease_namespace_lock_path(root.path());
        let owner_anchor =
            crate::daemons::state::daemon_lock_anchor_path(&owner_lock).expect("owner lock anchor");
        crate::fs_security::ensure_directory_without_symlinks(
            owner_anchor.parent().expect("owner anchor parent"),
        )
        .expect("owner anchor directory");
        let held_owner =
            open_repository_merge_lock_anchor(&owner_lock, &owner_anchor).expect("owner lock pair");
        held_owner.try_lock().expect("hold owner namespace");
        let started = Instant::now();
        let error = with_bounded_delegation_lock_in(
            &owner_lock,
            &root.path().join(".nib"),
            Duration::from_millis(75),
            |_, _| -> Result<(), String> {
                panic!("held owner namespace must not run its operation")
            },
        )
        .expect_err("held owner namespace must time out");
        assert!(error.contains("timed out acquiring delegation state lock"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn repository_merge_lock_replacement_child_process() {
        let Some(project_root) = std::env::var_os(MERGE_LOCK_CHILD_PROJECT_ROOT) else {
            return;
        };
        let expectation =
            std::env::var(MERGE_LOCK_CHILD_EXPECTATION).expect("child lock expectation");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("child runtime");
        let error = runtime
            .block_on(RepositoryMergeLock::acquire_with_timeout(
                Path::new(&project_root),
                Duration::from_secs(2),
            ))
            .expect_err("replacement must not create a second repository lock domain");
        match expectation.as_str() {
            "timeout" => assert!(
                error.contains("timed out acquiring repository merge lock"),
                "{error}"
            ),
            "identity" => assert!(
                error.contains("persistent anchor have different identities"),
                "{error}"
            ),
            "offline" => assert!(error.contains("confirm-no-legacy-processes"), "{error}"),
            value => panic!("unsupported child expectation: {value}"),
        }
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn persistent_anchor_prevents_replaced_repository_lock_domains() {
        let root = tempfile::tempdir().expect("root");
        let held = RepositoryMergeLock::acquire_with_timeout(root.path(), Duration::from_secs(1))
            .await
            .expect("held persistent repository merge lock");

        let run_child = |expectation: &str| {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "tools::delegation::tests::repository_merge_lock_replacement_child_process",
                    "--nocapture",
                ])
                .env(MERGE_LOCK_CHILD_PROJECT_ROOT, root.path())
                .env(MERGE_LOCK_CHILD_EXPECTATION, expectation)
                .output()
                .expect("run repository merge lock child process");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run_child("timeout");

        let records_path = records_dir(root.path());
        let lock_path = records_path.join(".merge.lock");
        let displaced_lock = records_path.join(".merge.lock.displaced");
        std::fs::rename(&lock_path, &displaced_lock).expect("displace visible lock path");
        std::fs::write(&lock_path, b"replacement").expect("replace visible lock path");
        run_child("identity");

        std::fs::remove_file(&lock_path).expect("remove replacement lock path");
        std::fs::rename(&displaced_lock, &lock_path).expect("restore anchored lock path");

        let displaced_records = root.path().join(".nib/subagents.displaced");
        #[cfg(unix)]
        {
            std::fs::rename(&records_path, &displaced_records)
                .expect("displace subagent records directory");
            std::fs::create_dir(&records_path).expect("replace subagent records directory");
            run_child("offline");
            std::fs::remove_dir_all(&records_path).expect("remove replacement records directory");
            std::fs::rename(&displaced_records, &records_path)
                .expect("restore subagent records directory");
        }
        #[cfg(windows)]
        {
            std::fs::rename(&records_path, &displaced_records)
                .expect_err("the locked Windows record inode must pin its parent directory");
            run_child("timeout");
        }

        drop(held);
        RepositoryMergeLock::acquire_with_timeout(root.path(), Duration::from_millis(100))
            .await
            .expect("restored persistent lock identity remains usable");
    }

    #[cfg(unix)]
    #[test]
    fn repository_merge_lock_rejects_path_replacement_after_open() {
        let root = tempfile::tempdir().expect("root");
        let directory = ensure_records_directory(root.path()).expect("record directory");
        let path = directory.join(".merge.lock");
        let opened = open_repository_merge_lock(&path).expect("opened lock");
        let displaced = directory.join(".merge.lock.displaced");
        std::fs::rename(&path, &displaced).expect("displace opened lock");
        std::fs::write(&path, b"replacement").expect("replacement lock");

        let error = validate_repository_lock_path(&opened, &path)
            .expect_err("replacement must change identity");

        assert!(error.contains("changed while it was acquired"));
    }

    #[tokio::test]
    async fn merge_rejects_child_edit_after_immutable_verification_snapshot() {
        fn git(root: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .expect("git starts");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = tempfile::tempdir().expect("root");
        git(root.path(), &["init", "-q"]);
        git(
            root.path(),
            &["config", "user.email", "nib@example.invalid"],
        );
        git(root.path(), &["config", "user.name", "nib"]);
        std::fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
        git(root.path(), &["add", "README.md"]);
        git(root.path(), &["commit", "-qm", "initial"]);
        let worktree = crate::sandbox::worktree::Worktree::create(root.path(), "sub-post-verify")
            .expect("worktree");
        std::fs::write(worktree.path.join("result.txt"), "verified\n").expect("result");
        let record = SubagentRecord {
            id: "sub-post-verify".to_string(),
            parent_session_id: Some("parent".to_string()),
            child_session_id: "child".to_string(),
            prompt: "fixture".to_string(),
            status: "completed".to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: Some(worktree.branch_oid.clone()),
            result: Some(json!({"summary": "done"})),
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        write_subagent_record(root.path(), &record).expect("record");
        let target = prepare_subagent_verification_target(root.path(), &record.id, None)
            .await
            .expect("immutable verification target");
        let verified_commit = target.snapshot_commit.clone();
        assert_eq!(
            get_subagent_record(root.path(), &record.id)
                .expect("snapshot ownership record")
                .branch_oid
                .as_deref(),
            Some(verified_commit.as_str())
        );
        std::fs::write(
            worktree.path.join("result.txt"),
            "edited after verification\n",
        )
        .expect("post-verification edit");
        let evidence = VerificationEvidence {
            tool_name: "run_terminal".to_string(),
            command: "true".to_string(),
            worktree_path: target.worktree_path.clone(),
            success: true,
            output: Some(json!({
                "command": "true",
                "exit_code": 0,
                "cwd": target.worktree_path,
                "provider": "internal",
            })),
            error: None,
            approval_granted: true,
            approval_source: Some("user".to_string()),
            duration_seconds: 0.01,
            configured_provider: "internal".to_string(),
            sandbox_profile: "internal".to_string(),
            boundaries: BoundaryConfig::default(),
            session_id: Some("parent".to_string()),
            snapshot_commit: Some(verified_commit.clone()),
            executed_at: Utc::now(),
        };

        let error = merge_verified_subagent_worktree(
            &json!({
                "subagent_id": record.id,
                "verification_command": "true",
            }),
            root.path(),
            evidence,
            None,
        )
        .await
        .expect_err("post-verification edit must fail closed");

        assert!(error.contains("mergeable changes after verification"));
        assert!(error.contains("fresh verification"));
        assert!(!root.path().join("result.txt").exists());
        let snapshot = std::process::Command::new("git")
            .current_dir(&worktree.path)
            .args(["show", &format!("{verified_commit}:result.txt")])
            .output()
            .expect("inspect snapshot");
        assert!(snapshot.status.success());
        assert_eq!(String::from_utf8_lossy(&snapshot.stdout), "verified\n");
        assert_eq!(
            get_subagent_record(root.path(), "sub-post-verify")
                .expect("failed record")
                .status,
            "verification_failed"
        );
    }

    #[test]
    fn sync_spawn_compensation_does_not_short_circuit_after_record_failure() {
        let calls = std::cell::RefCell::new(Vec::new());
        let errors = collect_spawn_compensation_sync(
            || {
                calls.borrow_mut().push("record");
                Err("record sentinel".to_string())
            },
            || {
                calls.borrow_mut().push("worktree");
                Err("worktree sentinel".to_string())
            },
            |action| {
                assert_eq!(action, OwnerLeaseCompensation::ReleaseForReconciliation);
                calls.borrow_mut().push("lease");
                Err("lease sentinel".to_string())
            },
        );

        assert_eq!(*calls.borrow(), ["record", "worktree", "lease"]);
        assert_eq!(errors.len(), 3);
        assert!(errors[0].contains("record sentinel"));
        assert!(errors[1].contains("worktree sentinel"));
        assert!(errors[2].contains("lease sentinel"));
    }

    #[tokio::test]
    async fn async_spawn_compensation_does_not_short_circuit_after_record_failure() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record_calls = calls.clone();
        let worktree_calls = calls.clone();
        let lease_calls = calls.clone();
        let errors = collect_spawn_compensation_async(
            move || {
                record_calls.lock().expect("calls").push("record");
                Err("record sentinel".to_string())
            },
            async move {
                worktree_calls.lock().expect("calls").push("worktree");
                Err("worktree sentinel".to_string())
            },
            move |action| {
                assert_eq!(action, OwnerLeaseCompensation::ReleaseForReconciliation);
                lease_calls.lock().expect("calls").push("lease");
                Err("lease sentinel".to_string())
            },
        )
        .await;

        assert_eq!(
            *calls.lock().expect("calls"),
            ["record", "lease", "worktree"]
        );
        assert_eq!(errors.len(), 3);
        assert!(errors[0].contains("record sentinel"));
        assert!(errors[1].contains("worktree sentinel"));
        assert!(errors[2].contains("lease sentinel"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_async_spawn_compensation_still_runs_every_cleanup() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record_calls = calls.clone();
        let lease_calls = calls.clone();
        let worktree_calls = calls.clone();
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let (finished_sender, finished_receiver) = tokio::sync::oneshot::channel();
        let worktree_cleanup = async move {
            tokio::task::spawn_blocking(move || {
                worktree_calls.lock().expect("calls").push("worktree");
                let _ = started_sender.send(());
                let _ = release_receiver.recv();
                let _ = finished_sender.send(());
                Ok(())
            })
            .await
            .map_err(|error| error.to_string())?
        };
        let compensation = tokio::spawn(collect_spawn_compensation_async(
            move || {
                record_calls.lock().expect("calls").push("record");
                Ok(())
            },
            worktree_cleanup,
            move |action| {
                assert_eq!(action, OwnerLeaseCompensation::Remove);
                lease_calls.lock().expect("calls").push("lease");
                Ok(())
            },
        ));
        started_receiver.await.expect("worktree cleanup starts");

        compensation.abort();
        assert!(compensation
            .await
            .expect_err("compensation task cancellation")
            .is_cancelled());
        release_sender.send(()).expect("release worktree cleanup");
        finished_receiver.await.expect("worktree cleanup finishes");

        let calls = calls.lock().expect("calls").clone();
        assert_eq!(calls, ["record", "lease", "worktree"]);
    }

    #[test]
    fn failed_record_compensation_preserves_an_unlockable_owner_lease() {
        let root = tempfile::tempdir().expect("root");
        let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
        let execution_generation = owner_lease.execution_generation;
        let lease_id = owner_lease.lease_id.clone();

        let errors = collect_spawn_compensation_sync(
            || Err("record remains durable".to_string()),
            || Ok(()),
            |action| compensate_owner_lease(owner_lease, action),
        );

        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("record remains durable"));
        let OwnerLeaseProbe::Acquired(reconciler) =
            SubagentOwnerLease::probe(root.path(), execution_generation, &lease_id)
                .expect("preserved lease remains probeable")
        else {
            panic!("released compensation lease must be unlockable");
        };
        reconciler.remove().expect("cleanup preserved lease");
    }

    #[test]
    fn precommit_cleanup_preserves_a_moved_owned_branch() {
        fn git(root: &Path, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .expect("git starts");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        let root = tempfile::tempdir().expect("root");
        git(root.path(), &["init", "-q"]);
        git(
            root.path(),
            &["config", "user.email", "nib@example.invalid"],
        );
        git(root.path(), &["config", "user.name", "nib"]);
        std::fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
        git(root.path(), &["add", "README.md"]);
        git(root.path(), &["commit", "-qm", "initial"]);
        let worktree = crate::sandbox::worktree::Worktree::create(root.path(), "sub-moved-branch")
            .expect("worktree");
        std::fs::write(root.path().join("parent.txt"), "advanced\n").expect("parent change");
        git(root.path(), &["add", "parent.txt"]);
        git(root.path(), &["commit", "-qm", "advance parent"]);
        let moved_oid = git(root.path(), &["rev-parse", "HEAD"]);
        let reference = format!("refs/heads/{}", worktree.branch);
        git(
            root.path(),
            &["update-ref", reference.as_str(), moved_oid.as_str()],
        );

        let error = cleanup_precommit_worktree_sync(root.path(), &worktree)
            .expect_err("moved branch must be preserved");
        assert!(error.contains("identity changed"), "{error}");
        assert!(error.contains("preserving"), "{error}");
        assert!(!worktree.path.exists(), "owned worktree path was removed");
        assert_eq!(
            git(root.path(), &["show-ref", "--hash", "--verify", &reference]),
            moved_oid
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn delegation_accepts_a_dos_short_project_root_and_persists_canonical_ownership() {
        fn git(root: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .expect("git starts");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = tempfile::tempdir().expect("delegation DOS-alias repository");
        let _spawn_timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(30));
        let _timeout = SubagentCancellationTimeoutGuard::set(Duration::from_secs(30));
        git(root.path(), &["init", "-q"]);
        git(
            root.path(),
            &["config", "user.email", "nib@example.invalid"],
        );
        git(root.path(), &["config", "user.name", "nib"]);
        std::fs::write(root.path().join(".gitignore"), ".nib/\n").expect("gitignore");
        std::fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
        git(root.path(), &["add", ".gitignore", "README.md"]);
        git(root.path(), &["commit", "-qm", "initial"]);
        let mut config = crate::config::NibConfig::default();
        config.execution.plan_mode = false;
        crate::config::save_nib_config_full(root.path(), &mut config).expect("save config");

        let canonical_root = root.path().canonicalize().expect("canonical repository");
        let short_root = crate::fs_security::windows_dos_short_path_for_test(&canonical_root)
            .expect("DOS short project root");
        if short_root == crate::fs_security::path_without_windows_verbatim_prefix(&canonical_root) {
            return;
        }

        let started = spawn_subagent(
            &json!({"prompt": "Return a bounded fixture response.", "max_steps": 1}),
            &short_root,
        )
        .expect("delegate through DOS short root");
        let id = started["subagent_id"].as_str().expect("subagent id");
        let record = get_subagent_record(&short_root, id).expect("delegation record");
        assert!(record.worktree_path.starts_with(&canonical_root));
        assert!(!record.worktree_path.starts_with(&short_root));

        match resolve_subagent_cancellation_async(&short_root, id).await {
            CancelSubagentResolution::Cancelled { .. }
            | CancelSubagentResolution::Terminal { .. } => {}
            CancelSubagentResolution::Unresolved { error, .. } => {
                panic!("DOS-alias delegation cancellation was unresolved: {error}")
            }
        }
        crate::sandbox::worktree::Worktree::remove(&short_root, id)
            .expect("remove delegated worktree through DOS short root");
    }
}
