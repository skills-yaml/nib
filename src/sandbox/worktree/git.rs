//! Managed worktree internals.

use super::*;

pub(crate) fn recover_owned_ref_lock_location(
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

pub(crate) fn recover_owned_ref_restart_artifacts(
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

pub(crate) struct OwnedRefClaim {
    pub(crate) reference: String,
    pub(crate) expected_oid: String,
    pub(crate) common_directory: crate::daemons::state::StableDirectory,
    pub(crate) ref_directory: crate::daemons::state::StableDirectory,
    pub(crate) ref_path: PathBuf,
    pub(crate) target_lock_path: PathBuf,
    pub(crate) lock_owner: Option<String>,
    pub(crate) packed_lock: OwnedRefLock,
    pub(crate) target_lock: Option<OwnedRefLock>,
}

pub(crate) struct ReservedBranchPublication {
    pub(crate) path: PathBuf,
    pub(crate) file: std::fs::File,
    pub(crate) contents: Vec<u8>,
}

pub(crate) fn ref_names_conflict(existing: &[u8], requested: &[u8]) -> bool {
    existing == requested
        || (existing.starts_with(requested) && existing.get(requested.len()) == Some(&b'/'))
        || (requested.starts_with(existing) && requested.get(existing.len()) == Some(&b'/'))
}

pub(crate) fn valid_packed_object_id(value: &[u8]) -> bool {
    (40..=64).contains(&value.len()) && value.iter().all(u8::is_ascii_hexdigit)
}

pub(crate) fn packed_ref_namespace_conflict(
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
    pub(crate) fn release_locks(&mut self) -> Result<(), String> {
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

    pub(crate) fn fail<T>(&mut self, error: String) -> Result<T, String> {
        match self.release_locks() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; exact managed ref-lock cleanup failed: {cleanup}"
            )),
        }
    }
}

pub(crate) fn prepare_owned_ref_claim(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn claim_owned_ref_after_inspection(
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

pub(crate) fn remove_owned_file_receipt(
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

pub(crate) fn verify_open_file_contents(
    file: &std::fs::File,
    expected: &[u8],
) -> Result<(), String> {
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

pub(crate) fn validate_owned_ref_receipt(owned: &OwnedBranch) -> Result<(), String> {
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

pub(crate) fn capture_owned_branch_revision(
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

pub(crate) fn describe_existing_owned_ref(
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

pub(crate) fn remove_owned_ref_receipt(owned: &OwnedBranch) -> Result<(), String> {
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

pub(crate) fn owned_ref_namespace_is_absent(owned: &OwnedBranch) -> Result<bool, String> {
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

pub(crate) fn delete_owned_ref_with_receipt(
    owned: &OwnedBranch,
    deadline: Instant,
) -> Result<(), String> {
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

pub(crate) fn stage_reserved_branch_publication(
    reservation: &mut ManagedWorktreeReservation,
) -> Result<ReservedBranchPublication, String> {
    stage_reserved_branch_publication_with_hook(reservation, || {})
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn stage_reserved_branch_publication_with_hook(
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

pub(crate) async fn create_reserved_worktree_branch(
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

pub(crate) fn parse_git_oid(output: &Output, operation: &str) -> Result<String, String> {
    require_git_success(output, operation)?;
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !(40..=64).contains(&oid.len()) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("git {operation} returned an invalid object ID"));
    }
    Ok(oid)
}

pub(crate) fn parse_symbolic_head(output: &Output) -> Result<String, String> {
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

pub(crate) fn validate_owned_worktree_oid(
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

pub(crate) async fn run_git_cancellable<I, S>(
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

pub(crate) async fn run_git_controlled<I, S>(
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

pub(crate) fn run_git_bounded_sync_with_timeout_controlled<I, S>(
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

pub(crate) async fn validate_managed_git_configuration(
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

pub(crate) fn validate_managed_git_configuration_sync(
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

pub(crate) fn validate_worktree_config_activation(output: &Output) -> Result<bool, String> {
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

pub(crate) fn validate_executable_git_config_output(output: &Output) -> Result<(), String> {
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

pub(crate) fn managed_git_time_remaining(
    deadline: Instant,
    timeout: Duration,
) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            format!(
                "managed Git command exceeded {} seconds during configuration validation",
                timeout.as_secs_f64()
            )
        })
}

pub(crate) fn configure_git_command(
    command: &mut tokio::process::Command,
    cwd: &Path,
    args: &[OsString],
) {
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

pub(crate) fn configure_git_command_sync(command: &mut Command, cwd: &Path, args: &[OsString]) {
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

pub(crate) fn git_policy_args() -> Vec<OsString> {
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
pub(crate) fn git_null_device() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
pub(crate) fn git_null_device() -> &'static str {
    "/dev/null"
}

#[cfg(test)]
pub(crate) async fn run_process_bounded(
    command: tokio::process::Command,
    label: &str,
    command_timeout: Duration,
) -> Result<Output, String> {
    run_process_bounded_controlled(command, label, command_timeout, None).await
}

pub(crate) async fn run_process_bounded_controlled(
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

pub(crate) async fn drain_async_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
) -> Result<Vec<u8>, String> {
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

pub(crate) struct SyncManagedChild {
    pub(crate) child: std::process::Child,
    #[cfg(unix)]
    pub(crate) process_group: Option<u32>,
    #[cfg(windows)]
    pub(crate) windows_job: Option<crate::sandbox::windows_job::WindowsJob>,
    pub(crate) reaped: bool,
}

#[cfg(any(unix, test))]
pub(crate) fn should_create_inner_process_group(is_macos: bool, managed_scope: bool) -> bool {
    !(is_macos && managed_scope)
}

#[cfg(unix)]
pub(crate) fn create_git_process_group() -> bool {
    should_create_inner_process_group(
        cfg!(target_os = "macos"),
        std::env::var_os("NIB_MANAGED_PROCESS_SCOPE").is_some(),
    )
}

impl SyncManagedChild {
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<Self> {
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
        let (child, windows_job) = crate::sandbox::windows_job::spawn_contained_std(command)?;
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

    pub(crate) fn poll_exit(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
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

    pub(crate) fn terminate_and_reap(&mut self) {
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
    pub(crate) fn signal_owned_process_group(&mut self) {
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
pub(crate) fn run_process_bounded_sync(
    command: Command,
    label: &str,
    command_timeout: Duration,
) -> Result<Output, String> {
    run_process_bounded_sync_controlled(command, label, command_timeout, None)
}

pub(crate) fn run_process_bounded_sync_controlled(
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

pub(crate) fn spawn_sync_reader<R>(reader: R) -> std::sync::mpsc::Receiver<Result<Vec<u8>, String>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(drain_sync_bounded(reader));
    });
    receiver
}

pub(crate) fn receive_sync_reader(
    receiver: std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
    label: &str,
    stream: &str,
) -> Result<Vec<u8>, String> {
    receiver
        .recv_timeout(Duration::from_secs(2))
        .map_err(|error| format!("git {label} {stream} reader did not finish: {error}"))?
}
