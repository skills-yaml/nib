//! Git worktrees dedicated to linked subagent jobs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CANCELLED_CREATE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const SYNC_CREATE_CLEANUP_TIMEOUT: Duration = GIT_COMMAND_TIMEOUT;
const MAX_GIT_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_PACKED_REFS_BYTES: u64 = 64 * 1024 * 1024;
const MAX_WORKTREE_REGISTRATIONS: usize = 4096;
const MAX_WORKTREE_REGISTRATION_NAME_BYTES: usize = 1024 * 1024;
const MANAGED_WORKTREE_OWNERSHIP_SCHEMA_VERSION: u32 = 2;
const MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MANAGED_WORKTREE_OWNERSHIP_RECORDS: usize = 64;
const MAX_MANAGED_WORKTREE_OWNERSHIP_AGGREGATE_BYTES: u64 =
    MAX_MANAGED_WORKTREE_OWNERSHIP_RECORDS as u64 * MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES;
const MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_ENTRIES: usize = 8192;
const MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_NAME_BYTES: usize = 8 * 1024 * 1024;
const MANAGED_WORKTREE_OWNERSHIP_DIRECTORY: &str = "worktree-ownership";
const MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX: &str = ".nib-worktree-ownership-";
const MANAGED_REF_LOCK_TEMPORARY_PREFIX: &str = ".nib-ref-lock-";
const MANAGED_REF_LOCK_DELETE_PREFIX: &str = ".nib-ref-lock-delete-";
const RESERVED_REF_TEMPORARY_PREFIX: &str = ".nib-reserved-ref-";
const RESERVED_REF_DELETE_PREFIX: &str = ".nib-reserved-ref-delete-";
const MAX_MANAGED_REF_LOCK_DIRECTORY_ENTRIES: usize = 4096;
const MAX_MANAGED_REF_LOCK_DIRECTORY_NAME_BYTES: usize = 1024 * 1024;
const WORKTREE_CREATE_CANCELLED: &str = "subagent worktree creation cancelled";
const EXECUTABLE_GIT_CONFIG_PATTERN: &str = concat!(
    "^(filter\\..*\\.(clean|smudge|process)",
    "|diff\\.external",
    "|diff\\..*\\.(command|textconv)",
    "|merge\\..*\\.driver",
    "|credential(\\..+)?\\.helper",
    "|core\\.(sshcommand|gitproxy)",
    "|include\\.path",
    "|includeif\\..*\\.path)$"
);

#[cfg(test)]
static SYNC_POST_ADD_VALIDATION_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static SYNC_POST_ADD_BRANCH_MOVES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static SYNC_POST_ADD_BRANCH_SYMREFS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static SYNC_BEFORE_ADD_DESTINATION_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static SYNC_POST_CAPTURE_PATH_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static SYNC_POST_CAPTURE_REGISTRATION_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static SYNC_AFTER_DESTINATION_PUBLICATION_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static SYNC_AFTER_REGISTRATION_SNAPSHOT_FORGERIES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static OWNED_REF_LOCK_RELEASE_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static BEFORE_REF_PUBLICATION_SYMREFS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

type WorktreeOwnershipKey = (PathBuf, String);
type WorktreeOwnershipMap =
    std::collections::HashMap<WorktreeOwnershipKey, Arc<ManagedWorktreeReceipt>>;

static WORKTREE_OWNERSHIP: std::sync::LazyLock<std::sync::Mutex<WorktreeOwnershipMap>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedWorktreeKind {
    Subagent,
    Session,
}

impl ManagedWorktreeKind {
    fn label(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DurableArtifactPhase {
    Reserved,
    Present,
    Removing,
    Removed,
    Unattributed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DurableOwnershipPhase {
    Intent,
    Owned,
    Cleanup,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurablePreviousBranchAnchor {
    path: PathBuf,
    identity: crate::fs_security::FileIdentitySnapshot,
    oid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DurableManagedWorktreeOwnership {
    schema_version: u32,
    receipt_id: String,
    kind: ManagedWorktreeKind,
    logical_id: String,
    phase: DurableOwnershipPhase,
    project_root: PathBuf,
    common_git_dir: PathBuf,
    common_git_identity: crate::fs_security::DirectoryIdentity,
    worktree_path: PathBuf,
    worktree_staging_path: PathBuf,
    worktree_identity: Option<crate::fs_security::DirectoryIdentity>,
    registration_path: Option<PathBuf>,
    registration_identity: Option<crate::fs_security::DirectoryIdentity>,
    registration_namespace_identity: Option<crate::fs_security::DirectoryIdentity>,
    registration_snapshot_captured: bool,
    preexisting_registration_name_hashes: Vec<String>,
    preexisting_registration_identities: Vec<crate::fs_security::DirectoryIdentity>,
    branch_reference: String,
    branch_staging_path: PathBuf,
    branch_anchor_generation: u64,
    previous_branch_anchor: Option<DurablePreviousBranchAnchor>,
    branch_identity: Option<crate::fs_security::FileIdentitySnapshot>,
    initial_oid: String,
    current_oid: String,
    path_cleanup: DurableArtifactPhase,
    registration_cleanup: DurableArtifactPhase,
    branch_cleanup: DurableArtifactPhase,
}

#[derive(Debug)]
struct DurableOwnershipRevision {
    directory: crate::daemons::state::StableDirectory,
    path: PathBuf,
    file: std::fs::File,
    record: DurableManagedWorktreeOwnership,
}

pub(crate) struct ManagedWorktreeIntent {
    revision: DurableOwnershipRevision,
}

pub(crate) struct ManagedWorktreeReservation {
    intent: ManagedWorktreeIntent,
    registration_snapshot: ManagedWorktreeRegistrationSnapshot,
}

#[derive(Clone)]
pub(crate) struct BlockingGitCancellation {
    cancelled: Arc<AtomicBool>,
    upstream: Option<crate::agent::CancellationSignal>,
}

impl BlockingGitCancellation {
    pub(crate) fn new(upstream: Option<&crate::agent::CancellationSignal>) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            upstream: upstream.cloned(),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .upstream
                .as_ref()
                .is_some_and(crate::agent::CancellationSignal::is_cancelled)
    }
}

#[derive(Debug)]
struct OwnedRefReceipt {
    common_directory: crate::daemons::state::StableDirectory,
    directory: crate::daemons::state::StableDirectory,
    path: PathBuf,
    file: std::fs::File,
    anchor_path: Option<PathBuf>,
    anchor_file: Option<std::fs::File>,
    lock_owner: Option<String>,
    contents: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct OwnedBranch {
    reference: String,
    expected_oid: String,
    receipt: Arc<OwnedRefReceipt>,
}

#[derive(Debug)]
pub(crate) struct ManagedWorktreeReceipt {
    path: PathBuf,
    path_receipt: Option<crate::fs_security::DirectoryRemovalReceipt>,
    registration_path: PathBuf,
    registration_receipt: Option<crate::fs_security::DirectoryRemovalReceipt>,
    state: std::sync::Mutex<ManagedWorktreeState>,
}

#[derive(Debug)]
struct ManagedWorktreeState {
    owned_branch: Option<OwnedBranch>,
    path_removed: bool,
    registration_removed: bool,
    branch_removed: bool,
    reciprocal_link_proven: bool,
    durable: Option<DurableOwnershipRevision>,
}

#[derive(Debug)]
pub(crate) struct ManagedWorktreeCaptureError {
    pub(crate) message: String,
    pub(crate) ownership: Option<Box<ManagedWorktreeReceipt>>,
}

pub(crate) struct ManagedWorktreeRegistrationSnapshot {
    common_directory: crate::daemons::state::StableDirectory,
    registrations: Option<ExistingWorktreeRegistrations>,
}

struct ExistingWorktreeRegistrations {
    directory: crate::daemons::state::StableDirectory,
    entries: std::collections::HashMap<OsString, crate::fs_security::DirectoryIdentity>,
}

impl From<String> for ManagedWorktreeCaptureError {
    fn from(message: String) -> Self {
        Self {
            message,
            ownership: None,
        }
    }
}

impl From<&str> for ManagedWorktreeCaptureError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

fn managed_worktree_ownership_directory(
    project_root: &Path,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let nib = crate::fs_security::ensure_directory_without_symlinks(&project_root.join(".nib"))
        .map_err(|error| format!("managed worktree state root is unsafe: {error}"))?;
    let directory = crate::fs_security::ensure_directory_without_symlinks(
        &nib.join(MANAGED_WORKTREE_OWNERSHIP_DIRECTORY),
    )
    .map_err(|error| format!("managed worktree ownership directory is unsafe: {error}"))?;
    crate::daemons::state::StableDirectory::open(&directory)
}

fn managed_worktree_ownership_path(
    directory: &crate::daemons::state::StableDirectory,
    kind: ManagedWorktreeKind,
    logical_id: &str,
) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(b"nib-managed-worktree-ownership-v1\0");
    digest.update(kind.label().as_bytes());
    digest.update(b"\0");
    digest.update(logical_id.as_bytes());
    let digest = digest.finalize();
    let mut name = String::with_capacity(64 + 5);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(".json");
    directory.path().join(name)
}

fn encoded_name_hash(name: &OsStr) -> String {
    let digest = Sha256::digest(name.as_encoded_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn valid_git_oid(oid: &str) -> bool {
    (40..=64).contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn reservation_worktree_staging_path(
    worktree_path: &Path,
    receipt_id: &str,
) -> Result<PathBuf, String> {
    let parent = worktree_path
        .parent()
        .ok_or("managed worktree path has no parent")?;
    Ok(parent.join(format!(".nib-worktree-reservation-{receipt_id}")))
}

fn canonical_managed_worktree_reservation_paths(
    project_root: &Path,
    worktree_path: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let canonical_project_root = project_root.canonicalize().map_err(|error| {
        format!(
            "failed to resolve managed worktree project root {}: {error}",
            project_root.display()
        )
    })?;
    if !canonical_project_root.is_dir() {
        return Err(format!(
            "managed worktree project root is not a directory: {}",
            canonical_project_root.display()
        ));
    }
    let relative_worktree_path = worktree_path
        .strip_prefix(project_root)
        .or_else(|_| worktree_path.strip_prefix(&canonical_project_root))
        .map_err(|_| {
            format!(
                "managed worktree path is outside the project root: {}",
                worktree_path.display()
            )
        })?;
    if relative_worktree_path.as_os_str().is_empty()
        || relative_worktree_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("managed worktree path has an unsafe project-relative component".to_string());
    }
    let canonical_worktree_path = canonical_project_root.join(relative_worktree_path);
    Ok((canonical_project_root, canonical_worktree_path))
}

fn managed_branch_paths(
    common_git_dir: &Path,
    branch_reference: &str,
    receipt_id: &str,
    anchor_generation: u64,
) -> Result<(PathBuf, PathBuf), String> {
    let relative = branch_reference
        .strip_prefix("refs/heads/")
        .ok_or("managed worktree branch is outside refs/heads")?;
    let relative = Path::new(relative);
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("managed worktree branch has an unsafe component".to_string());
    }
    let ref_path = common_git_dir.join("refs").join("heads").join(relative);
    let parent = ref_path
        .parent()
        .ok_or("managed worktree branch has no parent directory")?
        .to_path_buf();
    Ok((
        ref_path,
        parent.join(format!(
            ".nib-branch-reservation-{receipt_id}-{anchor_generation}"
        )),
    ))
}

fn describe_reserved_branch_conflict(
    common_git_dir: &Path,
    branch_reference: &str,
) -> Result<String, String> {
    let (ref_path, _) = managed_branch_paths(common_git_dir, branch_reference, "probe", 0)?;
    if crate::fs_security::path_entry_exists(&ref_path)
        .map_err(|error| format!("failed to inspect reserved worktree branch: {error}"))?
    {
        let parent = ref_path
            .parent()
            .ok_or("managed worktree branch has no parent directory")?;
        let directory = crate::daemons::state::StableDirectory::open(parent)?;
        return Ok(describe_existing_owned_ref(
            &directory,
            &ref_path,
            branch_reference,
            "already has a loose ref",
        ));
    }
    let common = crate::daemons::state::StableDirectory::open(common_git_dir)?;
    if let Some(conflict) = packed_ref_namespace_conflict(&common, branch_reference)? {
        return Ok(format!(
            "managed worktree branch {branch_reference} conflicts with packed ref {conflict}; preserving it"
        ));
    }
    Ok(format!(
        "managed worktree branch {branch_reference} is already defined; preserving it"
    ))
}

fn validate_durable_ownership_record(
    record: &DurableManagedWorktreeOwnership,
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
) -> Result<(), String> {
    let project_root = project_root.canonicalize().map_err(|error| {
        format!(
            "failed to resolve managed worktree ownership project root {}: {error}",
            project_root.display()
        )
    })?;
    if record.schema_version != MANAGED_WORKTREE_OWNERSHIP_SCHEMA_VERSION {
        return Err(format!(
            "unsupported managed worktree ownership schema version {}",
            record.schema_version
        ));
    }
    if record.kind != kind || record.logical_id != logical_id {
        return Err("managed worktree ownership key does not match its contents".to_string());
    }
    uuid::Uuid::parse_str(&record.receipt_id)
        .map_err(|_| "managed worktree ownership receipt ID is invalid".to_string())?;
    if record.project_root != project_root {
        return Err("managed worktree ownership project root changed".to_string());
    }
    let managed_root = project_root.join(".nib").join("worktrees");
    if !record.worktree_path.is_absolute() || !record.worktree_path.starts_with(&managed_root) {
        return Err("managed worktree ownership path escapes managed state".to_string());
    }
    let expected_worktree_staging =
        reservation_worktree_staging_path(&record.worktree_path, &record.receipt_id)?;
    if record.worktree_staging_path != expected_worktree_staging
        || record.worktree_staging_path == record.worktree_path
    {
        return Err("managed worktree reservation staging path is invalid".to_string());
    }
    if !record.common_git_dir.is_absolute() || !record.common_git_dir.starts_with(&project_root) {
        return Err("managed worktree common Git directory escapes the repository".to_string());
    }
    if let Some(registration) = &record.registration_path {
        if registration.parent() != Some(record.common_git_dir.join("worktrees").as_path()) {
            return Err("managed worktree registration path escapes the Git namespace".to_string());
        }
        if record.registration_identity.is_none()
            || record.registration_namespace_identity.is_none()
        {
            return Err(
                "managed worktree registration is missing durable namespace identity".to_string(),
            );
        }
    }
    if !record.branch_reference.starts_with("refs/heads/nib/")
        || !valid_git_oid(&record.initial_oid)
        || !valid_git_oid(&record.current_oid)
    {
        return Err("managed worktree branch ownership is invalid".to_string());
    }
    let (_, expected_branch_staging) = managed_branch_paths(
        &record.common_git_dir,
        &record.branch_reference,
        &record.receipt_id,
        record.branch_anchor_generation,
    )?;
    if record.branch_staging_path != expected_branch_staging {
        return Err("managed worktree branch reservation staging path is invalid".to_string());
    }
    if let Some(previous) = &record.previous_branch_anchor {
        let previous_generation = record
            .branch_anchor_generation
            .checked_sub(1)
            .ok_or("managed branch previous anchor has no prior generation")?;
        let (_, expected_previous_path) = managed_branch_paths(
            &record.common_git_dir,
            &record.branch_reference,
            &record.receipt_id,
            previous_generation,
        )?;
        if previous.path != expected_previous_path || !valid_git_oid(&previous.oid) {
            return Err("managed branch previous generation anchor is invalid".to_string());
        }
    }
    if matches!(record.path_cleanup, DurableArtifactPhase::Reserved)
        && record.worktree_identity.is_some()
    {
        return Err("reserved worktree path unexpectedly has a durable identity".to_string());
    }
    if matches!(record.branch_cleanup, DurableArtifactPhase::Reserved)
        && record.branch_identity.is_some()
    {
        return Err("reserved worktree branch unexpectedly has a durable identity".to_string());
    }
    if matches!(
        record.path_cleanup,
        DurableArtifactPhase::Present | DurableArtifactPhase::Removing
    ) && record.worktree_identity.is_none()
    {
        return Err("managed worktree path phase is missing its durable identity".to_string());
    }
    if matches!(
        record.branch_cleanup,
        DurableArtifactPhase::Present | DurableArtifactPhase::Removing
    ) && record.branch_identity.is_none()
    {
        return Err("managed worktree branch phase is missing its durable identity".to_string());
    }
    if matches!(
        record.phase,
        DurableOwnershipPhase::Owned | DurableOwnershipPhase::Cleanup
    ) && (record.path_cleanup == DurableArtifactPhase::Reserved
        || record.branch_cleanup == DurableArtifactPhase::Reserved
        || record.worktree_identity.is_none()
        || record.branch_identity.is_none()
        || !record.registration_snapshot_captured)
    {
        return Err("managed worktree ownership generation is incomplete".to_string());
    }
    if record.phase == DurableOwnershipPhase::Complete
        && (record.path_cleanup != DurableArtifactPhase::Removed
            || record.registration_cleanup != DurableArtifactPhase::Removed
            || record.branch_cleanup != DurableArtifactPhase::Removed
            || record.previous_branch_anchor.is_some())
    {
        return Err("completed managed worktree ownership has incomplete artifacts".to_string());
    }
    Ok(())
}

fn encode_durable_ownership(record: &DurableManagedWorktreeOwnership) -> Result<Vec<u8>, String> {
    let encoded = serde_json::to_vec_pretty(record)
        .map_err(|error| format!("failed to encode managed worktree ownership: {error}"))?;
    if encoded.len() as u64 > MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES {
        return Err(format!(
            "managed worktree ownership exceeds {} bytes",
            MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES
        ));
    }
    Ok(encoded)
}

fn publish_durable_ownership(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    encoded: &[u8],
    previous: Option<&std::fs::File>,
) -> Result<crate::daemons::state::FilePublicationReceipt, String> {
    let expected = previous.map_or(
        crate::daemons::state::FileExpectation::Missing,
        crate::daemons::state::FileExpectation::Present,
    );
    match directory.save_bytes_atomically_expected_with_receipt(
        path,
        encoded,
        MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
        expected,
    ) {
        Ok(receipt) => Ok(receipt),
        Err(mut error) => {
            let Some(receipt) = error.receipt.take() else {
                return Err(error.message);
            };
            if !receipt.exact_identity {
                return Err(format!(
                    "{}; managed worktree ownership publication lacks exact recovery identity",
                    error.message
                ));
            }
            directory
                .finalize_failed_exact_publication(
                    path,
                    previous,
                    &receipt,
                    MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
                    encoded,
                )
                .map_err(|recovery| {
                    format!(
                        "{}; managed worktree ownership publication recovery failed and ambiguous state was preserved: {recovery}",
                        error.message
                    )
                })?;
            Ok(receipt)
        }
    }
}

fn decode_durable_ownership_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    file: &std::fs::File,
) -> Result<DurableManagedWorktreeOwnership, String> {
    let length = file
        .metadata()
        .map_err(|error| format!("failed to inspect managed worktree ownership: {error}"))?
        .len();
    if length > MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES {
        return Err(format!(
            "managed worktree ownership exceeds {} bytes",
            MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES
        ));
    }
    let read_limit = usize::try_from(MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES + 1)
        .map_err(|_| "managed worktree ownership read limit does not fit usize".to_string())?;
    let encoded = crate::daemons::state::read_open_file_prefix(file, read_limit)
        .map_err(|error| format!("failed to read managed worktree ownership: {error}"))?;
    if encoded.len() as u64 > MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES {
        return Err("managed worktree ownership exceeds its read limit".to_string());
    }
    directory.verify_file_identity(path, file)?;
    let record: DurableManagedWorktreeOwnership = serde_json::from_slice(&encoded)
        .map_err(|error| format!("managed worktree ownership is invalid: {error}"))?;
    Ok(record)
}

fn read_durable_ownership_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    file: &std::fs::File,
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
) -> Result<DurableManagedWorktreeOwnership, String> {
    let record = decode_durable_ownership_file(directory, path, file)?;
    validate_durable_ownership_record(&record, project_root, kind, logical_id)?;
    Ok(record)
}

fn lock_dead_ownership_artifact(
    file: &std::fs::File,
    path: &Path,
    label: &str,
) -> Result<(), String> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "managed worktree ownership {label} is still owned by a live writer: {}",
            path.display()
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "failed to inspect managed worktree ownership {label} kernel ownership: {error}: {}",
            path.display()
        )),
    }
}

fn recover_durable_ownership_transaction(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
) -> Result<(), String> {
    let temporary = directory.deterministic_artifact_path(
        path,
        MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
        ".tmp",
    )?;
    let previous = directory
        .deterministic_previous_artifact_path(path, MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX)?;
    let target_exists = directory.path_exists(path)?;
    let temporary_exists = directory.path_exists(&temporary)?;
    let previous_exists = directory.path_exists(&previous)?;
    if !temporary_exists && !previous_exists {
        return Ok(());
    }

    let target_file = target_exists
        .then(|| directory.open_read_write(path))
        .transpose()?;
    let temporary_file = temporary_exists
        .then(|| directory.open_read_write(&temporary))
        .transpose()?;
    let previous_file = previous_exists
        .then(|| directory.open_read_write(&previous))
        .transpose()?;
    let target_identity = target_file
        .as_ref()
        .map(crate::fs_security::file_identity_snapshot)
        .transpose()
        .map_err(|error| format!("failed to inspect ownership target: {error}"))?;
    let previous_identity = previous_file
        .as_ref()
        .map(crate::fs_security::file_identity_snapshot)
        .transpose()
        .map_err(|error| format!("failed to inspect ownership previous artifact: {error}"))?;
    let temporary_identity = temporary_file
        .as_ref()
        .map(crate::fs_security::file_identity_snapshot)
        .transpose()
        .map_err(|error| format!("failed to inspect ownership temporary artifact: {error}"))?;
    if let Some(file) = target_file.as_ref() {
        directory.verify_file_identity(path, file)?;
        lock_dead_ownership_artifact(file, path, "target")?;
    }
    if let Some(file) = previous_file.as_ref() {
        directory.verify_file_identity(&previous, file)?;
        if previous_identity != target_identity {
            lock_dead_ownership_artifact(file, &previous, "previous artifact")?;
        }
    }
    if let Some(file) = temporary_file.as_ref() {
        directory.verify_file_identity(&temporary, file)?;
        if temporary_identity != target_identity && temporary_identity != previous_identity {
            lock_dead_ownership_artifact(file, &temporary, "temporary artifact")?;
        }
    }

    if let Some(file) = target_file.as_ref() {
        let _ = read_durable_ownership_file(directory, path, file, project_root, kind, logical_id)?;
    }
    if let Some(file) = previous_file.as_ref() {
        let _ = read_durable_ownership_file(
            directory,
            &previous,
            file,
            project_root,
            kind,
            logical_id,
        )?;
    }
    if previous_file.is_some() {
        if let (Some(_), Some(_)) = (target_file.as_ref(), temporary_file.as_ref()) {
            if target_identity != temporary_identity {
                return Err(
                    "committed managed worktree ownership target and temporary artifact have distinct identities; both were preserved"
                        .to_string(),
                );
            }
        }
    }

    if target_file.is_some() {
        if let Some(previous_file) = previous_file.as_ref() {
            directory.remove_visible_file_if_matches_direct(&previous, previous_file)?;
        }
        if let Some(temporary_file) = temporary_file.as_ref() {
            directory.remove_visible_file_if_matches_direct(&temporary, temporary_file)?;
        }
    } else {
        if let Some(temporary_file) = temporary_file.as_ref() {
            directory.remove_visible_file_if_matches_direct(&temporary, temporary_file)?;
        }
        if let Some(previous_file) = previous_file.as_ref() {
            directory.restore_visible_file_no_replace_if_matches(&previous, previous_file, path)?;
        }
    }
    directory.sync_directory()?;
    if directory.path_exists(&temporary)? || directory.path_exists(&previous)? {
        return Err(format!(
            "managed worktree ownership transaction recovery left scratch for {}",
            path.display()
        ));
    }
    Ok(())
}

fn recover_all_durable_ownership_transactions(
    directory: &crate::daemons::state::StableDirectory,
    project_root: &Path,
) -> Result<(), String> {
    let mut targets = Vec::new();
    directory.for_each_entry_bounded(
        MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_ENTRIES,
        MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_NAME_BYTES,
        |name| {
            if let Some(target) =
                crate::daemons::state::StableDirectory::atomic_previous_target_name(
                    &name,
                    MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
                )
            {
                targets.push(target);
            }
            Ok(())
        },
    )?;
    targets.sort();
    targets.dedup();
    for target_name in targets {
        let target = directory.path().join(&target_name);
        let previous = directory.deterministic_previous_artifact_path(
            &target,
            MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
        )?;
        let source = if directory.path_exists(&target)? {
            &target
        } else {
            &previous
        };
        let file = directory.open_read(source)?;
        let record = decode_durable_ownership_file(directory, source, &file)?;
        validate_durable_ownership_record(&record, project_root, record.kind, &record.logical_id)?;
        if managed_worktree_ownership_path(directory, record.kind, &record.logical_id) != target {
            return Err(format!(
                "managed worktree ownership recovery artifact does not match its durable key: {}",
                source.display()
            ));
        }
        recover_durable_ownership_transaction(
            directory,
            &target,
            project_root,
            record.kind,
            &record.logical_id,
        )?;
    }
    directory.recover_stale_temporary_files_strict(
        MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
        MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_ENTRIES,
        MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_NAME_BYTES,
    )?;
    Ok(())
}

fn load_durable_ownership_revision(
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
) -> Result<Option<DurableOwnershipRevision>, String> {
    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("invalid managed worktree project root: {error}"))?;
    let directory = managed_worktree_ownership_directory(&project_root)?;
    let path = managed_worktree_ownership_path(&directory, kind, logical_id);
    recover_durable_ownership_transaction(&directory, &path, &project_root, kind, logical_id)?;
    if !directory.path_exists(&path)? {
        return Ok(None);
    }
    let file = directory.open_read(&path)?;
    let record =
        read_durable_ownership_file(&directory, &path, &file, &project_root, kind, logical_id)?;
    Ok(Some(DurableOwnershipRevision {
        directory,
        path,
        file,
        record,
    }))
}

const OWNERSHIP_COMPACTION_LOCK_NAME: &str = ".nib-worktree-ownership-compaction.lock";
const OWNERSHIP_COMPACTION_ANCHOR_NAME: &str = ".nib-worktree-ownership-compaction.anchor";

struct OwnershipCompactionLock {
    _anchor: std::fs::File,
}

impl OwnershipCompactionLock {
    fn acquire(
        directory: &crate::daemons::state::StableDirectory,
        timeout: Duration,
    ) -> Result<Self, String> {
        let visible = directory.path().join(OWNERSHIP_COMPACTION_LOCK_NAME);
        let anchor = directory.path().join(OWNERSHIP_COMPACTION_ANCHOR_NAME);
        let visible_exists = directory.path_exists(&visible)?;
        let anchor_exists = directory.path_exists(&anchor)?;
        match (visible_exists, anchor_exists) {
            (false, false) => {
                drop(directory.open_read_write_create(&visible)?);
                directory.hard_link_to(&visible, directory, &anchor)?;
            }
            (true, false) => directory.hard_link_to(&visible, directory, &anchor)?,
            (false, true) => directory.hard_link_to(&anchor, directory, &visible)?,
            (true, true) => {}
        }
        directory.sync_directory()?;
        let anchor_file = directory.open_read_write(&anchor)?;
        directory
            .verify_file_identity(&visible, &anchor_file)
            .map_err(|error| {
                format!(
                    "managed worktree ownership lock and anchor differ; both were preserved: {error}"
                )
            })?;
        let deadline = Instant::now() + timeout;
        loop {
            match anchor_file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error))
                    if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "failed to acquire managed worktree ownership lock: {error}"
                    ));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "timed out acquiring managed worktree ownership lock after {} seconds",
                    timeout.as_secs_f64()
                ));
            }
            std::thread::sleep(Duration::from_millis(10).min(deadline - now));
        }
        directory.verify_file_identity(&visible, &anchor_file)?;
        Ok(Self {
            _anchor: anchor_file,
        })
    }
}

fn compact_complete_ownership_records_with_limits(
    directory: &crate::daemons::state::StableDirectory,
    project_root: &Path,
    target_path: &Path,
    replacement_bytes: u64,
    max_records: usize,
    max_aggregate_bytes: u64,
) -> Result<(), String> {
    struct Candidate {
        path: PathBuf,
        file: std::fs::File,
        bytes: u64,
    }

    recover_all_durable_ownership_transactions(directory, project_root)?;
    let mut record_count = 0_usize;
    let mut aggregate_bytes = 0_u64;
    let mut target_bytes = 0_u64;
    let mut candidates = Vec::new();
    directory.for_each_entry_bounded(
        MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_ENTRIES,
        MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_NAME_BYTES,
        |name| {
            if name == OsStr::new(OWNERSHIP_COMPACTION_LOCK_NAME)
                || name == OsStr::new(OWNERSHIP_COMPACTION_ANCHOR_NAME)
            {
                return Ok(());
            }
            let path = directory.path().join(&name);
            if Path::new(&name).extension() != Some(OsStr::new("json")) {
                return Err(format!(
                    "managed worktree ownership directory contains an unrecognized artifact; aggregate bounds cannot be proven: {}",
                    path.display()
                ));
            }
            let mut file = directory.open_read(&path)?;
            let bytes = file
                .metadata()
                .map_err(|error| format!("failed to inspect managed ownership record: {error}"))?
                .len();
            if bytes > MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES {
                return Err(format!(
                    "managed worktree ownership record exceeds {} bytes: {}",
                    MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES,
                    path.display()
                ));
            }
            let mut encoded = Vec::with_capacity(bytes as usize);
            file.by_ref()
                .take(MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES + 1)
                .read_to_end(&mut encoded)
                .map_err(|error| format!("failed to read managed ownership record: {error}"))?;
            directory.verify_file_identity(&path, &file)?;
            let record: DurableManagedWorktreeOwnership = serde_json::from_slice(&encoded)
                .map_err(|error| format!("managed worktree ownership is invalid: {error}"))?;
            validate_durable_ownership_record(
                &record,
                project_root,
                record.kind,
                &record.logical_id,
            )?;
            if managed_worktree_ownership_path(directory, record.kind, &record.logical_id) != path {
                return Err(format!(
                    "managed worktree ownership filename does not match its durable key: {}",
                    path.display()
                ));
            }
            record_count = record_count
                .checked_add(1)
                .ok_or("managed worktree ownership record count overflowed")?;
            aggregate_bytes = aggregate_bytes
                .checked_add(bytes)
                .ok_or("managed worktree ownership aggregate byte count overflowed")?;
            if path == target_path {
                target_bytes = bytes;
            } else if record.phase == DurableOwnershipPhase::Complete {
                recover_owned_ref_restart_artifacts(&record)?;
                candidates.push(Candidate { path, file, bytes });
            }
            Ok(())
        },
    )?;

    let mut prospective_count = record_count + usize::from(target_bytes == 0);
    let mut prospective_bytes = aggregate_bytes
        .checked_sub(target_bytes)
        .and_then(|bytes| bytes.checked_add(replacement_bytes))
        .ok_or("managed worktree ownership prospective size overflowed")?;
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    for candidate in candidates {
        if prospective_count <= max_records && prospective_bytes <= max_aggregate_bytes {
            break;
        }
        directory.remove_visible_file_if_matches_direct(&candidate.path, &candidate.file)?;
        prospective_count = prospective_count.saturating_sub(1);
        prospective_bytes = prospective_bytes.saturating_sub(candidate.bytes);
    }
    if prospective_count > max_records || prospective_bytes > max_aggregate_bytes {
        return Err(format!(
            "active managed worktree ownership exceeds the durable namespace bound ({prospective_count}/{max_records} records, {prospective_bytes}/{max_aggregate_bytes} bytes)"
        ));
    }
    Ok(())
}

fn compact_complete_ownership_records(
    directory: &crate::daemons::state::StableDirectory,
    project_root: &Path,
    target_path: &Path,
    replacement_bytes: u64,
) -> Result<(), String> {
    compact_complete_ownership_records_with_limits(
        directory,
        project_root,
        target_path,
        replacement_bytes,
        MAX_MANAGED_WORKTREE_OWNERSHIP_RECORDS,
        MAX_MANAGED_WORKTREE_OWNERSHIP_AGGREGATE_BYTES,
    )
}

fn persist_durable_ownership_revision(
    revision: &mut DurableOwnershipRevision,
    record: DurableManagedWorktreeOwnership,
) -> Result<(), String> {
    validate_durable_ownership_record(
        &record,
        &record.project_root,
        record.kind,
        &record.logical_id,
    )?;
    let encoded = encode_durable_ownership(&record)?;
    let publication = publish_durable_ownership(
        &revision.directory,
        &revision.path,
        &encoded,
        Some(&revision.file),
    )?;
    if !publication.exact_identity {
        return Err(
            "managed worktree ownership update lacks an exact publication identity".to_string(),
        );
    }
    revision.file = publication.file;
    revision.record = record;
    Ok(())
}

// Reservation persistence validates each durable identity independently; a
// parameter bag would weaken that correspondence without simplifying callers.
#[allow(clippy::too_many_arguments)]
fn persist_managed_worktree_reservation(
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
    worktree_path: &Path,
    branch_reference: String,
    initial_oid: String,
    registration_snapshot: ManagedWorktreeRegistrationSnapshot,
    planned_receipt_id: Option<&str>,
) -> Result<ManagedWorktreeReservation, String> {
    let ownership_directory = managed_worktree_ownership_directory(project_root)?;
    let _compaction_lock =
        OwnershipCompactionLock::acquire(&ownership_directory, GIT_COMMAND_TIMEOUT)?;
    let existing = load_durable_ownership_revision(project_root, kind, logical_id)?;
    if let Some(existing) = existing.as_ref() {
        if existing.record.phase != DurableOwnershipPhase::Complete {
            return Err(format!(
                "managed {} worktree {} already has a durable ownership receipt {} in phase {:?}",
                kind.label(),
                logical_id,
                existing.record.receipt_id,
                existing.record.phase
            ));
        }
        recover_owned_ref_restart_artifacts(&existing.record)?;
    }
    let common_git_dir = registration_snapshot.common_directory.path().to_path_buf();
    let common_git_identity = registration_snapshot
        .common_directory
        .directory_removal_receipt()?
        .identity();
    let registration_namespace_identity = registration_snapshot
        .registrations
        .as_ref()
        .map(|registrations| registrations.directory.directory_removal_receipt())
        .transpose()?
        .map(|receipt| receipt.identity());
    let mut preexisting_registration_name_hashes = registration_snapshot
        .registrations
        .as_ref()
        .map(|registrations| {
            registrations
                .entries
                .keys()
                .map(|name| encoded_name_hash(name))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    preexisting_registration_name_hashes.sort();
    let mut preexisting_registration_identities = registration_snapshot
        .registrations
        .as_ref()
        .map(|registrations| registrations.entries.values().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    preexisting_registration_identities.sort_by_key(|identity| format!("{identity:?}"));
    let receipt_id = planned_receipt_id
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let worktree_staging_path = reservation_worktree_staging_path(worktree_path, &receipt_id)?;
    let (_, branch_staging_path) =
        managed_branch_paths(&common_git_dir, &branch_reference, &receipt_id, 0)?;
    let record = DurableManagedWorktreeOwnership {
        schema_version: MANAGED_WORKTREE_OWNERSHIP_SCHEMA_VERSION,
        receipt_id,
        kind,
        logical_id: logical_id.to_string(),
        phase: DurableOwnershipPhase::Intent,
        project_root: project_root.to_path_buf(),
        common_git_dir,
        common_git_identity,
        worktree_path: worktree_path.to_path_buf(),
        worktree_staging_path,
        worktree_identity: None,
        registration_path: None,
        registration_identity: None,
        registration_namespace_identity,
        registration_snapshot_captured: true,
        preexisting_registration_name_hashes,
        preexisting_registration_identities,
        branch_reference,
        branch_staging_path,
        branch_anchor_generation: 0,
        previous_branch_anchor: None,
        branch_identity: None,
        initial_oid: initial_oid.clone(),
        current_oid: initial_oid,
        path_cleanup: DurableArtifactPhase::Reserved,
        registration_cleanup: DurableArtifactPhase::Unattributed,
        branch_cleanup: DurableArtifactPhase::Reserved,
    };
    validate_durable_ownership_record(&record, project_root, kind, logical_id)?;
    let encoded = encode_durable_ownership(&record)?;
    let ownership_path = managed_worktree_ownership_path(&ownership_directory, kind, logical_id);
    compact_complete_ownership_records(
        &ownership_directory,
        project_root,
        &ownership_path,
        encoded.len() as u64,
    )?;
    let revision = if let Some(mut existing) = existing {
        persist_durable_ownership_revision(&mut existing, record)?;
        existing
    } else {
        let directory = ownership_directory.try_clone()?;
        let path = ownership_path;
        let publication = publish_durable_ownership(&directory, &path, &encoded, None)?;
        if !publication.exact_identity {
            return Err(
                "managed worktree reservation lacks an exact publication identity".to_string(),
            );
        }
        DurableOwnershipRevision {
            directory,
            path,
            file: publication.file,
            record,
        }
    };
    Ok(ManagedWorktreeReservation {
        intent: ManagedWorktreeIntent { revision },
        registration_snapshot,
    })
}

pub(crate) fn reserve_managed_worktree_sync_controlled(
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
    worktree_path: &Path,
    branch: &str,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<ManagedWorktreeReservation, String> {
    reserve_managed_worktree_sync_controlled_with_receipt(
        project_root,
        kind,
        logical_id,
        worktree_path,
        branch,
        cancellation,
        None,
    )
}

fn reserve_managed_worktree_sync_controlled_with_receipt(
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
    worktree_path: &Path,
    branch: &str,
    cancellation: Option<&BlockingGitCancellation>,
    planned_receipt_id: Option<&str>,
) -> Result<ManagedWorktreeReservation, String> {
    let (project_root, worktree_path) =
        canonical_managed_worktree_reservation_paths(project_root, worktree_path)?;
    let head = run_git_bounded_sync_with_timeout_controlled(
        &project_root,
        ["rev-parse", "--verify", "HEAD^{commit}"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let initial_oid = parse_git_oid(&head, "resolve reserved branch base")?;
    let common = run_git_bounded_sync_with_timeout_controlled(
        &project_root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let common = parse_common_git_directory(&project_root, &common)?;
    let branch_reference = format!("refs/heads/{branch}");
    let existing = run_git_bounded_sync_with_timeout_controlled(
        &project_root,
        ["show-ref", "--verify", "--quiet", branch_reference.as_str()],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    match existing.status.code() {
        Some(1) => {}
        Some(0) => {
            return Err(describe_reserved_branch_conflict(
                &common,
                &branch_reference,
            )?)
        }
        _ => return Err(git_failure(&existing, "inspect reserved worktree branch")),
    }
    let common_directory = crate::daemons::state::StableDirectory::open(&common)?;
    let registration_snapshot =
        capture_worktree_registration_snapshot_from_common(common_directory, &worktree_path)?;
    persist_managed_worktree_reservation(
        &project_root,
        kind,
        logical_id,
        &worktree_path,
        branch_reference,
        initial_oid,
        registration_snapshot,
        planned_receipt_id,
    )
}

async fn reserve_managed_worktree_cancellable_with_receipt(
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
    worktree_path: &Path,
    branch: &str,
    cancellation: Option<&crate::agent::CancellationSignal>,
    planned_receipt_id: Option<&str>,
) -> Result<ManagedWorktreeReservation, String> {
    let (project_root, worktree_path) =
        canonical_managed_worktree_reservation_paths(project_root, worktree_path)?;
    let head = run_git_cancellable(
        &project_root,
        ["rev-parse", "--verify", "HEAD^{commit}"],
        cancellation,
    )
    .await?;
    let initial_oid = parse_git_oid(&head, "resolve reserved branch base")?;
    let common = run_git_cancellable(
        &project_root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        cancellation,
    )
    .await?;
    let common = parse_common_git_directory(&project_root, &common)?;
    let branch_reference = format!("refs/heads/{branch}");
    let existing = run_git_cancellable(
        &project_root,
        ["show-ref", "--verify", "--quiet", branch_reference.as_str()],
        cancellation,
    )
    .await?;
    match existing.status.code() {
        Some(1) => {}
        Some(0) => {
            return Err(describe_reserved_branch_conflict(
                &common,
                &branch_reference,
            )?)
        }
        _ => return Err(git_failure(&existing, "inspect reserved worktree branch")),
    }
    let common_directory = crate::daemons::state::StableDirectory::open(&common)?;
    let registration_snapshot =
        capture_worktree_registration_snapshot_from_common(common_directory, &worktree_path)?;
    persist_managed_worktree_reservation(
        &project_root,
        kind,
        logical_id,
        &worktree_path,
        branch_reference,
        initial_oid,
        registration_snapshot,
        planned_receipt_id,
    )
}

pub(crate) fn reserved_worktree_registration_snapshot(
    reservation: &ManagedWorktreeReservation,
) -> &ManagedWorktreeRegistrationSnapshot {
    &reservation.registration_snapshot
}

pub(crate) fn finish_managed_worktree_reservation(
    reservation: ManagedWorktreeReservation,
    ownership: ManagedWorktreeReceipt,
) -> Result<ManagedWorktreeReceipt, ManagedWorktreeCaptureError> {
    let ManagedWorktreeReservation {
        intent,
        registration_snapshot,
    } = reservation;
    drop(registration_snapshot);
    finalize_managed_worktree_intent(intent, ownership)
}

pub(crate) fn reconcile_failed_managed_worktree_reservation(
    reservation: ManagedWorktreeReservation,
    primary: String,
) -> String {
    let ManagedWorktreeReservation {
        intent,
        registration_snapshot,
    } = reservation;
    drop(registration_snapshot);
    match reconcile_unfinished_intent(intent.revision, &primary) {
        Ok(()) => primary,
        Err(recovery) => format!("{primary}; durable creation-intent recovery failed: {recovery}"),
    }
}

pub(crate) fn finalize_managed_worktree_intent(
    mut intent: ManagedWorktreeIntent,
    ownership: ManagedWorktreeReceipt,
) -> Result<ManagedWorktreeReceipt, ManagedWorktreeCaptureError> {
    let mut record = intent.revision.record.clone();
    let registration_namespace_identity = ownership
        .registration_path
        .parent()
        .ok_or_else(|| "managed worktree registration has no namespace".to_string())
        .and_then(|parent| {
            crate::daemons::state::StableDirectory::open(parent)
                .and_then(|directory| directory.directory_removal_receipt())
                .map(|receipt| receipt.identity())
        });
    let registration_namespace_identity = match registration_namespace_identity {
        Ok(identity) => identity,
        Err(message) => {
            return Err(ManagedWorktreeCaptureError {
                message: format!(
                    "failed to retain managed worktree registration namespace: {message}"
                ),
                ownership: Some(Box::new(ownership)),
            })
        }
    };
    record.registration_path = Some(ownership.registration_path.clone());
    record.registration_identity = Some(
        ownership
            .registration_receipt
            .as_ref()
            .expect("newly captured worktree registration has an ownership receipt")
            .identity(),
    );
    record.registration_namespace_identity = Some(registration_namespace_identity);
    record.registration_cleanup = DurableArtifactPhase::Present;
    record.phase = DurableOwnershipPhase::Owned;
    if let Err(message) = persist_durable_ownership_revision(&mut intent.revision, record) {
        return Err(ManagedWorktreeCaptureError {
            message: format!("failed to commit managed worktree ownership receipt: {message}"),
            ownership: Some(Box::new(ownership)),
        });
    }
    ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .durable = Some(intent.revision);
    Ok(ownership)
}

fn reopen_directory_receipt(
    parent: &Path,
    path: &Path,
    expected: crate::fs_security::DirectoryIdentity,
    label: &str,
) -> Result<crate::fs_security::DirectoryRemovalReceipt, String> {
    let receipt = crate::fs_security::capture_directory_removal_receipt(parent, path)
        .map_err(|error| format!("failed to reopen {label}: {error}"))?;
    if receipt.identity() != expected {
        return Err(format!(
            "{label} no longer matches its durable ownership identity; replacement preserved: {}",
            path.display()
        ));
    }
    Ok(receipt)
}

fn reopen_common_git_directory(
    record: &DurableManagedWorktreeOwnership,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let common = crate::daemons::state::StableDirectory::open(&record.common_git_dir)?;
    if common.directory_removal_receipt()?.identity() != record.common_git_identity {
        return Err("managed worktree common Git directory identity changed".to_string());
    }
    Ok(common)
}

fn reopen_owned_branch(
    record: &DurableManagedWorktreeOwnership,
    expected_oid: &str,
    allow_identity_transition: bool,
) -> Result<OwnedBranch, String> {
    if !valid_git_oid(expected_oid) {
        return Err("managed worktree branch revision is invalid".to_string());
    }
    let relative = record
        .branch_reference
        .strip_prefix("refs/heads/")
        .ok_or("managed worktree branch is outside refs/heads")?;
    let relative = Path::new(relative);
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("managed worktree branch has an unsafe component".to_string());
    }
    let common_directory = reopen_common_git_directory(record)?;
    let ref_path = record
        .common_git_dir
        .join("refs")
        .join("heads")
        .join(relative);
    let ref_parent = ref_path
        .parent()
        .ok_or("managed worktree branch has no parent directory")?;
    let ref_directory = crate::daemons::state::StableDirectory::open(ref_parent)?;
    let file = ref_directory.open_read(&ref_path)?;
    let contents = format!("{expected_oid}\n").into_bytes();
    verify_open_file_contents(&file, &contents)?;
    ref_directory.verify_file_identity(&ref_path, &file)?;
    let identity = crate::fs_security::file_identity_snapshot(&file)
        .map_err(|error| format!("failed to reopen managed branch identity: {error}"))?;
    if !allow_identity_transition && Some(identity) != record.branch_identity {
        return Err(format!(
            "managed worktree branch no longer matches its durable ownership identity; replacement preserved: {}",
            ref_path.display()
        ));
    }
    let anchor_path = if allow_identity_transition {
        let next_generation = record
            .branch_anchor_generation
            .checked_add(1)
            .ok_or("managed branch anchor generation overflowed")?;
        managed_branch_paths(
            &record.common_git_dir,
            &record.branch_reference,
            &record.receipt_id,
            next_generation,
        )?
        .1
    } else {
        record.branch_staging_path.clone()
    };
    if allow_identity_transition && !ref_directory.path_exists(&anchor_path)? {
        ref_directory.hard_link_to(&ref_path, &ref_directory, &anchor_path)?;
    }
    let anchor_file = ref_directory.open_read(&anchor_path).map_err(|error| {
        format!("managed branch generation anchor is unavailable; preserving the ref: {error}")
    })?;
    ref_directory
        .verify_file_identity(&anchor_path, &file)
        .map_err(|error| {
            format!("managed branch ref no longer matches its retained generation anchor: {error}")
        })?;
    verify_open_file_contents(&anchor_file, &contents)?;
    Ok(OwnedBranch {
        reference: record.branch_reference.clone(),
        expected_oid: expected_oid.to_string(),
        receipt: Arc::new(OwnedRefReceipt {
            common_directory,
            directory: ref_directory,
            path: ref_path,
            file,
            anchor_path: Some(anchor_path),
            anchor_file: Some(anchor_file),
            lock_owner: Some(record.receipt_id.clone()),
            contents,
        }),
    })
}

fn promote_durable_intent(
    mut revision: DurableOwnershipRevision,
) -> Result<ManagedWorktreeReceipt, String> {
    let record = revision.record.clone();
    if record.path_cleanup != DurableArtifactPhase::Present
        || record.branch_cleanup != DurableArtifactPhase::Present
        || record.previous_branch_anchor.is_some()
    {
        return Err(
            "managed worktree intent artifacts are not fully published for promotion".to_string(),
        );
    }
    if crate::fs_security::path_entry_exists(&record.worktree_staging_path)
        .map_err(|error| format!("failed to inspect worktree staging during promotion: {error}"))?
    {
        return Err(
            "managed worktree intent still has an unpublished staged directory".to_string(),
        );
    }
    let path_parent = record
        .worktree_path
        .parent()
        .ok_or("managed worktree path has no parent")?;
    let path_receipt = reopen_directory_receipt(
        path_parent,
        &record.worktree_path,
        record
            .worktree_identity
            .ok_or("managed worktree intent has no path identity")?,
        "managed worktree intent path",
    )?;
    let common_directory = reopen_common_git_directory(&record)?;
    let worktree_directory = open_stable_direct_child(&record.worktree_path)?;
    let reported_registration_path = parse_gitdir_pointer(
        &read_small_stable_file(&worktree_directory, &record.worktree_path.join(".git"))?,
        "managed worktree intent .git pointer",
    )?;
    let registrations_path = record.common_git_dir.join("worktrees");
    let registration_path = trusted_git_registration_path(
        &registrations_path,
        &reported_registration_path,
        "managed worktree intent",
    )?;
    let registration_name = registration_path
        .file_name()
        .ok_or("managed worktree registration has no filename")?;
    if record
        .preexisting_registration_name_hashes
        .binary_search(&encoded_name_hash(registration_name))
        .is_ok()
    {
        return Err(
            "managed worktree registration predates the durable creation intent".to_string(),
        );
    }
    let registrations_directory = common_directory.open_child(&registrations_path)?;
    if let Some(expected) = record.registration_namespace_identity {
        if registrations_directory
            .directory_removal_receipt()?
            .identity()
            != expected
        {
            return Err(
                "Git worktree registration namespace changed after the durable intent".to_string(),
            );
        }
    }
    let registration_directory = registrations_directory.open_child(&registration_path)?;
    let registration_receipt = registration_directory.directory_removal_receipt()?;
    if record
        .preexisting_registration_identities
        .contains(&registration_receipt.identity())
    {
        return Err("managed worktree registration reused a pre-intent identity".to_string());
    }
    validate_reciprocal_worktree_link_opened(
        &record.worktree_path,
        &worktree_directory,
        &registration_path,
        &registration_directory,
    )?;
    let owned_branch = reopen_owned_branch(&record, &record.current_oid, false)?;
    let mut committed = record;
    committed.phase = DurableOwnershipPhase::Owned;
    committed.registration_path = Some(registration_path.clone());
    committed.registration_identity = Some(registration_receipt.identity());
    committed.registration_namespace_identity = Some(
        registrations_directory
            .directory_removal_receipt()?
            .identity(),
    );
    committed.registration_cleanup = DurableArtifactPhase::Present;
    persist_durable_ownership_revision(&mut revision, committed)?;
    Ok(ManagedWorktreeReceipt {
        path: revision.record.worktree_path.clone(),
        path_receipt: Some(path_receipt),
        registration_path,
        registration_receipt: Some(registration_receipt),
        state: std::sync::Mutex::new(ManagedWorktreeState {
            owned_branch: Some(owned_branch),
            path_removed: false,
            registration_removed: false,
            branch_removed: false,
            reciprocal_link_proven: true,
            durable: Some(revision),
        }),
    })
}

fn reopen_durable_directory_artifact(
    parent: &Path,
    path: &Path,
    expected: crate::fs_security::DirectoryIdentity,
    phase: DurableArtifactPhase,
    label: &str,
) -> Result<(Option<crate::fs_security::DirectoryRemovalReceipt>, bool), String> {
    let exists = crate::fs_security::path_entry_exists(path)
        .map_err(|error| format!("failed to inspect {label}: {error}"))?;
    match (phase, exists) {
        (DurableArtifactPhase::Removed, _) => Ok((None, true)),
        (DurableArtifactPhase::Removing, false) => {
            if crate::fs_security::directory_removal_quarantine_exists(parent, path)
                .map_err(|error| format!("failed to inspect {label} cleanup quarantine: {error}"))?
            {
                Err(format!(
                    "{label} has a persisted cleanup quarantine requiring exact physical recovery: {}",
                    path.display()
                ))
            } else {
                Ok((None, true))
            }
        }
        (DurableArtifactPhase::Present | DurableArtifactPhase::Removing, true) => {
            reopen_directory_receipt(parent, path, expected, label)
                .map(|receipt| (Some(receipt), false))
        }
        (DurableArtifactPhase::Present, false) => Err(format!(
            "{label} disappeared before durable cleanup began: {}",
            path.display()
        )),
        (DurableArtifactPhase::Reserved | DurableArtifactPhase::Unattributed, _) => {
            Err(format!("{label} has no durable ownership attribution"))
        }
    }
}

fn durable_branch_is_absent(record: &DurableManagedWorktreeOwnership) -> Result<bool, String> {
    let common = reopen_common_git_directory(record)?;
    let (path, expected_anchor) = managed_branch_paths(
        &record.common_git_dir,
        &record.branch_reference,
        &record.receipt_id,
        record.branch_anchor_generation,
    )?;
    if expected_anchor != record.branch_staging_path {
        return Err("managed branch generation anchor path changed".to_string());
    }
    let ref_parent = path
        .parent()
        .ok_or("managed worktree branch has no parent directory")?;
    let ref_directory = crate::daemons::state::StableDirectory::open(ref_parent)?;
    if ref_directory.path_exists(&path)? || ref_directory.path_exists(&expected_anchor)? {
        return Ok(false);
    }
    for (owned_path, prefix, label) in [
        (&path, ".nib-owned-ref-delete-", "branch ref"),
        (
            &expected_anchor,
            ".nib-owned-ref-anchor-delete-",
            "branch generation anchor",
        ),
    ] {
        let quarantine =
            ref_directory.deterministic_artifact_path(owned_path, prefix, ".quarantine")?;
        if ref_directory.path_exists(&quarantine)? {
            return Err(format!(
                "managed {label} has a persisted deletion quarantine requiring exact recovery: {}",
                quarantine.display()
            ));
        }
    }
    if let Some(previous) = &record.previous_branch_anchor {
        if ref_directory.path_exists(&previous.path)? {
            return Ok(false);
        }
        let quarantine = ref_directory.deterministic_artifact_path(
            &previous.path,
            ".nib-owned-ref-retire-",
            ".quarantine",
        )?;
        if ref_directory.path_exists(&quarantine)? {
            return Err(format!(
                "managed previous branch anchor has a persisted retirement quarantine requiring exact recovery: {}",
                quarantine.display()
            ));
        }
    }
    Ok(packed_ref_namespace_conflict(&common, &record.branch_reference)?.is_none())
}

fn reconcile_previous_branch_anchor(revision: &mut DurableOwnershipRevision) -> Result<(), String> {
    let Some(previous) = revision.record.previous_branch_anchor.clone() else {
        return Ok(());
    };
    let parent = previous
        .path
        .parent()
        .ok_or("previous managed branch anchor has no parent directory")?;
    let directory = crate::daemons::state::StableDirectory::open(parent)?;
    if directory.path_exists(&previous.path)? {
        let file = directory.open_read(&previous.path)?;
        let identity = crate::fs_security::file_identity_snapshot(&file)
            .map_err(|error| format!("failed to inspect previous branch anchor: {error}"))?;
        if identity != previous.identity {
            return Err(
                "previous managed branch anchor identity changed; replacement preserved"
                    .to_string(),
            );
        }
        let contents = format!("{}\n", previous.oid).into_bytes();
        verify_open_file_contents(&file, &contents)?;
        remove_owned_file_receipt(
            &directory,
            &previous.path,
            &file,
            &contents,
            ".nib-owned-ref-retire-",
        )?;
    } else {
        let quarantine = directory.deterministic_artifact_path(
            &previous.path,
            ".nib-owned-ref-retire-",
            ".quarantine",
        )?;
        if directory.path_exists(&quarantine)? {
            return Err(format!(
                "previous managed branch anchor has a persisted retirement quarantine requiring exact recovery: {}",
                quarantine.display()
            ));
        }
    }
    let mut record = revision.record.clone();
    record.previous_branch_anchor = None;
    persist_durable_ownership_revision(revision, record)
}

fn persist_durable_branch_removed(revision: &mut DurableOwnershipRevision) -> Result<(), String> {
    let mut record = revision.record.clone();
    record.branch_cleanup = DurableArtifactPhase::Removed;
    record.phase = if record.phase == DurableOwnershipPhase::Intent {
        if record.path_cleanup == DurableArtifactPhase::Removed
            && record.registration_cleanup == DurableArtifactPhase::Removed
        {
            DurableOwnershipPhase::Complete
        } else {
            DurableOwnershipPhase::Intent
        }
    } else if record.path_cleanup == DurableArtifactPhase::Removed
        && record.registration_cleanup == DurableArtifactPhase::Removed
        && record.previous_branch_anchor.is_none()
    {
        DurableOwnershipPhase::Complete
    } else {
        DurableOwnershipPhase::Cleanup
    };
    persist_durable_ownership_revision(revision, record)
}

fn with_owned_ref_namespace_locks<T>(
    common_directory: &crate::daemons::state::StableDirectory,
    ref_directory: &crate::daemons::state::StableDirectory,
    ref_path: &Path,
    reference: &str,
    lock_owner: Option<&str>,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let mut lock_name = ref_path
        .file_name()
        .ok_or("owned branch receipt has no filename")?
        .to_os_string();
    lock_name.push(".lock");
    let mut packed_lock = acquire_owned_ref_protocol_lock(
        common_directory,
        common_directory.path().join("packed-refs.lock"),
        lock_owner,
        reference,
        "packed",
    )?;
    let mut target_lock = match acquire_owned_ref_protocol_lock(
        ref_directory,
        ref_directory.path().join(lock_name),
        lock_owner,
        reference,
        "target",
    ) {
        Ok(lock) => lock,
        Err(error) => {
            return Err(match packed_lock.release() {
                Ok(()) => error,
                Err(cleanup) => {
                    format!("{error}; exact packed-ref lock cleanup failed: {cleanup}")
                }
            });
        }
    };
    let result = operation();
    let target_cleanup = target_lock.release();
    let packed_cleanup = packed_lock.release();
    match (result, target_cleanup, packed_cleanup) {
        (Ok(value), Ok(()), Ok(())) => Ok(value),
        (Err(error), Ok(()), Ok(())) => Err(error),
        (result, target, packed) => {
            let mut errors = Vec::new();
            if let Err(error) = result {
                errors.push(error);
            }
            if let Err(error) = target {
                errors.push(format!("target ref lock cleanup failed: {error}"));
            }
            if let Err(error) = packed {
                errors.push(format!("packed ref lock cleanup failed: {error}"));
            }
            Err(errors.join("; "))
        }
    }
}

fn reconcile_removing_branch_from_anchor(
    revision: &mut DurableOwnershipRevision,
    retained_anchor: Option<&std::fs::File>,
) -> Result<(), String> {
    if revision.record.branch_cleanup != DurableArtifactPhase::Removing {
        return Ok(());
    }
    let record = revision.record.clone();
    let (ref_path, anchor_path) = managed_branch_paths(
        &record.common_git_dir,
        &record.branch_reference,
        &record.receipt_id,
        record.branch_anchor_generation,
    )?;
    let parent = ref_path
        .parent()
        .ok_or("managed branch ref has no parent directory")?;
    let common = reopen_common_git_directory(&record)?;
    let directory = crate::daemons::state::StableDirectory::open(parent)?;
    let removed = with_owned_ref_namespace_locks(
        &common,
        &directory,
        &ref_path,
        &record.branch_reference,
        Some(&record.receipt_id),
        || {
            if directory.path_exists(&ref_path)? {
                return Ok(false);
            }
            if let Some(conflict) =
                packed_ref_namespace_conflict(&common, &record.branch_reference)?
            {
                return Err(format!(
                "managed branch cleanup encountered packed ref {conflict}; retained anchor was preserved"
            ));
            }
            let contents = format!("{}\n", record.current_oid).into_bytes();
            let delete_quarantine = directory.deterministic_artifact_path(
                &ref_path,
                ".nib-owned-ref-delete-",
                ".quarantine",
            )?;
            if directory.path_exists(&delete_quarantine)? {
                return Err(format!(
                "managed branch ref has a persisted deletion quarantine; its retained anchor and quarantine were preserved for exact physical recovery: {}",
                delete_quarantine.display()
            ));
            }
            if directory.path_exists(&anchor_path)? {
                let reopened_anchor = retained_anchor
                    .is_none()
                    .then(|| directory.open_read(&anchor_path))
                    .transpose()?;
                let anchor = retained_anchor
                    .or(reopened_anchor.as_ref())
                    .expect("anchor path was present");
                let identity = crate::fs_security::file_identity_snapshot(anchor)
                    .map_err(|error| format!("failed to inspect managed branch anchor: {error}"))?;
                if Some(identity) != record.branch_identity {
                    return Err(
                    "managed branch anchor no longer matches its durable identity; replacement preserved"
                        .to_string(),
                );
                }
                verify_open_file_contents(anchor, &contents)?;
                remove_owned_file_receipt(
                    &directory,
                    &anchor_path,
                    anchor,
                    &contents,
                    ".nib-owned-ref-anchor-delete-",
                )?;
            }
            let anchor_quarantine = directory.deterministic_artifact_path(
                &anchor_path,
                ".nib-owned-ref-anchor-delete-",
                ".quarantine",
            )?;
            if directory.path_exists(&anchor_quarantine)? {
                return Err(format!(
                "managed branch anchor has a persisted deletion quarantine requiring exact recovery: {}",
                anchor_quarantine.display()
            ));
            }
            Ok(true)
        },
    )?;
    if removed {
        persist_durable_branch_removed(revision)
    } else {
        Ok(())
    }
}

fn persist_intent_cleanup_phase(
    revision: &mut DurableOwnershipRevision,
    artifact: CleanupArtifact,
    phase: DurableArtifactPhase,
) -> Result<(), String> {
    let mut record = revision.record.clone();
    match artifact {
        CleanupArtifact::Path => record.path_cleanup = phase,
        CleanupArtifact::Registration => {
            if phase != DurableArtifactPhase::Removed {
                return Err(
                    "intent registration can only advance after bounded absence proof".into(),
                );
            }
            record.registration_cleanup = phase;
        }
        CleanupArtifact::Branch => record.branch_cleanup = phase,
    }
    record.phase = if record.path_cleanup == DurableArtifactPhase::Removed
        && record.registration_cleanup == DurableArtifactPhase::Removed
        && record.branch_cleanup == DurableArtifactPhase::Removed
    {
        DurableOwnershipPhase::Complete
    } else {
        DurableOwnershipPhase::Intent
    };
    persist_durable_ownership_revision(revision, record)
}

fn intent_directory_quarantine_exists(parent: &Path, path: &Path) -> Result<bool, String> {
    crate::fs_security::directory_removal_quarantine_exists(parent, path)
        .map_err(|error| format!("failed to inspect durable intent cleanup quarantine: {error}"))
}

fn reconcile_unfinished_intent_path(revision: &mut DurableOwnershipRevision) -> Result<(), String> {
    let phase = revision.record.path_cleanup;
    if phase == DurableArtifactPhase::Removed {
        return Ok(());
    }
    if phase == DurableArtifactPhase::Unattributed {
        return Err("managed worktree intent path has no durable attribution".to_string());
    }
    let final_path = revision.record.worktree_path.clone();
    let staging_path = revision.record.worktree_staging_path.clone();
    let parent = final_path
        .parent()
        .ok_or("managed worktree intent path has no parent")?
        .to_path_buf();
    let final_exists = crate::fs_security::path_entry_exists(&final_path)
        .map_err(|error| format!("failed to inspect durable intent worktree path: {error}"))?;
    let staging_exists = crate::fs_security::path_entry_exists(&staging_path)
        .map_err(|error| format!("failed to inspect durable intent worktree staging: {error}"))?;
    if final_exists && staging_exists {
        return Err(format!(
            "managed worktree intent has both final and staged directories; both were preserved: {} and {}",
            final_path.display(),
            staging_path.display()
        ));
    }
    if phase == DurableArtifactPhase::Reserved && final_exists {
        return Err(format!(
            "managed worktree destination appeared before its reserved staging identity was committed and was preserved: {}",
            final_path.display()
        ));
    }
    if !final_exists && !staging_exists {
        if intent_directory_quarantine_exists(&parent, &final_path)?
            || intent_directory_quarantine_exists(&parent, &staging_path)?
        {
            return Err(
                "managed worktree intent has a persisted directory quarantine requiring exact physical recovery"
                    .to_string(),
            );
        }
        return persist_intent_cleanup_phase(
            revision,
            CleanupArtifact::Path,
            DurableArtifactPhase::Removed,
        );
    }
    let owned_path = if staging_exists {
        staging_path
    } else {
        final_path
    };
    let receipt = if phase == DurableArtifactPhase::Reserved {
        let receipt =
            crate::fs_security::capture_directory_removal_receipt(&parent, &owned_path)
                .map_err(|error| format!("failed to capture reserved worktree staging: {error}"))?;
        let mut record = revision.record.clone();
        record.worktree_identity = Some(receipt.identity());
        record.path_cleanup = DurableArtifactPhase::Removing;
        persist_durable_ownership_revision(revision, record)?;
        receipt
    } else {
        reopen_directory_receipt(
            &parent,
            &owned_path,
            revision
                .record
                .worktree_identity
                .ok_or("managed worktree intent path has no durable identity")?,
            "managed worktree intent path",
        )?
    };
    if revision.record.path_cleanup != DurableArtifactPhase::Removing {
        persist_intent_cleanup_phase(
            revision,
            CleanupArtifact::Path,
            DurableArtifactPhase::Removing,
        )?;
    }
    if owned_path == revision.record.worktree_staging_path {
        let parent_directory = crate::daemons::state::StableDirectory::open(&parent)?;
        let staged_directory = parent_directory.open_owned_child(&owned_path)?;
        if staged_directory.directory_removal_receipt()?.identity() != receipt.identity() {
            return Err(
                "reserved worktree staging identity changed; replacement preserved".to_string(),
            );
        }
        parent_directory.remove_empty_child_directory_if_matches(&owned_path, staged_directory)?;
    } else {
        crate::fs_security::remove_directory_tree_capability_bound_if_matches(
            &parent,
            &owned_path,
            receipt,
            Instant::now() + GIT_COMMAND_TIMEOUT,
        )
        .map_err(|error| format!("failed to remove durable intent worktree path: {error}"))?;
    }
    persist_intent_cleanup_phase(
        revision,
        CleanupArtifact::Path,
        DurableArtifactPhase::Removed,
    )
}

fn reconcile_unfinished_intent_branch(
    revision: &mut DurableOwnershipRevision,
) -> Result<(), String> {
    if revision.record.branch_cleanup == DurableArtifactPhase::Removed {
        return Ok(());
    }
    if revision.record.branch_cleanup == DurableArtifactPhase::Unattributed {
        return Err("managed worktree intent branch has no durable attribution".to_string());
    }
    if revision.record.branch_cleanup == DurableArtifactPhase::Removing {
        reconcile_removing_branch_from_anchor(revision, None)?;
        if revision.record.branch_cleanup == DurableArtifactPhase::Removed {
            return Ok(());
        }
    }
    let record = revision.record.clone();
    let (ref_path, anchor_path) = managed_branch_paths(
        &record.common_git_dir,
        &record.branch_reference,
        &record.receipt_id,
        record.branch_anchor_generation,
    )?;
    let parent = ref_path
        .parent()
        .ok_or("managed worktree branch has no parent directory")?;
    crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| format!("managed worktree ref directory is unsafe: {error}"))?;
    let directory = crate::daemons::state::StableDirectory::open(parent)?;
    let ref_exists = directory.path_exists(&ref_path)?;
    let anchor_exists = directory.path_exists(&anchor_path)?;
    if record.branch_cleanup == DurableArtifactPhase::Reserved && ref_exists {
        return Err(format!(
            "managed branch appeared before its reserved anchor identity was committed and was preserved: {}",
            record.branch_reference
        ));
    }
    if record.branch_cleanup == DurableArtifactPhase::Reserved && !anchor_exists {
        let quarantine = directory.deterministic_artifact_path(
            &anchor_path,
            ".nib-reserved-ref-delete-",
            ".quarantine",
        )?;
        if directory.path_exists(&quarantine)? {
            return Err(format!(
                "reserved branch staging has a persisted deletion quarantine requiring exact recovery: {}",
                quarantine.display()
            ));
        }
        let common = reopen_common_git_directory(&record)?;
        with_owned_ref_namespace_locks(
            &common,
            &directory,
            &ref_path,
            &record.branch_reference,
            Some(&record.receipt_id),
            || {
                if directory.path_exists(&ref_path)? || directory.path_exists(&anchor_path)? {
                    return Err(
                        "managed branch reservation changed while its namespace locks were held; preserving it"
                            .to_string(),
                    );
                }
                if let Some(conflict) =
                    packed_ref_namespace_conflict(&common, &record.branch_reference)?
                {
                    return Err(format!(
                        "managed branch reservation encountered packed ref {conflict}; preserving it"
                    ));
                }
                Ok(())
            },
        )?;
        return persist_durable_branch_removed(revision);
    }
    if !ref_exists && !anchor_exists {
        let mut removing = revision.record.clone();
        removing.branch_cleanup = DurableArtifactPhase::Removing;
        persist_durable_ownership_revision(revision, removing)?;
        return reconcile_removing_branch_from_anchor(revision, None);
    }
    if record.branch_cleanup == DurableArtifactPhase::Reserved {
        let anchor = directory.open_read_write(&anchor_path)?;
        let contents = format!("{}\n", record.current_oid).into_bytes();
        match anchor.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(
                    "reserved branch staging is still owned by a live publisher and was preserved"
                        .to_string(),
                );
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to inspect reserved branch staging ownership; it was preserved: {error}"
                ));
            }
        }
        verify_open_file_contents(&anchor, &contents)?;
        let identity = crate::fs_security::file_identity_snapshot(&anchor)
            .map_err(|error| format!("failed to capture reserved branch anchor: {error}"))?;
        let mut removing = revision.record.clone();
        removing.branch_identity = Some(identity);
        removing.branch_cleanup = DurableArtifactPhase::Removing;
        persist_durable_ownership_revision(revision, removing)?;
        // The retained lock bridges the liveness check and durable identity CAS to
        // exact cleanup. On Windows, reopening this byte-locked file cannot read it.
        return reconcile_removing_branch_from_anchor(revision, Some(&anchor));
    }
    if !anchor_exists {
        return Err(
            "managed branch final ref exists without its retained generation anchor; ref preserved"
                .to_string(),
        );
    }
    if !ref_exists {
        let mut removing = revision.record.clone();
        removing.branch_cleanup = DurableArtifactPhase::Removing;
        persist_durable_ownership_revision(revision, removing)?;
        return reconcile_removing_branch_from_anchor(revision, None);
    }
    let owned_branch = reopen_owned_branch(&record, &record.current_oid, false)?;
    persist_intent_cleanup_phase(
        revision,
        CleanupArtifact::Branch,
        DurableArtifactPhase::Removing,
    )?;
    match delete_owned_branch_sync_with_timeout(
        &record.project_root,
        &owned_branch,
        GIT_COMMAND_TIMEOUT,
    ) {
        Ok(()) => persist_durable_branch_removed(revision),
        Err(error) => match reconcile_removing_branch_from_anchor(revision, None) {
            Ok(()) if revision.record.branch_cleanup == DurableArtifactPhase::Removed => Ok(()),
            Ok(()) => Err(error),
            Err(recovery) => Err(format!("{error}; exact branch recovery failed: {recovery}")),
        },
    }
}

fn prove_no_post_snapshot_registration(
    record: &DurableManagedWorktreeOwnership,
) -> Result<(), String> {
    if !record.registration_snapshot_captured {
        return Err("managed worktree intent has no pre-add registration snapshot".to_string());
    }
    let common = reopen_common_git_directory(record)?;
    let registrations_path = record.common_git_dir.join("worktrees");
    if common.entry_kind(&registrations_path)?.is_none() {
        return Ok(());
    }
    let registrations = common.open_child(&registrations_path)?;
    if let Some(expected) = record.registration_namespace_identity {
        if registrations.directory_removal_receipt()?.identity() != expected {
            return Err(
                "Git worktree registration namespace changed after the durable intent; registrations were preserved"
                    .to_string(),
            );
        }
    }
    let mut unexpected = None;
    registrations.for_each_entry_bounded(
        MAX_WORKTREE_REGISTRATIONS,
        MAX_WORKTREE_REGISTRATION_NAME_BYTES,
        |name| {
            let path = registrations_path.join(&name);
            if registrations.entry_kind(&path)?
                != Some(crate::daemons::state::StableEntryKind::Directory)
            {
                return Err(format!(
                    "Git worktree registration entry is not a directory and was preserved: {}",
                    path.display()
                ));
            }
            let directory = registrations.open_child(&path)?;
            let identity = directory.directory_removal_receipt()?.identity();
            let known_name = record
                .preexisting_registration_name_hashes
                .binary_search(&encoded_name_hash(&name))
                .is_ok();
            let known_identity = record
                .preexisting_registration_identities
                .contains(&identity);
            if !known_name || !known_identity {
                unexpected = Some(path);
            }
            Ok(())
        },
    )?;
    if let Some(path) = unexpected {
        return Err(format!(
            "post-snapshot Git worktree registration lacks exact creation attribution and was preserved: {}",
            path.display()
        ));
    }
    Ok(())
}

fn reconcile_unfinished_intent(
    mut revision: DurableOwnershipRevision,
    promotion_error: &str,
) -> Result<(), String> {
    if revision.record.phase != DurableOwnershipPhase::Intent {
        return Err("managed worktree intent changed before recovery".to_string());
    }
    recover_owned_ref_restart_artifacts(&revision.record).map_err(|error| {
        format!(
            "managed worktree intent could not be promoted ({promotion_error}); durable ref-lock recovery was incomplete: {error}"
        )
    })?;
    let mut errors = Vec::new();
    if let Err(error) = reconcile_unfinished_intent_path(&mut revision) {
        errors.push(error);
    }
    if let Err(error) = reconcile_unfinished_intent_branch(&mut revision) {
        errors.push(error);
    }
    if revision.record.registration_cleanup != DurableArtifactPhase::Removed {
        match prove_no_post_snapshot_registration(&revision.record) {
            Ok(()) => {
                if let Err(error) = persist_intent_cleanup_phase(
                    &mut revision,
                    CleanupArtifact::Registration,
                    DurableArtifactPhase::Removed,
                ) {
                    errors.push(error);
                }
            }
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() && revision.record.phase == DurableOwnershipPhase::Complete {
        Ok(())
    } else {
        let recovery = if errors.is_empty() {
            "durable intent recovery did not reach a complete tombstone".to_string()
        } else {
            errors.join("; ")
        };
        Err(format!(
            "managed worktree intent could not be promoted ({promotion_error}); exact recovery was incomplete: {recovery}"
        ))
    }
}

fn rehydrate_owned_worktree(
    mut revision: DurableOwnershipRevision,
    adopted_oid: Option<&str>,
) -> Result<ManagedWorktreeReceipt, String> {
    recover_owned_ref_restart_artifacts(&revision.record)?;
    if revision.record.phase == DurableOwnershipPhase::Intent {
        let project_root = revision.record.project_root.clone();
        let kind = revision.record.kind;
        let logical_id = revision.record.logical_id.clone();
        let ownership = match promote_durable_intent(revision) {
            Ok(ownership) => ownership,
            Err(promotion_error) => {
                let recovered = load_durable_ownership_revision(&project_root, kind, &logical_id)?
                    .ok_or("managed worktree intent disappeared during recovery")?;
                if recovered.record.phase != DurableOwnershipPhase::Intent {
                    return rehydrate_owned_worktree(recovered, adopted_oid);
                }
                reconcile_unfinished_intent(recovered, &promotion_error)?;
                return Err("managed worktree intent cleanup is already complete".to_string());
            }
        };
        if let Some(adopted_oid) = adopted_oid {
            let logical_id = {
                let state = ownership
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state
                    .durable
                    .as_ref()
                    .expect("promoted ownership has durable state")
                    .record
                    .logical_id
                    .clone()
            };
            adopt_managed_worktree_branch(&ownership, &logical_id, adopted_oid)?;
        }
        return Ok(ownership);
    }
    if revision.record.phase == DurableOwnershipPhase::Complete {
        return Err("managed worktree ownership is already complete".to_string());
    }
    reconcile_previous_branch_anchor(&mut revision)?;
    reconcile_removing_branch_from_anchor(&mut revision, None)?;
    if revision.record.phase == DurableOwnershipPhase::Complete {
        return Err("managed worktree ownership cleanup is already complete".to_string());
    }
    let mut record = revision.record.clone();
    let (path_receipt, path_removed) = reopen_durable_directory_artifact(
        record
            .worktree_path
            .parent()
            .ok_or("managed worktree path has no parent")?,
        &record.worktree_path,
        record
            .worktree_identity
            .ok_or("durable managed worktree has no path identity")?,
        record.path_cleanup,
        "managed worktree path",
    )?;
    let registration_path = record
        .registration_path
        .clone()
        .ok_or("durable managed worktree has no attributed Git registration")?;
    let registration_identity = record
        .registration_identity
        .ok_or("durable managed worktree has no registration identity")?;
    let registration_parent = registration_path
        .parent()
        .ok_or("managed worktree registration has no parent")?;
    let registration_namespace =
        reopen_common_git_directory(&record)?.open_child(registration_parent)?;
    if registration_namespace
        .directory_removal_receipt()?
        .identity()
        != record
            .registration_namespace_identity
            .ok_or("durable managed worktree has no registration namespace identity")?
    {
        return Err("managed worktree registration namespace identity changed".to_string());
    }
    drop(registration_namespace);
    let (registration_receipt, registration_removed) = reopen_durable_directory_artifact(
        registration_parent,
        &registration_path,
        registration_identity,
        record.registration_cleanup,
        "Git worktree registration",
    )?;
    if !path_removed && !registration_removed {
        validate_reciprocal_worktree_link(
            &record.worktree_path,
            path_receipt
                .as_ref()
                .ok_or("durable managed worktree path receipt is unavailable")?,
            &registration_path,
            registration_receipt
                .as_ref()
                .ok_or("durable managed worktree registration receipt is unavailable")?,
        )?;
    }
    let expected_oid = adopted_oid.unwrap_or(&record.current_oid);
    let adopting_new_revision = adopted_oid.is_some()
        && expected_oid != record.current_oid
        && !path_removed
        && !registration_removed;
    if adopted_oid.is_some()
        && (path_removed || registration_removed)
        && expected_oid != record.current_oid
    {
        return Err(
            "cannot adopt a different durable branch revision after cleanup has started"
                .to_string(),
        );
    }
    let branch_removed = record.branch_cleanup == DurableArtifactPhase::Removed
        || (record.branch_cleanup == DurableArtifactPhase::Removing
            && durable_branch_is_absent(&record)?);
    if path_removed && record.path_cleanup != DurableArtifactPhase::Removed {
        record.path_cleanup = DurableArtifactPhase::Removed;
    }
    if registration_removed && record.registration_cleanup != DurableArtifactPhase::Removed {
        record.registration_cleanup = DurableArtifactPhase::Removed;
    }
    if branch_removed && record.branch_cleanup != DurableArtifactPhase::Removed {
        record.branch_cleanup = DurableArtifactPhase::Removed;
    }
    let inferred_cleanup = record.path_cleanup != revision.record.path_cleanup
        || record.registration_cleanup != revision.record.registration_cleanup
        || record.branch_cleanup != revision.record.branch_cleanup;
    if record.path_cleanup == DurableArtifactPhase::Removed
        && record.registration_cleanup == DurableArtifactPhase::Removed
        && record.branch_cleanup == DurableArtifactPhase::Removed
    {
        record.phase = DurableOwnershipPhase::Complete;
    }
    if inferred_cleanup {
        persist_durable_ownership_revision(&mut revision, record.clone())?;
    }
    if record.phase == DurableOwnershipPhase::Complete {
        return Err("managed worktree ownership cleanup is already complete".to_string());
    }
    let owned_branch = if branch_removed {
        None
    } else {
        Some(reopen_owned_branch(
            &record,
            expected_oid,
            adopting_new_revision,
        )?)
    };
    if adopting_new_revision {
        validate_owned_worktree_sync(
            &record.worktree_path,
            owned_branch
                .as_ref()
                .ok_or("managed worktree branch ownership receipt is unavailable")?,
        )?;
    }
    let ownership = ManagedWorktreeReceipt {
        path: record.worktree_path.clone(),
        path_receipt,
        registration_path,
        registration_receipt,
        state: std::sync::Mutex::new(ManagedWorktreeState {
            owned_branch,
            path_removed,
            registration_removed,
            branch_removed,
            reciprocal_link_proven: !path_removed && !registration_removed,
            durable: Some(revision),
        }),
    };
    if adopting_new_revision {
        let adopted_oid = adopted_oid.expect("new-revision adoption has an object ID");
        persist_adopted_branch_revision(&ownership, adopted_oid)?;
    }
    Ok(ownership)
}

fn persist_adopted_branch_revision(
    ownership: &ManagedWorktreeReceipt,
    expected_oid: &str,
) -> Result<(), String> {
    let mut state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let owned_branch = state
        .owned_branch
        .as_ref()
        .cloned()
        .ok_or("managed worktree branch ownership receipt is unavailable")?;
    let branch_identity = crate::fs_security::file_identity_snapshot(&owned_branch.receipt.file)
        .map_err(|error| format!("failed to retain adopted branch identity: {error}"))?;
    let durable = state
        .durable
        .as_mut()
        .ok_or("managed worktree has no durable generational receipt")?;
    let mut record = durable.record.clone();
    let previous_anchor_path = record.branch_staging_path.clone();
    let previous_anchor_file = owned_branch
        .receipt
        .directory
        .open_read(&previous_anchor_path)?;
    verify_open_file_contents(
        &previous_anchor_file,
        format!("{}\n", record.current_oid).as_bytes(),
    )?;
    let previous_anchor_identity =
        crate::fs_security::file_identity_snapshot(&previous_anchor_file).map_err(|error| {
            format!("failed to retain previous branch anchor identity: {error}")
        })?;
    if Some(previous_anchor_identity) != record.branch_identity {
        return Err(
            "previous branch anchor no longer matches its durable generation; replacement preserved"
                .to_string(),
        );
    }
    let next_generation = record
        .branch_anchor_generation
        .checked_add(1)
        .ok_or("managed branch anchor generation overflowed")?;
    let next_anchor_path = owned_branch
        .receipt
        .anchor_path
        .clone()
        .ok_or("adopted branch generation anchor path is unavailable")?;
    let expected_next_anchor = managed_branch_paths(
        &record.common_git_dir,
        &record.branch_reference,
        &record.receipt_id,
        next_generation,
    )?
    .1;
    if next_anchor_path != expected_next_anchor {
        return Err("adopted branch generation anchor path is invalid".to_string());
    }
    let previous_oid = record.current_oid.clone();
    record.current_oid = expected_oid.to_string();
    record.branch_identity = Some(branch_identity);
    record.previous_branch_anchor = Some(DurablePreviousBranchAnchor {
        path: previous_anchor_path.clone(),
        identity: previous_anchor_identity,
        oid: previous_oid,
    });
    record.branch_anchor_generation = next_generation;
    record.branch_staging_path = next_anchor_path;
    persist_durable_ownership_revision(durable, record)?;
    remove_owned_file_receipt(
        &owned_branch.receipt.directory,
        &previous_anchor_path,
        &previous_anchor_file,
        format!(
            "{}\n",
            durable
                .record
                .previous_branch_anchor
                .as_ref()
                .expect("previous anchor persisted")
                .oid
        )
        .as_bytes(),
        ".nib-owned-ref-retire-",
    )?;
    let mut record = durable.record.clone();
    record.previous_branch_anchor = None;
    persist_durable_ownership_revision(durable, record)
}

pub(crate) fn load_managed_worktree_ownership(
    project_root: &Path,
    kind: ManagedWorktreeKind,
    logical_id: &str,
) -> Result<Option<Arc<ManagedWorktreeReceipt>>, String> {
    let Some(revision) = load_durable_ownership_revision(project_root, kind, logical_id)? else {
        return Ok(None);
    };
    if revision.record.phase == DurableOwnershipPhase::Complete {
        recover_owned_ref_restart_artifacts(&revision.record)?;
        return Ok(None);
    }
    match rehydrate_owned_worktree(revision, None) {
        Ok(ownership) => Ok(Some(Arc::new(ownership))),
        Err(error) => {
            let completed = load_durable_ownership_revision(project_root, kind, logical_id)?
                .is_some_and(|revision| revision.record.phase == DurableOwnershipPhase::Complete);
            if completed {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

pub(crate) fn managed_worktree_owned_path(ownership: &ManagedWorktreeReceipt) -> PathBuf {
    ownership.path.clone()
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub id: String,
    pub path: PathBuf,
    pub branch: String,
    pub branch_oid: String,
    project_root: PathBuf,
    pub(crate) ownership_receipt_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct WorktreePreparationAuthority {
    id: String,
    path: PathBuf,
    branch: String,
    branch_oid: String,
    pub(crate) ownership_receipt_id: String,
}

impl Worktree {
    pub(crate) fn plan_preparation_authority(
        project_root: &Path,
        id: &str,
    ) -> Result<WorktreePreparationAuthority, String> {
        let project_root = repository_root_bounded_sync(project_root)?;
        let safe_id = sanitize_component(id);
        let branch = branch_name(&safe_id);
        let head = run_git_bounded_sync(&project_root, ["rev-parse", "--verify", "HEAD^{commit}"])?;
        let branch_oid = parse_git_oid(&head, "plan subagent worktree base")?;
        Ok(WorktreePreparationAuthority {
            id: safe_id.clone(),
            path: project_root
                .join(".nib")
                .join("worktrees")
                .join("subagents")
                .join(safe_id),
            branch,
            branch_oid,
            ownership_receipt_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub(crate) async fn plan_preparation_authority_cancellable(
        project_root: &Path,
        id: &str,
        cancellation: Option<&crate::agent::CancellationSignal>,
    ) -> Result<WorktreePreparationAuthority, String> {
        let project_root = repository_root_bounded_cancellable(project_root, cancellation).await?;
        let safe_id = sanitize_component(id);
        let branch = branch_name(&safe_id);
        let head = run_git_cancellable(
            &project_root,
            ["rev-parse", "--verify", "HEAD^{commit}"],
            cancellation,
        )
        .await?;
        let branch_oid = parse_git_oid(&head, "plan subagent worktree base")?;
        Ok(WorktreePreparationAuthority {
            id: safe_id.clone(),
            path: project_root
                .join(".nib")
                .join("worktrees")
                .join("subagents")
                .join(safe_id),
            branch,
            branch_oid,
            ownership_receipt_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub(crate) fn preparation_authority_matches(
        authority: &WorktreePreparationAuthority,
        id: &str,
        path: &Path,
        branch: &str,
        branch_oid: Option<&str>,
        receipt_id: Option<&str>,
    ) -> bool {
        authority.id == id
            && authority.path == path
            && authority.branch == branch
            && branch_oid == Some(authority.branch_oid.as_str())
            && receipt_id == Some(authority.ownership_receipt_id.as_str())
    }

    pub(crate) fn preparation_authority(&self) -> WorktreePreparationAuthority {
        WorktreePreparationAuthority {
            id: self.id.clone(),
            path: self.path.clone(),
            branch: self.branch.clone(),
            branch_oid: self.branch_oid.clone(),
            ownership_receipt_id: self.ownership_receipt_id.clone(),
        }
    }

    pub(crate) fn cleanup_preparation_authority_with_guard(
        project_root: &Path,
        authority: &WorktreePreparationAuthority,
        timeout: Duration,
        external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "worktree preparation cleanup deadline overflow".to_string())?;
        Self::cleanup_preparation_authority_until_with_guard(
            project_root,
            authority,
            deadline,
            timeout,
            external_guard,
        )
    }

    pub(crate) fn cleanup_preparation_authority_until_with_guard(
        project_root: &Path,
        authority: &WorktreePreparationAuthority,
        deadline: Instant,
        timeout: Duration,
        mut external_guard: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        external_guard()?;
        let requested = canonical_requested_root(project_root)?;
        let inspect = run_git_bounded_sync_with_timeout(
            &requested,
            ["rev-parse", "--show-toplevel"],
            cleanup_time_remaining(deadline, timeout)?,
        )?;
        require_git_success(&inspect, "inspect repository for bounded worktree cleanup")?;
        let project_root = validate_reported_repository_root(&requested, &inspect.stdout)?;
        external_guard()?;
        let safe_id = sanitize_component(&authority.id);
        if let Some(revision) =
            load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, &safe_id)?
        {
            if revision.record.receipt_id != authority.ownership_receipt_id
                || revision.record.worktree_path != authority.path
            {
                return Err(
                    "persisted preparation worktree authority changed; replacement preserved"
                        .to_string(),
                );
            }
            if revision.record.phase == DurableOwnershipPhase::Complete {
                return external_guard();
            }
            if revision.record.phase == DurableOwnershipPhase::Intent {
                external_guard()?;
                if load_managed_worktree_ownership(
                    &project_root,
                    ManagedWorktreeKind::Subagent,
                    &safe_id,
                )?
                .is_none()
                {
                    let completed = load_durable_ownership_revision(
                        &project_root,
                        ManagedWorktreeKind::Subagent,
                        &safe_id,
                    )?
                    .ok_or("prepared worktree ownership disappeared during exact recovery")?;
                    if completed.record.receipt_id != authority.ownership_receipt_id
                        || completed.record.worktree_path != authority.path
                        || completed.record.phase != DurableOwnershipPhase::Complete
                    {
                        return Err(
                            "prepared worktree recovery did not retain an exact complete ownership proof"
                                .to_string(),
                        );
                    }
                    return external_guard();
                }
                external_guard()?;
            }
        } else {
            return prove_managed_worktree_namespace_absent_until(
                &project_root,
                &safe_id,
                deadline,
                timeout,
            )
            .and_then(|()| external_guard());
        }
        let worktree = Worktree {
            id: authority.id.clone(),
            path: authority.path.clone(),
            branch: authority.branch.clone(),
            branch_oid: authority.branch_oid.clone(),
            project_root: project_root.clone(),
            ownership_receipt_id: authority.ownership_receipt_id.clone(),
        };
        remove_registered_worktree_precommit_until_with_guard(
            &project_root,
            &worktree,
            deadline,
            timeout,
            &mut external_guard,
        )?;
        external_guard()
    }

    pub fn create(project_root: &Path, id: &str) -> Result<Self, String> {
        let authority = Self::plan_preparation_authority(project_root, id)?;
        Self::create_from_preparation_authority(project_root, &authority)
    }

    pub(crate) fn create_from_preparation_authority(
        project_root: &Path,
        authority: &WorktreePreparationAuthority,
    ) -> Result<Self, String> {
        Self::create_from_preparation_authority_with_guard(
            project_root,
            authority,
            Arc::new(|| Ok(())),
        )
    }

    pub(crate) fn create_from_preparation_authority_with_guard(
        project_root: &Path,
        authority: &WorktreePreparationAuthority,
        external_guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Result<Self, String> {
        external_guard()?;
        let project_root = repository_root(project_root)?;
        external_guard()?;
        let safe_id = sanitize_component(&authority.id);
        if WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&(project_root.clone(), safe_id.clone()))
        {
            return Err(format!(
                "worktree {safe_id} still has an active ownership receipt; cleanup is required before reuse"
            ));
        }
        if let Some(ownership) =
            load_managed_worktree_ownership(&project_root, ManagedWorktreeKind::Subagent, &safe_id)?
        {
            WORKTREE_OWNERSHIP
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((project_root.clone(), safe_id.clone()), ownership);
            return Err(format!(
                "worktree {safe_id} still has an active durable ownership receipt; cleanup is required before reuse"
            ));
        }
        let worktree_path = authority.path.clone();
        let expected_path = project_root
            .join(".nib")
            .join("worktrees")
            .join("subagents")
            .join(&safe_id);
        if worktree_path != expected_path || authority.id != safe_id {
            return Err(
                "planned subagent worktree authority does not match the repository".to_string(),
            );
        }
        let parent = worktree_path
            .parent()
            .ok_or("subagent worktree has no parent directory")?;
        let root_directory = crate::daemons::state::StableDirectory::open(&project_root)?;
        let canonical_parent_directory = root_directory
            .open_or_create_descendant_directory_with_guard(
                parent,
                || external_guard(),
                |_| Ok(()),
            )?;
        let canonical_parent = canonical_parent_directory.path().to_path_buf();
        external_guard()?;
        if !canonical_parent.starts_with(&project_root) {
            return Err(format!(
                "subagent worktree parent escapes the repository: {}",
                parent.display()
            ));
        }
        if crate::fs_security::path_entry_exists(&worktree_path)
            .map_err(|error| format!("failed to inspect subagent worktree destination: {error}"))?
        {
            return Err(format!("worktree {} already exists", safe_id));
        }

        let branch = authority.branch.clone();
        if branch != branch_name(&safe_id) {
            return Err("planned subagent worktree branch is invalid".to_string());
        }
        external_guard()?;
        let mut reservation = reserve_managed_worktree_sync_controlled_with_receipt(
            &project_root,
            ManagedWorktreeKind::Subagent,
            &safe_id,
            &worktree_path,
            &branch,
            None,
            Some(&authority.ownership_receipt_id),
        )?;
        external_guard()?;
        if reservation.intent.revision.record.initial_oid != authority.branch_oid {
            return Err(reconcile_failed_managed_worktree_reservation(
                reservation,
                "planned subagent worktree base changed before reservation".to_string(),
            ));
        }
        macro_rules! fail_reserved_create {
            ($primary:expr) => {
                return Err(reconcile_failed_managed_worktree_reservation(
                    reservation,
                    $primary,
                ))
            };
        }
        external_guard()?;
        let owned_branch =
            match create_reserved_worktree_branch_sync_controlled(&mut reservation, None) {
                Ok(branch) => branch,
                Err(error) => fail_reserved_create!(error),
            };
        external_guard()?;
        #[cfg(test)]
        if let Some(replacement) = SYNC_BEFORE_ADD_DESTINATION_REPLACEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&safe_id)
        {
            std::fs::create_dir(&worktree_path)
                .expect("install pre-add destination replacement fixture");
            std::fs::write(
                worktree_path.join("sentinel"),
                replacement.as_os_str().as_encoded_bytes(),
            )
            .expect("write pre-add destination replacement fixture");
        }
        if let Err(error) = prove_worktree_destination_absent(&canonical_parent, &worktree_path) {
            fail_reserved_create!(compensate_failed_create_sync(
                &project_root,
                &safe_id,
                &owned_branch,
                None,
                None,
                error,
            ));
        }
        external_guard()?;
        let path_receipt = match publish_reserved_empty_worktree_destination(
            &mut reservation,
            &canonical_parent,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                fail_reserved_create!(compensate_failed_create_sync(
                    &project_root,
                    &safe_id,
                    &owned_branch,
                    None,
                    None,
                    error,
                ));
            }
        };
        external_guard()?;
        #[cfg(test)]
        if SYNC_AFTER_DESTINATION_PUBLICATION_REPLACEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&safe_id)
        {
            let displaced =
                canonical_parent.join(format!(".owned-path-{}", uuid::Uuid::new_v4().simple()));
            std::fs::rename(&worktree_path, displaced)
                .expect("displace owned pre-add worktree path");
            std::fs::create_dir(&worktree_path).expect("install failed-add path replacement");
            std::fs::write(worktree_path.join("sentinel"), b"replacement")
                .expect("write failed-add path replacement sentinel");
        }
        let registration_snapshot = reserved_worktree_registration_snapshot(&reservation);
        #[cfg(test)]
        if SYNC_AFTER_REGISTRATION_SNAPSHOT_FORGERIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&safe_id)
        {
            let registration_path = registration_snapshot
                .common_directory
                .path()
                .join("worktrees")
                .join(format!("forged-{safe_id}"));
            std::fs::create_dir_all(&registration_path)
                .expect("create post-snapshot forged registration");
            std::fs::write(
                registration_path.join("gitdir"),
                worktree_path.join(".git").as_os_str().as_encoded_bytes(),
            )
            .expect("write forged registration backlink");
            std::fs::write(registration_path.join("sentinel"), b"foreign")
                .expect("write forged registration sentinel");
            std::fs::write(
                worktree_path.join(".git"),
                format!("gitdir: {}\n", registration_path.display()),
            )
            .expect("write forged worktree pointer");
        }
        external_guard()?;
        let create = run_git_bounded_sync(
            &project_root,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                crate::fs_security::path_for_external_command(&worktree_path).into_os_string(),
                OsString::from(&branch),
            ],
        )
        .and_then(|output| {
            require_git_success(&output, "worktree add")?;
            Ok(output)
        });
        if let Err(error) = create {
            fail_reserved_create!(compensate_failed_create_sync(
                &project_root,
                &safe_id,
                &owned_branch,
                Some(&path_receipt),
                None,
                format!(
                    "{error}; post-snapshot Git worktree registrations were preserved because a failed add provides no exact creation receipt"
                ),
            ));
        }
        external_guard()?;

        let ownership = match capture_managed_worktree_receipt_sync(
            &project_root,
            &worktree_path,
            &path_receipt,
            &owned_branch,
            registration_snapshot,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                let ManagedWorktreeCaptureError { message, ownership } = error;
                fail_reserved_create!(compensate_failed_create_sync(
                    &project_root,
                    &safe_id,
                    &owned_branch,
                    Some(&path_receipt),
                    ownership.as_deref(),
                    message,
                ));
            }
        };
        external_guard()?;
        let ownership = match finish_managed_worktree_reservation(reservation, ownership) {
            Ok(ownership) => ownership,
            Err(error) => {
                let ManagedWorktreeCaptureError { message, ownership } = error;
                return Err(compensate_failed_create_sync(
                    &project_root,
                    &safe_id,
                    &owned_branch,
                    Some(&path_receipt),
                    ownership.as_deref(),
                    message,
                ));
            }
        };
        external_guard()?;
        #[cfg(test)]
        if SYNC_POST_CAPTURE_PATH_REPLACEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&safe_id)
        {
            let displaced =
                canonical_parent.join(format!(".owned-path-{}", uuid::Uuid::new_v4().simple()));
            std::fs::rename(&worktree_path, displaced).expect("displace owned worktree path");
            std::fs::create_dir(&worktree_path).expect("install worktree path replacement");
            std::fs::write(worktree_path.join("sentinel"), b"replacement")
                .expect("write worktree path replacement sentinel");
        }
        #[cfg(test)]
        if SYNC_POST_CAPTURE_REGISTRATION_REPLACEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&safe_id)
        {
            let registration_parent = ownership
                .registration_path
                .parent()
                .expect("registration parent");
            let displaced = registration_parent.join(format!(
                ".owned-registration-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::rename(&ownership.registration_path, displaced)
                .expect("displace owned registration");
            std::fs::create_dir(&ownership.registration_path)
                .expect("install registration replacement");
            std::fs::write(ownership.registration_path.join("sentinel"), b"replacement")
                .expect("write registration replacement sentinel");
        }

        let path = match validate_created_path(&project_root, &canonical_parent, &worktree_path) {
            Ok(path) => path,
            Err(error) => {
                return Err(compensate_failed_create_sync(
                    &project_root,
                    &safe_id,
                    &owned_branch,
                    Some(&path_receipt),
                    Some(&ownership),
                    error,
                ))
            }
        };
        if let Err(error) = validate_created_worktree(&path, &owned_branch) {
            return Err(compensate_failed_create_sync(
                &project_root,
                &safe_id,
                &owned_branch,
                Some(&path_receipt),
                Some(&ownership),
                error,
            ));
        }
        let ownership_receipt_id = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .durable
            .as_ref()
            .ok_or("created worktree has no durable ownership receipt")?
            .record
            .receipt_id
            .clone();
        if ownership_receipt_id != authority.ownership_receipt_id {
            return Err(compensate_failed_create_sync(
                &project_root,
                &safe_id,
                &owned_branch,
                Some(&path_receipt),
                Some(&ownership),
                "created worktree ownership receipt differs from its durable plan".to_string(),
            ));
        }
        let ownership = Arc::new(ownership);
        WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((project_root.clone(), safe_id.clone()), ownership);
        external_guard()?;
        Ok(Self {
            id: safe_id,
            path,
            branch,
            branch_oid: owned_branch.expected_oid,
            project_root,
            ownership_receipt_id,
        })
    }

    #[cfg(test)]
    pub(crate) async fn create_cancellable(
        project_root: &Path,
        id: &str,
        cancellation: Option<&crate::agent::CancellationSignal>,
    ) -> Result<Self, String> {
        let authority = Self::plan_preparation_authority(project_root, id)?;
        Self::create_cancellable_from_preparation_authority(project_root, &authority, cancellation)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn create_cancellable_from_preparation_authority(
        project_root: &Path,
        authority: &WorktreePreparationAuthority,
        cancellation: Option<&crate::agent::CancellationSignal>,
    ) -> Result<Self, String> {
        Self::create_cancellable_from_preparation_authority_with_guard(
            project_root,
            authority,
            cancellation,
            Arc::new(|| Ok(())),
        )
        .await
    }

    pub(crate) async fn create_cancellable_from_preparation_authority_with_guard(
        project_root: &Path,
        authority: &WorktreePreparationAuthority,
        cancellation: Option<&crate::agent::CancellationSignal>,
        external_guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Result<Self, String> {
        external_guard()?;
        let project_root = repository_root_bounded_cancellable(project_root, cancellation).await?;
        external_guard()?;
        let safe_id = sanitize_component(&authority.id);
        if WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&(project_root.clone(), safe_id.clone()))
        {
            return Err(format!(
                "worktree {safe_id} still has an active ownership receipt; cleanup is required before reuse"
            ));
        }
        if let Some(ownership) =
            load_managed_worktree_ownership(&project_root, ManagedWorktreeKind::Subagent, &safe_id)?
        {
            WORKTREE_OWNERSHIP
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((project_root.clone(), safe_id.clone()), ownership);
            return Err(format!(
                "worktree {safe_id} still has an active durable ownership receipt; cleanup is required before reuse"
            ));
        }
        let worktree_path = authority.path.clone();
        let expected_path = project_root
            .join(".nib")
            .join("worktrees")
            .join("subagents")
            .join(&safe_id);
        if worktree_path != expected_path || authority.id != safe_id {
            return Err(
                "planned subagent worktree authority does not match the repository".to_string(),
            );
        }
        let parent = worktree_path
            .parent()
            .ok_or("subagent worktree has no parent directory")?;
        let root_directory = crate::daemons::state::StableDirectory::open(&project_root)?;
        let canonical_parent_directory = root_directory
            .open_or_create_descendant_directory_with_guard(
                parent,
                || external_guard(),
                |_| Ok(()),
            )?;
        let canonical_parent = canonical_parent_directory.path().to_path_buf();
        external_guard()?;
        if !canonical_parent.starts_with(&project_root) {
            return Err(format!(
                "subagent worktree parent escapes the repository: {}",
                parent.display()
            ));
        }
        if crate::fs_security::path_entry_exists(&worktree_path)
            .map_err(|error| format!("failed to inspect subagent worktree destination: {error}"))?
        {
            return Err(format!("worktree {} already exists", safe_id));
        }

        let branch = authority.branch.clone();
        if branch != branch_name(&safe_id) {
            return Err("planned subagent worktree branch is invalid".to_string());
        }
        external_guard()?;
        let mut reservation = reserve_managed_worktree_cancellable_with_receipt(
            &project_root,
            ManagedWorktreeKind::Subagent,
            &safe_id,
            &worktree_path,
            &branch,
            cancellation,
            Some(&authority.ownership_receipt_id),
        )
        .await?;
        external_guard()?;
        if reservation.intent.revision.record.initial_oid != authority.branch_oid {
            return Err(reconcile_failed_managed_worktree_reservation(
                reservation,
                "planned subagent worktree base changed before reservation".to_string(),
            ));
        }
        macro_rules! fail_reserved_create_async {
            ($primary:expr) => {
                return Err(reconcile_failed_managed_worktree_reservation(
                    reservation,
                    $primary,
                ))
            };
        }
        external_guard()?;
        let owned_branch =
            match create_reserved_worktree_branch(&mut reservation, cancellation).await {
                Ok(branch) => branch,
                Err(error) => fail_reserved_create_async!(error),
            };
        external_guard()?;
        if let Err(error) = prove_worktree_destination_absent(&canonical_parent, &worktree_path) {
            fail_reserved_create_async!(
                compensate_failed_create(
                    &project_root,
                    &safe_id,
                    &owned_branch,
                    None,
                    None,
                    error,
                )
                .await
            );
        }
        external_guard()?;
        let path_receipt = match publish_reserved_empty_worktree_destination(
            &mut reservation,
            &canonical_parent,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                fail_reserved_create_async!(
                    compensate_failed_create(
                        &project_root,
                        &safe_id,
                        &owned_branch,
                        None,
                        None,
                        error,
                    )
                    .await
                );
            }
        };
        external_guard()?;
        let registration_snapshot = reserved_worktree_registration_snapshot(&reservation);
        external_guard()?;
        let create = run_git_cancellable(
            &project_root,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                crate::fs_security::path_for_external_command(&worktree_path).into_os_string(),
                OsString::from(&branch),
            ],
            cancellation,
        )
        .await
        .and_then(|output| {
            require_git_success(&output, "worktree add")?;
            Ok(output)
        });
        if let Err(error) = create {
            fail_reserved_create_async!(
                compensate_failed_create(
                    &project_root,
                    &safe_id,
                    &owned_branch,
                    Some(&path_receipt),
                    None,
                    format!(
                        "{error}; post-snapshot Git worktree registrations were preserved because a failed add provides no exact creation receipt"
                    ),
                )
                .await
            );
        }
        external_guard()?;

        let ownership = match capture_managed_worktree_receipt_async(
            &project_root,
            &worktree_path,
            &path_receipt,
            &owned_branch,
            registration_snapshot,
            cancellation,
        )
        .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                let ManagedWorktreeCaptureError { message, ownership } = error;
                fail_reserved_create_async!(
                    compensate_failed_create(
                        &project_root,
                        &safe_id,
                        &owned_branch,
                        Some(&path_receipt),
                        ownership.as_deref(),
                        message,
                    )
                    .await
                );
            }
        };
        external_guard()?;
        let ownership = match finish_managed_worktree_reservation(reservation, ownership) {
            Ok(ownership) => ownership,
            Err(error) => {
                let ManagedWorktreeCaptureError { message, ownership } = error;
                return Err(compensate_failed_create(
                    &project_root,
                    &safe_id,
                    &owned_branch,
                    Some(&path_receipt),
                    ownership.as_deref(),
                    message,
                )
                .await);
            }
        };
        external_guard()?;

        let path = match validate_created_path(&project_root, &canonical_parent, &worktree_path) {
            Ok(path) => path,
            Err(error) => {
                return Err(compensate_failed_create(
                    &project_root,
                    &safe_id,
                    &owned_branch,
                    Some(&path_receipt),
                    Some(&ownership),
                    error,
                )
                .await)
            }
        };
        let validation = validate_created_worktree_async(&path, &owned_branch, cancellation).await;
        if let Err(error) = validation {
            return Err(compensate_failed_create(
                &project_root,
                &safe_id,
                &owned_branch,
                Some(&path_receipt),
                Some(&ownership),
                error,
            )
            .await);
        }
        let ownership_receipt_id = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .durable
            .as_ref()
            .ok_or("created worktree has no durable ownership receipt")?
            .record
            .receipt_id
            .clone();
        if ownership_receipt_id != authority.ownership_receipt_id {
            return Err(compensate_failed_create(
                &project_root,
                &safe_id,
                &owned_branch,
                Some(&path_receipt),
                Some(&ownership),
                "created worktree ownership receipt differs from its durable plan".to_string(),
            )
            .await);
        }
        let ownership = Arc::new(ownership);
        WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((project_root.clone(), safe_id.clone()), ownership);
        external_guard()?;
        Ok(Self {
            id: safe_id,
            path,
            branch,
            branch_oid: owned_branch.expected_oid,
            project_root,
            ownership_receipt_id,
        })
    }

    pub fn remove(project_root: &Path, id: &str) -> Result<(), String> {
        let project_root = repository_root_bounded_sync(project_root)?;
        remove_registered_worktree(&project_root, id, GIT_COMMAND_TIMEOUT)
    }

    pub fn adopt_branch_revision(
        project_root: &Path,
        id: &str,
        expected_oid: &str,
    ) -> Result<(), String> {
        let project_root = repository_root_bounded_sync(project_root)?;
        adopt_registered_worktree_branch(&project_root, id, expected_oid)
    }

    pub(crate) async fn remove_reconciled_async(
        project_root: &Path,
        id: &str,
        expected_oid: &str,
    ) -> Result<(), String> {
        let project_root = project_root.to_path_buf();
        let id = id.to_string();
        let expected_oid = expected_oid.to_string();
        tokio::task::spawn_blocking(move || {
            let project_root = repository_root_bounded_sync(&project_root)?;
            remove_registered_worktree_reconciled(&project_root, &id, &expected_oid)
        })
        .await
        .map_err(|error| format!("reconciled worktree cleanup worker failed: {error}"))?
    }

    #[cfg(test)]
    pub(crate) fn remove_precommit(project_root: &Path, worktree: &Self) -> Result<(), String> {
        let deadline = Instant::now() + GIT_COMMAND_TIMEOUT;
        let project_root = repository_root_bounded_sync(project_root)?;
        remove_registered_worktree_precommit_until(
            &project_root,
            worktree,
            deadline,
            GIT_COMMAND_TIMEOUT,
        )
    }

    #[cfg(test)]
    pub(crate) fn remove_precommit_bounded_sync(
        project_root: &Path,
        worktree: &Self,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let requested = canonical_requested_root(project_root)?;
        let inspect = run_git_bounded_sync_with_timeout(
            &requested,
            ["rev-parse", "--show-toplevel"],
            cleanup_time_remaining(deadline, timeout)?,
        )?;
        require_git_success(&inspect, "inspect repository for bounded worktree cleanup")?;
        let project_root = validate_reported_repository_root(&requested, &inspect.stdout)?;
        remove_registered_worktree_precommit_until(&project_root, worktree, deadline, timeout)
    }

    pub(crate) fn verify_owned_namespace(&self) -> Result<(), String> {
        let key = (self.project_root.clone(), self.id.clone());
        let ownership = WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
            .ok_or("created worktree ownership is no longer retained")?;
        let receipt_id = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .durable
            .as_ref()
            .ok_or("created worktree has no durable ownership revision")?
            .record
            .receipt_id
            .clone();
        if receipt_id != self.ownership_receipt_id || ownership.path != self.path {
            return Err("created worktree ownership generation changed".to_string());
        }
        validate_managed_worktree_ownership(&ownership)
    }
}

fn prove_worktree_destination_absent(parent: &Path, path: &Path) -> Result<(), String> {
    crate::fs_security::verify_directory_without_symlinks(parent)
        .map_err(|error| format!("worktree destination parent changed: {error}"))?;
    if crate::fs_security::path_entry_exists(path)
        .map_err(|error| format!("failed to inspect worktree destination: {error}"))?
    {
        return Err(format!(
            "worktree destination appeared before creation and was preserved: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn publish_reserved_empty_worktree_destination(
    reservation: &mut ManagedWorktreeReservation,
    parent: &Path,
) -> Result<crate::fs_security::DirectoryRemovalReceipt, String> {
    let revision = &mut reservation.intent.revision;
    if revision.record.phase != DurableOwnershipPhase::Intent
        || revision.record.path_cleanup != DurableArtifactPhase::Reserved
        || revision.record.worktree_identity.is_some()
    {
        return Err("managed worktree path reservation is not publishable".to_string());
    }
    let path = revision.record.worktree_path.clone();
    let staging = revision.record.worktree_staging_path.clone();
    if path.parent() != Some(parent) || staging.parent() != Some(parent) {
        return Err("managed worktree path reservation parent changed".to_string());
    }
    let parent_directory = crate::daemons::state::StableDirectory::open(parent)?;
    if parent_directory.entry_kind(&path)?.is_some() {
        return Err(format!(
            "worktree destination appeared before owned publication and was preserved: {}",
            path.display()
        ));
    }
    if parent_directory.entry_kind(&staging)?.is_some() {
        return Err(format!(
            "worktree reservation staging entry already exists and was preserved: {}",
            staging.display()
        ));
    }

    let staging_directory = parent_directory.create_owned_child_directory(&staging)?;
    let receipt = staging_directory.directory_removal_receipt()?;
    let mut record = revision.record.clone();
    record.worktree_identity = Some(receipt.identity());
    record.path_cleanup = DurableArtifactPhase::Present;
    if let Err(error) = persist_durable_ownership_revision(revision, record) {
        let cleanup =
            parent_directory.remove_empty_child_directory_if_matches(&staging, staging_directory);
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => {
                format!("{error}; exact reserved worktree staging cleanup failed: {cleanup}")
            }
        });
    }

    parent_directory.rename_child_directory(&staging, &staging_directory, &path)?;
    Ok(receipt)
}

#[cfg(test)]
pub(crate) fn publish_owned_empty_worktree_destination(
    parent: &Path,
    path: &Path,
) -> Result<crate::fs_security::DirectoryRemovalReceipt, String> {
    let parent_directory = crate::daemons::state::StableDirectory::open(parent)?;
    if parent_directory.entry_kind(path)?.is_some() {
        return Err(format!(
            "worktree destination appeared before owned publication and was preserved: {}",
            path.display()
        ));
    }
    let staging = parent.join(format!(
        ".nib-worktree-create-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let staging_directory = parent_directory.create_owned_child_directory(&staging)?;
    let receipt = staging_directory.directory_removal_receipt()?;
    if let Err(error) = parent_directory.rename_child_directory(&staging, &staging_directory, path)
    {
        drop(staging_directory);
        let deadline = Instant::now() + CANCELLED_CREATE_CLEANUP_TIMEOUT;
        let published_cleanup =
            crate::fs_security::remove_directory_tree_capability_bound_if_matches(
                parent,
                path,
                receipt.clone(),
                deadline,
            );
        if published_cleanup.is_ok() {
            return Err(error);
        }
        let staging_cleanup = crate::fs_security::remove_directory_tree_capability_bound_if_matches(
            parent,
            &staging,
            receipt.clone(),
            deadline,
        );
        return Err(match staging_cleanup {
            Ok(()) => error,
            Err(staging_cleanup) => format!(
                "{error}; exact published cleanup failed: {}; exact unpublished staging cleanup failed: {staging_cleanup}",
                published_cleanup.expect_err("published cleanup failed")
            ),
        });
    }
    Ok(receipt)
}

async fn capture_managed_worktree_receipt_async(
    project_root: &Path,
    path: &Path,
    path_receipt: &crate::fs_security::DirectoryRemovalReceipt,
    owned_branch: &OwnedBranch,
    registration_snapshot: &ManagedWorktreeRegistrationSnapshot,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<ManagedWorktreeReceipt, ManagedWorktreeCaptureError> {
    let output = run_git_cancellable(
        project_root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        cancellation,
    )
    .await?;
    let common_git_dir = parse_common_git_directory(project_root, &output)?;
    capture_managed_worktree_receipt(
        &common_git_dir,
        path,
        path_receipt,
        owned_branch,
        registration_snapshot,
    )
}

pub(crate) fn capture_managed_worktree_receipt_sync(
    project_root: &Path,
    path: &Path,
    path_receipt: &crate::fs_security::DirectoryRemovalReceipt,
    owned_branch: &OwnedBranch,
    registration_snapshot: &ManagedWorktreeRegistrationSnapshot,
) -> Result<ManagedWorktreeReceipt, ManagedWorktreeCaptureError> {
    capture_managed_worktree_receipt_sync_controlled(
        project_root,
        path,
        path_receipt,
        owned_branch,
        registration_snapshot,
        None,
    )
}

pub(crate) fn capture_managed_worktree_receipt_sync_controlled(
    project_root: &Path,
    path: &Path,
    path_receipt: &crate::fs_security::DirectoryRemovalReceipt,
    owned_branch: &OwnedBranch,
    registration_snapshot: &ManagedWorktreeRegistrationSnapshot,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<ManagedWorktreeReceipt, ManagedWorktreeCaptureError> {
    let output = run_git_bounded_sync_with_timeout_controlled(
        project_root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let common_git_dir = parse_common_git_directory(project_root, &output)?;
    capture_managed_worktree_receipt(
        &common_git_dir,
        path,
        path_receipt,
        owned_branch,
        registration_snapshot,
    )
}

fn capture_worktree_registration_snapshot_from_common(
    common_directory: crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<ManagedWorktreeRegistrationSnapshot, String> {
    let common_path = common_directory.path();
    let registrations_path = common_path.join("worktrees");
    let registrations = match common_directory.entry_kind(&registrations_path)? {
        None => {
            if common_directory.entry_kind(&registrations_path)?.is_some() {
                return Err(
                    "Git worktree registration directory appeared during pre-add inspection"
                        .to_string(),
                );
            }
            None
        }
        Some(crate::daemons::state::StableEntryKind::File) => {
            return Err(format!(
                "Git worktree registration entry is not a directory: {}",
                registrations_path.display()
            ));
        }
        Some(crate::daemons::state::StableEntryKind::Directory) => {
            let directory = common_directory.open_child(&registrations_path)?;
            let mut entries = std::collections::HashMap::new();
            let expected_git_file = path.join(".git");
            directory.for_each_entry_bounded(
                MAX_WORKTREE_REGISTRATIONS,
                MAX_WORKTREE_REGISTRATION_NAME_BYTES,
                |name| {
                    let registration_path = registrations_path.join(&name);
                    if directory.entry_kind(&registration_path)?
                        != Some(crate::daemons::state::StableEntryKind::Directory)
                    {
                        return Err(format!(
                            "Git worktree registration entry is not a directory: {}",
                            registration_path.display()
                        ));
                    }
                    let registration = directory.open_child(&registration_path)?;
                    let gitdir_path = registration_path.join("gitdir");
                    let backlink = parse_plain_path(
                        &read_small_stable_file(&registration, &gitdir_path)?,
                        "Git worktree registration backlink",
                    )?;
                    if crate::fs_security::canonical_paths_match(
                        &expected_git_file,
                        &backlink,
                    ) {
                        return Err(format!(
                            "pre-existing Git worktree registration already points to {}; preserving it: {}",
                            expected_git_file.display(),
                            registration_path.display()
                        ));
                    }
                    let identity = registration.directory_removal_receipt()?.identity();
                    if entries.insert(name, identity).is_some() {
                        return Err(
                            "Git worktree registration scan returned a duplicate entry".to_string(),
                        );
                    }
                    Ok(())
                },
            )?;
            directory.verify_visible_at(&registrations_path)?;
            Some(ExistingWorktreeRegistrations { directory, entries })
        }
    };

    let visible_common = crate::daemons::state::StableDirectory::open(common_path)?;
    if !common_directory.same_identity(&visible_common) {
        return Err(
            "common Git directory changed during worktree registration inspection".to_string(),
        );
    }
    Ok(ManagedWorktreeRegistrationSnapshot {
        common_directory,
        registrations,
    })
}

fn parse_common_git_directory(project_root: &Path, output: &Output) -> Result<PathBuf, String> {
    require_git_success(output, "inspect common Git directory")?;
    let reported = String::from_utf8(output.stdout.clone())
        .map_err(|_| "git common directory was not valid UTF-8".to_string())?;
    let reported = reported.trim_end_matches(['\r', '\n']);
    if reported.is_empty()
        || reported
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err("git returned an invalid common directory".to_string());
    }
    let reported = PathBuf::from(reported);
    let reported = if reported.is_absolute() {
        reported
    } else {
        project_root.join(reported)
    };
    let common = reported
        .canonicalize()
        .map_err(|error| format!("failed to resolve common Git directory: {error}"))?;
    crate::fs_security::verify_directory_without_symlinks(&common)
        .map_err(|error| format!("common Git directory is unsafe: {error}"))?;
    Ok(common)
}

fn capture_managed_worktree_receipt(
    common_git_dir: &Path,
    path: &Path,
    path_receipt: &crate::fs_security::DirectoryRemovalReceipt,
    owned_branch: &OwnedBranch,
    registration_snapshot: &ManagedWorktreeRegistrationSnapshot,
) -> Result<ManagedWorktreeReceipt, ManagedWorktreeCaptureError> {
    if registration_snapshot.common_directory.path() != common_git_dir {
        return Err(
            "managed worktree common Git directory changed after pre-add inspection".into(),
        );
    }
    let common_directory = crate::daemons::state::StableDirectory::open(common_git_dir)?;
    if !registration_snapshot
        .common_directory
        .same_identity(&common_directory)
    {
        return Err(
            "managed worktree common Git directory identity changed after pre-add inspection"
                .into(),
        );
    }

    let worktree_directory = open_stable_direct_child(path)?;
    let visible_path = worktree_directory.directory_removal_receipt()?;
    if !path_receipt.same_identity(&visible_path) {
        return Err(format!(
            "managed worktree destination was replaced after owned publication; preserving it: {}",
            path.display()
        )
        .into());
    }
    let git_file = path.join(".git");
    let reported_registration_path = parse_gitdir_pointer(
        &read_small_stable_file(&worktree_directory, &git_file)?,
        "managed worktree .git pointer",
    )?;
    let registrations = common_git_dir.join("worktrees");
    let opened_registrations;
    let registrations_directory = if let Some(existing) = &registration_snapshot.registrations {
        existing.directory.verify_visible_at(&registrations)?;
        &existing.directory
    } else {
        opened_registrations = common_directory.open_child(&registrations)?;
        &opened_registrations
    };
    let registration_path = trusted_git_registration_path(
        &registrations,
        &reported_registration_path,
        "managed worktree registration",
    )?;
    let registration_name = registration_path
        .file_name()
        .expect("registration filename was validated above");
    if registration_snapshot
        .registrations
        .as_ref()
        .is_some_and(|existing| existing.entries.contains_key(registration_name))
    {
        return Err(format!(
            "managed worktree registration existed before worktree add and was preserved: {}",
            registration_path.display()
        )
        .into());
    }
    let registration_directory = registrations_directory.open_child(&registration_path)?;
    let registration_receipt = registration_directory.directory_removal_receipt()?;
    if registration_snapshot
        .registrations
        .as_ref()
        .is_some_and(|existing| {
            existing
                .entries
                .values()
                .any(|identity| *identity == registration_receipt.identity())
        })
    {
        return Err(format!(
            "managed worktree registration reused a pre-add directory identity and was preserved: {}",
            registration_path.display()
        )
        .into());
    }
    let reciprocal_validation = validate_reciprocal_worktree_link_opened(
        path,
        &worktree_directory,
        &registration_path,
        &registration_directory,
    );
    let reciprocal_link_proven = reciprocal_validation.is_ok();
    let ownership = ManagedWorktreeReceipt {
        path: path.to_path_buf(),
        path_receipt: Some(path_receipt.clone()),
        registration_path,
        registration_receipt: Some(registration_receipt),
        state: std::sync::Mutex::new(ManagedWorktreeState {
            owned_branch: Some(owned_branch.clone()),
            path_removed: false,
            registration_removed: false,
            branch_removed: false,
            reciprocal_link_proven,
            durable: None,
        }),
    };
    match reciprocal_validation.and_then(|()| validate_owned_ref_receipt(owned_branch)) {
        Ok(()) => Ok(ownership),
        Err(message) => Err(ManagedWorktreeCaptureError {
            message,
            ownership: Some(Box::new(ownership)),
        }),
    }
}

fn validate_reciprocal_worktree_link(
    path: &Path,
    path_receipt: &crate::fs_security::DirectoryRemovalReceipt,
    registration_path: &Path,
    registration_receipt: &crate::fs_security::DirectoryRemovalReceipt,
) -> Result<(), String> {
    let worktree = open_stable_direct_child(path)?;
    if worktree.directory_removal_receipt()?.identity() != path_receipt.identity() {
        return Err("managed worktree path identity changed".to_string());
    }
    let registration = open_stable_direct_child(registration_path)?;
    if registration.directory_removal_receipt()?.identity() != registration_receipt.identity() {
        return Err("Git worktree registration identity changed".to_string());
    }
    validate_reciprocal_worktree_link_opened(path, &worktree, registration_path, &registration)
}

fn validate_reciprocal_worktree_link_opened(
    path: &Path,
    worktree: &crate::daemons::state::StableDirectory,
    registration_path: &Path,
    registration: &crate::daemons::state::StableDirectory,
) -> Result<(), String> {
    if worktree.path() != path || registration.path() != registration_path {
        return Err("managed worktree reciprocal-link capability path changed".to_string());
    }
    let linked_registration = parse_gitdir_pointer(
        &read_small_stable_file(worktree, &path.join(".git"))?,
        "managed worktree .git pointer",
    )?;
    if !crate::fs_security::canonical_paths_match(registration_path, &linked_registration) {
        return Err("managed worktree registration pointer changed".to_string());
    }
    let linked_worktree = parse_plain_path(
        &read_small_stable_file(registration, &registration_path.join("gitdir"))?,
        "Git worktree registration backlink",
    )?;
    let expected = path.join(".git");
    if !crate::fs_security::canonical_paths_match(&expected, &linked_worktree) {
        return Err(format!(
            "Git worktree registration does not point back to {}",
            expected.display()
        ));
    }
    worktree.verify_visible()?;
    registration.verify_visible()?;
    Ok(())
}

fn open_stable_direct_child(path: &Path) -> Result<crate::daemons::state::StableDirectory, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("managed directory has no parent: {}", path.display()))?;
    crate::daemons::state::StableDirectory::open(parent)?.open_child(path)
}

fn trusted_git_registration_path(
    registrations: &Path,
    reported: &Path,
    label: &str,
) -> Result<PathBuf, String> {
    if reported.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(format!(
            "{label} contains a non-direct registration path: {}",
            reported.display()
        ));
    }
    let parent = reported
        .parent()
        .ok_or_else(|| format!("{label} registration has no parent: {}", reported.display()))?;
    let name = reported.file_name().ok_or_else(|| {
        format!(
            "{label} registration has no filename: {}",
            reported.display()
        )
    })?;
    if !crate::fs_security::canonical_paths_match(registrations, parent) {
        return Err(format!(
            "{label} is not a direct child of {}: {}",
            registrations.display(),
            reported.display()
        ));
    }
    Ok(registrations.join(name))
}

fn parse_gitdir_pointer(contents: &[u8], label: &str) -> Result<PathBuf, String> {
    let contents = std::str::from_utf8(contents).map_err(|_| format!("{label} is not UTF-8"))?;
    let contents = contents
        .strip_suffix("\r\n")
        .or_else(|| contents.strip_suffix('\n'))
        .unwrap_or(contents);
    let path = contents
        .strip_prefix("gitdir: ")
        .ok_or_else(|| format!("{label} has an invalid format"))?;
    parse_plain_path(path.as_bytes(), label)
}

fn parse_plain_path(contents: &[u8], label: &str) -> Result<PathBuf, String> {
    let contents = std::str::from_utf8(contents).map_err(|_| format!("{label} is not UTF-8"))?;
    let contents = contents
        .strip_suffix("\r\n")
        .or_else(|| contents.strip_suffix('\n'))
        .unwrap_or(contents);
    if contents.is_empty()
        || contents.contains('\r')
        || contents.contains('\n')
        || contents
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(format!("{label} contains an invalid path"));
    }
    let path = PathBuf::from(contents);
    if !path.is_absolute() {
        return Err(format!("{label} must contain an absolute path"));
    }
    Ok(path)
}

fn read_small_stable_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<Vec<u8>, String> {
    let mut file = directory.open_read(path)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
    if metadata.len() > 4096 {
        return Err(format!(
            "managed Git metadata exceeds 4096 bytes: {}",
            path.display()
        ));
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(4097)
        .read_to_end(&mut contents)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if contents.len() > 4096 {
        return Err(format!(
            "managed Git metadata exceeds 4096 bytes: {}",
            path.display()
        ));
    }
    directory.verify_file_identity(path, &file)?;
    Ok(contents)
}

fn remove_registered_worktree(
    project_root: &Path,
    id: &str,
    timeout: Duration,
) -> Result<(), String> {
    remove_registered_worktree_until(project_root, id, Instant::now() + timeout, timeout)
}

fn remove_registered_worktree_until(
    project_root: &Path,
    id: &str,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), String> {
    let safe_id = sanitize_component(id);
    let key = (project_root.to_path_buf(), safe_id.clone());
    let ownership = WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned();
    let ownership = match ownership {
        Some(ownership) => Some(ownership),
        None => {
            load_managed_worktree_ownership(project_root, ManagedWorktreeKind::Subagent, &safe_id)?
        }
    };
    let Some(ownership) = ownership else {
        let ownership_directory = managed_worktree_ownership_directory(project_root)?;
        let _ownership_lock = OwnershipCompactionLock::acquire(
            &ownership_directory,
            cleanup_time_remaining(deadline, timeout)?,
        )?;
        if let Some(revision) =
            load_durable_ownership_revision(project_root, ManagedWorktreeKind::Subagent, &safe_id)?
        {
            if revision.record.phase == DurableOwnershipPhase::Complete {
                recover_owned_ref_restart_artifacts(&revision.record)?;
                return Ok(());
            }
            return Err(format!(
                "durable worktree ownership {} could not be rehydrated safely",
                revision.record.receipt_id
            ));
        }
        return prove_managed_worktree_namespace_absent_until(
            project_root,
            &safe_id,
            deadline,
            timeout,
        );
    };
    cleanup_managed_worktree(&ownership, deadline, timeout)?;
    WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key);
    Ok(())
}

#[cfg(test)]
fn remove_registered_worktree_precommit_until(
    project_root: &Path,
    worktree: &Worktree,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), String> {
    remove_registered_worktree_precommit_until_with_guard(
        project_root,
        worktree,
        deadline,
        timeout,
        &mut || Ok(()),
    )
}

fn remove_registered_worktree_precommit_until_with_guard(
    project_root: &Path,
    worktree: &Worktree,
    deadline: Instant,
    timeout: Duration,
    external_guard: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    external_guard()?;
    let safe_id = sanitize_component(&worktree.id);
    let key = (project_root.to_path_buf(), safe_id.clone());
    let cached = WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned();
    let ownership = match cached {
        Some(ownership) => Some(ownership),
        None => {
            load_managed_worktree_ownership(project_root, ManagedWorktreeKind::Subagent, &safe_id)?
        }
    }
    .ok_or_else(|| {
        format!(
            "precommit worktree {} no longer has its exact durable ownership receipt",
            safe_id
        )
    })?;
    {
        let state = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let durable = state
            .durable
            .as_ref()
            .ok_or("precommit worktree has no durable ownership revision")?;
        if durable.record.receipt_id != worktree.ownership_receipt_id {
            return Err(
                "precommit worktree ownership generation changed; replacement preserved"
                    .to_string(),
            );
        }
    }
    cleanup_managed_worktree_with_guard(&ownership, deadline, timeout, external_guard)?;

    external_guard()?;
    let ownership_directory = managed_worktree_ownership_directory(project_root)?;
    let _ownership_lock = OwnershipCompactionLock::acquire(
        &ownership_directory,
        cleanup_time_remaining(deadline, timeout)?,
    )?;
    let mut state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let durable = state
        .durable
        .as_ref()
        .ok_or("precommit worktree has no durable ownership revision")?;
    if durable.record.receipt_id != worktree.ownership_receipt_id
        || durable.record.phase != DurableOwnershipPhase::Complete
    {
        return Err(
            "precommit worktree cleanup did not reach its exact complete ownership generation"
                .to_string(),
        );
    }
    external_guard()?;
    cleanup_time_remaining(deadline, timeout)?;
    durable
        .directory
        .remove_visible_file_if_matches_direct_with_guard(&durable.path, &durable.file, || {
            external_guard()?;
            cleanup_time_remaining(deadline, timeout).map(|_| ())
        })?;
    external_guard()?;
    cleanup_time_remaining(deadline, timeout)?;
    state.durable = None;
    drop(state);
    WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&key);
    Ok(())
}

fn adopt_registered_worktree_branch(
    project_root: &Path,
    id: &str,
    expected_oid: &str,
) -> Result<(), String> {
    let safe_id = sanitize_component(id);
    let key = (project_root.to_path_buf(), safe_id.clone());
    let ownership = WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned();
    let ownership = match ownership {
        Some(ownership) => ownership,
        None => {
            let revision = load_durable_ownership_revision(
                project_root,
                ManagedWorktreeKind::Subagent,
                &safe_id,
            )?
            .ok_or_else(|| {
                format!(
                    "cannot adopt branch revision for worktree {safe_id} without its durable generational receipt"
                )
            })?;
            Arc::new(rehydrate_owned_worktree(revision, Some(expected_oid))?)
        }
    };
    adopt_managed_worktree_branch(&ownership, &safe_id, expected_oid)
}

fn adopt_managed_worktree_branch(
    ownership: &ManagedWorktreeReceipt,
    safe_id: &str,
    expected_oid: &str,
) -> Result<(), String> {
    let mut state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(durable) = state.durable.as_mut() {
        reconcile_previous_branch_anchor(durable)?;
    }
    if state.branch_removed {
        return Err(format!(
            "cannot adopt branch revision for worktree {safe_id} after cleanup has started"
        ));
    }
    let owned_branch = state.owned_branch.as_ref().cloned().ok_or_else(|| {
        format!("cannot adopt branch revision for worktree {safe_id} without its branch receipt")
    })?;
    if owned_branch.expected_oid == expected_oid {
        validate_owned_ref_receipt(&owned_branch).map_err(|error| {
            format!(
                "cannot adopt an identical branch revision for worktree {safe_id} because its generational ref identity changed: {error}"
            )
        })?;
        if !state.path_removed && !state.registration_removed {
            validate_managed_worktree_directories(ownership)?;
            validate_owned_worktree_sync(&ownership.path, &owned_branch)?;
        }
        return Ok(());
    }
    if state.path_removed || state.registration_removed {
        return Err(format!(
            "cannot adopt a different branch revision for worktree {safe_id} after cleanup has started"
        ));
    }
    validate_managed_worktree_directories(ownership)?;
    let durable = state.durable.as_ref().ok_or_else(|| {
        format!(
            "cannot adopt branch revision for worktree {safe_id} without its durable generational receipt"
        )
    })?;
    let next_generation = durable
        .record
        .branch_anchor_generation
        .checked_add(1)
        .ok_or("managed branch anchor generation overflowed")?;
    let next_anchor_path = managed_branch_paths(
        &durable.record.common_git_dir,
        &durable.record.branch_reference,
        &durable.record.receipt_id,
        next_generation,
    )?
    .1;
    let previous_anchor_path = owned_branch
        .receipt
        .anchor_path
        .clone()
        .ok_or("managed branch generation anchor path is unavailable")?;
    let previous_anchor_file = owned_branch
        .receipt
        .anchor_file
        .as_ref()
        .ok_or("managed branch generation anchor receipt is unavailable")?;
    let previous_anchor_identity = crate::fs_security::file_identity_snapshot(previous_anchor_file)
        .map_err(|error| format!("failed to retain previous branch anchor identity: {error}"))?;
    if Some(previous_anchor_identity) != durable.record.branch_identity {
        return Err(
            "previous branch anchor no longer matches its durable generation; replacement preserved"
                .to_string(),
        );
    }
    let adopted =
        capture_owned_branch_revision(&owned_branch, expected_oid, Some(&next_anchor_path))?;
    validate_owned_worktree_sync(&ownership.path, &adopted)?;
    let adopted_identity = crate::fs_security::file_identity_snapshot(&adopted.receipt.file)
        .map_err(|error| format!("failed to retain adopted branch identity: {error}"))?;
    let durable = state
        .durable
        .as_mut()
        .expect("durable branch generation was validated");
    let mut record = durable.record.clone();
    record.current_oid = expected_oid.to_string();
    record.branch_identity = Some(adopted_identity);
    record.previous_branch_anchor = Some(DurablePreviousBranchAnchor {
        path: previous_anchor_path.clone(),
        identity: previous_anchor_identity,
        oid: owned_branch.expected_oid.clone(),
    });
    record.branch_anchor_generation = next_generation;
    record.branch_staging_path = next_anchor_path;
    persist_durable_ownership_revision(durable, record)?;
    state.owned_branch = Some(adopted);
    remove_owned_file_receipt(
        &owned_branch.receipt.directory,
        &previous_anchor_path,
        previous_anchor_file,
        &owned_branch.receipt.contents,
        ".nib-owned-ref-retire-",
    )?;
    let durable = state
        .durable
        .as_mut()
        .expect("durable branch generation was validated");
    let mut record = durable.record.clone();
    record.previous_branch_anchor = None;
    persist_durable_ownership_revision(durable, record)?;
    Ok(())
}

fn remove_registered_worktree_reconciled(
    project_root: &Path,
    id: &str,
    expected_oid: &str,
) -> Result<(), String> {
    let safe_id = sanitize_component(id);
    let key = (project_root.to_path_buf(), safe_id.clone());
    let mut ownerships = WORKTREE_OWNERSHIP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(ownership) = ownerships.get(&key).cloned() {
        adopt_managed_worktree_branch(&ownership, &safe_id, expected_oid)?;
        cleanup_managed_worktree(
            &ownership,
            Instant::now() + GIT_COMMAND_TIMEOUT,
            GIT_COMMAND_TIMEOUT,
        )?;
        ownerships.remove(&key);
        return Ok(());
    }
    drop(ownerships);
    if let Some(revision) =
        load_durable_ownership_revision(project_root, ManagedWorktreeKind::Subagent, &safe_id)?
    {
        if revision.record.phase == DurableOwnershipPhase::Complete {
            return Ok(());
        }
        let ownership = rehydrate_owned_worktree(revision, Some(expected_oid))?;
        cleanup_managed_worktree(
            &ownership,
            Instant::now() + GIT_COMMAND_TIMEOUT,
            GIT_COMMAND_TIMEOUT,
        )?;
        return Ok(());
    }
    prove_managed_worktree_absent(project_root, &safe_id, expected_oid)
}

fn prove_managed_worktree_absent(
    project_root: &Path,
    safe_id: &str,
    expected_oid: &str,
) -> Result<(), String> {
    if !(40..=64).contains(&expected_oid.len())
        || !expected_oid.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("managed worktree absence proof has an invalid branch object ID".to_string());
    }
    prove_managed_worktree_namespace_absent_until(
        project_root,
        safe_id,
        Instant::now() + GIT_COMMAND_TIMEOUT,
        GIT_COMMAND_TIMEOUT,
    )
}

fn prove_managed_worktree_namespace_absent_until(
    project_root: &Path,
    safe_id: &str,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), String> {
    let path = project_root.join(".nib/worktrees/subagents").join(safe_id);
    if crate::fs_security::path_entry_exists(&path)
        .map_err(|error| format!("failed to inspect reconciled worktree path: {error}"))?
    {
        return Err(format!(
            "managed worktree path still exists without an active ownership receipt: {}",
            path.display()
        ));
    }

    let reference = format!("refs/heads/{}", branch_name(safe_id));
    let branch = run_git_bounded_sync_with_timeout(
        project_root,
        ["show-ref", "--verify", "--quiet", reference.as_str()],
        cleanup_time_remaining(deadline, timeout)?,
    )?;
    match branch.status.code() {
        Some(1) => {}
        Some(0) => {
            return Err(format!(
                "managed worktree branch {reference} still exists without an active ownership receipt"
            ));
        }
        _ => {
            return Err(git_failure(
                &branch,
                "prove managed worktree branch absence",
            ))
        }
    }

    let common = run_git_bounded_sync_with_timeout(
        project_root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        cleanup_time_remaining(deadline, timeout)?,
    )?;
    let common = parse_common_git_directory(project_root, &common)?;
    let registrations = common.join("worktrees");
    if !crate::fs_security::path_entry_exists(&registrations)
        .map_err(|error| format!("failed to inspect Git worktree registrations: {error}"))?
    {
        return Ok(());
    }
    let registrations_directory = crate::daemons::state::StableDirectory::open(&registrations)?;
    let expected_git_file = path.join(".git");
    let mut matching_registration = None;
    registrations_directory.for_each_entry_bounded(4096, 1024 * 1024, |name| {
        cleanup_time_remaining(deadline, timeout)?;
        let registration_path = registrations.join(&name);
        if registrations_directory.entry_kind(&registration_path)?
            != Some(crate::daemons::state::StableEntryKind::Directory)
        {
            return Err(format!(
                "Git worktree registration entry is not a directory: {}",
                registration_path.display()
            ));
        }
        let registration = registrations_directory.open_child(&registration_path)?;
        let gitdir_path = registration_path.join("gitdir");
        let gitdir = read_small_stable_file(&registration, &gitdir_path)?;
        if crate::fs_security::canonical_paths_match(
            &expected_git_file,
            &parse_plain_path(&gitdir, "Git worktree registration backlink")?,
        ) {
            matching_registration = Some(registration_path);
        }
        Ok(())
    })?;
    cleanup_time_remaining(deadline, timeout)?;
    if let Some(registration) = matching_registration {
        return Err(format!(
            "Git worktree registration still exists without an active ownership receipt: {}",
            registration.display()
        ));
    }
    Ok(())
}

pub(crate) fn cleanup_managed_worktree(
    ownership: &ManagedWorktreeReceipt,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), String> {
    cleanup_managed_worktree_with_guard(ownership, deadline, timeout, &mut || Ok(()))
}

fn cleanup_managed_worktree_with_guard(
    ownership: &ManagedWorktreeReceipt,
    deadline: Instant,
    timeout: Duration,
    external_guard: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    external_guard()?;
    let mut state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(durable) = state.durable.as_mut() {
        external_guard()?;
        reconcile_previous_branch_anchor(durable)?;
        external_guard()?;
    }
    let registration_parent = ownership
        .registration_path
        .parent()
        .ok_or("managed worktree registration has no parent")?;
    let path_parent = ownership
        .path
        .parent()
        .ok_or("managed worktree path has no parent")?;
    let path_validation = if state.path_removed {
        Ok(())
    } else {
        validate_directory_receipt(
            path_parent,
            &ownership.path,
            ownership
                .path_receipt
                .as_ref()
                .ok_or("managed worktree path ownership receipt is unavailable")?,
            "managed worktree path",
        )
    };
    let registration_validation = if state.registration_removed {
        Ok(())
    } else {
        validate_directory_receipt(
            registration_parent,
            &ownership.registration_path,
            ownership
                .registration_receipt
                .as_ref()
                .ok_or("Git worktree registration ownership receipt is unavailable")?,
            "Git worktree registration",
        )
    };
    let reciprocal_validation = if state.reciprocal_link_proven {
        Ok(())
    } else {
        match (&path_validation, &registration_validation) {
            (Ok(()), Ok(())) if !state.path_removed && !state.registration_removed => {
                validate_reciprocal_worktree_link(
                    &ownership.path,
                    ownership
                        .path_receipt
                        .as_ref()
                        .ok_or("managed worktree path ownership receipt is unavailable")?,
                    &ownership.registration_path,
                    ownership.registration_receipt.as_ref().ok_or(
                        "Git worktree registration ownership receipt is unavailable",
                    )?,
                )
            }
            _ => Err("reciprocal worktree linkage could not be proven because an owned directory identity changed".to_string()),
        }
    };
    if reciprocal_validation.is_ok() {
        state.reciprocal_link_proven = true;
    }
    let mut errors = Vec::new();
    if !state.registration_removed
        && registration_validation.is_ok()
        && reciprocal_validation.is_ok()
    {
        if let Err(error) = external_guard().and_then(|()| {
            persist_cleanup_artifact_phase(
                &mut state,
                CleanupArtifact::Registration,
                DurableArtifactPhase::Removing,
            )
        }) {
            errors.push(error);
        } else if let Err(error) = external_guard()
            .and_then(|()| {
                crate::fs_security::remove_directory_tree_capability_bound_if_matches(
                    registration_parent,
                    &ownership.registration_path,
                    ownership.registration_receipt.clone().ok_or_else(|| {
                        "Git worktree registration ownership receipt is unavailable".to_string()
                    })?,
                    deadline,
                )
                .map_err(|error| error.to_string())
            })
            .and_then(|()| external_guard())
        {
            errors.push(format!(
                "failed to remove owned Git worktree registration: {error}"
            ));
        } else {
            state.registration_removed = true;
            if let Err(error) = external_guard()
                .and_then(|()| {
                    persist_cleanup_artifact_phase(
                        &mut state,
                        CleanupArtifact::Registration,
                        DurableArtifactPhase::Removed,
                    )
                })
                .and_then(|()| external_guard())
            {
                errors.push(error);
            }
        }
    } else if !state.registration_removed {
        errors.push(
            registration_validation.err().unwrap_or_else(|| {
                reciprocal_validation.expect_err("reciprocal validation failed")
            }),
        );
    }
    if !state.path_removed && state.registration_removed && path_validation.is_ok() {
        if let Err(error) = external_guard().and_then(|()| {
            persist_cleanup_artifact_phase(
                &mut state,
                CleanupArtifact::Path,
                DurableArtifactPhase::Removing,
            )
        }) {
            errors.push(error);
        } else if let Err(error) = external_guard()
            .and_then(|()| {
                crate::fs_security::remove_directory_tree_capability_bound_if_matches(
                    path_parent,
                    &ownership.path,
                    ownership.path_receipt.clone().ok_or_else(|| {
                        "managed worktree path ownership receipt is unavailable".to_string()
                    })?,
                    deadline,
                )
                .map_err(|error| error.to_string())
            })
            .and_then(|()| external_guard())
        {
            errors.push(format!(
                "failed to remove owned worktree directory: {error}"
            ));
        } else {
            state.path_removed = true;
            if let Err(error) = external_guard()
                .and_then(|()| {
                    persist_cleanup_artifact_phase(
                        &mut state,
                        CleanupArtifact::Path,
                        DurableArtifactPhase::Removed,
                    )
                })
                .and_then(|()| external_guard())
            {
                errors.push(error);
            }
        }
    } else if !state.path_removed && !state.registration_removed {
        errors.push(
            "owned worktree path was preserved until registration cleanup is durably complete"
                .to_string(),
        );
    } else if !state.path_removed {
        errors.push(path_validation.expect_err("unremoved worktree path validation failed"));
    }
    match cleanup_time_remaining(deadline, timeout) {
        Ok(_) if state.branch_removed => {}
        Ok(remaining) => {
            let owned_branch = state
                .owned_branch
                .clone()
                .ok_or("managed worktree branch ownership receipt is unavailable")?;
            if let Err(error) = external_guard().and_then(|()| {
                persist_cleanup_artifact_phase(
                    &mut state,
                    CleanupArtifact::Branch,
                    DurableArtifactPhase::Removing,
                )
            }) {
                errors.push(error);
            } else if let Err(error) = external_guard()
                .and_then(|()| {
                    delete_owned_branch_sync_with_timeout(&ownership.path, &owned_branch, remaining)
                })
                .and_then(|()| external_guard())
            {
                errors.push(error);
                match owned_ref_namespace_is_absent(&owned_branch) {
                    Ok(true) => {
                        state.branch_removed = true;
                        if let Err(error) = external_guard()
                            .and_then(|()| {
                                persist_cleanup_artifact_phase(
                                    &mut state,
                                    CleanupArtifact::Branch,
                                    DurableArtifactPhase::Removed,
                                )
                            })
                            .and_then(|()| external_guard())
                        {
                            errors.push(error);
                        }
                    }
                    Ok(false) => {}
                    Err(error) => errors.push(format!(
                        "failed to verify branch state after cleanup error: {error}"
                    )),
                }
            } else {
                state.branch_removed = true;
                if let Err(error) = external_guard()
                    .and_then(|()| {
                        persist_cleanup_artifact_phase(
                            &mut state,
                            CleanupArtifact::Branch,
                            DurableArtifactPhase::Removed,
                        )
                    })
                    .and_then(|()| external_guard())
                {
                    errors.push(error);
                }
            }
        }
        Err(error) => errors.push(error),
    }
    external_guard()?;
    finish_cleanup_errors(errors)
}

#[derive(Clone, Copy)]
enum CleanupArtifact {
    Path,
    Registration,
    Branch,
}

fn persist_cleanup_artifact_phase(
    state: &mut ManagedWorktreeState,
    artifact: CleanupArtifact,
    phase: DurableArtifactPhase,
) -> Result<(), String> {
    let Some(durable) = state.durable.as_mut() else {
        return Ok(());
    };
    let mut record = durable.record.clone();
    match artifact {
        CleanupArtifact::Path => record.path_cleanup = phase,
        CleanupArtifact::Registration => record.registration_cleanup = phase,
        CleanupArtifact::Branch => record.branch_cleanup = phase,
    }
    record.phase = if record.path_cleanup == DurableArtifactPhase::Removed
        && record.registration_cleanup == DurableArtifactPhase::Removed
        && record.branch_cleanup == DurableArtifactPhase::Removed
        && record.previous_branch_anchor.is_none()
    {
        DurableOwnershipPhase::Complete
    } else {
        DurableOwnershipPhase::Cleanup
    };
    persist_durable_ownership_revision(durable, record)
}

pub(crate) fn validate_managed_worktree_ownership(
    ownership: &ManagedWorktreeReceipt,
) -> Result<(), String> {
    validate_managed_worktree_directories(ownership)?;
    let state = ownership
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.path_removed || state.registration_removed || state.branch_removed {
        return Err("managed worktree cleanup has already started".to_string());
    }
    validate_owned_ref_receipt(
        state
            .owned_branch
            .as_ref()
            .ok_or("managed worktree branch ownership receipt is unavailable")?,
    )
}

pub(crate) fn validate_managed_worktree_for_read(
    ownership: &ManagedWorktreeReceipt,
) -> Result<PathBuf, String> {
    validate_managed_worktree_ownership(ownership)?;
    let owned_branch = {
        let state = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .owned_branch
            .as_ref()
            .cloned()
            .ok_or("managed worktree branch ownership receipt is unavailable")?
    };
    validate_owned_worktree_sync(&ownership.path, &owned_branch)?;
    validate_managed_worktree_ownership(ownership)?;
    Ok(ownership.path.clone())
}

fn validate_managed_worktree_directories(ownership: &ManagedWorktreeReceipt) -> Result<(), String> {
    let path_parent = ownership
        .path
        .parent()
        .ok_or("managed worktree path has no parent")?;
    let registration_parent = ownership
        .registration_path
        .parent()
        .ok_or("managed worktree registration has no parent")?;
    validate_directory_receipt(
        path_parent,
        &ownership.path,
        ownership
            .path_receipt
            .as_ref()
            .ok_or("managed worktree path ownership receipt is unavailable")?,
        "managed worktree path",
    )?;
    validate_directory_receipt(
        registration_parent,
        &ownership.registration_path,
        ownership
            .registration_receipt
            .as_ref()
            .ok_or("Git worktree registration ownership receipt is unavailable")?,
        "Git worktree registration",
    )?;
    validate_reciprocal_worktree_link(
        &ownership.path,
        ownership
            .path_receipt
            .as_ref()
            .ok_or("managed worktree path ownership receipt is unavailable")?,
        &ownership.registration_path,
        ownership
            .registration_receipt
            .as_ref()
            .ok_or("Git worktree registration ownership receipt is unavailable")?,
    )?;
    Ok(())
}

fn validate_directory_receipt(
    parent: &Path,
    path: &Path,
    expected: &crate::fs_security::DirectoryRemovalReceipt,
    label: &str,
) -> Result<(), String> {
    let visible = crate::fs_security::capture_directory_removal_receipt(parent, path)
        .map_err(|error| format!("failed to validate {label}: {error}"))?;
    if expected.same_identity(&visible) {
        Ok(())
    } else {
        Err(format!(
            "{label} was replaced and was preserved: {}",
            path.display()
        ))
    }
}

fn validate_created_path(
    project_root: &Path,
    canonical_parent: &Path,
    worktree_path: &Path,
) -> Result<PathBuf, String> {
    crate::fs_security::verify_directory_without_symlinks(canonical_parent)
        .map_err(|error| format!("subagent worktree parent changed: {error}"))?;
    let path = worktree_path.canonicalize().map_err(|error| {
        format!(
            "created worktree {} cannot be resolved: {error}",
            worktree_path.display()
        )
    })?;
    if path != worktree_path || !path.starts_with(canonical_parent) || !path.is_dir() {
        return Err(format!(
            "created worktree escaped its repository-local destination: {}",
            path.display()
        ));
    }
    if !canonical_parent.starts_with(project_root) {
        return Err("created worktree parent escaped the repository".to_string());
    }
    crate::fs_security::verify_directory_without_symlinks(&path)
        .map_err(|error| format!("created worktree path is unsafe: {error}"))?;
    Ok(path)
}

async fn validate_created_worktree_async(
    path: &Path,
    owned_branch: &OwnedBranch,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<(), String> {
    let output = run_git_cancellable(path, ["rev-parse", "--show-toplevel"], cancellation).await?;
    require_git_success(&output, "inspect created worktree")?;
    let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .canonicalize()
        .map_err(|error| format!("failed to resolve created git worktree: {error}"))?;
    if reported != path {
        return Err(format!(
            "created git worktree root {} does not match {}",
            reported.display(),
            path.display()
        ));
    }
    validate_owned_worktree_async(path, owned_branch, cancellation).await
}

async fn validate_owned_worktree_async(
    path: &Path,
    owned: &OwnedBranch,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<(), String> {
    let symbolic =
        run_git_cancellable(path, ["symbolic-ref", "--quiet", "HEAD"], cancellation).await?;
    let symbolic = parse_symbolic_head(&symbolic)?;
    if symbolic != owned.reference {
        return Err(format!(
            "created worktree symbolic HEAD {symbolic} does not match owned branch {}",
            owned.reference
        ));
    }

    let head = run_git_cancellable(
        path,
        ["rev-parse", "--verify", "HEAD^{commit}"],
        cancellation,
    )
    .await?;
    let head = parse_git_oid(&head, "inspect created worktree HEAD")?;
    validate_owned_worktree_oid("HEAD", &head, owned)?;

    validate_owned_ref_receipt(owned)?;
    let reference = run_git_cancellable(
        path,
        ["show-ref", "--hash", "--verify", owned.reference.as_str()],
        cancellation,
    )
    .await?;
    let reference = parse_git_oid(&reference, "inspect created worktree branch")?;
    validate_owned_worktree_oid(&owned.reference, &reference, owned)?;
    validate_owned_ref_receipt(owned)
}

async fn compensate_failed_create(
    project_root: &Path,
    id: &str,
    owned_branch: &OwnedBranch,
    path_receipt: Option<&crate::fs_security::DirectoryRemovalReceipt>,
    ownership: Option<&ManagedWorktreeReceipt>,
    error: String,
) -> String {
    let deadline = Instant::now() + CANCELLED_CREATE_CLEANUP_TIMEOUT;
    match cleanup_partial_create(
        project_root,
        id,
        Some(owned_branch),
        path_receipt,
        ownership,
        deadline,
        CANCELLED_CREATE_CLEANUP_TIMEOUT,
    )
    .await
    {
        Ok(()) => error,
        Err(cleanup) => format!("{error}; partial worktree cleanup failed: {cleanup}"),
    }
}

fn compensate_failed_create_sync(
    project_root: &Path,
    id: &str,
    owned_branch: &OwnedBranch,
    path_receipt: Option<&crate::fs_security::DirectoryRemovalReceipt>,
    ownership: Option<&ManagedWorktreeReceipt>,
    error: String,
) -> String {
    let deadline = Instant::now() + SYNC_CREATE_CLEANUP_TIMEOUT;
    match cleanup_partial_create_sync(
        project_root,
        id,
        Some(owned_branch),
        path_receipt,
        ownership,
        deadline,
        SYNC_CREATE_CLEANUP_TIMEOUT,
    ) {
        Ok(()) => error,
        Err(cleanup) => format!("{error}; partial worktree cleanup failed: {cleanup}"),
    }
}

fn cleanup_partial_create_sync(
    project_root: &Path,
    id: &str,
    owned_branch: Option<&OwnedBranch>,
    path_receipt: Option<&crate::fs_security::DirectoryRemovalReceipt>,
    ownership: Option<&ManagedWorktreeReceipt>,
    deadline: Instant,
    cleanup_timeout: Duration,
) -> Result<(), String> {
    let path = project_root
        .join(".nib")
        .join("worktrees")
        .join("subagents")
        .join(id);
    if let Some(ownership) = ownership {
        return cleanup_managed_worktree(ownership, deadline, cleanup_timeout);
    }
    let mut errors = Vec::new();
    if let Some(path_receipt) = path_receipt {
        if let Some(parent) = path.parent() {
            if let Err(error) =
                crate::fs_security::remove_directory_tree_capability_bound_if_matches(
                    parent,
                    &path,
                    path_receipt.clone(),
                    deadline,
                )
            {
                errors.push(format!(
                    "failed to remove exact partial worktree path: {error}"
                ));
            }
        } else {
            errors.push("partial worktree path has no parent".to_string());
        }
    } else {
        match crate::fs_security::path_entry_exists(&path) {
            Ok(true) => errors.push(format!(
                "partial worktree path has no exact ownership receipt and was preserved: {}",
                path.display()
            )),
            Ok(false) => {}
            Err(error) => errors.push(format!(
                "failed to inspect unowned partial worktree path: {error}"
            )),
        }
    }
    if let Some(owned_branch) = owned_branch {
        match cleanup_time_remaining(deadline, cleanup_timeout) {
            Ok(remaining) => {
                if let Err(error) =
                    delete_owned_branch_sync_with_timeout(project_root, owned_branch, remaining)
                {
                    errors.push(error);
                }
            }
            Err(error) => errors.push(error),
        }
    }
    finish_cleanup_errors(errors)
}

async fn cleanup_partial_create(
    project_root: &Path,
    id: &str,
    owned_branch: Option<&OwnedBranch>,
    path_receipt: Option<&crate::fs_security::DirectoryRemovalReceipt>,
    ownership: Option<&ManagedWorktreeReceipt>,
    deadline: Instant,
    cleanup_timeout: Duration,
) -> Result<(), String> {
    cleanup_partial_create_sync(
        project_root,
        id,
        owned_branch,
        path_receipt,
        ownership,
        deadline,
        cleanup_timeout,
    )
}

fn finish_cleanup_errors(errors: Vec<String>) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn cleanup_time_remaining(
    deadline: Instant,
    cleanup_timeout: Duration,
) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            format!(
                "partial worktree cleanup deadline exceeded after {} seconds",
                cleanup_timeout.as_secs_f64()
            )
        })
}

async fn repository_root_bounded_cancellable(
    project_root: &Path,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<PathBuf, String> {
    let requested = canonical_requested_root(project_root)?;
    let output =
        run_git_cancellable(&requested, ["rev-parse", "--show-toplevel"], cancellation).await?;
    require_git_success(&output, "inspect repository")?;
    validate_reported_repository_root(&requested, &output.stdout)
}

fn repository_root_bounded_sync(project_root: &Path) -> Result<PathBuf, String> {
    let requested = canonical_requested_root(project_root)?;
    let output = run_git_bounded_sync(&requested, ["rev-parse", "--show-toplevel"])?;
    require_git_success(&output, "inspect repository")?;
    validate_reported_repository_root(&requested, &output.stdout)
}

fn canonical_requested_root(project_root: &Path) -> Result<PathBuf, String> {
    let requested = project_root
        .canonicalize()
        .map_err(|error| format!("invalid project root {}: {error}", project_root.display()))?;
    if !requested.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            requested.display()
        ));
    }
    Ok(requested)
}

fn validate_reported_repository_root(requested: &Path, stdout: &[u8]) -> Result<PathBuf, String> {
    let root = PathBuf::from(String::from_utf8_lossy(stdout).trim().to_string())
        .canonicalize()
        .map_err(|error| format!("failed to resolve git root: {error}"))?;
    if root != requested {
        return Err(format!(
            "subagent project root {} must be the git top-level {}",
            requested.display(),
            root.display()
        ));
    }
    Ok(root)
}

struct OwnedRefLock {
    directory: crate::daemons::state::StableDirectory,
    path: PathBuf,
    receipt: Option<crate::daemons::state::FilePublicationReceipt>,
    contents: Vec<u8>,
}

impl OwnedRefLock {
    fn acquire(
        directory: &crate::daemons::state::StableDirectory,
        path: PathBuf,
    ) -> Result<Self, String> {
        let contents = format!("nib-ref-lock {}\n", uuid::Uuid::new_v4()).into_bytes();
        Self::acquire_with_contents(directory, path, contents)
    }

    fn acquire_with_contents(
        directory: &crate::daemons::state::StableDirectory,
        path: PathBuf,
        contents: Vec<u8>,
    ) -> Result<Self, String> {
        let retained_directory = directory.try_clone()?;
        let receipt = match directory.save_bytes_atomically_expected_with_locked_receipt(
            &path,
            &contents,
            MANAGED_REF_LOCK_TEMPORARY_PREFIX,
            crate::daemons::state::FileExpectation::Missing,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                if let Some(receipt) = error.receipt {
                    let cleanup =
                        verify_open_file_contents(&receipt.file, &contents).and_then(|()| {
                            directory.remove_visible_file_if_matches_direct(&path, &receipt.file)
                        });
                    return Err(match cleanup {
                        Ok(()) => error.message,
                        Err(cleanup) => format!(
                            "{}; exact partial ref-lock cleanup failed and the ambiguous lock was preserved: {cleanup}",
                            error.message
                        ),
                    });
                }
                return Err(error.message);
            }
        };
        if !receipt.exact_identity {
            let cleanup = verify_open_file_contents(&receipt.file, &contents).and_then(|()| {
                directory.remove_visible_file_if_matches_direct(&path, &receipt.file)
            });
            return Err(match cleanup {
                Ok(()) => {
                    "managed Git ref locking requires an exact no-replace file identity on this platform"
                        .to_string()
                }
                Err(cleanup) => format!(
                    "managed Git ref locking requires an exact no-replace file identity on this platform; exact lock cleanup failed: {cleanup}"
                ),
            });
        }
        Ok(Self {
            directory: retained_directory,
            path,
            receipt: Some(receipt),
            contents,
        })
    }

    fn release(&mut self) -> Result<(), String> {
        let Some(receipt) = self.receipt.as_ref() else {
            return Ok(());
        };
        #[cfg(test)]
        if take_owned_ref_lock_release_failure(&self.path) {
            return Err("injected owned ref lock release failure".to_string());
        }
        verify_open_file_contents(&receipt.file, &self.contents)?;
        self.directory
            .remove_visible_file_if_matches_direct(&self.path, &receipt.file)?;
        self.receipt = None;
        Ok(())
    }
}

#[cfg(test)]
fn take_owned_ref_lock_release_failure(path: &Path) -> bool {
    let mut failures = OWNED_REF_LOCK_RELEASE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let matched = failures
        .iter()
        .find(|candidate| crate::fs_security::canonical_paths_match(path, candidate))
        .cloned();
    matched.is_some_and(|matched| failures.remove(&matched))
}

fn managed_ref_lock_contents(receipt_id: &str, reference: &str, role: &str) -> Vec<u8> {
    format!("nib-managed-ref-lock-v1\nreceipt={receipt_id}\nreference={reference}\nrole={role}\n")
        .into_bytes()
}

fn valid_managed_ref_lock_marker(contents: &[u8]) -> bool {
    let Ok(contents) = std::str::from_utf8(contents) else {
        return false;
    };
    let mut lines = contents.lines();
    if lines.next() != Some("nib-managed-ref-lock-v1") {
        return false;
    }
    let receipt = lines.next().and_then(|line| line.strip_prefix("receipt="));
    let reference = lines
        .next()
        .and_then(|line| line.strip_prefix("reference="));
    let role = lines.next().and_then(|line| line.strip_prefix("role="));
    lines.next().is_none()
        && receipt.is_some_and(|receipt| uuid::Uuid::parse_str(receipt).is_ok())
        && reference.is_some_and(|reference| {
            reference.starts_with("refs/heads/nib/")
                && !reference.bytes().any(|byte| byte.is_ascii_control())
        })
        && matches!(role, Some("packed" | "target"))
        && contents.as_bytes().last() == Some(&b'\n')
}

fn managed_ref_lock_marker_is_foreign(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    expected: &[u8],
) -> Result<bool, String> {
    let mut file = directory.open_read(path)?;
    let length = file
        .metadata()
        .map_err(|error| format!("failed to inspect managed ref lock marker: {error}"))?
        .len();
    if length > 4096 {
        return Ok(false);
    }
    let mut contents = Vec::with_capacity(length as usize);
    file.by_ref()
        .take(4097)
        .read_to_end(&mut contents)
        .map_err(|error| format!("failed to read managed ref lock marker: {error}"))?;
    directory.verify_file_identity(path, &file)?;
    Ok(contents != expected && valid_managed_ref_lock_marker(&contents))
}

fn acquire_owned_ref_protocol_lock(
    directory: &crate::daemons::state::StableDirectory,
    path: PathBuf,
    lock_owner: Option<&str>,
    reference: &str,
    role: &str,
) -> Result<OwnedRefLock, String> {
    match lock_owner {
        Some(receipt_id) => OwnedRefLock::acquire_with_contents(
            directory,
            path,
            managed_ref_lock_contents(receipt_id, reference, role),
        ),
        None => OwnedRefLock::acquire(directory, path),
    }
}

fn recover_atomic_ref_scratch(
    directory: &crate::daemons::state::StableDirectory,
    prefixes: &[&str],
) -> Result<(), String> {
    for prefix in prefixes {
        directory.recover_stale_temporary_files_strict(
            prefix,
            MAX_MANAGED_REF_LOCK_DIRECTORY_ENTRIES,
            MAX_MANAGED_REF_LOCK_DIRECTORY_NAME_BYTES,
        )?;
    }
    Ok(())
}

fn require_atomic_ref_scratch_absent(
    directory: &crate::daemons::state::StableDirectory,
    target: &Path,
    prefix: &str,
    label: &str,
) -> Result<(), String> {
    let temporary = directory.deterministic_artifact_path(target, prefix, ".tmp")?;
    let previous = directory.deterministic_previous_artifact_path(target, prefix)?;
    if directory.path_exists(&temporary)? || directory.path_exists(&previous)? {
        return Err(format!(
            "{label} atomic publication is still live or ambiguous; scratch was preserved: {}",
            target.display()
        ));
    }
    Ok(())
}

fn recover_dead_marker_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    expected: &[u8],
    label: &str,
) -> Result<(), String> {
    match directory.entry_kind(path)? {
        None => return Ok(()),
        Some(crate::daemons::state::StableEntryKind::File) => {}
        Some(crate::daemons::state::StableEntryKind::Directory) => {
            return Err(format!(
                "{label} is not a regular file and was preserved: {}",
                path.display()
            ));
        }
    }
    let file = directory.open_read_write(path)?;
    verify_open_file_contents(&file, expected).map_err(|error| {
        format!(
            "{label} does not match this durable receipt and was preserved: {error}: {}",
            path.display()
        )
    })?;
    directory.verify_file_identity(path, &file)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(format!(
                "{label} is still owned by a live process and was preserved: {}",
                path.display()
            ));
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(format!(
                "failed to inspect {label} kernel ownership; it was preserved: {error}: {}",
                path.display()
            ));
        }
    }
    verify_open_file_contents(&file, expected)?;
    directory.remove_visible_file_if_matches_direct(path, &file)
}

fn recover_owned_ref_lock_location(
    directory: &crate::daemons::state::StableDirectory,
    lock_path: &Path,
    expected: &[u8],
    label: &str,
) -> Result<(), String> {
    let quarantine = directory.deterministic_artifact_path(
        lock_path,
        MANAGED_REF_LOCK_DELETE_PREFIX,
        ".quarantine",
    )?;
    let visible = directory.path_exists(lock_path)?;
    let quarantined = directory.path_exists(&quarantine)?;
    if visible && quarantined {
        return Err(format!(
            "{label} and its deletion quarantine both exist; both were preserved: {}",
            lock_path.display()
        ));
    }
    if visible {
        if managed_ref_lock_marker_is_foreign(directory, lock_path, expected)? {
            return Ok(());
        }
        recover_dead_marker_file(directory, lock_path, expected, label)?;
    } else if quarantined {
        if managed_ref_lock_marker_is_foreign(directory, &quarantine, expected)? {
            return Ok(());
        }
        recover_dead_marker_file(
            directory,
            &quarantine,
            expected,
            &format!("{label} deletion quarantine"),
        )?;
    }
    if directory.path_exists(lock_path)? || directory.path_exists(&quarantine)? {
        return Err(format!(
            "{label} restart recovery did not prove physical absence: {}",
            lock_path.display()
        ));
    }
    Ok(())
}

fn recover_owned_ref_restart_artifacts(
    record: &DurableManagedWorktreeOwnership,
) -> Result<(), String> {
    let common = reopen_common_git_directory(record)?;
    recover_atomic_ref_scratch(
        &common,
        &[
            MANAGED_REF_LOCK_TEMPORARY_PREFIX,
            MANAGED_REF_LOCK_DELETE_PREFIX,
        ],
    )?;
    let packed_lock_path = common.path().join("packed-refs.lock");
    require_atomic_ref_scratch_absent(
        &common,
        &packed_lock_path,
        MANAGED_REF_LOCK_TEMPORARY_PREFIX,
        "managed packed-ref lock",
    )?;
    recover_owned_ref_lock_location(
        &common,
        &packed_lock_path,
        &managed_ref_lock_contents(&record.receipt_id, &record.branch_reference, "packed"),
        "managed packed-ref lock",
    )?;

    let (ref_path, anchor_path) = managed_branch_paths(
        &record.common_git_dir,
        &record.branch_reference,
        &record.receipt_id,
        record.branch_anchor_generation,
    )?;
    let parent = ref_path
        .parent()
        .ok_or("managed worktree branch has no parent directory")?;
    match std::fs::symlink_metadata(parent) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "failed to inspect managed worktree ref directory: {error}"
            ));
        }
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "managed worktree ref parent is not a directory and was preserved: {}",
                parent.display()
            ));
        }
    }
    crate::fs_security::verify_directory_without_symlinks(parent)
        .map_err(|error| format!("managed worktree ref directory is unsafe: {error}"))?;
    let directory = crate::daemons::state::StableDirectory::open(parent)?;
    recover_atomic_ref_scratch(
        &directory,
        &[
            MANAGED_REF_LOCK_TEMPORARY_PREFIX,
            MANAGED_REF_LOCK_DELETE_PREFIX,
            RESERVED_REF_TEMPORARY_PREFIX,
            RESERVED_REF_DELETE_PREFIX,
        ],
    )?;
    require_atomic_ref_scratch_absent(
        &directory,
        &anchor_path,
        RESERVED_REF_TEMPORARY_PREFIX,
        "reserved branch staging",
    )?;

    let reserved_quarantine = directory.deterministic_artifact_path(
        &anchor_path,
        RESERVED_REF_DELETE_PREFIX,
        ".quarantine",
    )?;
    recover_dead_marker_file(
        &directory,
        &reserved_quarantine,
        format!("{}\n", record.initial_oid).as_bytes(),
        "reserved branch staging deletion quarantine",
    )?;

    let mut target_lock_name = ref_path
        .file_name()
        .ok_or("managed branch ref has no filename")?
        .to_os_string();
    target_lock_name.push(".lock");
    let target_lock_path = parent.join(target_lock_name);
    require_atomic_ref_scratch_absent(
        &directory,
        &target_lock_path,
        MANAGED_REF_LOCK_TEMPORARY_PREFIX,
        "managed target-ref lock",
    )?;
    recover_owned_ref_lock_location(
        &directory,
        &target_lock_path,
        &managed_ref_lock_contents(&record.receipt_id, &record.branch_reference, "target"),
        "managed target-ref lock",
    )
}

impl Drop for OwnedRefLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

struct OwnedRefClaim {
    reference: String,
    expected_oid: String,
    common_directory: crate::daemons::state::StableDirectory,
    ref_directory: crate::daemons::state::StableDirectory,
    ref_path: PathBuf,
    target_lock_path: PathBuf,
    lock_owner: Option<String>,
    packed_lock: OwnedRefLock,
    target_lock: Option<OwnedRefLock>,
}

struct ReservedBranchPublication {
    path: PathBuf,
    file: std::fs::File,
    contents: Vec<u8>,
}

fn ref_names_conflict(existing: &[u8], requested: &[u8]) -> bool {
    existing == requested
        || (existing.starts_with(requested) && existing.get(requested.len()) == Some(&b'/'))
        || (requested.starts_with(existing) && requested.get(existing.len()) == Some(&b'/'))
}

fn valid_packed_object_id(value: &[u8]) -> bool {
    (40..=64).contains(&value.len()) && value.iter().all(u8::is_ascii_hexdigit)
}

fn packed_ref_namespace_conflict(
    directory: &crate::daemons::state::StableDirectory,
    requested: &str,
) -> Result<Option<String>, String> {
    let path = directory.path().join("packed-refs");
    match directory.entry_kind(&path)? {
        None => return Ok(None),
        Some(crate::daemons::state::StableEntryKind::File) => {}
        Some(crate::daemons::state::StableEntryKind::Directory) => {
            return Err("managed Git packed-refs entry is not a regular file".to_string());
        }
    }
    let file = directory.open_read(&path)?;
    let length = file
        .metadata()
        .map_err(|error| format!("failed to inspect packed-refs: {error}"))?
        .len();
    if length > MAX_PACKED_REFS_BYTES {
        return Err(format!(
            "managed Git packed-refs exceeds the {} byte safety limit",
            MAX_PACKED_REFS_BYTES
        ));
    }

    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut total = 0_u64;
    let mut saw_ref = false;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("failed to read packed-refs: {error}"))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_PACKED_REFS_BYTES {
            return Err(format!(
                "managed Git packed-refs exceeds the {} byte safety limit",
                MAX_PACKED_REFS_BYTES
            ));
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.is_empty() || line[0] == b'#' {
            saw_ref = false;
            continue;
        }
        if line[0] == b'^' {
            if !saw_ref || !valid_packed_object_id(&line[1..]) {
                return Err("managed Git packed-refs contains an invalid peeled entry".to_string());
            }
            saw_ref = false;
            continue;
        }
        let Some(separator) = line.iter().position(|byte| *byte == b' ') else {
            return Err("managed Git packed-refs contains an invalid ref entry".to_string());
        };
        let (oid, name_with_separator) = line.split_at(separator);
        let name = &name_with_separator[1..];
        if !valid_packed_object_id(oid)
            || name.is_empty()
            || name
                .iter()
                .any(|byte| byte.is_ascii_control() || *byte == b' ')
        {
            return Err("managed Git packed-refs contains an invalid ref entry".to_string());
        }
        saw_ref = true;
        if ref_names_conflict(name, requested.as_bytes()) {
            return Ok(Some(String::from_utf8_lossy(name).into_owned()));
        }
    }
    directory.verify_file_identity(&path, reader.get_ref())?;
    Ok(None)
}

impl OwnedRefClaim {
    fn release_locks(&mut self) -> Result<(), String> {
        let target = self
            .target_lock
            .as_mut()
            .map_or(Ok(()), OwnedRefLock::release);
        let packed = self.packed_lock.release();
        match (target, packed) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(target), Ok(())) => Err(target),
            (Ok(()), Err(packed)) => Err(packed),
            (Err(target), Err(packed)) => {
                Err(format!("target ref lock cleanup failed: {target}; packed ref lock cleanup failed: {packed}"))
            }
        }
    }

    fn fail<T>(&mut self, error: String) -> Result<T, String> {
        match self.release_locks() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; exact managed ref-lock cleanup failed: {cleanup}"
            )),
        }
    }
}

fn prepare_owned_ref_claim(
    common_git_dir: &Path,
    reference: &str,
    expected_oid: &str,
    lock_owner: Option<&str>,
) -> Result<OwnedRefClaim, String> {
    let relative = reference
        .strip_prefix("refs/heads/")
        .ok_or("managed worktree branch must be beneath refs/heads")?;
    let relative = Path::new(relative);
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("managed worktree branch contains an unsafe component".to_string());
    }
    let leaf = relative
        .file_name()
        .ok_or("managed worktree branch has no leaf component")?;
    let ref_parent = common_git_dir
        .join("refs")
        .join("heads")
        .join(relative.parent().unwrap_or_else(|| Path::new("")));
    crate::fs_security::ensure_directory_without_symlinks(&ref_parent)
        .map_err(|error| format!("managed worktree ref directory is unsafe: {error}"))?;
    let common_directory = crate::daemons::state::StableDirectory::open(common_git_dir)?;
    let ref_directory = crate::daemons::state::StableDirectory::open(&ref_parent)?;
    let packed_lock = acquire_owned_ref_protocol_lock(
        &common_directory,
        common_git_dir.join("packed-refs.lock"),
        lock_owner,
        reference,
        "packed",
    )?;
    let mut lock_name = leaf.to_os_string();
    lock_name.push(".lock");
    let ref_path = ref_parent.join(leaf);
    let mut claim = OwnedRefClaim {
        reference: reference.to_string(),
        expected_oid: expected_oid.to_string(),
        common_directory,
        ref_directory,
        ref_path,
        target_lock_path: ref_parent.join(lock_name),
        lock_owner: lock_owner.map(str::to_string),
        packed_lock,
        target_lock: None,
    };
    match claim.ref_directory.entry_kind(&claim.ref_path) {
        Ok(None) => {}
        Ok(Some(_)) => {
            let error = describe_existing_owned_ref(
                &claim.ref_directory,
                &claim.ref_path,
                reference,
                "already has a loose ref",
            );
            return claim.fail(error);
        }
        Err(error) => return claim.fail(error),
    }
    match packed_ref_namespace_conflict(&claim.common_directory, reference) {
        Ok(None) => Ok(claim),
        Ok(Some(existing)) => claim.fail(format!(
            "managed worktree branch {reference} conflicts with packed ref {existing}; preserving it"
        )),
        Err(error) => claim.fail(error),
    }
}

fn claim_owned_ref_after_inspection(
    claim: &mut OwnedRefClaim,
    existing: &Output,
    staged: Option<ReservedBranchPublication>,
) -> Result<OwnedBranch, String> {
    match existing.status.code() {
        Some(1) => {}
        Some(0) => {
            return claim.fail(format!(
                "managed worktree branch {} is already packed or otherwise defined; preserving it",
                claim.reference
            ));
        }
        _ => return claim.fail(git_failure(existing, "inspect missing worktree branch")),
    }
    let target_lock = match acquire_owned_ref_protocol_lock(
        &claim.ref_directory,
        claim.target_lock_path.clone(),
        claim.lock_owner.as_deref(),
        &claim.reference,
        "target",
    ) {
        Ok(lock) => lock,
        Err(error) => return claim.fail(error),
    };
    claim.target_lock = Some(target_lock);
    #[cfg(test)]
    if let Some(target) = BEFORE_REF_PUBLICATION_SYMREFS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&claim.reference)
    {
        std::fs::write(&claim.ref_path, format!("ref: {target}\n"))
            .expect("install hostile ref publication race fixture");
    }
    match claim.ref_directory.entry_kind(&claim.ref_path) {
        Ok(None) => {}
        Ok(Some(_)) => {
            let error = describe_existing_owned_ref(
                &claim.ref_directory,
                &claim.ref_path,
                &claim.reference,
                "appeared while its ref lock was held",
            );
            return claim.fail(error);
        }
        Err(error) => return claim.fail(error),
    }
    let common_directory = match claim.common_directory.try_clone() {
        Ok(directory) => directory,
        Err(error) => return claim.fail(error),
    };
    let ref_directory = match claim.ref_directory.try_clone() {
        Ok(directory) => directory,
        Err(error) => return claim.fail(error),
    };
    let contents = format!("{}\n", claim.expected_oid).into_bytes();
    let (publication, anchor) = if let Some(staged) = staged {
        if staged.path.parent() != claim.ref_path.parent() || staged.contents != contents {
            return claim.fail(
                "reserved branch publication does not match its final ref claim".to_string(),
            );
        }
        if let Err(error) = verify_open_file_contents(&staged.file, &contents) {
            return claim.fail(error);
        }
        if let Err(error) =
            claim
                .ref_directory
                .hard_link_to(&staged.path, &claim.ref_directory, &claim.ref_path)
        {
            return claim.fail(error);
        }
        let final_file = match claim.ref_directory.open_read(&claim.ref_path) {
            Ok(file) => file,
            Err(error) => return claim.fail(error),
        };
        if let Err(error) = claim
            .ref_directory
            .verify_file_identity(&claim.ref_path, &staged.file)
        {
            return claim.fail(format!(
                "reserved branch final ref does not match its retained anchor: {error}"
            ));
        }
        (
            crate::daemons::state::FilePublicationReceipt {
                file: final_file,
                exact_identity: true,
            },
            Some((staged.path, staged.file)),
        )
    } else {
        let publication = claim
            .ref_directory
            .save_bytes_atomically_expected_with_receipt(
                &claim.ref_path,
                &contents,
                ".nib-owned-ref-",
                crate::daemons::state::FileExpectation::Missing,
            );
        let publication = match publication {
            Ok(receipt) => receipt,
            Err(error) => {
                if let Some(receipt) = error.receipt {
                    let cleanup = remove_owned_file_receipt(
                        &claim.ref_directory,
                        &claim.ref_path,
                        &receipt.file,
                        &contents,
                        ".nib-owned-ref-delete-",
                    );
                    let error = match cleanup {
                        Ok(()) => error.message,
                        Err(cleanup) => format!(
                            "{}; exact partial ref cleanup failed: {cleanup}",
                            error.message
                        ),
                    };
                    return claim.fail(error);
                }
                return claim.fail(error.message);
            }
        };
        (publication, None)
    };
    if !publication.exact_identity {
        let cleanup = remove_owned_file_receipt(
            &claim.ref_directory,
            &claim.ref_path,
            &publication.file,
            &contents,
            ".nib-owned-ref-delete-",
        );
        let error = match cleanup {
            Ok(()) => {
                "managed Git ref publication requires an exact no-replace file identity on this platform"
                    .to_string()
            }
            Err(cleanup) => format!(
                "managed Git ref publication requires an exact no-replace file identity on this platform; exact ref cleanup failed: {cleanup}"
            ),
        };
        return claim.fail(error);
    }
    let receipt = Arc::new(OwnedRefReceipt {
        common_directory,
        directory: ref_directory,
        path: claim.ref_path.clone(),
        file: publication.file,
        anchor_path: anchor.as_ref().map(|(path, _)| path.clone()),
        anchor_file: anchor.map(|(_, file)| file),
        lock_owner: claim.lock_owner.clone(),
        contents,
    });
    let owned = OwnedBranch {
        reference: claim.reference.clone(),
        expected_oid: claim.expected_oid.clone(),
        receipt,
    };
    if let Err(lock_error) = claim.release_locks() {
        let cleanup = remove_owned_ref_receipt(&owned);
        return Err(match cleanup {
            Ok(()) => format!("managed worktree ref lock cleanup failed: {lock_error}"),
            Err(cleanup) => format!(
                "managed worktree ref lock cleanup failed: {lock_error}; exact ref compensation failed: {cleanup}"
            ),
        });
    }
    Ok(owned)
}

fn remove_owned_file_receipt(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    file: &std::fs::File,
    contents: &[u8],
    quarantine_prefix: &str,
) -> Result<(), String> {
    directory.remove_file_if_matches_with_hooks(
        path,
        file,
        quarantine_prefix,
        || verify_open_file_contents(file, contents),
        || verify_open_file_contents(file, contents),
    )
}

fn verify_open_file_contents(file: &std::fs::File, expected: &[u8]) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect retained ref receipt: {error}"))?;
    if metadata.len() != expected.len() as u64 {
        return Err("retained Git ref contents changed; preserving it".to_string());
    }
    let actual =
        crate::daemons::state::read_open_file_prefix(file, expected.len().saturating_add(1))
            .map_err(|error| format!("failed to read retained ref receipt: {error}"))?;
    if actual != expected {
        return Err("retained Git ref contents changed; preserving it".to_string());
    }
    Ok(())
}

fn validate_owned_ref_receipt(owned: &OwnedBranch) -> Result<(), String> {
    owned
        .receipt
        .directory
        .verify_file_identity(&owned.receipt.path, &owned.receipt.file)
        .map_err(|error| {
            describe_existing_owned_ref(
                &owned.receipt.directory,
                &owned.receipt.path,
                &owned.reference,
                &format!("changed; preserving its replacement: {error}"),
            )
        })?;
    verify_open_file_contents(&owned.receipt.file, &owned.receipt.contents)?;
    match (&owned.receipt.anchor_path, &owned.receipt.anchor_file) {
        (Some(anchor_path), Some(anchor_file)) => {
            owned
                .receipt
                .directory
                .verify_file_identity(anchor_path, &owned.receipt.file)
                .map_err(|error| {
                    format!("managed branch generation anchor changed; preserving the ref: {error}")
                })?;
            verify_open_file_contents(anchor_file, &owned.receipt.contents)
        }
        (None, None) => Ok(()),
        _ => Err("managed branch generation anchor receipt is incomplete".to_string()),
    }
}

fn capture_owned_branch_revision(
    owned: &OwnedBranch,
    expected_oid: &str,
    next_anchor_path: Option<&Path>,
) -> Result<OwnedBranch, String> {
    if !(40..=64).contains(&expected_oid.len())
        || !expected_oid.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("managed worktree branch revision has an invalid object ID".to_string());
    }
    if owned.expected_oid == expected_oid && validate_owned_ref_receipt(owned).is_ok() {
        return Ok(owned.clone());
    }
    let mut lock_name = owned
        .receipt
        .path
        .file_name()
        .ok_or("owned branch receipt has no filename")?
        .to_os_string();
    lock_name.push(".lock");
    let common_git_dir = owned.receipt.common_directory.path();
    let mut packed_lock = acquire_owned_ref_protocol_lock(
        &owned.receipt.common_directory,
        common_git_dir.join("packed-refs.lock"),
        owned.receipt.lock_owner.as_deref(),
        &owned.reference,
        "packed",
    )?;
    let mut target_lock = match acquire_owned_ref_protocol_lock(
        &owned.receipt.directory,
        owned.receipt.directory.path().join(lock_name),
        owned.receipt.lock_owner.as_deref(),
        &owned.reference,
        "target",
    ) {
        Ok(lock) => lock,
        Err(error) => {
            return Err(match packed_lock.release() {
                Ok(()) => error,
                Err(cleanup) => {
                    format!("{error}; exact packed-ref lock cleanup failed: {cleanup}")
                }
            });
        }
    };

    let contents = format!("{expected_oid}\n").into_bytes();
    let capture = (|| {
        let file = owned.receipt.directory.open_read(&owned.receipt.path)?;
        verify_open_file_contents(&file, &contents)?;
        owned
            .receipt
            .directory
            .verify_file_identity(&owned.receipt.path, &file)?;
        let (anchor_path, anchor_file) = match next_anchor_path {
            Some(anchor_path) => {
                if !owned.receipt.directory.path_exists(anchor_path)? {
                    owned.receipt.directory.hard_link_to(
                        &owned.receipt.path,
                        &owned.receipt.directory,
                        anchor_path,
                    )?;
                }
                let anchor_file = owned.receipt.directory.open_read(anchor_path)?;
                owned
                    .receipt
                    .directory
                    .verify_file_identity(anchor_path, &file)?;
                verify_open_file_contents(&anchor_file, &contents)?;
                (Some(anchor_path.to_path_buf()), Some(anchor_file))
            }
            None => (None, None),
        };
        Ok(OwnedBranch {
            reference: owned.reference.clone(),
            expected_oid: expected_oid.to_string(),
            receipt: Arc::new(OwnedRefReceipt {
                common_directory: owned.receipt.common_directory.try_clone()?,
                directory: owned.receipt.directory.try_clone()?,
                path: owned.receipt.path.clone(),
                file,
                anchor_path,
                anchor_file,
                lock_owner: owned.receipt.lock_owner.clone(),
                contents,
            }),
        })
    })();
    let target_cleanup = target_lock.release();
    let packed_cleanup = packed_lock.release();
    let lock_cleanup = match (target_cleanup, packed_cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(target), Ok(())) => Err(format!("target ref lock cleanup failed: {target}")),
        (Ok(()), Err(packed)) => Err(format!("packed ref lock cleanup failed: {packed}")),
        (Err(target), Err(packed)) => Err(format!(
            "target ref lock cleanup failed: {target}; packed ref lock cleanup failed: {packed}"
        )),
    };
    match (capture, lock_cleanup) {
        (Ok(owned), Ok(())) => Ok(owned),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(format!("{error}; {cleanup}")),
    }
}

fn describe_existing_owned_ref(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    reference: &str,
    fallback: &str,
) -> String {
    if let Ok(mut file) = directory.open_read(path) {
        let mut contents = Vec::with_capacity(256);
        if file.by_ref().take(257).read_to_end(&mut contents).is_ok() && contents.len() <= 256 {
            if let Ok(contents) = std::str::from_utf8(&contents) {
                if let Some(target) = contents
                    .trim_end_matches(['\r', '\n'])
                    .strip_prefix("ref: ")
                    .filter(|target| !target.is_empty())
                {
                    return format!(
                        "owned worktree branch {reference} is symbolic to {target}; preserving the symbolic ref and its referent"
                    );
                }
            }
        }
    }
    format!("owned worktree branch {reference} {fallback}; preserving it")
}

fn remove_owned_ref_receipt(owned: &OwnedBranch) -> Result<(), String> {
    validate_owned_ref_receipt(owned)?;
    remove_owned_file_receipt(
        &owned.receipt.directory,
        &owned.receipt.path,
        &owned.receipt.file,
        &owned.receipt.contents,
        ".nib-owned-ref-delete-",
    )?;
    match (&owned.receipt.anchor_path, &owned.receipt.anchor_file) {
        (Some(anchor_path), Some(anchor_file)) => remove_owned_file_receipt(
            &owned.receipt.directory,
            anchor_path,
            anchor_file,
            &owned.receipt.contents,
            ".nib-owned-ref-anchor-delete-",
        ),
        (None, None) => Ok(()),
        _ => Err("managed branch generation anchor receipt is incomplete".to_string()),
    }
}

fn owned_ref_namespace_is_absent(owned: &OwnedBranch) -> Result<bool, String> {
    if owned
        .receipt
        .directory
        .entry_kind(&owned.receipt.path)?
        .is_some()
    {
        return Ok(false);
    }
    let delete_quarantine = owned.receipt.directory.deterministic_artifact_path(
        &owned.receipt.path,
        ".nib-owned-ref-delete-",
        ".quarantine",
    )?;
    if owned.receipt.directory.path_exists(&delete_quarantine)? {
        return Err(format!(
            "owned branch ref has a persisted deletion quarantine requiring exact recovery: {}",
            delete_quarantine.display()
        ));
    }
    if let Some(anchor_path) = &owned.receipt.anchor_path {
        if owned.receipt.directory.path_exists(anchor_path)? {
            return Ok(false);
        }
        let anchor_quarantine = owned.receipt.directory.deterministic_artifact_path(
            anchor_path,
            ".nib-owned-ref-anchor-delete-",
            ".quarantine",
        )?;
        if owned.receipt.directory.path_exists(&anchor_quarantine)? {
            return Err(format!(
                "owned branch anchor has a persisted deletion quarantine requiring exact recovery: {}",
                anchor_quarantine.display()
            ));
        }
    }
    let packed_lock_path = owned
        .receipt
        .common_directory
        .path()
        .join("packed-refs.lock");
    let mut target_lock_name = owned
        .receipt
        .path
        .file_name()
        .ok_or("owned branch receipt has no filename")?
        .to_os_string();
    target_lock_name.push(".lock");
    let target_lock_path = owned.receipt.directory.path().join(target_lock_name);
    for (directory, lock_path) in [
        (&owned.receipt.common_directory, packed_lock_path),
        (&owned.receipt.directory, target_lock_path),
    ] {
        let quarantine = directory.deterministic_artifact_path(
            &lock_path,
            MANAGED_REF_LOCK_DELETE_PREFIX,
            ".quarantine",
        )?;
        let temporary = directory.deterministic_artifact_path(
            &lock_path,
            MANAGED_REF_LOCK_TEMPORARY_PREFIX,
            ".tmp",
        )?;
        if directory.path_exists(&lock_path)?
            || directory.path_exists(&quarantine)?
            || directory.path_exists(&temporary)?
        {
            return Ok(false);
        }
    }
    Ok(packed_ref_namespace_conflict(&owned.receipt.common_directory, &owned.reference)?.is_none())
}

fn delete_owned_ref_with_receipt(owned: &OwnedBranch, deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("owned branch cleanup deadline elapsed".to_string());
    }
    let mut lock_name = owned
        .receipt
        .path
        .file_name()
        .ok_or("owned branch receipt has no filename")?
        .to_os_string();
    lock_name.push(".lock");
    let common_git_dir = owned.receipt.common_directory.path();
    let mut packed_lock = acquire_owned_ref_protocol_lock(
        &owned.receipt.common_directory,
        common_git_dir.join("packed-refs.lock"),
        owned.receipt.lock_owner.as_deref(),
        &owned.reference,
        "packed",
    )?;
    let mut target_lock = match acquire_owned_ref_protocol_lock(
        &owned.receipt.directory,
        owned.receipt.directory.path().join(lock_name),
        owned.receipt.lock_owner.as_deref(),
        &owned.reference,
        "target",
    ) {
        Ok(lock) => lock,
        Err(error) => {
            return Err(match packed_lock.release() {
                Ok(()) => error,
                Err(cleanup) => {
                    format!("{error}; exact packed-ref lock cleanup failed: {cleanup}")
                }
            });
        }
    };
    let removal = if Instant::now() >= deadline {
        Err("owned branch cleanup deadline elapsed".to_string())
    } else {
        match packed_ref_namespace_conflict(&owned.receipt.common_directory, &owned.reference) {
            Ok(Some(conflict)) => Err(format!(
                "owned worktree branch {} conflicts with packed ref {conflict}; loose ref and generation anchor were preserved",
                owned.reference
            )),
            Ok(None) => remove_owned_ref_receipt(owned),
            Err(error) => Err(error),
        }
    };
    let target_cleanup = target_lock.release();
    let packed_cleanup = packed_lock.release();
    let mut errors = Vec::new();
    if let Err(error) = removal {
        errors.push(error);
    }
    if let Err(error) = target_cleanup {
        errors.push(format!("target ref lock cleanup failed: {error}"));
    }
    if let Err(error) = packed_cleanup {
        errors.push(format!("packed ref lock cleanup failed: {error}"));
    }
    finish_cleanup_errors(errors)?;
    if owned
        .receipt
        .directory
        .entry_kind(&owned.receipt.path)?
        .is_some()
    {
        return Err(format!(
            "owned worktree branch {} was replaced during cleanup and was preserved",
            owned.reference
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn create_owned_branch_sync(
    project_root: &Path,
    branch: &str,
) -> Result<OwnedBranch, String> {
    create_owned_branch_sync_controlled(project_root, branch, None)
}

#[cfg(test)]
pub(crate) fn create_owned_branch_sync_controlled(
    project_root: &Path,
    branch: &str,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<OwnedBranch, String> {
    let head = run_git_bounded_sync_with_timeout_controlled(
        project_root,
        ["rev-parse", "--verify", "HEAD^{commit}"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let expected_oid = parse_git_oid(&head, "resolve branch base")?;
    let common = run_git_bounded_sync_with_timeout_controlled(
        project_root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let common = parse_common_git_directory(project_root, &common)?;
    let reference = format!("refs/heads/{branch}");
    let existing = run_git_bounded_sync_with_timeout_controlled(
        project_root,
        ["show-ref", "--verify", "--quiet", reference.as_str()],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    if cancellation.is_some_and(BlockingGitCancellation::is_cancelled) {
        return Err(WORKTREE_CREATE_CANCELLED.to_string());
    }
    let mut claim = prepare_owned_ref_claim(&common, &reference, &expected_oid, None)?;
    claim_owned_ref_after_inspection(&mut claim, &existing, None)
}

pub(crate) fn delete_owned_branch_sync_with_timeout(
    _project_root: &Path,
    owned: &OwnedBranch,
    timeout: Duration,
) -> Result<(), String> {
    delete_owned_ref_with_receipt(owned, Instant::now() + timeout)
}

fn stage_reserved_branch_publication(
    reservation: &mut ManagedWorktreeReservation,
) -> Result<ReservedBranchPublication, String> {
    stage_reserved_branch_publication_with_hook(reservation, || {})
}

fn stage_reserved_branch_publication_with_hook(
    reservation: &mut ManagedWorktreeReservation,
    before_present_cas: impl FnOnce(),
) -> Result<ReservedBranchPublication, String> {
    let revision = &mut reservation.intent.revision;
    if revision.record.phase != DurableOwnershipPhase::Intent
        || revision.record.branch_cleanup != DurableArtifactPhase::Reserved
        || revision.record.branch_identity.is_some()
    {
        return Err("managed worktree branch reservation is not publishable".to_string());
    }
    let (expected_ref_path, expected_staging_path) = managed_branch_paths(
        &revision.record.common_git_dir,
        &revision.record.branch_reference,
        &revision.record.receipt_id,
        revision.record.branch_anchor_generation,
    )?;
    if revision.record.branch_staging_path != expected_staging_path
        || expected_staging_path.parent() != expected_ref_path.parent()
    {
        return Err(
            "managed worktree branch staging path does not match its reservation".to_string(),
        );
    }
    let parent = expected_ref_path
        .parent()
        .ok_or("managed worktree branch has no parent directory")?;
    crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| format!("managed worktree ref directory is unsafe: {error}"))?;
    let ref_directory = crate::daemons::state::StableDirectory::open(parent)?;
    if ref_directory.entry_kind(&expected_ref_path)?.is_some() {
        return Err(describe_existing_owned_ref(
            &ref_directory,
            &expected_ref_path,
            &revision.record.branch_reference,
            "appeared before its reserved anchor was staged",
        ));
    }
    if ref_directory.entry_kind(&expected_staging_path)?.is_some() {
        return Err(format!(
            "managed branch reservation staging entry already exists and was preserved: {}",
            expected_staging_path.display()
        ));
    }
    let contents = format!("{}\n", revision.record.initial_oid).into_bytes();
    let publication = ref_directory.save_bytes_atomically_expected_with_locked_receipt(
        &expected_staging_path,
        &contents,
        RESERVED_REF_TEMPORARY_PREFIX,
        crate::daemons::state::FileExpectation::Missing,
    );
    let publication = match publication {
        Ok(publication) => publication,
        Err(error) => {
            if let Some(receipt) = error.receipt {
                let cleanup = verify_open_file_contents(&receipt.file, &contents).and_then(|()| {
                    ref_directory.remove_visible_file_if_matches_direct(
                        &expected_staging_path,
                        &receipt.file,
                    )
                });
                return Err(match cleanup {
                    Ok(()) => error.message,
                    Err(cleanup) => format!(
                        "{}; exact reserved branch staging cleanup failed: {cleanup}",
                        error.message
                    ),
                });
            }
            return Err(error.message);
        }
    };
    if !publication.exact_identity {
        let cleanup = verify_open_file_contents(&publication.file, &contents).and_then(|()| {
            ref_directory
                .remove_visible_file_if_matches_direct(&expected_staging_path, &publication.file)
        });
        return Err(match cleanup {
            Ok(()) => "managed branch reservation requires an exact staging identity".to_string(),
            Err(cleanup) => format!(
                "managed branch reservation requires an exact staging identity; exact staging cleanup failed: {cleanup}"
            ),
        });
    }
    let identity = crate::fs_security::file_identity_snapshot(&publication.file)
        .map_err(|error| format!("failed to retain reserved branch identity: {error}"))?;
    let mut record = revision.record.clone();
    record.branch_identity = Some(identity);
    record.branch_cleanup = DurableArtifactPhase::Present;
    before_present_cas();
    if let Err(error) = persist_durable_ownership_revision(revision, record) {
        let cleanup = verify_open_file_contents(&publication.file, &contents).and_then(|()| {
            ref_directory
                .remove_visible_file_if_matches_direct(&expected_staging_path, &publication.file)
        });
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => {
                format!("{error}; exact reserved branch staging cleanup failed: {cleanup}")
            }
        });
    }
    if let Err(error) = publication.file.unlock() {
        return Err(format!(
            "failed to release reserved branch publication lock: {error}; its durable staging anchor was preserved for exact recovery"
        ));
    }
    Ok(ReservedBranchPublication {
        path: expected_staging_path,
        file: publication.file,
        contents,
    })
}

async fn create_reserved_worktree_branch(
    reservation: &mut ManagedWorktreeReservation,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<OwnedBranch, String> {
    let project_root = reservation.intent.revision.record.project_root.clone();
    let common_git_dir = reservation.intent.revision.record.common_git_dir.clone();
    let reference = reservation.intent.revision.record.branch_reference.clone();
    let expected_oid = reservation.intent.revision.record.initial_oid.clone();
    let existing = run_git_cancellable(
        &project_root,
        ["show-ref", "--verify", "--quiet", reference.as_str()],
        cancellation,
    )
    .await?;
    if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
        return Err(WORKTREE_CREATE_CANCELLED.to_string());
    }
    let staged = stage_reserved_branch_publication(reservation)?;
    if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
        return Err(WORKTREE_CREATE_CANCELLED.to_string());
    }
    let receipt_id = reservation.intent.revision.record.receipt_id.clone();
    let mut claim = prepare_owned_ref_claim(
        &common_git_dir,
        &reference,
        &expected_oid,
        Some(&receipt_id),
    )?;
    claim_owned_ref_after_inspection(&mut claim, &existing, Some(staged))
}

pub(crate) fn create_reserved_worktree_branch_sync_controlled(
    reservation: &mut ManagedWorktreeReservation,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<OwnedBranch, String> {
    let project_root = reservation.intent.revision.record.project_root.clone();
    let common_git_dir = reservation.intent.revision.record.common_git_dir.clone();
    let reference = reservation.intent.revision.record.branch_reference.clone();
    let expected_oid = reservation.intent.revision.record.initial_oid.clone();
    let existing = run_git_bounded_sync_with_timeout_controlled(
        &project_root,
        ["show-ref", "--verify", "--quiet", reference.as_str()],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    if cancellation.is_some_and(BlockingGitCancellation::is_cancelled) {
        return Err(WORKTREE_CREATE_CANCELLED.to_string());
    }
    let staged = stage_reserved_branch_publication(reservation)?;
    if cancellation.is_some_and(BlockingGitCancellation::is_cancelled) {
        return Err(WORKTREE_CREATE_CANCELLED.to_string());
    }
    let receipt_id = reservation.intent.revision.record.receipt_id.clone();
    let mut claim = prepare_owned_ref_claim(
        &common_git_dir,
        &reference,
        &expected_oid,
        Some(&receipt_id),
    )?;
    claim_owned_ref_after_inspection(&mut claim, &existing, Some(staged))
}

fn parse_git_oid(output: &Output, operation: &str) -> Result<String, String> {
    require_git_success(output, operation)?;
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !(40..=64).contains(&oid.len()) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("git {operation} returned an invalid object ID"));
    }
    Ok(oid)
}

fn parse_symbolic_head(output: &Output) -> Result<String, String> {
    require_git_success(output, "inspect created worktree symbolic HEAD")?;
    let reference = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !reference.starts_with("refs/")
        || reference.bytes().any(|byte| byte.is_ascii_control())
        || reference.contains(char::is_whitespace)
    {
        return Err(
            "git inspect created worktree symbolic HEAD returned an invalid ref".to_string(),
        );
    }
    Ok(reference)
}

fn validate_owned_worktree_oid(
    label: &str,
    actual_oid: &str,
    owned: &OwnedBranch,
) -> Result<(), String> {
    if actual_oid == owned.expected_oid {
        Ok(())
    } else {
        Err(format!(
            "created worktree {label} changed from {} to {}; refusing publication",
            owned.expected_oid, actual_oid
        ))
    }
}

pub(crate) async fn run_git_bounded<I, S>(cwd: &Path, args: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_controlled(cwd, args, GIT_COMMAND_TIMEOUT, None).await
}

async fn run_git_cancellable<I, S>(
    cwd: &Path,
    args: I,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_controlled(cwd, args, GIT_COMMAND_TIMEOUT, cancellation).await
}

async fn run_git_controlled<I, S>(
    cwd: &Path,
    args: I,
    command_timeout: Duration,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    let deadline = Instant::now() + command_timeout;
    validate_managed_git_configuration(cwd, command_timeout, cancellation).await?;
    let mut command = tokio::process::Command::new("git");
    configure_git_command(&mut command, cwd, &args);
    run_process_bounded_controlled(
        command,
        &format_git_args(&args),
        managed_git_time_remaining(deadline, command_timeout)?,
        cancellation,
    )
    .await
}

pub(crate) fn run_git_bounded_sync<I, S>(cwd: &Path, args: I) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_bounded_sync_with_timeout(cwd, args, GIT_COMMAND_TIMEOUT)
}

pub(crate) fn run_git_bounded_sync_controlled<I, S>(
    cwd: &Path,
    args: I,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_bounded_sync_with_timeout_controlled(cwd, args, GIT_COMMAND_TIMEOUT, cancellation)
}

pub(crate) fn run_git_bounded_sync_with_timeout<I, S>(
    cwd: &Path,
    args: I,
    command_timeout: Duration,
) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_bounded_sync_with_timeout_controlled(cwd, args, command_timeout, None)
}

fn run_git_bounded_sync_with_timeout_controlled<I, S>(
    cwd: &Path,
    args: I,
    command_timeout: Duration,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<Output, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    let deadline = Instant::now() + command_timeout;
    validate_managed_git_configuration_sync(cwd, command_timeout, cancellation)?;
    let mut command = Command::new("git");
    configure_git_command_sync(&mut command, cwd, &args);
    run_process_bounded_sync_controlled(
        command,
        &format_git_args(&args),
        managed_git_time_remaining(deadline, command_timeout)?,
        cancellation,
    )
}

async fn validate_managed_git_configuration(
    cwd: &Path,
    command_timeout: Duration,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<(), String> {
    let deadline = Instant::now() + command_timeout;
    let local_config_args = [
        OsString::from("config"),
        OsString::from("--local"),
        OsString::from("--no-includes"),
        OsString::from("--name-only"),
        OsString::from("--null"),
        OsString::from("--get-regexp"),
        OsString::from(EXECUTABLE_GIT_CONFIG_PATTERN),
    ];
    let mut command = tokio::process::Command::new("git");
    configure_git_command(&mut command, cwd, &local_config_args);
    let output = run_process_bounded_controlled(
        command,
        "inspect executable repository configuration",
        managed_git_time_remaining(deadline, command_timeout)?,
        cancellation,
    )
    .await?;
    validate_executable_git_config_output(&output)?;
    let worktree_config_args = [
        OsString::from("config"),
        OsString::from("--local"),
        OsString::from("--no-includes"),
        OsString::from("--type=bool"),
        OsString::from("--get"),
        OsString::from("extensions.worktreeConfig"),
    ];
    let mut command = tokio::process::Command::new("git");
    configure_git_command(&mut command, cwd, &worktree_config_args);
    let worktree_config = run_process_bounded_controlled(
        command,
        "inspect worktree configuration activation",
        managed_git_time_remaining(deadline, command_timeout)?,
        cancellation,
    )
    .await?;
    if validate_worktree_config_activation(&worktree_config)? {
        let args = [
            OsString::from("config"),
            OsString::from("--worktree"),
            OsString::from("--no-includes"),
            OsString::from("--name-only"),
            OsString::from("--null"),
            OsString::from("--get-regexp"),
            OsString::from(EXECUTABLE_GIT_CONFIG_PATTERN),
        ];
        let mut command = tokio::process::Command::new("git");
        configure_git_command(&mut command, cwd, &args);
        let output = run_process_bounded_controlled(
            command,
            "inspect executable repository configuration",
            managed_git_time_remaining(deadline, command_timeout)?,
            cancellation,
        )
        .await?;
        validate_executable_git_config_output(&output)?;
    }
    Ok(())
}

fn validate_managed_git_configuration_sync(
    cwd: &Path,
    command_timeout: Duration,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<(), String> {
    let deadline = Instant::now() + command_timeout;
    let local_config_args = [
        OsString::from("config"),
        OsString::from("--local"),
        OsString::from("--no-includes"),
        OsString::from("--name-only"),
        OsString::from("--null"),
        OsString::from("--get-regexp"),
        OsString::from(EXECUTABLE_GIT_CONFIG_PATTERN),
    ];
    let mut command = Command::new("git");
    configure_git_command_sync(&mut command, cwd, &local_config_args);
    let output = run_process_bounded_sync_controlled(
        command,
        "inspect executable repository configuration",
        managed_git_time_remaining(deadline, command_timeout)?,
        cancellation,
    )?;
    validate_executable_git_config_output(&output)?;
    let worktree_config_args = [
        OsString::from("config"),
        OsString::from("--local"),
        OsString::from("--no-includes"),
        OsString::from("--type=bool"),
        OsString::from("--get"),
        OsString::from("extensions.worktreeConfig"),
    ];
    let mut command = Command::new("git");
    configure_git_command_sync(&mut command, cwd, &worktree_config_args);
    let worktree_config = run_process_bounded_sync_controlled(
        command,
        "inspect worktree configuration activation",
        managed_git_time_remaining(deadline, command_timeout)?,
        cancellation,
    )?;
    if validate_worktree_config_activation(&worktree_config)? {
        let args = [
            OsString::from("config"),
            OsString::from("--worktree"),
            OsString::from("--no-includes"),
            OsString::from("--name-only"),
            OsString::from("--null"),
            OsString::from("--get-regexp"),
            OsString::from(EXECUTABLE_GIT_CONFIG_PATTERN),
        ];
        let mut command = Command::new("git");
        configure_git_command_sync(&mut command, cwd, &args);
        let output = run_process_bounded_sync_controlled(
            command,
            "inspect executable repository configuration",
            managed_git_time_remaining(deadline, command_timeout)?,
            cancellation,
        )?;
        validate_executable_git_config_output(&output)?;
    }
    Ok(())
}

fn validate_worktree_config_activation(output: &Output) -> Result<bool, String> {
    match output.status.code() {
        Some(1) => Ok(false),
        Some(0) => match String::from_utf8_lossy(&output.stdout).trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err("Git returned an invalid extensions.worktreeConfig value".to_string()),
        },
        _ => Err(git_failure(
            output,
            "inspect worktree configuration activation",
        )),
    }
}

fn validate_executable_git_config_output(output: &Output) -> Result<(), String> {
    match output.status.code() {
        Some(1) => Ok(()),
        Some(0) => {
            let keys = output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|key| !key.is_empty())
                .take(8)
                .map(|key| String::from_utf8_lossy(key).into_owned())
                .collect::<Vec<_>>();
            let detail = if keys.is_empty() {
                "an executable helper setting".to_string()
            } else {
                keys.join(", ")
            };
            Err(format!(
                "managed Git refuses executable repository configuration: {detail}"
            ))
        }
        _ => Err(git_failure(
            output,
            "inspect executable repository configuration",
        )),
    }
}

fn managed_git_time_remaining(deadline: Instant, timeout: Duration) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            format!(
                "managed Git command exceeded {} seconds during configuration validation",
                timeout.as_secs_f64()
            )
        })
}

fn configure_git_command(command: &mut tokio::process::Command, cwd: &Path, args: &[OsString]) {
    crate::sandbox::apply_child_environment(command, &std::collections::HashMap::new());
    command
        .current_dir(cwd)
        .args(git_policy_args())
        .args(args)
        .env_remove("HOME")
        .env_remove("USER")
        .env_remove("LOGNAME")
        .env_remove("SHELL")
        .env_remove("TERM")
        .env_remove("RUSTUP_HOME")
        .env_remove("CARGO_HOME")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", git_null_device())
        .env("GIT_CONFIG_GLOBAL", git_null_device())
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn configure_git_command_sync(command: &mut Command, cwd: &Path, args: &[OsString]) {
    crate::sandbox::apply_std_child_environment(command, &std::collections::HashMap::new());
    command
        .current_dir(cwd)
        .args(git_policy_args())
        .args(args)
        .env_remove("HOME")
        .env_remove("USER")
        .env_remove("LOGNAME")
        .env_remove("SHELL")
        .env_remove("TERM")
        .env_remove("RUSTUP_HOME")
        .env_remove("CARGO_HOME")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", git_null_device())
        .env("GIT_CONFIG_GLOBAL", git_null_device())
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GCM_INTERACTIVE", "never")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

fn git_policy_args() -> Vec<OsString> {
    vec![
        OsString::from("--no-pager"),
        OsString::from("-c"),
        OsString::from(format!("core.hooksPath={}", git_null_device())),
        OsString::from("-c"),
        OsString::from(format!("core.attributesFile={}", git_null_device())),
        OsString::from("-c"),
        OsString::from("core.fsmonitor=false"),
        OsString::from("-c"),
        OsString::from("credential.helper="),
        OsString::from("-c"),
        OsString::from("credential.interactive=false"),
        OsString::from("-c"),
        OsString::from("commit.gpgSign=false"),
        OsString::from("-c"),
        OsString::from("tag.gpgSign=false"),
        OsString::from("-c"),
        OsString::from("merge.verifySignatures=false"),
        OsString::from("-c"),
        OsString::from("protocol.ext.allow=never"),
    ]
}

#[cfg(windows)]
fn git_null_device() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn git_null_device() -> &'static str {
    "/dev/null"
}

#[cfg(test)]
async fn run_process_bounded(
    command: tokio::process::Command,
    label: &str,
    command_timeout: Duration,
) -> Result<Output, String> {
    run_process_bounded_controlled(command, label, command_timeout, None).await
}

async fn run_process_bounded_controlled(
    mut command: tokio::process::Command,
    label: &str,
    command_timeout: Duration,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<Output, String> {
    let mut child = crate::sandbox::spawn_managed_child(&mut command)
        .map_err(|error| format!("git {label} failed to start: {error}"))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            child.terminate_and_reap().await;
            return Err(format!("git {label} stdout was not captured"));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            child.terminate_and_reap().await;
            return Err(format!("git {label} stderr was not captured"));
        }
    };
    let mut operation = Box::pin(async {
        let wait = async {
            child
                .wait()
                .await
                .map_err(|error| format!("git {label} wait failed: {error}"))
        };
        let (stdout, stderr, status) = tokio::try_join!(
            drain_async_bounded(stdout),
            drain_async_bounded(stderr),
            wait
        )?;
        Ok::<_, String>(Output {
            status,
            stdout,
            stderr,
        })
    });
    let timeout = tokio::time::sleep(command_timeout);
    tokio::pin!(timeout);

    enum Outcome<T> {
        Complete(T),
        Cancelled,
        TimedOut,
    }
    let outcome = match cancellation {
        Some(cancellation) => {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Outcome::Cancelled,
                output = &mut operation => Outcome::Complete(output),
                _ = &mut timeout => Outcome::TimedOut,
            }
        }
        None => {
            tokio::select! {
                output = &mut operation => Outcome::Complete(output),
                _ = &mut timeout => Outcome::TimedOut,
            }
        }
    };
    match outcome {
        Outcome::Complete(output) => output,
        Outcome::Cancelled => {
            drop(operation);
            child.terminate_and_reap().await;
            Err(WORKTREE_CREATE_CANCELLED.to_string())
        }
        Outcome::TimedOut => {
            drop(operation);
            child.terminate_and_reap().await;
            Err(format!(
                "git {label} timed out after {} seconds",
                command_timeout.as_secs_f64()
            ))
        }
    }
}

async fn drain_async_bounded<R: AsyncRead + Unpin>(mut reader: R) -> Result<Vec<u8>, String> {
    let mut captured = Vec::with_capacity(MAX_GIT_OUTPUT_BYTES.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("failed to read git output: {error}"))?;
        if read == 0 {
            break;
        }
        let remaining = MAX_GIT_OUTPUT_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(captured)
}

struct SyncManagedChild {
    child: std::process::Child,
    #[cfg(unix)]
    process_group: Option<u32>,
    #[cfg(windows)]
    windows_job: Option<super::windows_job::WindowsJob>,
    reaped: bool,
}

#[cfg(any(unix, test))]
fn should_create_inner_process_group(is_macos: bool, managed_scope: bool) -> bool {
    !(is_macos && managed_scope)
}

#[cfg(unix)]
fn create_git_process_group() -> bool {
    should_create_inner_process_group(
        cfg!(target_os = "macos"),
        std::env::var_os("NIB_MANAGED_PROCESS_SCOPE").is_some(),
    )
}

impl SyncManagedChild {
    fn spawn(command: &mut Command) -> std::io::Result<Self> {
        #[cfg(unix)]
        let create_process_group = create_git_process_group();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            if create_process_group {
                command.process_group(0);
            }
        }
        #[cfg(windows)]
        let (child, windows_job) = super::windows_job::spawn_contained_std(command)?;
        #[cfg(not(windows))]
        let child = command.spawn()?;
        #[cfg(unix)]
        let process_group = create_process_group.then(|| child.id());
        Ok(Self {
            child,
            #[cfg(unix)]
            process_group,
            #[cfg(windows)]
            windows_job: Some(windows_job),
            reaped: false,
        })
    }

    fn poll_exit(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        #[cfg(unix)]
        if self.process_group.is_some() {
            match sync_child_has_exited_without_reaping(self.child.id()) {
                Ok(false) => return Ok(None),
                Ok(true) => {
                    self.signal_owned_process_group();
                    let status = self.child.wait()?;
                    self.reaped = true;
                    return Ok(Some(status));
                }
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    // Another wait consumed the child identity, so the cached
                    // numeric PGID is no longer an owned signalling target.
                    self.process_group.take();
                }
                Err(error) => return Err(error),
            }
        }
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn terminate_and_reap(&mut self) {
        #[cfg(unix)]
        self.signal_owned_process_group();
        #[cfg(windows)]
        if let Some(mut job) = self.windows_job.take() {
            job.terminate();
        }
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }

    #[cfg(unix)]
    fn signal_owned_process_group(&mut self) {
        if let Some(process_group) = self.process_group.take() {
            terminate_sync_process_tree(process_group);
        }
    }
}

impl Drop for SyncManagedChild {
    fn drop(&mut self) {
        self.terminate_and_reap();
    }
}

#[cfg(test)]
fn run_process_bounded_sync(
    command: Command,
    label: &str,
    command_timeout: Duration,
) -> Result<Output, String> {
    run_process_bounded_sync_controlled(command, label, command_timeout, None)
}

fn run_process_bounded_sync_controlled(
    mut command: Command,
    label: &str,
    command_timeout: Duration,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<Output, String> {
    if cancellation.is_some_and(BlockingGitCancellation::is_cancelled) {
        return Err(format!("git {label} cancelled before start"));
    }
    let mut managed = SyncManagedChild::spawn(&mut command)
        .map_err(|error| format!("git {label} failed to start: {error}"))?;
    let stdout = managed
        .child
        .stdout
        .take()
        .ok_or_else(|| format!("git {label} stdout was not captured"))?;
    let stderr = managed
        .child
        .stderr
        .take()
        .ok_or_else(|| format!("git {label} stderr was not captured"))?;
    let stdout_reader = spawn_sync_reader(stdout);
    let stderr_reader = spawn_sync_reader(stderr);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = managed
            .poll_exit()
            .map_err(|error| format!("git {label} wait failed: {error}"))?
        {
            break status;
        }
        if cancellation.is_some_and(BlockingGitCancellation::is_cancelled) {
            managed.terminate_and_reap();
            let _ = receive_sync_reader(stdout_reader, label, "stdout");
            let _ = receive_sync_reader(stderr_reader, label, "stderr");
            return Err(format!("git {label} cancelled"));
        }
        if started.elapsed() >= command_timeout {
            managed.terminate_and_reap();
            let _ = receive_sync_reader(stdout_reader, label, "stdout");
            let _ = receive_sync_reader(stderr_reader, label, "stderr");
            return Err(format!(
                "git {label} timed out after {} seconds",
                command_timeout.as_secs_f64()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    // The leader owns a fresh process group or Job Object. Descendants retaining
    // either pipe must not outlive a bounded command after the leader exits.
    managed.terminate_and_reap();
    let stdout = receive_sync_reader(stdout_reader, label, "stdout")?;
    let stderr = receive_sync_reader(stderr_reader, label, "stderr")?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn spawn_sync_reader<R>(reader: R) -> std::sync::mpsc::Receiver<Result<Vec<u8>, String>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(drain_sync_bounded(reader));
    });
    receiver
}

fn receive_sync_reader(
    receiver: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    label: &str,
    stream: &str,
) -> Result<Vec<u8>, String> {
    receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|error| format!("git {label} {stream} reader did not finish: {error}"))?
}

fn drain_sync_bounded<R: Read>(mut reader: R) -> Result<Vec<u8>, String> {
    let mut captured = Vec::with_capacity(MAX_GIT_OUTPUT_BYTES.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("failed to read git output: {error}"))?;
        if read == 0 {
            break;
        }
        let remaining = MAX_GIT_OUTPUT_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(captured)
}

#[cfg(unix)]
fn terminate_sync_process_tree(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            let _ = kill(-pid, 9);
        }
    }
}

#[cfg(unix)]
fn sync_child_has_exited_without_reaping(pid: u32) -> std::io::Result<bool> {
    let pid = i32::try_from(pid)
        .map_err(|_| std::io::Error::other("git child process identifier exceeds pid_t"))?;
    loop {
        // SAFETY: a zeroed siginfo_t is a valid output buffer for waitid.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: P_PID restricts observation to the exact child. WNOWAIT pins
        // its PID until the owned process group is signalled and Child::wait
        // performs the final reap.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: waitid initialized siginfo_t, and zero means no exited
            // child was available for this WNOHANG observation.
            let observed_pid = unsafe { info.si_pid() };
            if observed_pid == 0 {
                return Ok(false);
            }
            if observed_pid == pid {
                return Ok(true);
            }
            return Err(std::io::Error::other(
                "waitid returned an unexpected git child process identifier",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
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

fn format_git_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn repository_root(project_root: &Path) -> Result<PathBuf, String> {
    repository_root_bounded_sync(project_root)
}

fn validate_created_worktree(path: &Path, owned_branch: &OwnedBranch) -> Result<(), String> {
    #[cfg(test)]
    {
        let id = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if let Some(replacement) = SYNC_POST_ADD_BRANCH_MOVES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            let reference = format!("refs/heads/{}", branch_name(id));
            let moved = Command::new("git")
                .current_dir(path)
                .args(["update-ref", reference.as_str(), replacement.as_str()])
                .status()
                .expect("move subagent branch fixture");
            assert!(moved.success(), "move subagent branch fixture");
        }
        if let Some(target) = SYNC_POST_ADD_BRANCH_SYMREFS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            let reference = format!("refs/heads/{}", branch_name(id));
            let replaced = Command::new("git")
                .current_dir(path)
                .args(["symbolic-ref", reference.as_str(), target.as_str()])
                .status()
                .expect("replace subagent branch with symref fixture");
            assert!(
                replaced.success(),
                "replace subagent branch with symref fixture"
            );
        }
    }
    let output = run_git_bounded_sync(path, ["rev-parse", "--show-toplevel"])?;
    require_git_success(&output, "inspect created worktree")?;
    let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .canonicalize()
        .map_err(|error| format!("failed to resolve created git worktree: {error}"))?;
    if reported != path {
        return Err(format!(
            "created git worktree root {} does not match {}",
            reported.display(),
            path.display()
        ));
    }
    validate_owned_worktree_sync(path, owned_branch)?;
    #[cfg(test)]
    {
        let id = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        let mut failures = SYNC_POST_ADD_VALIDATION_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if failures.remove(id) {
            return Err("injected post-add worktree validation failure".to_string());
        }
    }
    Ok(())
}

pub(crate) fn validate_owned_worktree_sync(path: &Path, owned: &OwnedBranch) -> Result<(), String> {
    validate_owned_worktree_sync_controlled(path, owned, None)
}

pub(crate) fn validate_owned_worktree_sync_controlled(
    path: &Path,
    owned: &OwnedBranch,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<(), String> {
    let symbolic = run_git_bounded_sync_with_timeout_controlled(
        path,
        ["symbolic-ref", "--quiet", "HEAD"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let symbolic = parse_symbolic_head(&symbolic)?;
    if symbolic != owned.reference {
        return Err(format!(
            "created worktree symbolic HEAD {symbolic} does not match owned branch {}",
            owned.reference
        ));
    }

    let head = run_git_bounded_sync_with_timeout_controlled(
        path,
        ["rev-parse", "--verify", "HEAD^{commit}"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let head = parse_git_oid(&head, "inspect created worktree HEAD")?;
    validate_owned_worktree_oid("HEAD", &head, owned)?;

    validate_owned_ref_receipt(owned)?;
    let reference = run_git_bounded_sync_with_timeout_controlled(
        path,
        ["show-ref", "--hash", "--verify", owned.reference.as_str()],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let reference = parse_git_oid(&reference, "inspect created worktree branch")?;
    validate_owned_worktree_oid(&owned.reference, &reference, owned)?;
    validate_owned_ref_receipt(owned)
}

fn branch_name(id: &str) -> String {
    format!("nib/subagent/{id}")
}

fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "subagent".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(windows)]
    const SYNC_JOB_ROLE: &str = "NIB_SYNC_WORKTREE_JOB_ROLE";
    #[cfg(windows)]
    const SYNC_JOB_PID_PATH: &str = "NIB_SYNC_WORKTREE_JOB_PID_PATH";
    #[cfg(windows)]
    const SYNC_JOB_TEST: &str =
        "sandbox::worktree::tests::sync_bounded_timeout_kills_windows_descendant_tree";

    #[cfg(any(unix, windows))]
    const SYNC_CONTROL_ROLE: &str = "NIB_SYNC_MANAGED_CONTROL_ROLE";
    #[cfg(any(unix, windows))]
    const SYNC_CONTROL_PID_PATH: &str = "NIB_SYNC_MANAGED_CONTROL_PID_PATH";
    #[cfg(any(unix, windows))]
    const SYNC_CANCEL_TEST: &str =
        "sandbox::worktree::tests::sync_managed_cancellation_reaps_descendant_tree";
    #[cfg(any(unix, windows))]
    const SYNC_DROP_TEST: &str =
        "sandbox::worktree::tests::dropping_sync_managed_child_reaps_descendant_tree";
    const OWNERSHIP_LOCK_ROLE: &str = "NIB_WORKTREE_OWNERSHIP_LOCK_ROLE";
    const OWNERSHIP_LOCK_DIRECTORY: &str = "NIB_WORKTREE_OWNERSHIP_LOCK_DIRECTORY";
    const OWNERSHIP_LOCK_READY: &str = "NIB_WORKTREE_OWNERSHIP_LOCK_READY";
    const OWNERSHIP_LOCK_TEST: &str =
        "sandbox::worktree::tests::ownership_compaction_lock_recovers_after_holder_exit";
    const RESTART_CRASH_ROLE: &str = "NIB_WORKTREE_RESTART_CRASH_ROLE";
    const RESTART_CRASH_MODE: &str = "NIB_WORKTREE_RESTART_CRASH_MODE";
    const RESTART_CRASH_COMMON: &str = "NIB_WORKTREE_RESTART_CRASH_COMMON";
    const RESTART_CRASH_REF_DIRECTORY: &str = "NIB_WORKTREE_RESTART_CRASH_REF_DIRECTORY";
    const RESTART_CRASH_REF_PATH: &str = "NIB_WORKTREE_RESTART_CRASH_REF_PATH";
    const RESTART_CRASH_ANCHOR_PATH: &str = "NIB_WORKTREE_RESTART_CRASH_ANCHOR_PATH";
    const RESTART_CRASH_RECEIPT: &str = "NIB_WORKTREE_RESTART_CRASH_RECEIPT";
    const RESTART_CRASH_REFERENCE: &str = "NIB_WORKTREE_RESTART_CRASH_REFERENCE";
    const RESTART_CRASH_OID: &str = "NIB_WORKTREE_RESTART_CRASH_OID";
    const RESTART_CRASH_OWNERSHIP_DIRECTORY: &str =
        "NIB_WORKTREE_RESTART_CRASH_OWNERSHIP_DIRECTORY";
    const RESTART_CRASH_OWNERSHIP_PATH: &str = "NIB_WORKTREE_RESTART_CRASH_OWNERSHIP_PATH";
    const RESTART_CRASH_OWNERSHIP_NEW_BYTES: &str =
        "NIB_WORKTREE_RESTART_CRASH_OWNERSHIP_NEW_BYTES";
    const RESTART_CRASH_READY: &str = "NIB_WORKTREE_RESTART_CRASH_READY";

    #[cfg(unix)]
    async fn wait_for_pid(path: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(path) {
                    if let Ok(pid) = value.parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant pid is recorded")
    }

    #[cfg(unix)]
    fn process_is_alive(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("kill probe")
            .success()
    }

    #[cfg(unix)]
    async fn assert_process_terminated(pid: u32) {
        let terminated = tokio::time::timeout(Duration::from_secs(2), async {
            while process_is_alive(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(terminated.is_ok(), "descendant {pid} remained alive");
    }

    #[cfg(any(unix, windows))]
    fn wait_for_pid_sync(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(value) = std::fs::read_to_string(path) {
                if let Ok(pid) = value.parse::<u32>() {
                    return pid;
                }
            }
            assert!(Instant::now() < deadline, "descendant pid was not recorded");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn assert_process_terminated_sync(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_alive(pid) {
            assert!(Instant::now() < deadline, "descendant {pid} remained alive");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    fn assert_process_terminated_sync(pid: u32) {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };

        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            return;
        }
        let wait = unsafe { WaitForSingleObject(process, 5_000) };
        unsafe {
            let _ = CloseHandle(process);
        }
        assert_eq!(wait, WAIT_OBJECT_0, "descendant {pid} remained alive");
    }

    #[cfg(any(unix, windows))]
    fn run_sync_control_fixture(test_name: &str) -> bool {
        match std::env::var(SYNC_CONTROL_ROLE).as_deref() {
            Ok("leader") => {
                let mut descendant = Command::new(
                    std::env::current_exe().expect("current worktree test executable"),
                );
                descendant
                    .args(["--exact", test_name, "--nocapture"])
                    .env(SYNC_CONTROL_ROLE, "descendant")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let mut descendant = descendant.spawn().expect("spawn managed descendant");
                std::fs::write(
                    std::env::var_os(SYNC_CONTROL_PID_PATH).expect("managed descendant pid path"),
                    descendant.id().to_string(),
                )
                .expect("write managed descendant pid");
                let _ = descendant.wait();
                true
            }
            Ok("descendant") => loop {
                std::thread::sleep(Duration::from_secs(60));
            },
            _ => false,
        }
    }

    #[cfg(any(unix, windows))]
    fn sync_control_command(test_name: &str, pid_path: &Path) -> Command {
        let mut command =
            Command::new(std::env::current_exe().expect("current worktree test executable"));
        command
            .args(["--exact", test_name, "--nocapture"])
            .env(SYNC_CONTROL_ROLE, "leader")
            .env(SYNC_CONTROL_PID_PATH, pid_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run_restart_crash_fixture() -> bool {
        if std::env::var(RESTART_CRASH_ROLE).as_deref() != Ok("holder") {
            return false;
        }
        let mode = std::env::var(RESTART_CRASH_MODE).expect("restart crash mode");
        let ready =
            PathBuf::from(std::env::var_os(RESTART_CRASH_READY).expect("restart crash ready path"));
        match mode.as_str() {
            "packed-quarantine" | "target-lock" => {
                let common_path = PathBuf::from(
                    std::env::var_os(RESTART_CRASH_COMMON).expect("restart common directory"),
                );
                let ref_directory_path = PathBuf::from(
                    std::env::var_os(RESTART_CRASH_REF_DIRECTORY).expect("restart ref directory"),
                );
                let ref_path = PathBuf::from(
                    std::env::var_os(RESTART_CRASH_REF_PATH).expect("restart ref path"),
                );
                let receipt = std::env::var(RESTART_CRASH_RECEIPT).expect("restart receipt");
                let reference = std::env::var(RESTART_CRASH_REFERENCE).expect("restart reference");
                let common = crate::daemons::state::StableDirectory::open(&common_path)
                    .expect("restart common directory");
                let ref_directory =
                    crate::daemons::state::StableDirectory::open(&ref_directory_path)
                        .expect("restart ref directory");
                let packed_path = common.path().join("packed-refs.lock");
                let _packed = OwnedRefLock::acquire_with_contents(
                    &common,
                    packed_path.clone(),
                    managed_ref_lock_contents(&receipt, &reference, "packed"),
                )
                .expect("restart packed lock");
                let mut target_lock_name = ref_path.file_name().expect("ref leaf").to_os_string();
                target_lock_name.push(".lock");
                let target_lock_path = ref_directory.path().join(target_lock_name);
                let _target = (mode == "target-lock")
                    .then(|| {
                        OwnedRefLock::acquire_with_contents(
                            &ref_directory,
                            target_lock_path,
                            managed_ref_lock_contents(&receipt, &reference, "target"),
                        )
                    })
                    .transpose()
                    .expect("restart target lock");
                if mode == "packed-quarantine" {
                    let quarantine = common
                        .deterministic_artifact_path(
                            &packed_path,
                            MANAGED_REF_LOCK_DELETE_PREFIX,
                            ".quarantine",
                        )
                        .expect("packed lock quarantine");
                    std::fs::rename(&packed_path, &quarantine)
                        .expect("quarantine packed lock fixture");
                    let anchor_path = PathBuf::from(
                        std::env::var_os(RESTART_CRASH_ANCHOR_PATH).expect("restart anchor path"),
                    );
                    let temporary = ref_directory
                        .deterministic_artifact_path(
                            &anchor_path,
                            RESERVED_REF_TEMPORARY_PREFIX,
                            ".tmp",
                        )
                        .expect("reserved ref temporary path");
                    let mut temporary_file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(&temporary)
                        .expect("reserved ref temporary file");
                    use std::io::Write as _;
                    temporary_file
                        .write_all(
                            format!(
                                "{}\n",
                                std::env::var(RESTART_CRASH_OID).expect("restart oid")
                            )
                            .as_bytes(),
                        )
                        .expect("reserved ref temporary contents");
                    temporary_file.sync_all().expect("sync ref temporary");
                    temporary_file.lock().expect("lock ref temporary");
                    std::mem::forget(temporary_file);
                }
                std::mem::forget(_packed);
                if let Some(target) = _target {
                    std::mem::forget(target);
                }
            }
            "ownership-evacuated" | "ownership-committed" => {
                let directory_path = PathBuf::from(
                    std::env::var_os(RESTART_CRASH_OWNERSHIP_DIRECTORY)
                        .expect("ownership directory"),
                );
                let target = PathBuf::from(
                    std::env::var_os(RESTART_CRASH_OWNERSHIP_PATH).expect("ownership target"),
                );
                let directory = crate::daemons::state::StableDirectory::open(&directory_path)
                    .expect("stable ownership directory");
                let temporary = directory
                    .deterministic_artifact_path(
                        &target,
                        MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
                        ".tmp",
                    )
                    .expect("ownership temporary path");
                let previous = directory
                    .deterministic_previous_artifact_path(
                        &target,
                        MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
                    )
                    .expect("ownership previous path");
                let contents = std::fs::read(
                    std::env::var_os(RESTART_CRASH_OWNERSHIP_NEW_BYTES)
                        .map(PathBuf::from)
                        .unwrap_or_else(|| target.clone()),
                )
                .expect("ownership contents");
                let mut temporary_file = directory
                    .open_read_write_create(&temporary)
                    .expect("ownership temporary file");
                use std::io::Write as _;
                temporary_file
                    .write_all(&contents)
                    .expect("ownership temporary contents");
                temporary_file.sync_all().expect("sync ownership temporary");
                temporary_file.lock().expect("lock ownership temporary");
                let previous_file = directory
                    .open_read_write(&target)
                    .expect("ownership target file");
                previous_file.lock().expect("lock ownership previous");
                std::fs::rename(&target, &previous).expect("evacuate ownership target");
                if mode == "ownership-committed" {
                    std::fs::rename(&temporary, &target).expect("publish ownership target");
                }
                directory.sync_directory().expect("sync crash fixture");
                std::mem::forget(temporary_file);
                std::mem::forget(previous_file);
            }
            other => panic!("unknown restart crash mode {other}"),
        }
        std::fs::write(ready, b"ready").expect("publish restart crash readiness");
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    fn spawn_and_kill_restart_fixture(test_name: &str, configure: impl FnOnce(&mut Command)) {
        let ready = tempfile::NamedTempFile::new()
            .expect("restart ready fixture")
            .into_temp_path();
        std::fs::remove_file(&ready).expect("remove initial ready fixture");
        let mut child = Command::new(std::env::current_exe().expect("current test executable"));
        child
            .args(["--exact", test_name, "--nocapture"])
            .env(RESTART_CRASH_ROLE, "holder")
            .env(RESTART_CRASH_READY, &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure(&mut child);
        let mut child = child.spawn().expect("spawn restart crash holder");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "restart crash holder was not ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().expect("kill restart crash holder");
        child.wait().expect("reap restart crash holder");
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempdir().expect("repository");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "nib@example.invalid"],
            vec!["config", "user.name", "nib"],
        ] {
            assert!(Command::new("git")
                .current_dir(directory.path())
                .args(args)
                .status()
                .expect("git fixture")
                .success());
        }
        std::fs::write(directory.path().join("README.md"), "fixture\n").expect("fixture file");
        assert!(Command::new("git")
            .current_dir(directory.path())
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .current_dir(directory.path())
            .args(["commit", "-qm", "fixture"])
            .status()
            .unwrap()
            .success());
        directory
    }

    #[cfg(windows)]
    fn windows_path_without_verbatim_prefix(path: &Path) -> PathBuf {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        const BACKSLASH: u16 = b'\\' as u16;
        const QUESTION: u16 = b'?' as u16;
        const COLON: u16 = b':' as u16;
        let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if !encoded.starts_with(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]) {
            return path.to_path_buf();
        }
        let remainder = &encoded[4..];
        let ascii_eq = |value: u16, expected: u8| {
            value == u16::from(expected) || value == u16::from(expected.to_ascii_lowercase())
        };
        let normalized = if remainder.len() >= 4
            && ascii_eq(remainder[0], b'U')
            && ascii_eq(remainder[1], b'N')
            && ascii_eq(remainder[2], b'C')
            && remainder[3] == BACKSLASH
        {
            let mut normalized = vec![BACKSLASH, BACKSLASH];
            normalized.extend_from_slice(&remainder[4..]);
            normalized
        } else if remainder.get(1) == Some(&COLON) {
            remainder.to_vec()
        } else {
            return path.to_path_buf();
        };
        PathBuf::from(OsString::from_wide(&normalized))
    }

    #[cfg(windows)]
    fn windows_dos_short_path(path: &Path) -> PathBuf {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

        let path = windows_path_without_verbatim_prefix(path);
        let mut input = path.as_os_str().encode_wide().collect::<Vec<_>>();
        input.push(0);
        let required = unsafe { GetShortPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
        assert_ne!(
            required,
            0,
            "failed to size the DOS short-path buffer: {}",
            std::io::Error::last_os_error()
        );
        let mut output = vec![0_u16; required as usize];
        let written = unsafe { GetShortPathNameW(input.as_ptr(), output.as_mut_ptr(), required) };
        assert_ne!(
            written,
            0,
            "failed to resolve the DOS short path: {}",
            std::io::Error::last_os_error()
        );
        assert!(
            (written as usize) < output.len(),
            "DOS short-path output exceeded its sized buffer"
        );
        output.truncate(written as usize);
        PathBuf::from(OsString::from_wide(&output))
    }

    #[cfg(windows)]
    #[test]
    fn durable_reservation_canonicalizes_a_dos_short_project_root() {
        let repository = repository();
        let canonical_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let short_root = windows_dos_short_path(&canonical_root);
        if short_root == windows_path_without_verbatim_prefix(&canonical_root) {
            return;
        }

        let id = "dos-short-reservation";
        let relative_path = Path::new(".nib/worktrees/subagents").join(id);
        let short_worktree_path = short_root.join(&relative_path);
        crate::fs_security::ensure_directory_without_symlinks(
            short_worktree_path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let reservation = reserve_managed_worktree_sync_controlled(
            &short_root,
            ManagedWorktreeKind::Subagent,
            id,
            &short_worktree_path,
            &branch_name(id),
            None,
        )
        .expect("reserve through DOS short project root");
        let record = reservation.intent.revision.record.clone();
        let canonical_worktree_path = canonical_root.join(relative_path);

        assert_eq!(record.project_root, canonical_root);
        assert_eq!(record.worktree_path, canonical_worktree_path);
        assert_eq!(
            record.worktree_staging_path.parent(),
            canonical_worktree_path.parent()
        );
        assert!(record.common_git_dir.starts_with(&record.project_root));
        assert!(
            reservation
                .intent
                .revision
                .path
                .starts_with(&record.project_root),
            "durable ownership path retained the DOS short root"
        );
        validate_durable_ownership_record(&record, &short_root, ManagedWorktreeKind::Subagent, id)
            .expect("validate canonical ownership through DOS short root");
        drop(reservation);

        let reloaded =
            load_durable_ownership_revision(&canonical_root, ManagedWorktreeKind::Subagent, id)
                .expect("reload reservation through canonical root")
                .expect("durable reservation");
        assert_eq!(reloaded.record.project_root, record.project_root);
        assert_eq!(reloaded.record.worktree_path, record.worktree_path);
        drop(reloaded);

        Worktree::remove(&canonical_root, id)
            .expect("clean short-root reservation through canonical root");
        let tombstone =
            load_durable_ownership_revision(&short_root, ManagedWorktreeKind::Subagent, id)
                .expect("reload cleanup through DOS short root")
                .expect("durable cleanup tombstone");
        assert_eq!(tombstone.record.phase, DurableOwnershipPhase::Complete);
        assert!(!canonical_worktree_path.exists());

        let created_id = "dos-short-create";
        let created = Worktree::create(&short_root, created_id)
            .expect("create registered worktree through DOS short project root");
        assert_eq!(
            created.path,
            canonical_root
                .join(".nib/worktrees/subagents")
                .join(created_id)
        );
        assert!(created.path.join(".git").is_file());
        Worktree::remove(&short_root, created_id)
            .expect("remove registered worktree through DOS short project root");
        let created_tombstone = load_durable_ownership_revision(
            &canonical_root,
            ManagedWorktreeKind::Subagent,
            created_id,
        )
        .expect("reload created ownership through canonical root")
        .expect("created cleanup tombstone");
        assert_eq!(
            created_tombstone.record.phase,
            DurableOwnershipPhase::Complete
        );
        assert!(!created.path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn managed_worktree_create_adapts_a_verbatim_destination_for_git() {
        let repository = repository();
        let canonical_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let id = "windows-verbatim-git-argument";

        let worktree = Worktree::create(&canonical_root, id)
            .expect("Git accepts the adapted canonical worktree destination");

        assert_eq!(
            worktree.path,
            canonical_root.join(".nib/worktrees/subagents").join(id)
        );
        assert!(worktree.path.join(".git").is_file());
        Worktree::remove(&canonical_root, id).expect("remove adapted worktree");
        assert!(!worktree.path.exists());
    }

    fn git_stdout(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repository)
            .args(args)
            .output()
            .expect("git fixture command");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn forget_subagent_ownership(repository: &Path, id: &str) {
        let key = (
            repository.canonicalize().expect("canonical repository"),
            sanitize_component(id),
        );
        WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
    }

    fn leave_stale_registration_for_path(repository: &Path, path: &Path) -> PathBuf {
        let parent = path.parent().expect("stale worktree parent");
        std::fs::create_dir_all(parent).expect("stale worktree parent fixture");
        let output = Command::new("git")
            .current_dir(repository)
            .args(["worktree", "add", "--detach"])
            .arg(path)
            .arg("HEAD")
            .output()
            .expect("create stale worktree fixture");
        assert!(
            output.status.success(),
            "stale worktree fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let registration = parse_gitdir_pointer(
            &std::fs::read(path.join(".git")).expect("stale worktree pointer"),
            "stale worktree pointer",
        )
        .expect("stale registration path");
        std::fs::write(registration.join("nib-preserve-sentinel"), b"foreign")
            .expect("stale registration sentinel");
        std::fs::remove_dir_all(path).expect("remove stale worktree path only");
        assert!(registration.is_dir(), "stale registration remains");
        registration
    }

    fn replacement_commit(repository: &Path) -> String {
        let initial = git_stdout(repository, &["rev-parse", "HEAD"]);
        std::fs::write(repository.join("replacement.txt"), "replacement\n")
            .expect("replacement file");
        git_stdout(repository, &["add", "replacement.txt"]);
        git_stdout(repository, &["commit", "-m", "replacement fixture"]);
        let replacement = git_stdout(repository, &["rev-parse", "HEAD"]);
        git_stdout(repository, &["reset", "--hard", &initial]);
        replacement
    }

    fn assert_create_rejects_packed_ref_namespace_conflict(
        repository: &Path,
        id: &str,
        packed_reference: &str,
    ) {
        let requested = format!("refs/heads/{}", branch_name(id));
        let original = git_stdout(repository, &["rev-parse", "HEAD"]);
        git_stdout(repository, &["update-ref", packed_reference, &original]);
        git_stdout(repository, &["pack-refs", "--all", "--prune"]);

        let git_directory = repository.join(".git");
        assert!(
            !git_directory.join(packed_reference).exists(),
            "packed fixture retained a loose ref"
        );
        let packed_path = git_directory.join("packed-refs");
        let packed_before = std::fs::read(&packed_path).expect("packed-refs fixture");

        let error = Worktree::create(repository, id)
            .expect_err("packed ref namespace conflict must fail closed");

        assert!(
            error.contains(&format!(
                "managed worktree branch {requested} conflicts with packed ref {packed_reference}; preserving it"
            )),
            "{error}"
        );
        assert_eq!(
            git_stdout(
                repository,
                &["show-ref", "--hash", "--verify", packed_reference]
            ),
            original
        );
        assert_eq!(
            std::fs::read(&packed_path).expect("preserved packed-refs"),
            packed_before
        );
        assert!(
            !git_directory.join(&requested).exists(),
            "managed branch published a loose ref despite the packed conflict"
        );
        assert!(
            !git_directory.join("packed-refs.lock").exists(),
            "packed-refs lock was not released"
        );
        assert!(
            !git_directory.join(format!("{requested}.lock")).exists(),
            "managed branch lock was not released"
        );
        assert!(!repository
            .join(".nib/worktrees/subagents")
            .join(id)
            .exists());
    }

    #[test]
    fn managed_git_commands_disable_external_configuration_sources() {
        let mut command = Command::new("git");
        configure_git_command_sync(&mut command, Path::new("."), &[OsString::from("status")]);
        let environment = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            environment.get("GIT_CONFIG_NOSYSTEM").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            environment.get("GIT_CONFIG_SYSTEM").map(String::as_str),
            Some(git_null_device())
        );
        assert_eq!(
            environment.get("GIT_CONFIG_GLOBAL").map(String::as_str),
            Some(git_null_device())
        );
        assert_eq!(
            environment.get("GIT_ATTR_NOSYSTEM").map(String::as_str),
            Some("1")
        );
        for inherited in ["HOME", "RUSTUP_HOME", "CARGO_HOME"] {
            assert!(
                !environment.contains_key(inherited),
                "managed Git inherited {inherited}"
            );
        }
        assert!(!environment.contains_key("GIT_PAGER"));
        assert!(!environment.contains_key("PAGER"));

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(arguments.first().map(String::as_str), Some("--no-pager"));
        assert!(arguments.contains(&format!("core.hooksPath={}", git_null_device())));
        assert!(arguments.contains(&format!("core.attributesFile={}", git_null_device())));
        assert!(arguments.contains(&"core.fsmonitor=false".to_string()));
        assert!(arguments.contains(&"credential.helper=".to_string()));
        assert!(arguments.contains(&"protocol.ext.allow=never".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn managed_git_rejects_executable_repository_helpers_without_running_them() {
        use std::os::unix::fs::PermissionsExt;

        let repository = repository();
        let marker = repository.path().join("helper-ran");
        let helper = repository.path().join("hostile-helper.sh");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nprintf ran > '{}'\ncat\n", marker.display()),
        )
        .expect("helper script");
        let mut permissions = std::fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&helper, permissions).expect("helper permissions");
        let helper_command = helper.to_string_lossy().into_owned();
        for key in [
            "filter.hostile.smudge",
            "diff.hostile.textconv",
            "merge.hostile.driver",
            "credential.https://example.invalid.helper",
            "core.sshCommand",
        ] {
            let configured = Command::new("git")
                .current_dir(repository.path())
                .args(["config", key, helper_command.as_str()])
                .status()
                .expect("hostile repository config");
            assert!(configured.success());
        }
        for (key, value) in [
            (
                "include.path",
                repository.path().join("missing-include.config"),
            ),
            (
                "includeIf.gitdir:/tmp/.path",
                repository.path().join("missing-conditional-include.config"),
            ),
        ] {
            let configured = Command::new("git")
                .current_dir(repository.path())
                .args(["config", key, value.to_string_lossy().as_ref()])
                .status()
                .expect("hostile repository include config");
            assert!(configured.success());
        }
        std::fs::write(
            repository.path().join(".gitattributes"),
            "README.md filter=hostile diff=hostile merge=hostile\n",
        )
        .expect("attributes fixture");
        git_stdout(repository.path(), &["add", ".gitattributes"]);
        git_stdout(
            repository.path(),
            &["commit", "-m", "hostile attributes fixture"],
        );

        let error = Worktree::create(repository.path(), "hostile-config")
            .expect_err("executable repository config must fail closed");

        assert!(
            error.contains("executable repository configuration"),
            "{error}"
        );
        assert!(error.contains("filter.hostile.smudge"), "{error}");
        assert!(
            error.contains("credential.https://example.invalid.helper"),
            "{error}"
        );
        assert!(error.contains("core.sshcommand"), "{error}");
        assert!(error.contains("include.path"), "{error}");
        assert!(error.contains("includeif.gitdir:/tmp/.path"), "{error}");
        assert!(!marker.exists(), "repository helper executed");
        assert!(!repository
            .path()
            .join(".nib/worktrees/subagents/hostile-config")
            .exists());
        let reference = "refs/heads/nib/subagent/hostile-config";
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", reference])
            .status()
            .expect("hostile branch lookup");
        assert_eq!(branch.code(), Some(1), "hostile branch was created");
    }

    #[cfg(unix)]
    #[test]
    fn managed_git_rejects_executable_worktree_configuration_without_running_it() {
        use std::os::unix::fs::PermissionsExt;

        let repository = repository();
        std::fs::write(
            repository.path().join(".gitattributes"),
            "README.md filter=worktree-hostile\n",
        )
        .expect("attributes fixture");
        git_stdout(repository.path(), &["add", ".gitattributes"]);
        git_stdout(
            repository.path(),
            &["commit", "-m", "worktree config fixture"],
        );
        let marker = repository.path().join("worktree-helper-ran");
        let helper = repository.path().join("worktree-helper.sh");
        std::fs::write(
            &helper,
            format!("#!/bin/sh\nprintf ran > '{}'\ncat\n", marker.display()),
        )
        .expect("worktree helper");
        let mut permissions = std::fs::metadata(&helper)
            .expect("worktree helper metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&helper, permissions).expect("worktree helper permissions");
        git_stdout(
            repository.path(),
            &["config", "extensions.worktreeConfig", "true"],
        );
        let helper_command = helper.to_string_lossy().into_owned();
        git_stdout(
            repository.path(),
            &[
                "config",
                "--worktree",
                "filter.worktree-hostile.smudge",
                &helper_command,
            ],
        );

        let error = Worktree::create(repository.path(), "hostile-worktree-config")
            .expect_err("worktree-scoped executable config must fail closed");

        assert!(
            error.contains("executable repository configuration"),
            "{error}"
        );
        assert!(error.contains("filter.worktree-hostile.smudge"), "{error}");
        assert!(!marker.exists(), "worktree-scoped filter executed");
        assert!(!repository
            .path()
            .join(".nib/worktrees/subagents/hostile-worktree-config")
            .exists());
    }

    #[test]
    fn create_preserves_a_preexisting_branch_ref() {
        let repository = repository();
        let reference = "refs/heads/nib/subagent/preexisting";
        let original = git_stdout(repository.path(), &["rev-parse", "HEAD"]);
        git_stdout(repository.path(), &["update-ref", reference, &original]);

        let error = Worktree::create(repository.path(), "preexisting")
            .expect_err("preexisting branch claim must fail");

        assert!(error.contains("already has a loose ref"), "{error}");
        assert_eq!(
            git_stdout(
                repository.path(),
                &["show-ref", "--hash", "--verify", reference]
            ),
            original
        );
        assert!(!repository
            .path()
            .join(".nib/worktrees/subagents/preexisting")
            .exists());
    }

    #[test]
    fn create_rejects_and_preserves_an_exact_packed_branch_ref() {
        let repository = repository();
        let id = "packed-exact";
        let reference = format!("refs/heads/{}", branch_name(id));

        assert_create_rejects_packed_ref_namespace_conflict(repository.path(), id, &reference);
    }

    #[test]
    fn create_rejects_and_preserves_a_packed_branch_ancestor() {
        let repository = repository();

        assert_create_rejects_packed_ref_namespace_conflict(
            repository.path(),
            "packed-ancestor",
            "refs/heads/nib/subagent",
        );
    }

    #[test]
    fn create_rejects_and_preserves_a_packed_branch_descendant() {
        let repository = repository();
        let id = "packed-descendant";
        let descendant = format!("refs/heads/{}/child", branch_name(id));

        assert_create_rejects_packed_ref_namespace_conflict(repository.path(), id, &descendant);
    }

    #[test]
    fn create_rejects_a_preexisting_symref_without_creating_its_referent() {
        let repository = repository();
        let reference = "refs/heads/nib/subagent/preexisting-symref";
        let referent = "refs/heads/unowned-missing-referent";
        git_stdout(repository.path(), &["symbolic-ref", reference, referent]);

        let error = Worktree::create(repository.path(), "preexisting-symref")
            .expect_err("preexisting symbolic branch must fail closed");

        assert!(error.contains("is symbolic to"), "{error}");
        assert!(error.contains("preserving"), "{error}");
        assert_eq!(
            git_stdout(repository.path(), &["symbolic-ref", reference]),
            referent
        );
        let referent_status = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", referent])
            .status()
            .expect("referent lookup");
        assert_eq!(
            referent_status.code(),
            Some(1),
            "branch claim created a symbolic-ref referent"
        );
    }

    #[test]
    fn create_preserves_a_preexisting_reciprocal_worktree_registration() {
        let repository = repository();
        let id = "preexisting-registration";
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        let registration = leave_stale_registration_for_path(repository.path(), &path);
        let gitdir_before =
            std::fs::read(registration.join("gitdir")).expect("stale registration backlink");

        let error = Worktree::create(repository.path(), id)
            .expect_err("pre-existing reciprocal registration must fail closed");

        assert!(
            error.contains("pre-existing Git worktree registration"),
            "{error}"
        );
        assert!(!path.exists(), "empty nib destination must be compensated");
        assert_eq!(
            std::fs::read(registration.join("gitdir")).expect("preserved registration backlink"),
            gitdir_before
        );
        assert_eq!(
            std::fs::read(registration.join("nib-preserve-sentinel"))
                .expect("preserved registration sentinel"),
            b"foreign"
        );
        let reference = format!("refs/heads/{}", branch_name(id));
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .expect("compensated branch lookup");
        assert_eq!(branch.code(), Some(1));
    }

    #[tokio::test]
    async fn cancellable_create_preserves_a_preexisting_reciprocal_registration() {
        let repository = repository();
        let id = "async-preexisting-registration";
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        let registration = leave_stale_registration_for_path(repository.path(), &path);

        let error = Worktree::create_cancellable(repository.path(), id, None)
            .await
            .expect_err("pre-existing reciprocal registration must fail closed");

        assert!(
            error.contains("pre-existing Git worktree registration"),
            "{error}"
        );
        assert!(!path.exists(), "empty nib destination must be compensated");
        assert_eq!(
            std::fs::read(registration.join("nib-preserve-sentinel"))
                .expect("preserved registration sentinel"),
            b"foreign"
        );
    }

    #[test]
    fn failed_add_preserves_a_registration_forged_after_the_snapshot() {
        let repository = repository();
        let id = "post-snapshot-forged-registration";
        SYNC_AFTER_REGISTRATION_SNAPSHOT_FORGERIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string());

        let error = Worktree::create(repository.path(), id)
            .expect_err("forged post-snapshot registration must fail closed");

        assert!(error.contains("registrations were preserved"), "{error}");
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        assert!(
            !path.exists(),
            "exact owned destination must be compensated"
        );
        let registration = repository
            .path()
            .join(".git/worktrees")
            .join(format!("forged-{id}"));
        assert_eq!(
            std::fs::read(registration.join("sentinel"))
                .expect("foreign registration was preserved"),
            b"foreign"
        );
        let reference = format!("refs/heads/{}", branch_name(id));
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .expect("compensated branch lookup");
        assert_eq!(branch.code(), Some(1));
    }

    #[test]
    fn branch_claim_never_replaces_a_symref_installed_after_missing_inspection() {
        let repository = repository();
        let id = "claim-symref-race";
        let reference = format!("refs/heads/{}", branch_name(id));
        let referent = "refs/heads/unowned-race-referent";
        BEFORE_REF_PUBLICATION_SYMREFS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(reference.clone(), referent.to_string());

        let error = Worktree::create(repository.path(), id)
            .expect_err("concurrent symbolic ref must win no-replace publication");

        assert!(error.contains("is symbolic to"), "{error}");
        assert_eq!(
            git_stdout(repository.path(), &["symbolic-ref", &reference]),
            referent
        );
        let referent_status = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", referent])
            .status()
            .expect("race referent lookup");
        assert_eq!(referent_status.code(), Some(1));
    }

    #[test]
    fn normal_remove_uses_exact_ownership_to_remove_path_registration_and_branch() {
        let repository = repository();
        let worktree = Worktree::create(repository.path(), "ambiguous-remove").expect("worktree");
        let reference = format!("refs/heads/{}", worktree.branch);
        Worktree::remove(repository.path(), &worktree.id).expect("remove worktree path");

        assert!(!worktree.path.exists());
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .expect("removed branch lookup");
        assert_eq!(branch.code(), Some(1));
    }

    #[test]
    fn cleanup_preserves_loose_ref_and_anchor_when_a_packed_copy_exists() {
        let repository = repository();
        let id = "packed-copy-cleanup";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        git_stdout(repository.path(), &["pack-refs", "--all", "--no-prune"]);
        let reference = format!("refs/heads/{}", worktree.branch);
        let loose = repository.path().join(".git").join(&reference);
        assert!(loose.exists(), "no-prune fixture lost its loose ref");
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("active ownership");
        let anchor = revision.record.branch_staging_path.clone();

        let error = Worktree::remove(repository.path(), id)
            .expect_err("packed ref ambiguity must fail closed");

        assert!(error.contains("packed ref"), "{error}");
        assert!(loose.exists(), "owned loose ref evidence was removed");
        assert!(anchor.exists(), "owned generation anchor was removed");
        assert_eq!(
            git_stdout(
                repository.path(),
                &["show-ref", "--hash", "--verify", &reference]
            ),
            worktree.branch_oid
        );
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("incomplete cleanup");
        assert_ne!(revision.record.phase, DurableOwnershipPhase::Complete);
        assert_ne!(
            revision.record.branch_cleanup,
            DurableArtifactPhase::Removed
        );
    }

    #[test]
    fn removing_restart_preserves_anchor_before_reporting_a_pruned_packed_ref() {
        let repository = repository();
        let id = "packed-pruned-restart";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        let project_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let key = (project_root.clone(), id.to_string());
        let ownership = WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
            .expect("owned worktree receipt");
        let mut state = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        persist_cleanup_artifact_phase(
            &mut state,
            CleanupArtifact::Branch,
            DurableArtifactPhase::Removing,
        )
        .expect("persist removing branch phase");
        let anchor = state
            .owned_branch
            .as_ref()
            .and_then(|branch| branch.receipt.anchor_path.clone())
            .expect("generation anchor");
        drop(state);
        drop(ownership);
        git_stdout(repository.path(), &["pack-refs", "--all", "--prune"]);
        let reference = format!("refs/heads/{}", worktree.branch);
        assert!(!repository.path().join(".git").join(&reference).exists());
        forget_subagent_ownership(repository.path(), id);

        let error = Worktree::remove(repository.path(), id)
            .expect_err("packed ref ambiguity must preserve restart anchor");

        assert!(error.contains("packed ref"), "{error}");
        assert!(
            anchor.exists(),
            "generation anchor was removed before reporting"
        );
        assert!(
            worktree.path.exists(),
            "worktree changed before fail-closed result"
        );
        assert_eq!(
            git_stdout(
                repository.path(),
                &["show-ref", "--hash", "--verify", &reference]
            ),
            worktree.branch_oid
        );
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("incomplete cleanup");
        assert_ne!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn ownership_compaction_lock_recovers_after_holder_exit() {
        if std::env::var(OWNERSHIP_LOCK_ROLE).as_deref() == Ok("holder") {
            let directory = crate::daemons::state::StableDirectory::open(Path::new(
                &std::env::var_os(OWNERSHIP_LOCK_DIRECTORY)
                    .expect("ownership lock directory fixture"),
            ))
            .expect("open ownership lock directory");
            let _lock = OwnershipCompactionLock::acquire(&directory, Duration::from_secs(5))
                .expect("acquire child ownership lock");
            std::fs::write(
                std::env::var_os(OWNERSHIP_LOCK_READY).expect("ownership lock ready fixture"),
                b"ready",
            )
            .expect("publish ownership lock readiness");
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }

        let directory = tempdir().expect("ownership lock fixture");
        let stable = crate::daemons::state::StableDirectory::open(directory.path())
            .expect("stable ownership lock directory");
        let ready = directory.path().join("ready");
        let mut child = Command::new(std::env::current_exe().expect("current test executable"));
        child
            .args(["--exact", OWNERSHIP_LOCK_TEST, "--nocapture"])
            .env(OWNERSHIP_LOCK_ROLE, "holder")
            .env(OWNERSHIP_LOCK_DIRECTORY, directory.path())
            .env(OWNERSHIP_LOCK_READY, &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = child.spawn().expect("spawn ownership lock holder");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "ownership lock holder was not ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().expect("kill ownership lock holder");
        child.wait().expect("reap ownership lock holder");

        OwnershipCompactionLock::acquire(&stable, Duration::from_secs(2))
            .expect("kernel lock is released after holder exit");
        let visible = directory.path().join(OWNERSHIP_COMPACTION_LOCK_NAME);
        let anchor = directory.path().join(OWNERSHIP_COMPACTION_ANCHOR_NAME);
        let anchor_file = stable.open_read(&anchor).expect("persistent lock anchor");
        stable
            .verify_file_identity(&visible, &anchor_file)
            .expect("stable lock identity");
    }

    #[test]
    fn restart_recovers_receipt_lock_quarantine_and_pre_stage_scratch_after_holder_exit() {
        const TEST_NAME: &str = "sandbox::worktree::tests::restart_recovers_receipt_lock_quarantine_and_pre_stage_scratch_after_holder_exit";
        if run_restart_crash_fixture() {
            return;
        }
        let repository = repository();
        let id = "restart-pre-stage-lock";
        let branch = branch_name(id);
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let reservation = reserve_managed_worktree_sync_controlled(
            repository.path(),
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("reserve worktree");
        let record = reservation.intent.revision.record.clone();
        let (ref_path, anchor_path) = managed_branch_paths(
            &record.common_git_dir,
            &record.branch_reference,
            &record.receipt_id,
            record.branch_anchor_generation,
        )
        .expect("managed branch paths");
        crate::fs_security::ensure_directory_without_symlinks(
            ref_path.parent().expect("ref parent"),
        )
        .expect("ref parent");
        spawn_and_kill_restart_fixture(TEST_NAME, |child| {
            child
                .env(RESTART_CRASH_MODE, "packed-quarantine")
                .env(RESTART_CRASH_COMMON, &record.common_git_dir)
                .env(
                    RESTART_CRASH_REF_DIRECTORY,
                    ref_path.parent().expect("ref parent"),
                )
                .env(RESTART_CRASH_REF_PATH, &ref_path)
                .env(RESTART_CRASH_ANCHOR_PATH, &anchor_path)
                .env(RESTART_CRASH_RECEIPT, &record.receipt_id)
                .env(RESTART_CRASH_REFERENCE, &record.branch_reference)
                .env(RESTART_CRASH_OID, &record.initial_oid);
        });
        let common = crate::daemons::state::StableDirectory::open(&record.common_git_dir)
            .expect("common directory");
        let packed = common.path().join("packed-refs.lock");
        let quarantine = common
            .deterministic_artifact_path(&packed, MANAGED_REF_LOCK_DELETE_PREFIX, ".quarantine")
            .expect("packed quarantine");
        let ref_directory =
            crate::daemons::state::StableDirectory::open(ref_path.parent().expect("ref parent"))
                .expect("ref directory");
        let reserved_temporary = ref_directory
            .deterministic_artifact_path(&anchor_path, RESERVED_REF_TEMPORARY_PREFIX, ".tmp")
            .expect("reserved temporary");
        assert!(quarantine.exists());
        assert!(reserved_temporary.exists());
        drop(reservation);

        Worktree::remove(repository.path(), id).expect("recover pre-stage crash");

        assert!(!quarantine.exists());
        assert!(!reserved_temporary.exists());
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable tombstone")
                .expect("complete tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn restart_recovers_dead_packed_and_target_ref_locks_after_holder_exit() {
        const TEST_NAME: &str = "sandbox::worktree::tests::restart_recovers_dead_packed_and_target_ref_locks_after_holder_exit";
        if run_restart_crash_fixture() {
            return;
        }
        let repository = repository();
        let id = "restart-target-lock";
        let branch = branch_name(id);
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let mut reservation = reserve_managed_worktree_sync_controlled(
            repository.path(),
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("reserve worktree");
        let staged = stage_reserved_branch_publication(&mut reservation).expect("stage branch");
        let record = reservation.intent.revision.record.clone();
        let (ref_path, _) = managed_branch_paths(
            &record.common_git_dir,
            &record.branch_reference,
            &record.receipt_id,
            record.branch_anchor_generation,
        )
        .expect("managed branch paths");
        drop(staged);
        drop(reservation);
        spawn_and_kill_restart_fixture(TEST_NAME, |child| {
            child
                .env(RESTART_CRASH_MODE, "target-lock")
                .env(RESTART_CRASH_COMMON, &record.common_git_dir)
                .env(
                    RESTART_CRASH_REF_DIRECTORY,
                    ref_path.parent().expect("ref parent"),
                )
                .env(RESTART_CRASH_REF_PATH, &ref_path)
                .env(RESTART_CRASH_RECEIPT, &record.receipt_id)
                .env(RESTART_CRASH_REFERENCE, &record.branch_reference);
        });
        let packed = record.common_git_dir.join("packed-refs.lock");
        let mut target_name = ref_path.file_name().expect("ref leaf").to_os_string();
        target_name.push(".lock");
        let target = ref_path.parent().expect("ref parent").join(target_name);
        assert!(packed.exists());
        assert!(target.exists());

        Worktree::remove(repository.path(), id).expect("recover target-lock crash");

        assert!(!packed.exists());
        assert!(!target.exists());
        assert!(!record.branch_staging_path.exists());
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable tombstone")
                .expect("complete tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    fn assert_ownership_cas_crash_recovers(mode: &str, test_name: &str, id: &str) {
        let repository = repository();
        let branch = branch_name(id);
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let mut reservation = reserve_managed_worktree_sync_controlled(
            repository.path(),
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("reserve worktree");
        let staged = stage_reserved_branch_publication(&mut reservation).expect("stage branch");
        let record = reservation.intent.revision.record.clone();
        let generation_marker = encoded_name_hash(OsStr::new("restart-cas-new-generation"));
        assert!(!record
            .preexisting_registration_name_hashes
            .contains(&generation_marker));
        let mut next_record = record.clone();
        next_record
            .preexisting_registration_name_hashes
            .push(generation_marker.clone());
        next_record.preexisting_registration_name_hashes.sort();
        next_record.preexisting_registration_name_hashes.dedup();
        validate_durable_ownership_record(
            &next_record,
            repository.path(),
            ManagedWorktreeKind::Subagent,
            id,
        )
        .expect("next ownership generation");
        let next_generation = tempfile::NamedTempFile::new().expect("next ownership fixture");
        std::fs::write(
            next_generation.path(),
            encode_durable_ownership(&next_record).expect("next ownership bytes"),
        )
        .expect("write next ownership fixture");
        let directory = reservation
            .intent
            .revision
            .directory
            .try_clone()
            .expect("ownership dir");
        let ownership_path = reservation.intent.revision.path.clone();
        drop(staged);
        drop(reservation);
        spawn_and_kill_restart_fixture(test_name, |child| {
            child
                .env(RESTART_CRASH_MODE, mode)
                .env(RESTART_CRASH_OWNERSHIP_DIRECTORY, directory.path())
                .env(RESTART_CRASH_OWNERSHIP_PATH, &ownership_path)
                .env(RESTART_CRASH_OWNERSHIP_NEW_BYTES, next_generation.path());
        });
        let previous = directory
            .deterministic_previous_artifact_path(
                &ownership_path,
                MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
            )
            .expect("ownership previous");
        assert!(previous.exists());
        if mode == "ownership-evacuated" {
            assert!(!ownership_path.exists());
        } else {
            assert!(ownership_path.exists());
        }

        let recovered =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("recover interrupted ownership CAS")
                .expect("recovered ownership generation");
        assert_eq!(
            recovered
                .record
                .preexisting_registration_name_hashes
                .contains(&generation_marker),
            mode == "ownership-committed",
            "restart selected the wrong side of the ownership CAS commit point"
        );
        assert!(!previous.exists());
        drop(recovered);

        Worktree::remove(repository.path(), id).expect("recover ownership CAS crash");

        assert!(!previous.exists());
        assert!(!record.branch_staging_path.exists());
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable tombstone")
                .expect("complete tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn restart_rolls_back_ownership_evacuated_before_publication() {
        const TEST_NAME: &str =
            "sandbox::worktree::tests::restart_rolls_back_ownership_evacuated_before_publication";
        if run_restart_crash_fixture() {
            return;
        }
        assert_ownership_cas_crash_recovers(
            "ownership-evacuated",
            TEST_NAME,
            "restart-cas-evacuated",
        );
    }

    #[test]
    fn restart_finalizes_committed_ownership_with_previous_scratch() {
        const TEST_NAME: &str =
            "sandbox::worktree::tests::restart_finalizes_committed_ownership_with_previous_scratch";
        if run_restart_crash_fixture() {
            return;
        }
        assert_ownership_cas_crash_recovers(
            "ownership-committed",
            TEST_NAME,
            "restart-cas-committed",
        );
    }

    #[test]
    fn foreign_packed_lock_owner_is_deferred_until_its_receipt_is_recovered() {
        let repository = repository();
        let reserve = |id: &str| {
            let path = repository.path().join(".nib/worktrees/subagents").join(id);
            crate::fs_security::ensure_directory_without_symlinks(
                path.parent().expect("worktree parent"),
            )
            .expect("worktree parent");
            reserve_managed_worktree_sync_controlled(
                repository.path(),
                ManagedWorktreeKind::Subagent,
                id,
                &path,
                &branch_name(id),
                None,
            )
            .expect("reserve worktree")
        };
        let reservation_a = reserve("foreign-lock-owner-a");
        let reservation_b = reserve("foreign-lock-owner-b");
        let record_a = reservation_a.intent.revision.record.clone();
        let record_b = reservation_b.intent.revision.record.clone();
        let common = crate::daemons::state::StableDirectory::open(&record_a.common_git_dir)
            .expect("common directory");
        let packed = common.path().join("packed-refs.lock");
        let publication = common
            .save_bytes_atomically_expected_with_receipt(
                &packed,
                &managed_ref_lock_contents(
                    &record_a.receipt_id,
                    &record_a.branch_reference,
                    "packed",
                ),
                MANAGED_REF_LOCK_TEMPORARY_PREFIX,
                crate::daemons::state::FileExpectation::Missing,
            )
            .expect("dead packed lock fixture");
        drop(publication);

        recover_owned_ref_restart_artifacts(&record_b).expect("defer foreign owner");
        assert!(packed.exists());
        recover_owned_ref_restart_artifacts(&record_a).expect("recover matching owner");
        assert!(!packed.exists());
        drop(reservation_a);
        drop(reservation_b);
        Worktree::remove(repository.path(), "foreign-lock-owner-a").expect("remove owner A");
        Worktree::remove(repository.path(), "foreign-lock-owner-b").expect("remove owner B");
    }

    #[test]
    fn collected_tombstone_fallback_respects_cleanup_deadline_while_lock_is_held() {
        let repository = repository();
        let project_root =
            repository_root_bounded_sync(repository.path()).expect("validated repository root");
        let directory =
            managed_worktree_ownership_directory(&project_root).expect("ownership directory");
        let _held = OwnershipCompactionLock::acquire(&directory, Duration::from_secs(2))
            .expect("hold ownership lock");
        let timeout = Duration::from_millis(100);
        let started = Instant::now();
        let deadline = started + timeout;

        let error = remove_registered_worktree_until(
            &project_root,
            "deadline-with-collected-tombstone",
            deadline,
            timeout,
        )
        .expect_err("held ownership lock must exhaust cleanup deadline");

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            error.contains("timed out") || error.contains("deadline"),
            "{error}"
        );
    }

    #[test]
    fn completed_tombstone_compaction_recovers_stale_cas_and_keeps_remove_idempotent() {
        let repository = repository();
        let ids = ["compact-a", "compact-b", "compact-c"];
        for id in ids {
            Worktree::create(repository.path(), id).expect("create compacted worktree");
            Worktree::remove(repository.path(), id).expect("complete compacted worktree");
        }
        let directory =
            managed_worktree_ownership_directory(repository.path()).expect("ownership directory");
        let _lock = OwnershipCompactionLock::acquire(&directory, Duration::from_secs(2))
            .expect("ownership compaction lock");
        let retained =
            managed_worktree_ownership_path(&directory, ManagedWorktreeKind::Subagent, ids[0]);
        let retained_bytes = directory
            .open_read(&retained)
            .expect("retained tombstone")
            .metadata()
            .expect("retained tombstone metadata")
            .len();
        let stale_temporary = directory
            .deterministic_artifact_path(
                &retained,
                MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX,
                ".tmp",
            )
            .expect("stale ownership transaction path");
        std::fs::write(&stale_temporary, b"stale").expect("stale ownership transaction");

        compact_complete_ownership_records_with_limits(
            &directory,
            repository.path(),
            &retained,
            retained_bytes,
            1,
            MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES,
        )
        .expect("compact complete ownership tombstones");

        assert!(!stale_temporary.exists(), "stale CAS transaction remains");
        let mut records = 0;
        directory
            .for_each_entry_bounded(16, 4096, |name| {
                if Path::new(&name).extension() == Some(OsStr::new("json")) {
                    records += 1;
                }
                Ok(())
            })
            .expect("bounded ownership listing");
        assert_eq!(records, 1);
        drop(_lock);
        Worktree::remove(repository.path(), ids[1])
            .expect("collected tombstone uses bounded absence proof");
    }

    #[test]
    fn removing_branch_resumes_from_anchor_only_after_restart() {
        let repository = repository();
        let id = "restart-anchor-only";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        let key = (
            repository
                .path()
                .canonicalize()
                .expect("canonical repository"),
            id.to_string(),
        );
        let ownership = WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
            .expect("owned worktree receipt");
        let mut state = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        persist_cleanup_artifact_phase(
            &mut state,
            CleanupArtifact::Branch,
            DurableArtifactPhase::Removing,
        )
        .expect("persist removing branch phase");
        let owned_branch = state.owned_branch.as_ref().expect("owned branch").clone();
        remove_owned_file_receipt(
            &owned_branch.receipt.directory,
            &owned_branch.receipt.path,
            &owned_branch.receipt.file,
            &owned_branch.receipt.contents,
            ".nib-owned-ref-delete-",
        )
        .expect("remove final ref only");
        drop(state);
        drop(ownership);
        forget_subagent_ownership(repository.path(), id);

        Worktree::remove(repository.path(), id).expect("resume anchor-only cleanup");

        assert!(!worktree.path.exists());
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("complete tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn quarantine_only_branch_cleanup_remains_incomplete_and_reported() {
        let repository = repository();
        let id = "restart-ref-quarantine";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        let key = (
            repository
                .path()
                .canonicalize()
                .expect("canonical repository"),
            id.to_string(),
        );
        let ownership = WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
            .expect("owned worktree receipt");
        let mut state = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        persist_cleanup_artifact_phase(
            &mut state,
            CleanupArtifact::Branch,
            DurableArtifactPhase::Removing,
        )
        .expect("persist removing branch phase");
        let owned_branch = state.owned_branch.as_ref().expect("owned branch").clone();
        let quarantine = owned_branch
            .receipt
            .directory
            .deterministic_artifact_path(
                &owned_branch.receipt.path,
                ".nib-owned-ref-delete-",
                ".quarantine",
            )
            .expect("branch quarantine path");
        std::fs::rename(&owned_branch.receipt.path, &quarantine)
            .expect("simulate ref deletion quarantine crash");
        drop(state);
        drop(ownership);
        forget_subagent_ownership(repository.path(), id);

        let error = Worktree::remove(repository.path(), id)
            .expect_err("quarantine-only cleanup requires physical recovery");

        assert!(error.contains("deletion quarantine"), "{error}");
        assert!(quarantine.exists());
        assert!(owned_branch
            .receipt
            .anchor_path
            .as_ref()
            .expect("branch anchor")
            .exists());
        assert!(worktree.path.exists());
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("incomplete cleanup");
        assert_ne!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn durable_receipt_rehydrates_cleanup_after_process_state_loss() {
        let repository = repository();
        let id = "restart-cleanup";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        let reference = format!("refs/heads/{}", worktree.branch);
        forget_subagent_ownership(repository.path(), id);

        Worktree::remove(repository.path(), id).expect("restart cleanup");

        assert!(!worktree.path.exists());
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .expect("removed branch lookup");
        assert_eq!(branch.code(), Some(1));
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("durable tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
        assert_eq!(revision.record.path_cleanup, DurableArtifactPhase::Removed);
        assert_eq!(
            revision.record.registration_cleanup,
            DurableArtifactPhase::Removed
        );
        assert_eq!(
            revision.record.branch_cleanup,
            DurableArtifactPhase::Removed
        );
    }

    #[test]
    fn durable_generational_receipt_adopts_branch_oid_after_restart() {
        let repository = repository();
        let id = "restart-adoption";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        let initial_revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("initial durable receipt")
                .expect("initial ownership");
        let initial_anchor = initial_revision.record.branch_staging_path.clone();
        std::fs::write(worktree.path.join("adopted.txt"), "adopted\n").expect("adopted fixture");
        git_stdout(&worktree.path, &["add", "adopted.txt"]);
        git_stdout(&worktree.path, &["commit", "-m", "adopted revision"]);
        let adopted_oid = git_stdout(&worktree.path, &["rev-parse", "HEAD"]);
        forget_subagent_ownership(repository.path(), id);

        Worktree::adopt_branch_revision(repository.path(), id, &adopted_oid)
            .expect("durable adoption");

        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("active ownership");
        assert_eq!(revision.record.current_oid, adopted_oid);
        assert_eq!(revision.record.branch_anchor_generation, 1);
        assert!(revision.record.previous_branch_anchor.is_none());
        assert!(!initial_anchor.exists(), "prior anchor was not retired");
        assert!(revision.record.branch_staging_path.exists());

        let anchor_directory = crate::daemons::state::StableDirectory::open(
            initial_anchor.parent().expect("initial anchor parent"),
        )
        .expect("branch anchor directory");
        let initial_contents = format!("{}\n", worktree.branch_oid).into_bytes();
        let previous_publication = anchor_directory
            .save_bytes_atomically_expected_with_receipt(
                &initial_anchor,
                &initial_contents,
                ".nib-test-previous-anchor-",
                crate::daemons::state::FileExpectation::Missing,
            )
            .expect("recreate previous anchor fixture");
        let previous_identity =
            crate::fs_security::file_identity_snapshot(&previous_publication.file)
                .expect("previous anchor identity");
        let key = (
            repository
                .path()
                .canonicalize()
                .expect("canonical repository"),
            id.to_string(),
        );
        let ownership =
            load_managed_worktree_ownership(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("rehydrate ownership")
                .expect("active ownership");
        WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, ownership.clone());
        let mut state = ownership
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let durable = state.durable.as_mut().expect("durable ownership state");
        let mut record = durable.record.clone();
        record.previous_branch_anchor = Some(DurablePreviousBranchAnchor {
            path: initial_anchor.clone(),
            identity: previous_identity,
            oid: worktree.branch_oid.clone(),
        });
        persist_durable_ownership_revision(durable, record)
            .expect("persist interrupted prior-anchor retirement");
        drop(state);
        drop(ownership);

        Worktree::adopt_branch_revision(repository.path(), id, &adopted_oid)
            .expect("same-OID retry retires the prior anchor");
        let reconciled =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("reconciled durable receipt")
                .expect("reconciled active ownership");
        assert!(reconciled.record.previous_branch_anchor.is_none());
        assert!(!initial_anchor.exists());
        forget_subagent_ownership(repository.path(), id);
        Worktree::remove(repository.path(), id).expect("cleanup adopted revision");
    }

    #[test]
    fn durable_branch_identity_preserves_a_same_oid_replacement_after_restart() {
        let repository = repository();
        let id = "restart-branch-replacement";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        forget_subagent_ownership(repository.path(), id);
        let reference_path = repository
            .path()
            .join(".git/refs/heads")
            .join(&worktree.branch);
        let displaced = reference_path.with_extension("owned-away");
        std::fs::rename(&reference_path, &displaced).expect("displace owned ref");
        std::fs::write(&reference_path, format!("{}\n", worktree.branch_oid))
            .expect("same-OID replacement ref");

        let error = Worktree::remove(repository.path(), id)
            .expect_err("same contents must not replace durable ref identity");

        assert!(error.contains("durable ownership identity"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&reference_path).expect("preserved replacement ref"),
            format!("{}\n", worktree.branch_oid)
        );
        assert!(
            worktree.path.is_dir(),
            "worktree mutated before fail-closed result"
        );
    }

    #[test]
    fn identical_oid_adoption_without_a_generational_receipt_fails_closed() {
        let repository = repository();
        let id = "missing-generational-receipt";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        forget_subagent_ownership(repository.path(), id);
        let ownership_directory =
            managed_worktree_ownership_directory(repository.path()).expect("ownership directory");
        let ownership_path = managed_worktree_ownership_path(
            &ownership_directory,
            ManagedWorktreeKind::Subagent,
            id,
        );
        std::fs::remove_file(ownership_path).expect("remove durable receipt fixture");

        let error = Worktree::adopt_branch_revision(repository.path(), id, &worktree.branch_oid)
            .expect_err("OID equality alone must not prove ownership");

        assert!(error.contains("durable generational receipt"), "{error}");
        assert!(worktree.path.is_dir(), "owned worktree was mutated");
        assert_eq!(
            git_stdout(
                repository.path(),
                &[
                    "show-ref",
                    "--hash",
                    "--verify",
                    &format!("refs/heads/{}", worktree.branch)
                ]
            ),
            worktree.branch_oid
        );
    }

    #[test]
    fn durable_path_identity_preserves_a_replacement_after_restart() {
        let repository = repository();
        let id = "restart-path-replacement";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        forget_subagent_ownership(repository.path(), id);
        let displaced = worktree.path.with_extension("owned-away");
        std::fs::rename(&worktree.path, &displaced).expect("displace owned path");
        std::fs::create_dir(&worktree.path).expect("replacement path");
        std::fs::write(worktree.path.join("sentinel"), b"replacement")
            .expect("replacement sentinel");

        let error = Worktree::remove(repository.path(), id)
            .expect_err("replacement must not match persisted identity");

        assert!(error.contains("durable ownership identity"), "{error}");
        assert_eq!(
            std::fs::read(worktree.path.join("sentinel")).expect("preserved replacement"),
            b"replacement"
        );
        assert!(displaced.is_dir(), "original owned path was mutated");
    }

    #[test]
    fn removing_phases_reconcile_to_a_complete_tombstone_after_restart() {
        let repository = repository();
        let id = "restart-removing-phases";
        let worktree = Worktree::create(repository.path(), id).expect("worktree");
        let key = (
            repository
                .path()
                .canonicalize()
                .expect("canonical repository"),
            id.to_string(),
        );
        let ownership = WORKTREE_OWNERSHIP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
            .expect("process ownership");
        let (path_receipt, registration_receipt, owned_branch) = {
            let mut state = ownership
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for artifact in [
                CleanupArtifact::Path,
                CleanupArtifact::Registration,
                CleanupArtifact::Branch,
            ] {
                persist_cleanup_artifact_phase(
                    &mut state,
                    artifact,
                    DurableArtifactPhase::Removing,
                )
                .expect("write-ahead cleanup phase");
            }
            (
                ownership.path_receipt.clone().expect("path receipt"),
                ownership
                    .registration_receipt
                    .clone()
                    .expect("registration receipt"),
                state.owned_branch.clone().expect("branch receipt"),
            )
        };
        crate::fs_security::remove_directory_tree_capability_bound_if_matches(
            ownership
                .registration_path
                .parent()
                .expect("registration parent"),
            &ownership.registration_path,
            registration_receipt,
            Instant::now() + GIT_COMMAND_TIMEOUT,
        )
        .expect("remove registration before simulated crash");
        crate::fs_security::remove_directory_tree_capability_bound_if_matches(
            ownership.path.parent().expect("worktree parent"),
            &ownership.path,
            path_receipt,
            Instant::now() + GIT_COMMAND_TIMEOUT,
        )
        .expect("remove path before simulated crash");
        delete_owned_branch_sync_with_timeout(
            repository.path(),
            &owned_branch,
            GIT_COMMAND_TIMEOUT,
        )
        .expect("remove branch before simulated crash");
        forget_subagent_ownership(repository.path(), id);

        Worktree::remove(repository.path(), id).expect("reconcile write-ahead cleanup");

        assert!(!worktree.path.exists());
        let revision =
            load_durable_ownership_revision(repository.path(), ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("durable tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn unfinished_creation_intent_is_compensated_from_durable_provenance() {
        let repository = repository();
        let project_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let id = "restart-creation-intent";
        let branch = branch_name(id);
        let path = project_root.join(".nib/worktrees/subagents").join(id);
        let parent = crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let mut reservation = reserve_managed_worktree_sync_controlled(
            &project_root,
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("durable reservation");
        let owned_branch = create_reserved_worktree_branch_sync_controlled(&mut reservation, None)
            .expect("owned branch");
        let path_receipt = publish_reserved_empty_worktree_destination(&mut reservation, &parent)
            .expect("owned empty destination");
        drop(reservation);
        drop(path_receipt);
        drop(owned_branch);

        Worktree::remove(&project_root, id).expect("recover incomplete creation intent");

        assert!(!path.exists());
        let reference = format!("refs/heads/{branch}");
        let branch_status = Command::new("git")
            .current_dir(&project_root)
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .expect("branch absence lookup");
        assert_eq!(branch_status.code(), Some(1));
        let revision =
            load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("durable tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
        assert_eq!(
            revision.record.registration_cleanup,
            DurableArtifactPhase::Removed
        );
    }

    #[test]
    fn partial_add_restart_preserves_and_reports_unattributed_registration() {
        let repository = repository();
        let project_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let id = "restart-partial-registration";
        let branch = branch_name(id);
        let path = project_root.join(".nib/worktrees/subagents").join(id);
        let parent = crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let mut reservation = reserve_managed_worktree_sync_controlled(
            &project_root,
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("durable reservation");
        create_reserved_worktree_branch_sync_controlled(&mut reservation, None)
            .expect("reserved branch");
        publish_reserved_empty_worktree_destination(&mut reservation, &parent)
            .expect("reserved worktree path");
        let registration = reservation
            .intent
            .revision
            .record
            .common_git_dir
            .join("worktrees")
            .join("partial-restart-fixture");
        std::fs::create_dir_all(&registration).expect("partial Git registration");
        let gitdir = path.join(".git");
        std::fs::write(
            registration.join("gitdir"),
            gitdir.as_os_str().as_encoded_bytes(),
        )
        .expect("partial Git registration backlink");
        std::fs::write(registration.join("sentinel"), b"unattributed")
            .expect("partial Git registration sentinel");
        drop(reservation);

        let error = Worktree::remove(&project_root, id)
            .expect_err("unattributed partial registration must remain reported");

        assert!(
            error.contains("post-snapshot Git worktree registration"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(registration.join("sentinel")).expect("preserved registration"),
            b"unattributed"
        );
        assert!(!path.exists(), "exact reserved path was not compensated");
        let revision =
            load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("incomplete intent");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Intent);
        assert_eq!(revision.record.path_cleanup, DurableArtifactPhase::Removed);
        assert_eq!(
            revision.record.branch_cleanup,
            DurableArtifactPhase::Removed
        );
        assert_eq!(
            revision.record.registration_cleanup,
            DurableArtifactPhase::Unattributed
        );
        let retry = Worktree::remove(&project_root, id)
            .expect_err("unattributed registration remains nonterminal");
        assert!(
            retry.contains("post-snapshot Git worktree registration"),
            "{retry}"
        );
    }

    #[test]
    fn reserved_branch_publisher_excludes_recovery_until_present_cas() {
        let repository = repository();
        let project_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let id = "live-reserved-branch-publisher";
        let branch = branch_name(id);
        let path = project_root.join(".nib/worktrees/subagents").join(id);
        let mut reservation = reserve_managed_worktree_sync_controlled(
            &project_root,
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("durable reservation");
        let mut record = reservation.intent.revision.record.clone();
        record.path_cleanup = DurableArtifactPhase::Removed;
        record.registration_cleanup = DurableArtifactPhase::Removed;
        persist_durable_ownership_revision(&mut reservation.intent.revision, record.clone())
            .expect("isolate branch recovery");
        let recovery_attempted = std::cell::Cell::new(false);

        let staged = stage_reserved_branch_publication_with_hook(&mut reservation, || {
            let error = Worktree::remove(&project_root, id)
                .expect_err("recovery must preserve a live reserved branch publisher");
            assert!(error.contains("live publisher"), "{error}");
            assert!(
                record.branch_staging_path.is_file(),
                "recovery removed the live publisher's staging anchor"
            );
            let observed =
                load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
                    .expect("read live reservation")
                    .expect("durable reservation");
            assert_eq!(
                observed.record.branch_cleanup,
                DurableArtifactPhase::Reserved
            );
            recovery_attempted.set(true);
        })
        .expect("publish reserved branch identity");

        assert!(recovery_attempted.get());
        assert_eq!(
            reservation.intent.revision.record.branch_cleanup,
            DurableArtifactPhase::Present
        );
        let branch_directory = crate::daemons::state::StableDirectory::open(
            staged.path.parent().expect("branch staging parent"),
        )
        .expect("stable branch parent");
        let contender = branch_directory
            .open_read_write(&staged.path)
            .expect("open post-CAS lock contender");
        contender
            .try_lock()
            .expect("publisher must release the staging lock after the Present CAS");
        contender.unlock().expect("release lock contender");
        let staged_path = staged.path.clone();
        drop(contender);
        drop(branch_directory);
        drop(staged);
        drop(reservation);

        Worktree::remove(&project_root, id).expect("recover released staged branch");

        assert!(!staged_path.exists());
        let revision =
            load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("complete tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn reserved_staging_names_recover_a_crash_before_identity_cas() {
        let repository = repository();
        let project_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let id = "restart-before-staging-cas";
        let branch = branch_name(id);
        let path = project_root.join(".nib/worktrees/subagents").join(id);
        let parent = crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let reservation = reserve_managed_worktree_sync_controlled(
            &project_root,
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("durable reservation");
        let record = reservation.intent.revision.record.clone();
        let parent_directory =
            crate::daemons::state::StableDirectory::open(&parent).expect("stable worktree parent");
        parent_directory
            .create_owned_child_directory(&record.worktree_staging_path)
            .expect("reserved worktree staging");
        let branch_parent = crate::fs_security::ensure_directory_without_symlinks(
            record
                .branch_staging_path
                .parent()
                .expect("branch staging parent"),
        )
        .expect("branch staging parent");
        let branch_directory = crate::daemons::state::StableDirectory::open(&branch_parent)
            .expect("stable branch parent");
        let staged_receipt = branch_directory
            .save_bytes_atomically_expected_with_receipt(
                &record.branch_staging_path,
                format!("{}\n", record.current_oid).as_bytes(),
                ".nib-test-reserved-ref-",
                crate::daemons::state::FileExpectation::Missing,
            )
            .expect("reserved branch staging");
        drop(staged_receipt);
        drop(branch_directory);
        drop(parent_directory);
        drop(reservation);

        Worktree::remove(&project_root, id).expect("recover receipt-bound reserved staging");

        assert!(!record.worktree_staging_path.exists());
        assert!(!record.branch_staging_path.exists());
        let revision =
            load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("complete tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn identity_cas_before_final_publication_recovers_staged_artifacts() {
        let repository = repository();
        let project_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let id = "restart-after-staging-cas";
        let branch = branch_name(id);
        let path = project_root.join(".nib/worktrees/subagents").join(id);
        let parent = crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let mut reservation = reserve_managed_worktree_sync_controlled(
            &project_root,
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("durable reservation");
        let record = reservation.intent.revision.record.clone();
        let parent_directory =
            crate::daemons::state::StableDirectory::open(&parent).expect("stable worktree parent");
        let staged_directory = parent_directory
            .create_owned_child_directory(&record.worktree_staging_path)
            .expect("reserved worktree staging");
        let staged_receipt = staged_directory
            .directory_removal_receipt()
            .expect("reserved staging identity");
        let mut attached = reservation.intent.revision.record.clone();
        attached.worktree_identity = Some(staged_receipt.identity());
        attached.path_cleanup = DurableArtifactPhase::Present;
        persist_durable_ownership_revision(&mut reservation.intent.revision, attached)
            .expect("persist staged worktree identity");
        stage_reserved_branch_publication(&mut reservation)
            .expect("persist staged branch identity");
        let staged_record = reservation.intent.revision.record.clone();
        drop(staged_receipt);
        drop(staged_directory);
        drop(parent_directory);
        drop(reservation);

        Worktree::remove(&project_root, id).expect("recover identity-bound staging");

        assert!(!staged_record.worktree_staging_path.exists());
        assert!(!staged_record.branch_staging_path.exists());
        assert!(!staged_record.worktree_path.exists());
        let revision =
            load_durable_ownership_revision(&project_root, ManagedWorktreeKind::Subagent, id)
                .expect("durable receipt")
                .expect("complete tombstone");
        assert_eq!(revision.record.phase, DurableOwnershipPhase::Complete);
    }

    #[test]
    fn reservation_preserves_artifacts_whose_exact_identity_was_not_attached() {
        let repository = repository();
        let project_root = repository
            .path()
            .canonicalize()
            .expect("canonical repository");
        let id = "restart-unattached-reservation";
        let branch = branch_name(id);
        let path = project_root.join(".nib/worktrees/subagents").join(id);
        let parent = crate::fs_security::ensure_directory_without_symlinks(
            path.parent().expect("worktree parent"),
        )
        .expect("worktree parent");
        let reservation = reserve_managed_worktree_sync_controlled(
            &project_root,
            ManagedWorktreeKind::Subagent,
            id,
            &path,
            &branch,
            None,
        )
        .expect("durable reservation");
        let owned_branch = create_owned_branch_sync(&project_root, &branch).expect("owned branch");
        let path_receipt = publish_owned_empty_worktree_destination(&parent, &path)
            .expect("owned empty destination");
        drop(reservation);
        drop(path_receipt);
        drop(owned_branch);

        let error = Worktree::remove(&project_root, id)
            .expect_err("unattached identities must remain fail-closed");

        assert!(error.contains("before its reserved"), "{error}");
        assert!(path.is_dir(), "unattached path was removed");
        assert_eq!(
            git_stdout(
                &project_root,
                &[
                    "show-ref",
                    "--hash",
                    "--verify",
                    &format!("refs/heads/{branch}")
                ]
            ),
            git_stdout(&project_root, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn remove_without_a_retained_tombstone_uses_bounded_absence_proof() {
        let repository = repository();
        let id = "absent-without-receipt";
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        assert!(!path.exists());

        Worktree::remove(repository.path(), id).expect("bounded absence proof");
        assert!(!path.exists());
    }

    #[test]
    fn failed_add_preserves_a_replacement_installed_after_owned_publication() {
        let repository = repository();
        let id = "failed-add-path-replacement";
        SYNC_AFTER_DESTINATION_PUBLICATION_REPLACEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string());

        let error = Worktree::create(repository.path(), id)
            .expect_err("failed add must preserve the replacement destination");

        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        assert_eq!(
            std::fs::read(path.join("sentinel")).expect("replacement sentinel"),
            b"replacement"
        );
        assert!(
            error.contains("replacement") || error.contains("identity"),
            "{error}"
        );
        let reference = format!("refs/heads/{}", branch_name(id));
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .expect("failed-add branch lookup");
        assert_eq!(branch.code(), Some(1));
    }

    #[test]
    fn final_absence_proof_preserves_a_destination_that_appeared_before_publication() {
        let repository = repository();
        let id = "pre-publication-replacement";
        SYNC_BEFORE_ADD_DESTINATION_REPLACEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), PathBuf::from("replacement"));

        let error = Worktree::create(repository.path(), id)
            .expect_err("destination replacement must fail the final absence proof");

        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        assert!(path.join("sentinel").is_file());
        assert!(
            error.contains("appeared") || error.contains("preserved"),
            "{error}"
        );
    }

    #[test]
    fn compensation_preserves_a_hostile_visible_path_replacement() {
        let repository = repository();
        let id = "post-add-path-replacement";
        SYNC_POST_CAPTURE_PATH_REPLACEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string());

        let error = Worktree::create(repository.path(), id)
            .expect_err("post-add path replacement must fail closed");

        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        assert_eq!(
            std::fs::read(path.join("sentinel")).expect("replacement sentinel"),
            b"replacement"
        );
        assert!(
            error.contains("replaced") || error.contains("preserved"),
            "{error}"
        );
        let reference = format!("refs/heads/{}", branch_name(id));
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .expect("replacement branch lookup");
        assert_eq!(branch.code(), Some(1));
    }

    #[test]
    fn compensation_preserves_a_hostile_registration_replacement() {
        let repository = repository();
        let id = "post-add-registration-replacement";
        SYNC_POST_CAPTURE_REGISTRATION_REPLACEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string());

        let error = Worktree::create(repository.path(), id)
            .expect_err("post-add registration replacement must fail closed");

        assert!(
            error.contains("registration") || error.contains("Git"),
            "{error}"
        );
        let registrations = repository.path().join(".git/worktrees");
        assert!(
            std::fs::read_dir(&registrations)
                .expect("registration directory")
                .filter_map(Result::ok)
                .any(|entry| entry.path().join("sentinel").is_file()),
            "registration replacement was removed"
        );
        assert!(
            repository
                .path()
                .join(".nib/worktrees/subagents")
                .join(id)
                .exists(),
            "owned path must remain until registration cleanup is exact"
        );
    }

    #[test]
    fn create_refuses_to_overwrite_a_stale_in_process_ownership_receipt() {
        let repository = repository();
        let id = "stale-ownership";
        let worktree = Worktree::create(repository.path(), id).expect("owned worktree");
        let displaced = repository.path().join("displaced-owned-worktree");
        std::fs::rename(&worktree.path, &displaced).expect("displace owned worktree");

        let error = Worktree::create(repository.path(), id)
            .expect_err("same-id create must retain the prior ownership receipt");

        assert!(error.contains("active ownership receipt"), "{error}");
        assert!(displaced.is_dir());
    }

    #[test]
    fn compensation_preserves_a_branch_moved_after_creation() {
        let repository = repository();
        let id = "moved-branch-compensation";
        let replacement = replacement_commit(repository.path());
        SYNC_POST_ADD_BRANCH_MOVES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), replacement.clone());

        let error = Worktree::create(repository.path(), id)
            .expect_err("moved branch compensation must fail closed");

        assert!(error.contains("refusing publication"), "{error}");
        assert!(error.contains("changed from"), "{error}");
        assert!(error.contains("preserving it"), "{error}");
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        assert!(!path.exists(), "partial worktree path remains");
        let reference = format!("refs/heads/{}", branch_name(id));
        assert_eq!(
            git_stdout(
                repository.path(),
                &["show-ref", "--hash", "--verify", &reference]
            ),
            replacement
        );
    }

    #[test]
    fn compensation_preserves_a_post_add_symref_and_its_referent() {
        let repository = repository();
        let id = "symref-branch-compensation";
        let referent = "refs/heads/unowned-protected-referent";
        let expected = git_stdout(repository.path(), &["rev-parse", "HEAD"]);
        git_stdout(repository.path(), &["update-ref", referent, &expected]);
        SYNC_POST_ADD_BRANCH_SYMREFS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), referent.to_string());

        let error = Worktree::create(repository.path(), id)
            .expect_err("post-add symbolic branch must fail closed");

        assert!(error.contains("is symbolic to"), "{error}");
        assert!(error.contains("preserving"), "{error}");
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        assert!(!path.exists(), "partial worktree path remains");
        let reference = format!("refs/heads/{}", branch_name(id));
        assert_eq!(
            git_stdout(repository.path(), &["symbolic-ref", &reference]),
            referent
        );
        assert_eq!(
            git_stdout(
                repository.path(),
                &["show-ref", "--hash", "--verify", referent]
            ),
            expected,
            "compensation deleted or moved the symbolic-ref referent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_rejects_symlinked_worktree_ancestor_without_outside_mutation() {
        use std::os::unix::fs::symlink;

        let repository = repository();
        let outside = tempdir().expect("outside");
        std::fs::create_dir(repository.path().join(".nib")).expect("state");
        symlink(outside.path(), repository.path().join(".nib/worktrees"))
            .expect("worktrees symlink");

        let error = Worktree::create(repository.path(), "hostile")
            .expect_err("symlinked worktree root must fail closed");

        assert!(error.contains("unsafe") || error.contains("symlink"));
        assert!(!outside.path().join("subagents").exists());
    }

    #[test]
    fn sync_create_compensates_a_failure_after_worktree_add() {
        let repository = repository();
        let id = "post-add-compensation";
        SYNC_POST_ADD_VALIDATION_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string());

        let error = Worktree::create(repository.path(), id)
            .expect_err("injected post-add validation must fail");

        assert!(error.contains("injected post-add"), "{error}");
        assert!(
            !error.contains("partial worktree cleanup failed"),
            "post-add compensation did not finish: {error}"
        );
        let path = repository.path().join(".nib/worktrees/subagents").join(id);
        assert!(!path.exists(), "partial worktree path remains: {error}");
        let registered = Command::new("git")
            .current_dir(repository.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("worktree list");
        assert!(
            !String::from_utf8_lossy(&registered.stdout).contains(path.to_string_lossy().as_ref()),
            "partial worktree registration remains: {error}"
        );
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/nib/subagent/post-add-compensation",
            ])
            .status()
            .expect("branch lookup");
        assert_eq!(
            branch.code(),
            Some(1),
            "partial worktree branch remains: {error}"
        );
    }

    #[test]
    fn cleanup_retry_resumes_after_branch_deletion_and_lock_release_failure() {
        let repository = repository();
        let id = "retry-after-ref-lock-release";
        let worktree = Worktree::create(repository.path(), id).expect("managed worktree");
        let lock_path = repository
            .path()
            .join(".git")
            .join(format!("refs/heads/{}.lock", worktree.branch));
        OWNED_REF_LOCK_RELEASE_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(lock_path);

        let error = Worktree::remove(repository.path(), id)
            .expect_err("injected ref lock release failure must be reported");
        assert!(error.contains("injected owned ref lock release"), "{error}");
        let reference = format!("refs/heads/{}", worktree.branch);
        let branch = Command::new("git")
            .current_dir(repository.path())
            .args(["show-ref", "--verify", "--quiet", &reference])
            .status()
            .expect("branch absence lookup");
        assert_eq!(
            branch.code(),
            Some(1),
            "owned branch deletion did not commit"
        );

        Worktree::remove(repository.path(), id).expect("cleanup retry resumes from ref absence");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_git_process_timeout_kills_its_descendants() {
        let directory = tempdir().expect("timeout fixture");
        let pid_file = directory.path().join("child.pid");
        let script = format!(
            "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
            pid_file.display()
        );
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let error = run_process_bounded(command, "timeout fixture", Duration::from_millis(100))
            .await
            .expect_err("command must time out");

        assert!(error.contains("timed out"), "{error}");
        let pid = wait_for_pid(&pid_file).await;
        assert_process_terminated(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_bounded_git_future_kills_its_descendants() {
        let directory = tempdir().expect("cancellation fixture");
        let pid_file = directory.path().join("child.pid");
        let script = format!(
            "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
            pid_file.display()
        );
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let task = tokio::spawn(run_process_bounded(
            command,
            "cancellation fixture",
            Duration::from_secs(30),
        ));
        let pid = wait_for_pid(&pid_file).await;

        task.abort();
        assert!(task.await.expect_err("task cancellation").is_cancelled());
        assert_process_terminated(pid).await;
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn sync_managed_cancellation_reaps_descendant_tree() {
        if run_sync_control_fixture(SYNC_CANCEL_TEST) {
            return;
        }
        let directory = tempdir().expect("sync cancellation fixture");
        let pid_path = directory.path().join("descendant.pid");
        let command = sync_control_command(SYNC_CANCEL_TEST, &pid_path);
        let cancellation = BlockingGitCancellation::new(None);
        let worker_cancellation = cancellation.clone();
        let worker = std::thread::spawn(move || {
            run_process_bounded_sync_controlled(
                command,
                "sync cancellation fixture",
                Duration::from_secs(30),
                Some(&worker_cancellation),
            )
        });
        let descendant = wait_for_pid_sync(&pid_path);

        cancellation.cancel();
        let error = worker
            .join()
            .expect("sync cancellation worker")
            .expect_err("sync managed command must be cancelled");

        assert!(error.contains("cancelled"), "{error}");
        assert_process_terminated_sync(descendant);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn dropping_sync_managed_child_reaps_descendant_tree() {
        if run_sync_control_fixture(SYNC_DROP_TEST) {
            return;
        }
        let directory = tempdir().expect("sync drop fixture");
        let pid_path = directory.path().join("descendant.pid");
        let mut command = sync_control_command(SYNC_DROP_TEST, &pid_path);
        let managed = SyncManagedChild::spawn(&mut command).expect("spawn sync managed child");
        let descendant = wait_for_pid_sync(&pid_path);

        drop(managed);

        assert_process_terminated_sync(descendant);
    }

    #[test]
    fn managed_scope_process_group_selection_preserves_macos_outer_scope() {
        assert!(!should_create_inner_process_group(true, true));
        assert!(should_create_inner_process_group(true, false));
        assert!(should_create_inner_process_group(false, true));
        assert!(should_create_inner_process_group(false, false));
    }

    #[cfg(unix)]
    #[test]
    fn sync_managed_wait_discards_stale_group_authority_after_direct_reap() {
        use std::os::unix::process::CommandExt;

        let mut victim_command = Command::new("sh");
        victim_command
            .args(["-c", "sleep 60"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let victim_child = victim_command.spawn().expect("spawn victim group");
        let victim_group = victim_child.id();
        let mut victim = SyncManagedChild {
            child: victim_child,
            process_group: Some(victim_group),
            reaped: false,
        };

        let mut completed_command = Command::new("sh");
        completed_command
            .args(["-c", "exit 0"])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let completed_child = completed_command.spawn().expect("spawn completed group");
        let completed_group = completed_child.id();
        let mut completed = SyncManagedChild {
            child: completed_child,
            process_group: Some(completed_group),
            reaped: false,
        };
        assert!(completed.child.wait().expect("direct reap").success());

        // Simulate reuse of the stored numeric PGID after another wait consumed
        // the child identity that established signalling authority.
        completed.process_group = Some(victim_group);
        assert!(completed
            .poll_exit()
            .expect("cached wait")
            .expect("completed status")
            .success());
        assert!(
            victim.child.try_wait().expect("inspect victim").is_none(),
            "sync Git wait signalled a process group after losing the leader identity"
        );

        victim.terminate_and_reap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_sync_leader_does_not_wait_for_descendant_owned_pipes() {
        let directory = tempdir().expect("successful leader fixture");
        let pid_file = directory.path().join("child.pid");
        let script = format!(
            "(trap '' HUP; sleep 30) & child=$!; printf '%s' \"$child\" > '{}'; exit 0",
            pid_file.display()
        );
        let mut command = Command::new("sh");
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();

        let output =
            run_process_bounded_sync(command, "successful leader fixture", Duration::from_secs(5))
                .expect("successful leader result");

        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = wait_for_pid(&pid_file).await;
        assert_process_terminated(pid).await;
    }

    #[cfg(windows)]
    #[test]
    fn sync_bounded_timeout_kills_windows_descendant_tree() {
        match std::env::var(SYNC_JOB_ROLE).as_deref() {
            Ok("leader") => {
                let mut descendant = Command::new(
                    std::env::current_exe().expect("current worktree test executable"),
                );
                descendant
                    .args(["--exact", SYNC_JOB_TEST, "--nocapture"])
                    .env(SYNC_JOB_ROLE, "descendant")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let mut descendant = descendant.spawn().expect("spawn sync descendant");
                std::fs::write(
                    std::env::var_os(SYNC_JOB_PID_PATH).expect("sync descendant pid path"),
                    descendant.id().to_string(),
                )
                .expect("write sync descendant pid");
                let _ = descendant.wait();
                return;
            }
            Ok("descendant") => loop {
                std::thread::sleep(Duration::from_secs(60));
            },
            _ => {}
        }

        let directory = tempdir().expect("sync Windows Job fixture");
        let pid_path = directory.path().join("descendant.pid");
        let mut command =
            Command::new(std::env::current_exe().expect("current worktree test executable"));
        command
            .args(["--exact", SYNC_JOB_TEST, "--nocapture"])
            .env(SYNC_JOB_ROLE, "leader")
            .env(SYNC_JOB_PID_PATH, &pid_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let error = run_process_bounded_sync(
            command,
            "Windows sync Job Object fixture",
            Duration::from_secs(5),
        )
        .expect_err("sync fixture must time out");
        assert!(error.contains("timed out"), "{error}");
        let descendant_id = std::fs::read_to_string(&pid_path)
            .expect("sync descendant pid")
            .parse::<u32>()
            .expect("numeric sync descendant pid");

        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        let descendant = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant_id) };
        if !descendant.is_null() {
            let wait = unsafe { WaitForSingleObject(descendant, 5_000) };
            unsafe {
                let _ = CloseHandle(descendant);
            }
            assert_eq!(wait, WAIT_OBJECT_0, "sync descendant survived timeout");
        }
    }
}
