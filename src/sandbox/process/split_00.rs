//! T043 split.

use super::*;

// Version 2 records bind `direct_child` to the Linux namespace init rather than
// the outer bubblewrap monitor, so version 1 state must fail closed on recovery.
pub(crate) const PROCESS_SCOPE_VERSION: u32 = 2;
pub(crate) const MAX_SCOPE_RECORD_BYTES: u64 = 256 * 1024;
#[cfg(windows)]
pub(crate) const CLEANUP_LEASE_LOCK_OFFSET: u64 = MAX_SCOPE_RECORD_BYTES + 1;
pub(crate) const MAX_PROCESS_IDENTITY_MARKER_BYTES: usize = 1024;
pub(crate) const MAX_PROCESS_CLEANUP_TEXT_BYTES: usize = 32 * 1024;
pub(crate) const MAX_PROCESS_SCOPE_RECORDS: usize = 10_000;
pub(crate) const MAX_PROCESS_SCOPE_DIRECTORY_ENTRIES: usize = MAX_PROCESS_SCOPE_RECORDS * 3 + 512;
pub(crate) const MAX_PROCESS_SCOPE_DIRECTORY_NAME_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_PROCESS_SCOPE_DIRECTORY_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const SCOPE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const SCOPE_DIRECTORY: &str = "process-scopes";
pub(crate) const SCOPE_STORE_LOCK: &str = "process-scopes.lock";
pub(crate) const CLEANUP_LEASE_SUFFIX: &str = ".cleanup.lease";
pub(crate) const SCOPE_WRITE_PREFIX: &str = ".nib-daemon-";
pub(crate) const CLEANUP_LEASE_WRITE_PREFIX: &str = ".nib-process-cleanup-lease-write-";
pub(crate) const CLEANUP_LEASE_DELETE_PREFIX: &str = ".nib-process-cleanup-lease-delete-";
pub(crate) const SCOPE_DELETE_PREFIX: &str = ".nib-process-scope-delete-";
pub(crate) const LAUNCH_ABORT_OUTCOME: &str = "gate_eof_before_running";
pub(crate) const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_millis(10);
pub(crate) const MAINTENANCE_REGISTRY_POLL_INTERVAL: Duration = Duration::from_millis(5);
pub(crate) const SUPERVISOR_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MAX_SUPERVISED_OUTPUT_BYTES: usize = 1024 * 1024;
#[cfg(debug_assertions)]
pub(crate) const PRE_RUNNING_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_PRE_RUNNING_PAUSE";
#[cfg(debug_assertions)]
pub(crate) const PRE_SPAWN_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_PRE_SPAWN_PAUSE";
#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) const POST_BWRAP_SPAWN_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_POST_BWRAP_SPAWN_PAUSE";
#[cfg(target_os = "linux")]
pub(crate) const MAX_BWRAP_INFO_BYTES: u64 = 16 * 1024;
#[cfg(target_os = "linux")]
pub(crate) const LINUX_LAUNCH_READY_FRAME: &[u8] = b"nib-ready\n";
#[cfg(target_os = "linux")]
pub(crate) const LINUX_LAUNCH_FRAME: &[u8] = b"nib-launch\n";
#[cfg(target_os = "linux")]
pub(crate) const LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS: usize = 3;
#[cfg(target_os = "linux")]
pub(crate) const LINUX_LAUNCH_GATE_SCRIPT: &str = "\
if ! printf 'nib-ready\\n'; then exit 125; fi
if ! IFS= read -r nib_gate; then exit 125; fi
if [ \"$nib_gate\" != 'nib-launch' ]; then exit 125; fi
exec \"$@\"";

#[derive(Clone, Copy)]
pub(crate) struct ProcessScopeDirectoryLimits {
    pub(crate) max_records: usize,
    pub(crate) max_entries: usize,
    pub(crate) max_name_bytes: usize,
    pub(crate) max_bytes: u64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ProcessScopeDirectoryUsage {
    pub(crate) records: usize,
    pub(crate) entries: usize,
    pub(crate) name_bytes: usize,
    pub(crate) bytes: u64,
}

pub(crate) const PROCESS_SCOPE_DIRECTORY_LIMITS: ProcessScopeDirectoryLimits =
    ProcessScopeDirectoryLimits {
        max_records: MAX_PROCESS_SCOPE_RECORDS,
        max_entries: MAX_PROCESS_SCOPE_DIRECTORY_ENTRIES,
        max_name_bytes: MAX_PROCESS_SCOPE_DIRECTORY_NAME_BYTES,
        max_bytes: MAX_PROCESS_SCOPE_DIRECTORY_BYTES,
    };

pub(crate) type ProcessScopeMaintenanceRegistry = Mutex<std::collections::HashSet<PathBuf>>;

pub(crate) static MAINTAINED_PROCESS_SCOPE_STORES: LazyLock<ProcessScopeMaintenanceRegistry> =
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
        if !crate::sandbox::protected_owner::protected_owner_policy_is_configured() {
            return Err("protected cleanup owner policy is not configured".to_string());
        }
        #[cfg(target_os = "linux")]
        {
            Self::current()
        }
        #[cfg(windows)]
        {
            crate::sandbox::protected_owner::require_windows_protected_owner()?;
            Err(
                "production subagent supervision is unavailable on Windows until native qualification of the protected cleanup owner"
                    .to_string(),
            )
        }
        #[cfg(target_os = "macos")]
        {
            crate::sandbox::protected_owner::require_macos_protected_reaper()?;
            Err(
                "production subagent supervision is unavailable on macOS until native qualification of the protected cleanup owner"
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
    /// Immutable nonce authorizing the hidden supervisor to bind its own OS
    /// identity before it reads any parent-controlled launch data. Legacy and
    /// non-subagent scopes do not carry this authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_registration_nonce: Option<String>,
    pub owner: ProcessIdentity,
    pub backend: ProcessScopeBackend,
    pub status: ProcessScopeStatus,
    /// `None` is the legacy v2 encoding, whose `Running` state already meant
    /// the OS launch gate had been released. New supervisors persist
    /// `Some(false)` while the exact child is running behind its gate and
    /// advance it monotonically to `Some(true)` before releasing that gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_committed: Option<bool>,
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

#[derive(Debug)]
pub struct ProcessScopeStore {
    pub(crate) project_root: PathBuf,
    pub(crate) directory: PathBuf,
    pub(crate) directory_capability: crate::daemons::state::StableDirectory,
    pub(crate) records_binding: Option<crate::daemons::state::StableDirectory>,
    pub(crate) lock_deadline: Option<Instant>,
    pub(crate) operation_timeout: Duration,
}

pub(crate) enum CompletionAuthority<'a> {
    Cleanup(&'a CleanupProof),
    LaunchAbort(&'a LaunchAbortProof),
}

impl CompletionAuthority<'_> {
    pub(crate) fn matches(&self, record: &ProcessScopeRecord, execution_generation: u64) -> bool {
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
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn try_clone(&self) -> Result<Self, String> {
        Ok(Self {
            project_root: self.project_root.clone(),
            directory: self.directory.clone(),
            directory_capability: self.directory_capability.try_clone()?,
            records_binding: self
                .records_binding
                .as_ref()
                .map(crate::daemons::state::StableDirectory::try_clone)
                .transpose()?,
            lock_deadline: self.lock_deadline,
            operation_timeout: self.operation_timeout,
        })
    }

    pub fn open(project_root: &Path) -> Result<Self, String> {
        Self::open_with_optional_lock_deadline(project_root, None)
    }

    pub(crate) fn open_with_lock_deadline(
        project_root: &Path,
        deadline: Instant,
    ) -> Result<Self, String> {
        Self::open_with_optional_lock_deadline(project_root, Some(deadline))
    }

    pub(crate) fn open_with_optional_lock_deadline(
        project_root: &Path,
        lock_deadline: Option<Instant>,
    ) -> Result<Self, String> {
        Self::open_with_optional_lock_deadline_and_setup_hook(
            project_root,
            lock_deadline,
            || Ok(()),
        )
    }

    #[cfg(test)]
    pub(crate) fn open_with_lock_deadline_and_setup_hook(
        project_root: &Path,
        deadline: Instant,
        before_setup_step: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        Self::open_with_optional_lock_deadline_and_setup_hook(
            project_root,
            Some(deadline),
            before_setup_step,
        )
    }

    pub(crate) fn open_with_optional_lock_deadline_and_setup_hook(
        project_root: &Path,
        lock_deadline: Option<Instant>,
        mut before_setup_step: impl FnMut() -> Result<(), String>,
    ) -> Result<Self, String> {
        ensure_process_scope_deadline(lock_deadline)?;
        let project_root = project_root.canonicalize().map_err(|error| {
            format!(
                "failed to resolve managed-process project root {}: {error}",
                project_root.display()
            )
        })?;
        ensure_process_scope_deadline(lock_deadline)?;
        let nib = project_root.join(".nib");
        let directory = nib.join(SCOPE_DIRECTORY);
        let directory_capability = if lock_deadline.is_some() {
            let project_directory = crate::daemons::state::StableDirectory::open(&project_root)?;
            let mut setup_guard = || {
                before_setup_step()?;
                ensure_process_scope_deadline(lock_deadline)
            };
            let nib_directory = project_directory
                .open_or_create_descendant_directory_with_guard(&nib, &mut setup_guard, |_| Ok(()))
                .map_err(|error| format!("managed-process state root is unsafe: {error}"))?;
            setup_guard()?;
            nib_directory
                .open_or_create_descendant_directory_with_guard(
                    &directory,
                    &mut setup_guard,
                    |_| Ok(()),
                )
                .map_err(|error| format!("managed-process scope directory is unsafe: {error}"))?
        } else {
            crate::fs_security::ensure_directory_without_symlinks(&nib)
                .map_err(|error| format!("managed-process state root is unsafe: {error}"))?;
            crate::fs_security::ensure_directory_without_symlinks(&directory)
                .map_err(|error| format!("managed-process scope directory is unsafe: {error}"))?;
            crate::daemons::state::StableDirectory::open(&directory)?
        };
        ensure_process_scope_deadline(lock_deadline)?;
        let store = Self {
            project_root,
            directory,
            directory_capability,
            records_binding: None,
            lock_deadline,
            operation_timeout: SCOPE_LOCK_TIMEOUT,
        };
        store.maintain_once_with_setup_hook(&mut before_setup_step)?;
        Ok(store)
    }

    /// Opens an existing process-scope namespace through the exact authorized
    /// subagent-records capability. This deliberately does not create or run
    /// cold maintenance: restart reconciliation must classify absence without
    /// ambient path probes or unrelated namespace mutation.
    pub(crate) fn open_existing_bound_to_records(
        project_root: &Path,
        records: &crate::daemons::state::StableDirectory,
        deadline: Instant,
    ) -> Result<Option<Self>, String> {
        ensure_process_scope_deadline(Some(deadline))?;
        records.verify_visible()?;
        let expected_records = project_root.join(".nib").join("subagents");
        if records.path() != expected_records {
            return Err(
                "managed-process records capability is bound to another project".to_string(),
            );
        }
        let nib_path = expected_records
            .parent()
            .ok_or("managed-process records capability has no state parent")?;
        let nib = crate::daemons::state::StableDirectory::open(nib_path)?;
        let rebound_records = nib.open_owned_child(&expected_records)?;
        if !rebound_records.same_identity(records) {
            return Err(
                "managed-process records capability changed before scope lookup".to_string(),
            );
        }
        let directory = nib_path.join(SCOPE_DIRECTORY);
        let directory_capability = match nib.entry_kind(&directory)? {
            None => {
                records.verify_visible()?;
                nib.verify_visible()?;
                ensure_process_scope_deadline(Some(deadline))?;
                return Ok(None);
            }
            Some(crate::daemons::state::StableEntryKind::Directory) => {
                nib.open_owned_child(&directory)?
            }
            Some(crate::daemons::state::StableEntryKind::File) => {
                return Err(format!(
                    "managed-process scope namespace is unsafe and was preserved: {}",
                    directory.display()
                ));
            }
        };
        records.verify_visible()?;
        nib.verify_visible()?;
        directory_capability.verify_visible()?;
        ensure_process_scope_deadline(Some(deadline))?;
        Ok(Some(Self {
            project_root: project_root.to_path_buf(),
            directory,
            directory_capability,
            records_binding: Some(records.try_clone()?),
            lock_deadline: Some(deadline),
            operation_timeout: SCOPE_LOCK_TIMEOUT,
        }))
    }

    pub fn prepare(
        &self,
        scope_id: &str,
        workload_kind: &str,
        execution_generation: u64,
        owner: ProcessIdentity,
        backend: ProcessScopeBackend,
    ) -> Result<ProcessScopeRecord, String> {
        self.prepare_with_launch_authority(
            scope_id,
            workload_kind,
            execution_generation,
            uuid::Uuid::new_v4().to_string(),
            None,
            owner,
            backend,
        )
    }

    pub(crate) fn prepare_subagent_launch(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        supervisor_registration_nonce: &str,
        owner: ProcessIdentity,
        backend: ProcessScopeBackend,
    ) -> Result<ProcessScopeRecord, String> {
        self.prepare_with_launch_authority(
            scope_id,
            "subagent",
            execution_generation,
            cleanup_lease_id.to_string(),
            Some(supervisor_registration_nonce.to_string()),
            owner,
            backend,
        )
    }

    // Keep the persisted scope authorities explicit at this lifecycle boundary;
    // grouping them would obscure which values are independently validated.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_with_launch_authority(
        &self,
        scope_id: &str,
        workload_kind: &str,
        execution_generation: u64,
        cleanup_lease_id: String,
        supervisor_registration_nonce: Option<String>,
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
            cleanup_lease_id,
            supervisor_registration_nonce,
            owner,
            backend,
            status: ProcessScopeStatus::Prepared,
            launch_committed: Some(false),
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
        self.with_scope_lock(scope_id, |directory, path, deadline| {
            ensure_process_scope_publication_budget_until(
                directory,
                path,
                encoded.len() as u64,
                ProcessAtomicKind::Scope,
                PROCESS_SCOPE_DIRECTORY_LIMITS,
                Some(deadline),
            )?;
            directory.save_bytes_atomically_expected_with_guard_and_hook(
                path,
                &encoded,
                SCOPE_WRITE_PREFIX,
                true,
                crate::daemons::state::FileExpectation::Missing,
                || ensure_process_scope_deadline(Some(deadline)),
                || Ok(()),
            )?;
            Ok(record.clone())
        })
    }

    /// Opens only the already-published scope namespace. Hidden supervisors use
    /// this before any parent request, record, worktree, or worker access so a
    /// missing or replaced scope can never be recreated by a late process.
    pub(crate) fn open_existing_for_supervisor(project_root: &Path) -> Result<Self, String> {
        let deadline = Instant::now() + SCOPE_LOCK_TIMEOUT;
        ensure_process_scope_deadline(Some(deadline))?;
        if !project_root.is_absolute() {
            return Err("managed-process supervisor project root is not absolute".to_string());
        }
        let project = crate::daemons::state::StableDirectory::open(project_root)?;
        ensure_process_scope_deadline(Some(deadline))?;
        let nib_path = project_root.join(".nib");
        let nib = project.open_owned_child(&nib_path)?;
        ensure_process_scope_deadline(Some(deadline))?;
        let directory = nib_path.join(SCOPE_DIRECTORY);
        let directory_capability = nib.open_owned_child(&directory)?;
        project.verify_visible()?;
        nib.verify_visible()?;
        directory_capability.verify_visible()?;
        ensure_process_scope_deadline(Some(deadline))?;
        Ok(Self {
            project_root: project_root.to_path_buf(),
            directory,
            directory_capability,
            records_binding: None,
            lock_deadline: Some(deadline),
            operation_timeout: SCOPE_LOCK_TIMEOUT,
        })
    }

    /// Converts a successfully started supervisor store into the ordinary
    /// long-lived lock policy without reopening any ambient path. The exact
    /// directory and records capabilities are cloned from their retained file
    /// handles; later operations therefore acquire their own bounded lock
    /// deadline while remaining bound to the original namespace identity.
    pub(crate) fn rebind_long_lived_after_handoff(&self) -> Result<Self, String> {
        self.verify_visible()?;
        let rebound = Self {
            project_root: self.project_root.clone(),
            directory: self.directory.clone(),
            directory_capability: self.directory_capability.try_clone()?,
            records_binding: self
                .records_binding
                .as_ref()
                .map(crate::daemons::state::StableDirectory::try_clone)
                .transpose()?,
            lock_deadline: None,
            operation_timeout: self.operation_timeout,
        };
        rebound.directory_capability.verify_visible()?;
        if let Some(records) = &rebound.records_binding {
            records.verify_visible()?;
        }
        self.verify_visible()?;
        Ok(rebound)
    }

    pub(crate) fn verify_visible(&self) -> Result<(), String> {
        self.directory_capability.verify_visible()?;
        if let Some(records) = &self.records_binding {
            records.verify_visible()?;
        }
        ensure_process_scope_deadline(self.lock_deadline)
    }

    pub fn load(&self, scope_id: &str) -> Result<ProcessScopeRecord, String> {
        let deadline = self.effective_operation_deadline()?;
        self.load_until(scope_id, deadline)
    }

    pub(crate) fn load_until(
        &self,
        scope_id: &str,
        deadline: Instant,
    ) -> Result<ProcessScopeRecord, String> {
        validate_scope_id(scope_id)?;
        self.with_scope_lock_until(scope_id, deadline, |directory, path, _deadline| {
            let record = read_scope_record(directory, path)?;
            if record.scope_id != scope_id {
                return Err(
                    "managed-process scope filename key does not match its record".to_string(),
                );
            }
            Ok(record)
        })
    }

    pub fn try_load(&self, scope_id: &str) -> Result<Option<ProcessScopeRecord>, String> {
        validate_scope_id(scope_id)?;
        self.with_scope_lock(scope_id, |directory, path, deadline| {
            let quarantine =
                directory.deterministic_artifact_path(path, SCOPE_DELETE_PREFIX, ".quarantine")?;
            let canonical_exists = directory.path_exists(path)?;
            let quarantine_exists = directory.path_exists(&quarantine)?;
            if canonical_exists && quarantine_exists {
                return Err(
                    "managed-process scope has ambiguous canonical and deletion-quarantine state"
                        .to_string(),
                );
            }
            let lease_path = self.cleanup_lease_path(scope_id)?;
            let lease_quarantine = directory.deterministic_artifact_path(
                &lease_path,
                CLEANUP_LEASE_DELETE_PREFIX,
                ".quarantine",
            )?;
            if !canonical_exists && !quarantine_exists {
                if directory.path_exists(&lease_path)?
                    || directory.path_exists(&lease_quarantine)?
                {
                    return Err(
                        "managed-process cleanup authority exists without its exact scope"
                            .to_string(),
                    );
                }
                ensure_process_scope_deadline(Some(deadline))?;
                return Ok(None);
            }
            let source = if canonical_exists { path } else { &quarantine };
            let opened = directory.open_read(source)?;
            let record: ProcessScopeRecord = read_bounded_json(&opened, source)?;
            validate_record(&record)?;
            if record.scope_id != scope_id {
                return Err(
                    "managed-process scope filename key does not match its record".to_string(),
                );
            }
            let _live_cleanup = recover_cleanup_lease_deletion_until(
                directory,
                &lease_path,
                &record,
                Some(deadline),
            )?;
            ensure_process_scope_deadline(Some(deadline))?;
            Ok(Some(record))
        })
    }

    pub fn cleanup_lease_state(
        &self,
        record: &ProcessScopeRecord,
    ) -> Result<CleanupLeaseState, String> {
        let deadline = self.effective_operation_deadline()?;
        self.cleanup_lease_state_until(record, deadline)
    }

    pub(crate) fn cleanup_lease_state_until(
        &self,
        record: &ProcessScopeRecord,
        deadline: Instant,
    ) -> Result<CleanupLeaseState, String> {
        validate_record(record)?;
        let path = self.cleanup_lease_path(&record.scope_id)?;
        self.with_scope_lock_until(&record.scope_id, deadline, |directory, _, deadline| {
            if recover_cleanup_lease_deletion_until(directory, &path, record, Some(deadline))? {
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
        self.with_scope_lock(&expected.scope_id, |directory, path, deadline| {
            if recover_scope_deletion_for_expected_until(directory, path, expected, Some(deadline))?
            {
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
            directory.remove_file_if_matches_with_guard(path, &opened, SCOPE_DELETE_PREFIX, || {
                ensure_process_scope_deadline(Some(deadline))
            })
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

    pub(crate) fn retire_completed_scope(
        &self,
        scope_id: &str,
        workload_execution_generation: u64,
        workload_authority: CompletionAuthority<'_>,
    ) -> Result<bool, String> {
        self.retire_completed_scope_with_guard(
            scope_id,
            workload_execution_generation,
            workload_authority,
            || Ok(()),
        )
    }

    pub(crate) fn retire_completed_scope_with_guard(
        &self,
        scope_id: &str,
        workload_execution_generation: u64,
        workload_authority: CompletionAuthority<'_>,
        mut before_namespace_step: impl FnMut() -> Result<(), String>,
    ) -> Result<bool, String> {
        validate_scope_id(scope_id)?;
        self.with_scope_lock(scope_id, |directory, path, deadline| {
            let quarantine =
                directory.deterministic_artifact_path(path, SCOPE_DELETE_PREFIX, ".quarantine")?;
            let source = if directory.path_exists(path)? {
                path
            } else if directory.path_exists(&quarantine)? {
                &quarantine
            } else {
                let cleanup_lease = self.cleanup_lease_path(scope_id)?;
                let cleanup_quarantine = directory.deterministic_artifact_path(
                    &cleanup_lease,
                    CLEANUP_LEASE_DELETE_PREFIX,
                    ".quarantine",
                )?;
                if directory.path_exists(&cleanup_lease)?
                    || directory.path_exists(&cleanup_quarantine)?
                {
                    return Err(
                        "managed-process cleanup authority exists without its exact scope"
                            .to_string(),
                    );
                }
                ensure_process_scope_deadline(Some(deadline))?;
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
            if recover_cleanup_lease_deletion_until(
                directory,
                &cleanup_lease,
                &record,
                Some(deadline),
            )? {
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
                before_namespace_step()?;
                ensure_process_scope_deadline(Some(deadline))?;
                directory.remove_visible_file_if_matches_direct_with_guard(
                    &quarantine,
                    &opened,
                    || {
                        before_namespace_step()?;
                        ensure_process_scope_deadline(Some(deadline))
                    },
                )?;
            } else {
                directory.remove_file_if_matches_with_guard(
                    path,
                    &opened,
                    SCOPE_DELETE_PREFIX,
                    || {
                        before_namespace_step()?;
                        ensure_process_scope_deadline(Some(deadline))
                    },
                )?;
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

    /// The hidden supervisor's first durable action. The immutable nonce makes
    /// this an exact CAS for the preplanned launch, while the scope lock orders
    /// it against restart removal of an unregistered Prepared scope.
    pub(crate) fn self_register_launch_supervisor(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        supervisor_registration_nonce: &str,
        supervisor: ProcessIdentity,
    ) -> Result<ProcessScopeRecord, String> {
        validate_canonical_process_uuid(
            supervisor_registration_nonce,
            "supervisor registration nonce",
        )?;
        self.with_scope_lock(scope_id, |directory, path, deadline| {
            let opened = directory.open_read(path)?;
            let mut record: ProcessScopeRecord = read_bounded_json(&opened, path)?;
            validate_record(&record)?;
            if record.execution_generation != execution_generation
                || record.cleanup_lease_id != cleanup_lease_id
            {
                return Err("stale managed-process scope generation was rejected".to_string());
            }
            if record.workload_kind != "subagent"
                || record.status != ProcessScopeStatus::Prepared
                || record.launch_committed != Some(false)
                || record.direct_child.is_some()
                || record.supervisor_registration_nonce.as_deref()
                    != Some(supervisor_registration_nonce)
            {
                return Err(
                    "managed-process supervisor registration authority does not match the exact prepared launch"
                        .to_string(),
                );
            }
            let previous = record.clone();
            match &record.supervisor {
                Some(observed) if observed == &supervisor => return Ok(record),
                Some(_) => {
                    return Err(
                        "managed-process scope already has another launch supervisor".to_string(),
                    );
                }
                None => record.supervisor = Some(supervisor),
            }
            record.updated_at = Utc::now();
            validate_record(&record)?;
            validate_process_scope_transition(&previous, &record)?;
            let encoded = encode_process_state_bounded(&record, "scope record")?;
            ensure_process_scope_publication_budget_until(
                directory,
                path,
                encoded.len() as u64,
                ProcessAtomicKind::Scope,
                PROCESS_SCOPE_DIRECTORY_LIMITS,
                Some(deadline),
            )?;
            directory.save_bytes_atomically_expected_with_guard_and_hook(
                path,
                &encoded,
                SCOPE_WRITE_PREFIX,
                true,
                crate::daemons::state::FileExpectation::Present(&opened),
                || ensure_process_scope_deadline(Some(deadline)),
                || Ok(()),
            )?;
            Ok(record)
        })
    }

    /// Parent-side observation only. `None` means the exact preplanned scope is
    /// still unregistered; any differing authority is an error rather than a
    /// second publisher.
    pub(crate) fn observe_registered_launch_supervisor(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        supervisor_registration_nonce: &str,
        supervisor: &ProcessIdentity,
    ) -> Result<Option<ProcessScopeRecord>, String> {
        validate_scope_id(scope_id)?;
        validate_canonical_process_uuid(
            supervisor_registration_nonce,
            "supervisor registration nonce",
        )?;
        self.with_scope_lock(scope_id, |directory, path, _deadline| {
            let record = read_scope_record(directory, path)?;
            if record.scope_id != scope_id
                || record.execution_generation != execution_generation
                || record.cleanup_lease_id != cleanup_lease_id
                || record.supervisor_registration_nonce.as_deref()
                    != Some(supervisor_registration_nonce)
                || record.workload_kind != "subagent"
                || record.status != ProcessScopeStatus::Prepared
                || record.launch_committed != Some(false)
                || record.direct_child.is_some()
            {
                return Err(
                    "managed-process parent observation does not match the exact prepared launch"
                        .to_string(),
                );
            }
            match record.supervisor.as_ref() {
                None => Ok(None),
                Some(observed) if observed == supervisor => Ok(Some(record)),
                Some(_) => Err(
                    "managed-process self-registered supervisor identity differs from the spawned child"
                        .to_string(),
                ),
            }
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
            record.launch_committed = Some(true);
            Ok(())
        })
    }

    pub(crate) fn mark_gated_running(
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

    pub(crate) fn commit_running_launch(
        &self,
        expected: &ProcessScopeRecord,
    ) -> Result<ProcessScopeRecord, String> {
        validate_record(expected)?;
        self.mutate(
            &expected.scope_id,
            expected.execution_generation,
            &expected.cleanup_lease_id,
            |record| {
                if record != expected
                    || record.status != ProcessScopeStatus::Running
                    || record.launch_committed != Some(false)
                {
                    return Err(
                        "managed process launch commit lost its exact gated scope authority"
                            .to_string(),
                    );
                }
                record.launch_committed = Some(true);
                Ok(())
            },
        )
    }

    pub(crate) fn register_gated_child(
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
            begin_cleanup_mutation(record, reason)
        })
    }

    pub(crate) fn complete_cleanup(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        outcome: impl Into<String>,
        descendants_reaped: bool,
    ) -> Result<ProcessScopeRecord, String> {
        let outcome = outcome.into();
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            complete_cleanup_mutation(
                record,
                execution_generation,
                cleanup_lease_id,
                outcome,
                descendants_reaped,
            )
        })
    }

    pub(crate) fn complete_launch_abort(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
    ) -> Result<ProcessScopeRecord, String> {
        self.mutate(scope_id, execution_generation, cleanup_lease_id, |record| {
            complete_launch_abort_mutation(record, execution_generation, cleanup_lease_id)
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
    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub fn recover_linux_supervisor_loss(
        &self,
        expected: &ProcessScopeRecord,
    ) -> Result<ProcessScopeRecord, String> {
        let operation_deadline = self.effective_operation_deadline()?;
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
        let cleanup_lease = match self.cleanup_lease_state_until(expected, operation_deadline)? {
            CleanupLeaseState::Recoverable => {
                self.acquire_cleanup_lease_until(expected, operation_deadline)?
            }
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
                if self.load_until(&expected.scope_id, operation_deadline)? != *expected {
                    return Err(
                        "managed process scope changed before prepared launch recovery".to_string(),
                    );
                }
                self.acquire_cleanup_lease_until(expected, operation_deadline)?
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
        let deadline = (Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT).min(operation_deadline);
        let mut namespace_init_signalled = false;
        loop {
            let supervisor_live = linux_identity_still_matches(supervisor)?;
            let direct_child_live = direct_child
                .map(linux_identity_still_matches)
                .transpose()?
                .unwrap_or(false);
            if !supervisor_live && !direct_child_live {
                ensure_process_scope_deadline(Some(operation_deadline))?;
                break;
            }
            if Instant::now() >= deadline {
                let reason = format!(
                    "Linux supervisor-loss recovery remained unproven (supervisor_live={supervisor_live}, direct_child_live={direct_child_live})"
                );
                if expected.status != ProcessScopeStatus::Prepared {
                    let _ = self.mutate_with_commit_check_until(
                        &expected.scope_id,
                        expected.execution_generation,
                        &expected.cleanup_lease_id,
                        operation_deadline,
                        |record| {
                            if record.status != ProcessScopeStatus::Prepared {
                                record.status = ProcessScopeStatus::RecoveryRequired;
                            }
                            record.cleanup_reason = Some(reason.clone());
                            Ok(())
                        },
                        || Ok(()),
                    );
                }
                return Err(reason);
            }
            if !supervisor_live && direct_child_live && !namespace_init_signalled {
                if let Err(error) = signal_linux_process_identity(
                    direct_child.expect("live direct child has an identity"),
                ) {
                    let reason =
                        format!("failed to terminate recovered Linux namespace init: {error}");
                    if expected.status != ProcessScopeStatus::Prepared {
                        let _ = self.mutate_with_commit_check_until(
                            &expected.scope_id,
                            expected.execution_generation,
                            &expected.cleanup_lease_id,
                            operation_deadline,
                            |record| {
                                if record.status != ProcessScopeStatus::Prepared {
                                    record.status = ProcessScopeStatus::RecoveryRequired;
                                }
                                record.cleanup_reason = Some(reason.clone());
                                Ok(())
                            },
                            || Ok(()),
                        );
                    }
                    return Err(reason);
                }
                namespace_init_signalled = true;
                sleep_until_supervisor_poll(deadline);
                continue;
            }
            sleep_until_supervisor_poll(deadline);
        }

        let launch_aborted = expected.status == ProcessScopeStatus::Prepared
            || expected.launch_committed == Some(false);
        let completed = if launch_aborted {
            self.mutate_with_commit_check_until(
                &expected.scope_id,
                expected.execution_generation,
                &expected.cleanup_lease_id,
                operation_deadline,
                |record| {
                    complete_launch_abort_mutation(
                        record,
                        expected.execution_generation,
                        &expected.cleanup_lease_id,
                    )
                },
                || Ok(()),
            )?
        } else {
            let cleaning = self.mutate_with_commit_check_until(
                &expected.scope_id,
                expected.execution_generation,
                &expected.cleanup_lease_id,
                operation_deadline,
                |record| {
                    begin_cleanup_mutation(
                        record,
                        "supervisor_lost_linux_pid_namespace".to_string(),
                    )
                },
                || Ok(()),
            )?;
            self.mutate_with_commit_check_until(
                &cleaning.scope_id,
                cleaning.execution_generation,
                &cleaning.cleanup_lease_id,
                operation_deadline,
                |record| {
                    complete_cleanup_mutation(
                        record,
                        cleaning.execution_generation,
                        &cleaning.cleanup_lease_id,
                        "supervisor_lost_linux_pid_namespace".to_string(),
                        true,
                    )
                },
                || Ok(()),
            )?
        };
        let proof = completed.cleanup_proof.as_ref();
        if let Some(proof) = proof {
            cleanup_lease.release_after_proof_until(proof, operation_deadline, || Ok(()))?;
        } else {
            let proof = completed
                .launch_abort_proof
                .as_ref()
                .ok_or("recovered managed-process scope has no completion proof")?;
            cleanup_lease.release_after_launch_abort_until(proof, operation_deadline, || Ok(()))?;
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
        let deadline = self.effective_operation_deadline()?;
        self.acquire_cleanup_lease_until(record, deadline)
    }

    pub(crate) fn acquire_cleanup_lease_until(
        &self,
        record: &ProcessScopeRecord,
        deadline: Instant,
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
        self.with_scope_lock_until(
            &record.scope_id,
            deadline,
            |directory, record_path, deadline| {
                let authoritative = read_scope_record(directory, record_path)?;
                if authoritative != *record {
                    return Err(
                        "managed-process scope changed before cleanup-lease acquisition"
                            .to_string(),
                    );
                }
                if recover_cleanup_lease_deletion_until(
                    directory,
                    &path,
                    &authoritative,
                    Some(deadline),
                )? {
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
                            "managed-process cleanup lease belongs to another generation"
                                .to_string(),
                        );
                    }
                    acquire_file_lock(&file, &path)?;
                    return Ok(CleanupLease {
                        directory: self.directory_capability.try_clone()?,
                        path: path.clone(),
                        file: Some(file),
                        record: expected.clone(),
                        lock_deadline: self.lock_deadline,
                        operation_timeout: self.operation_timeout,
                    });
                }
                ensure_process_scope_publication_budget_until(
                    directory,
                    &path,
                    encoded.len() as u64,
                    ProcessAtomicKind::CleanupLease,
                    PROCESS_SCOPE_DIRECTORY_LIMITS,
                    Some(deadline),
                )?;
                let receipt = directory
                    .save_bytes_atomically_expected_with_receipt_and_guard(
                        &path,
                        &encoded,
                        CLEANUP_LEASE_WRITE_PREFIX,
                        crate::daemons::state::FileExpectation::Missing,
                        || ensure_process_scope_deadline(Some(deadline)),
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
                    directory: self.directory_capability.try_clone()?,
                    path: path.clone(),
                    file: Some(receipt.file),
                    record: expected.clone(),
                    lock_deadline: self.lock_deadline,
                    operation_timeout: self.operation_timeout,
                })
            },
        )
    }

    pub(crate) fn mutate(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        mutation: impl FnOnce(&mut ProcessScopeRecord) -> Result<(), String>,
    ) -> Result<ProcessScopeRecord, String> {
        let deadline = self.effective_operation_deadline()?;
        self.mutate_with_commit_check_until(
            scope_id,
            execution_generation,
            cleanup_lease_id,
            deadline,
            mutation,
            || Ok(()),
        )
    }

    #[cfg(test)]
    pub(crate) fn mutate_with_commit_check(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        mutation: impl FnOnce(&mut ProcessScopeRecord) -> Result<(), String>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<ProcessScopeRecord, String> {
        let deadline = self.effective_operation_deadline()?;
        self.mutate_with_commit_check_until(
            scope_id,
            execution_generation,
            cleanup_lease_id,
            deadline,
            mutation,
            before_commit,
        )
    }

    pub(crate) fn mutate_with_commit_check_until(
        &self,
        scope_id: &str,
        execution_generation: u64,
        cleanup_lease_id: &str,
        deadline: Instant,
        mutation: impl FnOnce(&mut ProcessScopeRecord) -> Result<(), String>,
        before_commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<ProcessScopeRecord, String> {
        validate_scope_id(scope_id)?;
        self.with_scope_lock_until(scope_id, deadline, |directory, path, deadline| {
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
            ensure_process_scope_publication_budget_until(
                directory,
                path,
                encoded.len() as u64,
                ProcessAtomicKind::Scope,
                PROCESS_SCOPE_DIRECTORY_LIMITS,
                Some(deadline),
            )?;
            before_commit()?;
            ensure_process_scope_deadline(Some(deadline))?;
            directory.save_bytes_atomically_expected_with_guard_and_hook(
                path,
                &encoded,
                SCOPE_WRITE_PREFIX,
                true,
                crate::daemons::state::FileExpectation::Present(&opened),
                || ensure_process_scope_deadline(Some(deadline)),
                || Ok(()),
            )?;
            Ok(record)
        })
    }

    pub(crate) fn with_scope_lock<T>(
        &self,
        scope_id: &str,
        operation: impl FnOnce(
            &crate::daemons::state::StableDirectory,
            &Path,
            Instant,
        ) -> Result<T, String>,
    ) -> Result<T, String> {
        let deadline = self.effective_operation_deadline()?;
        self.with_scope_lock_until(scope_id, deadline, operation)
    }

    pub(crate) fn with_scope_lock_until<T>(
        &self,
        scope_id: &str,
        deadline: Instant,
        operation: impl FnOnce(
            &crate::daemons::state::StableDirectory,
            &Path,
            Instant,
        ) -> Result<T, String>,
    ) -> Result<T, String> {
        self.with_scope_lock_until_and_recovery_hook(scope_id, deadline, operation, || Ok(()))
    }

    pub(crate) fn with_scope_lock_until_and_recovery_hook<T>(
        &self,
        scope_id: &str,
        deadline: Instant,
        operation: impl FnOnce(
            &crate::daemons::state::StableDirectory,
            &Path,
            Instant,
        ) -> Result<T, String>,
        mut before_recovery_step: impl FnMut() -> Result<(), String>,
    ) -> Result<T, String> {
        validate_scope_id(scope_id)?;
        let path = self.record_path(scope_id)?;
        self.with_store_lock_until(deadline, |directory| {
            before_recovery_step()?;
            ensure_process_scope_deadline(Some(deadline))?;
            recover_process_atomic_transaction_until(
                directory,
                &path,
                SCOPE_WRITE_PREFIX,
                ProcessAtomicKind::Scope,
                Some(deadline),
            )?;
            let cleanup_lease = self.cleanup_lease_path(scope_id)?;
            before_recovery_step()?;
            ensure_process_scope_deadline(Some(deadline))?;
            recover_process_atomic_transaction_until(
                directory,
                &cleanup_lease,
                CLEANUP_LEASE_WRITE_PREFIX,
                ProcessAtomicKind::CleanupLease,
                Some(deadline),
            )?;
            ensure_process_scope_deadline(Some(deadline))?;
            let result = operation(directory, &path, deadline)?;
            ensure_process_scope_deadline(Some(deadline))?;
            Ok(result)
        })
    }

    pub(crate) fn with_store_lock_until<T>(
        &self,
        deadline: Instant,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, String>,
    ) -> Result<T, String> {
        self.with_store_lock_until_and_setup_hook(deadline, operation, || Ok(()))
    }

    pub(crate) fn with_store_lock_until_and_setup_hook<T>(
        &self,
        deadline: Instant,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, String>,
        mut before_setup_step: impl FnMut() -> Result<(), String>,
    ) -> Result<T, String> {
        let lock = self.project_root.join(".nib").join(SCOPE_STORE_LOCK);
        ensure_process_scope_deadline(Some(deadline))?;
        crate::daemons::state::with_file_lock_in_until_with_setup_hook(
            &lock,
            &self.directory,
            deadline,
            |directory| {
                if !directory.same_identity(&self.directory_capability) {
                    return Err(
                        "managed-process scope capability changed before locked operation"
                            .to_string(),
                    );
                }
                self.verify_retained_capabilities(deadline)?;
                let result = operation(directory);
                self.verify_retained_capabilities(deadline)?;
                result
            },
            &mut before_setup_step,
        )
    }

    pub(crate) fn effective_operation_deadline(&self) -> Result<Instant, String> {
        let deadline = self
            .lock_deadline
            .unwrap_or_else(|| Instant::now() + self.operation_timeout);
        ensure_process_scope_deadline(Some(deadline))?;
        Ok(deadline)
    }

    pub(crate) fn verify_retained_capabilities(&self, deadline: Instant) -> Result<(), String> {
        ensure_process_scope_deadline(Some(deadline))?;
        if let Some(records) = &self.records_binding {
            records.verify_visible()?;
        }
        self.directory_capability.verify_visible()?;
        ensure_process_scope_deadline(Some(deadline))
    }

    #[cfg(test)]
    pub(crate) fn maintain_once(&self) -> Result<(), String> {
        self.maintain_once_in(&MAINTAINED_PROCESS_SCOPE_STORES)
    }

    pub(crate) fn maintain_once_with_setup_hook(
        &self,
        before_setup_step: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        self.maintain_once_in_with_setup_hook(&MAINTAINED_PROCESS_SCOPE_STORES, before_setup_step)
    }

    #[cfg(test)]
    pub(crate) fn maintain_once_in(
        &self,
        registry: &ProcessScopeMaintenanceRegistry,
    ) -> Result<(), String> {
        self.maintain_once_in_with_setup_hook(registry, || Ok(()))
    }

    pub(crate) fn maintain_once_in_with_setup_hook(
        &self,
        registry: &ProcessScopeMaintenanceRegistry,
        before_setup_step: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let deadline = self.effective_operation_deadline()?;
        let maintained = lock_process_scope_maintenance_registry(registry, Some(deadline))?;
        if maintained.contains(&self.directory) {
            ensure_process_scope_deadline(Some(deadline))?;
            return Ok(());
        }
        drop(maintained);
        self.with_store_lock_until_and_setup_hook(
            deadline,
            |directory| maintain_process_scope_directory_until(directory, false, Some(deadline)),
            before_setup_step,
        )?;
        let mut maintained = lock_process_scope_maintenance_registry(registry, Some(deadline))?;
        ensure_process_scope_deadline(Some(deadline))?;
        if maintained.len() >= 1_024 {
            maintained.clear();
        }
        maintained.insert(self.directory.clone());
        Ok(())
    }

    pub(crate) fn record_path(&self, scope_id: &str) -> Result<PathBuf, String> {
        validate_scope_id(scope_id)?;
        Ok(self.directory.join(format!("{scope_id}.json")))
    }

    pub(crate) fn cleanup_lease_path(&self, scope_id: &str) -> Result<PathBuf, String> {
        validate_scope_id(scope_id)?;
        Ok(self
            .directory
            .join(format!("{scope_id}{CLEANUP_LEASE_SUFFIX}")))
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }
}

pub(crate) fn ensure_process_scope_deadline(deadline: Option<Instant>) -> Result<(), String> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err("managed-process scope lock deadline elapsed".to_string());
    }
    Ok(())
}

pub(crate) fn lock_process_scope_maintenance_registry<'a>(
    registry: &'a ProcessScopeMaintenanceRegistry,
    deadline: Option<Instant>,
) -> Result<std::sync::MutexGuard<'a, std::collections::HashSet<PathBuf>>, String> {
    let Some(deadline) = deadline else {
        return registry
            .lock()
            .map_err(|_| "managed-process maintenance registry is poisoned".to_string());
    };
    loop {
        ensure_process_scope_deadline(Some(deadline))?;
        match registry.try_lock() {
            Ok(maintained) => {
                ensure_process_scope_deadline(Some(deadline))?;
                return Ok(maintained);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("managed-process maintenance registry is poisoned".to_string());
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err("managed-process scope lock deadline elapsed".to_string());
        };
        if remaining.is_zero() {
            return Err("managed-process scope lock deadline elapsed".to_string());
        }
        thread::sleep(MAINTENANCE_REGISTRY_POLL_INTERVAL.min(remaining));
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn sleep_until_supervisor_poll(deadline: Instant) {
    if let Some(remaining) = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
    {
        thread::sleep(SUPERVISOR_POLL_INTERVAL.min(remaining));
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
