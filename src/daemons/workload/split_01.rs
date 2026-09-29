//! T043 split.

use super::*;

pub(crate) struct TerminalWorkerJob {
    pub(crate) command: String,
    pub(crate) cwd: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) profile_id: String,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) execution: ExecutionConfig,
    pub(crate) timeout_secs: u64,
    pub(crate) max_output_bytes: usize,
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn run_terminal_worker(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    job: TerminalWorkerJob,
) -> Result<(), String> {
    let (profile, config_sensitive_values) = load_worker_profile(
        &job.project_root,
        &job.profile_id,
        &job.sessions_dir,
        store.daemon_dir(),
    )?;
    let session_target = BackgroundTaskSession {
        session_store: SessionStore::at_dir(job.sessions_dir.clone()),
        session_id: job.session_id.clone(),
        audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
    };
    let mut environment_keys: Vec<_> = profile.custom_env().keys().cloned().collect();
    environment_keys.sort();
    let started = Instant::now();
    let run = timeout(
        Duration::from_secs(job.timeout_secs.max(1)),
        crate::sandbox::run_sandboxed_streaming_with_environment(
            &job.command,
            &job.cwd,
            &job.execution.provider,
            &job.execution.default_profile,
            &job.execution.boundaries,
            profile.custom_env(),
            job.max_output_bytes,
            None,
        ),
    );
    tokio::pin!(run);
    loop {
        tokio::select! {
            outcome = &mut run => {
                let record = match store.poll_worker_owned(task_id, owner, true) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                };
                if record.cancel_requested {
                    match publish_terminal_cancellation_owned(
                        store,
                        owner,
                        task_id,
                        &session_target,
                        "cancelled by user",
                    ) {
                        Ok(_) => {}
                        Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                        Err(error) => return Err(error),
                    }
                    return Ok(());
                }
                let (success, result, error) = terminal_outcome(
                    task_id,
                    &job,
                    started.elapsed().as_secs_f64(),
                    &environment_keys,
                    outcome,
                );
                let result = crate::tools::executor::redact_value_with_encoded_sensitive_values(
                    result,
                    config_sensitive_values.iter().cloned(),
                );
                let result = crate::tools::executor::redact_value_with_environment(
                    result,
                    profile.custom_env(),
                );
                let error = error.map(|value| {
                    let value = crate::tools::executor::redact_text_with_encoded_sensitive_values(
                        &value,
                        config_sensitive_values.iter().cloned(),
                    );
                    crate::tools::executor::redact_text_with_environment(
                        &value,
                        profile.custom_env(),
                    )
                });
                match publish_terminal_outcome_owned(
                    store,
                    owner,
                    task_id,
                    &session_target,
                    success,
                    result,
                    error,
                ) {
                    Ok(_) => {}
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
                return Ok(());
            }
            _ = sleep(WORKER_POLL_INTERVAL) => {
                let record = match store.poll_worker_owned(task_id, owner, false) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                };
                if record.status == "reconciling" || record.is_terminal() {
                    return Ok(());
                }
                if record.cancel_requested {
                    match publish_terminal_cancellation_owned(
                        store,
                        owner,
                        task_id,
                        &session_target,
                        "cancelled by user",
                    ) {
                        Ok(_) => {}
                        Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                        Err(error) => return Err(error),
                    }
                    return Ok(());
                }
            }
        }
    }
}

pub(crate) fn publish_terminal_outcome_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_target: &BackgroundTaskSession,
    success: bool,
    result: Value,
    error: Option<String>,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        let delivery_error = deliver_background_task_observation(
            session_target,
            task_id,
            &task.record.execution_id,
            success,
            Some(&result),
            error.as_deref(),
        )
        .err();
        let final_error = combine_delivery_error(error, delivery_error, success);
        let status = if success && final_error.is_none() {
            "completed"
        } else {
            "failed"
        };
        finish_task_file(task, status, Some(result), final_error);
        Ok(())
    })
}

pub(crate) fn publish_terminal_cancellation_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_target: &BackgroundTaskSession,
    reason: &str,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        let delivery_error = deliver_background_task_observation(
            session_target,
            task_id,
            &task.record.execution_id,
            false,
            None,
            Some(reason),
        )
        .err();
        let error = delivery_error.map_or_else(
            || reason.to_string(),
            |delivery| format!("{reason}; cancellation delivery failed: {delivery}"),
        );
        finish_task_file(task, "cancelled", None, Some(error));
        Ok(())
    })
}

#[allow(clippy::type_complexity)]
pub(crate) fn terminal_outcome(
    task_id: &str,
    job: &TerminalWorkerJob,
    duration: f64,
    environment_keys: &[String],
    outcome: Result<
        Result<(crate::sandbox::BoundedOutput, Option<Vec<String>>), String>,
        tokio::time::error::Elapsed,
    >,
) -> (bool, Value, Option<String>) {
    match outcome {
        Ok(Ok((output, bwrap_args))) => {
            let success = output.status.success();
            let error = (!success)
                .then(|| format!("command exited with {}", output.status.code().unwrap_or(-1)));
            let result = json!({
                "task_id": task_id,
                "command": job.command,
                "cwd": job.cwd.to_string_lossy(),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
                "stdout_bytes": output.stdout_bytes,
                "stderr_bytes": output.stderr_bytes,
                "stdout_bytes_retained": output.stdout.len(),
                "stderr_bytes_retained": output.stderr.len(),
                "stdout_truncated": output.stdout_truncated(),
                "stderr_truncated": output.stderr_truncated(),
                "max_output_bytes": job.max_output_bytes,
                "exit_code": output.status.code(),
                "duration": duration,
                "provider": if bwrap_args.is_some() { "bwrap" } else { "internal" },
                "sandbox_profile": job.execution.default_profile,
                "boundaries": job.execution.boundaries,
                "bwrap_args": bwrap_args,
                "environment_keys": environment_keys,
            });
            (success, result, error)
        }
        Ok(Err(error)) => (
            false,
            empty_terminal_result(task_id, job, duration, environment_keys),
            Some(format!("background command failed: {error}")),
        ),
        Err(_) => (
            false,
            empty_terminal_result(task_id, job, duration, environment_keys),
            Some(format!("command timed out after {}s", job.timeout_secs)),
        ),
    }
}

pub(crate) fn empty_terminal_result(
    task_id: &str,
    job: &TerminalWorkerJob,
    duration: f64,
    environment_keys: &[String],
) -> Value {
    json!({
        "task_id": task_id,
        "command": job.command,
        "cwd": job.cwd.to_string_lossy(),
        "stdout": "",
        "stderr": "",
        "stdout_bytes": 0,
        "stderr_bytes": 0,
        "stdout_bytes_retained": 0,
        "stderr_bytes_retained": 0,
        "stdout_truncated": false,
        "stderr_truncated": false,
        "max_output_bytes": job.max_output_bytes,
        "exit_code": null,
        "duration": duration,
        "provider": job.execution.provider,
        "sandbox_profile": job.execution.default_profile,
        "boundaries": job.execution.boundaries,
        "bwrap_args": null,
        "environment_keys": environment_keys,
    })
}

pub(crate) fn combine_delivery_error(
    error: Option<String>,
    delivery_error: Option<String>,
    success: bool,
) -> Option<String> {
    match (error, delivery_error, success) {
        (Some(error), Some(delivery), _) => {
            Some(format!("{error}; completion delivery failed: {delivery}"))
        }
        (Some(error), None, _) => Some(error),
        (None, Some(delivery), true) => Some(format!(
            "background command completed but delivery failed: {delivery}"
        )),
        (None, Some(delivery), false) => Some(delivery),
        (None, None, true) => None,
        (None, None, false) => Some("background command failed".to_string()),
    }
}

pub(crate) async fn monitor_worker_future<F, T>(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    cancellation: &crate::agent::CancellationSignal,
    future: F,
) -> Result<MonitoredRun<T>, String>
where
    F: std::future::Future<Output = T>,
{
    let mut cancellation_requested = match store.poll_worker_owned(task_id, owner, true) {
        Ok(record) => record.cancel_requested,
        Err(error) if is_worker_lease_lost(&error) => {
            cancellation.cancel();
            return Ok(MonitoredRun::LeaseLost);
        }
        Err(error) => return Err(error),
    };
    if cancellation_requested {
        cancellation.cancel();
    }

    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => {
                let record = match store.poll_worker_owned(task_id, owner, true) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => {
                        cancellation.cancel();
                        return Ok(MonitoredRun::LeaseLost);
                    }
                    Err(error) => return Err(error),
                };
                if cancellation_requested || record.cancel_requested {
                    cancellation.cancel();
                    return Ok(MonitoredRun::Cancelled);
                }
                return Ok(MonitoredRun::Completed(output));
            }
            _ = sleep(WORKER_POLL_INTERVAL) => {
                match store.poll_worker_owned(task_id, owner, false) {
                    Ok(record) if record.cancel_requested => {
                        cancellation_requested = true;
                        cancellation.cancel();
                    }
                    Ok(_) => {}
                    Err(error) if is_worker_lease_lost(&error) => {
                        cancellation.cancel();
                        return Ok(MonitoredRun::LeaseLost);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

pub(crate) async fn wait_for_schedule_run_lease(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    session_id: &str,
) -> Result<MonitoredRun<SessionRunLease>, String> {
    loop {
        let record = match store.poll_worker_owned(task_id, owner, false) {
            Ok(record) => record,
            Err(error) if is_worker_lease_lost(&error) => return Ok(MonitoredRun::LeaseLost),
            Err(error) => return Err(error),
        };
        if record.status == "reconciling" || record.is_terminal() {
            return Ok(MonitoredRun::LeaseLost);
        }
        if record.cancel_requested {
            return Ok(MonitoredRun::Cancelled);
        }

        match session_store.try_acquire_run_lease(session_id) {
            Ok(run_lease) => {
                let record = match store.poll_worker_owned(task_id, owner, true) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => {
                        return Ok(MonitoredRun::LeaseLost)
                    }
                    Err(error) => return Err(error),
                };
                if record.status == "reconciling" || record.is_terminal() {
                    return Ok(MonitoredRun::LeaseLost);
                }
                if record.cancel_requested {
                    return Ok(MonitoredRun::Cancelled);
                }
                run_lease
                    .verify_for(session_id, session_store.sessions_dir())
                    .map_err(|error| error.to_string())?;
                return Ok(MonitoredRun::Completed(run_lease));
            }
            Err(SessionError::RunLeaseHeld(_)) => sleep(WORKER_POLL_INTERVAL).await,
            Err(error) => {
                return Err(format!(
                    "failed to acquire scheduled agent run lease for {session_id}: {error}"
                ))
            }
        }
    }
}

pub(crate) struct ScheduleWorkerJob {
    pub(crate) prompt: String,
    pub(crate) project_root: PathBuf,
    pub(crate) profile_id: String,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) session_id: String,
    pub(crate) interval_secs: u64,
    pub(crate) repeat_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleWakePublication {
    Started,
    Finished,
}

pub(crate) fn schedule_delivery_key(
    task_id: &str,
    execution_id: &str,
    occurrence: u32,
    phase: &str,
) -> String {
    format!("schedule:{task_id}:{execution_id}:{occurrence}:{phase}")
}

pub(crate) fn execution_id_matches(details: &Value, execution_id: &str) -> bool {
    match details.get("execution_id").and_then(Value::as_str) {
        Some(existing) => existing == execution_id,
        None => execution_id.starts_with("legacy-"),
    }
}

pub(crate) fn record_schedule_event_once(
    store: &SessionStore,
    session_id: &str,
    kind: &str,
    mut details: Value,
    delivery_key: &str,
) -> Result<(), String> {
    let details_object = details
        .as_object_mut()
        .ok_or_else(|| "scheduled event details must be an object".to_string())?;
    details_object.insert(
        "delivery_key".to_string(),
        Value::String(delivery_key.to_string()),
    );
    store
        .update_session(session_id, |session| {
            if session.events.iter().any(|event| {
                event.details.get("delivery_key").and_then(Value::as_str) == Some(delivery_key)
            }) {
                return Ok(());
            }
            session.events.push(SessionEvent {
                index: session.events.len(),
                kind: kind.to_string(),
                details,
                timestamp: Some(Utc::now()),
            });
            Ok(())
        })
        .map_err(|error| format!("failed to record scheduled event: {error}"))
}

pub(crate) fn append_schedule_audit_once(
    audit: &DaemonAuditLog,
    session_id: &str,
    action: &str,
    outcome: &str,
    detail: String,
    delivery_key: &str,
    equivalent_outcomes: &[&str],
) -> Result<(), String> {
    audit.append_once_for_detail_key(
        &DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "timer".to_string(),
            action: action.to_string(),
            target: Some(session_id.to_string()),
            outcome: outcome.to_string(),
            authorized: true,
            detail: Some(format!("delivery_key={delivery_key}; {detail}")),
        },
        &format!("delivery_key={delivery_key}"),
        equivalent_outcomes,
    )
}

pub(crate) fn schedule_publication_error(context: &str, errors: Vec<String>) -> Option<String> {
    (!errors.is_empty()).then(|| format!("{context}: {}", errors.join("; ")))
}

pub(crate) struct ScheduleCancellation<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) repeat_count: u32,
    pub(crate) occurrence: u32,
    pub(crate) reason: &'a str,
}

pub(crate) struct ScheduleWake<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) task_id: &'a str,
    pub(crate) execution_id: &'a str,
    pub(crate) occurrence: u32,
    pub(crate) repeat_count: u32,
    pub(crate) prompt: &'a str,
    pub(crate) delivery_key: &'a str,
}

pub(crate) fn publish_schedule_cancellation_effects(
    task: &mut DurableTaskFile,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    cancellation: ScheduleCancellation<'_>,
) {
    let delivery_key = schedule_delivery_key(
        task_id,
        &task.record.execution_id,
        cancellation.occurrence,
        "cancel",
    );
    let mut errors = Vec::new();
    if let Err(error) = record_schedule_event_once(
        session_store,
        cancellation.session_id,
        "timer_cancelled",
        json!({
            "timer_id": task_id,
            "execution_id": task.record.execution_id,
            "occurrence": cancellation.occurrence,
            "completed_occurrences": task.record.completed_occurrences,
            "repeat_count": cancellation.repeat_count,
        }),
        &delivery_key,
    ) {
        errors.push(error);
    }
    if let Err(error) = append_schedule_audit_once(
        audit,
        cancellation.session_id,
        "cancel",
        "cancelled",
        format!("timer_id={task_id}; occurrence={}", cancellation.occurrence),
        &delivery_key,
        &["cancelled"],
    ) {
        errors.push(format!(
            "failed to record timer cancellation audit: {error}"
        ));
    }
    let error = schedule_publication_error(cancellation.reason, errors)
        .unwrap_or_else(|| cancellation.reason.to_string());
    let result = task.record.result.clone();
    finish_task_file(task, "cancelled", result, Some(error));
}

pub(crate) fn publish_schedule_cancellation_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    cancellation: ScheduleCancellation<'_>,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        publish_schedule_cancellation_effects(task, task_id, session_store, audit, cancellation);
        Ok(())
    })
}

pub(crate) fn publish_schedule_wake_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    job: &ScheduleWorkerJob,
    occurrence: u32,
) -> Result<ScheduleWakePublication, String> {
    store
        .update_owned_with(task_id, owner, |task| {
            if task.record.cancel_requested {
                publish_schedule_cancellation_effects(
                    task,
                    task_id,
                    session_store,
                    audit,
                    ScheduleCancellation {
                        session_id: &job.session_id,
                        repeat_count: job.repeat_count,
                        occurrence,
                        reason: "cancelled by user",
                    },
                );
                return Ok(ScheduleWakePublication::Finished);
            }

            let delivery_key =
                schedule_delivery_key(task_id, &task.record.execution_id, occurrence, "start");
            let mut errors = Vec::new();
            if let Err(error) = archive_pending_plan_and_record_wake(
                session_store,
                ScheduleWake {
                    session_id: &job.session_id,
                    task_id,
                    execution_id: &task.record.execution_id,
                    occurrence,
                    repeat_count: job.repeat_count,
                    prompt: &job.prompt,
                    delivery_key: &delivery_key,
                },
            ) {
                errors.push(error);
            }
            if errors.is_empty() {
                if let Err(error) = append_schedule_audit_once(
                    audit,
                    &job.session_id,
                    "wake_agent_loop",
                    "started",
                    format!(
                        "timer_id={task_id}; occurrence={occurrence}/{}; mode=plan",
                        job.repeat_count
                    ),
                    &delivery_key,
                    &["started"],
                ) {
                    errors.push(format!("failed to record scheduled wake audit: {error}"));
                }
            }
            if let Some(error) =
                schedule_publication_error("failed to publish scheduled wake", errors)
            {
                let result = task.record.result.clone();
                finish_task_file(task, "failed", result, Some(error));
                return Ok(ScheduleWakePublication::Finished);
            }
            task.active_occurrence = Some(occurrence);
            task.record.updated_at = Utc::now();
            Ok(ScheduleWakePublication::Started)
        })
        .map(|(_, publication)| publication)
}

pub(crate) struct ScheduleCompletion<'a> {
    pub(crate) occurrence: u32,
    pub(crate) repeat_count: u32,
    pub(crate) outcome: &'a str,
    pub(crate) steps_taken: u32,
    pub(crate) next_run_at: Option<DateTime<Utc>>,
    pub(crate) run: Value,
}

pub(crate) struct ScheduleFailure<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) occurrence: u32,
    pub(crate) repeat_count: u32,
    pub(crate) outcome: &'a str,
    pub(crate) error: String,
    pub(crate) failure: Option<&'a crate::llm::LlmError>,
}

pub(crate) fn publish_schedule_completion_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    session_id: &str,
    completion: ScheduleCompletion<'_>,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        let delivery_key = schedule_delivery_key(
            task_id,
            &task.record.execution_id,
            completion.occurrence,
            "terminal",
        );
        let mut errors = Vec::new();
        if let Err(error) = record_schedule_event_once(
            session_store,
            session_id,
            "scheduled_agent_run_completed",
            json!({
                "timer_id": task_id,
                "execution_id": task.record.execution_id,
                "occurrence": completion.occurrence,
                "repeat_count": completion.repeat_count,
                "outcome": completion.outcome,
                "steps_taken": completion.steps_taken,
                "mode": "plan",
            }),
            &delivery_key,
        ) {
            errors.push(error);
        }
        if let Err(error) = append_schedule_audit_once(
            audit,
            session_id,
            "wake_agent_loop",
            "completed",
            format!(
                "timer_id={task_id}; occurrence={}/{}; agent_outcome={}",
                completion.occurrence, completion.repeat_count, completion.outcome
            ),
            &delivery_key,
            &["completed", "failed"],
        ) {
            errors.push(format!(
                "failed to record scheduled completion audit: {error}"
            ));
        }
        if let Some(error) =
            schedule_publication_error("scheduled completion publication failed", errors)
        {
            let result = task.record.result.clone();
            finish_task_file(task, "failed", result, Some(error));
        } else {
            update_schedule_progress_file(
                task,
                completion.occurrence,
                completion.next_run_at,
                completion.run,
            );
        }
        Ok(())
    })
}

pub(crate) fn publish_schedule_failure_owned(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    session_store: &SessionStore,
    audit: &DaemonAuditLog,
    failure: ScheduleFailure<'_>,
) -> Result<DurableTaskRecord, String> {
    store.update_owned(task_id, owner, |task| {
        let delivery_key = schedule_delivery_key(
            task_id,
            &task.record.execution_id,
            failure.occurrence,
            "terminal",
        );
        let mut errors = Vec::new();
        if let Err(delivery_error) = record_schedule_event_once(
            session_store,
            failure.session_id,
            "scheduled_agent_run_failed",
            json!({
                "timer_id": task_id,
                "execution_id": task.record.execution_id,
                "occurrence": failure.occurrence,
                "repeat_count": failure.repeat_count,
                "outcome": failure.outcome,
                "error": failure.error,
                "failure": failure.failure,
                "mode": "plan",
            }),
            &delivery_key,
        ) {
            errors.push(delivery_error);
        }
        if let Err(audit_error) = append_schedule_audit_once(
            audit,
            failure.session_id,
            "wake_agent_loop",
            "failed",
            format!(
                "timer_id={task_id}; occurrence={}/{}; error={}",
                failure.occurrence, failure.repeat_count, failure.error
            ),
            &delivery_key,
            &["completed", "failed"],
        ) {
            errors.push(format!(
                "failed to record scheduled failure audit: {audit_error}"
            ));
        }
        let final_error = schedule_publication_error(&failure.error, errors)
            .unwrap_or_else(|| failure.error.clone());
        let mut runs = task
            .record
            .result
            .as_ref()
            .and_then(|value| value.get("runs"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        runs.push(json!({
            "occurrence": failure.occurrence,
            "session_id": failure.session_id,
            "outcome": failure.outcome,
            "failure": failure.failure,
            "mode": "plan",
        }));
        let result = Some(json!({
            "delivered_count": task.record.completed_occurrences,
            "repeat_count": task.record.total_occurrences,
            "runs": runs,
            "execution_mode": "plan",
        }));
        finish_task_file(task, "failed", result, Some(final_error));
        Ok(())
    })
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn run_schedule_worker(
    store: &DurableTaskStore,
    owner: &WorkerOwner,
    task_id: &str,
    job: ScheduleWorkerJob,
) -> Result<(), String> {
    let (_profile, config_sensitive_values) = load_worker_profile(
        &job.project_root,
        &job.profile_id,
        &job.sessions_dir,
        store.daemon_dir(),
    )?;
    let session_store = SessionStore::at_dir(job.sessions_dir.clone());
    let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
    for occurrence in 1..=job.repeat_count {
        loop {
            let record = match store.poll_worker_owned(task_id, owner, false) {
                Ok(record) => record,
                Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                Err(error) => return Err(error),
            };
            if record.status == "reconciling" || record.is_terminal() {
                return Ok(());
            }
            if record.cancel_requested {
                match publish_schedule_cancellation_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    ScheduleCancellation {
                        session_id: &job.session_id,
                        repeat_count: job.repeat_count,
                        occurrence,
                        reason: "cancelled by user",
                    },
                ) {
                    Ok(_) => {}
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
                return Ok(());
            }
            let Some(next_run) = record.next_run_at else {
                return Err(format!(
                    "scheduled task {task_id} has no next run timestamp"
                ));
            };
            let remaining = next_run.signed_duration_since(Utc::now());
            if remaining <= ChronoDuration::zero() {
                break;
            }
            let delay_ms = remaining.num_milliseconds().clamp(1, 500) as u64;
            sleep(Duration::from_millis(delay_ms)).await;
        }

        let run_lease = match wait_for_schedule_run_lease(
            store,
            owner,
            task_id,
            &session_store,
            &job.session_id,
        )
        .await?
        {
            MonitoredRun::Completed(run_lease) => run_lease,
            MonitoredRun::Cancelled => {
                match publish_schedule_cancellation_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    ScheduleCancellation {
                        session_id: &job.session_id,
                        repeat_count: job.repeat_count,
                        occurrence,
                        reason: "cancelled by user",
                    },
                ) {
                    Ok(_) => return Ok(()),
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            MonitoredRun::LeaseLost => return Ok(()),
        };

        match publish_schedule_wake_owned(
            store,
            owner,
            task_id,
            &session_store,
            &audit,
            &job,
            occurrence,
        ) {
            Ok(ScheduleWakePublication::Started) => {}
            Ok(ScheduleWakePublication::Finished) => return Ok(()),
            Err(error) if is_worker_lease_lost(&error) => return Ok(()),
            Err(error) => return Err(error),
        }

        let cancellation = crate::agent::CancellationSignal::new();
        let outcome = monitor_worker_future(
            store,
            owner,
            task_id,
            &cancellation,
            crate::agent::r#loop::run_agent_loop_for_profile_with_lease(
                job.project_root.clone(),
                &job.profile_id,
                &job.sessions_dir,
                &job.session_id,
                &job.prompt,
                AgentLoopConfig {
                    mode: "plan".to_string(),
                    auto_approve: false,
                    cancellation: Some(cancellation.clone()),
                    ..AgentLoopConfig::default()
                },
                run_lease,
            ),
        )
        .await?;
        let outcome = match outcome {
            MonitoredRun::Completed(outcome) => outcome,
            MonitoredRun::Cancelled => {
                match publish_schedule_cancellation_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    ScheduleCancellation {
                        session_id: &job.session_id,
                        repeat_count: job.repeat_count,
                        occurrence,
                        reason: "cancelled by user",
                    },
                ) {
                    Ok(_) => return Ok(()),
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
            MonitoredRun::LeaseLost => return Ok(()),
        };
        let outcome = classify_agent_run_outcome(outcome);
        match outcome {
            Ok(summary) => {
                let next_run = if occurrence < job.repeat_count {
                    Some(
                        Utc::now()
                            .checked_add_signed(ChronoDuration::seconds(
                                i64::try_from(job.interval_secs)
                                    .map_err(|_| "schedule interval is too large".to_string())?,
                            ))
                            .ok_or("schedule next-run timestamp overflow")?,
                    )
                } else {
                    None
                };
                let run = json!({
                    "occurrence": occurrence,
                    "session_id": job.session_id,
                    "outcome": summary.outcome,
                    "failure": summary.failure,
                    "steps_taken": summary.steps_taken,
                    "mode": "plan",
                });
                let record = match publish_schedule_completion_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    &job.session_id,
                    ScheduleCompletion {
                        occurrence,
                        repeat_count: job.repeat_count,
                        outcome: &summary.outcome,
                        steps_taken: summary.steps_taken,
                        next_run_at: next_run,
                        run,
                    },
                ) {
                    Ok(record) => record,
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                };
                if record.is_terminal() {
                    return Ok(());
                }
            }
            Err(failed) => {
                let error = if failed.failure.is_some() {
                    // Typed provider evidence is persisted separately. Keep rendered
                    // user prose out of durable machine state and daemon audit text.
                    "scheduled agent run failed".to_string()
                } else {
                    crate::tools::executor::redact_text_with_encoded_sensitive_values(
                        &failed.report,
                        config_sensitive_values.iter().cloned(),
                    )
                };
                match publish_schedule_failure_owned(
                    store,
                    owner,
                    task_id,
                    &session_store,
                    &audit,
                    ScheduleFailure {
                        session_id: &job.session_id,
                        occurrence,
                        repeat_count: job.repeat_count,
                        outcome: &failed.outcome,
                        error,
                        failure: failed.failure.as_ref(),
                    },
                ) {
                    Ok(_) => {}
                    Err(error) if is_worker_lease_lost(&error) => return Ok(()),
                    Err(error) => return Err(error),
                }
                return Ok(());
            }
        }
    }
    Ok(())
}

pub(crate) fn archive_pending_plan_and_record_wake(
    store: &SessionStore,
    wake: ScheduleWake<'_>,
) -> Result<(), String> {
    let ScheduleWake {
        session_id,
        task_id,
        execution_id,
        occurrence,
        repeat_count,
        prompt,
        delivery_key,
    } = wake;
    store
        .update_session(session_id, |session| {
            if session.events.iter().any(|event| {
                event.kind == "timer_fired"
                    && (event.details.get("delivery_key").and_then(Value::as_str)
                        == Some(delivery_key)
                        || (event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                            && execution_id_matches(&event.details, execution_id)
                            && event.details.get("occurrence").and_then(Value::as_u64)
                                == Some(u64::from(occurrence))))
            }) {
                return Ok(());
            }
            if let Some(plan) = session.plan.take() {
                if !plan.is_complete() {
                    session.events.push(SessionEvent {
                        index: session.events.len(),
                        kind: "scheduled_plan_archived".to_string(),
                        details: json!({
                            "timer_id": task_id,
                            "execution_id": execution_id,
                            "plan": plan,
                        }),
                        timestamp: Some(Utc::now()),
                    });
                }
            }
            session.events.push(SessionEvent {
                index: session.events.len(),
                kind: "timer_fired".to_string(),
                details: json!({
                    "timer_id": task_id,
                    "execution_id": execution_id,
                    "occurrence": occurrence,
                    "repeat_count": repeat_count,
                    "prompt": prompt,
                    "execution_mode": "plan",
                    "delivery_key": delivery_key,
                }),
                timestamp: Some(Utc::now()),
            });
            Ok(())
        })
        .map_err(|error| format!("failed to record scheduled wake: {error}"))
}

pub(crate) enum ScheduleReconcileAudit {
    ReconciledFailure,
    ExistingTerminal {
        action: &'static str,
        outcome: &'static str,
        detail: String,
        delivery_key: String,
    },
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn reconcile_expired_job(
    job: &DurableJob,
    daemon_dir: &Path,
    task_id: &str,
    execution_id: &str,
    occurrence: Option<u32>,
    error: &str,
) -> Result<(), String> {
    let audit = DaemonAuditLog::at_path(daemon_dir.join("audit.jsonl"));
    match job {
        DurableJob::Terminal {
            sessions_dir,
            session_id,
            ..
        } => deliver_background_task_observation(
            &BackgroundTaskSession {
                session_store: SessionStore::at_dir(sessions_dir.clone()),
                session_id: session_id.clone(),
                audit_log: audit,
            },
            task_id,
            execution_id,
            false,
            None,
            Some(error),
        ),
        DurableJob::Schedule {
            sessions_dir,
            session_id,
            repeat_count,
            ..
        } => {
            let session_store = SessionStore::at_dir(sessions_dir.clone());
            let occurrence = occurrence.unwrap_or(1);
            let delivery_key = schedule_delivery_key(task_id, execution_id, occurrence, "terminal");
            let audit_action = session_store
                .update_session(session_id, |session| {
                    if let Some(existing) = session.events.iter().find(|event| {
                        matches!(
                            event.kind.as_str(),
                            "scheduled_agent_run_completed"
                                | "scheduled_agent_run_failed"
                                | "timer_cancelled"
                        ) && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                            && execution_id_matches(&event.details, execution_id)
                            && (event.details.get("delivery_key").and_then(Value::as_str)
                                == Some(delivery_key.as_str())
                                || event.details.get("occurrence").and_then(Value::as_u64)
                                    == Some(u64::from(occurrence))
                                || event.details.get("reconciled").and_then(Value::as_bool)
                                    == Some(true))
                    }) {
                        if existing.details.get("reconciled").and_then(Value::as_bool) == Some(true)
                        {
                            return Ok(ScheduleReconcileAudit::ReconciledFailure);
                        }
                        let phase = if existing.kind == "timer_cancelled" {
                            "cancel"
                        } else {
                            "terminal"
                        };
                        let existing_delivery_key = existing
                            .details
                            .get("delivery_key")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                schedule_delivery_key(task_id, execution_id, occurrence, phase)
                            });
                        let (action, outcome, detail) = match existing.kind.as_str() {
                            "scheduled_agent_run_completed" => (
                                "wake_agent_loop",
                                "completed",
                                format!(
                                    "timer_id={task_id}; occurrence={occurrence}/{}; agent_outcome={}",
                                    existing
                                        .details
                                        .get("repeat_count")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(u64::from(*repeat_count)),
                                    existing
                                        .details
                                        .get("outcome")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown")
                                ),
                            ),
                            "scheduled_agent_run_failed" => (
                                "wake_agent_loop",
                                "failed",
                                format!(
                                    "timer_id={task_id}; occurrence={occurrence}/{}; error={}",
                                    existing
                                        .details
                                        .get("repeat_count")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(u64::from(*repeat_count)),
                                    existing
                                        .details
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown")
                                ),
                            ),
                            "timer_cancelled" => (
                                "cancel",
                                "cancelled",
                                format!("timer_id={task_id}; occurrence={occurrence}"),
                            ),
                            _ => unreachable!("filtered schedule terminal event kind"),
                        };
                        return Ok(ScheduleReconcileAudit::ExistingTerminal {
                            action,
                            outcome,
                            detail,
                            delivery_key: existing_delivery_key,
                        });
                    }
                    session.events.push(SessionEvent {
                        index: session.events.len(),
                        kind: "scheduled_agent_run_failed".to_string(),
                        details: json!({
                            "timer_id": task_id,
                            "execution_id": execution_id,
                            "occurrence": occurrence,
                            "error": error,
                            "reconciled": true,
                            "mode": "plan",
                            "delivery_key": delivery_key,
                        }),
                        timestamp: Some(Utc::now()),
                    });
                    Ok(ScheduleReconcileAudit::ReconciledFailure)
                })
                .map_err(|failure| format!("failed to reconcile scheduled session: {failure}"))?;
            match audit_action {
                ScheduleReconcileAudit::ReconciledFailure => append_schedule_audit_once(
                    &audit,
                    session_id,
                    "reconcile_expired_worker",
                    "failed",
                    format!("timer_id={task_id}; occurrence={occurrence}; error={error}"),
                    &delivery_key,
                    &["failed"],
                ),
                ScheduleReconcileAudit::ExistingTerminal {
                    action,
                    outcome,
                    detail,
                    delivery_key,
                } => append_schedule_audit_once(
                    &audit,
                    session_id,
                    action,
                    outcome,
                    detail,
                    &delivery_key,
                    if outcome == "cancelled" {
                        &["cancelled"]
                    } else {
                        &["completed", "failed"]
                    },
                ),
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct ScheduledAgentFailure {
    pub(crate) outcome: String,
    pub(crate) report: String,
    pub(crate) failure: Option<crate::llm::LlmError>,
}

pub(crate) fn classify_agent_run_outcome(
    outcome: Result<crate::agent::AgentRunSummary, String>,
) -> Result<crate::agent::AgentRunSummary, Box<ScheduledAgentFailure>> {
    match outcome {
        Ok(summary)
            if summary.final_state == crate::agent::state::AgentState::Done
                && summary.outcome == "plan_ready"
                && !summary.bound_reached
                && summary.tool_call_count == 0 =>
        {
            Ok(summary)
        }
        Ok(summary) => Err(Box::new(ScheduledAgentFailure {
            outcome: summary.outcome.clone(),
            report: summary
                .user_failure_report()
                .unwrap_or_else(|| summary.outcome.clone()),
            failure: summary.failure,
        })),
        Err(error) => Err(Box::new(ScheduledAgentFailure {
            outcome: "local_error".to_string(),
            report: error,
            failure: None,
        })),
    }
}

pub(crate) fn load_worker_profile(
    project_root: &Path,
    profile_id: &str,
    sessions_dir: &Path,
    daemon_dir: &Path,
) -> Result<(Profile, Vec<String>), String> {
    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("failed to resolve worker project root: {error}"))?;
    let config =
        crate::config::load_nib_config_full(&project_root).map_err(|error| error.to_string())?;
    let sensitive_values =
        crate::llm::factory::provider_error_sensitive_values(config.sensitive_values());
    let profiles = ProfileRegistry::load(&project_root, &config.profiles)
        .map_err(|error| error.to_string())?;
    let profile = profiles
        .get(profile_id)
        .ok_or_else(|| format!("durable task profile no longer exists: {profile_id}"))?
        .clone();
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;
    let expected_sessions = profile
        .sessions_dir()
        .canonicalize()
        .map_err(|error| format!("failed to resolve profile sessions: {error}"))?;
    let actual_sessions = sessions_dir
        .canonicalize()
        .map_err(|error| format!("failed to resolve task sessions: {error}"))?;
    if actual_sessions != expected_sessions {
        return Err(format!(
            "durable task session scope changed: expected {}, got {}",
            expected_sessions.display(),
            actual_sessions.display()
        ));
    }
    let expected_daemon = profile
        .daemon_dir()
        .canonicalize()
        .map_err(|error| format!("failed to resolve profile daemon state: {error}"))?;
    let actual_daemon = daemon_dir
        .canonicalize()
        .map_err(|error| format!("failed to resolve task daemon state: {error}"))?;
    if actual_daemon != expected_daemon {
        return Err(format!(
            "durable task daemon scope changed: expected {}, got {}",
            expected_daemon.display(),
            actual_daemon.display()
        ));
    }
    Ok((profile, sensitive_values))
}

pub(crate) fn validate_terminal_request(request: &DurableTerminalRequest) -> Result<(), String> {
    if request.command.trim().is_empty() {
        return Err("background command must not be empty".to_string());
    }
    if request.command.len() > MAX_TERMINAL_COMMAND_BYTES {
        return Err(format!(
            "background command exceeds the {MAX_TERMINAL_COMMAND_BYTES}-byte limit"
        ));
    }
    if request.timeout_secs == 0 || request.timeout_secs > MAX_TERMINAL_TIMEOUT_SECONDS {
        return Err(format!(
            "background command timeout must be between 1 and {MAX_TERMINAL_TIMEOUT_SECONDS} seconds"
        ));
    }
    if request.max_output_bytes == 0 || request.max_output_bytes > MAX_TERMINAL_OUTPUT_BYTES {
        return Err(format!(
            "background command output limit must be between 1 and {MAX_TERMINAL_OUTPUT_BYTES} bytes"
        ));
    }
    validate_scoped_worker_paths(
        &request.project_root,
        &request.cwd,
        &request.sessions_dir,
        &request.session_id,
    )
}

pub(crate) fn validate_schedule_request(request: &DurableScheduleRequest) -> Result<(), String> {
    if request.prompt.trim().is_empty() {
        return Err("scheduled prompt must not be empty".to_string());
    }
    if request.prompt.len() > MAX_SCHEDULE_PROMPT_BYTES {
        return Err(format!(
            "scheduled prompt exceeds the {MAX_SCHEDULE_PROMPT_BYTES}-byte limit"
        ));
    }
    if request.repeat_count == 0 || request.repeat_count > 100 {
        return Err("timer repeat_count must be between 1 and 100".to_string());
    }
    if request.initial_delay.is_zero() || request.interval.is_zero() {
        return Err("timer delays must be greater than zero".to_string());
    }
    if request.initial_delay.as_secs() > MAX_SCHEDULE_DELAY_SECONDS
        || request.interval.as_secs() > MAX_SCHEDULE_DELAY_SECONDS
    {
        return Err(format!(
            "timer delays must not exceed {MAX_SCHEDULE_DELAY_SECONDS} seconds"
        ));
    }
    validate_scoped_worker_paths(
        &request.project_root,
        &request.project_root,
        &request.sessions_dir,
        &request.session_id,
    )
}

pub(crate) fn validate_scoped_worker_paths(
    project_root: &Path,
    cwd: &Path,
    sessions_dir: &Path,
    session_id: &str,
) -> Result<(), String> {
    validate_task_id(session_id)?;
    let project_root = project_root
        .canonicalize()
        .map_err(|error| format!("invalid durable task project root: {error}"))?;
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("invalid durable task working directory: {error}"))?;
    if !cwd.starts_with(&project_root) || !cwd.is_dir() {
        return Err(format!(
            "durable task working directory escapes its project: {}",
            cwd.display()
        ));
    }
    if !sessions_dir.is_dir() {
        return Err(format!(
            "durable task sessions directory is unavailable: {}",
            sessions_dir.display()
        ));
    }
    if SessionStore::at_dir(sessions_dir.to_path_buf())
        .load_result(session_id)
        .map_err(|error| format!("failed to inspect durable task session: {error}"))?
        .is_none()
    {
        return Err(format!("originating session not found: {session_id}"));
    }
    Ok(())
}

pub(crate) fn validate_task_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 160
        || !id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(format!(
            "background task id contains unsupported characters: {id}"
        ));
    }
    Ok(())
}

pub(crate) fn legacy_execution_id(record: &DurableTaskRecord) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let created_at = record.created_at.to_rfc3339();
    for byte in record
        .id
        .as_bytes()
        .iter()
        .chain(record.kind.as_bytes())
        .chain(created_at.as_bytes())
    {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("legacy-{hash:016x}")
}

pub(crate) fn validate_task_record_size(path: &Path, bytes: u64) -> Result<(), String> {
    if bytes > MAX_TASK_RECORD_BYTES {
        return Err(format!(
            "task record {} is {bytes} bytes; maximum is {MAX_TASK_RECORD_BYTES} bytes",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn validate_task_enumeration_size(
    path: &Path,
    bytes: u64,
    remaining_bytes: u64,
) -> Result<(), String> {
    if bytes > remaining_bytes {
        return Err(format!(
            "durable task enumeration exceeds its aggregate byte limit before reading {} ({bytes} bytes, {remaining_bytes} bytes remaining)",
            path.display(),
        ));
    }
    Ok(())
}

pub(crate) fn require_lease_token(
    task: &DurableTaskFile,
    task_id: &str,
    lease_token: &str,
) -> Result<(), String> {
    if task
        .worker_lease
        .as_ref()
        .is_some_and(|lease| lease.token == lease_token)
    {
        Ok(())
    } else {
        Err(worker_lease_lost(task_id))
    }
}

pub(crate) fn require_worker_owner(
    task: &DurableTaskFile,
    task_id: &str,
    owner: &WorkerOwner,
) -> Result<(), String> {
    require_lease_token(task, task_id, &owner.token)?;
    if task
        .record
        .worker_pid
        .is_none_or(|worker_pid| worker_pid == owner.pid)
    {
        Ok(())
    } else {
        Err(worker_lease_lost(task_id))
    }
}

pub(crate) fn worker_lease_lost(task_id: &str) -> String {
    format!("worker lease lost for background task {task_id}")
}

pub(crate) fn is_worker_lease_lost(error: &str) -> bool {
    error.starts_with("worker lease lost for background task ")
}

pub(crate) fn ensure_local_directory(path: &Path, label: &str) -> Result<(), String> {
    crate::fs_security::ensure_directory_without_symlinks(path)
        .map_err(|error| format!("failed to create {label} {}: {error}", path.display()))?;
    Ok(())
}

pub(crate) fn job_project_root(job: &DurableJob) -> &Path {
    match job {
        DurableJob::Terminal { project_root, .. } | DurableJob::Schedule { project_root, .. } => {
            project_root
        }
    }
}

pub(crate) fn finish_task_file(
    task: &mut DurableTaskFile,
    status: &str,
    result: Option<Value>,
    error: Option<String>,
) {
    task.record.status = status.to_string();
    task.record.result = result;
    task.record.error = error;
    task.record.worker_pid = None;
    task.record.updated_at = Utc::now();
    task.worker_lease = None;
    task.active_occurrence = None;
    scrub_completed_job(&mut task.job);
}

pub(crate) fn update_schedule_progress_file(
    task: &mut DurableTaskFile,
    occurrence: u32,
    next_run_at: Option<DateTime<Utc>>,
    run: Value,
) {
    let mut runs = task
        .record
        .result
        .as_ref()
        .and_then(|value| value.get("runs"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    runs.push(run);
    task.record.completed_occurrences = occurrence;
    task.record.next_run_at = next_run_at;
    task.record.result = Some(json!({
        "delivered_count": occurrence,
        "repeat_count": task.record.total_occurrences,
        "runs": runs,
        "execution_mode": "plan",
    }));
    task.record.updated_at = Utc::now();
    task.active_occurrence = None;
    if occurrence >= task.record.total_occurrences {
        task.record.status = "completed".to_string();
        task.record.worker_pid = None;
        task.worker_lease = None;
        scrub_completed_job(&mut task.job);
    }
}

pub(crate) fn scrub_completed_job(job: &mut DurableJob) {
    match job {
        DurableJob::Terminal { command, .. } => {
            *command = "[removed after task completion]".to_string();
        }
        DurableJob::Schedule { prompt, .. } => {
            *prompt = "[removed after schedule completion]".to_string();
        }
    }
}

pub(crate) fn worker_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("NIB_WORKER_EXECUTABLE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "NIB_WORKER_EXECUTABLE is not a file: {}",
            path.display()
        ));
    }
    let current = std::env::current_exe()
        .map_err(|error| format!("failed to locate nib worker executable: {error}"))?;
    if current
        .file_stem()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value == "nib")
    {
        return Ok(current);
    }
    if current.parent().and_then(Path::parent).is_some() {
        let debug_dir = current
            .parent()
            .and_then(Path::parent)
            .expect("checked parent");
        let candidate = debug_dir.join(if cfg!(windows) { "nib.exe" } else { "nib" });
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "cannot locate nib worker executable from {}; set NIB_WORKER_EXECUTABLE",
        current.display()
    ))
}

#[cfg(not(windows))]
pub(crate) static WORKER_REAPER: OnceLock<Result<Sender<Child>, String>> = OnceLock::new();

#[cfg(not(windows))]
pub(crate) fn worker_reaper_sender() -> Result<&'static Sender<Child>, String> {
    WORKER_REAPER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            std::thread::Builder::new()
                .name("nib-worker-reaper".to_string())
                .spawn(move || reap_worker_children(receiver))
                .map_err(|error| format!("failed to start durable worker reaper: {error}"))?;
            Ok(sender)
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(not(windows))]
pub(crate) fn reap_worker_children(receiver: Receiver<Child>) {
    let mut children: Vec<Child> = Vec::new();
    loop {
        match receiver.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(child) => children.push(child),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                for mut child in children {
                    let _ = child.wait();
                }
                return;
            }
        }

        reap_worker_children_once(&mut children, |child| child.try_wait());
    }
}

#[cfg(not(windows))]
pub(crate) fn reap_worker_children_once(
    children: &mut Vec<Child>,
    mut poll: impl FnMut(&mut Child) -> std::io::Result<Option<std::process::ExitStatus>>,
) {
    let mut index = 0;
    while index < children.len() {
        match poll(&mut children[index]) {
            Ok(Some(_)) => {
                let mut child = children.swap_remove(index);
                let _ = child.wait();
            }
            Ok(None) => index += 1,
            Err(_) => {
                let mut child = children.swap_remove(index);
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn hand_off_worker(sender: &Sender<Child>, child: Child) -> Result<(), String> {
    sender.send(child).map_err(|error| {
        let mut child = error.0;
        let pid = child.id();
        let _ = child.kill();
        let wait_error = child.wait().err();
        match wait_error {
            Some(wait_error) => format!(
                "durable worker reaper stopped before accepting pid {pid}; failed to reap worker: {wait_error}"
            ),
            None => format!("durable worker reaper stopped before accepting pid {pid}"),
        }
    })
}

#[cfg(not(windows))]
pub(crate) fn configure_worker_process(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

pub(crate) fn task_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash % TASK_LOCK_STRIPES as u64) as usize
}

pub(crate) fn legacy_visible_lock_id(name: &std::ffi::OsStr) -> Option<String> {
    let name = name.to_str()?;
    if name == ".admission.lock" || name.starts_with(".task-stripe-") {
        return None;
    }
    let id = name.strip_suffix(".lock")?;
    validate_task_id(id).ok()?;
    Some(id.to_string())
}

pub(crate) fn legacy_anchor_lock_id(name: &std::ffi::OsStr) -> Option<String> {
    let name = name.to_str()?;
    if name == ".admission.task.lock.anchor" || name.starts_with(".task-stripe-") {
        return None;
    }
    let id = name.strip_prefix('.')?.strip_suffix(".task.lock.anchor")?;
    validate_task_id(id).ok()?;
    Some(id.to_string())
}

#[derive(Debug)]
pub(crate) struct TaskLock {
    pub(crate) _file: File,
    pub(crate) _lock_directory: crate::daemons::state::StableDirectory,
    pub(crate) _anchor_directory: crate::daemons::state::StableDirectory,
}
