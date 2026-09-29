//! Split for T043 C02.

use super::*;

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

pub(crate) fn sweep_owner_lease_artifacts(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn sweep_owner_lease_artifacts_with_timeout_and_guard(
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

pub(crate) fn get_subagent_record_internal(
    project_root: &Path,
    id: &str,
) -> Result<SubagentRecord, String> {
    let project_root = canonical_project_root(project_root)?;
    let records = ensure_records_directory_capability_until(&project_root, None)?;
    reconcile_spawn_preparations(&project_root, &records)?;
    let record = reconcile_subagent_ownership(&project_root, id)?;
    records.verify_visible()?;
    sync_subagent_task_manager(&record);
    Ok(record)
}

pub(crate) fn public_subagent_record(mut record: SubagentRecord) -> SubagentRecord {
    record.execution_generation = None;
    record.owner_lease = None;
    record.result = record
        .result
        .take()
        .and_then(project_public_subagent_result);
    record
}

pub(crate) fn project_public_subagent_result(result: Value) -> Option<Value> {
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

pub(crate) fn sync_subagent_task_manager(record: &SubagentRecord) {
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

pub(crate) fn get_opened_subagent_record(
    project_root: &Path,
    id: &str,
) -> Result<OpenedSubagentRecord, String> {
    reconcile_subagent_ownership(project_root, id)?;
    let records = ensure_records_directory(project_root)?;
    let directory = crate::daemons::state::StableDirectory::open(&records)?;
    let path = record_path(project_root, id)?;
    read_opened_subagent_record_in(&directory, &path)
}

pub(crate) fn get_subagent_record_unreconciled(
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

pub(crate) fn subagent_audit_session_id(record: &SubagentRecord) -> &str {
    record
        .parent_session_id
        .as_deref()
        .unwrap_or(&record.child_session_id)
}

pub(crate) fn resolve_legacy_subagent_audit_target(
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

pub(crate) fn subagent_reconciliation_id(
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

pub(crate) fn ownership_reconciliation_evidence(
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

pub(crate) fn open_subagent_audit_store_until(
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

pub(crate) fn subagent_audit_target(
    record: &SubagentRecord,
) -> Result<Option<SubagentAuditTarget>, String> {
    let Some(target) = process_scope_retirement_result(record)
        .and_then(|result| result.get(OWNERSHIP_AUDIT_TARGET_KEY))
    else {
        return Ok(None);
    };
    serde_json::from_value(target.clone())
        .map(Some)
        .map_err(|error| format!("subagent ownership audit target is invalid: {error}"))
}

pub(crate) fn set_subagent_audit_target(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn legacy_running_reconciliation_evidence(
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

pub(crate) fn no_ownership_reconciliation_work(
    record: &SubagentRecord,
) -> OwnershipReconciliationWork {
    OwnershipReconciliationWork {
        record: record.clone(),
        evidence: None,
        acquired_owner_lease: None,
        retry_persisted_owner_cleanup: false,
    }
}

pub(crate) fn process_scope_retirement_result_mut(
    record: &mut SubagentRecord,
) -> Option<&mut serde_json::Map<String, Value>> {
    let result = record.result.as_mut()?.as_object_mut()?;
    if matches!(record.status.as_str(), MERGE_PENDING_STATUS | "merged") {
        result.get_mut("subagent_result")?.as_object_mut()
    } else {
        Some(result)
    }
}

pub(crate) fn reconcile_subagent_ownership(
    project_root: &Path,
    id: &str,
) -> Result<SubagentRecord, String> {
    reconcile_subagent_ownership_with_owner_state(project_root, id, false)
}

pub(crate) fn reconcile_subagent_ownership_until(
    project_root: &Path,
    id: &str,
    deadline: Instant,
) -> Result<SubagentRecord, String> {
    reconcile_subagent_ownership_with_owner_state_until(project_root, id, false, Some(deadline))
}

pub(crate) fn reconcile_subagent_ownership_with_owner_state(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn reconcile_subagent_ownership_with_owner_state_until(
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

pub(crate) fn with_subagent_reconciliation_lock_in<T>(
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

pub(crate) fn with_subagent_record_lock_in_timeout<T>(
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

pub(crate) fn with_subagent_record_lock_bridge_in_deadline<T>(
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

pub(crate) fn with_modern_subagent_record_lock_in<T>(
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

pub(crate) fn ensure_subagent_reconciliation_deadline(
    deadline: Option<Instant>,
) -> Result<(), String> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(subagent_reconciliation_deadline_elapsed());
    }
    Ok(())
}

pub(crate) fn subagent_reconciliation_deadline_elapsed() -> String {
    "subagent cancellation reconciliation deadline elapsed".to_string()
}

pub(crate) fn open_process_scope_store_until(
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

pub(crate) fn record_subagent_ownership_reconciliation_event(
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
