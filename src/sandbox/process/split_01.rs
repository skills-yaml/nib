//! T043 split.

use super::*;

pub fn supervise_foreground_with_ready<R, F>(
    store: &ProcessScopeStore,
    prepared: &ProcessScopeRecord,
    owner_eof: R,
    command: SupervisedCommand,
    ready: F,
) -> Result<SupervisedOutput, String>
where
    R: Read + Send + 'static,
    F: FnOnce(&ProcessScopeRecord) -> Result<(), String>,
{
    validate_record(prepared)?;
    if prepared.status != ProcessScopeStatus::Prepared {
        return Err("managed-process supervisor requires a prepared scope".to_string());
    }
    if prepared.backend != ProcessScopeBackend::current()? {
        return Err("managed-process backend changed after scope preparation".to_string());
    }
    let cleanup_lease = store.acquire_cleanup_lease(prepared)?;
    supervise_foreground_with_claimed_cleanup(
        store,
        prepared,
        cleanup_lease,
        owner_eof,
        command,
        ready,
    )
}

#[doc(hidden)]
pub fn supervise_foreground_with_claimed_cleanup<R, F>(
    store: &ProcessScopeStore,
    prepared: &ProcessScopeRecord,
    cleanup_lease: CleanupLease,
    owner_eof: R,
    command: SupervisedCommand,
    ready: F,
) -> Result<SupervisedOutput, String>
where
    R: Read + Send + 'static,
    F: FnOnce(&ProcessScopeRecord) -> Result<(), String>,
{
    supervise_foreground_with_claimed_cleanup_and_commit(
        store,
        prepared,
        cleanup_lease,
        owner_eof,
        command,
        ready,
        |_| Ok(()),
        |_| Ok(()),
    )
}

#[doc(hidden)]
// READY, COMMIT, and STARTED are deliberately separate protocol callbacks, and
// the cleanup lease remains a distinct linear authority.
#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub fn supervise_foreground_with_claimed_cleanup_and_commit<R, F, C, S>(
    store: &ProcessScopeStore,
    prepared: &ProcessScopeRecord,
    cleanup_lease: CleanupLease,
    owner_eof: R,
    command: SupervisedCommand,
    ready: F,
    commit: C,
    started: S,
) -> Result<SupervisedOutput, String>
where
    R: Read + Send + 'static,
    F: FnOnce(&ProcessScopeRecord) -> Result<(), String>,
    C: FnOnce(&ProcessScopeRecord) -> Result<(), String>,
    S: FnOnce(&ProcessScopeRecord) -> Result<(), String>,
{
    validate_record(prepared)?;
    if prepared.status != ProcessScopeStatus::Prepared {
        return Err("managed-process supervisor requires a prepared scope".to_string());
    }
    if prepared.backend != ProcessScopeBackend::current()? {
        return Err("managed-process backend changed after scope preparation".to_string());
    }
    if cleanup_lease.execution_generation() != prepared.execution_generation
        || cleanup_lease.cleanup_lease_id() != prepared.cleanup_lease_id
    {
        return Err("managed-process cleanup lease does not own the prepared scope".to_string());
    }
    let supervisor_identity = ProcessIdentity::current()?;
    let launching = store.register_launch_supervisor(
        &prepared.scope_id,
        prepared.execution_generation,
        &prepared.cleanup_lease_id,
        supervisor_identity.clone(),
    )?;
    pause_before_supervised_spawn(&supervisor_identity)?;
    let mut child_scope = spawn_supervised_command(prepared.backend, &command)?;
    let direct_child = child_scope.scope_root.clone();
    let gated = store.register_gated_child(
        &launching.scope_id,
        launching.execution_generation,
        &launching.cleanup_lease_id,
        supervisor_identity.clone(),
        direct_child.clone(),
    )?;
    pause_before_running_publication(&child_scope.scope_root)?;
    let running = store.mark_gated_running(
        &gated.scope_id,
        gated.execution_generation,
        &gated.cleanup_lease_id,
        supervisor_identity,
        direct_child.clone(),
    )?;
    let (owner_eof_tx, owner_eof_rx) = mpsc::sync_channel(1);
    let owner_watcher = thread::Builder::new()
        .name(format!("nib-owner-eof-{}", prepared.scope_id))
        .spawn(move || {
            let mut owner_eof = owner_eof;
            let mut buffer = [0_u8; 1024];
            loop {
                match owner_eof.read(&mut buffer) {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(0) | Err(_) => {
                        let _ = owner_eof_tx.send(false);
                        break;
                    }
                    Ok(_) => {
                        // The launch request has already been consumed. Any remaining
                        // owner-channel data is the cancellation control frame.
                        let _ = owner_eof_tx.send(true);
                        break;
                    }
                }
            }
        })
        .map_err(|error| format!("failed to start owner-EOF watcher: {error}"))?;
    let stdout = child_scope
        .child
        .stdout
        .take()
        .ok_or("supervised child stdout is unavailable")?;
    let stderr = child_scope
        .child
        .stderr
        .take()
        .ok_or("supervised child stderr is unavailable")?;
    let stdout_reader = spawn_bounded_reader(stdout, "stdout")?;
    let stderr_reader = spawn_bounded_reader(stderr, "stderr")?;
    let committed = match ready(&running)
        .and_then(|()| commit(&running))
        .and_then(|()| store.commit_running_launch(&running))
    {
        Ok(committed) => committed,
        Err(error) => {
            child_scope.terminate()?;
            let _ = child_scope.wait_bounded()?;
            let stdout = join_bounded_reader(stdout_reader, "stdout")?;
            let stderr = join_bounded_reader(stderr_reader, "stderr")?;
            let descendants_reaped = child_scope.verify_descendants_reaped()?;
            if !descendants_reaped {
                return Err(format!(
                "managed-process gated launch cleanup was not proven after protocol failure: {error}"
            ));
            }
            let completed = store.complete_launch_abort(
                &running.scope_id,
                running.execution_generation,
                &running.cleanup_lease_id,
            )?;
            let proof = completed
                .launch_abort_proof
                .as_ref()
                .ok_or("gated managed-process abort has no launch-abort proof")?;
            cleanup_lease.release_after_launch_abort(proof)?;
            drop(owner_watcher);
            let output_detail = if stdout.is_empty() && stderr.is_empty() {
                String::new()
            } else {
                "; gated child output was suppressed".to_string()
            };
            return Err(format!(
                "managed-process launch commit was rejected: {error}{output_detail}"
            ));
        }
    };

    let mut owner_lost = false;
    let mut cancelled = false;
    let mut pending_owner_signal = owner_eof_rx.try_recv().ok();
    let input_writer = if pending_owner_signal.is_none() {
        child_scope.release_launch_gate()?;
        let child_stdin = child_scope
            .child
            .stdin
            .take()
            .ok_or("supervised child stdin is unavailable")?;
        Some(spawn_input_writer(child_stdin, command.stdin.clone())?)
    } else {
        None
    };
    let handoff_started = pending_owner_signal.is_none();
    let mut protocol_error = if handoff_started {
        started(&committed)
            .and_then(|()| store.verify_visible())
            .err()
    } else {
        None
    };
    let mut cleanup_lease = cleanup_lease;
    let long_lived_store = if handoff_started && protocol_error.is_none() {
        match store.rebind_long_lived_after_handoff().and_then(|rebound| {
            cleanup_lease.rebind_long_lived_after_handoff(store, &rebound)?;
            Ok(rebound)
        }) {
            Ok(rebound) => Some(rebound),
            Err(error) => {
                protocol_error = Some(format!(
                    "managed-process scope authority could not be retained after STARTED: {error}"
                ));
                None
            }
        }
    } else {
        None
    };
    if protocol_error.is_some() {
        pending_owner_signal = Some(true);
    }
    let store = long_lived_store.as_ref().unwrap_or(store);
    let exit_status = loop {
        if let Some(cancellation_requested) = pending_owner_signal
            .take()
            .or_else(|| owner_eof_rx.try_recv().ok())
        {
            cancelled = cancellation_requested;
            owner_lost = !cancellation_requested;
            store.begin_cleanup(
                &running.scope_id,
                running.execution_generation,
                &running.cleanup_lease_id,
                if cancellation_requested {
                    "owner_cancelled"
                } else {
                    "owner_eof"
                },
            )?;
            child_scope.terminate()?;
            break child_scope.wait_bounded()?;
        }
        match child_scope.observe_exit()? {
            ChildExitObservation::Exited(status) => {
                store.begin_cleanup(
                    &running.scope_id,
                    running.execution_generation,
                    &running.cleanup_lease_id,
                    "child_exit",
                )?;
                #[cfg(not(target_os = "linux"))]
                child_scope.terminate()?;
                break match status {
                    Some(status) => status,
                    None => child_scope.wait_bounded()?,
                };
            }
            ChildExitObservation::Running => thread::sleep(SUPERVISOR_POLL_INTERVAL),
        }
    };

    let input_error = match input_writer {
        Some(input_writer) => match input_writer.recv_timeout(SUPERVISOR_CLEANUP_TIMEOUT) {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(format!("supervised stdin writer did not finish: {error}")),
        },
        None => None,
    };
    let stdout = join_bounded_reader(stdout_reader, "stdout")?;
    let mut stderr = join_bounded_reader(stderr_reader, "stderr")?;
    if let Some(error) = &input_error {
        if !stderr.is_empty() {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(error.as_bytes());
    }
    drop(owner_watcher);
    let child_reaped = child_scope.verify_descendants_reaped()?;
    let outcome = if cancelled {
        "cancelled"
    } else if owner_lost {
        "owner_eof"
    } else if exit_status.success() && input_error.is_none() {
        "completed"
    } else {
        "child_failed"
    };
    let completed = store.complete_cleanup(
        &running.scope_id,
        running.execution_generation,
        &running.cleanup_lease_id,
        outcome,
        child_reaped,
    )?;
    let proof = completed
        .cleanup_proof
        .clone()
        .ok_or("completed managed-process scope has no cleanup proof")?;
    cleanup_lease.release_after_proof(&proof)?;
    let output = SupervisedOutput {
        exit_code: exit_status.code(),
        stdout,
        stderr,
        owner_lost,
        cancelled,
        cleanup_proof: proof,
    };
    match protocol_error {
        Some(error) => Err(format!(
            "managed-process STARTED acknowledgement failed after launch commit: {error}"
        )),
        None => Ok(output),
    }
}

#[cfg(debug_assertions)]
pub(crate) fn pause_before_running_publication(scope_root: &ProcessIdentity) -> Result<(), String> {
    let Some(marker) = std::env::var_os(PRE_RUNNING_PAUSE_ENV) else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(scope_root)
        .map_err(|error| format!("failed to encode pre-running test marker: {error}"))?;
    std::fs::write(&marker, bytes).map_err(|error| {
        format!(
            "failed to publish pre-running test marker {}: {error}",
            Path::new(&marker).display()
        )
    })?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(debug_assertions)]
pub(crate) fn pause_before_supervised_spawn(supervisor: &ProcessIdentity) -> Result<(), String> {
    let Some(marker) = std::env::var_os(PRE_SPAWN_PAUSE_ENV) else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(supervisor)
        .map_err(|error| format!("failed to encode pre-spawn test marker: {error}"))?;
    std::fs::write(&marker, bytes).map_err(|error| {
        format!(
            "failed to publish pre-spawn test marker {}: {error}",
            Path::new(&marker).display()
        )
    })?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(not(debug_assertions))]
pub(crate) fn pause_before_supervised_spawn(_supervisor: &ProcessIdentity) -> Result<(), String> {
    Ok(())
}

#[cfg(not(debug_assertions))]
pub(crate) fn pause_before_running_publication(
    _scope_root: &ProcessIdentity,
) -> Result<(), String> {
    Ok(())
}

#[cfg(all(debug_assertions, target_os = "linux"))]
pub(crate) fn pause_after_bwrap_spawn(child: &std::process::Child) -> Result<(), String> {
    let Some(marker) = std::env::var_os(POST_BWRAP_SPAWN_PAUSE_ENV) else {
        return Ok(());
    };
    let identity = ProcessIdentity::capture(child.id())
        .map_err(|error| format!("failed to identify the pre-handshake bwrap monitor: {error}"))?;
    let bytes = serde_json::to_vec(&identity)
        .map_err(|error| format!("failed to encode post-spawn test marker: {error}"))?;
    std::fs::write(&marker, bytes).map_err(|error| {
        format!(
            "failed to publish post-spawn test marker {}: {error}",
            Path::new(&marker).display()
        )
    })?;
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[cfg(all(not(debug_assertions), target_os = "linux"))]
pub(crate) fn pause_after_bwrap_spawn(_child: &std::process::Child) -> Result<(), String> {
    Ok(())
}

pub(crate) fn spawn_input_writer(
    mut stdin: std::process::ChildStdin,
    input: Vec<u8>,
) -> Result<mpsc::Receiver<Result<(), String>>, String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nib-supervisor-stdin".to_string())
        .spawn(move || {
            let result = stdin
                .write_all(&input)
                .and_then(|()| stdin.flush())
                .map_err(|error| format!("failed to write supervised child stdin: {error}"));
            drop(stdin);
            let _ = sender.send(result);
        })
        .map_err(|error| format!("failed to start supervised stdin writer: {error}"))?;
    Ok(receiver)
}

pub(crate) struct SupervisedChild {
    pub(crate) child: std::process::Child,
    pub(crate) backend: SupervisedBackendHandle,
    pub(crate) scope_root: ProcessIdentity,
    pub(crate) backend_cleanup_started: bool,
    #[cfg(windows)]
    pub(crate) windows_job_cleanup_verified: bool,
    pub(crate) direct_reaped: bool,
    pub(crate) cleanup_complete: bool,
}

pub(crate) enum SupervisedBackendHandle {
    #[cfg(target_os = "linux")]
    LinuxPidNamespace { monitor_group: i32 },
    #[cfg(target_os = "macos")]
    ProcessGroup(i32),
    #[cfg(windows)]
    WindowsJob(crate::sandbox::windows_job::WindowsJob),
}

impl SupervisedChild {
    pub(crate) fn release_launch_gate(&mut self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            let stdin = self
                .child
                .stdin
                .as_mut()
                .ok_or("supervised Linux launch gate stdin is unavailable")?;
            stdin.write_all(LINUX_LAUNCH_FRAME).map_err(|error| {
                format!("failed to release supervised Linux namespace init: {error}")
            })?;
            stdin.flush().map_err(|error| {
                format!("failed to flush supervised Linux launch gate: {error}")
            })?;
        }
        Ok(())
    }

    pub(crate) fn terminate(&mut self) -> Result<(), String> {
        self.backend_cleanup_started = true;
        #[cfg(target_os = "linux")]
        {
            let SupervisedBackendHandle::LinuxPidNamespace { monitor_group } = &self.backend;
            let result = cleanup_linux_namespace_and_monitor(
                &mut self.child,
                *monitor_group,
                &self.scope_root,
            );
            if self.child.try_wait().ok().flatten().is_some() {
                self.direct_reaped = true;
            }
            result
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut first_error = None;
            match &mut self.backend {
                #[cfg(target_os = "macos")]
                SupervisedBackendHandle::ProcessGroup(group) => {
                    if let Err(error) = signal_process_group(*group) {
                        first_error = Some(error);
                    }
                }
                #[cfg(windows)]
                SupervisedBackendHandle::WindowsJob(job) => {
                    match job.terminate_and_wait(SUPERVISOR_CLEANUP_TIMEOUT) {
                        Ok(()) => self.windows_job_cleanup_verified = true,
                        Err(error) => {
                            first_error = Some(format!(
                                "failed to terminate and verify Windows Job cleanup: {error}"
                            ));
                        }
                    }
                }
            }
            if !self.direct_reaped {
                match self.child.kill() {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
                    Err(error) => {
                        first_error.get_or_insert_with(|| {
                            format!("failed to terminate supervised child: {error}")
                        });
                    }
                }
            }
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    pub(crate) fn wait_bounded(&mut self) -> Result<std::process::ExitStatus, String> {
        let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.direct_reaped = true;
                    return Ok(status);
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(SUPERVISOR_POLL_INTERVAL);
                }
                Ok(None) => {
                    return Err(format!(
                        "supervised child did not exit within {} seconds",
                        SUPERVISOR_CLEANUP_TIMEOUT.as_secs()
                    ));
                }
                Err(error) => return Err(format!("failed to reap supervised child: {error}")),
            }
        }
    }

    pub(crate) fn observe_exit(&mut self) -> Result<ChildExitObservation, String> {
        #[cfg(unix)]
        {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if result == 0 && info.si_signo == libc::SIGCHLD {
                return Ok(ChildExitObservation::Exited(None));
            }
            if result == 0 && info.si_signo == 0 {
                return Ok(ChildExitObservation::Running);
            }
            if result != 0 {
                return Err(format!(
                    "failed to observe supervised child: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Err(format!(
                "supervised child returned unexpected wait signal {}",
                info.si_signo
            ))
        }
        #[cfg(windows)]
        {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.direct_reaped = true;
                    Ok(ChildExitObservation::Exited(Some(status)))
                }
                Ok(None) => Ok(ChildExitObservation::Running),
                Err(error) => Err(format!("failed to observe supervised child: {error}")),
            }
        }
    }

    pub(crate) fn verify_descendants_reaped(&mut self) -> Result<bool, String> {
        #[cfg(target_os = "linux")]
        let verified =
            wait_for_linux_identity_absent(&self.scope_root, SUPERVISOR_CLEANUP_TIMEOUT)?;
        #[cfg(target_os = "macos")]
        {
            if self.scope_root.still_matches() {
                return Ok(false);
            }
        }
        #[cfg(target_os = "macos")]
        let verified = {
            let SupervisedBackendHandle::ProcessGroup(group) = &self.backend;
            wait_for_process_group_empty(*group)?
        };
        #[cfg(windows)]
        let verified = {
            if !self.direct_reaped || !self.windows_job_cleanup_verified {
                return Ok(false);
            }
            let SupervisedBackendHandle::WindowsJob(job) = &mut self.backend;
            job.wait_until_empty(SUPERVISOR_CLEANUP_TIMEOUT)
                .map_err(|error| format!("failed to verify Windows Job cleanup: {error}"))?
        };
        self.cleanup_complete = verified;
        Ok(verified)
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if self.cleanup_complete {
            return;
        }
        #[cfg(target_os = "linux")]
        let backend_needs_cleanup =
            !self.direct_reaped || linux_identity_still_matches(&self.scope_root).unwrap_or(true);
        #[cfg(not(target_os = "linux"))]
        let backend_needs_cleanup = !self.backend_cleanup_started;
        if backend_needs_cleanup {
            let _ = self.terminate();
        }
        if !self.direct_reaped {
            let _ = self.wait_bounded();
        }
    }
}

pub(crate) enum ChildExitObservation {
    Running,
    Exited(Option<std::process::ExitStatus>),
}

pub(crate) fn spawn_supervised_command(
    backend: ProcessScopeBackend,
    command: &SupervisedCommand,
) -> Result<SupervisedChild, String> {
    spawn_supervised_command_inner(backend, command, false, true)
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn spawn_supervised_command_inner(
    backend: ProcessScopeBackend,
    command: &SupervisedCommand,
    inject_info_failure: bool,
    allow_post_spawn_pause: bool,
) -> Result<SupervisedChild, String> {
    if !command.cwd.is_absolute() || !command.cwd.is_dir() {
        return Err("supervised command cwd must be an existing absolute directory".to_string());
    }
    if !command.program.is_absolute() {
        return Err("supervised command program must be absolute".to_string());
    }

    #[cfg(target_os = "linux")]
    let (mut process, info_reader, info_writer) = {
        if backend != ProcessScopeBackend::LinuxPidNamespace {
            return Err("Linux supervisor received a non-Linux backend".to_string());
        }
        let (info_reader, info_writer) = create_cloexec_pipe("bubblewrap namespace information")?;
        let command_shell = crate::sandbox::command_shell_path()?;
        let info_write_fd = info_writer.as_raw_fd();
        let mut process = Command::new("bwrap");
        process.args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--die-with-parent",
            "--new-session",
            "--as-pid-1",
        ]);
        process
            .arg("--info-fd")
            .arg(info_write_fd.to_string())
            .arg("--bind");
        process.arg(&command.cwd).arg(&command.cwd).arg("--chdir");
        process
            .arg(&command.cwd)
            .arg("--")
            .arg(command_shell)
            .arg("-c")
            .arg(LINUX_LAUNCH_GATE_SCRIPT)
            .arg("nib-managed-launch-gate")
            .arg(&command.program);
        process.args(&command.args);
        (process, info_reader, info_writer)
    };
    #[cfg(target_os = "macos")]
    let mut process = {
        if backend != ProcessScopeBackend::MacosProcessGroup {
            return Err("macOS supervisor received a non-macOS backend".to_string());
        }
        let mut process = Command::new(&command.program);
        process.args(&command.args).current_dir(&command.cwd);
        process
    };
    #[cfg(windows)]
    let mut process = {
        if backend != ProcessScopeBackend::WindowsJobObject {
            return Err("Windows supervisor received a non-Windows backend".to_string());
        }
        let mut process = Command::new(&command.program);
        process.args(&command.args).current_dir(&command.cwd);
        process
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let mut process = {
        let _ = backend;
        let _ = inject_info_failure;
        let _ = allow_post_spawn_pause;
        return Err(
            "managed foreground process scopes are unsupported on this platform".to_string(),
        );
    };

    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process.envs(command.environment.iter().cloned());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let info_write_fd = info_writer.as_raw_fd();
        process.process_group(0);
        unsafe {
            process.pre_exec(move || {
                clear_close_on_exec(info_write_fd)?;
                Ok(())
            });
        }
        let mut child = process
            .spawn()
            .map_err(|error| format!("failed to start supervised command: {error}"))?;
        if allow_post_spawn_pause {
            if let Err(error) = pause_after_bwrap_spawn(&child) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }
        drop(info_writer);
        let group = match i32::try_from(child.id()) {
            Ok(group) => group,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("supervised process identifier exceeds the Unix pid range".to_string());
            }
        };
        let namespace_pid = match read_bwrap_namespace_pid(info_reader) {
            Ok(pid) if !inject_info_failure => pid,
            Ok(_) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, None);
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(
                        "injected bubblewrap namespace information failure".to_string(),
                        &mut child,
                    ),
                    cleanup,
                ));
            }
            Err(error) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, None);
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(error, &mut child),
                    cleanup,
                ));
            }
        };
        let scope_root = match ProcessIdentity::capture(namespace_pid) {
            Ok(identity) => identity,
            Err(error) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, None);
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(
                        format!("failed to identify supervised Linux namespace init: {error}"),
                        &mut child,
                    ),
                    cleanup,
                ));
            }
        };
        if let Err(error) = validate_bwrap_namespace_init(&scope_root, child.id()) {
            let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
            return Err(append_linux_spawn_cleanup_error(
                append_linux_launch_diagnostics(error, &mut child),
                cleanup,
            ));
        }
        let ready = child
            .stdout
            .as_mut()
            .ok_or_else(|| "supervised Linux launch gate stdout is unavailable".to_string());
        if let Err(error) = ready.and_then(read_linux_launch_ready) {
            let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
            return Err(append_linux_spawn_cleanup_error(
                append_linux_launch_diagnostics(error, &mut child),
                cleanup,
            ));
        }
        match ProcessIdentity::capture(scope_root.pid) {
            Ok(current) if current == scope_root => {}
            Ok(_) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(
                        "supervised Linux namespace init changed identity during launch"
                            .to_string(),
                        &mut child,
                    ),
                    cleanup,
                ));
            }
            Err(error) => {
                let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
                return Err(append_linux_spawn_cleanup_error(
                    append_linux_launch_diagnostics(
                        format!(
                            "failed to revalidate supervised Linux namespace init during launch: {error}"
                        ),
                        &mut child,
                    ),
                    cleanup,
                ));
            }
        }
        if let Err(error) = validate_bwrap_namespace_init(&scope_root, child.id()) {
            let cleanup = cleanup_failed_linux_spawn(&mut child, group, Some(&scope_root));
            return Err(append_linux_spawn_cleanup_error(
                append_linux_launch_diagnostics(error, &mut child),
                cleanup,
            ));
        }
        Ok(SupervisedChild {
            child,
            backend: SupervisedBackendHandle::LinuxPidNamespace {
                monitor_group: group,
            },
            scope_root,
            backend_cleanup_started: false,
            direct_reaped: false,
            cleanup_complete: false,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let _ = inject_info_failure;
        let _ = allow_post_spawn_pause;
        use std::os::unix::process::CommandExt;
        process.process_group(0);
        let mut child = process
            .spawn()
            .map_err(|error| format!("failed to start supervised command: {error}"))?;
        let group = i32::try_from(child.id())
            .map_err(|_| "supervised process identifier exceeds the Unix pid range".to_string())?;
        let scope_root = ProcessIdentity::capture(child.id()).map_err(|error| {
            let _ = signal_process_group(group);
            let _ = child.kill();
            let _ = child.wait();
            format!("failed to identify supervised child: {error}")
        })?;
        Ok(SupervisedChild {
            child,
            backend: SupervisedBackendHandle::ProcessGroup(group),
            scope_root,
            backend_cleanup_started: false,
            direct_reaped: false,
            cleanup_complete: false,
        })
    }
    #[cfg(windows)]
    {
        let _ = inject_info_failure;
        let _ = allow_post_spawn_pause;
        let (mut child, mut job) =
            crate::sandbox::windows_job::spawn_contained_std(&mut process)
                .map_err(|error| format!("failed to start supervised command: {error}"))?;
        let scope_root = ProcessIdentity::capture(child.id()).map_err(|error| {
            job.terminate();
            let _ = child.kill();
            let _ = child.wait();
            format!("failed to identify supervised child: {error}")
        })?;
        Ok(SupervisedChild {
            child,
            backend: SupervisedBackendHandle::WindowsJob(job),
            scope_root,
            backend_cleanup_started: false,
            windows_job_cleanup_verified: false,
            direct_reaped: false,
            cleanup_complete: false,
        })
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn probe_linux_managed_process_backend() -> Result<(), String> {
    run_linux_managed_process_probe_attempts(probe_linux_managed_process_backend_once)
}

#[cfg(target_os = "linux")]
pub(crate) fn run_linux_managed_process_probe_attempts(
    mut probe: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for attempt in 1..=LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS {
        match probe() {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retryable = linux_managed_process_probe_failure_is_retryable(&error);
                failures.push(format!("attempt {attempt}: {error}"));
                if !retryable || attempt == LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS {
                    return Err(format!(
                        "managed-process containment probe failed after {attempt} attempt(s): {}",
                        failures.join(" | ")
                    ));
                }
            }
        }
    }
    unreachable!("managed-process probe attempt range is non-empty")
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_managed_process_probe_failure_is_retryable(error: &str) -> bool {
    !error.contains("supervised launch cleanup was not proven")
        && !error.contains("bubblewrap stderr:")
        && [
            "launch gate closed before reporting readiness",
            "launch gate readiness timed out",
            "launch gate returned an invalid readiness frame",
        ]
        .iter()
        .any(|diagnostic| error.contains(diagnostic))
}

#[cfg(target_os = "linux")]
pub(crate) fn probe_linux_managed_process_backend_once() -> Result<(), String> {
    let shell = crate::sandbox::command_shell_path()?;
    let mut child = spawn_supervised_command_inner(
        ProcessScopeBackend::LinuxPidNamespace,
        &SupervisedCommand {
            program: shell,
            args: vec![OsString::from("-c"), OsString::from("sleep 60")],
            cwd: PathBuf::from("/tmp"),
            stdin: Vec::new(),
            environment: Vec::new(),
        },
        false,
        false,
    )?;

    child.release_launch_gate()?;
    thread::sleep(SUPERVISOR_POLL_INTERVAL);
    match child.observe_exit() {
        Ok(ChildExitObservation::Running) => {}
        Ok(ChildExitObservation::Exited(_)) => {
            return Err(
                "managed-process containment probe gate did not launch its command".to_string(),
            );
        }
        Err(error) => {
            return Err(format!(
                "failed to observe managed-process probe launch: {error}"
            ));
        }
    }
    let terminate_error = child.terminate().err();
    let wait_error = child.wait_bounded().err();
    let verify_error = match child.verify_descendants_reaped() {
        Ok(true) => None,
        Ok(false) => Some("managed-process probe did not reap its namespace".to_string()),
        Err(error) => Some(error),
    };
    if let Some(error) = terminate_error.or(wait_error).or(verify_error) {
        Err(error)
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn create_cloexec_pipe(label: &str) -> Result<(File, File), String> {
    let mut descriptors = [-1; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "failed to create {label} pipe: {}",
            std::io::Error::last_os_error()
        ));
    }
    let reader = unsafe { File::from_raw_fd(descriptors[0]) };
    let writer = unsafe { File::from_raw_fd(descriptors[1]) };
    Ok((reader, writer))
}

#[cfg(target_os = "linux")]
pub(crate) fn clear_close_on_exec(descriptor: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn read_linux_launch_ready(
    stdout: &mut std::process::ChildStdout,
) -> Result<(), String> {
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    let mut ready = [0_u8; LINUX_LAUNCH_READY_FRAME.len()];
    let mut offset = 0_usize;
    while offset < ready.len() {
        let now = Instant::now();
        if now >= deadline {
            return Err("supervised Linux launch gate readiness timed out".to_string());
        }
        let remaining = deadline.saturating_duration_since(now);
        let timeout = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if result == 0 {
            return Err("supervised Linux launch gate readiness timed out".to_string());
        }
        if result == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!(
                "failed to wait for supervised Linux launch gate readiness: {error}"
            ));
        }
        match stdout.read(&mut ready[offset..]) {
            Ok(0) => {
                return Err(
                    "supervised Linux launch gate closed before reporting readiness".to_string(),
                )
            }
            Ok(read) => offset += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(format!(
                    "failed to read supervised Linux launch gate readiness: {error}"
                ))
            }
        }
    }
    if ready != LINUX_LAUNCH_READY_FRAME {
        return Err("supervised Linux launch gate returned an invalid readiness frame".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn read_bwrap_namespace_pid(reader: File) -> Result<u32, String> {
    #[derive(Deserialize)]
    struct BwrapInfo {
        #[serde(rename = "child-pid")]
        pub(crate) child_pid: u32,
    }

    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("nib-bwrap-info".to_string())
        .spawn(move || {
            let result = (|| {
                let mut bytes = Vec::new();
                reader
                    .take(MAX_BWRAP_INFO_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| {
                        format!("failed to read bubblewrap namespace information: {error}")
                    })?;
                if bytes.len() as u64 > MAX_BWRAP_INFO_BYTES {
                    return Err(format!(
                        "bubblewrap namespace information exceeds the {MAX_BWRAP_INFO_BYTES}-byte limit"
                    ));
                }
                let info: BwrapInfo = serde_json::from_slice(&bytes).map_err(|error| {
                    format!("invalid bubblewrap namespace information: {error}")
                })?;
                if info.child_pid == 0 {
                    return Err(
                        "bubblewrap namespace information has an invalid child pid".to_string()
                    );
                }
                Ok(info.child_pid)
            })();
            let _ = sender.send(result);
        })
        .map_err(|error| {
            format!("failed to start bubblewrap namespace information reader: {error}")
        })?;
    receiver
        .recv_timeout(SUPERVISOR_CLEANUP_TIMEOUT)
        .map_err(|error| format!("bubblewrap namespace information was not ready: {error}"))?
}

#[cfg(target_os = "linux")]
pub(crate) fn append_linux_spawn_cleanup_error(
    launch_error: String,
    cleanup: Result<(), String>,
) -> String {
    match cleanup {
        Ok(()) => launch_error,
        Err(cleanup_error) => {
            format!("{launch_error}; supervised launch cleanup was not proven: {cleanup_error}")
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn append_linux_launch_diagnostics(
    mut launch_error: String,
    child: &mut std::process::Child,
) -> String {
    let status = match child.try_wait() {
        Ok(Some(status)) => status,
        Ok(None) => {
            launch_error.push_str("; bubblewrap monitor was still running after launch cleanup");
            return launch_error;
        }
        Err(error) => {
            launch_error.push_str(&format!(
                "; failed to inspect bubblewrap monitor after launch cleanup: {error}"
            ));
            return launch_error;
        }
    };
    launch_error.push_str(&format!("; bubblewrap monitor status: {status}"));
    let Some(mut stderr) = child.stderr.take() else {
        return launch_error;
    };
    let descriptor = stderr.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        launch_error.push_str(&format!(
            "; failed to make bubblewrap stderr nonblocking: {}",
            std::io::Error::last_os_error()
        ));
        return launch_error;
    }
    let mut bytes = Vec::new();
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    let mut timed_out = false;
    let mut read_error = None;
    while bytes.len() <= MAX_PROCESS_CLEANUP_TEXT_BYTES {
        let mut buffer = [0_u8; 1024];
        match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    timed_out = true;
                    break;
                }
                let remaining = deadline.saturating_duration_since(now);
                let timeout = i32::try_from(remaining.as_millis().max(1)).unwrap_or(i32::MAX);
                let mut poll_descriptor = libc::pollfd {
                    fd: descriptor,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                let result = unsafe { libc::poll(&mut poll_descriptor, 1, timeout) };
                if result == 0 {
                    timed_out = true;
                    break;
                }
                if result == -1 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    read_error = Some(error);
                    break;
                }
            }
            Err(error) => {
                read_error = Some(error);
                break;
            }
        }
    }
    let truncated = bytes.len() > MAX_PROCESS_CLEANUP_TEXT_BYTES;
    bytes.truncate(MAX_PROCESS_CLEANUP_TEXT_BYTES);
    let diagnostic = String::from_utf8_lossy(&bytes);
    let diagnostic = diagnostic.trim();
    if !diagnostic.is_empty() {
        launch_error.push_str("; bubblewrap stderr: ");
        launch_error.push_str(diagnostic);
        if truncated {
            launch_error.push_str(" [truncated]");
        }
    }
    if timed_out {
        launch_error.push_str("; timed out draining bubblewrap stderr");
    }
    if let Some(error) = read_error {
        launch_error.push_str(&format!(
            "; failed to read bubblewrap stderr after launch cleanup: {error}"
        ));
    }
    launch_error
}

#[cfg(target_os = "linux")]
pub(crate) fn cleanup_linux_namespace_and_monitor(
    child: &mut std::process::Child,
    monitor_group: i32,
    scope_root: &ProcessIdentity,
) -> Result<(), String> {
    let signal_result = signal_linux_process_identity(scope_root);
    let (initially_absent, initial_identity_error) =
        match wait_for_linux_identity_absent(scope_root, SUPERVISOR_CLEANUP_TIMEOUT) {
            Ok(absent) => (absent, None),
            Err(error) => (false, Some(error)),
        };

    if !initially_absent {
        let _ = signal_process_group(monitor_group);
        let _ = child.kill();
    }

    let mut reap_result = wait_for_child_reap(child, SUPERVISOR_CLEANUP_TIMEOUT);
    if reap_result.is_err() {
        let group_result = signal_process_group(monitor_group);
        let child_result = match child.kill() {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
            Err(error) => Err(format!("failed to terminate bubblewrap monitor: {error}")),
        };
        reap_result = wait_for_child_reap(child, SUPERVISOR_CLEANUP_TIMEOUT);
        group_result?;
        child_result?;
    }

    let final_identity_result = if initially_absent {
        Ok(true)
    } else {
        wait_for_linux_identity_absent(scope_root, SUPERVISOR_CLEANUP_TIMEOUT)
    };
    signal_result?;
    reap_result?;
    if let Some(error) = initial_identity_error {
        return Err(format!(
            "failed to observe namespace cleanup before forced monitor teardown: {error}"
        ));
    }
    match final_identity_result {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "bubblewrap namespace init {} survived ordered cleanup",
            scope_root.pid
        )),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn cleanup_failed_linux_spawn(
    child: &mut std::process::Child,
    monitor_group: i32,
    known_scope_root: Option<&ProcessIdentity>,
) -> Result<(), String> {
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    let mut scope_root = known_scope_root.cloned();

    while scope_root.is_none() && Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect bubblewrap monitor during launch cleanup: {error}"
                ));
            }
        }
        match discover_bwrap_namespace_init(child.id()) {
            Ok(Some(identity)) => scope_root = Some(identity),
            Ok(None) => thread::sleep(SUPERVISOR_POLL_INTERVAL),
            Err(error) => return Err(error),
        }
    }

    let Some(scope_root) = scope_root else {
        let _ = signal_process_group(monitor_group);
        let _ = child.kill();
        let _ = wait_for_child_reap(child, SUPERVISOR_CLEANUP_TIMEOUT);
        return Err(
            "bubblewrap monitor did not exit and its namespace init could not be identified"
                .to_string(),
        );
    };

    cleanup_linux_namespace_and_monitor(child, monitor_group, &scope_root)
}

#[cfg(target_os = "linux")]
pub(crate) fn discover_bwrap_namespace_init(
    monitor_pid: u32,
) -> Result<Option<ProcessIdentity>, String> {
    let children_path = format!("/proc/{monitor_pid}/task/{monitor_pid}/children");
    let children = match std::fs::read_to_string(&children_path) {
        Ok(children) => children,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to inspect bubblewrap monitor children {children_path}: {error}"
            ));
        }
    };
    let pids = children
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("bubblewrap monitor reported an invalid child pid: {error}"))?;
    if pids.is_empty() {
        return Ok(None);
    }
    if pids.len() != 1 {
        return Err(format!(
            "bubblewrap monitor {monitor_pid} has an ambiguous child set: {pids:?}"
        ));
    }
    let identity = ProcessIdentity::capture(pids[0])?;
    validate_bwrap_namespace_init(&identity, monitor_pid)?;
    Ok(Some(identity))
}

#[cfg(target_os = "linux")]
pub(crate) fn wait_for_child_reap(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(SUPERVISOR_POLL_INTERVAL);
            }
            Ok(None) => {
                return Err(format!(
                    "bubblewrap monitor did not exit within {} seconds",
                    timeout.as_secs()
                ));
            }
            Err(error) => return Err(format!("failed to reap bubblewrap monitor: {error}")),
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn validate_bwrap_namespace_init(
    identity: &ProcessIdentity,
    monitor_pid: u32,
) -> Result<(), String> {
    let status =
        std::fs::read_to_string(format!("/proc/{}/status", identity.pid)).map_err(|error| {
            format!(
                "failed to inspect supervised Linux namespace init {}: {error}",
                identity.pid
            )
        })?;
    let parent_pid = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|value| value.trim().parse::<u32>().ok())
        .ok_or_else(|| {
            format!(
                "supervised Linux namespace init {} has no valid parent pid",
                identity.pid
            )
        })?;
    if parent_pid != monitor_pid {
        return Err(format!(
            "bubblewrap reported process {} with unexpected parent {parent_pid}; expected monitor {monitor_pid}",
            identity.pid
        ));
    }
    let namespace_pids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .map(|value| {
            value
                .split_whitespace()
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map_err(|error| {
            format!(
                "supervised Linux namespace init {} has invalid namespace pid data: {error}",
                identity.pid
            )
        })?
        .ok_or_else(|| {
            format!(
                "supervised Linux namespace init {} has no namespace pid data",
                identity.pid
            )
        })?;
    if namespace_pids.len() < 2
        || namespace_pids.first() != Some(&identity.pid)
        || namespace_pids.last() != Some(&1)
    {
        return Err(format!(
            "bubblewrap reported process {} without namespace PID 1 identity",
            identity.pid
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn signal_linux_process_identity(identity: &ProcessIdentity) -> Result<(), String> {
    let pid = i32::try_from(identity.pid)
        .map_err(|_| "Linux process identifier exceeds the pid range".to_string())?;
    let raw_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw_pidfd == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) && !linux_identity_still_matches(identity)? {
            return Ok(());
        }
        return Err(format!(
            "failed to open exact Linux namespace-init handle {}: {error}",
            identity.pid
        ));
    }
    let pidfd = unsafe { File::from_raw_fd(raw_pidfd as libc::c_int) };
    match ProcessIdentity::capture(identity.pid) {
        Ok(current) if current == *identity => {}
        Ok(_) => {
            return Err(format!(
                "Linux namespace init {} changed identity before termination",
                identity.pid
            ));
        }
        Err(error) => {
            if !linux_identity_still_matches(identity)? {
                return Ok(());
            }
            return Err(format!(
                "failed to revalidate Linux namespace init {} before termination: {error}",
                identity.pid
            ));
        }
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!(
            "failed to terminate Linux namespace init {}: {error}",
            identity.pid
        ))
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn wait_for_linux_identity_absent(
    identity: &ProcessIdentity,
    timeout: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if !linux_identity_still_matches(identity)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }
}

#[cfg(unix)]
pub(crate) fn signal_process_group(group: i32) -> Result<(), String> {
    if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!(
            "failed to terminate supervised process group {group}: {error}"
        ))
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn wait_for_process_group_empty(group: i32) -> Result<bool, String> {
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    loop {
        if unsafe { libc::kill(-group, 0) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(true);
            }
            if error.raw_os_error() != Some(libc::EPERM) {
                return Err(format!(
                    "failed to verify supervised process group {group}: {error}"
                ));
            }
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }
}

pub(crate) fn spawn_bounded_reader<R: Read + Send + 'static>(
    mut reader: R,
    stream: &str,
) -> Result<mpsc::Receiver<Result<Vec<u8>, String>>, String> {
    let thread_name = format!("nib-supervisor-{stream}");
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let result = (|| {
                let mut retained = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    let count = reader.read(&mut buffer).map_err(|error| {
                        format!("failed to read supervised process output: {error}")
                    })?;
                    if count == 0 {
                        break;
                    }
                    if count >= MAX_SUPERVISED_OUTPUT_BYTES {
                        retained.clear();
                        retained.extend_from_slice(
                            &buffer[count - MAX_SUPERVISED_OUTPUT_BYTES.min(count)..count],
                        );
                        continue;
                    }
                    let overflow = retained
                        .len()
                        .saturating_add(count)
                        .saturating_sub(MAX_SUPERVISED_OUTPUT_BYTES);
                    if overflow > 0 {
                        retained.drain(..overflow);
                    }
                    retained.extend_from_slice(&buffer[..count]);
                }
                Ok(retained)
            })();
            let _ = sender.send(result);
        })
        .map_err(|error| format!("failed to start supervised {stream} reader: {error}"))?;
    Ok(receiver)
}

pub(crate) fn join_bounded_reader(
    reader: mpsc::Receiver<Result<Vec<u8>, String>>,
    stream: &str,
) -> Result<Vec<u8>, String> {
    reader
        .recv_timeout(SUPERVISOR_CLEANUP_TIMEOUT)
        .map_err(|error| format!("supervised {stream} did not drain after cleanup: {error}"))?
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CleanupLeaseRecord {
    pub(crate) version: u32,
    pub(crate) scope_id: String,
    pub(crate) execution_generation: u64,
    pub(crate) cleanup_lease_id: String,
}

pub struct CleanupLease {
    pub(crate) directory: crate::daemons::state::StableDirectory,
    pub(crate) path: PathBuf,
    pub(crate) file: Option<File>,
    pub(crate) record: CleanupLeaseRecord,
    pub(crate) lock_deadline: Option<Instant>,
    pub(crate) operation_timeout: Duration,
}

impl CleanupLease {
    pub fn execution_generation(&self) -> u64 {
        self.record.execution_generation
    }

    pub fn cleanup_lease_id(&self) -> &str {
        &self.record.cleanup_lease_id
    }

    pub(crate) fn rebind_long_lived_after_handoff(
        &mut self,
        startup_store: &ProcessScopeStore,
        long_lived_store: &ProcessScopeStore,
    ) -> Result<(), String> {
        startup_store.verify_visible()?;
        if !self
            .directory
            .same_identity(&long_lived_store.directory_capability)
        {
            return Err(
                "managed-process cleanup lease is bound to another scope namespace".to_string(),
            );
        }
        let directory = long_lived_store.directory_capability.try_clone()?;
        startup_store.verify_visible()?;
        self.directory = directory;
        self.lock_deadline = None;
        self.operation_timeout = long_lived_store.operation_timeout;
        Ok(())
    }

    pub fn release_after_proof(self, proof: &CleanupProof) -> Result<(), String> {
        self.release_after_proof_with_guard(proof, || Ok(()))
    }

    pub(crate) fn release_after_proof_with_guard(
        self,
        proof: &CleanupProof,
        before_namespace_step: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let deadline = self.effective_operation_deadline()?;
        self.release_after_proof_until(proof, deadline, before_namespace_step)
    }

    pub(crate) fn release_after_proof_until(
        mut self,
        proof: &CleanupProof,
        deadline: Instant,
        mut before_namespace_step: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        ensure_process_scope_deadline(Some(deadline))?;
        if proof.execution_generation != self.record.execution_generation
            || proof.cleanup_lease_id != self.record.cleanup_lease_id
            || !proof.descendants_reaped
        {
            return Err("cleanup proof does not own this managed-process lease".to_string());
        }
        let scope_path = self
            .path
            .with_file_name(format!("{}.json", self.record.scope_id));
        let scope_file = self.directory.open_read(&scope_path)?;
        let scope: ProcessScopeRecord = read_bounded_json(&scope_file, &scope_path)?;
        ensure_process_scope_deadline(Some(deadline))?;
        validate_record(&scope)?;
        if scope.status != ProcessScopeStatus::Complete
            || scope.cleanup_proof.as_ref() != Some(proof)
        {
            return Err(
                "cleanup proof is not the authoritative completed process scope".to_string(),
            );
        }
        let file = self
            .file
            .take()
            .ok_or("managed-process cleanup lease is already released")?;
        self.directory.remove_file_if_matches_with_guard(
            &self.path,
            &file,
            CLEANUP_LEASE_DELETE_PREFIX,
            || {
                before_namespace_step()?;
                ensure_process_scope_deadline(Some(deadline))
            },
        )?;
        self.directory.verify_visible()?;
        ensure_process_scope_deadline(Some(deadline))
    }

    pub fn release_after_launch_abort(self, proof: &LaunchAbortProof) -> Result<(), String> {
        self.release_after_launch_abort_with_guard(proof, || Ok(()))
    }

    pub(crate) fn release_after_launch_abort_with_guard(
        self,
        proof: &LaunchAbortProof,
        before_namespace_step: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let deadline = self.effective_operation_deadline()?;
        self.release_after_launch_abort_until(proof, deadline, before_namespace_step)
    }

    pub(crate) fn release_after_launch_abort_until(
        mut self,
        proof: &LaunchAbortProof,
        deadline: Instant,
        mut before_namespace_step: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        ensure_process_scope_deadline(Some(deadline))?;
        if proof.execution_generation != self.record.execution_generation
            || proof.cleanup_lease_id != self.record.cleanup_lease_id
            || !proof.workload_never_launched
        {
            return Err("launch-abort proof does not own this managed-process lease".to_string());
        }
        let scope_path = self
            .path
            .with_file_name(format!("{}.json", self.record.scope_id));
        let scope_file = self.directory.open_read(&scope_path)?;
        let scope: ProcessScopeRecord = read_bounded_json(&scope_file, &scope_path)?;
        ensure_process_scope_deadline(Some(deadline))?;
        validate_record(&scope)?;
        if scope.status != ProcessScopeStatus::Complete
            || scope.launch_abort_proof.as_ref() != Some(proof)
            || scope.cleanup_proof.is_some()
        {
            return Err(
                "launch-abort proof is not the authoritative completed process scope".to_string(),
            );
        }
        let file = self
            .file
            .take()
            .ok_or("managed-process cleanup lease is already released")?;
        self.directory.remove_file_if_matches_with_guard(
            &self.path,
            &file,
            CLEANUP_LEASE_DELETE_PREFIX,
            || {
                before_namespace_step()?;
                ensure_process_scope_deadline(Some(deadline))
            },
        )?;
        self.directory.verify_visible()?;
        ensure_process_scope_deadline(Some(deadline))
    }

    pub(crate) fn effective_operation_deadline(&self) -> Result<Instant, String> {
        let deadline = self
            .lock_deadline
            .unwrap_or_else(|| Instant::now() + self.operation_timeout);
        ensure_process_scope_deadline(Some(deadline))?;
        Ok(deadline)
    }
}

pub(crate) fn begin_cleanup_mutation(
    record: &mut ProcessScopeRecord,
    reason: String,
) -> Result<(), String> {
    if !matches!(
        record.status,
        ProcessScopeStatus::Running
            | ProcessScopeStatus::CleanupInProgress
            | ProcessScopeStatus::RecoveryRequired
    ) {
        return Err(format!(
            "managed process scope cannot begin cleanup from status {:?}",
            record.status
        ));
    }
    record.status = ProcessScopeStatus::CleanupInProgress;
    record.cleanup_reason = Some(reason);
    Ok(())
}

pub(crate) fn complete_cleanup_mutation(
    record: &mut ProcessScopeRecord,
    execution_generation: u64,
    cleanup_lease_id: &str,
    outcome: String,
    descendants_reaped: bool,
) -> Result<(), String> {
    if !matches!(
        record.status,
        ProcessScopeStatus::Running | ProcessScopeStatus::CleanupInProgress
    ) {
        return Err(format!(
            "managed process cleanup cannot complete from status {:?}",
            record.status
        ));
    }
    let direct_child = record
        .direct_child
        .clone()
        .ok_or("managed process scope has no registered direct child")?;
    if !descendants_reaped {
        return Err(
            "managed process scope cannot complete without descendant cleanup proof".to_string(),
        );
    }
    record.cleanup_proof = Some(CleanupProof {
        execution_generation,
        cleanup_lease_id: cleanup_lease_id.to_string(),
        backend: record.backend,
        direct_child,
        outcome,
        descendants_reaped,
        completed_at: Utc::now(),
    });
    record.status = ProcessScopeStatus::Complete;
    Ok(())
}

pub(crate) fn complete_launch_abort_mutation(
    record: &mut ProcessScopeRecord,
    execution_generation: u64,
    cleanup_lease_id: &str,
) -> Result<(), String> {
    if record.status != ProcessScopeStatus::Prepared
        && !(matches!(
            record.status,
            ProcessScopeStatus::Running
                | ProcessScopeStatus::CleanupInProgress
                | ProcessScopeStatus::RecoveryRequired
        ) && record.launch_committed == Some(false))
    {
        return Err(format!(
            "managed process launch abort cannot complete from status {:?}",
            record.status
        ));
    }
    let supervisor = record
        .supervisor
        .clone()
        .ok_or("managed process launch abort has no supervisor identity")?;
    record.cleanup_reason = Some(LAUNCH_ABORT_OUTCOME.to_string());
    record.launch_abort_proof = Some(LaunchAbortProof {
        execution_generation,
        cleanup_lease_id: cleanup_lease_id.to_string(),
        backend: record.backend,
        supervisor,
        namespace_root: record.direct_child.clone(),
        outcome: LAUNCH_ABORT_OUTCOME.to_string(),
        workload_never_launched: true,
        completed_at: Utc::now(),
    });
    record.status = ProcessScopeStatus::Complete;
    Ok(())
}
