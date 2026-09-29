//! Managed worktree ownership records.

use super::*;

pub(crate) const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CANCELLED_CREATE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const SYNC_CREATE_CLEANUP_TIMEOUT: Duration = GIT_COMMAND_TIMEOUT;
pub(crate) const MAX_GIT_OUTPUT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_PACKED_REFS_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_WORKTREE_REGISTRATIONS: usize = 4096;
pub(crate) const MAX_WORKTREE_REGISTRATION_NAME_BYTES: usize = 1024 * 1024;
pub(crate) const MANAGED_WORKTREE_OWNERSHIP_SCHEMA_VERSION: u32 = 2;
pub(crate) const MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_MANAGED_WORKTREE_OWNERSHIP_RECORDS: usize = 64;
pub(crate) const MAX_MANAGED_WORKTREE_OWNERSHIP_AGGREGATE_BYTES: u64 =
    MAX_MANAGED_WORKTREE_OWNERSHIP_RECORDS as u64 * MAX_MANAGED_WORKTREE_OWNERSHIP_BYTES;
pub(crate) const MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_ENTRIES: usize = 8192;
pub(crate) const MAX_MANAGED_WORKTREE_OWNERSHIP_DIRECTORY_NAME_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MANAGED_WORKTREE_OWNERSHIP_DIRECTORY: &str = "worktree-ownership";
pub(crate) const MANAGED_WORKTREE_OWNERSHIP_TEMPORARY_PREFIX: &str = ".nib-worktree-ownership-";
pub(crate) const MANAGED_REF_LOCK_TEMPORARY_PREFIX: &str = ".nib-ref-lock-";
pub(crate) const MANAGED_REF_LOCK_DELETE_PREFIX: &str = ".nib-ref-lock-delete-";
pub(crate) const RESERVED_REF_TEMPORARY_PREFIX: &str = ".nib-reserved-ref-";
pub(crate) const RESERVED_REF_DELETE_PREFIX: &str = ".nib-reserved-ref-delete-";
pub(crate) const MAX_MANAGED_REF_LOCK_DIRECTORY_ENTRIES: usize = 4096;
pub(crate) const MAX_MANAGED_REF_LOCK_DIRECTORY_NAME_BYTES: usize = 1024 * 1024;
pub(crate) const WORKTREE_CREATE_CANCELLED: &str = "subagent worktree creation cancelled";
pub(crate) const EXECUTABLE_GIT_CONFIG_PATTERN: &str = concat!(
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
pub(crate) static SYNC_POST_ADD_VALIDATION_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
pub(crate) static SYNC_POST_ADD_BRANCH_MOVES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) static SYNC_POST_ADD_BRANCH_SYMREFS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) static SYNC_BEFORE_ADD_DESTINATION_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(crate) static SYNC_POST_CAPTURE_PATH_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
pub(crate) static SYNC_POST_CAPTURE_REGISTRATION_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
pub(crate) static SYNC_AFTER_DESTINATION_PUBLICATION_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
pub(crate) static SYNC_AFTER_REGISTRATION_SNAPSHOT_FORGERIES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
pub(crate) static OWNED_REF_LOCK_RELEASE_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
pub(crate) static BEFORE_REF_PUBLICATION_SYMREFS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub(crate) type WorktreeOwnershipKey = (PathBuf, String);
pub(crate) type WorktreeOwnershipMap =
    std::collections::HashMap<WorktreeOwnershipKey, Arc<ManagedWorktreeReceipt>>;

pub(crate) static WORKTREE_OWNERSHIP: std::sync::LazyLock<std::sync::Mutex<WorktreeOwnershipMap>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManagedWorktreeKind {
    Subagent,
    Session,
}

impl ManagedWorktreeKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DurableArtifactPhase {
    Reserved,
    Present,
    Removing,
    Removed,
    Unattributed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DurableOwnershipPhase {
    Intent,
    Owned,
    Cleanup,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurablePreviousBranchAnchor {
    pub(crate) path: PathBuf,
    pub(crate) identity: crate::fs_security::FileIdentitySnapshot,
    pub(crate) oid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DurableManagedWorktreeOwnership {
    pub(crate) schema_version: u32,
    pub(crate) receipt_id: String,
    pub(crate) kind: ManagedWorktreeKind,
    pub(crate) logical_id: String,
    pub(crate) phase: DurableOwnershipPhase,
    pub(crate) project_root: PathBuf,
    pub(crate) common_git_dir: PathBuf,
    pub(crate) common_git_identity: crate::fs_security::DirectoryIdentity,
    pub(crate) worktree_path: PathBuf,
    pub(crate) worktree_staging_path: PathBuf,
    pub(crate) worktree_identity: Option<crate::fs_security::DirectoryIdentity>,
    pub(crate) registration_path: Option<PathBuf>,
    pub(crate) registration_identity: Option<crate::fs_security::DirectoryIdentity>,
    pub(crate) registration_namespace_identity: Option<crate::fs_security::DirectoryIdentity>,
    pub(crate) registration_snapshot_captured: bool,
    pub(crate) preexisting_registration_name_hashes: Vec<String>,
    pub(crate) preexisting_registration_identities: Vec<crate::fs_security::DirectoryIdentity>,
    pub(crate) branch_reference: String,
    pub(crate) branch_staging_path: PathBuf,
    pub(crate) branch_anchor_generation: u64,
    pub(crate) previous_branch_anchor: Option<DurablePreviousBranchAnchor>,
    pub(crate) branch_identity: Option<crate::fs_security::FileIdentitySnapshot>,
    pub(crate) initial_oid: String,
    pub(crate) current_oid: String,
    pub(crate) path_cleanup: DurableArtifactPhase,
    pub(crate) registration_cleanup: DurableArtifactPhase,
    pub(crate) branch_cleanup: DurableArtifactPhase,
}

#[derive(Debug)]
pub(crate) struct DurableOwnershipRevision {
    pub(crate) directory: crate::daemons::state::StableDirectory,
    pub(crate) path: PathBuf,
    pub(crate) file: std::fs::File,
    pub(crate) record: DurableManagedWorktreeOwnership,
}

pub(crate) struct ManagedWorktreeIntent {
    pub(crate) revision: DurableOwnershipRevision,
}

pub(crate) struct ManagedWorktreeReservation {
    pub(crate) intent: ManagedWorktreeIntent,
    pub(crate) registration_snapshot: ManagedWorktreeRegistrationSnapshot,
}

#[derive(Clone)]
pub(crate) struct BlockingGitCancellation {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) upstream: Option<crate::agent::CancellationSignal>,
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
pub(crate) struct OwnedRefReceipt {
    pub(crate) common_directory: crate::daemons::state::StableDirectory,
    pub(crate) directory: crate::daemons::state::StableDirectory,
    pub(crate) path: PathBuf,
    pub(crate) file: std::fs::File,
    pub(crate) anchor_path: Option<PathBuf>,
    pub(crate) anchor_file: Option<std::fs::File>,
    pub(crate) lock_owner: Option<String>,
    pub(crate) contents: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct OwnedBranch {
    pub(crate) reference: String,
    pub(crate) expected_oid: String,
    pub(crate) receipt: Arc<OwnedRefReceipt>,
}

#[derive(Debug)]
pub(crate) struct ManagedWorktreeReceipt {
    pub(crate) path: PathBuf,
    pub(crate) path_receipt: Option<crate::fs_security::DirectoryRemovalReceipt>,
    pub(crate) registration_path: PathBuf,
    pub(crate) registration_receipt: Option<crate::fs_security::DirectoryRemovalReceipt>,
    pub(crate) state: std::sync::Mutex<ManagedWorktreeState>,
}

#[derive(Debug)]
pub(crate) struct ManagedWorktreeState {
    pub(crate) owned_branch: Option<OwnedBranch>,
    pub(crate) path_removed: bool,
    pub(crate) registration_removed: bool,
    pub(crate) branch_removed: bool,
    pub(crate) reciprocal_link_proven: bool,
    pub(crate) durable: Option<DurableOwnershipRevision>,
}

#[derive(Debug)]
pub(crate) struct ManagedWorktreeCaptureError {
    pub(crate) message: String,
    pub(crate) ownership: Option<Box<ManagedWorktreeReceipt>>,
}

pub(crate) struct ManagedWorktreeRegistrationSnapshot {
    pub(crate) common_directory: crate::daemons::state::StableDirectory,
    pub(crate) registrations: Option<ExistingWorktreeRegistrations>,
}

pub(crate) struct ExistingWorktreeRegistrations {
    pub(crate) directory: crate::daemons::state::StableDirectory,
    pub(crate) entries: std::collections::HashMap<OsString, crate::fs_security::DirectoryIdentity>,
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

pub(crate) fn managed_worktree_ownership_directory(
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

pub(crate) fn managed_worktree_ownership_path(
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

pub(crate) fn encoded_name_hash(name: &OsStr) -> String {
    let digest = Sha256::digest(name.as_encoded_bytes());
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

pub(crate) fn valid_git_oid(oid: &str) -> bool {
    (40..=64).contains(&oid.len()) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn reservation_worktree_staging_path(
    worktree_path: &Path,
    receipt_id: &str,
) -> Result<PathBuf, String> {
    let parent = worktree_path
        .parent()
        .ok_or("managed worktree path has no parent")?;
    Ok(parent.join(format!(".nib-worktree-reservation-{receipt_id}")))
}

pub(crate) fn canonical_managed_worktree_reservation_paths(
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

pub(crate) fn managed_branch_paths(
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

pub(crate) fn describe_reserved_branch_conflict(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn validate_durable_ownership_record(
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

pub(crate) fn encode_durable_ownership(
    record: &DurableManagedWorktreeOwnership,
) -> Result<Vec<u8>, String> {
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

pub(crate) fn publish_durable_ownership(
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

pub(crate) fn decode_durable_ownership_file(
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

pub(crate) fn read_durable_ownership_file(
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

pub(crate) fn lock_dead_ownership_artifact(
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

pub(crate) fn recover_durable_ownership_transaction(
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

pub(crate) fn recover_all_durable_ownership_transactions(
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

pub(crate) fn load_durable_ownership_revision(
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

pub(crate) const OWNERSHIP_COMPACTION_LOCK_NAME: &str = ".nib-worktree-ownership-compaction.lock";
pub(crate) const OWNERSHIP_COMPACTION_ANCHOR_NAME: &str =
    ".nib-worktree-ownership-compaction.anchor";

pub(crate) struct OwnershipCompactionLock {
    pub(crate) _anchor: std::fs::File,
}

impl OwnershipCompactionLock {
    pub(crate) fn acquire(
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

fn recover_complete_ref_artifacts_if_current(
    record: &DurableManagedWorktreeOwnership,
) -> Result<(), String> {
    let current = crate::daemons::state::StableDirectory::open(&record.common_git_dir)?;
    if current.directory_removal_receipt()?.identity() != record.common_git_identity {
        return Ok(());
    }
    recover_owned_ref_restart_artifacts(record)
}

pub(crate) fn compact_complete_ownership_records_with_limits(
    directory: &crate::daemons::state::StableDirectory,
    project_root: &Path,
    target_path: &Path,
    replacement_bytes: u64,
    max_records: usize,
    max_aggregate_bytes: u64,
) -> Result<(), String> {
    struct Candidate {
        pub(crate) path: PathBuf,
        pub(crate) file: std::fs::File,
        pub(crate) bytes: u64,
        pub(crate) record: DurableManagedWorktreeOwnership,
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
                recover_complete_ref_artifacts_if_current(&record)?;
                candidates.push(Candidate {
                    path,
                    file,
                    bytes,
                    record,
                });
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
        recover_owned_ref_restart_artifacts(&candidate.record)?;
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

pub(crate) fn compact_complete_ownership_records(
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

pub(crate) fn persist_durable_ownership_revision(
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
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn persist_managed_worktree_reservation(
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

pub(crate) fn reserve_managed_worktree_sync_controlled_with_receipt(
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

pub(crate) async fn reserve_managed_worktree_cancellable_with_receipt(
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

pub(crate) fn reopen_directory_receipt(
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

pub(crate) fn reopen_common_git_directory(
    record: &DurableManagedWorktreeOwnership,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let common = crate::daemons::state::StableDirectory::open(&record.common_git_dir)?;
    if common.directory_removal_receipt()?.identity() != record.common_git_identity {
        return Err("managed worktree common Git directory identity changed".to_string());
    }
    Ok(common)
}

pub(crate) fn reopen_owned_branch(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn promote_durable_intent(
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

pub(crate) fn reopen_durable_directory_artifact(
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

pub(crate) fn durable_branch_is_absent(
    record: &DurableManagedWorktreeOwnership,
) -> Result<bool, String> {
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

pub(crate) fn reconcile_previous_branch_anchor(
    revision: &mut DurableOwnershipRevision,
) -> Result<(), String> {
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

pub(crate) fn persist_durable_branch_removed(
    revision: &mut DurableOwnershipRevision,
) -> Result<(), String> {
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

pub(crate) fn with_owned_ref_namespace_locks<T>(
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
