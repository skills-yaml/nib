//! Split for T043 C02.

use super::*;

#[cfg(test)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn cleanup_precommit_record_with_timeout_and_hooks(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    timeout: Duration,
    before_quarantine: impl FnOnce() -> Result<(), String>,
    mut before_namespace_step: impl FnMut() -> Result<(), String>,
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
    with_subagent_record_lock_in_timeout(
        &project_root,
        &attempted_record.id,
        &records,
        timeout,
        |directory, deadline| {
            let quarantine = directory.deterministic_artifact_path(
                &path,
                ".nib-subagent-precommit-delete-",
                ".quarantine",
            )?;
            let canonical_exists = directory.path_exists(&path)?;
            let quarantine_exists = directory.path_exists(&quarantine)?;
            if !canonical_exists && !quarantine_exists {
                return Ok(());
            }
            let expected_publication = expected_publication.ok_or_else(|| {
                format!(
                    "exact precommit subagent record publication identity is unavailable; preserved {}",
                    path.display()
                )
            })?;
            if !canonical_exists {
                let quarantined = directory.open_read_write(&quarantine)?;
                if !crate::daemons::state::same_open_file_identity(
                    expected_publication,
                    &quarantined,
                )? {
                    return Err(format!(
                        "precommit subagent deletion quarantine has an unexpected identity; preserved {}",
                        quarantine.display()
                    ));
                }
                verify_open_subagent_record_bytes(&quarantined, &attempted_bytes)?;
                return directory.remove_visible_file_if_matches_direct_with_guard(
                    &quarantine,
                    &quarantined,
                    || {
                        before_namespace_step()?;
                        ensure_subagent_reconciliation_deadline(Some(deadline))?;
                        verify_open_subagent_record_bytes(&quarantined, &attempted_bytes)
                    },
                );
            }
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
            let mut before_quarantine = Some(before_quarantine);
            directory.remove_file_if_matches_with_guard(
                &path,
                expected_publication,
                ".nib-subagent-precommit-delete-",
                || {
                    if let Some(before_quarantine) = before_quarantine.take() {
                        before_quarantine()?;
                    }
                    before_namespace_step()?;
                    ensure_subagent_reconciliation_deadline(Some(deadline))?;
                    verify_open_subagent_record_bytes(expected_publication, &attempted_bytes)
                },
            )
        },
    )
}

pub(crate) fn verify_open_subagent_record_bytes(
    file: &File,
    expected: &[u8],
) -> Result<(), String> {
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn merge_verified_subagent_worktree(
    args: &Value,
    project_root: &Path,
    evidence: VerificationEvidence,
    cancellation: Option<&crate::agent::CancellationSignal>,
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
    let _merge_lock = RepositoryMergeLock::acquire(&project_root, cancellation).await?;
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
    let record = get_subagent_record_internal(project_root, subagent_id)?;
    validate_record_worktree(&canonical_project_root(project_root)?, &record)?;
    let store = crate::session::SessionStore::for_project(&record.worktree_path)?;
    store
        .try_append_message_with_origin(
            &record.child_session_id,
            "user",
            message,
            crate::session::MessageOrigin::ToolOutput,
        )
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "status": "sent",
        "subagent_id": record.id,
        "child_session_id": record.child_session_id,
    }))
}
