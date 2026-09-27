//! Split for T043 C02.

use super::*;

pub fn cancel_subagent(project_root: &Path, id: &str) -> Result<Value, String> {
    cancellation_result(resolve_subagent_cancellation(project_root, id))
}

pub(crate) async fn cancel_subagent_async(project_root: &Path, id: &str) -> Result<Value, String> {
    cancellation_result(resolve_subagent_cancellation_async(project_root, id).await)
}

pub(crate) fn cancellation_result(resolution: CancelSubagentResolution) -> Result<Value, String> {
    match resolution {
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

pub(crate) fn resolve_subagent_cancellation_async(
    project_root: &Path,
    id: &str,
) -> impl std::future::Future<Output = CancelSubagentResolution> + Send + 'static {
    resolve_subagent_cancellation_async_with_start_hook(
        project_root,
        id,
        subagent_cancellation_reconciliation_timeout(),
        || {},
    )
}

pub(crate) fn resolve_subagent_cancellation_async_with_start_hook(
    project_root: &Path,
    id: &str,
    timeout: Duration,
    before_reconciliation: impl FnOnce() + Send + 'static,
) -> impl std::future::Future<Output = CancelSubagentResolution> + Send + 'static {
    // This deadline is intentionally derived before constructing the future. A
    // caller that cannot poll the future or start its worker promptly must not
    // receive a fresh reconciliation budget later.
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    let project_root = project_root.to_path_buf();
    let id = id.to_string();
    let reconciliation_id = id.clone();
    async move {
        run_subagent_cancellation_worker(id, move || {
            before_reconciliation();
            resolve_subagent_cancellation_until(&project_root, &reconciliation_id, deadline)
        })
        .await
    }
}

pub(crate) async fn run_subagent_cancellation_worker(
    id: String,
    reconcile: impl FnOnce() -> CancelSubagentResolution + Send + 'static,
) -> CancelSubagentResolution {
    let (resolution_tx, resolution_rx) = tokio::sync::oneshot::channel();
    let worker = match std::thread::Builder::new()
        .name("nib-subagent-cancellation".to_string())
        .spawn(move || {
            let resolution = reconcile();
            let _ = resolution_tx.send(resolution);
        }) {
        Ok(worker) => worker,
        Err(error) => {
            return subagent_cancellation_worker_failure(
                &id,
                format!("failed to start reconciliation worker: {error}"),
            );
        }
    };
    let mut worker = SubagentCancellationWorker::new(worker);
    let resolution = resolution_rx.await;
    let joined = worker.join();
    match (resolution, joined) {
        (_, Err(error)) => subagent_cancellation_worker_failure(&id, error),
        (Err(error), Ok(())) => subagent_cancellation_worker_failure(
            &id,
            format!("reconciliation worker stopped without a resolution: {error}"),
        ),
        (Ok(resolution), Ok(())) => resolution,
    }
}

pub(crate) fn subagent_cancellation_worker_failure(
    id: &str,
    error: String,
) -> CancelSubagentResolution {
    let manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
    CancelSubagentResolution::Unresolved {
        manager_stopped: manager_status
            .as_deref()
            .is_some_and(is_stopped_task_status),
        observed_status: None,
        error: format!("subagent cancellation reconciliation worker failed: {error}"),
    }
}

pub(crate) fn resolve_subagent_cancellation(
    project_root: &Path,
    id: &str,
) -> CancelSubagentResolution {
    let started = Instant::now();
    let deadline = started
        .checked_add(subagent_cancellation_reconciliation_timeout())
        .unwrap_or(started);
    resolve_subagent_cancellation_until(project_root, id, deadline)
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn resolve_subagent_cancellation_until(
    project_root: &Path,
    id: &str,
    deadline: Instant,
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
    let initial = match reconcile_subagent_ownership_until(&project_root, id, deadline) {
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
        if Instant::now() >= deadline {
            reconciliation_errors.push(subagent_reconciliation_deadline_elapsed());
            break;
        }
        manager_status = crate::daemons::task::TASK_MANAGER.get_status(id);
        match reconcile_subagent_ownership_until(&project_root, id, deadline) {
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
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                reconciliation_errors.push(subagent_reconciliation_deadline_elapsed());
                break;
            };
            std::thread::sleep(Duration::from_millis(10).min(remaining));
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

pub(crate) fn terminal_manager_status_matches(
    record_status: &str,
    manager_status: Option<&str>,
) -> bool {
    match manager_status {
        None => true,
        Some("failed") => record_status == "failed",
        Some("completed") => record_status != "failed",
        Some(_) => false,
    }
}

pub(crate) fn is_terminal_subagent_status(status: &str) -> bool {
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

pub(crate) fn is_stopped_task_status(status: &str) -> bool {
    matches!(status, "cancelled" | "completed" | "failed")
}

pub fn write_subagent_record(project_root: &Path, record: &SubagentRecord) -> Result<(), String> {
    write_subagent_record_with_refresh_hook(project_root, record, || Ok(()))
        .map(drop)
        .map_err(|error| error.message)
}

pub(crate) fn write_subagent_record_with_refresh_hook(
    project_root: &Path,
    record: &SubagentRecord,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<InitialSubagentRecordPublication, InitialSubagentRecordPublicationError> {
    write_subagent_record_with_refresh_hook_and_timeout(
        project_root,
        record,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        after_publication,
    )
}

pub(crate) fn write_subagent_record_with_refresh_hook_and_timeout(
    project_root: &Path,
    record: &SubagentRecord,
    timeout: Duration,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<InitialSubagentRecordPublication, InitialSubagentRecordPublicationError> {
    write_subagent_record_with_refresh_hooks_and_timeout(
        project_root,
        record,
        timeout,
        || Ok(()),
        after_publication,
    )
}

pub(crate) fn write_subagent_record_with_refresh_hooks_and_timeout(
    project_root: &Path,
    record: &SubagentRecord,
    timeout: Duration,
    mut before_namespace_step: impl FnMut() -> Result<(), String>,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<InitialSubagentRecordPublication, InitialSubagentRecordPublicationError> {
    let project_root = canonical_project_root(project_root).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
            publication_attempted: false,
        }
    })?;
    let path = record_path(&project_root, &record.id).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
            publication_attempted: false,
        }
    })?;
    let records_directory = ensure_records_directory(&project_root).map_err(|message| {
        InitialSubagentRecordPublicationError {
            message,
            receipt: None,
            publication_attempted: false,
        }
    })?;
    let mut rescued_receipt = None;
    let result = with_subagent_record_lock_in_timeout(
        &project_root,
        &record.id,
        &records_directory,
        timeout,
        |directory, deadline| {
            let receipt = match write_subagent_record_unlocked_with_receipt_and_guard(
                &project_root,
                directory,
                &path,
                record,
                crate::daemons::state::FileExpectation::Missing,
                Some(deadline),
                &mut before_namespace_step,
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
            publication_attempted: true,
        }),
    }
}

pub(crate) fn write_subagent_record_unlocked_until(
    project_root: &Path,
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    record: &SubagentRecord,
    expected: crate::daemons::state::FileExpectation<'_>,
    deadline: Option<Instant>,
) -> Result<crate::daemons::state::FilePublicationReceipt, String> {
    let publication = write_subagent_record_unlocked_with_receipt(
        project_root,
        directory,
        path,
        record,
        expected,
        deadline,
    );
    let error = match publication {
        Ok(receipt) => return Ok(receipt),
        Err(error) => error,
    };
    let Some(receipt) = error.receipt.filter(|receipt| receipt.exact_identity) else {
        return Err(error.message);
    };
    ensure_subagent_reconciliation_deadline(deadline)?;
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
    ensure_subagent_reconciliation_deadline(deadline)?;
    let mut finalization_guard = || {
        pause_subagent_record_finalization(&record.id)?;
        ensure_subagent_reconciliation_deadline(deadline)
    };
    if let Err(recovery) = directory.finalize_failed_exact_publication_with_guard(
        path,
        previous_expected,
        &receipt,
        ".nib-subagent-",
        &expected_bytes,
        &mut finalization_guard,
    ) {
        return Err(format!(
            "{}; exact publication recovery failed and all ambiguous state was preserved: {recovery}",
            error.message
        ));
    }
    ensure_subagent_reconciliation_deadline(deadline)?;
    let reopened = read_opened_subagent_record_in(directory, path).map_err(|recovery| {
        format!(
            "{}; finalized publication readback failed: {recovery}",
            error.message
        )
    })?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    validate_reopened_subagent_record(record, &reopened.record).map_err(|recovery| {
        format!(
            "{}; finalized publication record validation failed: {recovery}",
            error.message
        )
    })?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    if !crate::daemons::state::same_open_file_identity(&receipt.file, &reopened.file)? {
        return Err(format!(
            "{}; finalized publication was replaced before its authority could be adopted",
            error.message
        ));
    }
    ensure_subagent_reconciliation_deadline(deadline)?;
    directory.verify_file_identity(path, &receipt.file)?;
    ensure_subagent_reconciliation_deadline(deadline)?;
    Ok(receipt)
}

pub(crate) fn write_subagent_record_unlocked_with_receipt(
    project_root: &Path,
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    record: &SubagentRecord,
    expected: crate::daemons::state::FileExpectation<'_>,
    deadline: Option<Instant>,
) -> Result<
    crate::daemons::state::FilePublicationReceipt,
    crate::daemons::state::FilePublicationError,
> {
    write_subagent_record_unlocked_with_receipt_and_guard(
        project_root,
        directory,
        path,
        record,
        expected,
        deadline,
        &mut || Ok(()),
    )
}

pub(crate) fn write_subagent_record_unlocked_with_receipt_and_guard(
    project_root: &Path,
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    record: &SubagentRecord,
    expected: crate::daemons::state::FileExpectation<'_>,
    deadline: Option<Instant>,
    before_namespace_step: &mut impl FnMut() -> Result<(), String>,
) -> Result<
    crate::daemons::state::FilePublicationReceipt,
    crate::daemons::state::FilePublicationError,
> {
    ensure_subagent_reconciliation_deadline(deadline)?;
    let parent = path
        .parent()
        .ok_or_else(|| "subagent record has no parent".to_string())?;
    crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| format!("failed to resolve subagent records: {error}"))?;
    let within_project =
        crate::fs_security::canonical_path_starts_with(&canonical_parent, project_root)
            .map_err(|error| format!("failed to resolve subagent project root: {error}"))?;
    if crate::fs_security::metadata_is_link_or_reparse(&metadata)
        || !metadata.is_dir()
        || !within_project
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
    let publication = directory.save_bytes_atomically_expected_with_receipt_and_guard(
        path,
        &contents,
        ".nib-subagent-",
        expected,
        || {
            before_namespace_step()?;
            ensure_subagent_reconciliation_deadline(deadline)
        },
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

pub(crate) fn persist_subagent_record_revision(
    project_root: &Path,
    record: &SubagentRecord,
    expected_file: &mut File,
) -> Result<(), String> {
    persist_subagent_record_revision_with_refresh_hook(project_root, record, expected_file, || {
        Ok(())
    })
}

pub(crate) fn persist_subagent_record_revision_with_refresh_hook(
    project_root: &Path,
    record: &SubagentRecord,
    expected_file: &mut File,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    persist_subagent_record_revision_with_refresh_hook_and_timeout(
        project_root,
        record,
        expected_file,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        after_publication,
    )
}

pub(crate) fn persist_subagent_record_revision_with_refresh_hook_and_timeout(
    project_root: &Path,
    record: &SubagentRecord,
    expected_file: &mut File,
    timeout: Duration,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    persist_subagent_record_revision_with_refresh_hooks_and_timeout(
        project_root,
        record,
        expected_file,
        timeout,
        || Ok(()),
        after_publication,
    )
}

pub(crate) fn persist_subagent_record_revision_with_refresh_hooks_and_timeout(
    project_root: &Path,
    record: &SubagentRecord,
    expected_file: &mut File,
    timeout: Duration,
    mut before_namespace_step: impl FnMut() -> Result<(), String>,
    after_publication: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let path = record_path(project_root, &record.id)?;
    let records_directory = ensure_records_directory(project_root)?;
    let expected_bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| format!("failed to encode revised subagent record: {error}"))?;
    let mut rescued_receipt = None;
    let result = with_subagent_record_lock_in_timeout(
        project_root,
        &record.id,
        &records_directory,
        timeout,
        |directory, deadline| {
            let receipt = match write_subagent_record_unlocked_with_receipt_and_guard(
                project_root,
                directory,
                &path,
                record,
                crate::daemons::state::FileExpectation::Present(expected_file),
                Some(deadline),
                &mut before_namespace_step,
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
                    let mut namespace_guard =
                        || ensure_subagent_reconciliation_deadline(Some(deadline));
                    if let Err(recovery) = directory.finalize_failed_exact_publication_with_guard(
                        &path,
                        Some(expected_file),
                        &receipt,
                        ".nib-subagent-",
                        &expected_bytes,
                        &mut namespace_guard,
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

pub(crate) fn validate_reopened_subagent_record(
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
pub(crate) fn inject_recoverable_revision_publication_failures(id: &str, count: usize) {
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
pub(crate) fn fail_recoverable_revision_publication(
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
pub(crate) fn fail_cancelled_record_write(record_id: &str, status: &str) -> Result<(), String> {
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
pub(crate) fn fail_cancelled_record_write(_record_id: &str, _status: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
pub(crate) fn pause_subagent_record_write(record_id: &str) -> Result<(), String> {
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
pub(crate) fn pause_subagent_record_write(_record_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
pub(crate) fn pause_subagent_record_finalization(record_id: &str) -> Result<(), String> {
    let Some(expected_id) = std::env::var_os("NIB_TEST_SUBAGENT_FINALIZE_ID") else {
        return Ok(());
    };
    if expected_id != std::ffi::OsStr::new(record_id) {
        return Ok(());
    }
    let ready = PathBuf::from(
        std::env::var_os("NIB_TEST_SUBAGENT_FINALIZE_READY")
            .ok_or_else(|| "missing subagent finalization readiness path".to_string())?,
    );
    let resume = PathBuf::from(
        std::env::var_os("NIB_TEST_SUBAGENT_FINALIZE_RESUME")
            .ok_or_else(|| "missing subagent finalization resume path".to_string())?,
    );
    std::fs::write(&ready, b"ready")
        .map_err(|error| format!("failed to publish subagent finalization readiness: {error}"))?;
    let started = Instant::now();
    while !resume.exists() {
        if started.elapsed() >= Duration::from_secs(10) {
            return Err("timed out waiting to resume subagent record finalization".to_string());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn pause_subagent_record_finalization(_record_id: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
pub(crate) fn prepare_child_runtime_config(
    config: &crate::config::NibConfig,
    worktree_path: &Path,
) -> Result<(), String> {
    let mut config = selected_child_runtime_config(config, worktree_path)?;
    crate::config::save_nib_config_full_new_unpublished_root(worktree_path, &mut config)
        .map_err(|error| error.to_string())
}

pub(crate) fn prepare_child_runtime_config_with_authority(
    config: &crate::config::NibConfig,
    worktree_path: &Path,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    let deadline = authority.operation_deadline();
    authority.verify_until(deadline)?;
    let mut config = selected_child_runtime_config(config, worktree_path)?;

    crate::config::save_nib_config_full_new_unpublished_root_with_guard(
        worktree_path,
        &mut config,
        deadline,
        || authority.verify_until(deadline),
    )
    .map_err(|error| error.to_string())?;
    authority.verify_until(deadline)
}

pub(crate) fn selected_child_runtime_config(
    config: &crate::config::NibConfig,
    worktree_path: &Path,
) -> Result<crate::config::NibConfig, String> {
    let mut config = config.clone();

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
    // The worktree is a new configuration root. Do not treat the parent's
    // snapshot revision as authoritative for a path that has no config yet.
    config.revision = 0;
    Ok(config)
}

#[cfg(test)]
pub(crate) fn persist_subagent_outcome(
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

pub(crate) fn persist_subagent_outcome_with_cleanup(
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

pub(crate) fn persist_subagent_outcome_internal(
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
        let previous_result = record.result.clone();
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
                    "failure": summary.failure,
                    "bound_reached": summary.bound_reached,
                    "trace": summary.trace,
                });
                if let (Some(result), Some(cleanup)) =
                    (result.as_object_mut(), cleanup_evidence.clone())
                {
                    result.insert("cleanup_verified".to_string(), Value::Bool(true));
                    result.insert("cleanup_proof".to_string(), cleanup);
                }
                preserve_spawn_internal_authority(previous_result.as_ref(), &mut result);
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
                let mut result = cleanup_evidence.clone().map_or_else(
                    || json!({}),
                    |cleanup| {
                        json!({
                            "outcome": "worker_failed",
                            "cleanup_verified": true,
                            "cleanup_proof": cleanup,
                        })
                    },
                );
                preserve_spawn_internal_authority(previous_result.as_ref(), &mut result);
                record.result = Some(result.clone());
                TaskUpdate::Fail(error, Some(result))
            }
        };
        record.updated_at = Utc::now();
        Ok(Some(update))
    })?;

    match task_update {
        Some(TaskUpdate::Complete(result)) => {
            crate::daemons::task::TASK_MANAGER.complete(id, project_public_subagent_result(result));
        }
        Some(TaskUpdate::Fail(error, result)) => {
            crate::daemons::task::TASK_MANAGER.fail(
                id,
                error,
                result.and_then(project_public_subagent_result),
            );
        }
        None => {}
    }
    Ok(())
}

pub(crate) fn persist_supervised_interruption(
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
        let previous_result = record.result.clone();
        let mut result = json!({
            "outcome": if cancelled { "cancelled" } else { "owner_process_lost" },
            "cleanup_verified": true,
            "cleanup_scope": "foreground_descendant_process_tree",
            "cleanup_proof": cleanup,
        });
        preserve_spawn_internal_authority(previous_result.as_ref(), &mut result);
        record.result = Some(result);
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

#[cfg(test)]
pub(crate) fn persist_interrupted_subagent(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    reason: &str,
) -> bool {
    persist_interrupted_subagent_until(
        project_root,
        id,
        execution_generation,
        lease_id,
        reason,
        None,
    )
}

pub(crate) fn persist_interrupted_subagent_until(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    reason: &str,
    deadline: Option<Instant>,
) -> bool {
    let updated = update_subagent_record_until(project_root, id, deadline, |record| {
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

pub(crate) fn persist_unstarted_after_preparation_unlock(
    project_root: &Path,
    id: &str,
    execution_generation: Option<u64>,
    lease_id: Option<&str>,
    reason: &str,
    deadline: Instant,
) -> Result<(), String> {
    let (Some(execution_generation), Some(lease_id)) = (execution_generation, lease_id) else {
        return Err("pending subagent record lost its execution ownership".to_string());
    };
    ensure_subagent_reconciliation_deadline(Some(deadline))?;
    let nib_path = project_root.join(".nib");
    let nib = crate::daemons::state::StableDirectory::open(&nib_path)?;
    let scope_directory = nib_path.join("process-scopes");
    let scope = match nib.entry_kind(&scope_directory)? {
        Some(crate::daemons::state::StableEntryKind::Directory) => {
            crate::sandbox::process::ProcessScopeStore::open_with_lock_deadline(
                project_root,
                deadline,
            )?
            .try_load(id)?
        }
        Some(crate::daemons::state::StableEntryKind::File) => {
            return Err("managed-process scope namespace is not a directory".to_string());
        }
        None => None,
    };
    if let Some(scope) = scope {
        if scope.execution_generation != execution_generation {
            return Err(
                "unstarted subagent process scope belongs to another execution generation"
                    .to_string(),
            );
        }
        // A supervisor monitor owns terminalization once its process-scope
        // record is durable. Publishing a plain failure here would erase the
        // distinction between verified launch abort and descendant cleanup.
        return Ok(());
    }
    if persist_interrupted_subagent_until(
        project_root,
        id,
        execution_generation,
        lease_id,
        reason,
        Some(deadline),
    ) {
        Ok(())
    } else {
        Err("failed to persist exact unstarted subagent outcome".to_string())
    }
}

pub(crate) fn persist_owner_lease_cleanup_error(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    cleanup_error: &str,
) {
    persist_owner_lease_cleanup_error_until(
        project_root,
        id,
        execution_generation,
        lease_id,
        cleanup_error,
        None,
    );
}

pub(crate) fn persist_owner_lease_cleanup_error_until(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    cleanup_error: &str,
    deadline: Option<Instant>,
) {
    let _ = update_subagent_record_until(project_root, id, deadline, |record| {
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

pub(crate) fn persist_owner_lease_cleanup_success_until(
    project_root: &Path,
    id: &str,
    execution_generation: u64,
    lease_id: &str,
    deadline: Option<Instant>,
) -> Result<SubagentRecord, String> {
    update_subagent_record_until(project_root, id, deadline, |record| {
        if !record_matches_execution(record, execution_generation, lease_id)? {
            return Err("terminal owner lease cleanup lost execution ownership".to_string());
        }
        let result = record
            .result
            .as_mut()
            .and_then(Value::as_object_mut)
            .ok_or("terminal owner lease cleanup result is not an object")?;
        result.insert("cleanup_unverified".to_string(), Value::Bool(false));
        result.insert(
            "owner_lease_cleanup".to_string(),
            json!({
                "status": "completed",
                "cleanup_unverified": false,
                "recorded_at": Utc::now(),
            }),
        );
        record.updated_at = Utc::now();
        Ok(record.clone())
    })
}

pub(crate) fn record_matches_execution(
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
pub(crate) struct PendingMergeIntent {
    pub(crate) branch_commit: String,
    pub(crate) parent_head: String,
    pub(crate) verification_command: String,
    pub(crate) active_merge_base: Option<String>,
}

pub(crate) fn validate_verification_evidence(
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

pub(crate) fn begin_pending_merge(
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

pub(crate) fn pending_merge_intent(record: &SubagentRecord) -> Result<PendingMergeIntent, String> {
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

pub(crate) fn set_active_merge_base(record: &mut SubagentRecord, base: Option<&str>) {
    if let Some(result) = record.result.as_mut().and_then(Value::as_object_mut) {
        result.insert(
            "active_merge_base".to_string(),
            base.map_or(Value::Null, |value| Value::String(value.to_string())),
        );
    }
    record.updated_at = Utc::now();
}

pub(crate) fn valid_git_object_id(value: &str) -> bool {
    (40..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn require_record_branch_oid(
    record: &SubagentRecord,
    expected: &str,
) -> Result<(), String> {
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

pub(crate) fn pending_verification_root(
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

pub(crate) async fn stage_subagent_changes(worktree: &Path) -> Result<(), String> {
    unstage_nib_runtime_state(worktree).await?;
    git_checked(worktree, ["add", "--all"]).await?;
    unstage_nib_runtime_state(worktree).await
}

pub(crate) async fn stage_and_commit_subagent_snapshot(
    worktree: &Path,
    id: &str,
) -> Result<(), String> {
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

pub(crate) async fn create_immutable_subagent_snapshot(
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

pub(crate) async fn ensure_child_snapshot_unchanged(
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

pub(crate) async fn unstage_nib_runtime_state(worktree: &Path) -> Result<(), String> {
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

pub(crate) async fn ensure_parent_clean(project_root: &Path) -> Result<String, String> {
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

pub(crate) async fn recover_interrupted_merge(
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
pub(crate) enum OwnedMergeRecovery {
    NoMerge,
    Aborted,
    Integrated,
}

pub(crate) async fn recover_owned_merge_state(
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
