//! Split for T043 C02.

use super::*;

#[cfg(not(test))]
// The production launcher keeps request data, durable ownership, the start
// gate, and process authority explicit across the handoff protocol.
#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn launch_subagent_task(
    project_root: PathBuf,
    subagent_id: String,
    prompt: String,
    max_steps: u32,
    parent_session_id: Option<String>,
    prepared: PreparedSubagentTask,
    start_gate: tokio::sync::oneshot::Receiver<()>,
    authority: &SpawnPreparationAuthority,
    process_scope_plan: Option<SubagentProcessScopePlan>,
) -> Result<LaunchedSubagentTask, String> {
    let deadline = authority.operation_deadline();
    authority.verify_until(deadline)?;
    let PreparedSubagentTask {
        record,
        worktree,
        owner_lease,
    } = prepared;
    let execution_generation = owner_lease.execution_generation;
    let lease_id = owner_lease.lease_id.clone();
    let process_scope_plan = match process_scope_plan {
        Some(plan) => plan,
        None => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                "subagent supervisor launch has no preplanned process authority".to_string(),
                deadline,
            );
        }
    };
    let scope_store = match crate::sandbox::process::ProcessScopeStore::open_with_lock_deadline(
        &project_root,
        deadline,
    ) {
        Ok(store) => store,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                error,
                deadline,
            );
        }
    };
    let backend = match crate::sandbox::process::ProcessScopeBackend::production() {
        Ok(backend) => backend,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                error,
                deadline,
            );
        }
    };
    let owner_identity = match crate::sandbox::process::ProcessIdentity::current() {
        Ok(identity) => identity,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                error,
                deadline,
            );
        }
    };
    let mut process_scope = match scope_store.prepare_subagent_launch(
        &subagent_id,
        execution_generation,
        &process_scope_plan.cleanup_lease_id,
        &process_scope_plan.supervisor_registration_nonce,
        owner_identity,
        backend,
    ) {
        Ok(scope) => scope,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                None,
                error,
                deadline,
            );
        }
    };
    if let Err(error) = authority.verify_until(deadline) {
        return fail_unstarted_subagent(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            owner_lease,
            Some((&scope_store, &process_scope)),
            format!("subagent process-scope preparation exceeded its deadline: {error}"),
            deadline,
        );
    }
    let executable = match resolve_nib_executable() {
        Ok(executable) => executable,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                Some((&scope_store, &process_scope)),
                error,
                deadline,
            );
        }
    };
    if let Err(error) = pause_after_scope_prepared_before_supervisor_spawn(&subagent_id) {
        return fail_unstarted_subagent(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            owner_lease,
            Some((&scope_store, &process_scope)),
            error,
            deadline,
        );
    }
    let handoff_nonce = process_scope_plan.supervisor_registration_nonce.clone();
    let request = SubagentSupervisorRequest {
        version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
        handoff_nonce: handoff_nonce.clone(),
        subagent_id: subagent_id.clone(),
        execution_generation,
        owner_lease: lease_id.clone(),
        cleanup_lease_id: process_scope.cleanup_lease_id.clone(),
        worker: SubagentWorkerRequest { prompt, max_steps },
    };
    let mut encoded_request = match serde_json::to_vec(&request) {
        Ok(request) => request,
        Err(error) => {
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                Some((&scope_store, &process_scope)),
                format!("failed to encode subagent worker request: {error}"),
                deadline,
            );
        }
    };
    encoded_request.push(b'\n');

    let mut command = std::process::Command::new(&executable);
    command
        .arg("subagent-supervisor")
        .arg("--project-root")
        .arg(&project_root)
        .arg("--subagent-id")
        .arg(&subagent_id)
        .arg("--execution-generation")
        .arg(execution_generation.to_string())
        .arg("--owner-lease")
        .arg(&lease_id)
        .arg("--cleanup-lease-id")
        .arg(&process_scope.cleanup_lease_id)
        .arg("--supervisor-registration-nonce")
        .arg(&handoff_nonce)
        .arg("--worktree")
        .arg(&worktree.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    if let Err(error) = authority.verify_until(deadline) {
        return fail_unstarted_subagent(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            owner_lease,
            Some((&scope_store, &process_scope)),
            format!("subagent supervisor launch exceeded its deadline: {error}"),
            deadline,
        );
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("failed to start subagent supervisor: {error}");
            return fail_unstarted_subagent(
                &project_root,
                &subagent_id,
                execution_generation,
                &lease_id,
                owner_lease,
                Some((&scope_store, &process_scope)),
                message,
                deadline,
            );
        }
    };
    if let Err(error) = authority.verify_until(deadline) {
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            format!("subagent supervisor launch exceeded its deadline: {error}"),
        );
    }
    let supervisor_identity = match crate::sandbox::process::ProcessIdentity::capture(child.id()) {
        Ok(identity) => identity,
        Err(error) => {
            return fail_pre_delivery_subagent(
                PreDeliverySubagent {
                    project_root: &project_root,
                    id: &subagent_id,
                    execution_generation,
                    lease_id: &lease_id,
                    scope_store: &scope_store,
                    process_scope: &process_scope,
                    deadline,
                },
                owner_lease,
                child,
                format!("failed to identify the subagent supervisor: {error}"),
            );
        }
    };
    process_scope = loop {
        match scope_store.observe_registered_launch_supervisor(
            &subagent_id,
            execution_generation,
            &process_scope.cleanup_lease_id,
            &handoff_nonce,
            &supervisor_identity,
        ) {
            Ok(Some(scope)) => break scope,
            Ok(None) if Instant::now() < deadline => match child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Ok(Some(status)) => {
                    return fail_pre_delivery_subagent(
                        PreDeliverySubagent {
                            project_root: &project_root,
                            id: &subagent_id,
                            execution_generation,
                            lease_id: &lease_id,
                            scope_store: &scope_store,
                            process_scope: &process_scope,
                            deadline,
                        },
                        owner_lease,
                        child,
                        format!("subagent supervisor exited before self-registration: {status}"),
                    );
                }
                Err(error) => {
                    return fail_pre_delivery_subagent(
                        PreDeliverySubagent {
                            project_root: &project_root,
                            id: &subagent_id,
                            execution_generation,
                            lease_id: &lease_id,
                            scope_store: &scope_store,
                            process_scope: &process_scope,
                            deadline,
                        },
                        owner_lease,
                        child,
                        format!("failed to observe subagent supervisor: {error}"),
                    );
                }
            },
            Ok(None) => {
                return fail_pre_delivery_subagent(
                    PreDeliverySubagent {
                        project_root: &project_root,
                        id: &subagent_id,
                        execution_generation,
                        lease_id: &lease_id,
                        scope_store: &scope_store,
                        process_scope: &process_scope,
                        deadline,
                    },
                    owner_lease,
                    child,
                    "subagent supervisor did not self-register before its deadline".to_string(),
                );
            }
            Err(error) => {
                return fail_pre_delivery_subagent(
                    PreDeliverySubagent {
                        project_root: &project_root,
                        id: &subagent_id,
                        execution_generation,
                        lease_id: &lease_id,
                        scope_store: &scope_store,
                        process_scope: &process_scope,
                        deadline,
                    },
                    owner_lease,
                    child,
                    format!("failed to validate the self-registered subagent supervisor: {error}"),
                );
            }
        }
    };
    if let Err(error) = authority.verify_until(deadline) {
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            format!("subagent supervisor registration exceeded its deadline: {error}"),
        );
    }
    let supervisor_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            return fail_pre_delivery_subagent(
                PreDeliverySubagent {
                    project_root: &project_root,
                    id: &subagent_id,
                    execution_generation,
                    lease_id: &lease_id,
                    scope_store: &scope_store,
                    process_scope: &process_scope,
                    deadline,
                },
                owner_lease,
                child,
                "subagent supervisor stdin is unavailable".to_string(),
            );
        }
    };
    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("missing-stdout") {
        child.stdout.take();
        drop(supervisor_stdin);
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            "injected missing subagent supervisor stdout".to_string(),
        );
    }
    let supervisor_stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            drop(supervisor_stdin);
            return fail_pre_delivery_subagent(
                PreDeliverySubagent {
                    project_root: &project_root,
                    id: &subagent_id,
                    execution_generation,
                    lease_id: &lease_id,
                    scope_store: &scope_store,
                    process_scope: &process_scope,
                    deadline,
                },
                owner_lease,
                child,
                "subagent supervisor stdout is unavailable".to_string(),
            );
        }
    };
    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("readiness-monitor-failure") {
        drop(supervisor_stdout);
        drop(supervisor_stdin);
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            "injected subagent readiness-monitor failure".to_string(),
        );
    }
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(2);
    if let Err(error) = std::thread::Builder::new()
        .name(format!("nib-subagent-ready-{subagent_id}"))
        .spawn(move || {
            let mut reader = BufReader::new(supervisor_stdout);
            for phase in ["READY", "STARTED"] {
                let result = read_subagent_supervisor_frame(&mut reader, phase);
                let terminal = result.is_err();
                if ready_tx.send(result).is_err() || terminal {
                    break;
                }
            }
        })
    {
        let message = format!("failed to monitor subagent supervisor readiness: {error}");
        drop(supervisor_stdin);
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            message,
        );
    }

    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("wait-monitor-failure") {
        drop(supervisor_stdin);
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            "injected subagent exit-monitor failure".to_string(),
        );
    }

    let control = Arc::new(Mutex::new(Some(supervisor_stdin)));
    let (wait_tx, wait_rx) = tokio::sync::oneshot::channel();
    let wait_root = project_root.clone();
    let wait_id = subagent_id.clone();
    let wait_lease_id = lease_id.clone();
    let (ownership_tx, ownership_rx) = std::sync::mpsc::sync_channel(1);
    let wait_thread = std::thread::Builder::new()
        .name(format!("nib-subagent-supervisor-{subagent_id}"))
        .spawn(move || match ownership_rx.recv() {
            Ok((child, owner_lease)) => monitor_subagent_supervisor_exit(
                child,
                owner_lease,
                wait_root,
                wait_id,
                execution_generation,
                wait_lease_id,
                wait_tx,
            ),
            Err(error) => {
                let _ = wait_tx.send(Err(format!(
                    "subagent supervisor ownership handoff failed: {error}"
                )));
            }
        });
    if let Err(error) = wait_thread {
        let message = format!("failed to monitor subagent supervisor: {error}");
        if let Ok(mut stdin) = control.lock() {
            stdin.take();
        }
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            message,
        );
    }
    if let Err(error) = ownership_tx.send((child, owner_lease)) {
        let (child, owner_lease) = error.0;
        if let Ok(mut stdin) = control.lock() {
            stdin.take();
        }
        return fail_pre_delivery_subagent(
            PreDeliverySubagent {
                project_root: &project_root,
                id: &subagent_id,
                execution_generation,
                lease_id: &lease_id,
                scope_store: &scope_store,
                process_scope: &process_scope,
                deadline,
            },
            owner_lease,
            child,
            "subagent supervisor monitor stopped before ownership handoff".to_string(),
        );
    }

    if let Err(error) = authority.verify_until(deadline) {
        signal_supervisor_cancellation(&control);
        return Err(format!(
            "subagent supervisor ownership handoff exceeded its deadline: {error}"
        ));
    }

    authority.verify_until(deadline)?;
    let initialization = control
        .lock()
        .map_err(|_| "subagent supervisor control lock is poisoned".to_string())
        .and_then(|mut control| {
            let supervisor_stdin = control
                .as_mut()
                .ok_or("subagent supervisor control pipe is unavailable".to_string())?;
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("after-first-byte") {
                supervisor_stdin
                    .write_all(&encoded_request[..1])
                    .and_then(|()| supervisor_stdin.flush())
                    .map_err(|error| {
                        format!("failed to write the subagent launch failpoint byte: {error}")
                    })?;
                return Err("injected subagent launch failure after first request byte".to_string());
            }
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("after-request-body") {
                supervisor_stdin
                    .write_all(&encoded_request[..encoded_request.len() - 1])
                    .and_then(|()| supervisor_stdin.flush())
                    .map_err(|error| {
                        format!("failed to write the subagent launch failpoint body: {error}")
                    })?;
                return Err(
                    "injected subagent launch failure after request body before newline"
                        .to_string(),
                );
            }
            supervisor_stdin
                .write_all(&encoded_request)
                .map_err(|error| format!("failed to initialize subagent supervisor: {error}"))?;
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("after-request-write") {
                return Err("injected subagent launch failure after request write".to_string());
            }
            supervisor_stdin
                .flush()
                .map_err(|error| format!("failed to initialize subagent supervisor: {error}"))?;
            #[cfg(debug_assertions)]
            if subagent_launch_failpoint("after-request-flush") {
                return Err("injected subagent launch failure after request flush".to_string());
            }
            Ok(())
        });
    if let Err(error) = initialization {
        signal_supervisor_cancellation(&control);
        return Err(error);
    }
    if let Err(error) = authority.verify_until(deadline) {
        signal_supervisor_cancellation(&control);
        return Err(format!(
            "subagent supervisor initialization exceeded its deadline: {error}"
        ));
    }

    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("readiness-timeout") {
        signal_supervisor_cancellation(&control);
        return Err("injected subagent supervisor readiness timeout".to_string());
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        signal_supervisor_cancellation(&control);
        return Err("subagent supervisor readiness exceeded its preparation deadline".to_string());
    }
    let ready = match ready_rx.recv_timeout(remaining.min(SUBAGENT_SUPERVISOR_READY_TIMEOUT)) {
        Ok(Ok(frame)) => frame,
        Ok(Err(error)) => {
            signal_supervisor_cancellation(&control);
            return Err(format!(
                "failed to read subagent supervisor readiness: {error}"
            ));
        }
        Err(error) => {
            signal_supervisor_cancellation(&control);
            return Err(format!("subagent supervisor did not become ready: {error}"));
        }
    };
    if ready.version != SUBAGENT_SUPERVISOR_PROTOCOL_VERSION
        || ready.phase != "ready"
        || ready.handoff_nonce != handoff_nonce
        || ready.subagent_id != subagent_id
        || ready.execution_generation != execution_generation
        || ready.owner_lease != lease_id
        || ready.process_scope.scope_id != subagent_id
        || ready.process_scope.execution_generation != execution_generation
        || ready.process_scope.cleanup_lease_id != process_scope.cleanup_lease_id
        || ready.process_scope.status != crate::sandbox::process::ProcessScopeStatus::Running
        || ready.process_scope.launch_committed != Some(false)
    {
        signal_supervisor_cancellation(&control);
        return Err(
            "subagent supervisor READY frame does not match its gated execution authority"
                .to_string(),
        );
    }
    let persisted_ready_scope = scope_store.load(&subagent_id)?;
    if persisted_ready_scope != ready.process_scope {
        signal_supervisor_cancellation(&control);
        return Err(
            "subagent supervisor READY frame is not the exact durable process scope".to_string(),
        );
    }
    if let Err(error) = authority.verify_until(deadline) {
        signal_supervisor_cancellation(&control);
        return Err(format!(
            "subagent supervisor readiness exceeded its deadline: {error}"
        ));
    }

    let supervisor_guard = SupervisorControlGuard::new(Arc::clone(&control));
    let handle = tokio::spawn(async move {
        let mut guard = supervisor_guard;
        if start_gate.await.is_err() {
            return;
        }
        let _ = wait_rx.await;
        guard.disarm();
    });

    #[cfg(debug_assertions)]
    if subagent_launch_failpoint("abort-handle-failure") {
        handle.abort();
        return Err("injected subagent abort-handle attachment failure".to_string());
    }

    if let Err(error) =
        crate::daemons::task::TASK_MANAGER.attach_abort_handle(&subagent_id, handle.abort_handle())
    {
        handle.abort();
        return Err(error);
    }
    if let Err(error) = authority.verify_until(deadline) {
        handle.abort();
        return Err(format!(
            "subagent execution handoff exceeded its preparation deadline: {error}"
        ));
    }

    Ok(LaunchedSubagentTask {
        response: public_subagent_start_response(&record, parent_session_id),
        supervisor_handoff: Some(SubagentSupervisorHandoff {
            control,
            responses: ready_rx,
            ready,
        }),
    })
}

#[cfg(not(test))]
pub(crate) struct PreDeliverySubagent<'a> {
    pub(crate) project_root: &'a Path,
    pub(crate) id: &'a str,
    pub(crate) execution_generation: u64,
    pub(crate) lease_id: &'a str,
    pub(crate) scope_store: &'a crate::sandbox::process::ProcessScopeStore,
    pub(crate) process_scope: &'a crate::sandbox::process::ProcessScopeRecord,
    pub(crate) deadline: Instant,
}

#[cfg(not(test))]
pub(crate) fn fail_pre_delivery_subagent<T>(
    context: PreDeliverySubagent<'_>,
    owner_lease: SubagentOwnerLease,
    mut supervisor: std::process::Child,
    error: String,
) -> Result<T, String> {
    match terminate_unstarted_supervisor(&mut supervisor, context.deadline) {
        Ok(()) => fail_unstarted_subagent(
            context.project_root,
            context.id,
            context.execution_generation,
            context.lease_id,
            owner_lease,
            Some((context.scope_store, context.process_scope)),
            error,
            context.deadline,
        ),
        Err(cleanup_error) => {
            let _ = context.scope_store.mark_recovery_required(
                context.id,
                context.execution_generation,
                &context.process_scope.cleanup_lease_id,
                format!("unstarted supervisor cleanup was not proven: {cleanup_error}"),
            );
            let lease_cleanup = owner_lease.release_for_reconciliation().err();
            let lease_detail = lease_cleanup
                .map(|detail| format!("; owner-lease release failed: {detail}"))
                .unwrap_or_default();
            Err(format!(
                "{error}; supervisor cleanup remains unproven: {cleanup_error}{lease_detail}"
            ))
        }
    }
}

#[cfg(not(test))]
pub(crate) fn terminate_unstarted_supervisor(
    supervisor: &mut std::process::Child,
    deadline: Instant,
) -> Result<(), String> {
    let kill_error = supervisor
        .kill()
        .err()
        .filter(|error| error.kind() != std::io::ErrorKind::InvalidInput);
    loop {
        match supervisor.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                return Err(match kill_error {
                    Some(error) => format!(
                        "failed to terminate supervisor ({error}); it remained live for {} seconds",
                        SUBAGENT_SUPERVISOR_READY_TIMEOUT.as_secs()
                    ),
                    None => format!(
                        "supervisor remained live for {} seconds after termination",
                        SUBAGENT_SUPERVISOR_READY_TIMEOUT.as_secs()
                    ),
                });
            }
            Err(error) => {
                return Err(format!("failed to reap unstarted supervisor: {error}"));
            }
        }
    }
}

#[cfg(not(test))]
// Cleanup requires each exact persisted authority plus the original operation
// deadline; keeping them separate documents the fail-closed compensation set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fail_unstarted_subagent<T>(
    _project_root: &Path,
    _id: &str,
    _execution_generation: u64,
    _lease_id: &str,
    owner_lease: SubagentOwnerLease,
    process_scope: Option<(
        &crate::sandbox::process::ProcessScopeStore,
        &crate::sandbox::process::ProcessScopeRecord,
    )>,
    error: String,
    deadline: Instant,
) -> Result<T, String> {
    let mut details = Vec::new();
    if let Some((store, scope)) = process_scope {
        if let Err(cleanup) = store.remove_prepared(scope) {
            details.push(format!("process-scope cleanup failed: {cleanup}"));
        }
    }
    if let Err(cleanup) = owner_lease.remove_until(Some(deadline)) {
        details.push(format!("owner-lease cleanup failed: {cleanup}"));
    }
    if details.is_empty() {
        Err(error)
    } else {
        Err(format!("{error}; {}", details.join("; ")))
    }
}

#[cfg(not(test))]
pub(crate) struct SupervisorControlGuard {
    pub(crate) control: Arc<Mutex<Option<std::process::ChildStdin>>>,
    pub(crate) armed: bool,
}

#[cfg(not(test))]
impl SupervisorControlGuard {
    pub(crate) fn new(control: Arc<Mutex<Option<std::process::ChildStdin>>>) -> Self {
        Self {
            control,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
        if let Ok(mut control) = self.control.lock() {
            control.take();
        }
    }
}

#[cfg(not(test))]
impl Drop for SupervisorControlGuard {
    fn drop(&mut self) {
        if self.armed {
            signal_supervisor_cancellation(&self.control);
        }
    }
}

#[cfg(not(test))]
pub(crate) fn signal_supervisor_cancellation(
    control: &Arc<Mutex<Option<std::process::ChildStdin>>>,
) {
    if let Ok(mut control) = control.lock() {
        if let Some(mut stdin) = control.take() {
            let _ = stdin.write_all(b"cancel\n");
            let _ = stdin.flush();
        }
    }
}

#[cfg(all(debug_assertions, not(test)))]
pub(crate) fn subagent_launch_failpoint(expected: &str) -> bool {
    std::env::var(SUBAGENT_LAUNCH_FAILPOINT_ENV).as_deref() == Ok(expected)
}

#[doc(hidden)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub fn run_subagent_supervisor(
    project_root: &Path,
    subagent_id: &str,
    execution_generation: u64,
    owner_lease_id: &str,
    cleanup_lease_id: &str,
    supervisor_registration_nonce: &str,
    worktree: &Path,
) -> Result<(), String> {
    if !is_valid_subagent_id(subagent_id) {
        return Err("invalid subagent id".to_string());
    }
    validate_execution_ownership(execution_generation, owner_lease_id)?;
    let supervisor_identity = crate::sandbox::process::ProcessIdentity::current()?;
    pause_before_supervisor_self_registration(subagent_id, &supervisor_identity)?;
    let scope_store =
        crate::sandbox::process::ProcessScopeStore::open_existing_for_supervisor(project_root)?;
    let _registered_scope = scope_store.self_register_launch_supervisor(
        subagent_id,
        execution_generation,
        cleanup_lease_id,
        supervisor_registration_nonce,
        supervisor_identity.clone(),
    )?;
    scope_store.verify_visible()?;
    let project_root = canonical_project_root(project_root)?;
    scope_store.verify_visible()?;
    let record = get_subagent_record_unreconciled(&project_root, subagent_id)?;
    if record.status != "running"
        || !record_matches_execution(&record, execution_generation, owner_lease_id)?
    {
        return Err("subagent supervisor does not own the running record".to_string());
    }
    validate_record_worktree(&project_root, &record)?;
    let worktree = worktree
        .canonicalize()
        .map_err(|error| format!("subagent supervisor worktree is unavailable: {error}"))?;
    let record_worktree = record
        .worktree_path
        .canonicalize()
        .map_err(|error| format!("subagent record worktree is unavailable: {error}"))?;
    if worktree != record_worktree {
        return Err("subagent supervisor worktree does not match its record".to_string());
    }

    let stdin = std::io::stdin();
    let mut owner_input = BufReader::new(stdin);
    let request = read_supervisor_request(&mut owner_input)?;
    if request.version != SUBAGENT_SUPERVISOR_PROTOCOL_VERSION
        || uuid::Uuid::parse_str(&request.handoff_nonce)
            .map(|nonce| nonce.to_string())
            .ok()
            .as_deref()
            != Some(request.handoff_nonce.as_str())
        || request.subagent_id != subagent_id
        || request.execution_generation != execution_generation
        || request.owner_lease != owner_lease_id
        || request.cleanup_lease_id != cleanup_lease_id
        || request.handoff_nonce != supervisor_registration_nonce
        || request.worker.prompt != record.prompt
        || request.worker.max_steps > 100
    {
        return Err("subagent supervisor request does not match its record".to_string());
    }
    let process_scope = scope_store.load(subagent_id)?;
    if process_scope.execution_generation != execution_generation
        || process_scope.workload_kind != "subagent"
        || process_scope.status != crate::sandbox::process::ProcessScopeStatus::Prepared
        || process_scope.supervisor.as_ref() != Some(&supervisor_identity)
        || process_scope.supervisor_registration_nonce.as_deref()
            != Some(supervisor_registration_nonce)
        || process_scope.direct_child.is_some()
    {
        return Err(
            "subagent supervisor process scope changed before cleanup ownership".to_string(),
        );
    }
    let cleanup_lease = scope_store.acquire_cleanup_lease(&process_scope)?;
    let worker_input = serde_json::to_vec(&request.worker)
        .map_err(|error| format!("failed to encode subagent worker request: {error}"))?;
    let (commit_rx, owner_signal) = start_subagent_owner_protocol_reader(owner_input)?;
    let protocol_identity = request;
    let executable = resolve_nib_executable()?;
    let mut environment: Vec<_> = std::env::vars_os().collect();
    environment.push(("NIB_MANAGED_PROCESS_SCOPE".into(), subagent_id.into()));
    let ready_identity = protocol_identity.handoff_nonce.clone();
    let ready_subagent = protocol_identity.subagent_id.clone();
    let ready_owner = protocol_identity.owner_lease.clone();
    let commit_identity = protocol_identity.handoff_nonce.clone();
    let commit_subagent = protocol_identity.subagent_id.clone();
    let commit_owner = protocol_identity.owner_lease.clone();
    let started_identity = protocol_identity.handoff_nonce;
    let started_subagent = protocol_identity.subagent_id;
    let started_owner = protocol_identity.owner_lease;
    let output = crate::sandbox::process::supervise_foreground_with_claimed_cleanup_and_commit(
        &scope_store,
        &process_scope,
        cleanup_lease,
        owner_signal,
        crate::sandbox::process::SupervisedCommand {
            program: executable,
            args: vec![
                "subagent-worker".into(),
                "--worktree".into(),
                worktree.as_os_str().to_owned(),
                "--subagent-id".into(),
                subagent_id.into(),
            ],
            cwd: worktree.clone(),
            stdin: worker_input,
            environment,
        },
        |running| {
            write_subagent_supervisor_frame(&SubagentSupervisorFrame {
                version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
                phase: "ready".to_string(),
                handoff_nonce: ready_identity,
                subagent_id: ready_subagent,
                execution_generation,
                owner_lease: ready_owner,
                process_scope: running.clone(),
            })
        },
        |running| {
            let commit = commit_rx
                .recv_timeout(SUBAGENT_SUPERVISOR_READY_TIMEOUT)
                .map_err(|error| format!("subagent supervisor COMMIT wait failed: {error}"))??;
            let expected = SubagentSupervisorFrame {
                version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
                phase: "ready".to_string(),
                handoff_nonce: commit_identity,
                subagent_id: commit_subagent,
                execution_generation,
                owner_lease: commit_owner,
                process_scope: running.clone(),
            };
            validate_subagent_supervisor_frame(&expected, &commit, "commit")
        },
        |committed| {
            write_subagent_supervisor_frame(&SubagentSupervisorFrame {
                version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
                phase: "started".to_string(),
                handoff_nonce: started_identity,
                subagent_id: started_subagent,
                execution_generation,
                owner_lease: started_owner,
                process_scope: committed.clone(),
            })
        },
    )?;

    let persistence = if output.cancelled || output.owner_lost {
        persist_supervised_interruption(
            &project_root,
            subagent_id,
            execution_generation,
            owner_lease_id,
            &output.cleanup_proof,
            output.cancelled,
        )
    } else {
        let outcome = serde_json::from_slice::<SubagentWorkerResponse>(&output.stdout)
            .map(|response| response.outcome)
            .unwrap_or_else(|error| {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!(
                    "subagent worker returned an invalid response: {error}; stderr: {}",
                    stderr.trim()
                ))
            });
        persist_subagent_outcome_with_cleanup(
            &project_root,
            subagent_id,
            execution_generation,
            owner_lease_id,
            outcome,
            &output.cleanup_proof,
        )
    };
    persistence?;
    if output.owner_lost {
        if let OwnerLeaseProbe::Acquired(owner_lease) =
            SubagentOwnerLease::probe(&project_root, execution_generation, owner_lease_id)?
        {
            owner_lease.remove()?;
            let record = get_subagent_record_unreconciled(&project_root, subagent_id)?;
            let _ = retire_terminal_process_scope(&project_root, &record)?;
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn run_subagent_worker(worktree: &Path, subagent_id: &str) -> Result<(), String> {
    if !is_valid_subagent_id(subagent_id) {
        return Err("invalid subagent id".to_string());
    }
    #[cfg(all(debug_assertions, not(test)))]
    if let Some(path) = std::env::var_os(SUBAGENT_WORKER_STARTED_PATH_ENV) {
        std::fs::write(PathBuf::from(path), subagent_id.as_bytes()).map_err(|error| {
            format!("failed to publish subagent worker start sentinel: {error}")
        })?;
    }
    #[cfg(all(debug_assertions, not(test)))]
    if let Some(delay) = std::env::var_os(SUBAGENT_WORKER_DELAY_MS_ENV) {
        let delay = delay
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value <= 15_000)
            .ok_or("invalid debug subagent worker delay")?;
        std::thread::sleep(Duration::from_millis(delay));
    }
    let worktree = worktree
        .canonicalize()
        .map_err(|error| format!("subagent worker worktree is unavailable: {error}"))?;
    if !worktree.is_dir() {
        return Err("subagent worker worktree is not a directory".to_string());
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((MAX_SUBAGENT_WORKER_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read subagent worker request: {error}"))?;
    if bytes.len() > MAX_SUBAGENT_WORKER_REQUEST_BYTES {
        return Err("subagent worker request exceeds its size limit".to_string());
    }
    let request: SubagentWorkerRequest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid subagent worker request: {error}"))?;
    if request.prompt.trim().is_empty() || request.max_steps > 100 {
        return Err("subagent worker request is invalid".to_string());
    }
    let runtime = crate::agent::build_agent_runtime("failed to start subagent worker runtime")?;
    let config = crate::agent::AgentLoopConfig {
        max_steps: request.max_steps,
        auto_approve: false,
        approval_handler: Some(Arc::new(NonInteractiveSubagentApproval)),
        ..Default::default()
    };
    let session_lock_policy = crate::session::SessionStore::current_lock_policy();
    let worker_subagent_id = subagent_id.to_string();
    let worker_prompt = request.prompt;
    let outcome = crate::agent::block_on_agent_runtime_worker(
        &runtime,
        async move {
            crate::session::SessionStore::with_optional_lock_policy(
                session_lock_policy,
                crate::agent::run_agent_loop(worktree, &worker_subagent_id, &worker_prompt, config),
            )
            .await
        },
        "subagent runtime worker",
    )?;
    serde_json::to_writer(
        std::io::stdout().lock(),
        &SubagentWorkerResponse { outcome },
    )
    .map_err(|error| format!("failed to write subagent worker response: {error}"))
}

pub(crate) fn read_supervisor_request<R: BufRead>(
    reader: &mut R,
) -> Result<SubagentSupervisorRequest, String> {
    let mut bytes = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|error| format!("failed to read subagent supervisor request: {error}"))?;
        if available.is_empty() {
            return Err("subagent owner closed before sending a complete request".to_string());
        }
        let delimiter = available.iter().position(|byte| *byte == b'\n');
        let count = delimiter.map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(count) > MAX_SUBAGENT_WORKER_REQUEST_BYTES + 1 {
            return Err("subagent supervisor request exceeds its size limit".to_string());
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if delimiter.is_some() {
            bytes.pop();
            break;
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid subagent supervisor request: {error}"))
}

pub(crate) fn read_subagent_supervisor_frame<R: BufRead>(
    reader: &mut R,
    label: &str,
) -> Result<SubagentSupervisorFrame, String> {
    let bytes = read_bounded_supervisor_line(reader, label)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid subagent supervisor {label} frame: {error}"))
}

pub(crate) fn read_bounded_supervisor_line<R: BufRead>(
    reader: &mut R,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|error| {
            format!("failed to read subagent supervisor {label} frame: {error}")
        })?;
        if available.is_empty() {
            return Err(format!(
                "subagent supervisor closed before a complete {label} frame"
            ));
        }
        let delimiter = available.iter().position(|byte| *byte == b'\n');
        let count = delimiter.map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(count) > MAX_SUBAGENT_WORKER_REQUEST_BYTES + 1 {
            return Err(format!(
                "subagent supervisor {label} frame exceeds its size limit"
            ));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if delimiter.is_some() {
            bytes.pop();
            return Ok(bytes);
        }
    }
}

pub(crate) struct SubagentOwnerSignalReader {
    pub(crate) receiver: std::sync::mpsc::Receiver<bool>,
}

impl Read for SubagentOwnerSignalReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        match self.receiver.recv() {
            Ok(true) => {
                buffer[0] = b'!';
                Ok(1)
            }
            Ok(false) | Err(_) => Ok(0),
        }
    }
}

pub(crate) fn start_subagent_owner_protocol_reader<R: BufRead + Send + 'static>(
    mut reader: R,
) -> Result<
    (
        std::sync::mpsc::Receiver<Result<SubagentSupervisorFrame, String>>,
        SubagentOwnerSignalReader,
    ),
    String,
> {
    let (commit_tx, commit_rx) = std::sync::mpsc::sync_channel(1);
    let (signal_tx, signal_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("nib-subagent-owner-protocol".to_string())
        .spawn(move || {
            let commit = read_subagent_supervisor_frame(&mut reader, "COMMIT");
            let valid = commit.is_ok();
            let _ = commit_tx.send(commit);
            if !valid {
                let _ = signal_tx.send(false);
                return;
            }
            let mut byte = [0_u8; 1];
            loop {
                match reader.read(&mut byte) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(0) | Err(_) => {
                        let _ = signal_tx.send(false);
                        return;
                    }
                    Ok(_) => {
                        let _ = signal_tx.send(true);
                        return;
                    }
                }
            }
        })
        .map_err(|error| format!("failed to start subagent owner protocol reader: {error}"))?;
    Ok((
        commit_rx,
        SubagentOwnerSignalReader {
            receiver: signal_rx,
        },
    ))
}

pub(crate) fn write_subagent_supervisor_frame(
    frame: &SubagentSupervisorFrame,
) -> Result<(), String> {
    let encoded = serde_json::to_vec(frame)
        .map_err(|error| format!("failed to encode subagent supervisor frame: {error}"))?;
    if encoded.len() > MAX_SUBAGENT_WORKER_REQUEST_BYTES {
        return Err("subagent supervisor response frame exceeds its size limit".to_string());
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&encoded)
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("failed to write subagent supervisor frame: {error}"))
}

pub(crate) fn resolve_nib_executable() -> Result<PathBuf, String> {
    for variable in ["NIB_EXECUTABLE", "CARGO_BIN_EXE_nib"] {
        if let Some(path) = std::env::var_os(variable) {
            let path = PathBuf::from(path);
            let canonical = path.canonicalize().map_err(|error| {
                format!("{variable} does not name an available executable: {error}")
            })?;
            if canonical.is_file() {
                return Ok(canonical);
            }
        }
    }
    let current = std::env::current_exe()
        .map_err(|error| format!("failed to resolve the nib executable: {error}"))?;
    let expected_name = if cfg!(windows) { "nib.exe" } else { "nib" };
    if current.file_name().and_then(|name| name.to_str()) == Some(expected_name) {
        return current
            .canonicalize()
            .map_err(|error| format!("failed to canonicalize the nib executable: {error}"));
    }
    if current.parent().and_then(Path::parent).is_some() {
        let candidate = current
            .parent()
            .and_then(Path::parent)
            .expect("checked parent")
            .join(expected_name);
        if candidate.is_file() {
            return candidate.canonicalize().map_err(|error| {
                format!("failed to canonicalize the test nib executable: {error}")
            });
        }
    }
    Err("could not locate the nib executable for subagent supervision".to_string())
}

#[cfg(test)]
pub(crate) fn collect_spawn_compensation_sync(
    record_cleanup: impl FnOnce() -> Result<(), String>,
    worktree_cleanup: impl FnOnce() -> Result<(), String>,
    owner_lease_cleanup: impl FnOnce(OwnerLeaseCompensation) -> Result<(), String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let record_removed = match record_cleanup() {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!("record compensation failed: {error}"));
            false
        }
    };
    if let Err(error) = worktree_cleanup() {
        errors.push(format!("worktree compensation failed: {error}"));
    }
    let lease_action = if record_removed {
        OwnerLeaseCompensation::Remove
    } else {
        OwnerLeaseCompensation::ReleaseForReconciliation
    };
    if let Err(error) = owner_lease_cleanup(lease_action) {
        errors.push(format!("owner lease compensation failed: {error}"));
    }
    errors
}

pub(crate) fn collect_spawn_compensation_sync_with_audit(
    record_cleanup: impl FnOnce() -> Result<(), String>,
    worktree_cleanup: impl FnOnce() -> Result<(), String>,
    owner_lease_cleanup: impl FnOnce(OwnerLeaseCompensation) -> Result<(), String>,
    audit: PreparedSubagentAudit,
    authority: &SpawnPreparationAuthority,
) -> Vec<String> {
    let mut errors = Vec::new();
    let record_removed = match record_cleanup() {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!("record compensation failed: {error}"));
            false
        }
    };
    if let Err(error) = worktree_cleanup() {
        errors.push(format!("worktree compensation failed: {error}"));
    }
    let lease_action = if record_removed {
        OwnerLeaseCompensation::Remove
    } else {
        OwnerLeaseCompensation::ReleaseForReconciliation
    };
    if let Err(error) = owner_lease_cleanup(lease_action) {
        errors.push(format!("owner lease compensation failed: {error}"));
    }
    if record_removed {
        if let Err(error) = audit.cleanup_with_authority(authority) {
            errors.push(format!("audit preparation compensation failed: {error}"));
        }
    } else {
        audit.disarm();
    }
    errors
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerLeaseCompensation {
    Remove,
    ReleaseForReconciliation,
}

#[cfg(test)]
pub(crate) fn compensate_owner_lease(
    owner_lease: SubagentOwnerLease,
    action: OwnerLeaseCompensation,
) -> Result<(), String> {
    match action {
        OwnerLeaseCompensation::Remove => owner_lease.remove(),
        OwnerLeaseCompensation::ReleaseForReconciliation => {
            owner_lease.release_for_reconciliation()
        }
    }
}

#[cfg(test)]
pub(crate) async fn collect_spawn_compensation_async(
    record_cleanup: impl FnOnce() -> Result<(), String>,
    worktree_cleanup: impl std::future::Future<Output = Result<(), String>>,
    owner_lease_cleanup: impl FnOnce(OwnerLeaseCompensation) -> Result<(), String>,
) -> Vec<String> {
    let (record_removed, record_error) = match record_cleanup() {
        Ok(()) => (true, None),
        Err(error) => (false, Some(error)),
    };
    let lease_action = if record_removed {
        OwnerLeaseCompensation::Remove
    } else {
        OwnerLeaseCompensation::ReleaseForReconciliation
    };
    // Lease compensation must run before the first suspension point. The
    // worktree cleanup future continues its blocking cleanup task if this
    // caller is dropped while awaiting it.
    let owner_lease_error = owner_lease_cleanup(lease_action).err();
    let worktree_error = worktree_cleanup.await.err();
    let mut errors = Vec::new();
    if let Some(error) = record_error {
        errors.push(format!("record compensation failed: {error}"));
    }
    if let Some(error) = worktree_error {
        errors.push(format!("worktree compensation failed: {error}"));
    }
    if let Some(error) = owner_lease_error {
        errors.push(format!("owner lease compensation failed: {error}"));
    }
    errors
}

pub(crate) async fn collect_spawn_compensation_async_with_audit(
    record_cleanup: impl FnOnce() -> Result<(), String>,
    worktree_cleanup: impl std::future::Future<Output = Result<(), String>>,
    owner_lease_cleanup: impl FnOnce(OwnerLeaseCompensation) -> Result<(), String>,
    audit: PreparedSubagentAudit,
    authority: &SpawnPreparationAuthority,
) -> Vec<String> {
    let (record_removed, record_error) = match record_cleanup() {
        Ok(()) => (true, None),
        Err(error) => (false, Some(error)),
    };
    let lease_action = if record_removed {
        OwnerLeaseCompensation::Remove
    } else {
        OwnerLeaseCompensation::ReleaseForReconciliation
    };
    let owner_lease_error = owner_lease_cleanup(lease_action).err();
    let audit_error = if record_removed {
        audit.cleanup_with_authority(authority).err()
    } else {
        audit.disarm();
        None
    };
    let worktree_error = worktree_cleanup.await.err();
    let mut errors = Vec::new();
    if let Some(error) = record_error {
        errors.push(format!("record compensation failed: {error}"));
    }
    if let Some(error) = worktree_error {
        errors.push(format!("worktree compensation failed: {error}"));
    }
    if let Some(error) = owner_lease_error {
        errors.push(format!("owner lease compensation failed: {error}"));
    }
    if let Some(error) = audit_error {
        errors.push(format!("audit preparation compensation failed: {error}"));
    }
    errors
}

#[cfg(test)]
pub(crate) fn cleanup_precommit_worktree_sync(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
) -> Result<(), String> {
    crate::sandbox::worktree::Worktree::remove_precommit(project_root, worktree)
}

#[cfg(test)]
pub(crate) async fn cleanup_precommit_worktree(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
) -> Result<(), String> {
    let project_root = project_root.to_path_buf();
    let worktree = worktree.clone();
    tokio::task::spawn_blocking(move || {
        crate::sandbox::worktree::Worktree::remove_precommit_bounded_sync(
            &project_root,
            &worktree,
            SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT,
        )
    })
    .await
    .map_err(|error| format!("subagent worktree cleanup worker failed: {error}"))?
}

pub(crate) fn cleanup_precommit_worktree_sync_with_authority(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    #[cfg(test)]
    if consume_spawn_failure(&SPAWN_WORKTREE_CLEANUP_FAILURES) {
        return Err("injected subagent worktree cleanup failure".to_string());
    }
    let preparation = worktree.preparation_authority();
    let deadline = authority.operation_deadline();
    crate::sandbox::worktree::Worktree::cleanup_preparation_authority_until_with_guard(
        project_root,
        &preparation,
        deadline,
        SUBAGENT_PRECOMMIT_CLEANUP_TIMEOUT,
        || authority.verify_until(deadline),
    )
}

pub(crate) async fn cleanup_precommit_worktree_with_authority(
    project_root: &Path,
    worktree: &crate::sandbox::worktree::Worktree,
    authority: std::sync::Arc<SpawnPreparationAuthority>,
) -> Result<(), String> {
    let project_root = project_root.to_path_buf();
    let worktree = worktree.clone();
    tokio::task::spawn_blocking(move || {
        cleanup_precommit_worktree_sync_with_authority(&project_root, &worktree, &authority)
    })
    .await
    .map_err(|error| format!("subagent worktree cleanup worker failed: {error}"))?
}

pub(crate) fn compensate_owner_lease_with_authority(
    owner_lease: SubagentOwnerLease,
    action: OwnerLeaseCompensation,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    #[cfg(test)]
    if consume_spawn_failure(&SPAWN_OWNER_CLEANUP_FAILURES) {
        return Err("injected subagent owner cleanup failure".to_string());
    }
    match action {
        OwnerLeaseCompensation::Remove => {
            let deadline = authority.operation_deadline();
            owner_lease.remove_until_with_guard(Some(deadline), || authority.verify_until(deadline))
        }
        OwnerLeaseCompensation::ReleaseForReconciliation => {
            let verification = owner_lease.verify_pair();
            drop(owner_lease);
            authority.verify_until(authority.operation_deadline())?;
            verification
        }
    }
}

#[cfg(test)]
pub(crate) fn cleanup_precommit_record(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
) -> Result<(), String> {
    cleanup_precommit_record_with_hook(project_root, attempted_record, expected_publication, || {
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn cleanup_record_after_registration_failure(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    publication: &InitialSubagentRecordPublication,
) -> Result<(), String> {
    let exact_publication = publication
        .receipt
        .exact_identity
        .then_some(&publication.receipt.file);
    cleanup_precommit_record(project_root, attempted_record, exact_publication)
}

#[cfg(test)]
pub(crate) fn cleanup_record_after_publication_failure(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    error: &InitialSubagentRecordPublicationError,
) -> Result<(), String> {
    if !error.publication_attempted {
        return Ok(());
    }
    let exact_publication = error
        .receipt
        .as_ref()
        .filter(|receipt| receipt.exact_identity)
        .map(|receipt| &receipt.file);
    cleanup_precommit_record(project_root, attempted_record, exact_publication)
}

pub(crate) fn cleanup_record_after_publication_failure_locked(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    error: &InitialSubagentRecordPublicationError,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    if !error.publication_attempted {
        return authority.verify_until(authority.operation_deadline());
    }
    let exact = error
        .receipt
        .as_ref()
        .filter(|receipt| receipt.exact_identity)
        .map(|receipt| &receipt.file);
    cleanup_precommit_record_locked(project_root, attempted_record, exact, authority)
}

pub(crate) fn cleanup_record_after_registration_failure_locked(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    publication: &InitialSubagentRecordPublication,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    let exact = publication
        .receipt
        .exact_identity
        .then_some(&publication.receipt.file);
    cleanup_precommit_record_locked(project_root, attempted_record, exact, authority)
}

pub(crate) fn cleanup_precommit_record_locked(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    authority: &SpawnPreparationAuthority,
) -> Result<(), String> {
    let deadline = authority.operation_deadline();
    authority.verify_until(deadline)?;
    let path = record_path(project_root, &attempted_record.id)?;
    let quarantine = authority.records.deterministic_artifact_path(
        &path,
        ".nib-subagent-precommit-delete-",
        ".quarantine",
    )?;
    let canonical_exists = authority.records.path_exists(&path)?;
    let quarantine_exists = authority.records.path_exists(&quarantine)?;
    if !canonical_exists && !quarantine_exists {
        return authority.verify_until(deadline);
    }
    let expected = expected_publication.ok_or_else(|| {
        format!(
            "exact precommit subagent record publication identity is unavailable; preserved {}",
            path.display()
        )
    })?;
    let encoded = serde_json::to_vec_pretty(attempted_record)
        .map_err(|error| format!("failed to encode precommit subagent record: {error}"))?;
    let mut guard = || {
        authority.verify_until(deadline)?;
        verify_open_subagent_record_bytes(expected, &encoded)
    };
    if !canonical_exists {
        let quarantined = authority.records.open_read_write(&quarantine)?;
        if !crate::daemons::state::same_open_file_identity(expected, &quarantined)? {
            return Err(format!(
                "precommit subagent deletion quarantine has an unexpected identity; preserved {}",
                quarantine.display()
            ));
        }
        verify_open_subagent_record_bytes(&quarantined, &encoded)?;
        authority
            .records
            .remove_visible_file_if_matches_direct_with_guard(
                &quarantine,
                &quarantined,
                &mut guard,
            )?;
    } else {
        authority.records.verify_file_identity(&path, expected)?;
        verify_open_subagent_record_bytes(expected, &encoded)?;
        authority.records.remove_file_if_matches_with_guard(
            &path,
            expected,
            ".nib-subagent-precommit-delete-",
            &mut guard,
        )?;
    }
    authority.verify_until(deadline)
}

#[cfg(test)]
pub(crate) fn cleanup_precommit_record_with_hook(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    before_quarantine: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    cleanup_precommit_record_with_timeout_and_hook(
        project_root,
        attempted_record,
        expected_publication,
        SUBAGENT_RECORD_LOCK_TIMEOUT,
        before_quarantine,
    )
}

#[cfg(test)]
pub(crate) fn cleanup_precommit_record_with_timeout_and_hook(
    project_root: &Path,
    attempted_record: &SubagentRecord,
    expected_publication: Option<&File>,
    timeout: Duration,
    before_quarantine: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    cleanup_precommit_record_with_timeout_and_hooks(
        project_root,
        attempted_record,
        expected_publication,
        timeout,
        before_quarantine,
        || Ok(()),
    )
}
