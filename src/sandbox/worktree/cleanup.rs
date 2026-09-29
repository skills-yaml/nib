//! Managed worktree internals.

use super::*;

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

pub(crate) async fn capture_managed_worktree_receipt_async(
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

pub(crate) fn capture_worktree_registration_snapshot_from_common(
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

pub(crate) fn parse_common_git_directory(
    project_root: &Path,
    output: &Output,
) -> Result<PathBuf, String> {
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn capture_managed_worktree_receipt(
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

pub(crate) fn validate_reciprocal_worktree_link(
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

pub(crate) fn validate_reciprocal_worktree_link_opened(
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

pub(crate) fn open_stable_direct_child(
    path: &Path,
) -> Result<crate::daemons::state::StableDirectory, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("managed directory has no parent: {}", path.display()))?;
    crate::daemons::state::StableDirectory::open(parent)?.open_child(path)
}

pub(crate) fn trusted_git_registration_path(
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

pub(crate) fn parse_gitdir_pointer(contents: &[u8], label: &str) -> Result<PathBuf, String> {
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

pub(crate) fn parse_plain_path(contents: &[u8], label: &str) -> Result<PathBuf, String> {
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

pub(crate) fn read_small_stable_file(
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

pub(crate) fn remove_registered_worktree(
    project_root: &Path,
    id: &str,
    timeout: Duration,
) -> Result<(), String> {
    remove_registered_worktree_until(project_root, id, Instant::now() + timeout, timeout)
}

pub(crate) fn remove_registered_worktree_until(
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
pub(crate) fn remove_registered_worktree_precommit_until(
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

pub(crate) fn remove_registered_worktree_precommit_until_with_guard(
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

pub(crate) fn adopt_registered_worktree_branch(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn adopt_managed_worktree_branch(
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

pub(crate) fn remove_registered_worktree_reconciled(
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

pub(crate) fn prove_managed_worktree_absent(
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

pub(crate) fn prove_managed_worktree_namespace_absent_until(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn cleanup_managed_worktree_with_guard(
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
pub(crate) enum CleanupArtifact {
    Path,
    Registration,
    Branch,
}

pub(crate) fn persist_cleanup_artifact_phase(
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

pub(crate) fn validate_managed_worktree_directories(
    ownership: &ManagedWorktreeReceipt,
) -> Result<(), String> {
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

pub(crate) fn validate_directory_receipt(
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

pub(crate) fn validate_created_path(
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

pub(crate) async fn validate_created_worktree_async(
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

pub(crate) async fn validate_owned_worktree_async(
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

pub(crate) async fn compensate_failed_create(
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

pub(crate) fn compensate_failed_create_sync(
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

pub(crate) fn cleanup_partial_create_sync(
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

pub(crate) async fn cleanup_partial_create(
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

pub(crate) fn finish_cleanup_errors(errors: Vec<String>) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub(crate) fn cleanup_time_remaining(
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

pub(crate) async fn repository_root_bounded_cancellable(
    project_root: &Path,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<PathBuf, String> {
    let requested = canonical_requested_root(project_root)?;
    let output =
        run_git_cancellable(&requested, ["rev-parse", "--show-toplevel"], cancellation).await?;
    require_git_success(&output, "inspect repository")?;
    validate_reported_repository_root(&requested, &output.stdout)
}

pub(crate) fn repository_root_bounded_sync(project_root: &Path) -> Result<PathBuf, String> {
    let requested = canonical_requested_root(project_root)?;
    let output = run_git_bounded_sync(&requested, ["rev-parse", "--show-toplevel"])?;
    require_git_success(&output, "inspect repository")?;
    validate_reported_repository_root(&requested, &output.stdout)
}

pub(crate) fn canonical_requested_root(project_root: &Path) -> Result<PathBuf, String> {
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

pub(crate) fn validate_reported_repository_root(
    requested: &Path,
    stdout: &[u8],
) -> Result<PathBuf, String> {
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

pub(crate) struct OwnedRefLock {
    pub(crate) directory: crate::daemons::state::StableDirectory,
    pub(crate) path: PathBuf,
    pub(crate) receipt: Option<crate::daemons::state::FilePublicationReceipt>,
    pub(crate) contents: Vec<u8>,
}

impl OwnedRefLock {
    pub(crate) fn acquire(
        directory: &crate::daemons::state::StableDirectory,
        path: PathBuf,
    ) -> Result<Self, String> {
        let contents = format!("nib-ref-lock {}\n", uuid::Uuid::new_v4()).into_bytes();
        Self::acquire_with_contents(directory, path, contents)
    }

    pub(crate) fn acquire_with_contents(
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

    pub(crate) fn release(&mut self) -> Result<(), String> {
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
pub(crate) fn take_owned_ref_lock_release_failure(path: &Path) -> bool {
    let mut failures = OWNED_REF_LOCK_RELEASE_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let matched = failures
        .iter()
        .find(|candidate| crate::fs_security::canonical_paths_match(path, candidate))
        .cloned();
    matched.is_some_and(|matched| failures.remove(&matched))
}

pub(crate) fn managed_ref_lock_contents(receipt_id: &str, reference: &str, role: &str) -> Vec<u8> {
    format!("nib-managed-ref-lock-v1\nreceipt={receipt_id}\nreference={reference}\nrole={role}\n")
        .into_bytes()
}

pub(crate) fn valid_managed_ref_lock_marker(contents: &[u8]) -> bool {
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

pub(crate) fn managed_ref_lock_marker_is_foreign(
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

pub(crate) fn acquire_owned_ref_protocol_lock(
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

pub(crate) fn recover_atomic_ref_scratch(
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

pub(crate) fn require_atomic_ref_scratch_absent(
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

pub(crate) fn recover_dead_marker_file(
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
