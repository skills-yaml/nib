//! Split for T043 C02.

use super::*;

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn reconcile_spawn_preparations(
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

pub(crate) fn recover_spawn_preparation_transactions(
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

pub(crate) fn validate_spawn_preparation_temporary_artifacts(
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

pub(crate) fn read_spawn_preparation_bytes(file: &File, path: &Path) -> Result<Vec<u8>, String> {
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

pub(crate) fn validate_spawn_preparation_revision_successor(
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

pub(crate) fn remove_spawn_preparation_entry(
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

pub(crate) fn validate_spawn_intent_record_identity(
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

pub(crate) fn remove_spawn_preparation_intent(
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

pub(crate) fn validate_subagent_audit_argument_pair(
    args: &Value,
) -> Result<Option<String>, String> {
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

pub(crate) fn preflight_subagent_audit_target(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn commit_subagent_audit_target(
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

pub(crate) fn cleanup_session_preparation(
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
pub(crate) fn prepare_subagent_audit_target(
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

pub(crate) fn subagent_audit_target_for_store(
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

pub(crate) fn serialize_exact_subagent_audit_destination<T: Serialize + ?Sized>(
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
