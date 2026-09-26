//! Managed worktree internals.

use super::*;

pub(crate) fn reconcile_removing_branch_from_anchor(
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

pub(crate) fn persist_intent_cleanup_phase(
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

pub(crate) fn intent_directory_quarantine_exists(
    parent: &Path,
    path: &Path,
) -> Result<bool, String> {
    crate::fs_security::directory_removal_quarantine_exists(parent, path)
        .map_err(|error| format!("failed to inspect durable intent cleanup quarantine: {error}"))
}

pub(crate) fn reconcile_unfinished_intent_path(
    revision: &mut DurableOwnershipRevision,
) -> Result<(), String> {
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn reconcile_unfinished_intent_branch(
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

pub(crate) fn prove_no_post_snapshot_registration(
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

pub(crate) fn reconcile_unfinished_intent(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn rehydrate_owned_worktree(
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

pub(crate) fn persist_adopted_branch_revision(
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
    pub(crate) project_root: PathBuf,
    pub(crate) ownership_receipt_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct WorktreePreparationAuthority {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) branch: String,
    pub(crate) branch_oid: String,
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

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
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

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
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

pub(crate) fn prove_worktree_destination_absent(parent: &Path, path: &Path) -> Result<(), String> {
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
