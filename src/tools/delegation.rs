use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
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
const MERGE_PENDING_STATUS: &str = "merge_pending";
const MERGE_FAILED_STATUS: &str = "merge_failed";
const NIB_EXCLUDE_PATHSPEC: &str = ":(exclude).nib";
const NIB_DESCENDANTS_EXCLUDE_PATHSPEC: &str = ":(exclude).nib/**";
const REPOSITORY_MERGE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const SUBAGENT_RECORD_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const SUBAGENT_CANCELLATION_RECONCILIATION_ATTEMPTS: usize = 500;
const MAX_SUBAGENT_DIRECTORY_ENTRIES: usize = MAX_SUBAGENT_RECORDS + 1_024;
const MAX_SUBAGENT_DIRECTORY_NAME_BYTES: usize = 4 * 1024 * 1024;
const OWNER_LEASE_DIRECTORY: &str = "subagent-owner-leases";
const OWNER_LEASE_SUFFIX: &str = ".lease";
const OWNER_LEASE_ANCHOR_PREFIX: &str = ".subagent-owner-";
const OWNER_LEASE_ANCHOR_SUFFIX: &str = ".anchor";
const OWNER_LOST_ERROR: &str =
    "subagent execution was interrupted because its owner process is no longer live";
const MAX_SUBAGENT_WORKER_REQUEST_BYTES: usize = 16 * 1024 * 1024;
#[cfg(not(test))]
const SUBAGENT_SUPERVISOR_READY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(debug_assertions, not(test)))]
const SUBAGENT_LAUNCH_FAILPOINT_ENV: &str = "NIB_TEST_SUBAGENT_LAUNCH_FAILPOINT";

#[derive(Debug, Serialize, Deserialize)]
struct SubagentWorkerRequest {
    prompt: String,
    max_steps: u32,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl std::fmt::Debug for InitialSubagentRecordPublicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InitialSubagentRecordPublicationError")
            .field("message", &self.message)
            .field("has_receipt", &self.receipt.is_some())
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

enum OwnerLeaseProbe {
    Live,
    Acquired(SubagentOwnerLease),
}

impl SubagentOwnerLease {
    fn create(project_root: &Path) -> Result<Self, String> {
        let visible = owner_lease_directory(project_root);
        crate::fs_security::ensure_directory_without_symlinks(&visible)
            .map_err(|error| format!("subagent owner lease directory is unsafe: {error}"))?;
        with_bounded_delegation_lock_in(
            &owner_lease_namespace_lock_path(project_root),
            &project_root.join(".nib"),
            OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT,
            |_| Self::create_locked(project_root),
        )
    }

    fn create_locked(project_root: &Path) -> Result<Self, String> {
        let (anchor_directory, visible_directory) =
            open_owner_lease_directories(project_root, true)?;
        visible_directory.for_each_entry_bounded(
            MAX_SUBAGENT_DIRECTORY_ENTRIES,
            MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
            |_| Ok(()),
        )?;
        let execution_generation = new_execution_generation();
        let lease_id = uuid::Uuid::new_v4().to_string();
        let visible_path = owner_lease_path(project_root, &lease_id)?;
        let anchor_path = owner_lease_anchor_path(project_root, &lease_id)?;
        if visible_directory.path_exists(&visible_path)?
            || anchor_directory.path_exists(&anchor_path)?
        {
            return Err("new subagent owner lease identifier unexpectedly exists".to_string());
        }
        let visible_file = visible_directory.open_read_write_create(&visible_path)?;
        if let Err(error) =
            visible_directory.hard_link_to(&visible_path, &anchor_directory, &anchor_path)
        {
            let cleanup = visible_directory
                .remove_file_if_matches(
                    &visible_path,
                    &visible_file,
                    ".nib-subagent-owner-create-visible-delete-",
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
                    .remove_file_if_matches(
                        &visible_path,
                        &visible_file,
                        ".nib-subagent-owner-create-visible-delete-",
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
        let nib = self
            .anchor_path
            .parent()
            .ok_or_else(|| "subagent owner lease anchor has no parent directory".to_string())?
            .to_path_buf();
        let project_root = nib
            .parent()
            .ok_or_else(|| "subagent owner lease has no project root".to_string())?
            .to_path_buf();
        with_bounded_delegation_lock_in(
            &owner_lease_namespace_lock_path(&project_root),
            &nib,
            OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT,
            |_| self.remove_locked(),
        )
    }

    fn release_for_reconciliation(self) -> Result<(), String> {
        let verification = self.verify_pair();
        drop(self);
        verification
    }

    fn remove_locked(mut self) -> Result<(), String> {
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
                    visible_directory.remove_file_if_matches(
                        &self.visible_path,
                        file,
                        ".nib-subagent-owner-visible-delete-",
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
        self.anchor_directory.remove_file_if_matches(
            &self.anchor_path,
            file,
            ".nib-subagent-owner-anchor-delete-",
        )?;
        drop(self.file.take());
        Ok(())
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
) -> Result<(), String> {
    let visible = visible_directory.remove_file_if_matches(
        visible_path,
        visible_file,
        ".nib-subagent-owner-create-visible-delete-",
    );
    let anchor = anchor_directory.remove_file_if_matches(
        anchor_path,
        anchor_file,
        ".nib-subagent-owner-create-anchor-delete-",
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
    if crate::fs_security::metadata_is_link_or_reparse(&metadata)
        || !metadata.is_dir()
        || !canonical.starts_with(project_root)
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

struct NonInteractiveSubagentApproval;

#[derive(Debug)]
struct RepositoryMergeLock {
    _anchor_file: File,
}

impl RepositoryMergeLock {
    async fn acquire(project_root: &Path) -> Result<Self, String> {
        Self::acquire_with_timeout(project_root, REPOSITORY_MERGE_LOCK_TIMEOUT).await
    }

    async fn acquire_with_timeout(project_root: &Path, timeout: Duration) -> Result<Self, String> {
        let directory = ensure_records_directory(project_root)?;
        let path = directory.join(".merge.lock");
        let anchor_path = repository_merge_lock_anchor_path(project_root);
        let anchor_directory = anchor_path.parent().ok_or_else(|| {
            format!(
                "repository merge lock anchor has no parent: {}",
                anchor_path.display()
            )
        })?;
        crate::fs_security::verify_directory_without_symlinks(anchor_directory).map_err(
            |error| format!("repository merge lock anchor directory is unsafe: {error}"),
        )?;
        let anchor_file = open_repository_merge_lock_anchor(&path, &anchor_path)?;
        let locked_identity = repository_lock_identity(&anchor_file, &anchor_path)?;
        let deadline = Instant::now() + timeout;
        loop {
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
            tokio::time::sleep(Duration::from_millis(25).min(deadline - now)).await;
        }
        crate::fs_security::verify_directory_without_symlinks(&directory)
            .map_err(|error| format!("repository merge lock directory changed: {error}"))?;
        crate::fs_security::verify_directory_without_symlinks(anchor_directory)
            .map_err(|error| format!("repository merge lock anchor directory changed: {error}"))?;
        for lock_path in [&anchor_path, &path] {
            let path_identity = open_repository_lock_identity(lock_path)?;
            if locked_identity != path_identity {
                return Err(format!(
                    "repository merge lock identity changed while it was acquired: {}",
                    lock_path.display()
                ));
            }
        }
        Ok(Self {
            _anchor_file: anchor_file,
        })
    }
}

fn with_bounded_delegation_lock_in<T>(
    lock_path: &Path,
    protected_directory: &Path,
    timeout: Duration,
    operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, String>,
) -> Result<T, String> {
    let lock_parent = lock_path
        .parent()
        .ok_or_else(|| format!("delegation lock has no parent: {}", lock_path.display()))?;
    let lock_parent = crate::fs_security::ensure_directory_without_symlinks(lock_parent)
        .map_err(|error| format!("delegation lock directory is unsafe: {error}"))?;
    let file_name = lock_path
        .file_name()
        .ok_or_else(|| format!("delegation lock has no file name: {}", lock_path.display()))?;
    let lock_path = lock_parent.join(file_name);
    let anchor_path = crate::daemons::state::daemon_lock_anchor_path(&lock_path)?;
    let anchor_parent = anchor_path.parent().ok_or_else(|| {
        format!(
            "delegation lock anchor has no parent: {}",
            anchor_path.display()
        )
    })?;
    crate::fs_security::ensure_directory_without_symlinks(anchor_parent)
        .map_err(|error| format!("delegation lock anchor directory is unsafe: {error}"))?;
    crate::fs_security::verify_directory_without_symlinks(protected_directory)
        .map_err(|error| format!("delegation protected directory is unsafe: {error}"))?;

    let protected_directory = crate::daemons::state::StableDirectory::open(protected_directory)?;
    let anchor_file = open_repository_merge_lock_anchor(&lock_path, &anchor_path)?;
    let locked_identity = repository_lock_identity(&anchor_file, &anchor_path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match anchor_file.try_lock() {
            Ok(()) => break,
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
            return Err(format!(
                "timed out acquiring delegation state lock {} after {} seconds",
                lock_path.display(),
                timeout.as_secs_f64()
            ));
        }
        std::thread::sleep(Duration::from_millis(25).min(deadline - now));
    }

    let verify_lock_domain = || -> Result<(), String> {
        crate::fs_security::verify_directory_without_symlinks(&lock_parent)
            .map_err(|error| format!("delegation lock directory changed: {error}"))?;
        crate::fs_security::verify_directory_without_symlinks(anchor_parent)
            .map_err(|error| format!("delegation lock anchor directory changed: {error}"))?;
        for path in [&anchor_path, &lock_path] {
            if open_repository_lock_identity(path)? != locked_identity {
                return Err(format!(
                    "delegation state lock identity changed while acquired: {}",
                    path.display()
                ));
            }
        }
        protected_directory.verify_visible()
    };
    verify_lock_domain()?;
    let result = operation(&protected_directory);
    let attachment = verify_lock_domain();
    match (result, attachment) {
        (_, Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
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

#[cfg(any(unix, windows))]
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

#[cfg(not(any(unix, windows)))]
fn open_repository_merge_lock_anchor(
    lock_path: &Path,
    _anchor_path: &Path,
) -> Result<File, String> {
    Err(format!(
        "repository merge lock anchors are unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(any(unix, windows))]
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

#[cfg(any(unix, windows))]
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

#[cfg(any(unix, windows))]
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

#[cfg(not(any(unix, windows)))]
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

#[cfg(any(unix, windows))]
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

#[cfg(not(any(unix, windows)))]
fn open_repository_lock_identity(path: &Path) -> Result<(), String> {
    Err(format!(
        "repository merge lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

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

#[cfg(any(unix, windows))]
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
    let parent_session_id = args
        .get("_parent_session_id")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    let worktree = crate::sandbox::worktree::Worktree::create(&project_root, &subagent_id)?;
    if let Err(error) = prepare_child_runtime_config(&project_root, &worktree.path) {
        let cleanup = cleanup_precommit_worktree_sync(&project_root, &worktree)
            .err()
            .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
            .unwrap_or_default();
        return Err(format!(
            "failed to prepare subagent runtime: {error}{cleanup}"
        ));
    }
    let owner_lease = SubagentOwnerLease::create(&project_root).map_err(|error| {
        let cleanup = cleanup_precommit_worktree_sync(&project_root, &worktree)
            .err()
            .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
            .unwrap_or_default();
        format!("failed to establish subagent execution ownership: {error}{cleanup}")
    })?;
    let record = SubagentRecord {
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
        result: None,
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let publication =
        match write_subagent_record_with_refresh_hook(&project_root, &record, || Ok(())) {
            Ok(publication) => publication,
            Err(error) => {
                let message = error.message.clone();
                let compensation_errors = collect_spawn_compensation_sync(
                    || cleanup_record_after_publication_failure(&project_root, &record, &error),
                    || cleanup_precommit_worktree_sync(&project_root, &worktree),
                    |action| compensate_owner_lease(owner_lease, action),
                );
                return if compensation_errors.is_empty() {
                    Err(message)
                } else {
                    Err(format!("{message}; {}", compensation_errors.join("; ")))
                };
            }
        };
    if let Err(error) =
        crate::daemons::task::TASK_MANAGER.register_task(subagent_id.clone(), "subagent")
    {
        let compensation_errors = collect_spawn_compensation_sync(
            || cleanup_record_after_registration_failure(&project_root, &record, &publication),
            || cleanup_precommit_worktree_sync(&project_root, &worktree),
            |action| compensate_owner_lease(owner_lease, action),
        );
        return if compensation_errors.is_empty() {
            Err(error)
        } else {
            Err(format!("{error}; {}", compensation_errors.join("; ")))
        };
    }
    drop(publication);

    launch_subagent_task(
        project_root,
        subagent_id,
        prompt,
        max_steps,
        parent_session_id,
        PreparedSubagentTask {
            record,
            worktree,
            owner_lease,
        },
    )
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
        let parent_session_id = args
            .get("_parent_session_id")
            .and_then(|value| value.as_str())
            .map(str::to_string);

        let worktree = crate::sandbox::worktree::Worktree::create_cancellable(
            &project_root,
            &subagent_id,
            cancellation,
        )
        .await?;
        if let Err(error) = prepare_child_runtime_config(&project_root, &worktree.path) {
            let cleanup = cleanup_precommit_worktree(&project_root, &worktree)
                .await
                .err()
                .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
                .unwrap_or_default();
            return Err(format!(
                "failed to prepare subagent runtime: {error}{cleanup}"
            ));
        }
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            cleanup_precommit_worktree(&project_root, &worktree).await?;
            return Err("subagent spawn cancelled before commit".to_string());
        }
        let owner_lease = match SubagentOwnerLease::create(&project_root) {
            Ok(owner_lease) => owner_lease,
            Err(error) => {
                let cleanup = cleanup_precommit_worktree(&project_root, &worktree)
                    .await
                    .err()
                    .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
                    .unwrap_or_default();
                return Err(format!(
                    "failed to establish subagent execution ownership: {error}{cleanup}"
                ));
            }
        };
        let record = SubagentRecord {
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
            result: None,
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let publication =
            match write_subagent_record_with_refresh_hook(&project_root, &record, || Ok(())) {
                Ok(publication) => publication,
                Err(error) => {
                    let message = error.message.clone();
                    let compensation_errors = collect_spawn_compensation_async(
                        || cleanup_record_after_publication_failure(&project_root, &record, &error),
                        cleanup_precommit_worktree(&project_root, &worktree),
                        |action| compensate_owner_lease(owner_lease, action),
                    )
                    .await;
                    return if compensation_errors.is_empty() {
                        Err(message)
                    } else {
                        Err(format!("{message}; {}", compensation_errors.join("; ")))
                    };
                }
            };
        if let Err(error) =
            crate::daemons::task::TASK_MANAGER.register_task(subagent_id.clone(), "subagent")
        {
            let compensation_errors = collect_spawn_compensation_async(
                || cleanup_record_after_registration_failure(&project_root, &record, &publication),
                cleanup_precommit_worktree(&project_root, &worktree),
                |action| compensate_owner_lease(owner_lease, action),
            )
            .await;
            return if compensation_errors.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", compensation_errors.join("; ")))
            };
        }
        drop(publication);

        launch_subagent_task(
            project_root,
            subagent_id,
            prompt,
            max_steps,
            parent_session_id,
            PreparedSubagentTask {
                record,
                worktree,
                owner_lease,
            },
        )
    })
}

#[cfg(test)]
fn launch_subagent_task(
    project_root: PathBuf,
    subagent_id: String,
    prompt: String,
    max_steps: u32,
    parent_session_id: Option<String>,
    prepared: PreparedSubagentTask,
) -> Result<Value, String> {
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
    let guard = SubagentRunGuard::new(record_root.clone(), record_id.clone(), owner_lease);
    let session_lock_policy = crate::session::SessionStore::current_lock_policy();
    let handle = tokio::spawn(crate::session::SessionStore::with_optional_lock_policy(
        session_lock_policy,
        async move {
            let mut guard = guard;
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

    Ok(json!({
        "status": "started",
        "subagent_id": subagent_id,
        "parent_session_id": parent_session_id,
        "child_session_id": record.child_session_id,
        "worktree_path": record.worktree_path,
        "branch": record.branch,
        "branch_oid": record.branch_oid,
    }))
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
    let completion_verified = initial_record
        .as_ref()
        .is_some_and(|record| record.status != "running")
        && crate::sandbox::process::ProcessScopeStore::open(&project_root)
            .and_then(|store| store.load(&subagent_id))
            .ok()
            .is_some_and(|scope| {
                scope.status == crate::sandbox::process::ProcessScopeStatus::Complete
                    && (scope.cleanup_proof.is_some() || scope.launch_abort_proof.is_some())
            });
    let (lease_cleanup, record) = if completion_verified {
        (owner_lease.remove(), initial_record)
    } else {
        let cleanup = owner_lease.release_for_reconciliation();
        let reconciled =
            reconcile_subagent_ownership_with_owner_state(&project_root, &subagent_id, true)
                .ok()
                .or(initial_record);
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
            let _ = retire_terminal_process_scope(&project_root, &record);
        }
        match record.status.as_str() {
            "completed" => {
                crate::daemons::task::TASK_MANAGER.complete(&subagent_id, record.result.clone())
            }
            "failed" => crate::daemons::task::TASK_MANAGER.fail(
                &subagent_id,
                record
                    .error
                    .clone()
                    .unwrap_or_else(|| "subagent supervisor failed".to_string()),
                record.result.clone(),
            ),
            _ => {}
        }
    }
    let _ = wait_tx.send(status);
}

fn retire_terminal_process_scope(
    project_root: &Path,
    expected: &SubagentRecord,
) -> Result<bool, String> {
    if !status_retains_process_scope_retirement_authority(&expected.status) {
        return Ok(false);
    }
    let project_root = canonical_project_root(project_root)?;
    let path = record_path(&project_root, &expected.id)?;
    let records_directory = ensure_records_directory(&project_root)?;
    with_bounded_delegation_lock_in(
        &record_lock_path(&project_root, &expected.id)?,
        &records_directory,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        |directory| {
            let opened = read_opened_subagent_record_in(directory, &path)?;
            validate_reopened_subagent_record(expected, &opened.record)?;
            let Some((execution_generation, authority)) =
                terminal_process_scope_authority(&opened.record)?
            else {
                return Ok(false);
            };
            directory.verify_file_identity(&path, &opened.file)?;
            let store = crate::sandbox::process::ProcessScopeStore::open(&project_root)?;
            let retired = match &authority {
                TerminalProcessScopeAuthority::Cleanup(proof) => {
                    store.retire_complete(&opened.record.id, execution_generation, proof)?
                }
                TerminalProcessScopeAuthority::LaunchAbort(proof) => {
                    store.retire_launch_abort(&opened.record.id, execution_generation, proof)?
                }
            };
            directory.verify_file_identity(&path, &opened.file)?;
            Ok(retired)
        },
    )
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
fn launch_subagent_task(
    project_root: PathBuf,
    subagent_id: String,
    prompt: String,
    max_steps: u32,
    parent_session_id: Option<String>,
    prepared: PreparedSubagentTask,
) -> Result<Value, String> {
    let PreparedSubagentTask {
        record,
        worktree,
        owner_lease,
    } = prepared;
    let execution_generation = owner_lease.execution_generation;
    let lease_id = owner_lease.lease_id.clone();
    let scope_store = match crate::sandbox::process::ProcessScopeStore::open(&project_root) {
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
            );
        }
    };
    let mut process_scope = match scope_store.prepare(
        &subagent_id,
        "subagent",
        execution_generation,
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
            );
        }
    };
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
            );
        }
    };
    let request = SubagentWorkerRequest { prompt, max_steps };
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
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("failed to start subagent supervisor: {error}");
            let _ = scope_store.remove_prepared(&process_scope);
            let _ = persist_interrupted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                &message,
            );
            let _ = owner_lease.remove();
            return Err(message);
        }
    };
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
                },
                owner_lease,
                child,
                format!("failed to identify the subagent supervisor: {error}"),
            );
        }
    };
    process_scope = match scope_store.register_launch_supervisor(
        &subagent_id,
        execution_generation,
        &process_scope.cleanup_lease_id,
        supervisor_identity,
    ) {
        Ok(scope) => scope,
        Err(error) => {
            return fail_pre_delivery_subagent(
                PreDeliverySubagent {
                    project_root: &project_root,
                    id: &subagent_id,
                    execution_generation,
                    lease_id: &lease_id,
                    scope_store: &scope_store,
                    process_scope: &process_scope,
                },
                owner_lease,
                child,
                format!("failed to register the subagent supervisor: {error}"),
            );
        }
    };
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
            },
            owner_lease,
            child,
            "injected subagent readiness-monitor failure".to_string(),
        );
    }
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    if let Err(error) = std::thread::Builder::new()
        .name(format!("nib-subagent-ready-{subagent_id}"))
        .spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(supervisor_stdout)
                .read_line(&mut line)
                .map(|_| line);
            let _ = ready_tx.send(result);
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
            },
            owner_lease,
            child,
            "subagent supervisor monitor stopped before ownership handoff".to_string(),
        );
    }

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

    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("readiness-timeout") {
        signal_supervisor_cancellation(&control);
        return Err("injected subagent supervisor readiness timeout".to_string());
    }

    match ready_rx.recv_timeout(SUBAGENT_SUPERVISOR_READY_TIMEOUT) {
        Ok(Ok(line)) if line.trim_end() == "READY" => {}
        Ok(Ok(line)) => {
            signal_supervisor_cancellation(&control);
            return Err(format!(
                "subagent supervisor returned an invalid readiness frame: {:?}",
                line.trim_end()
            ));
        }
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
    }

    let supervisor_guard = SupervisorControlGuard::new(Arc::clone(&control));
    let handle = tokio::spawn(async move {
        let mut guard = supervisor_guard;
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

    Ok(json!({
        "status": "started",
        "subagent_id": subagent_id,
        "parent_session_id": parent_session_id,
        "child_session_id": record.child_session_id,
        "worktree_path": record.worktree_path,
        "branch": record.branch,
        "branch_oid": record.branch_oid,
        "process_scope": {
            "backend": process_scope.backend,
            "execution_generation": process_scope.execution_generation,
            "cleanup_lease_id": process_scope.cleanup_lease_id,
        },
    }))
}

#[cfg(not(test))]
struct PreDeliverySubagent<'a> {
    project_root: &'a Path,
    id: &'a str,
    execution_generation: u64,
    lease_id: &'a str,
    scope_store: &'a crate::sandbox::process::ProcessScopeStore,
    process_scope: &'a crate::sandbox::process::ProcessScopeRecord,
}

#[cfg(not(test))]
fn fail_pre_delivery_subagent<T>(
    context: PreDeliverySubagent<'_>,
    owner_lease: SubagentOwnerLease,
    mut supervisor: std::process::Child,
    error: String,
) -> Result<T, String> {
    match terminate_unstarted_supervisor(&mut supervisor) {
        Ok(()) => fail_unstarted_subagent(
            context.project_root,
            context.id,
            context.execution_generation,
            context.lease_id,
            owner_lease,
            Some((context.scope_store, context.process_scope)),
            error,
        ),
        Err(cleanup_error) => {
            let _ = context.scope_store.mark_recovery_required(
                context.id,
                context.execution_generation,
                &context.process_scope.cleanup_lease_id,
                format!("unstarted supervisor cleanup was not proven: {cleanup_error}"),
            );
            let lease_cleanup = owner_lease.release_for_reconciliation().err();
            let _ = reconcile_subagent_ownership_with_owner_state(
                context.project_root,
                context.id,
                false,
            );
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
fn terminate_unstarted_supervisor(supervisor: &mut std::process::Child) -> Result<(), String> {
    let kill_error = supervisor
        .kill()
        .err()
        .filter(|error| error.kind() != std::io::ErrorKind::InvalidInput);
    let deadline = Instant::now() + SUBAGENT_SUPERVISOR_READY_TIMEOUT;
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
fn fail_unstarted_subagent<T>(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    owner_lease: SubagentOwnerLease,
    process_scope: Option<(
        &crate::sandbox::process::ProcessScopeStore,
        &crate::sandbox::process::ProcessScopeRecord,
    )>,
    error: String,
) -> Result<T, String> {
    let mut details = Vec::new();
    if let Some((store, scope)) = process_scope {
        if let Err(cleanup) = store.remove_prepared(scope) {
            details.push(format!("process-scope cleanup failed: {cleanup}"));
        }
    }
    if !persist_interrupted_subagent(project_root, id, execution_generation, lease_id, &error) {
        details.push("failed to persist the unstarted subagent outcome".to_string());
    }
    if let Err(cleanup) = owner_lease.remove() {
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
    worktree: &Path,
) -> Result<(), String> {
    let project_root = canonical_project_root(project_root)?;
    if !is_valid_subagent_id(subagent_id) {
        return Err("invalid subagent id".to_string());
    }
    validate_execution_ownership(execution_generation, owner_lease_id)?;
    let scope_store = crate::sandbox::process::ProcessScopeStore::open(&project_root)?;
    let process_scope = scope_store.load(subagent_id)?;
    if process_scope.execution_generation != execution_generation
        || process_scope.workload_kind != "subagent"
        || process_scope.status != crate::sandbox::process::ProcessScopeStatus::Prepared
    {
        return Err("subagent supervisor process scope does not match its launch".to_string());
    }
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
    if request.prompt != record.prompt || request.max_steps > 100 {
        return Err("subagent supervisor request does not match its record".to_string());
    }
    let process_scope = scope_store.load(subagent_id)?;
    let supervisor_identity = crate::sandbox::process::ProcessIdentity::current()?;
    if process_scope.execution_generation != execution_generation
        || process_scope.workload_kind != "subagent"
        || process_scope.status != crate::sandbox::process::ProcessScopeStatus::Prepared
        || process_scope.supervisor.as_ref() != Some(&supervisor_identity)
        || process_scope.direct_child.is_some()
    {
        return Err(
            "subagent supervisor process scope changed before cleanup ownership".to_string(),
        );
    }
    let cleanup_lease = scope_store.acquire_cleanup_lease(&process_scope)?;
    let worker_input = serde_json::to_vec(&request)
        .map_err(|error| format!("failed to encode subagent worker request: {error}"))?;
    let executable = resolve_nib_executable()?;
    let mut environment: Vec<_> = std::env::vars_os().collect();
    environment.push(("NIB_MANAGED_PROCESS_SCOPE".into(), subagent_id.into()));
    let output = crate::sandbox::process::supervise_foreground_with_claimed_cleanup(
        &scope_store,
        &process_scope,
        cleanup_lease,
        owner_input,
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
        |_| {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(b"READY\n")
                .and_then(|()| stdout.flush())
                .map_err(|error| format!("failed to acknowledge subagent readiness: {error}"))
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to start subagent worker runtime: {error}"))?;
    let config = crate::agent::AgentLoopConfig {
        max_steps: request.max_steps,
        auto_approve: false,
        approval_handler: Some(Arc::new(NonInteractiveSubagentApproval)),
        ..Default::default()
    };
    let session_lock_policy = crate::session::SessionStore::current_lock_policy();
    let outcome = runtime.block_on(crate::session::SessionStore::with_optional_lock_policy(
        session_lock_policy,
        crate::agent::run_agent_loop(worktree, subagent_id, &request.prompt, config),
    ));
    serde_json::to_writer(
        std::io::stdout().lock(),
        &SubagentWorkerResponse { outcome },
    )
    .map_err(|error| format!("failed to write subagent worker response: {error}"))
}

fn read_supervisor_request<R: BufRead>(reader: &mut R) -> Result<SubagentWorkerRequest, String> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerLeaseCompensation {
    Remove,
    ReleaseForReconciliation,
}

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

fn cleanup_precommit_worktree_sync(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
) -> Result<(), String> {
    crate::sandbox::worktree::Worktree::remove(project_root, &worktree.id)
}

async fn cleanup_precommit_worktree(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
) -> Result<(), String> {
    let project_root = project_root.to_path_buf();
    let id = worktree.id.clone();
    tokio::task::spawn_blocking(move || {
        crate::sandbox::worktree::Worktree::remove_bounded_sync(
            &project_root,
            &id,
            SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT,
        )
    })
    .await
    .map_err(|error| format!("subagent worktree cleanup worker failed: {error}"))?
}

fn cleanup_precommit_record(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
) -> Result<(), String> {
    cleanup_precommit_record_with_hook(project_root, attempted_record, expected_publication, || {
        Ok(())
    })
}

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

fn cleanup_record_after_publication_failure(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    error: &InitialSubagentRecordPublicationError,
) -> Result<(), String> {
    let exact_publication = error
        .receipt
        .as_ref()
        .filter(|receipt| receipt.exact_identity)
        .map(|receipt| &receipt.file);
    cleanup_precommit_record(project_root, attempted_record, exact_publication)
}

fn cleanup_precommit_record_with_hook(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    before_quarantine: impl FnOnce() -> Result<(), String>,
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
    with_bounded_delegation_lock_in(
        &record_lock_path(&project_root, &attempted_record.id)?,
        &records,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        |directory| {
            if !directory.path_exists(&path)? {
                return Ok(());
            }
            let expected_publication = expected_publication.ok_or_else(|| {
                format!(
                    "exact precommit subagent record publication identity is unavailable; preserved {}",
                    path.display()
                )
            })?;
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
            directory.remove_file_if_matches_with_hooks(
                &path,
                expected_publication,
                ".nib-subagent-precommit-delete-",
                || {
                    before_quarantine()?;
                    verify_open_subagent_record_bytes(expected_publication, &attempted_bytes)
                },
                || verify_open_subagent_record_bytes(expected_publication, &attempted_bytes),
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
    let _merge_lock = RepositoryMergeLock::acquire(&project_root).await?;
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
    let record = get_subagent_record(project_root, subagent_id)?;
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
    let directory = records_dir(&project_root);
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            sweep_owner_lease_artifacts(&project_root, &std::collections::HashSet::new())?;
            return Ok(Vec::new());
        }
        Err(error) => return Err(error.to_string()),
    };
    validate_records_directory(&project_root, &directory, &metadata)?;
    let stable_directory = crate::daemons::state::StableDirectory::open(&directory)?;
    let mut record_ids = Vec::new();
    stable_directory.for_each_entry_bounded(
        MAX_SUBAGENT_DIRECTORY_ENTRIES,
        MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
        |name| {
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
        records.push(json!(record));
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
        OWNER_LEASE_NAMESPACE_LOCK_TIMEOUT,
        |anchor_directory| {
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
            anchor_directory.for_each_entry_bounded(
                MAX_SUBAGENT_DIRECTORY_ENTRIES,
                MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
                |name| {
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
            if let Some(visible_directory) = &visible_directory {
                visible_directory.for_each_entry_bounded(
                    MAX_SUBAGENT_DIRECTORY_ENTRIES,
                    MAX_SUBAGENT_DIRECTORY_NAME_BYTES,
                    |name| {
                        let Some(name) = name.to_str() else {
                            return Err(
                                "subagent owner lease directory contains a non-UTF-8 filename"
                                    .to_string(),
                            );
                        };
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
                let visible_exists = match &visible_directory {
                    Some(directory) => directory.path_exists(&visible)?,
                    None => false,
                };
                let anchor_exists = anchor_directory.path_exists(&anchor)?;
                match (visible_exists, anchor_exists) {
                    (true, true) => {
                        let visible_directory = visible_directory
                            .as_ref()
                            .expect("visible owner lease directory was opened");
                        let visible_file = visible_directory.open_read_write(&visible)?;
                        let anchor_file = anchor_directory.open_read_write(&anchor)?;
                        verify_owner_lease_pair(
                            visible_directory,
                            &visible,
                            &visible_file,
                            anchor_directory,
                            &anchor,
                            &anchor_file,
                        )?;
                        match anchor_file.try_lock() {
                            Ok(()) if record_is_running => {
                                return Err(format!(
                                    "running subagent owner lease is unlocked; artifacts were preserved: {lease_id}"
                                ));
                            }
                            Ok(()) => {
                                drop(visible_file);
                                visible_directory.remove_file_if_matches(
                                    &visible,
                                    &anchor_file,
                                    ".nib-subagent-owner-visible-delete-",
                                )?;
                                anchor_directory.remove_file_if_matches(
                                    &anchor,
                                    &anchor_file,
                                    ".nib-subagent-owner-anchor-delete-",
                                )?;
                            }
                            Err(std::fs::TryLockError::WouldBlock) => {
                                verify_owner_lease_pair_from_anchor(
                                    visible_directory,
                                    &visible,
                                    anchor_directory,
                                    &anchor,
                                    &anchor_file,
                                )?;
                            }
                            Err(std::fs::TryLockError::Error(error)) => {
                                return Err(format!(
                                    "failed to inspect subagent owner lease artifact: {error}"
                                ));
                            }
                        }
                    }
                    (false, true) => {
                        let anchor_file = anchor_directory.open_read_write(&anchor)?;
                        match anchor_file.try_lock() {
                            Ok(()) if record_is_running => {
                                return Err(format!(
                                    "running subagent owner anchor has no visible lease; artifact was preserved: {lease_id}"
                                ));
                            }
                            Ok(()) => {
                                verify_owner_lease_anchor(anchor_directory, &anchor, &anchor_file)?;
                                anchor_directory.remove_file_if_matches(
                                    &anchor,
                                    &anchor_file,
                                    ".nib-subagent-owner-anchor-delete-",
                                )?;
                            }
                            Err(std::fs::TryLockError::WouldBlock) if record_is_running => {
                                verify_owner_lease_anchor(anchor_directory, &anchor, &anchor_file)?;
                            }
                            Err(std::fs::TryLockError::WouldBlock) => {
                                verify_owner_lease_anchor(anchor_directory, &anchor, &anchor_file)?;
                                return Err(format!(
                                    "live subagent owner anchor has no visible lease; artifact was preserved: {lease_id}"
                                ));
                            }
                            Err(std::fs::TryLockError::Error(error)) => {
                                return Err(format!(
                                    "failed to inspect anchor-only subagent owner lease: {error}"
                                ));
                            }
                        }
                    }
                    (true, false) => {
                        let visible_directory = visible_directory
                            .as_ref()
                            .expect("visible owner lease directory was opened");
                        let visible_file = visible_directory.open_read_write(&visible)?;
                        match visible_file.try_lock() {
                            Ok(()) if record_is_running => {
                                return Err(format!(
                                    "running subagent visible owner lease has no anchor; artifact was preserved: {lease_id}"
                                ));
                            }
                            Ok(()) => {
                                visible_directory.verify_file_identity(&visible, &visible_file)?;
                                visible_directory.remove_file_if_matches(
                                    &visible,
                                    &visible_file,
                                    ".nib-subagent-owner-visible-delete-",
                                )?;
                            }
                            Err(std::fs::TryLockError::WouldBlock) => {
                                visible_directory.verify_file_identity(&visible, &visible_file)?;
                                return Err(format!(
                                    "live visible subagent owner lease has no anchor; artifact was preserved: {lease_id}"
                                ));
                            }
                            Err(std::fs::TryLockError::Error(error)) => {
                                return Err(format!(
                                    "failed to inspect visible-only subagent owner lease: {error}"
                                ));
                            }
                        }
                    }
                    (false, false) => {}
                }
            }
            Ok(())
        },
    )
}

pub fn get_subagent_record(project_root: &Path, id: &str) -> Result<SubagentRecord, String> {
    let project_root = canonical_project_root(project_root)?;
    let record = reconcile_subagent_ownership(&project_root, id)?;
    sync_subagent_task_manager(&record);
    Ok(record)
}

fn sync_subagent_task_manager(record: &SubagentRecord) {
    match record.status.as_str() {
        "completed" => {
            crate::daemons::task::TASK_MANAGER.complete(&record.id, record.result.clone())
        }
        "failed" => crate::daemons::task::TASK_MANAGER.fail(
            &record.id,
            record
                .error
                .clone()
                .unwrap_or_else(|| "subagent failed".to_string()),
            record.result.clone(),
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

fn reconcile_subagent_ownership(project_root: &Path, id: &str) -> Result<SubagentRecord, String> {
    reconcile_subagent_ownership_with_owner_state(project_root, id, false)
}

fn reconcile_subagent_ownership_with_owner_state(
    project_root: &Path,
    id: &str,
    owner_confirmed_stopped: bool,
) -> Result<SubagentRecord, String> {
    let path = record_path(project_root, id)?;
    let records_directory = ensure_records_directory(project_root)?;
    let (record, cleanup) = with_bounded_delegation_lock_in(
        &record_lock_path(project_root, id)?,
        &records_directory,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        |directory| {
            let mut opened = read_opened_subagent_record_in(directory, &path)?;
            let record = &mut opened.record;
            if record.status != "running" {
                return Ok((record.clone(), None));
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
            let scope_store = crate::sandbox::process::ProcessScopeStore::open(project_root)?;
            let Some(mut process_scope) = scope_store.try_load(id)? else {
                if !owner_confirmed_stopped {
                    match SubagentOwnerLease::probe(project_root, execution_generation, &lease_id)?
                    {
                        OwnerLeaseProbe::Live => return Ok((record.clone(), None)),
                        OwnerLeaseProbe::Acquired(lease) => {
                            lease.release_for_reconciliation()?;
                        }
                    }
                }
                if record.result.as_ref().and_then(|result| {
                    result
                        .get("process_scope")
                        .and_then(|scope| scope.get("status"))
                        .and_then(Value::as_str)
                }) != Some("missing")
                {
                    record.result = Some(json!({
                        "outcome": "recovery_required",
                        "process_scope": {
                            "status": "missing",
                            "execution_generation": execution_generation,
                        },
                        "cleanup_verified": false,
                    }));
                    record.updated_at = Utc::now();
                    write_subagent_record_unlocked(
                        project_root,
                        directory,
                        &path,
                        record,
                        crate::daemons::state::FileExpectation::Present(&opened.file),
                    )?;
                }
                return Ok((record.clone(), None));
            };
            if process_scope.execution_generation != execution_generation
                || process_scope.workload_kind != "subagent"
            {
                return Err("running subagent has a mismatched managed-process scope".to_string());
            }

            if process_scope.status != crate::sandbox::process::ProcessScopeStatus::Complete {
                if manager_status.as_deref() == Some("running") && !owner_confirmed_stopped {
                    return Ok((record.clone(), None));
                }
                let cleanup_state = scope_store.cleanup_lease_state(&process_scope)?;
                if cleanup_state == crate::sandbox::process::CleanupLeaseState::Live {
                    return Ok((record.clone(), None));
                }
                if !owner_confirmed_stopped {
                    if process_scope.status == crate::sandbox::process::ProcessScopeStatus::Prepared
                        && Utc::now()
                            .signed_duration_since(process_scope.updated_at)
                            .num_seconds()
                            < 5
                    {
                        return Ok((record.clone(), None));
                    }
                    match SubagentOwnerLease::probe(project_root, execution_generation, &lease_id)?
                    {
                        OwnerLeaseProbe::Live => return Ok((record.clone(), None)),
                        OwnerLeaseProbe::Acquired(lease) => {
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
                            process_scope = scope_store.mark_recovery_required(
                                id,
                                execution_generation,
                                &process_scope.cleanup_lease_id,
                                format!(
                                    "supervisor stopped before cleanup proof (previous status: {previous_status:?}): {recovery_error}"
                                ),
                            )?;
                        }
                        record.result = Some(json!({
                            "outcome": "recovery_required",
                            "process_scope": process_scope,
                            "cleanup_verified": false,
                            "error": recovery_error,
                        }));
                        record.updated_at = Utc::now();
                        write_subagent_record_unlocked(
                            project_root,
                            directory,
                            &path,
                            record,
                            crate::daemons::state::FileExpectation::Present(&opened.file),
                        )?;
                        return Ok((record.clone(), None));
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
                    return Ok((record.clone(), None));
                }
                crate::sandbox::process::CleanupLeaseState::Recoverable => {
                    let cleanup_lease = scope_store.acquire_cleanup_lease(&process_scope)?;
                    match &completion_authority {
                        TerminalProcessScopeAuthority::Cleanup(proof) => {
                            cleanup_lease.release_after_proof(proof)?;
                        }
                        TerminalProcessScopeAuthority::LaunchAbort(proof) => {
                            cleanup_lease.release_after_launch_abort(proof)?;
                        }
                    }
                }
                crate::sandbox::process::CleanupLeaseState::Missing => {}
            }
            let owner_lease =
                match SubagentOwnerLease::probe(project_root, execution_generation, &lease_id)? {
                    OwnerLeaseProbe::Live => return Ok((record.clone(), None)),
                    OwnerLeaseProbe::Acquired(owner_lease) => owner_lease,
                };

            let reconciled_at = Utc::now();
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
            let terminal_status = if cancelled { "cancelled" } else { "failed" };
            let error = if cancelled {
                "subagent cancellation was reconciled after its execution owner stopped"
            } else if matches!(
                completion_authority,
                TerminalProcessScopeAuthority::LaunchAbort(_)
            ) {
                "subagent supervisor stopped before the gated workload launched"
            } else {
                OWNER_LOST_ERROR
            };
            let mut evidence = json!({
                "outcome": outcome,
                "subagent_id": record.id,
                "execution_generation": execution_generation,
                "owner_lease": lease_id,
                "manager_status": manager_status,
                "terminal_status": terminal_status,
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
            record_subagent_ownership_reconciliation_event(project_root, record, &evidence)?;
            record.status = terminal_status.to_string();
            record.result = Some(json!({
                "outcome": "interrupted",
                "ownership_reconciliation": evidence,
            }));
            record.error = Some(error.to_string());
            record.updated_at = reconciled_at;
            write_subagent_record_unlocked(
                project_root,
                directory,
                &path,
                record,
                crate::daemons::state::FileExpectation::Present(&opened.file),
            )?;
            Ok((record.clone(), Some(owner_lease)))
        },
    )?;

    if let Some(owner_lease) = cleanup {
        if let Err(error) = owner_lease.remove() {
            persist_owner_lease_cleanup_error(
                project_root,
                id,
                record.execution_generation.unwrap_or_default(),
                record.owner_lease.as_deref().unwrap_or_default(),
                &error,
            );
            return get_subagent_record_unreconciled(project_root, id);
        }
    }
    let _ = retire_terminal_process_scope(project_root, &record);
    Ok(record)
}

fn record_subagent_ownership_reconciliation_event(
    project_root: &Path,
    record: &SubagentRecord,
    evidence: &Value,
) -> Result<(), String> {
    let session_id = record
        .parent_session_id
        .as_deref()
        .unwrap_or(&record.child_session_id);
    crate::session::SessionStore::for_project(project_root)?
        .record_event(
            session_id,
            "subagent_execution_reconciled",
            evidence.clone(),
        )
        .map(|_| ())
        .map_err(|error| format!("failed to audit subagent ownership reconciliation: {error}"))
}

pub(crate) async fn prepare_subagent_verification_target(
    project_root: &Path,
    id: &str,
) -> Result<VerificationTarget, String> {
    let project_root = canonical_project_root(project_root)?;
    let _merge_lock = RepositoryMergeLock::acquire(&project_root).await?;
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
    match resolve_subagent_cancellation(project_root, id) {
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

pub(crate) fn resolve_subagent_cancellation(
    project_root: &Path,
    id: &str,
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
    let initial = match reconcile_subagent_ownership(&project_root, id) {
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
        manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
        match reconcile_subagent_ownership(&project_root, id) {
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
            std::thread::sleep(Duration::from_millis(10));
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
    let project_root = canonical_project_root(project_root).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
        }
    })?;
    let path = record_path(&project_root, &record.id).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
        }
    })?;
    let records_directory = ensure_records_directory(&project_root).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
        }
    })?;
    let mut rescued_receipt = None;
    let lock_path = record_lock_path(&project_root, &record.id).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
        }
    })?;
    let result = with_bounded_delegation_lock_in(
        &lock_path,
        &records_directory,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        |directory| {
            let receipt = match write_subagent_record_unlocked_with_receipt(
                &project_root,
                directory,
                &path,
                record,
                crate::daemons::state::FileExpectation::Missing,
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
        }),
    }
}

fn write_subagent_record_unlocked(
    project_root: &Path,
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    record: &SubagentRecord,
    expected: crate::daemons::state::FileExpectation<'_>,
) -> Result<crate::daemons::state::FilePublicationReceipt, String> {
    let publication = write_subagent_record_unlocked_with_receipt(
        project_root,
        directory,
        path,
        record,
        expected,
    );
    let error = match publication {
        Ok(receipt) => return Ok(receipt),
        Err(error) => error,
    };
    let Some(receipt) = error.receipt.filter(|receipt| receipt.exact_identity) else {
        return Err(error.message);
    };
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
    if let Err(recovery) = directory.finalize_failed_exact_publication(
        path,
        previous_expected,
        &receipt,
        ".nib-subagent-",
        &expected_bytes,
    ) {
        return Err(format!(
            "{}; exact publication recovery failed and all ambiguous state was preserved: {recovery}",
            error.message
        ));
    }
    let reopened = read_opened_subagent_record_in(directory, path).map_err(|recovery| {
        format!(
            "{}; finalized publication readback failed: {recovery}",
            error.message
        )
    })?;
    validate_reopened_subagent_record(record, &reopened.record).map_err(|recovery| {
        format!(
            "{}; finalized publication record validation failed: {recovery}",
            error.message
        )
    })?;
    if !crate::daemons::state::same_open_file_identity(&receipt.file, &reopened.file)? {
        return Err(format!(
            "{}; finalized publication was replaced before its authority could be adopted",
            error.message
        ));
    }
    directory.verify_file_identity(path, &receipt.file)?;
    Ok(receipt)
}

fn write_subagent_record_unlocked_with_receipt(
    project_root: &Path,
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    record: &SubagentRecord,
    expected: crate::daemons::state::FileExpectation<'_>,
) -> Result<
    crate::daemons::state::FilePublicationReceipt,
    crate::daemons::state::FilePublicationError,
> {
    let parent = path
        .parent()
        .ok_or_else(|| "subagent record has no parent".to_string())?;
    crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent records: {error}"))?;
    if crate::fs_security::metadata_is_link_or_reparse(&metadata)
        || !metadata.is_dir()
        || !canonical_parent.starts_with(project_root)
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
    let publication = directory.save_bytes_atomically_expected_with_receipt(
        path,
        &contents,
        ".nib-subagent-",
        expected,
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
    let path = record_path(project_root, &record.id)?;
    let records_directory = ensure_records_directory(project_root)?;
    let expected_bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| format!("failed to encode revised subagent record: {error}"))?;
    let mut rescued_receipt = None;
    let result = with_bounded_delegation_lock_in(
        &record_lock_path(project_root, &record.id)?,
        &records_directory,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        |directory| {
            let receipt = match write_subagent_record_unlocked_with_receipt(
                project_root,
                directory,
                &path,
                record,
                crate::daemons::state::FileExpectation::Present(expected_file),
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
                    if let Err(recovery) = directory.finalize_failed_exact_publication(
                        &path,
                        Some(expected_file),
                        &receipt,
                        ".nib-subagent-",
                        &expected_bytes,
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

fn prepare_child_runtime_config(project_root: &Path, worktree_path: &Path) -> Result<(), String> {
    let mut config =
        crate::config::load_nib_config_full(project_root).map_err(|error| error.to_string())?;

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
    crate::config::save_nib_config_full(worktree_path, &mut config)
        .map_err(|error| error.to_string())
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
                    "bound_reached": summary.bound_reached,
                    "trace": summary.trace,
                });
                if let (Some(result), Some(cleanup)) =
                    (result.as_object_mut(), cleanup_evidence.clone())
                {
                    result.insert("cleanup_verified".to_string(), Value::Bool(true));
                    result.insert("cleanup_proof".to_string(), cleanup);
                }
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
                let result = cleanup_evidence.clone().map(|cleanup| {
                    json!({
                        "outcome": "worker_failed",
                        "cleanup_verified": true,
                        "cleanup_proof": cleanup,
                    })
                });
                record.result = result.clone();
                TaskUpdate::Fail(error, result)
            }
        };
        record.updated_at = Utc::now();
        Ok(Some(update))
    })?;

    match task_update {
        Some(TaskUpdate::Complete(result)) => {
            crate::daemons::task::TASK_MANAGER.complete(id, Some(result));
        }
        Some(TaskUpdate::Fail(error, result)) => {
            crate::daemons::task::TASK_MANAGER.fail(id, error, result);
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
        record.result = Some(json!({
            "outcome": if cancelled { "cancelled" } else { "owner_process_lost" },
            "cleanup_verified": true,
            "cleanup_scope": "foreground_descendant_process_tree",
            "cleanup_proof": cleanup,
        }));
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

fn persist_interrupted_subagent(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    reason: &str,
) -> bool {
    let updated = update_subagent_record(project_root, id, |record| {
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

fn persist_owner_lease_cleanup_error(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    cleanup_error: &str,
) {
    let _ = update_subagent_record(project_root, id, |record| {
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
    let directory = records_dir(project_root);
    crate::fs_security::ensure_directory_without_symlinks(&directory)
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
    validate_records_directory(project_root, &directory, &metadata)?;
    migrate_legacy_record_locks(project_root, &directory)?;
    Ok(directory)
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
    if crate::fs_security::metadata_is_link_or_reparse(metadata)
        || !metadata.is_dir()
        || !canonical.starts_with(project_root)
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
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || !canonical.starts_with(project_root)
    {
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

fn subagent_record_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash % SUBAGENT_RECORD_LOCK_STRIPES as u64) as usize
}

fn migrate_legacy_record_locks(project_root: &Path, records: &Path) -> Result<(), String> {
    let legacy_locks = records.join(".locks");
    match std::fs::symlink_metadata(&legacy_locks) {
        Ok(metadata)
            if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() =>
        {
            return Err(format!(
                "legacy subagent lock path is unsafe: {}",
                legacy_locks.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    }
    let records_directory = crate::daemons::state::StableDirectory::open(records)?;
    let legacy_directory = records_directory.open_child(&legacy_locks)?;
    legacy_directory.for_each_entry_bounded(
        MAX_LEGACY_RECORD_LOCK_ENTRIES,
        MAX_LEGACY_RECORD_LOCK_NAME_BYTES,
        |name| {
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".lock"))
                .filter(|id| is_valid_subagent_id(id))
            else {
                return Ok(());
            };
            let legacy_path = legacy_locks.join(&name);
            let anchor_path = crate::daemons::state::daemon_lock_anchor_path(&legacy_path)?;
            with_bounded_delegation_lock_in(
                &record_lock_path(project_root, id)?,
                records,
                SUBAGENT_RECORD_LOCK_TIMEOUT,
                |protected_records| {
                    let protected_legacy = protected_records.open_child(&legacy_locks)?;
                    crate::daemons::state::cleanup_legacy_lock_pair(
                        &protected_legacy,
                        &legacy_path,
                        protected_records,
                        &anchor_path,
                    )
                },
            )
        },
    )
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
    let project_root = canonical_project_root(project_root)?;
    let path = record_path(&project_root, id)?;
    let records_directory = ensure_records_directory(&project_root)?;
    with_bounded_delegation_lock_in(
        &record_lock_path(&project_root, id)?,
        &records_directory,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        |directory| {
            let mut opened = read_opened_subagent_record_in(directory, &path)?;
            let result = update(&mut opened.record)?;
            write_subagent_record_unlocked(
                &project_root,
                directory,
                &path,
                &opened.record,
                crate::daemons::state::FileExpectation::Present(&opened.file),
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

    const MERGE_LOCK_CHILD_PROJECT_ROOT: &str = "NIB_TEST_MERGE_LOCK_PROJECT_ROOT";
    const MERGE_LOCK_CHILD_EXPECTATION: &str = "NIB_TEST_MERGE_LOCK_EXPECTATION";
    const RECORD_WRITE_CHILD_PROJECT_ROOT: &str = "NIB_TEST_SUBAGENT_WRITE_PROJECT_ROOT";
    const OWNER_LOSS_CHILD_PROJECT_ROOT: &str = "NIB_TEST_OWNER_LOSS_PROJECT_ROOT";
    const OWNER_LOSS_CHILD_READY: &str = "NIB_TEST_OWNER_LOSS_READY";
    const OWNER_LOSS_SUBAGENT_ID: &str = "sub-owner-process-loss";

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
            outcome: "fixture".to_string(),
            descendants_reaped: true,
            completed_at: Utc::now(),
        };
        scope.status = crate::sandbox::process::ProcessScopeStatus::Complete;
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

        let live = get_subagent_record(root.path(), OWNER_LOSS_SUBAGENT_ID)
            .expect("cross-process live owner remains running");
        assert_eq!(live.status, "running");
        assert!(
            live.result.is_none(),
            "live owner probing must not publish missing-scope recovery evidence"
        );
        let lease_id = live.owner_lease.clone().expect("owner lease id");
        child.kill().expect("kill owner process");
        child.wait().expect("reap owner process");

        let reconciled = get_subagent_record(root.path(), OWNER_LOSS_SUBAGENT_ID)
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

        let persisted = get_subagent_record(root.path(), &record.id)
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
        let reconciled = get_subagent_record(root.path(), &record.id).expect("reconciled record");
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
                let persisted = get_subagent_record(root.path(), &orphan.id)
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

    #[cfg(any(unix, windows))]
    #[test]
    fn global_subagent_legacy_lock_migration_is_bounded_and_fails_closed_for_live_owner() {
        let root = tempfile::tempdir().expect("root");
        let records = records_dir(root.path());
        let legacy = records.join(".locks");
        std::fs::create_dir_all(&legacy).expect("legacy lock directory");
        let mut legacy_pairs = Vec::new();
        for index in 0..96 {
            let visible = legacy.join(format!("untouched-{index}.lock"));
            let anchor = crate::daemons::state::daemon_lock_anchor_path(&visible)
                .expect("legacy anchor path");
            std::fs::write(&visible, b"legacy").expect("legacy lock");
            std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
            legacy_pairs.push((visible, anchor));
        }

        ensure_records_directory(root.path()).expect("global legacy migration");
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
            .expect_err("global migration must reject a live legacy owner");
        assert!(error.contains("still owned"), "{error}");
        assert!(live_visible.exists());
        assert!(live_anchor.exists());
        drop(owner);
        ensure_records_directory(root.path()).expect("released legacy lock migrates");
        assert!(!live_visible.exists());
        assert!(!live_anchor.exists());
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
            |_| -> Result<(), String> { panic!("held record stripe must not run its operation") },
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
            |_| -> Result<(), String> { panic!("held owner namespace must not run its operation") },
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
                Duration::from_millis(100),
            ))
            .expect_err("replacement must not create a second repository lock domain");
        match expectation.as_str() {
            "timeout" => assert!(error.contains("timed out acquiring repository merge lock")),
            "identity" => assert!(
                error.contains("persistent anchor have different identities"),
                "{error}"
            ),
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
        std::fs::rename(&records_path, &displaced_records)
            .expect("displace subagent records directory");
        std::fs::create_dir(&records_path).expect("replace subagent records directory");
        run_child("timeout");
        std::fs::remove_dir_all(&records_path).expect("remove replacement records directory");
        std::fs::rename(&displaced_records, &records_path)
            .expect("restore subagent records directory");

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
        let target = prepare_subagent_verification_target(root.path(), &record.id)
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
}
