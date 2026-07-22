//! Durable execution scopes for independently supervised foreground processes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

// Version 2 records bind `direct_child` to the Linux namespace init rather than
// the outer bubblewrap monitor, so version 1 state must fail closed on recovery.
const PROCESS_SCOPE_VERSION: u32 = 2;
const MAX_SCOPE_RECORD_BYTES: u64 = 256 * 1024;
#[cfg(windows)]
const CLEANUP_LEASE_LOCK_OFFSET: u64 = MAX_SCOPE_RECORD_BYTES + 1;
const MAX_PROCESS_IDENTITY_MARKER_BYTES: usize = 1024;
const MAX_PROCESS_CLEANUP_TEXT_BYTES: usize = 32 * 1024;
const MAX_PROCESS_SCOPE_RECORDS: usize = 10_000;
const MAX_PROCESS_SCOPE_DIRECTORY_ENTRIES: usize = MAX_PROCESS_SCOPE_RECORDS * 3 + 512;
const MAX_PROCESS_SCOPE_DIRECTORY_NAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROCESS_SCOPE_DIRECTORY_BYTES: u64 = 64 * 1024 * 1024;
const SCOPE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const SCOPE_DIRECTORY: &str = "process-scopes";
const SCOPE_STORE_LOCK: &str = "process-scopes.lock";
const CLEANUP_LEASE_SUFFIX: &str = ".cleanup.lease";
const SCOPE_WRITE_PREFIX: &str = ".nib-daemon-";
const CLEANUP_LEASE_WRITE_PREFIX: &str = ".nib-process-cleanup-lease-write-";
const CLEANUP_LEASE_DELETE_PREFIX: &str = ".nib-process-cleanup-lease-delete-";
const SCOPE_DELETE_PREFIX: &str = ".nib-process-scope-delete-";
const LAUNCH_ABORT_OUTCOME: &str = "gate_eof_before_running";
const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SUPERVISOR_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SUPERVISED_OUTPUT_BYTES: usize = 1024 * 1024;
#[cfg(debug_assertions)]
const PRE_RUNNING_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_PRE_RUNNING_PAUSE";
#[cfg(debug_assertions)]
const PRE_SPAWN_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_PRE_SPAWN_PAUSE";
#[cfg(all(debug_assertions, target_os = "linux"))]
const POST_BWRAP_SPAWN_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_POST_BWRAP_SPAWN_PAUSE";
#[cfg(target_os = "linux")]
const MAX_BWRAP_INFO_BYTES: u64 = 16 * 1024;
#[cfg(target_os = "linux")]
const LINUX_LAUNCH_READY_FRAME: &[u8] = b"nib-ready\n";
#[cfg(target_os = "linux")]
const LINUX_LAUNCH_FRAME: &[u8] = b"nib-launch\n";
#[cfg(target_os = "linux")]
const LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS: usize = 3;
#[cfg(target_os = "linux")]
const LINUX_LAUNCH_GATE_SCRIPT: &str = "\
if ! printf 'nib-ready\\n'; then exit 125; fi
if ! IFS= read -r nib_gate; then exit 125; fi
if [ \"$nib_gate\" != 'nib-launch' ]; then exit 125; fi
exec \"$@\"";

#[derive(Clone, Copy)]
struct ProcessScopeDirectoryLimits {
    max_records: usize,
    max_entries: usize,
    max_name_bytes: usize,
    max_bytes: u64,
}

#[derive(Clone, Copy, Default)]
struct ProcessScopeDirectoryUsage {
    records: usize,
    entries: usize,
    name_bytes: usize,
    bytes: u64,
}

const PROCESS_SCOPE_DIRECTORY_LIMITS: ProcessScopeDirectoryLimits = ProcessScopeDirectoryLimits {
    max_records: MAX_PROCESS_SCOPE_RECORDS,
    max_entries: MAX_PROCESS_SCOPE_DIRECTORY_ENTRIES,
    max_name_bytes: MAX_PROCESS_SCOPE_DIRECTORY_NAME_BYTES,
    max_bytes: MAX_PROCESS_SCOPE_DIRECTORY_BYTES,
};

static MAINTAINED_PROCESS_SCOPE_STORES: LazyLock<Mutex<std::collections::HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessScopeBackend {
    LinuxPidNamespace,
    WindowsJobObject,
    MacosProcessGroup,
}

impl ProcessScopeBackend {
    /// Returns the containment primitive available to backend-specific code and
    /// native mechanism tests on this host.
    pub fn current() -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            crate::sandbox::require_managed_process_capability()?;
            Ok(Self::LinuxPidNamespace)
        }
        #[cfg(windows)]
        {
            Ok(Self::WindowsJobObject)
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Self::MacosProcessGroup)
        }
        #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
        {
            Err("managed foreground process scopes are unsupported on this platform".to_string())
        }
    }

    /// Returns the backend whose durable authority boundary is safe for an
    /// untrusted production worker.
    pub fn production() -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            Self::current()
        }
        #[cfg(windows)]
        {
            Err(
                "production subagent supervision is unavailable on Windows until process-scope state is isolated from the managed worker"
                    .to_string(),
            )
        }
        #[cfg(target_os = "macos")]
        {
            Err(
                "production subagent supervision is unavailable on macOS until cleanup authority is isolated from the managed worker"
                    .to_string(),
            )
        }
        #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
        {
            Err(
                "production managed-process supervision is unsupported on this platform"
                    .to_string(),
            )
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_marker: String,
}

impl ProcessIdentity {
    pub fn current() -> Result<Self, String> {
        Self::capture(std::process::id())
    }

    pub fn capture(pid: u32) -> Result<Self, String> {
        Ok(Self {
            pid,
            start_marker: platform_process_start_marker(pid)?,
        })
    }

    pub fn still_matches(&self) -> bool {
        Self::capture(self.pid)
            .map(|current| current == *self)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessScopeStatus {
    Prepared,
    Running,
    CleanupInProgress,
    Complete,
    RecoveryRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupLeaseState {
    Live,
    Recoverable,
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupProof {
    pub execution_generation: u64,
    pub cleanup_lease_id: String,
    pub backend: ProcessScopeBackend,
    pub direct_child: ProcessIdentity,
    pub outcome: String,
    pub descendants_reaped: bool,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchAbortProof {
    pub execution_generation: u64,
    pub cleanup_lease_id: String,
    pub backend: ProcessScopeBackend,
    pub supervisor: ProcessIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_root: Option<ProcessIdentity>,
    pub outcome: String,
    pub workload_never_launched: bool,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessScopeRecord {
    pub version: u32,
    pub scope_id: String,
    pub workload_kind: String,
    pub execution_generation: u64,
    pub cleanup_lease_id: String,
    pub owner: ProcessIdentity,
    pub backend: ProcessScopeBackend,
    pub status: ProcessScopeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor: Option<ProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_child: Option<ProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_proof: Option<CleanupProof>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_abort_proof: Option<LaunchAbortProof>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct SupervisedCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub stdin: Vec<u8>,
    pub environment: Vec<(OsString, OsString)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisedOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub owner_lost: bool,
    pub cancelled: bool,
    pub cleanup_proof: CleanupProof,
}

#[derive(Debug, Clone)]
pub struct ProcessScopeStore {
    project_root: PathBuf,
    directory: PathBuf,
}

enum CompletionAuthority<'a> {
    Cleanup(&'a CleanupProof),
    LaunchAbort(&'a LaunchAbortProof),
}

impl CompletionAuthority<'_> {
    fn matches(&self, record: &ProcessScopeRecord, execution_generation: u64) -> bool {
        match self {
            Self::Cleanup(proof) => {
                proof.execution_generation == execution_generation
                    && record.cleanup_proof.as_ref() == Some(*proof)
                    && record.launch_abort_proof.is_none()
            }
            Self::LaunchAbort(proof) => {
                proof.execution_generation == execution_generation
                    && record.launch_abort_proof.as_ref() == Some(*proof)
                    && record.cleanup_proof.is_none()
            }
        }
    }
}

impl ProcessScopeStore {
    pub fn open(project_root: &Path) -> Result<Self, String> {
        let project_root = project_root.canonicalize().map_err(|error| {
            format!(
                "failed to resolve managed-process project root {}: {error}",
                project_root.display()
            )
        })?;
        let nib = project_root.join(".nib");
        crate::fs_security::ensure_directory_without_symlinks(&nib)
            .map_err(|error| format!("managed-process state root is unsafe: {error}"))?;
        let directory = nib.join(SCOPE_DIRECTORY);
        crate::fs_security::ensure_directory_without_symlinks(&directory)
            .map_err(|error| format!("managed-process scope directory is unsafe: {error}"))?;
        let store = Self {
            project_root,
            directory,
        };
        store.maintain_once()?;
        Ok(store)
    }

    pub fn prepare(
        &self,
        scope_id: &str,
        workload_kind: &str,
        execution_generation: u64,
        owner: ProcessIdentity,
        backend: ProcessScopeBackend,
    ) -> Result<ProcessScopeRecord, String> {
        validate_scope_fields(scope_id, workload_kind, execution_generation)?;
        let now = Utc::now();
        let record = ProcessScopeRecord {
            version: PROCESS_SCOPE_VERSION,
            scope_id: scope_id.to_string(),
            workload_kind: workload_kind.to_string(),
            execution_generation,
            cleanup_lease_id: uuid::Uuid::new_v4().to_string(),
            owner,
            backend,
            status: ProcessScopeStatus::Prepared,
            supervisor: None,
            direct_child: None,
            cleanup_reason: None,
            cleanup_proof: None,
            launch_abort_proof: None,
            created_at: now,
            updated_at: now,
        };
        validate_record(&record)?;
        let encoded = encode_process_state_bounded(&record, "scope record")?;
        self.with_scope_lock(scope_id, |directory, path| {
            ensure_process_scope_publication_budget(
                directory,
                path,
                encoded.len() as u64,
                ProcessAtomicKind::Scope,
                PROCESS_SCOPE_DIRECTORY_LIMITS,
            )?;
            directory.save_bytes_atomically_expected(
                path,
                &encoded,
                SCOPE_WRITE_PREFIX,
                crate::daemons::state::FileExpectation::Missing,
            )?;
            Ok(record.clone())
        })
    }

    pub fn load(&self, scope_id: &str) -> Result<ProcessScopeRecord, String> {
        validate_scope_id(scope_id)?;
        self.with_scope_lock(scope_id, |directory, path| {
            read_scope_record(directory, path)
        })
    }

    pub fn try_load(&self, scope_id: &str) -> Result<Option<ProcessScopeRecord>, String> {
        validate_scope_id(scope_id)?;
        self.with_scope_lock(scope_id, |directory, path| {
            if !directory.path_exists(path)? {
                return Ok(None);
            }
            read_scope_record(directory, path).map(Some)
        })
    }

    pub fn cleanup_lease_state(
        &self,
        record: &ProcessScopeRecord,
    ) -> Result<CleanupLeaseState, String> {
        validate_record(record)?;
        let path = self.cleanup_lease_path(&record.scope_id)?;
        self.with_scope_lock(&record.scope_id, |directory, _| {
            if recover_cleanup_lease_deletion(directory, &path, record)? {
                return Ok(CleanupLeaseState::Live);
            }
            if !directory.path_exists(&path)? {
                return Ok(CleanupLeaseState::Missing);
            }
            let file = directory.open_read_write(&path)?;
            let observed: CleanupLeaseRecord = read_bounded_json(&file, &path)?;
            if observed.execution_generation != record.execution_generation
                || observed.cleanup_lease_id != record.cleanup_lease_id
                || observed.scope_id != record.scope_id
            {
                return Err(
                    "managed-process cleanup lease belongs to another generation".to_string(),
                );
            }
            match try_cleanup_lease_lock(&file) {
                Ok(()) => Ok(CleanupLeaseState::Recoverable),
                Err(std::fs::TryLockError::WouldBlock) => Ok(CleanupLeaseState::Live),
                Err(std::fs::TryLockError::Error(error)) => Err(format!(
                    "failed to inspect managed-process cleanup lease {}: {error}",
                    path.display()
                )),
            }
        })
    }

    pub fn remove_prepared(&self, expected: &ProcessScopeRecord) -> Result<(), String> {
        validate_record(expected)?;
        self.with_scope_lock(&expected.scope_id, |directory, path| {
            if recover_scope_deletion_for_expected(directory, path, expected)? {
                return Ok(());
            }
            let opened = directory.open_read(path)?;
            let observed: ProcessScopeRecord = read_bounded_json(&opened, path)?;
            validate_record(&observed)?;
            if observed != *expected || observed.status != ProcessScopeStatus::Prepared {
                return Err(
                    "managed-process scope changed before prepared-state cleanup; it was preserved"
                        .to_string(),
                );
            }
            let lease_path = self.cleanup_lease_path(&expected.scope_id)?;
            if directory.path_exists(&lease_path)? {
                return Err(
                    "managed-process cleanup lease exists; prepared scope was preserved"
                        .to_string(),
                );
            }
            directory.remove_file_if_matches(path, &opened, SCOPE_DELETE_PREFIX)
        })
    }

    pub(crate) fn retire_complete(
        &self,
        scope_id: &str,
        workload_execution_generation: u64,
        workload_proof: &CleanupProof,
    ) -> Result<bool, String> {
        self.retire_completed_scope(
            scope_id,
            workload_execution_generation,
            CompletionAuthority::Cleanup(workload_proof),
        )
    }

    pub(crate) fn retire_launch_abort(
        &self,
        scope_id: &str,
        workload_execution_generation: u64,
        workload_proof: &LaunchAbortProof,
    ) -> Result<bool, String> {
        self.retire_completed_scope(
            scope_id,
            workload_execution_generation,
            CompletionAuthority::LaunchAbort(workload_proof),
        )
    }

    fn retire_completed_scope(
        &self,
        scope_id: &str,
        workload_execution_generation: u64,
        workload_authority: CompletionAuthority<'_>,
    ) -> Result<bool, String> {
        validate_scope_id(scope_id)?;
        self.with_scope_lock(scope_id, |directory, path| {
            let quarantine =
                directory.deterministic_artifact_path(path, SCOPE_DELETE_PREFIX, ".quarantine")?;
            let source = if directory.path_exists(path)? {
                path
            } else if directory.path_exists(&quarantine)? {
                &quarantine
            } else {
                return Ok(false);
            };
            let opened = directory.open_read(source)?;
            let record: ProcessScopeRecord = read_bounded_json(&opened, source)?;
            validate_record(&record)?;
            if record.scope_id != scope_id
                || record.workload_kind != "subagent"
                || record.status != ProcessScopeStatus::Complete
            {
                return Err(
                    "managed-process scope retirement requires a completed exact subagent scope"
                        .to_string(),
                );
            }
            if record.execution_generation != workload_execution_generation
                || !workload_authority.matches(&record, workload_execution_generation)
            {
                return Err(
                    "terminal workload authority does not match its completed process scope"
                        .to_string(),
                );
            }
            let cleanup_lease = self.cleanup_lease_path(scope_id)?;
            if recover_cleanup_lease_deletion(directory, &cleanup_lease, &record)? {
                return Err(
                    "managed-process scope cannot retire while cleanup-lease finalization is live"
                        .to_string(),
                );
            }
            if directory.path_exists(&cleanup_lease)? {
                return Err(
                    "managed-process scope cannot retire while its cleanup lease exists"
                        .to_string(),
                );
            }
            if source == quarantine {
                directory.remove_visible_file_if_matches_direct(&quarantine, &opened)?;
            } else {
                directory.remove_file_if_matches(path, &opened, SCOPE_DELETE_PREFIX)?;
            }
            Ok(true)
        })
    }

    pub(crate) fn register_launch_supervisor(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        supervisor: ProcessIdentity,
    ) -> Result<ProcessScopeRecord, String> {
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            if record.status != ProcessScopeStatus::Prepared {
                return Err(format!(
                    "managed process scope cannot register its launch supervisor from status {:?}",
                    record.status
                ));
            }
            if record.direct_child.is_some() {
                return Err(
                    "managed process scope already has a registered launch identity".to_string(),
                );
            }
            match &record.supervisor {
                Some(expected) if expected == &supervisor => {}
                Some(_) => {
                    return Err(
                        "managed process scope already has another launch supervisor".to_string(),
                    );
                }
                None => record.supervisor = Some(supervisor),
            }
            Ok(())
        })
    }

    pub fn mark_running(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        supervisor: ProcessIdentity,
        direct_child: ProcessIdentity,
    ) -> Result<ProcessScopeRecord, String> {
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            if record.status != ProcessScopeStatus::Prepared {
                return Err(format!(
                    "managed process scope cannot start from status {:?}",
                    record.status
                ));
            }
            record.supervisor = Some(supervisor);
            record.direct_child = Some(direct_child);
            record.status = ProcessScopeStatus::Running;
            Ok(())
        })
    }

    fn register_gated_child(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        supervisor: ProcessIdentity,
        direct_child: ProcessIdentity,
    ) -> Result<ProcessScopeRecord, String> {
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            if record.status != ProcessScopeStatus::Prepared {
                return Err(format!(
                    "managed process scope cannot register a gated child from status {:?}",
                    record.status
                ));
            }
            if record.supervisor.as_ref() != Some(&supervisor) || record.direct_child.is_some() {
                return Err(
                    "managed process scope launch supervisor changed before child registration"
                        .to_string(),
                );
            }
            record.direct_child = Some(direct_child);
            Ok(())
        })
    }

    pub fn begin_cleanup(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        reason: impl Into<String>,
    ) -> Result<ProcessScopeRecord, String> {
        let reason = reason.into();
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            if !matches!(
                record.status,
                ProcessScopeStatus::Running
                    | ProcessScopeStatus::CleanupInProgress
                    | ProcessScopeStatus::RecoveryRequired
            ) {
                return Err(format!(
                    "managed process scope cannot begin cleanup from status {:?}",
                    record.status
                ));
            }
            record.status = ProcessScopeStatus::CleanupInProgress;
            record.cleanup_reason = Some(reason);
            Ok(())
        })
    }

    fn complete_cleanup(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        outcome: impl Into<String>,
        descendants_reaped: bool,
    ) -> Result<ProcessScopeRecord, String> {
        let outcome = outcome.into();
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            if !matches!(
                record.status,
                ProcessScopeStatus::Running | ProcessScopeStatus::CleanupInProgress
            ) {
                return Err(format!(
                    "managed process cleanup cannot complete from status {:?}",
                    record.status
                ));
            }
            let direct_child = record
                .direct_child
                .clone()
                .ok_or("managed process scope has no registered direct child")?;
            if !descendants_reaped {
                return Err(
                    "managed process scope cannot complete without descendant cleanup proof"
                        .to_string(),
                );
            }
            record.cleanup_proof = Some(CleanupProof {
                execution_generation,
                cleanup_lease_id: cleanup_lease_id.to_string(),
                backend: record.backend,
                direct_child,
                outcome,
                descendants_reaped,
                completed_at: Utc::now(),
            });
            record.status = ProcessScopeStatus::Complete;
            Ok(())
        })
    }

    #[cfg(target_os = "linux")]
    fn complete_launch_abort(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
    ) -> Result<ProcessScopeRecord, String> {
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            if record.status != ProcessScopeStatus::Prepared {
                return Err(format!(
                    "managed process launch abort cannot complete from status {:?}",
                    record.status
                ));
            }
            let supervisor = record
                .supervisor
                .clone()
                .ok_or("managed process launch abort has no supervisor identity")?;
            record.cleanup_reason = Some(LAUNCH_ABORT_OUTCOME.to_string());
            record.launch_abort_proof = Some(LaunchAbortProof {
                execution_generation,
                cleanup_lease_id: cleanup_lease_id.to_string(),
                backend: record.backend,
                supervisor,
                namespace_root: record.direct_child.clone(),
                outcome: LAUNCH_ABORT_OUTCOME.to_string(),
                workload_never_launched: true,
                completed_at: Utc::now(),
            });
            record.status = ProcessScopeStatus::Complete;
            Ok(())
        })
    }

    pub fn mark_recovery_required(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        reason: impl Into<String>,
    ) -> Result<ProcessScopeRecord, String> {
        let reason = reason.into();
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            if record.status == ProcessScopeStatus::Complete {
                return Ok(());
            }
            if record.status != ProcessScopeStatus::Prepared {
                record.status = ProcessScopeStatus::RecoveryRequired;
            }
            record.cleanup_reason = Some(reason);
            Ok(())
        })
    }

    /// Completes a crashed Linux supervisor scope only after the exact cleanup
    /// lease is recoverable and both recorded process generations are gone.
    #[cfg(target_os = "linux")]
    pub fn recover_linux_supervisor_loss(
        &self,
        expected: &ProcessScopeRecord,
    ) -> Result<ProcessScopeRecord, String> {
        validate_record(expected)?;
        if expected.backend != ProcessScopeBackend::LinuxPidNamespace {
            return Err(
                "automatic supervisor-loss recovery is supported only for Linux PID namespaces"
                    .to_string(),
            );
        }
        if !matches!(
            expected.status,
            ProcessScopeStatus::Prepared
                | ProcessScopeStatus::Running
                | ProcessScopeStatus::CleanupInProgress
                | ProcessScopeStatus::RecoveryRequired
        ) {
            return Err(format!(
                "managed process scope cannot recover supervisor loss from status {:?}",
                expected.status
            ));
        }
        let supervisor = expected
            .supervisor
            .as_ref()
            .ok_or("managed process recovery has no supervisor identity")?;
        let direct_child = expected.direct_child.as_ref();
        let cleanup_lease = match self.cleanup_lease_state(expected)? {
            CleanupLeaseState::Recoverable => self.acquire_cleanup_lease(expected)?,
            CleanupLeaseState::Missing
                if expected.status == ProcessScopeStatus::Prepared
                    && expected.direct_child.is_none() =>
            {
                if linux_identity_still_matches(supervisor)? {
                    return Err(
                        "managed process launch supervisor is still live before cleanup-lease creation"
                            .to_string(),
                    );
                }
                if self.load(&expected.scope_id)? != *expected {
                    return Err(
                        "managed process scope changed before prepared launch recovery".to_string(),
                    );
                }
                self.acquire_cleanup_lease(expected)?
            }
            CleanupLeaseState::Live => {
                return Err("managed process cleanup lease is still live".to_string());
            }
            CleanupLeaseState::Missing => {
                return Err(
                    "managed process cleanup lease is missing for a launched process scope"
                        .to_string(),
                );
            }
        };
        let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
        let mut namespace_init_signalled = false;
        loop {
            let supervisor_live = linux_identity_still_matches(supervisor)?;
            let direct_child_live = direct_child
                .map(linux_identity_still_matches)
                .transpose()?
                .unwrap_or(false);
            if !supervisor_live && direct_child_live && !namespace_init_signalled {
                if let Err(error) = signal_linux_process_identity(
                    direct_child.expect("live direct child has an identity"),
                ) {
                    let reason =
                        format!("failed to terminate recovered Linux namespace init: {error}");
                    if expected.status != ProcessScopeStatus::Prepared {
                        let _ = self.mark_recovery_required(
                            &expected.scope_id,
                            expected.execution_generation,
                            &expected.cleanup_lease_id,
                            &reason,
                        );
                    }
                    return Err(reason);
                }
                namespace_init_signalled = true;
                thread::sleep(SUPERVISOR_POLL_INTERVAL);
                continue;
            }
            if !supervisor_live && !direct_child_live {
                break;
            }
            if Instant::now() >= deadline {
                let reason = format!(
                    "Linux supervisor-loss recovery remained unproven (supervisor_live={supervisor_live}, direct_child_live={direct_child_live})"
                );
                if expected.status != ProcessScopeStatus::Prepared {
                    let _ = self.mark_recovery_required(
                        &expected.scope_id,
                        expected.execution_generation,
                        &expected.cleanup_lease_id,
                        &reason,
                    );
                }
                return Err(reason);
            }
            thread::sleep(SUPERVISOR_POLL_INTERVAL);
        }

        let launch_aborted = expected.status == ProcessScopeStatus::Prepared;
        let completed = if launch_aborted {
            self.complete_launch_abort(
                &expected.scope_id,
                expected.execution_generation,
                &expected.cleanup_lease_id,
            )?
        } else {
            let cleaning = self.begin_cleanup(
                &expected.scope_id,
                expected.execution_generation,
                &expected.cleanup_lease_id,
                "supervisor_lost_linux_pid_namespace",
            )?;
            self.complete_cleanup(
                &cleaning.scope_id,
                cleaning.execution_generation,
                &cleaning.cleanup_lease_id,
                "supervisor_lost_linux_pid_namespace",
                true,
            )?
        };
        let proof = completed.cleanup_proof.as_ref();
        if let Some(proof) = proof {
            cleanup_lease.release_after_proof(proof)?;
        } else {
            let proof = completed
                .launch_abort_proof
                .as_ref()
                .ok_or("recovered managed-process scope has no completion proof")?;
            cleanup_lease.release_after_launch_abort(proof)?;
        }
        Ok(completed)
    }

    /// Rejects persisted Linux recovery records on hosts that cannot prove Linux
    /// process identity or signal the namespace root.
    #[cfg(not(target_os = "linux"))]
    pub fn recover_linux_supervisor_loss(
        &self,
        expected: &ProcessScopeRecord,
    ) -> Result<ProcessScopeRecord, String> {
        validate_record(expected)?;
        if expected.backend != ProcessScopeBackend::LinuxPidNamespace {
            return Err(
                "automatic supervisor-loss recovery is supported only for Linux PID namespaces"
                    .to_string(),
            );
        }
        Err("Linux supervisor-loss recovery is unavailable on this platform".to_string())
    }

    pub fn acquire_cleanup_lease(
        &self,
        record: &ProcessScopeRecord,
    ) -> Result<CleanupLease, String> {
        validate_record(record)?;
        let path = self.cleanup_lease_path(&record.scope_id)?;
        let expected = CleanupLeaseRecord {
            version: PROCESS_SCOPE_VERSION,
            scope_id: record.scope_id.clone(),
            execution_generation: record.execution_generation,
            cleanup_lease_id: record.cleanup_lease_id.clone(),
        };
        validate_cleanup_lease_record(&expected)?;
        let encoded = encode_process_state_bounded(&expected, "cleanup lease")?;
        self.with_scope_lock(&record.scope_id, |directory, record_path| {
            let authoritative = read_scope_record(directory, record_path)?;
            if authoritative != *record {
                return Err(
                    "managed-process scope changed before cleanup-lease acquisition".to_string(),
                );
            }
            if recover_cleanup_lease_deletion(directory, &path, &authoritative)? {
                return Err(format!(
                    "managed-process cleanup lease is already live: {}",
                    path.display()
                ));
            }
            if directory.path_exists(&path)? {
                let file = directory.open_read_write(&path)?;
                let observed: CleanupLeaseRecord = read_bounded_json(&file, &path)?;
                if observed != expected {
                    return Err(
                        "managed-process cleanup lease belongs to another generation".to_string(),
                    );
                }
                acquire_file_lock(&file, &path)?;
                return Ok(CleanupLease {
                    directory: crate::daemons::state::StableDirectory::open(&self.directory)?,
                    path: path.clone(),
                    file: Some(file),
                    record: expected.clone(),
                });
            }
            ensure_process_scope_publication_budget(
                directory,
                &path,
                encoded.len() as u64,
                ProcessAtomicKind::CleanupLease,
                PROCESS_SCOPE_DIRECTORY_LIMITS,
            )?;
            let receipt = directory
                .save_bytes_atomically_expected_with_receipt(
                    &path,
                    &encoded,
                    CLEANUP_LEASE_WRITE_PREFIX,
                    crate::daemons::state::FileExpectation::Missing,
                )
                .map_err(|error| error.message)?;
            if !receipt.exact_identity {
                return Err(
                    "managed-process cleanup lease requires exact no-replace publication"
                        .to_string(),
                );
            }
            acquire_file_lock(&receipt.file, &path)?;
            Ok(CleanupLease {
                directory: crate::daemons::state::StableDirectory::open(&self.directory)?,
                path: path.clone(),
                file: Some(receipt.file),
                record: expected.clone(),
            })
        })
    }

    fn mutate(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        mutation: impl FnOnce(&mut ProcessScopeRecord) -> Result<(), String>,
    ) -> Result<ProcessScopeRecord, String> {
        validate_scope_id(scope_id)?;
        self.with_scope_lock(scope_id, |directory, path| {
            let opened = directory.open_read(path)?;
            let mut record: ProcessScopeRecord = read_bounded_json(&opened, path)?;
            validate_record(&record)?;
            if record.execution_generation != execution_generation
                || record.cleanup_lease_id != cleanup_lease_id
            {
                return Err("stale managed-process scope generation was rejected".to_string());
            }
            let previous = record.clone();
            mutation(&mut record)?;
            record.updated_at = Utc::now();
            validate_record(&record)?;
            validate_process_scope_transition(&previous, &record)?;
            let encoded = encode_process_state_bounded(&record, "scope record")?;
            ensure_process_scope_publication_budget(
                directory,
                path,
                encoded.len() as u64,
                ProcessAtomicKind::Scope,
                PROCESS_SCOPE_DIRECTORY_LIMITS,
            )?;
            directory.save_bytes_atomically_expected(
                path,
                &encoded,
                SCOPE_WRITE_PREFIX,
                crate::daemons::state::FileExpectation::Present(&opened),
            )?;
            Ok(record)
        })
    }

    fn with_scope_lock<T>(
        &self,
        scope_id: &str,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory, &Path) -> Result<T, String>,
    ) -> Result<T, String> {
        validate_scope_id(scope_id)?;
        let path = self.record_path(scope_id)?;
        self.with_store_lock(|directory| {
            recover_process_atomic_transaction(
                directory,
                &path,
                SCOPE_WRITE_PREFIX,
                ProcessAtomicKind::Scope,
            )?;
            let cleanup_lease = self.cleanup_lease_path(scope_id)?;
            recover_process_atomic_transaction(
                directory,
                &cleanup_lease,
                CLEANUP_LEASE_WRITE_PREFIX,
                ProcessAtomicKind::CleanupLease,
            )?;
            operation(directory, &path)
        })
    }

    fn with_store_lock<T>(
        &self,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, String>,
    ) -> Result<T, String> {
        let lock = self.project_root.join(".nib").join(SCOPE_STORE_LOCK);
        let deadline = Instant::now() + SCOPE_LOCK_TIMEOUT;
        crate::daemons::state::with_file_lock_in_until(&lock, &self.directory, deadline, operation)
    }

    fn maintain_once(&self) -> Result<(), String> {
        if MAINTAINED_PROCESS_SCOPE_STORES
            .lock()
            .map_err(|_| "managed-process maintenance registry is poisoned".to_string())?
            .contains(&self.directory)
        {
            return Ok(());
        }
        self.with_store_lock(|directory| maintain_process_scope_directory(directory, false))?;
        let mut maintained = MAINTAINED_PROCESS_SCOPE_STORES
            .lock()
            .map_err(|_| "managed-process maintenance registry is poisoned".to_string())?;
        if maintained.len() >= 1_024 {
            maintained.clear();
        }
        maintained.insert(self.directory.clone());
        Ok(())
    }

    fn record_path(&self, scope_id: &str) -> Result<PathBuf, String> {
        validate_scope_id(scope_id)?;
        Ok(self.directory.join(format!("{scope_id}.json")))
    }

    fn cleanup_lease_path(&self, scope_id: &str) -> Result<PathBuf, String> {
        validate_scope_id(scope_id)?;
        Ok(self
            .directory
            .join(format!("{scope_id}{CLEANUP_LEASE_SUFFIX}")))
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }
}

/// Runs one foreground root under the platform scope while independently watching
/// the interactive owner's EOF signal. This function is intended to execute in the
/// hidden supervisor process, not in the interactive owner.
pub fn supervise_foreground<R: Read + Send + 'static>(
    store: &ProcessScopeStore,
    prepared: &ProcessScopeRecord,
    owner_eof: R,
    command: SupervisedCommand,
) -> Result<SupervisedOutput, String> {
    supervise_foreground_with_ready(store, prepared, owner_eof, command, |_| Ok(()))
}

pub fn supervise_foreground_with_ready<R, F>(
    store: &ProcessScopeStore,
    prepared: &ProcessScopeRecord,
    owner_eof: R,
    command: SupervisedCommand,
    ready: F,
) -> Result<SupervisedOutput, String>
where
    R: Read + Send + 'static,
    F: FnOnce(&ProcessScopeRecord) -> Result<(), String>,
{
    validate_record(prepared)?;
    if prepared.status != ProcessScopeStatus::Prepared {
        return Err("managed-process supervisor requires a prepared scope".to_string());
    }
    if prepared.backend != ProcessScopeBackend::current()? {
        return Err("managed-process backend changed after scope preparation".to_string());
    }
    let cleanup_lease = store.acquire_cleanup_lease(prepared)?;
    supervise_foreground_with_claimed_cleanup(
        store,
        prepared,
        cleanup_lease,
        owner_eof,
        command,
        ready,
    )
}

#[doc(hidden)]
pub fn supervise_foreground_with_claimed_cleanup<R, F>(
    store: &ProcessScopeStore,
    prepared: &ProcessScopeRecord,
    cleanup_lease: CleanupLease,
    owner_eof: R,
    command: SupervisedCommand,
    ready: F,
) -> Result<SupervisedOutput, String>
where
    R: Read + Send + 'static,
    F: FnOnce(&ProcessScopeRecord) -> Result<(), String>,
{
    validate_record(prepared)?;
    if prepared.status != ProcessScopeStatus::Prepared {
        return Err("managed-process supervisor requires a prepared scope".to_string());
    }
    if prepared.backend != ProcessScopeBackend::current()? {
        return Err("managed-process backend changed after scope preparation".to_string());
    }
    if cleanup_lease.execution_generation() != prepared.execution_generation
        || cleanup_lease.cleanup_lease_id() != prepared.cleanup_lease_id
    {
        return Err("managed-process cleanup lease does not own the prepared scope".to_string());
    }
    let supervisor_identity = ProcessIdentity::current()?;
    let launching = store.register_launch_supervisor(
        &prepared.scope_id,
        prepared.execution_generation,
        &prepared.cleanup_lease_id,
        supervisor_identity.clone(),
    )?;
    pause_before_supervised_spawn(&supervisor_identity)?;
    let mut child_scope = spawn_supervised_command(prepared.backend, &command)?;
    let direct_child = child_scope.scope_root.clone();
    let gated = store.register_gated_child(
        &launching.scope_id,
        launching.execution_generation,
        &launching.cleanup_lease_id,
        supervisor_identity.clone(),
        direct_child.clone(),
    )?;
    pause_before_running_publication(&child_scope.scope_root)?;
    let running = store.mark_running(
        &gated.scope_id,
        gated.execution_generation,
        &gated.cleanup_lease_id,
        supervisor_identity,
        direct_child.clone(),
    )?;
    let (owner_eof_tx, owner_eof_rx) = mpsc::sync_channel(1);
    let owner_watcher = thread::Builder::new()
        .name(format!("nib-owner-eof-{}", prepared.scope_id))
        .spawn(move || {
            let mut owner_eof = owner_eof;
            let mut buffer = [0_u8; 1024];
            loop {
                match owner_eof.read(&mut buffer) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(0) | Err(_) => {
                        let _ = owner_eof_tx.send(false);
                        break;
                    }
                    Ok(_) => {
                        // The launch request has already been consumed. Any remaining
                        // owner-channel data is the cancellation control frame.
                        let _ = owner_eof_tx.send(true);
                        break;
                    }
                }
            }
        })
        .map_err(|error| format!("failed to start owner-EOF watcher: {error}"))?;
    let stdout = child_scope
        .child
        .stdout
        .take()
        .ok_or("supervised child stdout is unavailable")?;
    let stderr = child_scope
        .child
        .stderr
        .take()
        .ok_or("supervised child stderr is unavailable")?;
    let stdout_reader = spawn_bounded_reader(stdout, "stdout")?;
    let stderr_reader = spawn_bounded_reader(stderr, "stderr")?;
    ready(&running)?;

    let mut owner_lost = false;
    let mut cancelled = false;
    let mut pending_owner_signal = owner_eof_rx.try_recv().ok();
    let input_writer = if pending_owner_signal.is_none() {
        child_scope.release_launch_gate()?;
        let child_stdin = child_scope
            .child
            .stdin
            .take()
            .ok_or("supervised child stdin is unavailable")?;
        Some(spawn_input_writer(child_stdin, command.stdin.clone())?)
    } else {
        None
    };
    let exit_status = loop {
        if let Some(cancellation_requested) = pending_owner_signal
            .take()
            .or_else(|| owner_eof_rx.try_recv().ok())
        {
            cancelled = cancellation_requested;
            owner_lost = !cancellation_requested;
            store.begin_cleanup(
                &running.scope_id,
                running.execution_generation,
                &running.cleanup_lease_id,
                if cancellation_requested {
                    "owner_cancelled"
                } else {
                    "owner_eof"
                },
            )?;
            child_scope.terminate()?;
            break child_scope.wait_bounded()?;
        }
        match child_scope.observe_exit()? {
            ChildExitObservation::Exited(status) => {
                store.begin_cleanup(
                    &running.scope_id,
                    running.execution_generation,
                    &running.cleanup_lease_id,
                    "child_exit",
                )?;
                #[cfg(not(target_os = "linux"))]
                child_scope.terminate()?;
                break match status {
                    Some(status) => status,
                    None => child_scope.wait_bounded()?,
                };
            }
            ChildExitObservation::Running => thread::sleep(SUPERVISOR_POLL_INTERVAL),
        }
    };

    let input_error = match input_writer {
        Some(input_writer) => match input_writer.recv_timeout(SUPERVISOR_CLEANUP_TIMEOUT) {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(format!("supervised stdin writer did not finish: {error}")),
        },
        None => None,
    };
    let stdout = join_bounded_reader(stdout_reader, "stdout")?;
    let mut stderr = join_bounded_reader(stderr_reader, "stderr")?;
    if let Some(error) = &input_error {
        if !stderr.is_empty() {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(error.as_bytes());
    }
    drop(owner_watcher);
    let child_reaped = child_scope.verify_descendants_reaped()?;
    let outcome = if cancelled {
        "cancelled"
    } else if owner_lost {
        "owner_eof"
    } else if exit_status.success() && input_error.is_none() {
        "completed"
    } else {
        "child_failed"
    };
    let completed = store.complete_cleanup(
        &running.scope_id,
        running.execution_generation,
        &running.cleanup_lease_id,
        outcome,
        child_reaped,
    )?;
    let proof = completed
        .cleanup_proof
        .clone()
        .ok_or("completed managed-process scope has no cleanup proof")?;
    cleanup_lease.release_after_proof(&proof)?;
    Ok(SupervisedOutput {
        exit_code: exit_status.code(),
        stdout,
        stderr,
        owner_lost,
        cancelled,
        cleanup_proof: proof,
    })
}

#[cfg(debug_assertions)]
fn pause_before_running_publication(scope_root: &ProcessIdentity) -> Result<(), String> {
    let Some(marker) = std::env::var_os(PRE_RUNNING_PAUSE_ENV) else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(scope_root)
        .map_err(|error| format!("failed to encode pre-running test marker: {error}"))?;
    std::fs::write(&marker, bytes).map_err(|error| {
        format!(
            "failed to publish pre-running test marker {}: {error}",
            Path::new(&marker).display()
        )
    })?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(debug_assertions)]
fn pause_before_supervised_spawn(supervisor: &ProcessIdentity) -> Result<(), String> {
    let Some(marker) = std::env::var_os(PRE_SPAWN_PAUSE_ENV) else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(supervisor)
        .map_err(|error| format!("failed to encode pre-spawn test marker: {error}"))?;
    std::fs::write(&marker, bytes).map_err(|error| {
        format!(
            "failed to publish pre-spawn test marker {}: {error}",
            Path::new(&marker).display()
        )
    })?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(not(debug_assertions))]
fn pause_before_supervised_spawn(_supervisor: &ProcessIdentity) -> Result<(), String> {
    Ok(())
}

#[cfg(not(debug_assertions))]
fn pause_before_running_publication(_scope_root: &ProcessIdentity) -> Result<(), String> {
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
fn pause_after_bwrap_spawn(child: &std::process::Child) -> Result<(), String> {
    let Some(marker) = std::env::var_os(POST_BWRAP_SPAWN_PAUSE_ENV) else {
        return Ok(());
    };
    let identity = ProcessIdentity::capture(child.id())
        .map_err(|error| format!("failed to identify the pre-handshake bwrap monitor: {error}"))?;
    let bytes = serde_json::to_vec(&identity)
        .map_err(|error| format!("failed to encode post-spawn test marker: {error}"))?;
    std::fs::write(&marker, bytes).map_err(|error| {
        format!(
            "failed to publish post-spawn test marker {}: {error}",
            Path::new(&marker).display()
        )
    })?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(all(not(debug_assertions), target_os = "linux"))]
fn pause_after_bwrap_spawn(_child: &std::process::Child) -> Result<(), String> {
    Ok(())
}

fn spawn_input_writer(
    mut stdin: std::process::ChildStdin,
    input: Vec<u8>,
) -> Result<mpsc::Receiver<Result<(), String>>, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nib-supervisor-stdin".to_string())
        .spawn(move || {
            let result = stdin
                .write_all(&input)
                .and_then(|()| stdin.flush())
                .map_err(|error| format!("failed to write supervised child stdin: {error}"));
            drop(stdin);
            let _ = sender.send(result);
        })
        .map_err(|error| format!("failed to start supervised stdin writer: {error}"))?;
    Ok(receiver)
}

struct SupervisedChild {
    child: std::process::Child,
    backend: SupervisedBackendHandle,
    scope_root: ProcessIdentity,
    backend_cleanup_started: bool,
    #[cfg(windows)]
    windows_job_cleanup_verified: bool,
    direct_reaped: bool,
    cleanup_complete: bool,
}

enum SupervisedBackendHandle {
    #[cfg(target_os = "linux")]
    LinuxPidNamespace { monitor_group: i32 },
    #[cfg(target_os = "macos")]
    ProcessGroup(i32),
    #[cfg(windows)]
    WindowsJob(crate::sandbox::windows_job::WindowsJob),
}

impl SupervisedChild {
    fn release_launch_gate(&mut self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            let stdin = self
                .child
                .stdin
                .as_mut()
                .ok_or("supervised Linux launch gate stdin is unavailable")?;
            stdin.write_all(LINUX_LAUNCH_FRAME).map_err(|error| {
                format!("failed to release supervised Linux namespace init: {error}")
            })?;
            stdin.flush().map_err(|error| {
                format!("failed to flush supervised Linux launch gate: {error}")
            })?;
        }
        Ok(())
    }

    fn terminate(&mut self) -> Result<(), String> {
        self.backend_cleanup_started = true;
        #[cfg(target_os = "linux")]
        {
            let SupervisedBackendHandle::LinuxPidNamespace { monitor_group } = &self.backend;
            let result = cleanup_linux_namespace_and_monitor(
                &mut self.child,
                *monitor_group,
                &self.scope_root,
            );
            if self.child.try_wait().ok().flatten().is_some() {
                self.direct_reaped = true;
            }
            result
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut first_error = None;
            match &mut self.backend {
                #[cfg(target_os = "macos")]
                SupervisedBackendHandle::ProcessGroup(group) => {
                    if let Err(error) = signal_process_group(*group) {
                        first_error = Some(error);
                    }
                }
                #[cfg(windows)]
                SupervisedBackendHandle::WindowsJob(job) => {
                    match job.terminate_and_wait(SUPERVISOR_CLEANUP_TIMEOUT) {
                        Ok(()) => self.windows_job_cleanup_verified = true,
                        Err(error) => {
                            first_error = Some(format!(
                                "failed to terminate and verify Windows Job cleanup: {error}"
                            ));
                        }
                    }
                }
            }
            if !self.direct_reaped {
                match self.child.kill() {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
                    Err(error) => {
                        first_error.get_or_insert_with(|| {
                            format!("failed to terminate supervised child: {error}")
                        });
                    }
                }
            }
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    fn wait_bounded(&mut self) -> Result<std::process::ExitStatus, String> {
        let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.direct_reaped = true;
                    return Ok(status);
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(SUPERVISOR_POLL_INTERVAL);
                }
                Ok(None) => {
                    return Err(format!(
                        "supervised child did not exit within {} seconds",
                        SUPERVISOR_CLEANUP_TIMEOUT.as_secs()
                    ));
                }
                Err(error) => return Err(format!("failed to reap supervised child: {error}")),
            }
        }
    }

    fn observe_exit(&mut self) -> Result<ChildExitObservation, String> {
        #[cfg(unix)]
        {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if result == 0 && info.si_signo == libc::SIGCHLD {
                return Ok(ChildExitObservation::Exited(None));
            }
            if result == 0 && info.si_signo == 0 {
                return Ok(ChildExitObservation::Running);
            }
            if result != 0 {
                return Err(format!(
                    "failed to observe supervised child: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Err(format!(
                "supervised child returned unexpected wait signal {}",
                info.si_signo
            ))
        }
        #[cfg(windows)]
        {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.direct_reaped = true;
                    Ok(ChildExitObservation::Exited(Some(status)))
                }
                Ok(None) => Ok(ChildExitObservation::Running),
                Err(error) => Err(format!("failed to observe supervised child: {error}")),
            }
        }
    }

    fn verify_descendants_reaped(&mut self) -> Result<bool, String> {
        #[cfg(target_os = "linux")]
        let verified =
            wait_for_linux_identity_absent(&self.scope_root, SUPERVISOR_CLEANUP_TIMEOUT)?;
        #[cfg(target_os = "macos")]
        {
            if self.scope_root.still_matches() {
                return Ok(false);
            }
        }
        #[cfg(target_os = "macos")]
        let verified = {
            let SupervisedBackendHandle::ProcessGroup(group) = &self.backend;
            wait_for_process_group_empty(*group)?
        };
        #[cfg(windows)]
        let verified = {
            if !self.direct_reaped || !self.windows_job_cleanup_verified {
                return Ok(false);
            }
            let SupervisedBackendHandle::WindowsJob(job) = &mut self.backend;
            job.wait_until_empty(SUPERVISOR_CLEANUP_TIMEOUT)
                .map_err(|error| format!("failed to verify Windows Job cleanup: {error}"))?
        };
        self.cleanup_complete = verified;
        Ok(verified)
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if self.cleanup_complete {
            return;
        }
        #[cfg(target_os = "linux")]
        let backend_needs_cleanup =
            !self.direct_reaped || linux_identity_still_matches(&self.scope_root).unwrap_or(true);
        #[cfg(not(target_os = "linux"))]
        let backend_needs_cleanup = !self.backend_cleanup_started;
        if backend_needs_cleanup {
            let _ = self.terminate();
        }
        if !self.direct_reaped {
            let _ = self.wait_bounded();
        }
    }
}

enum ChildExitObservation {
    Running,
    Exited(Option<std::process::ExitStatus>),
}

fn spawn_supervised_command(
    backend: ProcessScopeBackend,
    command: &SupervisedCommand,
) -> Result<SupervisedChild, String> {
    spawn_supervised_command_inner(backend, command, false, true)
}

fn spawn_supervised_command_inner(
    backend: ProcessScopeBackend,
    command: &SupervisedCommand,
    inject_info_failure: bool,
    allow_post_spawn_pause: bool,
) -> Result<SupervisedChild, String> {
    if !command.cwd.is_absolute() || !command.cwd.is_dir() {
        return Err("supervised command cwd must be an existing absolute directory".to_string());
    }
    if !command.program.is_absolute() {
        return Err("supervised command program must be absolute".to_string());
    }

    #[cfg(target_os = "linux")]
    let (mut process, info_reader, info_writer) = {
        if backend != ProcessScopeBackend::LinuxPidNamespace {
            return Err("Linux supervisor received a non-Linux backend".to_string());
        }
        let (info_reader, info_writer) = create_cloexec_pipe("bubblewrap namespace information")?;
        let command_shell = crate::sandbox::command_shell_path()?;
        let info_write_fd = info_writer.as_raw_fd();
        let mut process = Command::new("bwrap");
        process.args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--die-with-parent",
            "--new-session",
            "--as-pid-1",
        ]);
        process
            .arg("--info-fd")
            .arg(info_write_fd.to_string())
            .arg("--bind");
        process.arg(&command.cwd).arg(&command.cwd).arg("--chdir");
        process
            .arg(&command.cwd)
            .arg("--")
            .arg(command_shell)
            .arg("-c")
            .arg(LINUX_LAUNCH_GATE_SCRIPT)
            .arg("nib-managed-launch-gate")
            .arg(&command.program);
        process.args(&command.args);
        (process, info_reader, info_writer)
    };
    #[cfg(target_os = "macos")]
    let mut process = {
        if backend != ProcessScopeBackend::MacosProcessGroup {
            return Err("macOS supervisor received a non-macOS backend".to_string());
        }
        let mut process = Command::new(&command.program);
        process.args(&command.args).current_dir(&command.cwd);
        process
    };
    #[cfg(windows)]
    let mut process = {
        if backend != ProcessScopeBackend::WindowsJobObject {
            return Err("Windows supervisor received a non-Windows backend".to_string());
        }
        let mut process = Command::new(&command.program);
        process.args(&command.args).current_dir(&command.cwd);
        process
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let mut process = {
        let _ = backend;
        let _ = inject_info_failure;
        let _ = allow_post_spawn_pause;
        return Err(
            "managed foreground process scopes are unsupported on this platform".to_string(),
        );
    };

    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process.envs(command.environment.iter().cloned());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let info_write_fd = info_writer.as_raw_fd();
        process.process_group(0);
        unsafe {
            process.pre_exec(move || {
                clear_close_on_exec(info_write_fd)?;
                Ok(())
            });
        }
        let mut child = process
            .spawn()
            .map_err(|error| format!("failed to start supervised command: {error}"))?;
        if allow_post_spawn_pause {
            if let Err(error) = pause_after_bwrap_spawn(&child) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
        drop(info_writer);
        let group = match i32::try_from(child.id()) {
            Ok(group) => group,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("supervised process identifier exceeds the Unix pid range".to_string());
            }
        };
        let namespace_pid = match read_bwrap_namespace_pid(info_reader) {
            Ok(pid) if !inject_info_failure => pid,
            Ok(_) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, None);
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(
                        "injected bubblewrap namespace information failure".to_string(),
                        &mut child,
                    ),
                    cleanup,
                ));
            }
            Err(error) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, None);
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(error, &mut child),
                    cleanup,
                ));
            }
        };
        let scope_root = match ProcessIdentity::capture(namespace_pid) {
            Ok(identity) => identity,
            Err(error) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, None);
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(
                        format!("failed to identify supervised Linux namespace init: {error}"),
                        &mut child,
                    ),
                    cleanup,
                ));
            }
        };
        if let Err(error) = validate_bwrap_namespace_init(&scope_root, child.id()) {
            let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
            return Err(append_linux_spawn_cleanup_error(
                append_linux_launch_diagnostics(error, &mut child),
                cleanup,
            ));
        }
        let ready = child
            .stdout
            .as_mut()
            .ok_or_else(|| "supervised Linux launch gate stdout is unavailable".to_string());
        if let Err(error) = ready.and_then(read_linux_launch_ready) {
            let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
            return Err(append_linux_spawn_cleanup_error(
                append_linux_launch_diagnostics(error, &mut child),
                cleanup,
            ));
        }
        match ProcessIdentity::capture(scope_root.pid) {
            Ok(current) if current == scope_root => {}
            Ok(_) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(
                        "supervised Linux namespace init changed identity during launch"
                            .to_string(),
                        &mut child,
                    ),
                    cleanup,
                ));
            }
            Err(error) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(
                        format!(
                            "failed to revalidate supervised Linux namespace init during launch: {error}"
                        ),
                        &mut child,
                    ),
                    cleanup,
                ));
            }
        }
        if let Err(error) = validate_bwrap_namespace_init(&scope_root, child.id()) {
            let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
            return Err(append_linux_spawn_cleanup_error(
                append_linux_launch_diagnostics(error, &mut child),
                cleanup,
            ));
        }
        Ok(SupervisedChild {
            child,
            backend: SupervisedBackendHandle::LinuxPidNamespace {
                monitor_group: group,
            },
            scope_root,
            backend_cleanup_started: false,
            direct_reaped: false,
            cleanup_complete: false,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let _ = inject_info_failure;
        let _ = allow_post_spawn_pause;
        use std::os::unix::process::CommandExt;
        process.process_group(0);
        let mut child = process
            .spawn()
            .map_err(|error| format!("failed to start supervised command: {error}"))?;
        let group = i32::try_from(child.id())
            .map_err(|_| "supervised process identifier exceeds the Unix pid range".to_string())?;
        let scope_root = ProcessIdentity::capture(child.id()).map_err(|error| {
            let _ = signal_process_group(group);
            let _ = child.kill();
            let _ = child.wait();
            format!("failed to identify supervised child: {error}")
        })?;
        Ok(SupervisedChild {
            child,
            backend: SupervisedBackendHandle::ProcessGroup(group),
            scope_root,
            backend_cleanup_started: false,
            direct_reaped: false,
            cleanup_complete: false,
        })
    }
    #[cfg(windows)]
    {
        let _ = inject_info_failure;
        let _ = allow_post_spawn_pause;
        let (mut child, mut job) =
            crate::sandbox::windows_job::spawn_contained_std(&mut process)
                .map_err(|error| format!("failed to start supervised command: {error}"))?;
        let scope_root = ProcessIdentity::capture(child.id()).map_err(|error| {
            job.terminate();
            let _ = child.kill();
            let _ = child.wait();
            format!("failed to identify supervised child: {error}")
        })?;
        Ok(SupervisedChild {
            child,
            backend: SupervisedBackendHandle::WindowsJob(job),
            scope_root,
            backend_cleanup_started: false,
            windows_job_cleanup_verified: false,
            direct_reaped: false,
            cleanup_complete: false,
        })
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn probe_linux_managed_process_backend() -> Result<(), String> {
    run_linux_managed_process_probe_attempts(probe_linux_managed_process_backend_once)
}

#[cfg(target_os = "linux")]
fn run_linux_managed_process_probe_attempts(
    mut probe: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for attempt in 1..=LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS {
        match probe() {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retryable = linux_managed_process_probe_failure_is_retryable(&error);
                failures.push(format!("attempt {attempt}: {error}"));
                if !retryable || attempt == LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS {
                    return Err(format!(
                        "managed-process containment probe failed after {attempt} attempt(s): {}",
                        failures.join(" | ")
                    ));
                }
            }
        }
    }
    unreachable!("managed-process probe attempt range is non-empty")
}

#[cfg(target_os = "linux")]
fn linux_managed_process_probe_failure_is_retryable(error: &str) -> bool {
    !error.contains("supervised launch cleanup was not proven")
        && !error.contains("bubblewrap stderr:")
        && [
            "launch gate closed before reporting readiness",
            "launch gate readiness timed out",
            "launch gate returned an invalid readiness frame",
        ]
        .iter()
        .any(|diagnostic| error.contains(diagnostic))
}

#[cfg(target_os = "linux")]
fn probe_linux_managed_process_backend_once() -> Result<(), String> {
    let shell = crate::sandbox::command_shell_path()?;
    let mut child = spawn_supervised_command_inner(
        ProcessScopeBackend::LinuxPidNamespace,
        &SupervisedCommand {
            program: shell,
            args: vec![OsString::from("-c"), OsString::from("sleep 60")],
            cwd: PathBuf::from("/tmp"),
            stdin: Vec::new(),
            environment: Vec::new(),
        },
        false,
        false,
    )?;

    child.release_launch_gate()?;
    thread::sleep(SUPERVISOR_POLL_INTERVAL);
    match child.observe_exit() {
        Ok(ChildExitObservation::Running) => {}
        Ok(ChildExitObservation::Exited(_)) => {
            return Err(
                "managed-process containment probe gate did not launch its command".to_string(),
            );
        }
        Err(error) => {
            return Err(format!(
                "failed to observe managed-process probe launch: {error}"
            ));
        }
    }
    let terminate_error = child.terminate().err();
    let wait_error = child.wait_bounded().err();
    let verify_error = match child.verify_descendants_reaped() {
        Ok(true) => None,
        Ok(false) => Some("managed-process probe did not reap its namespace".to_string()),
        Err(error) => Some(error),
    };
    if let Some(error) = terminate_error.or(wait_error).or(verify_error) {
        Err(error)
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn create_cloexec_pipe(label: &str) -> Result<(File, File), String> {
    let mut descriptors = [-1; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "failed to create {label} pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    let reader = unsafe { File::from_raw_fd(descriptors[0]) };
    let writer = unsafe { File::from_raw_fd(descriptors[1]) };
    Ok((reader, writer))
}

#[cfg(target_os = "linux")]
fn clear_close_on_exec(descriptor: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_linux_launch_ready(stdout: &mut std::process::ChildStdout) -> Result<(), String> {
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    let mut ready = [0_u8; LINUX_LAUNCH_READY_FRAME.len()];
    let mut offset = 0_usize;
    while offset < ready.len() {
        let now = Instant::now();
        if now >= deadline {
            return Err("supervised Linux launch gate readiness timed out".to_string());
        }
        let remaining = deadline.saturating_duration_since(now);
        let timeout = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result == 0 {
            return Err("supervised Linux launch gate readiness timed out".to_string());
        }
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!(
                "failed to wait for supervised Linux launch gate readiness: {error}"
            ));
        }
        match stdout.read(&mut ready[offset..]) {
            Ok(0) => {
                return Err(
                    "supervised Linux launch gate closed before reporting readiness".to_string(),
                )
            }
            Ok(read) => offset += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(format!(
                    "failed to read supervised Linux launch gate readiness: {error}"
                ))
            }
        }
    }
    if ready != LINUX_LAUNCH_READY_FRAME {
        return Err("supervised Linux launch gate returned an invalid readiness frame".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_bwrap_namespace_pid(reader: File) -> Result<u32, String> {
    #[derive(Deserialize)]
    struct BwrapInfo {
        #[serde(rename = "child-pid")]
        child_pid: u32,
    }

    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nib-bwrap-info".to_string())
        .spawn(move || {
            let result = (|| {
                let mut bytes = Vec::new();
                reader
                    .take(MAX_BWRAP_INFO_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| {
                        format!("failed to read bubblewrap namespace information: {error}")
                    })?;
                if bytes.len() as u64 > MAX_BWRAP_INFO_BYTES {
                    return Err(format!(
                        "bubblewrap namespace information exceeds the {MAX_BWRAP_INFO_BYTES}-byte limit"
                    ));
                }
                let info: BwrapInfo = serde_json::from_slice(&bytes).map_err(|error| {
                    format!("invalid bubblewrap namespace information: {error}")
                })?;
                if info.child_pid == 0 {
                    return Err(
                        "bubblewrap namespace information has an invalid child pid".to_string()
                    );
                }
                Ok(info.child_pid)
            })();
            let _ = sender.send(result);
        })
        .map_err(|error| {
            format!("failed to start bubblewrap namespace information reader: {error}")
        })?;
    receiver
        .recv_timeout(SUPERVISOR_CLEANUP_TIMEOUT)
        .map_err(|error| format!("bubblewrap namespace information was not ready: {error}"))?
}

#[cfg(target_os = "linux")]
fn append_linux_spawn_cleanup_error(launch_error: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => launch_error,
        Err(cleanup_error) => {
            format!("{launch_error}; supervised launch cleanup was not proven: {cleanup_error}")
        }
    }
}

#[cfg(target_os = "linux")]
fn append_linux_launch_diagnostics(
    mut launch_error: String,
    child: &mut std::process::Child,
) -> String {
    let status = match child.try_wait() {
        Ok(Some(status)) => status,
        Ok(None) => {
            launch_error.push_str("; bubblewrap monitor was still running after launch cleanup");
            return launch_error;
        }
        Err(error) => {
            launch_error.push_str(&format!(
                "; failed to inspect bubblewrap monitor after launch cleanup: {error}"
            ));
            return launch_error;
        }
    };
    launch_error.push_str(&format!("; bubblewrap monitor status: {status}"));
    let Some(mut stderr) = child.stderr.take() else {
        return launch_error;
    };
    let descriptor = stderr.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        launch_error.push_str(&format!(
            "; failed to make bubblewrap stderr nonblocking: {}",
            std::io::Error::last_os_error()
        ));
        return launch_error;
    }
    let mut bytes = Vec::new();
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    let mut timed_out = false;
    let mut read_error = None;
    while bytes.len() <= MAX_PROCESS_CLEANUP_TEXT_BYTES {
        let mut buffer = [0_u8; 1024];
        match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    timed_out = true;
                    break;
                }
                let remaining = deadline.saturating_duration_since(now);
                let timeout = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
                let mut poll_descriptor = libc::pollfd {
                    fd: descriptor,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                let result = unsafe { libc::poll(&mut poll_descriptor, 1, timeout) };
                if result == 0 {
                    timed_out = true;
                    break;
                }
                if result == -1 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    read_error = Some(error);
                    break;
                }
            }
            Err(error) => {
                read_error = Some(error);
                break;
            }
        }
    }
    let truncated = bytes.len() > MAX_PROCESS_CLEANUP_TEXT_BYTES;
    bytes.truncate(MAX_PROCESS_CLEANUP_TEXT_BYTES);
    let diagnostic = String::from_utf8_lossy(&bytes);
    let diagnostic = diagnostic.trim();
    if !diagnostic.is_empty() {
        launch_error.push_str("; bubblewrap stderr: ");
        launch_error.push_str(diagnostic);
        if truncated {
            launch_error.push_str(" [truncated]");
        }
    }
    if timed_out {
        launch_error.push_str("; timed out draining bubblewrap stderr");
    }
    if let Some(error) = read_error {
        launch_error.push_str(&format!(
            "; failed to read bubblewrap stderr after launch cleanup: {error}"
        ));
    }
    launch_error
}

#[cfg(target_os = "linux")]
fn cleanup_linux_namespace_and_monitor(
    child: &mut std::process::Child,
    monitor_group: i32,
    scope_root: &ProcessIdentity,
) -> Result<(), String> {
    let signal_result = signal_linux_process_identity(scope_root);
    let (initially_absent, initial_identity_error) =
        match wait_for_linux_identity_absent(scope_root, SUPERVISOR_CLEANUP_TIMEOUT) {
            Ok(absent) => (absent, None),
            Err(error) => (false, Some(error)),
        };

    if !initially_absent {
        let _ = signal_process_group(monitor_group);
        let _ = child.kill();
    }

    let mut reap_result = wait_for_child_reap(child, SUPERVISOR_CLEANUP_TIMEOUT);
    if reap_result.is_err() {
        let group_result = signal_process_group(monitor_group);
        let child_result = match child.kill() {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
            Err(error) => Err(format!("failed to terminate bubblewrap monitor: {error}")),
        };
        reap_result = wait_for_child_reap(child, SUPERVISOR_CLEANUP_TIMEOUT);
        group_result?;
        child_result?;
    }

    let final_identity_result = if initially_absent {
        Ok(true)
    } else {
        wait_for_linux_identity_absent(scope_root, SUPERVISOR_CLEANUP_TIMEOUT)
    };
    signal_result?;
    reap_result?;
    if let Some(error) = initial_identity_error {
        return Err(format!(
            "failed to observe namespace cleanup before forced monitor teardown: {error}"
        ));
    }
    match final_identity_result {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "bubblewrap namespace init {} survived ordered cleanup",
            scope_root.pid
        )),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn cleanup_failed_linux_spawn(
    child: &mut std::process::Child,
    monitor_group: i32,
    known_scope_root: Option<&ProcessIdentity>,
) -> Result<(), String> {
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    let mut scope_root = known_scope_root.cloned();

    while scope_root.is_none() && Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect bubblewrap monitor during launch cleanup: {error}"
                ));
            }
        }
        match discover_bwrap_namespace_init(child.id()) {
            Ok(Some(identity)) => scope_root = Some(identity),
            Ok(None) => thread::sleep(SUPERVISOR_POLL_INTERVAL),
            Err(error) => return Err(error),
        }
    }

    let Some(scope_root) = scope_root else {
        let _ = signal_process_group(monitor_group);
        let _ = child.kill();
        let _ = wait_for_child_reap(child, SUPERVISOR_CLEANUP_TIMEOUT);
        return Err(
            "bubblewrap monitor did not exit and its namespace init could not be identified"
                .to_string(),
        );
    };

    cleanup_linux_namespace_and_monitor(child, monitor_group, &scope_root)
}

#[cfg(target_os = "linux")]
fn discover_bwrap_namespace_init(monitor_pid: u32) -> Result<Option<ProcessIdentity>, String> {
    let children_path = format!("/proc/{monitor_pid}/task/{monitor_pid}/children");
    let children = match std::fs::read_to_string(&children_path) {
        Ok(children) => children,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to inspect bubblewrap monitor children {children_path}: {error}"
            ));
        }
    };
    let pids = children
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("bubblewrap monitor reported an invalid child pid: {error}"))?;
    if pids.is_empty() {
        return Ok(None);
    }
    if pids.len() != 1 {
        return Err(format!(
            "bubblewrap monitor {monitor_pid} has an ambiguous child set: {pids:?}"
        ));
    }
    let identity = ProcessIdentity::capture(pids[0])?;
    validate_bwrap_namespace_init(&identity, monitor_pid)?;
    Ok(Some(identity))
}

#[cfg(target_os = "linux")]
fn wait_for_child_reap(child: &mut std::process::Child, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(SUPERVISOR_POLL_INTERVAL);
            }
            Ok(None) => {
                return Err(format!(
                    "bubblewrap monitor did not exit within {} seconds",
                    timeout.as_secs()
                ));
            }
            Err(error) => return Err(format!("failed to reap bubblewrap monitor: {error}")),
        }
    }
}

#[cfg(target_os = "linux")]
fn validate_bwrap_namespace_init(
    identity: &ProcessIdentity,
    monitor_pid: u32,
) -> Result<(), String> {
    let status =
        std::fs::read_to_string(format!("/proc/{}/status", identity.pid)).map_err(|error| {
            format!(
                "failed to inspect supervised Linux namespace init {}: {error}",
                identity.pid
            )
        })?;
    let parent_pid = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|value| value.trim().parse::<u32>().ok())
        .ok_or_else(|| {
            format!(
                "supervised Linux namespace init {} has no valid parent pid",
                identity.pid
            )
        })?;
    if parent_pid != monitor_pid {
        return Err(format!(
            "bubblewrap reported process {} with unexpected parent {parent_pid}; expected monitor {monitor_pid}",
            identity.pid
        ));
    }
    let namespace_pids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .map(|value| {
            value
                .split_whitespace()
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map_err(|error| {
            format!(
                "supervised Linux namespace init {} has invalid namespace pid data: {error}",
                identity.pid
            )
        })?
        .ok_or_else(|| {
            format!(
                "supervised Linux namespace init {} has no namespace pid data",
                identity.pid
            )
        })?;
    if namespace_pids.len() < 2
        || namespace_pids.first() != Some(&identity.pid)
        || namespace_pids.last() != Some(&1)
    {
        return Err(format!(
            "bubblewrap reported process {} without namespace PID 1 identity",
            identity.pid
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn signal_linux_process_identity(identity: &ProcessIdentity) -> Result<(), String> {
    let pid = i32::try_from(identity.pid)
        .map_err(|_| "Linux process identifier exceeds the pid range".to_string())?;
    let raw_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw_pidfd == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) && !linux_identity_still_matches(identity)? {
            return Ok(());
        }
        return Err(format!(
            "failed to open exact Linux namespace-init handle {}: {error}",
            identity.pid
        ));
    }
    let pidfd = unsafe { File::from_raw_fd(raw_pidfd as libc::c_int) };
    match ProcessIdentity::capture(identity.pid) {
        Ok(current) if current == *identity => {}
        Ok(_) => {
            return Err(format!(
                "Linux namespace init {} changed identity before termination",
                identity.pid
            ));
        }
        Err(error) => {
            if !linux_identity_still_matches(identity)? {
                return Ok(());
            }
            return Err(format!(
                "failed to revalidate Linux namespace init {} before termination: {error}",
                identity.pid
            ));
        }
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!(
            "failed to terminate Linux namespace init {}: {error}",
            identity.pid
        ))
    }
}

#[cfg(target_os = "linux")]
fn wait_for_linux_identity_absent(
    identity: &ProcessIdentity,
    timeout: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if !linux_identity_still_matches(identity)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }
}

#[cfg(unix)]
fn signal_process_group(group: i32) -> Result<(), String> {
    if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!(
            "failed to terminate supervised process group {group}: {error}"
        ))
    }
}

#[cfg(target_os = "macos")]
fn wait_for_process_group_empty(group: i32) -> Result<bool, String> {
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    loop {
        if unsafe { libc::kill(-group, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(true);
            }
            if error.raw_os_error() != Some(libc::EPERM) {
                return Err(format!(
                    "failed to verify supervised process group {group}: {error}"
                ));
            }
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }
}

fn spawn_bounded_reader<R: Read + Send + 'static>(
    mut reader: R,
    stream: &str,
) -> Result<mpsc::Receiver<Result<Vec<u8>, String>>, String> {
    let thread_name = format!("nib-supervisor-{stream}");
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let result = (|| {
                let mut retained = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    let count = reader.read(&mut buffer).map_err(|error| {
                        format!("failed to read supervised process output: {error}")
                    })?;
                    if count == 0 {
                        break;
                    }
                    if count >= MAX_SUPERVISED_OUTPUT_BYTES {
                        retained.clear();
                        retained.extend_from_slice(
                            &buffer[count - MAX_SUPERVISED_OUTPUT_BYTES.min(count)..count],
                        );
                        continue;
                    }
                    let overflow = retained
                        .len()
                        .saturating_add(count)
                        .saturating_sub(MAX_SUPERVISED_OUTPUT_BYTES);
                    if overflow > 0 {
                        retained.drain(..overflow);
                    }
                    retained.extend_from_slice(&buffer[..count]);
                }
                Ok(retained)
            })();
            let _ = sender.send(result);
        })
        .map_err(|error| format!("failed to start supervised {stream} reader: {error}"))?;
    Ok(receiver)
}

fn join_bounded_reader(
    reader: mpsc::Receiver<Result<Vec<u8>, String>>,
    stream: &str,
) -> Result<Vec<u8>, String> {
    reader
        .recv_timeout(SUPERVISOR_CLEANUP_TIMEOUT)
        .map_err(|error| format!("supervised {stream} did not drain after cleanup: {error}"))?
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CleanupLeaseRecord {
    version: u32,
    scope_id: String,
    execution_generation: u64,
    cleanup_lease_id: String,
}

pub struct CleanupLease {
    directory: crate::daemons::state::StableDirectory,
    path: PathBuf,
    file: Option<File>,
    record: CleanupLeaseRecord,
}

impl CleanupLease {
    pub fn execution_generation(&self) -> u64 {
        self.record.execution_generation
    }

    pub fn cleanup_lease_id(&self) -> &str {
        &self.record.cleanup_lease_id
    }

    pub fn release_after_proof(mut self, proof: &CleanupProof) -> Result<(), String> {
        if proof.execution_generation != self.record.execution_generation
            || proof.cleanup_lease_id != self.record.cleanup_lease_id
            || !proof.descendants_reaped
        {
            return Err("cleanup proof does not own this managed-process lease".to_string());
        }
        let scope_path = self
            .path
            .with_file_name(format!("{}.json", self.record.scope_id));
        let scope_file = self.directory.open_read(&scope_path)?;
        let scope: ProcessScopeRecord = read_bounded_json(&scope_file, &scope_path)?;
        validate_record(&scope)?;
        if scope.status != ProcessScopeStatus::Complete
            || scope.cleanup_proof.as_ref() != Some(proof)
        {
            return Err(
                "cleanup proof is not the authoritative completed process scope".to_string(),
            );
        }
        let file = self
            .file
            .take()
            .ok_or("managed-process cleanup lease is already released")?;
        self.directory
            .remove_file_if_matches(&self.path, &file, CLEANUP_LEASE_DELETE_PREFIX)
    }

    pub fn release_after_launch_abort(mut self, proof: &LaunchAbortProof) -> Result<(), String> {
        if proof.execution_generation != self.record.execution_generation
            || proof.cleanup_lease_id != self.record.cleanup_lease_id
            || !proof.workload_never_launched
        {
            return Err("launch-abort proof does not own this managed-process lease".to_string());
        }
        let scope_path = self
            .path
            .with_file_name(format!("{}.json", self.record.scope_id));
        let scope_file = self.directory.open_read(&scope_path)?;
        let scope: ProcessScopeRecord = read_bounded_json(&scope_file, &scope_path)?;
        validate_record(&scope)?;
        if scope.status != ProcessScopeStatus::Complete
            || scope.launch_abort_proof.as_ref() != Some(proof)
            || scope.cleanup_proof.is_some()
        {
            return Err(
                "launch-abort proof is not the authoritative completed process scope".to_string(),
            );
        }
        let file = self
            .file
            .take()
            .ok_or("managed-process cleanup lease is already released")?;
        self.directory
            .remove_file_if_matches(&self.path, &file, CLEANUP_LEASE_DELETE_PREFIX)
    }
}

fn validate_scope_fields(
    scope_id: &str,
    workload_kind: &str,
    execution_generation: u64,
) -> Result<(), String> {
    validate_scope_id(scope_id)?;
    if workload_kind.is_empty()
        || workload_kind.len() > 64
        || !workload_kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("managed-process workload kind is invalid".to_string());
    }
    if execution_generation == 0 {
        return Err("managed-process execution generation must be non-zero".to_string());
    }
    Ok(())
}

fn validate_scope_id(scope_id: &str) -> Result<(), String> {
    if scope_id.is_empty()
        || scope_id.len() > 160
        || !scope_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("managed-process scope identifier is invalid".to_string());
    }
    Ok(())
}

fn validate_record(record: &ProcessScopeRecord) -> Result<(), String> {
    if record.version != PROCESS_SCOPE_VERSION {
        return Err(format!(
            "unsupported managed-process scope version {}",
            record.version
        ));
    }
    validate_record_contents(record)
}

fn validate_record_contents(record: &ProcessScopeRecord) -> Result<(), String> {
    validate_scope_fields(
        &record.scope_id,
        &record.workload_kind,
        record.execution_generation,
    )?;
    let cleanup_lease = uuid::Uuid::parse_str(&record.cleanup_lease_id)
        .map_err(|_| "managed-process cleanup lease identifier is invalid".to_string())?;
    if cleanup_lease.to_string() != record.cleanup_lease_id {
        return Err("managed-process cleanup lease identifier is not canonical".to_string());
    }
    validate_process_identity(&record.owner, "owner")?;
    if let Some(supervisor) = &record.supervisor {
        validate_process_identity(supervisor, "supervisor")?;
    }
    if let Some(direct_child) = &record.direct_child {
        validate_process_identity(direct_child, "direct child")?;
    }
    if let Some(reason) = &record.cleanup_reason {
        validate_process_cleanup_text(reason, "cleanup reason")?;
    }
    if record.status == ProcessScopeStatus::Complete {
        match (&record.cleanup_proof, &record.launch_abort_proof) {
            (Some(proof), None) => {
                validate_process_identity(&proof.direct_child, "cleanup proof direct child")?;
                validate_process_cleanup_text(&proof.outcome, "cleanup outcome")?;
                if proof.execution_generation != record.execution_generation
                    || proof.cleanup_lease_id != record.cleanup_lease_id
                    || proof.backend != record.backend
                    || Some(&proof.direct_child) != record.direct_child.as_ref()
                    || !proof.descendants_reaped
                {
                    return Err(
                        "managed-process cleanup proof does not match its scope".to_string()
                    );
                }
            }
            (None, Some(proof)) => {
                validate_process_identity(&proof.supervisor, "launch-abort supervisor")?;
                if let Some(namespace_root) = &proof.namespace_root {
                    validate_process_identity(namespace_root, "launch-abort namespace root")?;
                }
                validate_process_cleanup_text(&proof.outcome, "launch-abort outcome")?;
                if proof.execution_generation != record.execution_generation
                    || proof.cleanup_lease_id != record.cleanup_lease_id
                    || proof.backend != record.backend
                    || Some(&proof.supervisor) != record.supervisor.as_ref()
                    || proof.namespace_root != record.direct_child
                    || proof.outcome != LAUNCH_ABORT_OUTCOME
                    || !proof.workload_never_launched
                {
                    return Err(
                        "managed-process launch-abort proof does not match its scope".to_string(),
                    );
                }
            }
            (Some(_), Some(_)) => {
                return Err(
                    "completed managed-process scope carries conflicting completion proofs"
                        .to_string(),
                );
            }
            (None, None) => {
                return Err("completed managed-process scope has no completion proof".to_string());
            }
        }
    } else if record.cleanup_proof.is_some() || record.launch_abort_proof.is_some() {
        return Err("nonterminal managed-process scope carries a completion proof".to_string());
    }
    Ok(())
}

fn validate_process_identity(identity: &ProcessIdentity, label: &str) -> Result<(), String> {
    if identity.pid == 0
        || identity.start_marker.is_empty()
        || identity.start_marker.len() > MAX_PROCESS_IDENTITY_MARKER_BYTES
    {
        return Err(format!("managed-process {label} identity is invalid"));
    }
    Ok(())
}

fn validate_process_cleanup_text(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_PROCESS_CLEANUP_TEXT_BYTES {
        return Err(format!(
            "managed-process {label} exceeds the {MAX_PROCESS_CLEANUP_TEXT_BYTES}-byte limit or is empty"
        ));
    }
    Ok(())
}

fn encode_process_state_bounded<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>, String> {
    let encoded = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("failed to encode managed-process {label}: {error}"))?;
    if encoded.len() as u64 > MAX_SCOPE_RECORD_BYTES {
        return Err(format!(
            "managed-process {label} exceeds the {MAX_SCOPE_RECORD_BYTES}-byte limit"
        ));
    }
    Ok(encoded)
}

fn maintain_process_scope_directory(
    directory: &crate::daemons::state::StableDirectory,
    reserve_new_scope: bool,
) -> Result<(), String> {
    maintain_process_scope_directory_with_limits(
        directory,
        reserve_new_scope,
        PROCESS_SCOPE_DIRECTORY_LIMITS,
    )
}

fn maintain_process_scope_directory_with_limits(
    directory: &crate::daemons::state::StableDirectory,
    reserve_new_scope: bool,
    limits: ProcessScopeDirectoryLimits,
) -> Result<(), String> {
    let usage = process_scope_directory_usage_with_limits(directory, limits)?;
    let reserved = usize::from(reserve_new_scope);
    if usage.records > limits.max_records.saturating_sub(reserved) {
        return Err(format!(
            "managed-process scope count exceeds the {}-record limit",
            limits.max_records
        ));
    }
    Ok(())
}

fn process_scope_directory_usage_with_limits(
    directory: &crate::daemons::state::StableDirectory,
    limits: ProcessScopeDirectoryLimits,
) -> Result<ProcessScopeDirectoryUsage, String> {
    recover_all_process_atomic_transactions(directory, limits)?;
    recover_stale_process_temporaries(
        directory,
        SCOPE_WRITE_PREFIX,
        ProcessAtomicKind::Scope,
        limits,
    )?;
    recover_stale_process_temporaries(
        directory,
        CLEANUP_LEASE_WRITE_PREFIX,
        ProcessAtomicKind::CleanupLease,
        limits,
    )?;

    let mut usage = ProcessScopeDirectoryUsage::default();
    directory.for_each_entry_bounded(limits.max_entries, limits.max_name_bytes, |name| {
        usage.entries = usage
            .entries
            .checked_add(1)
            .ok_or_else(|| "managed-process state entry count overflowed".to_string())?;
        usage.name_bytes = usage
            .name_bytes
            .checked_add(name.as_encoded_bytes().len())
            .ok_or_else(|| "managed-process state filename byte count overflowed".to_string())?;
        let name_text = name
            .to_str()
            .ok_or_else(|| "managed-process state contains a non-UTF-8 filename".to_string())?;
        let path = directory.path().join(&name);
        let file = directory.open_read(&path)?;
        let length = file
            .metadata()
            .map_err(|error| {
                format!(
                    "failed to inspect managed-process state {}: {error}",
                    path.display()
                )
            })?
            .len();
        usage.bytes = usage
            .bytes
            .checked_add(length)
            .ok_or_else(|| "managed-process state aggregate byte count overflowed".to_string())?;
        if usage.bytes > limits.max_bytes {
            return Err(format!(
                "managed-process state exceeds the {}-byte aggregate limit",
                limits.max_bytes
            ));
        }

        if crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
            &name,
            SCOPE_WRITE_PREFIX,
        ) || crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
            &name,
            CLEANUP_LEASE_WRITE_PREFIX,
        ) || is_process_deletion_quarantine(name_text)
            || (name_text.starts_with('.') && name_text.ends_with(".lock"))
        {
            return Ok(());
        }
        if let Some(scope_id) = name_text.strip_suffix(".json") {
            validate_scope_id(scope_id)?;
            let record: ProcessScopeRecord = read_bounded_json(&file, &path)?;
            if record.version == 1 {
                validate_record_contents(&record)?;
            } else {
                validate_record(&record)?;
            }
            if record.scope_id != scope_id {
                return Err(format!(
                    "managed-process scope filename does not match its record: {}",
                    path.display()
                ));
            }
            usage.records = usage
                .records
                .checked_add(1)
                .ok_or_else(|| "managed-process scope count overflowed".to_string())?;
            return Ok(());
        }
        if let Some(scope_id) = name_text.strip_suffix(CLEANUP_LEASE_SUFFIX) {
            validate_scope_id(scope_id)?;
            let lease: CleanupLeaseRecord = read_bounded_json(&file, &path)?;
            if lease.version == 1 {
                validate_cleanup_lease_contents(&lease)?;
            } else {
                validate_cleanup_lease_record(&lease)?;
            }
            if lease.scope_id != scope_id {
                return Err(format!(
                    "managed-process cleanup lease filename does not match its record: {}",
                    path.display()
                ));
            }
            return Ok(());
        }
        Err(format!(
            "managed-process state contains an unknown entry: {}",
            path.display()
        ))
    })?;
    Ok(usage)
}

fn ensure_process_scope_publication_budget(
    directory: &crate::daemons::state::StableDirectory,
    target: &Path,
    encoded_len: u64,
    kind: ProcessAtomicKind,
    limits: ProcessScopeDirectoryLimits,
) -> Result<(), String> {
    if encoded_len > MAX_SCOPE_RECORD_BYTES {
        return Err(format!(
            "managed-process publication exceeds the {MAX_SCOPE_RECORD_BYTES}-byte record limit"
        ));
    }
    let usage = process_scope_directory_usage_with_limits(directory, limits)?;
    let target_exists = directory.path_exists(target)?;
    let temporary = directory.deterministic_artifact_path(target, kind.write_prefix(), ".tmp")?;
    let transaction_peer = if target_exists {
        directory.deterministic_previous_artifact_path(target, kind.write_prefix())?
    } else {
        target.to_path_buf()
    };
    let adds_record = usize::from(!target_exists && matches!(kind, ProcessAtomicKind::Scope));
    let transaction_name_bytes = temporary
        .file_name()
        .ok_or("managed-process temporary publication has no filename")?
        .as_encoded_bytes()
        .len()
        .checked_add(
            transaction_peer
                .file_name()
                .ok_or("managed-process publication transaction peer has no filename")?
                .as_encoded_bytes()
                .len(),
        )
        .ok_or_else(|| "managed-process transaction filename byte count overflowed".to_string())?;
    let records = usage
        .records
        .checked_add(adds_record)
        .ok_or_else(|| "managed-process scope count overflowed".to_string())?;
    let entries = usage
        .entries
        .checked_add(2)
        .ok_or_else(|| "managed-process state entry count overflowed".to_string())?;
    let name_bytes = usage
        .name_bytes
        .checked_add(transaction_name_bytes)
        .ok_or_else(|| "managed-process state filename byte count overflowed".to_string())?;
    let transaction_bytes = encoded_len
        .checked_mul(2)
        .ok_or_else(|| "managed-process transaction byte count overflowed".to_string())?;
    let bytes = usage
        .bytes
        .checked_add(transaction_bytes)
        .ok_or_else(|| "managed-process state aggregate byte count overflowed".to_string())?;

    if records > limits.max_records {
        return Err(format!(
            "managed-process scope count exceeds the {}-record limit",
            limits.max_records
        ));
    }
    if entries > limits.max_entries {
        return Err(format!(
            "managed-process state exceeds the {}-entry limit",
            limits.max_entries
        ));
    }
    if name_bytes > limits.max_name_bytes {
        return Err(format!(
            "managed-process state exceeds the {}-byte filename limit",
            limits.max_name_bytes
        ));
    }
    if bytes > limits.max_bytes {
        return Err(format!(
            "managed-process state exceeds the {}-byte aggregate limit",
            limits.max_bytes
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ProcessAtomicKind {
    Scope,
    CleanupLease,
}

impl ProcessAtomicKind {
    fn write_prefix(self) -> &'static str {
        match self {
            Self::Scope => SCOPE_WRITE_PREFIX,
            Self::CleanupLease => CLEANUP_LEASE_WRITE_PREFIX,
        }
    }
}

fn recover_stale_process_temporaries(
    directory: &crate::daemons::state::StableDirectory,
    temporary_prefix: &str,
    kind: ProcessAtomicKind,
    limits: ProcessScopeDirectoryLimits,
) -> Result<(), String> {
    let mut temporary_names = Vec::new();
    directory.for_each_entry_bounded(limits.max_entries, limits.max_name_bytes, |name| {
        if crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
            &name,
            temporary_prefix,
        ) && crate::daemons::state::StableDirectory::atomic_previous_target_name(
            &name,
            temporary_prefix,
        )
        .is_none()
        {
            temporary_names.push(name);
        }
        Ok(())
    })?;
    for name in temporary_names {
        let path = directory.path().join(name);
        if !directory.path_exists(&path)? {
            continue;
        }
        let file = directory.open_read_write(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to inspect managed-process temporary state {}: {error}",
                    path.display()
                ));
            }
        }
        if process_atomic_payload_version(&file, &path, None, kind)? == 1
            && legacy_process_temporary_matches_payload(
                directory,
                &file,
                &path,
                temporary_prefix,
                kind,
            )?
        {
            continue;
        }
        directory.remove_visible_file_if_matches_direct(&path, &file)?;
    }
    directory.sync_directory()
}

fn recover_all_process_atomic_transactions(
    directory: &crate::daemons::state::StableDirectory,
    limits: ProcessScopeDirectoryLimits,
) -> Result<(), String> {
    let mut transactions = Vec::new();
    directory.for_each_entry_bounded(limits.max_entries, limits.max_name_bytes, |name| {
        for (prefix, kind) in [
            (SCOPE_WRITE_PREFIX, ProcessAtomicKind::Scope),
            (CLEANUP_LEASE_WRITE_PREFIX, ProcessAtomicKind::CleanupLease),
        ] {
            if let Some(target) =
                crate::daemons::state::StableDirectory::atomic_previous_target_name(&name, prefix)
            {
                transactions.push((target, prefix, kind));
            }
        }
        Ok(())
    })?;
    transactions.sort_by(|left, right| left.0.cmp(&right.0));
    transactions.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    for (target, prefix, kind) in transactions {
        recover_process_atomic_transaction(
            directory,
            &directory.path().join(target),
            prefix,
            kind,
        )?;
    }
    Ok(())
}

fn recover_process_atomic_transaction(
    directory: &crate::daemons::state::StableDirectory,
    target: &Path,
    temporary_prefix: &str,
    kind: ProcessAtomicKind,
) -> Result<(), String> {
    let temporary = directory.deterministic_artifact_path(target, temporary_prefix, ".tmp")?;
    let previous = directory.deterministic_previous_artifact_path(target, temporary_prefix)?;
    let temporary_file = if directory.path_exists(&temporary)? {
        let file = directory.open_read_write(&temporary)?;
        match file.try_lock() {
            Ok(()) => Some(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "managed-process atomic transaction is still owned by a live writer: {}",
                    target.display()
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to inspect managed-process atomic writer {}: {error}",
                    temporary.display()
                ));
            }
        }
    } else {
        None
    };

    let target_file = directory
        .path_exists(target)?
        .then(|| directory.open_read(target))
        .transpose()?;
    let previous_file = directory
        .path_exists(&previous)?
        .then(|| directory.open_read(&previous))
        .transpose()?;
    let target_payload_file = target_file
        .as_ref()
        .map(|file| process_atomic_read_handle(file, temporary_file.as_ref()))
        .transpose()?;
    let previous_payload_file = previous_file
        .as_ref()
        .map(|file| process_atomic_read_handle(file, temporary_file.as_ref()))
        .transpose()?;
    if process_atomic_transaction_is_legacy(
        target,
        target_payload_file.map(|file| (file, target)),
        previous_payload_file.map(|file| (file, previous.as_path())),
        temporary_file
            .as_ref()
            .map(|file| (file, temporary.as_path())),
        kind,
    )? {
        return Ok(());
    }
    match (target_file.as_ref(), previous_file.as_ref()) {
        (None, Some(previous_file)) => {
            validate_process_atomic_payload(
                directory,
                target,
                previous_payload_file.expect("previous payload handle"),
                kind,
            )?;
            directory.restore_visible_file_no_replace_if_matches(
                &previous,
                previous_file,
                target,
            )?;
        }
        (Some(target_file), Some(previous_file)) => {
            validate_process_atomic_pair(
                directory,
                target,
                target_payload_file.expect("target payload handle"),
                &previous,
                previous_payload_file.expect("previous payload handle"),
                kind,
            )?;
            directory.verify_file_identity(target, target_file)?;
            directory.remove_visible_file_if_matches_direct(&previous, previous_file)?;
        }
        (Some(target_file), None) => {
            validate_process_atomic_payload(
                directory,
                target,
                target_payload_file.expect("target payload handle"),
                kind,
            )?;
            directory.verify_file_identity(target, target_file)?;
        }
        (None, None) => {}
    }
    if let Some(temporary_file) = temporary_file {
        if directory.path_exists(&temporary)? {
            directory.remove_visible_file_if_matches_direct(&temporary, &temporary_file)?;
        }
    }
    directory.sync_directory()
}

fn process_atomic_read_handle<'a>(
    visible: &'a File,
    temporary: Option<&'a File>,
) -> Result<&'a File, String> {
    if let Some(temporary) = temporary {
        if crate::daemons::state::same_open_file_identity(visible, temporary)? {
            return Ok(temporary);
        }
    }
    Ok(visible)
}

fn process_atomic_transaction_is_legacy(
    target_path: &Path,
    target: Option<(&File, &Path)>,
    previous: Option<(&File, &Path)>,
    temporary: Option<(&File, &Path)>,
    kind: ProcessAtomicKind,
) -> Result<bool, String> {
    let mut has_legacy = false;
    let mut has_current_or_unknown = false;
    for (file, path) in [target, previous, temporary].into_iter().flatten() {
        if process_atomic_payload_version(file, path, Some(target_path), kind)? == 1 {
            has_legacy = true;
        } else {
            has_current_or_unknown = true;
        }
    }
    if has_legacy && has_current_or_unknown {
        return Err("managed-process atomic transaction mixes schema versions".to_string());
    }
    Ok(has_legacy)
}

fn process_atomic_payload_version(
    file: &File,
    path: &Path,
    target: Option<&Path>,
    kind: ProcessAtomicKind,
) -> Result<u32, String> {
    match kind {
        ProcessAtomicKind::Scope => {
            let record: ProcessScopeRecord = read_bounded_json(file, path)?;
            if record.version == 1 {
                validate_record_contents(&record)?;
                if let Some(target) = target {
                    validate_process_atomic_scope_key(target, &record.scope_id)?;
                }
            }
            Ok(record.version)
        }
        ProcessAtomicKind::CleanupLease => {
            let record: CleanupLeaseRecord = read_bounded_json(file, path)?;
            if record.version == 1 {
                validate_cleanup_lease_contents(&record)?;
                if let Some(target) = target {
                    validate_process_atomic_cleanup_lease_key(target, &record.scope_id)?;
                }
            }
            Ok(record.version)
        }
    }
}

fn validate_process_atomic_scope_key(target: &Path, scope_id: &str) -> Result<(), String> {
    let target_scope_id = target
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".json"))
        .ok_or("managed-process atomic scope target has an invalid suffix")?;
    validate_scope_id(target_scope_id)?;
    if scope_id != target_scope_id {
        return Err("managed-process atomic scope target has a mismatched key".to_string());
    }
    Ok(())
}

fn validate_process_atomic_cleanup_lease_key(target: &Path, scope_id: &str) -> Result<(), String> {
    let target_scope_id = target
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(CLEANUP_LEASE_SUFFIX))
        .ok_or("managed-process atomic cleanup-lease target has an invalid suffix")?;
    validate_scope_id(target_scope_id)?;
    if scope_id != target_scope_id {
        return Err("managed-process atomic cleanup-lease target has a mismatched key".to_string());
    }
    Ok(())
}

fn legacy_process_temporary_matches_payload(
    directory: &crate::daemons::state::StableDirectory,
    file: &File,
    temporary: &Path,
    temporary_prefix: &str,
    kind: ProcessAtomicKind,
) -> Result<bool, String> {
    let target = match kind {
        ProcessAtomicKind::Scope => {
            let record: ProcessScopeRecord = read_bounded_json(file, temporary)?;
            directory.path().join(format!("{}.json", record.scope_id))
        }
        ProcessAtomicKind::CleanupLease => {
            let record: CleanupLeaseRecord = read_bounded_json(file, temporary)?;
            directory
                .path()
                .join(format!("{}{}", record.scope_id, CLEANUP_LEASE_SUFFIX))
        }
    };
    let expected = directory.deterministic_artifact_path(&target, temporary_prefix, ".tmp")?;
    Ok(expected == temporary)
}

fn validate_process_atomic_payload(
    _directory: &crate::daemons::state::StableDirectory,
    target: &Path,
    file: &File,
    kind: ProcessAtomicKind,
) -> Result<(), String> {
    match kind {
        ProcessAtomicKind::Scope => {
            let record: ProcessScopeRecord = read_bounded_json(file, target)?;
            validate_record(&record)?;
            validate_process_atomic_scope_key(target, &record.scope_id)?;
        }
        ProcessAtomicKind::CleanupLease => {
            let record: CleanupLeaseRecord = read_bounded_json(file, target)?;
            validate_cleanup_lease_record(&record)?;
            validate_process_atomic_cleanup_lease_key(target, &record.scope_id)?;
        }
    }
    Ok(())
}

fn validate_process_atomic_pair(
    directory: &crate::daemons::state::StableDirectory,
    target_path: &Path,
    target_file: &File,
    previous_path: &Path,
    previous_file: &File,
    kind: ProcessAtomicKind,
) -> Result<(), String> {
    validate_process_atomic_payload(directory, target_path, target_file, kind)?;
    validate_process_atomic_payload(directory, target_path, previous_file, kind)?;
    match kind {
        ProcessAtomicKind::Scope => {
            let target: ProcessScopeRecord = read_bounded_json(target_file, target_path)?;
            let previous: ProcessScopeRecord = read_bounded_json(previous_file, previous_path)?;
            validate_process_scope_transition(&previous, &target)
        }
        ProcessAtomicKind::CleanupLease => {
            let target: CleanupLeaseRecord = read_bounded_json(target_file, target_path)?;
            let previous: CleanupLeaseRecord = read_bounded_json(previous_file, previous_path)?;
            if target == previous {
                Ok(())
            } else {
                Err(
                    "managed-process cleanup-lease target and prior belong to different generations"
                        .to_string(),
                )
            }
        }
    }
}

fn validate_process_scope_transition(
    previous: &ProcessScopeRecord,
    target: &ProcessScopeRecord,
) -> Result<(), String> {
    let immutable_matches = previous.version == target.version
        && previous.scope_id == target.scope_id
        && previous.workload_kind == target.workload_kind
        && previous.execution_generation == target.execution_generation
        && previous.cleanup_lease_id == target.cleanup_lease_id
        && previous.owner == target.owner
        && previous.backend == target.backend
        && previous.created_at == target.created_at;
    let identities_are_monotonic = (previous.supervisor.is_none()
        || previous.supervisor == target.supervisor)
        && (previous.direct_child.is_none() || previous.direct_child == target.direct_child);
    let proof_is_monotonic = (previous.cleanup_proof.is_none()
        || previous.cleanup_proof == target.cleanup_proof)
        && (previous.launch_abort_proof.is_none()
            || previous.launch_abort_proof == target.launch_abort_proof);
    let launch_abort_is_valid = matches!(
        (previous.status, target.status),
        (ProcessScopeStatus::Prepared, ProcessScopeStatus::Complete)
    ) && previous.supervisor.is_some()
        && previous.supervisor == target.supervisor
        && previous.direct_child == target.direct_child
        && target.cleanup_reason.as_deref() == Some(LAUNCH_ABORT_OUTCOME)
        && target.cleanup_proof.is_none()
        && target
            .launch_abort_proof
            .as_ref()
            .is_some_and(|proof| proof.outcome == LAUNCH_ABORT_OUTCOME);
    let status_is_monotonic = matches!(
        (previous.status, target.status),
        (ProcessScopeStatus::Prepared, ProcessScopeStatus::Prepared)
            | (ProcessScopeStatus::Prepared, ProcessScopeStatus::Running)
            | (ProcessScopeStatus::Running, ProcessScopeStatus::Running)
            | (
                ProcessScopeStatus::Running,
                ProcessScopeStatus::CleanupInProgress
            )
            | (ProcessScopeStatus::Running, ProcessScopeStatus::Complete)
            | (
                ProcessScopeStatus::Running,
                ProcessScopeStatus::RecoveryRequired
            )
            | (
                ProcessScopeStatus::CleanupInProgress,
                ProcessScopeStatus::CleanupInProgress
            )
            | (
                ProcessScopeStatus::CleanupInProgress,
                ProcessScopeStatus::Complete
            )
            | (
                ProcessScopeStatus::CleanupInProgress,
                ProcessScopeStatus::RecoveryRequired
            )
            | (
                ProcessScopeStatus::RecoveryRequired,
                ProcessScopeStatus::RecoveryRequired
            )
            | (
                ProcessScopeStatus::RecoveryRequired,
                ProcessScopeStatus::CleanupInProgress
            )
            | (ProcessScopeStatus::Complete, ProcessScopeStatus::Complete)
    ) || launch_abort_is_valid;
    if immutable_matches
        && identities_are_monotonic
        && proof_is_monotonic
        && status_is_monotonic
        && target.updated_at >= previous.updated_at
    {
        Ok(())
    } else {
        Err(
            "managed-process atomic target is not a legal monotonic successor of its prior"
                .to_string(),
        )
    }
}

fn is_process_deletion_quarantine(name: &str) -> bool {
    (name.starts_with(CLEANUP_LEASE_DELETE_PREFIX) || name.starts_with(SCOPE_DELETE_PREFIX))
        && name.ends_with(".quarantine")
}

fn validate_cleanup_lease_record(record: &CleanupLeaseRecord) -> Result<(), String> {
    if record.version != PROCESS_SCOPE_VERSION {
        return Err(format!(
            "unsupported managed-process cleanup lease version {}",
            record.version
        ));
    }
    validate_cleanup_lease_contents(record)
}

fn validate_cleanup_lease_contents(record: &CleanupLeaseRecord) -> Result<(), String> {
    validate_scope_id(&record.scope_id)?;
    if record.execution_generation == 0 {
        return Err("managed-process cleanup lease generation must be non-zero".to_string());
    }
    let lease_id = uuid::Uuid::parse_str(&record.cleanup_lease_id)
        .map_err(|_| "managed-process cleanup lease identifier is invalid".to_string())?;
    if lease_id.to_string() != record.cleanup_lease_id {
        return Err("managed-process cleanup lease identifier is not canonical".to_string());
    }
    Ok(())
}

fn recover_cleanup_lease_deletion(
    directory: &crate::daemons::state::StableDirectory,
    lease_path: &Path,
    scope: &ProcessScopeRecord,
) -> Result<bool, String> {
    let quarantine = directory.deterministic_artifact_path(
        lease_path,
        CLEANUP_LEASE_DELETE_PREFIX,
        ".quarantine",
    )?;
    if !directory.path_exists(&quarantine)? {
        return Ok(false);
    }
    if directory.path_exists(lease_path)? {
        directory.recover_quarantined_file(lease_path, CLEANUP_LEASE_DELETE_PREFIX)?;
        return Ok(false);
    }
    if scope.status != ProcessScopeStatus::Complete
        || (scope.cleanup_proof.is_none() && scope.launch_abort_proof.is_none())
    {
        return Err(
            "managed-process cleanup lease has an unproven deletion quarantine".to_string(),
        );
    }
    let file = directory.open_read_write(&quarantine)?;
    let observed: CleanupLeaseRecord = read_bounded_json(&file, &quarantine)?;
    validate_cleanup_lease_record(&observed)?;
    let expected = CleanupLeaseRecord {
        version: PROCESS_SCOPE_VERSION,
        scope_id: scope.scope_id.clone(),
        execution_generation: scope.execution_generation,
        cleanup_lease_id: scope.cleanup_lease_id.clone(),
    };
    if observed != expected {
        return Err(
            "managed-process cleanup lease deletion quarantine belongs to another generation"
                .to_string(),
        );
    }
    match try_cleanup_lease_lock(&file) {
        Ok(()) => {
            directory.remove_visible_file_if_matches_direct(&quarantine, &file)?;
            Ok(false)
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "failed to inspect quarantined managed-process cleanup lease {}: {error}",
            quarantine.display()
        )),
    }
}

fn recover_scope_deletion_for_expected(
    directory: &crate::daemons::state::StableDirectory,
    scope_path: &Path,
    expected: &ProcessScopeRecord,
) -> Result<bool, String> {
    let quarantine =
        directory.deterministic_artifact_path(scope_path, SCOPE_DELETE_PREFIX, ".quarantine")?;
    if !directory.path_exists(&quarantine)? {
        return Ok(false);
    }
    if directory.path_exists(scope_path)? {
        directory.recover_quarantined_file(scope_path, SCOPE_DELETE_PREFIX)?;
        return Ok(false);
    }
    let file = directory.open_read(&quarantine)?;
    let observed: ProcessScopeRecord = read_bounded_json(&file, &quarantine)?;
    validate_record(&observed)?;
    if observed != *expected {
        return Err(
            "managed-process scope deletion quarantine does not match the expected generation"
                .to_string(),
        );
    }
    directory.remove_visible_file_if_matches_direct(&quarantine, &file)?;
    Ok(true)
}

fn read_scope_record(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<ProcessScopeRecord, String> {
    let file = directory.open_read(path)?;
    let record = read_bounded_json(&file, path)?;
    validate_record(&record)?;
    Ok(record)
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(file: &File, path: &Path) -> Result<T, String> {
    read_bounded_json_with_limit(file, path, MAX_SCOPE_RECORD_BYTES, "managed-process state")
}

fn read_bounded_json_with_limit<T: for<'de> Deserialize<'de>>(
    file: &File,
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<T, String> {
    let length = file
        .metadata()
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?
        .len();
    if length > max_bytes {
        return Err(format!("{label} exceeds the {max_bytes}-byte limit"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    let read_limit = max_bytes.saturating_add(1);
    let mut offset = 0_u64;
    let mut buffer = [0_u8; 8 * 1024];
    while offset < read_limit {
        let remaining = usize::try_from((read_limit - offset).min(buffer.len() as u64))
            .map_err(|_| format!("failed to size bounded read for {}", path.display()))?;
        let read = read_process_state_at(file, &mut buffer[..remaining], offset)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| format!("bounded read offset overflowed for {}", path.display()))?;
    }
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{label} exceeds the {max_bytes}-byte limit"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid managed-process state {}: {error}", path.display()))
}

#[cfg(unix)]
fn read_process_state_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(windows)]
fn read_process_state_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

fn acquire_file_lock(file: &File, path: &Path) -> Result<(), String> {
    match try_cleanup_lease_lock(file) {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "managed-process cleanup lease is already live: {}",
            path.display()
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "failed to acquire managed-process cleanup lease {}: {error}",
            path.display()
        )),
    }
}

#[cfg(not(windows))]
fn try_cleanup_lease_lock(file: &File) -> Result<(), std::fs::TryLockError> {
    file.try_lock()
}

#[cfg(windows)]
fn try_cleanup_lease_lock(file: &File) -> Result<(), std::fs::TryLockError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous.Offset = CLEANUP_LEASE_LOCK_OFFSET as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (CLEANUP_LEASE_LOCK_OFFSET >> 32) as u32;
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Err(std::fs::TryLockError::WouldBlock)
    } else {
        Err(std::fs::TryLockError::Error(error))
    }
}

#[cfg(target_os = "linux")]
fn linux_identity_still_matches(identity: &ProcessIdentity) -> Result<bool, String> {
    match ProcessIdentity::capture(identity.pid) {
        Ok(current) => Ok(current == *identity),
        Err(capture_error) => match std::fs::metadata(format!("/proc/{}", identity.pid)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Ok(_) => Err(capture_error),
            Err(error) => Err(format!(
                "failed to determine whether process {} still exists: {error}; identity inspection failed: {capture_error}",
                identity.pid
            )),
        },
    }
}

#[cfg(target_os = "linux")]
fn platform_process_start_marker(pid: u32) -> Result<String, String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("failed to inspect process {pid}: {error}"))?;
    let close = stat
        .rfind(')')
        .ok_or_else(|| format!("process {pid} has an invalid procfs stat record"))?;
    let fields: Vec<_> = stat[close + 1..].split_whitespace().collect();
    let start_time = fields
        .get(19)
        .ok_or_else(|| format!("process {pid} procfs stat has no start time"))?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("failed to read Linux boot identity: {error}"))?;
    Ok(format!("{}:{}", boot_id.trim(), start_time))
}

#[cfg(target_os = "macos")]
fn platform_process_start_marker(pid: u32) -> Result<String, String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let expected = std::mem::size_of::<libc::proc_bsdinfo>();
    let read = unsafe {
        libc::proc_pidinfo(
            i32::try_from(pid).map_err(|_| "process identifier exceeds i32".to_string())?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            expected as libc::c_int,
        )
    };
    if read != expected as libc::c_int {
        return Err(format!(
            "failed to inspect process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid {
        return Err(format!("process {pid} identity changed while inspected"));
    }
    Ok(format!(
        "{}:{:06}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

#[cfg(windows)]
fn platform_process_start_marker(pid: u32) -> Result<String, String> {
    use std::mem::MaybeUninit;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(format!(
            "failed to open process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut created = MaybeUninit::uninit();
    let mut exited = MaybeUninit::uninit();
    let mut kernel = MaybeUninit::uninit();
    let mut user = MaybeUninit::uninit();
    let result = unsafe {
        GetProcessTimes(
            handle,
            created.as_mut_ptr(),
            exited.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    unsafe {
        CloseHandle(handle);
    }
    if result == 0 {
        return Err(format!(
            "failed to query process {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    let created = unsafe { created.assume_init() };
    Ok(format!(
        "{:08x}{:08x}",
        created.dwHighDateTime, created.dwLowDateTime
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_process_start_marker(_pid: u32) -> Result<String, String> {
    Err("process identity capture is unsupported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_project() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("temp project");
        std::fs::create_dir(root.path().join(".nib")).expect("state root");
        root
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_process_probe_retries_a_transient_cleaned_launch_failure() {
        let mut attempts = 0;
        run_linux_managed_process_probe_attempts(|| {
            attempts += 1;
            if attempts == 1 {
                Err("supervised Linux launch gate closed before reporting readiness".to_string())
            } else {
                Ok(())
            }
        })
        .expect("transient cleaned probe failure is retried");
        assert_eq!(attempts, 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_process_probe_retry_budget_preserves_every_failure() {
        let mut attempts = 0;
        let error = run_linux_managed_process_probe_attempts(|| {
            attempts += 1;
            Err(format!(
                "supervised Linux launch gate readiness timed out ({attempts})"
            ))
        })
        .expect_err("repeated cleaned probe failures exhaust the retry budget");
        assert_eq!(attempts, LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS);
        for attempt in 1..=LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS {
            assert!(error.contains(&format!("attempt {attempt}:")), "{error}");
            assert!(error.contains(&format!("timed out ({attempt})")), "{error}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_process_probe_does_not_retry_unproven_cleanup() {
        let mut attempts = 0;
        let error = run_linux_managed_process_probe_attempts(|| {
            attempts += 1;
            Err(
                "supervised Linux launch gate closed before reporting readiness; supervised launch cleanup was not proven: namespace survived"
                    .to_string(),
            )
        })
        .expect_err("unproven cleanup must fail immediately");
        assert_eq!(attempts, 1);
        assert!(error.contains("after 1 attempt(s)"), "{error}");
        assert!(error.contains("cleanup was not proven"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_process_probe_does_not_retry_diagnosed_monitor_exit() {
        let mut attempts = 0;
        let error = run_linux_managed_process_probe_attempts(|| {
            attempts += 1;
            Err(
                "supervised Linux launch gate closed before reporting readiness; bubblewrap monitor status: exit status: 1; bubblewrap stderr: mount denied"
                    .to_string(),
            )
        })
        .expect_err("diagnosed monitor exit must not be retried");
        assert_eq!(attempts, 1);
        assert!(error.contains("mount denied"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_linux_launch_diagnostics_capture_status_and_stderr() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf 'mount denied' >&2; exit 42"])
            .stderr(Stdio::piped())
            .spawn()
            .expect("diagnostic fixture");
        child.wait().expect("diagnostic fixture exit");

        let diagnostic = append_linux_launch_diagnostics("launch failed".to_string(), &mut child);

        assert!(
            diagnostic.contains("bubblewrap monitor status:"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("42"), "{diagnostic}");
        assert!(
            diagnostic.contains("bubblewrap stderr: mount denied"),
            "{diagnostic}"
        );
    }

    #[test]
    fn process_identity_rejects_pid_reuse_markers() {
        let identity = ProcessIdentity::current().expect("current identity");
        assert!(identity.still_matches());
        let mut forged = identity;
        forged.start_marker.push_str("-reused");
        assert!(!forged.still_matches());
    }

    #[test]
    fn scope_transitions_require_exact_generation_and_cleanup_proof() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let owner = ProcessIdentity::current().expect("owner identity");
        let record = store
            .prepare(
                "sub-test",
                "subagent",
                41,
                owner.clone(),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let lease = store.acquire_cleanup_lease(&record).expect("cleanup lease");
        let error = store
            .mutate(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                |record| {
                    record.status = ProcessScopeStatus::CleanupInProgress;
                    Ok(())
                },
            )
            .expect_err("central transition validator rejects prepared cleanup");
        assert!(error.contains("not a legal monotonic successor"), "{error}");
        let error = store
            .begin_cleanup(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "not-running",
            )
            .expect_err("prepared scope cannot enter cleanup");
        assert!(error.contains("cannot begin cleanup from status Prepared"));
        assert_eq!(
            store
                .load(&record.scope_id)
                .expect("prepared scope remains"),
            record
        );
        let child = owner;
        let running = store
            .mark_running(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                ProcessIdentity::current().expect("supervisor identity"),
                child,
            )
            .expect("mark running");
        assert_eq!(running.status, ProcessScopeStatus::Running);
        assert!(store
            .begin_cleanup(
                &record.scope_id,
                record.execution_generation + 1,
                &record.cleanup_lease_id,
                "stale",
            )
            .is_err());
        store
            .begin_cleanup(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "owner_eof",
            )
            .expect("begin cleanup");
        assert!(store
            .complete_cleanup(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "owner_eof",
                false,
            )
            .is_err());
        let complete = store
            .complete_cleanup(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "owner_eof",
                true,
            )
            .expect("complete cleanup");
        let proof = complete.cleanup_proof.as_ref().expect("cleanup proof");
        lease.release_after_proof(proof).expect("release lease");
        assert_eq!(store.load("sub-test").expect("reload"), complete);
    }

    #[test]
    fn live_cleanup_lease_excludes_a_second_supervisor() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let record = store
            .prepare(
                "sub-live",
                "subagent",
                9,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let _lease = store.acquire_cleanup_lease(&record).expect("cleanup lease");
        assert_eq!(
            store
                .cleanup_lease_state(&record)
                .expect("inspect live cleanup lease"),
            CleanupLeaseState::Live
        );
        let directory = crate::daemons::state::StableDirectory::open(&store.directory)
            .expect("stable scope directory");
        let path = store
            .cleanup_lease_path(&record.scope_id)
            .expect("cleanup lease path");
        let readable = directory.open_read(&path).expect("open live cleanup lease");
        let observed: CleanupLeaseRecord =
            read_bounded_json(&readable, &path).expect("read live cleanup lease");
        assert_eq!(observed.scope_id, record.scope_id);
        let error = store
            .acquire_cleanup_lease(&record)
            .err()
            .expect("second cleanup owner must be excluded");
        assert!(error.contains("already live"), "{error}");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn linux_supervisor_loss_recovery_fails_closed_on_other_platforms() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let record = store
            .prepare(
                "sub-linux-recovery",
                "subagent",
                12,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");

        let error = store
            .recover_linux_supervisor_loss(&record)
            .expect_err("non-Linux hosts cannot recover a Linux process scope");

        assert!(error.contains("unavailable on this platform"), "{error}");
        assert_eq!(
            store.load(&record.scope_id).expect("scope remains intact"),
            record
        );
        assert_eq!(
            store
                .cleanup_lease_state(&record)
                .expect("cleanup lease state"),
            CleanupLeaseState::Missing
        );
    }

    #[test]
    fn stale_scope_snapshot_cannot_acquire_cleanup_lease() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let record = store
            .prepare(
                "sub-stale-snapshot",
                "subagent",
                10,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let mutated = store
            .mark_recovery_required(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "fixture mutation",
            )
            .expect("mutate authoritative scope");
        assert_eq!(mutated.status, ProcessScopeStatus::Prepared);
        assert_eq!(mutated.cleanup_reason.as_deref(), Some("fixture mutation"));
        let error = store
            .acquire_cleanup_lease(&record)
            .err()
            .expect("stale scope snapshot must be fenced");
        assert!(error.contains("scope changed"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepared_recovery_claims_lease_only_after_the_supervisor_exits() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let prepared = store
            .prepare(
                "sub-live-prepared-supervisor",
                "subagent",
                42,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let mut supervisor = Command::new("sh")
            .args(["-c", "sleep 60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn live supervisor");
        let supervisor_identity =
            ProcessIdentity::capture(supervisor.id()).expect("supervisor identity");
        let prepared = store
            .register_launch_supervisor(
                &prepared.scope_id,
                prepared.execution_generation,
                &prepared.cleanup_lease_id,
                supervisor_identity.clone(),
            )
            .expect("register supervisor");

        let started = Instant::now();
        let error = store
            .recover_linux_supervisor_loss(&prepared)
            .expect_err("live supervisor cannot enter prepared recovery");
        assert!(error.contains("still live"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            store.load(&prepared.scope_id).expect("preserved scope"),
            prepared
        );
        assert_eq!(
            store
                .cleanup_lease_state(&prepared)
                .expect("pre-recovery lease state"),
            CleanupLeaseState::Missing
        );

        supervisor.kill().expect("kill supervisor");
        supervisor.wait().expect("reap supervisor");
        let completed = store
            .recover_linux_supervisor_loss(&prepared)
            .expect("recover stopped pre-lease supervisor");
        assert_eq!(completed.status, ProcessScopeStatus::Complete);
        let proof = completed
            .launch_abort_proof
            .as_ref()
            .expect("launch-abort proof");
        assert_eq!(proof.supervisor, supervisor_identity);
        assert!(proof.namespace_root.is_none());
        assert!(proof.workload_never_launched);
        assert_eq!(
            store
                .cleanup_lease_state(&completed)
                .expect("released recovery lease"),
            CleanupLeaseState::Missing
        );
    }

    #[test]
    fn caller_supplied_cleanup_proof_cannot_release_a_noncomplete_scope() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let owner = ProcessIdentity::current().expect("owner identity");
        let record = store
            .prepare(
                "sub-forged-proof",
                "subagent",
                11,
                owner.clone(),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let lease = store.acquire_cleanup_lease(&record).expect("cleanup lease");
        let running = store
            .mark_running(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                owner.clone(),
                owner.clone(),
            )
            .expect("mark running");
        let forged = CleanupProof {
            execution_generation: record.execution_generation,
            cleanup_lease_id: record.cleanup_lease_id.clone(),
            backend: record.backend,
            direct_child: owner,
            outcome: "forged".to_string(),
            descendants_reaped: true,
            completed_at: Utc::now(),
        };
        let error = lease
            .release_after_proof(&forged)
            .expect_err("noncomplete scope must not release cleanup ownership");
        assert!(error.contains("authoritative completed"), "{error}");
        assert_eq!(
            store
                .cleanup_lease_state(&running)
                .expect("cleanup lease state"),
            CleanupLeaseState::Recoverable
        );
    }

    #[test]
    fn scope_store_recovers_atomic_evacuation_before_reading() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let record = store
            .prepare(
                "sub-atomic-recovery",
                "subagent",
                12,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let directory = crate::daemons::state::StableDirectory::open(&store.directory)
            .expect("stable scope directory");
        let target = store.record_path(&record.scope_id).expect("record path");
        let previous = directory
            .deterministic_previous_artifact_path(&target, SCOPE_WRITE_PREFIX)
            .expect("previous path");
        std::fs::rename(&target, &previous).expect("simulate crash after evacuation");

        assert_eq!(
            store.load(&record.scope_id).expect("recovered record"),
            record
        );
        assert!(target.is_file());
        assert!(!previous.exists());

        let committed = store
            .mark_recovery_required(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "committed successor",
            )
            .expect("publish committed successor");
        std::fs::write(
            &previous,
            serde_json::to_vec_pretty(&record).expect("encode prior record"),
        )
        .expect("simulate retained evacuated prior");
        assert_eq!(
            store
                .load(&record.scope_id)
                .expect("finalize committed record"),
            committed
        );
        assert!(!previous.exists());

        let temporary = directory
            .deterministic_artifact_path(&target, SCOPE_WRITE_PREFIX, ".tmp")
            .expect("temporary path");
        std::fs::remove_file(&target).expect("remove visible successor for crash fixture");
        std::fs::write(
            &previous,
            serde_json::to_vec_pretty(&record).expect("encode previous record"),
        )
        .expect("write previous record");
        std::fs::write(
            &temporary,
            serde_json::to_vec_pretty(&committed).expect("encode temporary successor"),
        )
        .expect("write temporary successor");
        std::fs::hard_link(&temporary, &target).expect("publish linked successor");
        let peak_entries = 3;
        let peak_name_bytes = [&previous, &temporary, &target]
            .iter()
            .map(|path| {
                path.file_name()
                    .expect("transaction filename")
                    .as_encoded_bytes()
                    .len()
            })
            .sum();
        let peak_bytes = [&previous, &temporary, &target]
            .iter()
            .map(|path| std::fs::metadata(path).expect("transaction metadata").len())
            .sum();
        maintain_process_scope_directory_with_limits(
            &directory,
            false,
            ProcessScopeDirectoryLimits {
                max_entries: peak_entries,
                max_name_bytes: peak_name_bytes,
                max_bytes: peak_bytes,
                ..PROCESS_SCOPE_DIRECTORY_LIMITS
            },
        )
        .expect("recover transaction at its exact reserved peak");
        assert_eq!(
            store.load(&record.scope_id).expect("recovered successor"),
            committed
        );
        assert!(!previous.exists());
        assert!(!temporary.exists());
    }

    #[test]
    fn completed_scope_quarantines_recover_and_retire_from_embedded_proof() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let identity = ProcessIdentity::current().expect("process identity");
        let record = store
            .prepare(
                "sub-retire",
                "subagent",
                13,
                identity.clone(),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let lease = store.acquire_cleanup_lease(&record).expect("cleanup lease");
        store
            .mark_running(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                identity.clone(),
                identity,
            )
            .expect("running scope");
        store
            .begin_cleanup(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "fixture",
            )
            .expect("begin cleanup");
        let complete = store
            .complete_cleanup(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "fixture",
                true,
            )
            .expect("complete cleanup");
        let proof = complete.cleanup_proof.clone().expect("cleanup proof");
        let directory = crate::daemons::state::StableDirectory::open(&store.directory)
            .expect("stable scope directory");
        let lease_path = store
            .cleanup_lease_path(&record.scope_id)
            .expect("lease path");
        let lease_quarantine = directory
            .deterministic_artifact_path(&lease_path, CLEANUP_LEASE_DELETE_PREFIX, ".quarantine")
            .expect("lease quarantine");
        std::fs::rename(&lease_path, &lease_quarantine)
            .expect("simulate crash after lease quarantine");
        assert_eq!(
            store
                .cleanup_lease_state(&complete)
                .expect("inspect live quarantined lease"),
            CleanupLeaseState::Live
        );
        assert!(lease_quarantine.exists());
        assert!(store.acquire_cleanup_lease(&complete).is_err());
        assert!(store
            .retire_complete(&record.scope_id, record.execution_generation, &proof)
            .is_err());
        drop(lease);

        assert_eq!(
            store
                .cleanup_lease_state(&complete)
                .expect("recover lease quarantine"),
            CleanupLeaseState::Missing
        );
        assert!(!lease_quarantine.exists());

        let scope_path = store.record_path(&record.scope_id).expect("scope path");
        let scope_quarantine = directory
            .deterministic_artifact_path(&scope_path, SCOPE_DELETE_PREFIX, ".quarantine")
            .expect("scope quarantine");
        std::fs::rename(&scope_path, &scope_quarantine)
            .expect("simulate crash after scope quarantine");
        assert!(store
            .retire_complete(&record.scope_id, record.execution_generation + 1, &proof)
            .is_err());
        assert!(scope_quarantine.exists());

        let mut wrong_proof = proof.clone();
        wrong_proof.outcome.push_str("-wrong");
        assert!(store
            .retire_complete(&record.scope_id, record.execution_generation, &wrong_proof)
            .is_err());
        assert!(scope_quarantine.exists());

        assert!(store
            .retire_complete(&record.scope_id, record.execution_generation, &proof)
            .expect("retire completed scope"));
        assert!(!scope_quarantine.exists());
        assert!(store
            .try_load(&record.scope_id)
            .expect("retired scope lookup")
            .is_none());
    }

    #[test]
    fn process_scope_maintenance_enforces_aggregate_limits_and_unknown_entries() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        store
            .prepare(
                "sub-bounded",
                "subagent",
                14,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let directory = crate::daemons::state::StableDirectory::open(&store.directory)
            .expect("stable scope directory");
        let tiny_record_limit = ProcessScopeDirectoryLimits {
            max_records: 1,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        };
        let error =
            maintain_process_scope_directory_with_limits(&directory, true, tiny_record_limit)
                .expect_err("reserved scope must exceed the record cap");
        assert!(error.contains("record limit"), "{error}");
        let tiny_byte_limit = ProcessScopeDirectoryLimits {
            max_bytes: 1,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        };
        let error =
            maintain_process_scope_directory_with_limits(&directory, false, tiny_byte_limit)
                .expect_err("aggregate bytes must be bounded");
        assert!(error.contains("aggregate limit"), "{error}");

        std::fs::write(store.directory.join("foreign-state"), b"unknown")
            .expect("unknown state fixture");
        let error = store
            .prepare(
                "sub-rejected",
                "subagent",
                15,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect_err("unknown directory entries must fail closed");
        assert!(error.contains("unknown entry"), "{error}");
    }

    #[test]
    fn version_one_scope_state_is_preserved_without_blocking_new_generations() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let mut legacy = store
            .prepare(
                "sub-version-one",
                "subagent",
                151,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare legacy fixture");
        let lease = store
            .acquire_cleanup_lease(&legacy)
            .expect("legacy cleanup lease");
        drop(lease);
        legacy.version = 1;
        let scope_path = store.record_path(&legacy.scope_id).expect("scope path");
        std::fs::write(
            &scope_path,
            serde_json::to_vec_pretty(&legacy).expect("encode legacy scope"),
        )
        .expect("write legacy scope");
        let lease_path = store
            .cleanup_lease_path(&legacy.scope_id)
            .expect("lease path");
        let mut legacy_lease: CleanupLeaseRecord =
            serde_json::from_slice(&std::fs::read(&lease_path).expect("read cleanup lease"))
                .expect("decode cleanup lease");
        legacy_lease.version = 1;
        std::fs::write(
            &lease_path,
            serde_json::to_vec_pretty(&legacy_lease).expect("encode legacy lease"),
        )
        .expect("write legacy lease");
        let directory = crate::daemons::state::StableDirectory::open(&store.directory)
            .expect("stable scope directory");
        let scope_previous = directory
            .deterministic_previous_artifact_path(&scope_path, SCOPE_WRITE_PREFIX)
            .expect("legacy scope previous path");
        let scope_temporary = directory
            .deterministic_artifact_path(&scope_path, SCOPE_WRITE_PREFIX, ".tmp")
            .expect("legacy scope temporary path");
        let legacy_scope_bytes =
            serde_json::to_vec_pretty(&legacy).expect("encode legacy scope transaction");
        std::fs::write(&scope_previous, &legacy_scope_bytes)
            .expect("write legacy scope previous artifact");
        std::fs::write(&scope_temporary, &legacy_scope_bytes)
            .expect("write legacy scope temporary artifact");
        let miskeyed_target = store
            .record_path("sub-miskeyed-version-one")
            .expect("miskeyed legacy target");
        let miskeyed_temporary = directory
            .deterministic_artifact_path(&miskeyed_target, SCOPE_WRITE_PREFIX, ".tmp")
            .expect("miskeyed legacy temporary path");
        std::fs::write(&miskeyed_temporary, &legacy_scope_bytes)
            .expect("write miskeyed legacy temporary artifact");
        let lease_temporary = directory
            .deterministic_artifact_path(&lease_path, CLEANUP_LEASE_WRITE_PREFIX, ".tmp")
            .expect("legacy lease temporary path");
        std::fs::write(
            &lease_temporary,
            serde_json::to_vec_pretty(&legacy_lease).expect("encode legacy lease temporary"),
        )
        .expect("write legacy lease temporary artifact");

        let error = store
            .load(&legacy.scope_id)
            .expect_err("legacy scope cannot be interpreted as version two");
        assert!(error.contains("unsupported managed-process scope version 1"));
        assert!(scope_previous.exists());
        assert!(scope_temporary.exists());
        assert!(lease_temporary.exists());
        store
            .prepare(
                "sub-after-version-one",
                "subagent",
                152,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("unrelated version-two scope remains available");
        assert!(scope_path.exists());
        assert!(lease_path.exists());
        assert!(scope_previous.exists());
        assert!(scope_temporary.exists());
        assert!(lease_temporary.exists());
        assert!(!miskeyed_temporary.exists());
    }

    #[test]
    fn process_scope_publications_reserve_all_limits_before_writing() {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let mut oversized_owner = ProcessIdentity::current().expect("owner identity");
        oversized_owner.start_marker = "x".repeat(MAX_PROCESS_IDENTITY_MARKER_BYTES + 1);
        let error = store
            .prepare(
                "sub-oversized-owner",
                "subagent",
                16,
                oversized_owner,
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect_err("oversized identities must fail before publication");
        assert!(error.contains("owner identity"), "{error}");
        assert!(!store
            .record_path("sub-oversized-owner")
            .expect("record path")
            .exists());

        let record = store
            .prepare(
                "sub-publication-budget",
                "subagent",
                17,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare bounded scope");
        let error = store
            .mark_recovery_required(
                &record.scope_id,
                record.execution_generation,
                &record.cleanup_lease_id,
                "x".repeat(MAX_PROCESS_CLEANUP_TEXT_BYTES + 1),
            )
            .expect_err("oversized mutation must fail before publication");
        assert!(error.contains("cleanup reason"), "{error}");
        assert_eq!(
            store.load(&record.scope_id).expect("unchanged scope"),
            record
        );

        let directory = crate::daemons::state::StableDirectory::open(&store.directory)
            .expect("stable scope directory");
        let usage =
            process_scope_directory_usage_with_limits(&directory, PROCESS_SCOPE_DIRECTORY_LIMITS)
                .expect("scope directory usage");
        let lease_record = CleanupLeaseRecord {
            version: PROCESS_SCOPE_VERSION,
            scope_id: record.scope_id.clone(),
            execution_generation: record.execution_generation,
            cleanup_lease_id: record.cleanup_lease_id.clone(),
        };
        let lease_bytes =
            encode_process_state_bounded(&lease_record, "cleanup lease").expect("encode lease");
        let lease_path = store
            .cleanup_lease_path(&record.scope_id)
            .expect("cleanup lease path");
        let lease_temporary = directory
            .deterministic_artifact_path(&lease_path, CLEANUP_LEASE_WRITE_PREFIX, ".tmp")
            .expect("lease temporary path");
        let lease_transaction_name_bytes = lease_temporary
            .file_name()
            .expect("lease temporary name")
            .as_encoded_bytes()
            .len()
            + lease_path
                .file_name()
                .expect("lease target name")
                .as_encoded_bytes()
                .len();
        let lease_peak_bytes = usage.bytes + 2 * lease_bytes.len() as u64;
        let lease_peak_entries = usage.entries + 2;
        let lease_peak_name_bytes = usage.name_bytes + lease_transaction_name_bytes;

        ensure_process_scope_publication_budget(
            &directory,
            &lease_path,
            lease_bytes.len() as u64,
            ProcessAtomicKind::CleanupLease,
            ProcessScopeDirectoryLimits {
                max_bytes: lease_peak_bytes,
                max_entries: lease_peak_entries,
                max_name_bytes: lease_peak_name_bytes,
                ..PROCESS_SCOPE_DIRECTORY_LIMITS
            },
        )
        .expect("exact cleanup-lease transaction peak is admitted");

        let byte_limits = ProcessScopeDirectoryLimits {
            max_bytes: lease_peak_bytes - 1,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        };
        let error = ensure_process_scope_publication_budget(
            &directory,
            &lease_path,
            lease_bytes.len() as u64,
            ProcessAtomicKind::CleanupLease,
            byte_limits,
        )
        .expect_err("prospective aggregate bytes must be reserved");
        assert!(error.contains("aggregate limit"), "{error}");

        let entry_limits = ProcessScopeDirectoryLimits {
            max_entries: lease_peak_entries - 1,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        };
        let error = ensure_process_scope_publication_budget(
            &directory,
            &lease_path,
            lease_bytes.len() as u64,
            ProcessAtomicKind::CleanupLease,
            entry_limits,
        )
        .expect_err("prospective entries must be reserved");
        assert!(error.contains("entry limit"), "{error}");

        let name_limits = ProcessScopeDirectoryLimits {
            max_name_bytes: lease_peak_name_bytes - 1,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        };
        let error = ensure_process_scope_publication_budget(
            &directory,
            &lease_path,
            lease_bytes.len() as u64,
            ProcessAtomicKind::CleanupLease,
            name_limits,
        )
        .expect_err("prospective filename bytes must be reserved");
        assert!(error.contains("filename limit"), "{error}");

        let scope_path = store.record_path(&record.scope_id).expect("scope path");
        let scope_bytes = encode_process_state_bounded(&record, "scope record")
            .expect("encode replacement scope");
        let scope_temporary = directory
            .deterministic_artifact_path(&scope_path, SCOPE_WRITE_PREFIX, ".tmp")
            .expect("scope temporary path");
        let scope_previous = directory
            .deterministic_previous_artifact_path(&scope_path, SCOPE_WRITE_PREFIX)
            .expect("scope previous path");
        let scope_peak_name_bytes = usage.name_bytes
            + scope_temporary
                .file_name()
                .expect("scope temporary name")
                .as_encoded_bytes()
                .len()
            + scope_previous
                .file_name()
                .expect("scope previous name")
                .as_encoded_bytes()
                .len();
        ensure_process_scope_publication_budget(
            &directory,
            &scope_path,
            scope_bytes.len() as u64,
            ProcessAtomicKind::Scope,
            ProcessScopeDirectoryLimits {
                max_bytes: usage.bytes + 2 * scope_bytes.len() as u64,
                max_entries: usage.entries + 2,
                max_name_bytes: scope_peak_name_bytes,
                ..PROCESS_SCOPE_DIRECTORY_LIMITS
            },
        )
        .expect("exact replacement transaction peak is admitted");
        let error = ensure_process_scope_publication_budget(
            &directory,
            &scope_path,
            scope_bytes.len() as u64,
            ProcessAtomicKind::Scope,
            ProcessScopeDirectoryLimits {
                max_bytes: usage.bytes + 2 * scope_bytes.len() as u64 - 1,
                ..PROCESS_SCOPE_DIRECTORY_LIMITS
            },
        )
        .expect_err("replacement scratch bytes must be reserved");
        assert!(error.contains("aggregate limit"), "{error}");
        let error = ensure_process_scope_publication_budget(
            &directory,
            &scope_path,
            scope_bytes.len() as u64,
            ProcessAtomicKind::Scope,
            ProcessScopeDirectoryLimits {
                max_entries: usage.entries + 1,
                ..PROCESS_SCOPE_DIRECTORY_LIMITS
            },
        )
        .expect_err("replacement scratch entries must be reserved");
        assert!(error.contains("entry limit"), "{error}");
        let error = ensure_process_scope_publication_budget(
            &directory,
            &scope_path,
            scope_bytes.len() as u64,
            ProcessAtomicKind::Scope,
            ProcessScopeDirectoryLimits {
                max_name_bytes: scope_peak_name_bytes - 1,
                ..PROCESS_SCOPE_DIRECTORY_LIMITS
            },
        )
        .expect_err("replacement scratch filenames must be reserved");
        assert!(error.contains("filename limit"), "{error}");

        let next_scope_path = store
            .record_path("sub-next-scope")
            .expect("next scope path");
        let record_limits = ProcessScopeDirectoryLimits {
            max_records: usage.records,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        };
        let error = ensure_process_scope_publication_budget(
            &directory,
            &next_scope_path,
            1,
            ProcessAtomicKind::Scope,
            record_limits,
        )
        .expect_err("prospective scope records must be reserved");
        assert!(error.contains("record limit"), "{error}");
        assert!(!lease_path.exists());
        assert!(encode_process_state_bounded(
            &"x".repeat(MAX_SCOPE_RECORD_BYTES as usize),
            "oversized fixture"
        )
        .is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn launch_gate_preserves_payload_stdin_bytes() {
        use std::os::unix::net::UnixStream;

        if !crate::sandbox::detect_capabilities().managed_process_available {
            assert!(
                std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
                "CI requires a usable bwrap PID namespace"
            );
            return;
        }
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let record = store
            .prepare(
                "sub-gated-stdin",
                "subagent",
                88,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare gated stdin scope");
        let launched = root.path().join("payload-launched");
        let payload = b"nib-launch\npayload after the internal gate\n";
        let (owner_read, owner_write) = UnixStream::pair().expect("owner lifetime pipe");
        let output = supervise_foreground_with_ready(
            &store,
            &record,
            owner_read,
            SupervisedCommand {
                program: PathBuf::from("/bin/sh"),
                args: vec![
                    OsString::from("-c"),
                    OsString::from("printf launched > \"$1\"; cat"),
                    OsString::from("nib-gated-stdin-fixture"),
                    launched.as_os_str().to_os_string(),
                ],
                cwd: root.path().to_path_buf(),
                stdin: payload.to_vec(),
                environment: Vec::new(),
            },
            |running| {
                if running.status != ProcessScopeStatus::Running
                    || store.load(&running.scope_id)?.status != ProcessScopeStatus::Running
                    || launched.exists()
                {
                    return Err(
                        "launch gate was released before durable Running publication".to_string(),
                    );
                }
                Ok(())
            },
        )
        .expect("supervise gated stdin fixture");

        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, payload);
        assert_eq!(
            std::fs::read(&launched).expect("payload launch marker"),
            b"launched"
        );
        assert!(output.cleanup_proof.descendants_reaped);
        drop(owner_write);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_bwrap_info_handshake_reaps_the_unpublished_namespace() {
        if !crate::sandbox::detect_capabilities().managed_process_available {
            assert!(
                std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
                "CI requires a usable bwrap PID namespace"
            );
            return;
        }
        let root = git_project();
        let token = format!("nib-info-failure-{}", uuid::Uuid::new_v4());
        let error = spawn_supervised_command_inner(
            ProcessScopeBackend::LinuxPidNamespace,
            &SupervisedCommand {
                program: PathBuf::from("/bin/sh"),
                args: vec![
                    OsString::from("-c"),
                    OsString::from(format!("sleep 60 # {token}")),
                ],
                cwd: root.path().to_path_buf(),
                stdin: Vec::new(),
                environment: Vec::new(),
            },
            true,
            false,
        )
        .err()
        .expect("injected info failure");
        assert!(error.contains("injected bubblewrap namespace information failure"));
        let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
        while linux_process_command_contains(&token) && Instant::now() < deadline {
            thread::sleep(SUPERVISOR_POLL_INTERVAL);
        }
        assert!(
            !linux_process_command_contains(&token),
            "failed info handshake left its gated bubblewrap process alive"
        );
    }

    #[cfg(target_os = "linux")]
    fn linux_process_command_contains(token: &str) -> bool {
        let Ok(processes) = std::fs::read_dir("/proc") else {
            return false;
        };
        processes.flatten().any(|entry| {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
                .map(str::to_string)
            else {
                return false;
            };
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|command| {
                    String::from_utf8_lossy(&command)
                        .split('\0')
                        .any(|argument| argument.contains(token))
                })
                .unwrap_or(false)
        })
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owner_eof_reaps_a_setsid_descendant_before_cleanup_proof() {
        use std::os::unix::net::UnixStream;

        if !crate::sandbox::detect_capabilities().managed_process_available {
            assert!(
                std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
                "CI requires a usable bwrap PID namespace"
            );
            return;
        }
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let record = store
            .prepare(
                "sub-owner-eof",
                "subagent",
                77,
                ProcessIdentity::current().expect("owner identity"),
                ProcessScopeBackend::LinuxPidNamespace,
            )
            .expect("prepare scope");
        let descendant_path = root.path().join("descendant.started");
        let survived_path = root.path().join("descendant.survived");
        let command = format!(
            "setsid sh -c 'printf started > {}; sleep 2; printf survived > {}' & wait",
            descendant_path.display(),
            survived_path.display(),
        );
        let (owner_read, owner_write) = UnixStream::pair().expect("owner EOF pipe");
        let worker_store = store.clone();
        let worker_record = record.clone();
        let worker_root = root.path().to_path_buf();
        let supervisor = std::thread::spawn(move || {
            supervise_foreground(
                &worker_store,
                &worker_record,
                owner_read,
                SupervisedCommand {
                    program: PathBuf::from("/bin/sh"),
                    args: vec![OsString::from("-c"), OsString::from(command)],
                    cwd: worker_root,
                    stdin: Vec::new(),
                    environment: Vec::new(),
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !descendant_path.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            descendant_path.is_file(),
            "the real setsid descendant must start before owner EOF"
        );
        drop(owner_write);

        let output = supervisor
            .join()
            .expect("supervisor thread")
            .expect("supervised output");
        assert!(output.owner_lost);
        assert!(output.cleanup_proof.descendants_reaped);
        std::thread::sleep(Duration::from_millis(2200));
        assert!(!survived_path.exists());
        assert_eq!(
            store.load(&record.scope_id).expect("scope record").status,
            ProcessScopeStatus::Complete
        );
        assert_eq!(
            store
                .cleanup_lease_state(&store.load(&record.scope_id).expect("scope record"))
                .expect("cleanup lease state"),
            CleanupLeaseState::Missing
        );
    }
}
