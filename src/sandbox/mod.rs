//! Hybrid sandbox: executable bwrap isolation with a documented direct fallback.

pub mod process;
#[cfg(windows)]
#[doc(hidden)]
pub mod windows_job;
pub mod worktree;

use crate::config::BoundaryConfig;
use std::collections::{HashMap, VecDeque};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::LazyLock;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;

const POSIX_SHELL_ENV: &str = "NIB_POSIX_SHELL";
pub(crate) const MANAGED_PROCESS_SCOPE_ENV: &str = "NIB_MANAGED_PROCESS_SCOPE";

const INHERITED_ENVIRONMENT_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "TMPDIR",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
];

/// Resolves the POSIX shell used by terminal tools and skill hooks.
///
/// Windows installations normally receive this shell from Git for Windows. An
/// explicit path can be supplied through `NIB_POSIX_SHELL` on every platform.
pub fn command_shell_path() -> Result<PathBuf, String> {
    if let Some(configured) = std::env::var_os(POSIX_SHELL_ENV) {
        if configured.is_empty() {
            return Err(format!("{POSIX_SHELL_ENV} must not be empty"));
        }
        return resolve_executable(Path::new(&configured)).ok_or_else(|| {
            format!(
                "configured POSIX shell does not exist or is not a file: {}",
                Path::new(&configured).display()
            )
        });
    }

    if let Some(shell) = resolve_executable(Path::new("sh")) {
        return Ok(shell);
    }

    #[cfg(windows)]
    if let Some(shell) = git_for_windows_shell() {
        return Ok(shell);
    }

    Err(format!(
        "POSIX command shell `sh` is unavailable; install it{} or set {POSIX_SHELL_ENV} to its executable path",
        if cfg!(windows) {
            " with Git for Windows"
        } else {
            ""
        }
    ))
}

/// Probes the configured command shell for `nib doctor`.
pub fn check_command_shell() -> Result<PathBuf, String> {
    let shell = command_shell_path()?;
    let output = Command::new(&shell)
        .args(["-c", "exit 0"])
        .output()
        .map_err(|error| {
            format!(
                "failed to start POSIX command shell {}: {error}",
                shell.display()
            )
        })?;
    if output.status.success() {
        Ok(shell)
    } else {
        Err(format!(
            "POSIX command shell {} failed its startup probe with {}",
            shell.display(),
            output.status
        ))
    }
}

fn resolve_executable(program: &Path) -> Option<PathBuf> {
    if program.is_absolute() || program.components().count() > 1 {
        let candidate = if program.is_absolute() {
            program.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(program)
        };
        return canonical_executable(&candidate);
    }

    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        if let Some(executable) = canonical_executable(&candidate) {
            return Some(executable);
        }
        #[cfg(windows)]
        if candidate.extension().is_none() {
            if let Some(executable) = canonical_executable(&candidate.with_extension("exe")) {
                return Some(executable);
            }
        }
    }
    None
}

fn canonical_executable(path: &Path) -> Option<PathBuf> {
    path.is_file()
        .then(|| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

#[cfg(windows)]
fn git_for_windows_shell() -> Option<PathBuf> {
    let git = resolve_executable(Path::new("git"))?;
    for ancestor in git.ancestors().skip(1).take(5) {
        for relative in [Path::new("bin/sh.exe"), Path::new("usr/bin/sh.exe")] {
            if let Some(shell) = canonical_executable(&ancestor.join(relative)) {
                return Some(shell);
            }
        }
    }
    None
}

pub(crate) struct ManagedChild {
    child: Child,
    #[cfg(unix)]
    process_group: Option<i32>,
    #[cfg(windows)]
    windows_job: Option<windows_job::WindowsJob>,
}

impl ManagedChild {
    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            if let Some(pid) = self.owned_process_group_leader() {
                match wait_for_unix_child_exit_without_reaping(pid).await {
                    Ok(()) => self.terminate_process_tree(),
                    Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                        // Another direct wait has already consumed the child identity.
                        // The numeric process-group ID is no longer safe to signal.
                        self.process_group.take();
                    }
                    Err(error) => return Err(error),
                }
            }
            return self.child.wait().await;
        }

        #[cfg(not(unix))]
        {
            let status = self.child.wait().await;
            if status.is_ok() {
                self.terminate_process_tree();
            }
            status
        }
    }

    pub(crate) async fn terminate_and_reap(&mut self) {
        self.terminate_process_tree();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    #[cfg(unix)]
    fn terminate_process_tree(&mut self) {
        if self.owned_process_group_leader().is_some() {
            let process_group = self
                .process_group
                .take()
                .expect("validated process-group ownership");
            signal_process_group(process_group, 9);
        }
    }

    #[cfg(unix)]
    fn owned_process_group_leader(&mut self) -> Option<u32> {
        let process_group = self.process_group?;
        match self.child.id() {
            Some(pid) if i32::try_from(pid).ok() == Some(process_group) => Some(pid),
            _ => {
                self.process_group.take();
                None
            }
        }
    }

    #[cfg(windows)]
    fn terminate_process_tree(&mut self) {
        if let Some(mut job) = self.windows_job.take() {
            job.terminate();
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn terminate_process_tree(&mut self) {}
}

impl Deref for ManagedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for ManagedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.terminate_process_tree();
    }
}

pub(crate) fn spawn_managed_child(
    command: &mut tokio::process::Command,
) -> std::io::Result<ManagedChild> {
    spawn_child(command, true)
}

pub(crate) fn spawn_managed_stdio_relay_child(
    command: &mut tokio::process::Command,
) -> std::io::Result<ManagedChild> {
    // On Unix the relay must remain in the MCP server's process group so an
    // external owner can terminate the complete server tree. The relay handle
    // intentionally stores no group-kill authority and kills only its direct
    // child during an orderly server shutdown. Windows keeps a Job Object;
    // closing it when the server dies terminates the relay.
    #[cfg(unix)]
    {
        spawn_child(command, false)
    }
    #[cfg(not(unix))]
    {
        spawn_child(command, true)
    }
}

fn spawn_child(
    command: &mut tokio::process::Command,
    isolate_process_group: bool,
) -> std::io::Result<ManagedChild> {
    command.kill_on_drop(true);
    #[cfg(unix)]
    let isolate_process_group = isolate_process_group
        && !(cfg!(target_os = "macos") && std::env::var_os(MANAGED_PROCESS_SCOPE_ENV).is_some());
    #[cfg(unix)]
    if isolate_process_group {
        command.process_group(0);
    }

    #[cfg(windows)]
    let (child, windows_job) = if isolate_process_group {
        let (child, job) = windows_job::spawn_contained(command)?;
        (child, Some(job))
    } else {
        (command.spawn()?, None)
    };
    #[cfg(not(windows))]
    let child = command.spawn()?;
    #[cfg(unix)]
    let process_group = isolate_process_group
        .then(|| child.id().and_then(|id| i32::try_from(id).ok()))
        .flatten();
    Ok(ManagedChild {
        child,
        #[cfg(unix)]
        process_group,
        #[cfg(windows)]
        windows_job,
    })
}

#[cfg(unix)]
async fn wait_for_unix_child_exit_without_reaping(pid: u32) -> std::io::Result<()> {
    let pid = i32::try_from(pid)
        .map_err(|_| std::io::Error::other("child process identifier exceeds pid_t"))?;
    tokio::task::spawn_blocking(move || wait_for_unix_child_exit_without_reaping_blocking(pid))
        .await
        .map_err(|error| {
            std::io::Error::other(format!("child exit observer failed to join: {error}"))
        })?
}

#[cfg(unix)]
fn wait_for_unix_child_exit_without_reaping_blocking(pid: i32) -> std::io::Result<()> {
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: `info` points to writable storage and P_PID restricts the wait
        // to the exact child. WNOWAIT leaves that PID allocated until the caller
        // has signalled the owned process group.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(unix)]
fn signal_process_group(process_group: i32, signal: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    // The child is placed in a fresh process group before spawn, so a negative
    // PID targets only that child and the descendants it created.
    unsafe {
        let _ = kill(-process_group, signal);
    }
}

#[derive(Debug, Clone)]
pub struct SandboxCapabilities {
    pub bwrap_installed: bool,
    pub bwrap_available: bool,
    pub bwrap_error: Option<String>,
    pub managed_process_available: bool,
    pub managed_process_error: Option<String>,
    pub git_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

pub type OutputCallback = Arc<dyn Fn(OutputStream, &[u8]) + Send + Sync>;

#[derive(Debug)]
pub struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

impl BoundedOutput {
    pub fn stdout_truncated(&self) -> bool {
        self.stdout_bytes > self.stdout.len() as u64
    }

    pub fn stderr_truncated(&self) -> bool {
        self.stderr_bytes > self.stderr.len() as u64
    }
}

pub fn detect_capabilities() -> SandboxCapabilities {
    let bwrap_installed = command_succeeds("bwrap", &["--version"]);
    let (bwrap_available, bwrap_error) = if bwrap_installed {
        probe_bwrap()
    } else {
        (false, Some("bwrap executable not found".to_string()))
    };
    let (managed_process_available, managed_process_error) = managed_process_availability();

    SandboxCapabilities {
        bwrap_installed,
        bwrap_available,
        bwrap_error,
        managed_process_available,
        managed_process_error,
        git_available: command_succeeds("git", &["--version"]),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn require_managed_process_capability() -> Result<(), String> {
    let (available, error) = managed_process_availability();
    if available {
        Ok(())
    } else {
        Err(error.unwrap_or_else(|| {
            "strong foreground containment requires the gated bwrap and pidfd protocol on Linux"
                .to_string()
        }))
    }
}

fn managed_process_availability() -> (bool, Option<String>) {
    #[cfg(target_os = "linux")]
    {
        static MANAGED_PROCESS_PROBE: LazyLock<Result<(), String>> =
            LazyLock::new(process::probe_linux_managed_process_backend);
        match &*MANAGED_PROCESS_PROBE {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error.clone())),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        (
            false,
            Some("production managed-process supervision is Linux-only".to_string()),
        )
    }
}

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn probe_bwrap() -> (bool, Option<String>) {
    let output = Command::new("bwrap")
        .args([
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
            "--unshare-net",
            "--die-with-parent",
            "--new-session",
            "--bind",
            "/tmp",
            "/tmp",
            "--chdir",
            "/tmp",
            "--",
            "true",
        ])
        .output();

    match output {
        Ok(output) if output.status.success() => (true, None),
        Ok(output) => (
            false,
            Some(format!(
                "bwrap probe failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        ),
        Err(error) => (false, Some(format!("bwrap probe failed to start: {error}"))),
    }
}

/// Run a command inside bwrap when it is executable in the current environment.
/// A missing or unusable bwrap installation falls back to direct execution in `cwd`
/// unless that would violate an explicit fail-closed boundary.
pub async fn run_sandboxed(
    command: &str,
    cwd: &Path,
    profile: &str,
    boundaries: &BoundaryConfig,
) -> Result<(std::process::Output, Option<Vec<String>>), String> {
    run_sandboxed_with_environment(command, cwd, "hybrid", profile, boundaries, &HashMap::new())
        .await
}

pub async fn run_sandboxed_with_provider(
    command: &str,
    cwd: &Path,
    provider: &str,
    profile: &str,
    boundaries: &BoundaryConfig,
) -> Result<(std::process::Output, Option<Vec<String>>), String> {
    run_sandboxed_with_environment(command, cwd, provider, profile, boundaries, &HashMap::new())
        .await
}

pub async fn run_sandboxed_with_environment(
    command: &str,
    cwd: &Path,
    provider: &str,
    profile: &str,
    boundaries: &BoundaryConfig,
    environment: &HashMap<String, String>,
) -> Result<(std::process::Output, Option<Vec<String>>), String> {
    let cwd = canonical_directory(cwd)?;
    let capabilities = detect_capabilities();
    if provider == "internal" || profile == "internal" {
        ensure_direct_execution_allowed(boundaries)?;
        return run_direct(command, &cwd, environment)
            .await
            .map(|output| (output, None));
    }
    if !matches!(provider, "hybrid" | "bwrap") {
        return Err(format!("unsupported sandbox provider: {provider}"));
    }
    if capabilities.bwrap_available {
        return run_bwrap(command, &cwd, boundaries, profile, environment)
            .await
            .map(|(output, args)| (output, Some(args)));
    }
    if provider == "bwrap" {
        return Err(capabilities
            .bwrap_error
            .unwrap_or_else(|| "bwrap is unavailable".to_string()));
    }
    ensure_hybrid_fallback_allowed(boundaries, &capabilities)?;
    run_direct(command, &cwd, environment)
        .await
        .map(|output| (output, None))
}

/// Run a sandboxed command while retaining only the final `max_output_bytes`
/// from each output stream. The optional callback receives chunks as the child
/// produces them and must return promptly.
#[allow(clippy::too_many_arguments)]
pub async fn run_sandboxed_streaming_with_environment(
    command: &str,
    cwd: &Path,
    provider: &str,
    profile: &str,
    boundaries: &BoundaryConfig,
    environment: &HashMap<String, String>,
    max_output_bytes: usize,
    callback: Option<OutputCallback>,
) -> Result<(BoundedOutput, Option<Vec<String>>), String> {
    if max_output_bytes == 0 {
        return Err("sandbox output limit must be greater than zero".to_string());
    }
    let cwd = canonical_directory(cwd)?;
    let capabilities = detect_capabilities();
    if provider == "internal" || profile == "internal" {
        ensure_direct_execution_allowed(boundaries)?;
        let mut process = direct_shell_process(command, &cwd)?;
        process
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_child_environment(&mut process, environment);
        return capture_bounded(process, max_output_bytes, callback, true)
            .await
            .map(|output| (output, None));
    }
    if !matches!(provider, "hybrid" | "bwrap") {
        return Err(format!("unsupported sandbox provider: {provider}"));
    }
    if capabilities.bwrap_available {
        let args = build_bwrap_args(command, &cwd, boundaries, profile)?;
        let mut process = tokio::process::Command::new("bwrap");
        process
            .args(&args)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_child_environment(&mut process, environment);
        return capture_bounded(process, max_output_bytes, callback, false)
            .await
            .map(|output| (output, Some(args)));
    }
    if provider == "bwrap" {
        return Err(capabilities
            .bwrap_error
            .unwrap_or_else(|| "bwrap is unavailable".to_string()));
    }
    ensure_hybrid_fallback_allowed(boundaries, &capabilities)?;

    let mut process = direct_shell_process(command, &cwd)?;
    process
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_child_environment(&mut process, environment);
    capture_bounded(process, max_output_bytes, callback, true)
        .await
        .map(|output| (output, None))
}

async fn capture_bounded(
    mut command: tokio::process::Command,
    max_output_bytes: usize,
    callback: Option<OutputCallback>,
    isolate_process_group: bool,
) -> Result<BoundedOutput, String> {
    let mut child = spawn_child(&mut command, isolate_process_group)
        .map_err(|error| format!("sandbox command failed to start: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or("sandbox command stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("sandbox command stderr was not captured")?;
    let stdout_callback = callback.clone();
    let stdout_capture = capture_stream(
        stdout,
        OutputStream::Stdout,
        max_output_bytes,
        stdout_callback,
    );
    let stderr_capture = capture_stream(stderr, OutputStream::Stderr, max_output_bytes, callback);
    let wait = async {
        child
            .wait()
            .await
            .map_err(|error| format!("sandbox command wait failed: {error}"))
    };
    let (stdout, stderr, status) = tokio::try_join!(stdout_capture, stderr_capture, wait)?;
    Ok(BoundedOutput {
        status,
        stdout: stdout.bytes.into_iter().collect(),
        stderr: stderr.bytes.into_iter().collect(),
        stdout_bytes: stdout.total_bytes,
        stderr_bytes: stderr.total_bytes,
    })
}

struct CapturedStream {
    bytes: VecDeque<u8>,
    total_bytes: u64,
}

async fn capture_stream<R: AsyncRead + Unpin>(
    mut reader: R,
    stream: OutputStream,
    max_output_bytes: usize,
    callback: Option<OutputCallback>,
) -> Result<CapturedStream, String> {
    let mut captured = VecDeque::with_capacity(max_output_bytes.min(64 * 1024));
    let mut total_bytes = 0u64;
    let mut buffer = [0u8; 8 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("failed to read sandbox output: {error}"))?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        total_bytes = total_bytes.saturating_add(count as u64);
        if let Some(callback) = &callback {
            callback(stream, chunk);
        }
        if count >= max_output_bytes {
            captured.clear();
            captured.extend(&chunk[count - max_output_bytes..]);
            continue;
        }
        let overflow = captured
            .len()
            .saturating_add(count)
            .saturating_sub(max_output_bytes);
        if overflow > 0 {
            captured.drain(..overflow);
        }
        captured.extend(chunk);
    }
    Ok(CapturedStream {
        bytes: captured,
        total_bytes,
    })
}

fn canonical_directory(path: &Path) -> Result<PathBuf, String> {
    let canonical = path.canonicalize().map_err(|error| {
        format!(
            "invalid sandbox working directory {}: {error}",
            path.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "sandbox working directory is not a directory: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

async fn run_direct(
    command: &str,
    cwd: &Path,
    environment: &HashMap<String, String>,
) -> Result<std::process::Output, String> {
    let mut process = direct_shell_process(command, cwd)?;
    process.stdout(Stdio::piped()).stderr(Stdio::piped());
    apply_child_environment(&mut process, environment);
    capture_output(process, true).await
}

fn direct_shell_process(command: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
    let shell = command_shell_path()?;
    let mut process = tokio::process::Command::new(shell);
    process.arg("-c").arg(command).current_dir(cwd);
    Ok(process)
}

async fn capture_output(
    mut command: tokio::process::Command,
    isolate_process_group: bool,
) -> Result<std::process::Output, String> {
    let mut child = spawn_child(&mut command, isolate_process_group)
        .map_err(|error| format!("sandbox command failed to start: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("sandbox command stdout was not captured")?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or("sandbox command stderr was not captured")?;
    let stdout_capture = async {
        let mut bytes = Vec::new();
        stdout
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| format!("failed to read sandbox stdout: {error}"))?;
        Ok::<_, String>(bytes)
    };
    let stderr_capture = async {
        let mut bytes = Vec::new();
        stderr
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| format!("failed to read sandbox stderr: {error}"))?;
        Ok::<_, String>(bytes)
    };
    let wait = async {
        child
            .wait()
            .await
            .map_err(|error| format!("sandbox command wait failed: {error}"))
    };
    let (stdout, stderr, status) = tokio::try_join!(stdout_capture, stderr_capture, wait)?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn ensure_hybrid_fallback_allowed(
    boundaries: &BoundaryConfig,
    capabilities: &SandboxCapabilities,
) -> Result<(), String> {
    if boundaries.network != "disabled" {
        return Ok(());
    }

    Err(format!(
        "cannot enforce disabled network boundary without bwrap: {}",
        capabilities
            .bwrap_error
            .as_deref()
            .unwrap_or("bwrap is unavailable")
    ))
}

fn ensure_direct_execution_allowed(boundaries: &BoundaryConfig) -> Result<(), String> {
    if boundaries.network == "disabled" {
        return Err(
            "cannot enforce disabled network boundary with unisolated direct execution".to_string(),
        );
    }
    Ok(())
}

pub(crate) fn apply_child_environment(
    process: &mut tokio::process::Command,
    configured: &HashMap<String, String>,
) {
    process.env_clear();
    for key in INHERITED_ENVIRONMENT_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            process.env(key, value);
        }
    }
    for (key, value) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LC_") {
            process.env(key, value);
        }
    }
    for (key, value) in configured {
        if is_valid_environment_name(key) {
            process.env(key, value);
        }
    }
    if let Some(scope) = std::env::var_os(MANAGED_PROCESS_SCOPE_ENV) {
        process.env(MANAGED_PROCESS_SCOPE_ENV, scope);
    }
}

pub(crate) fn apply_std_child_environment(
    process: &mut std::process::Command,
    configured: &HashMap<String, String>,
) {
    process.env_clear();
    for key in INHERITED_ENVIRONMENT_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            process.env(key, value);
        }
    }
    for (key, value) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("LC_") {
            process.env(key, value);
        }
    }
    for (key, value) in configured {
        if is_valid_environment_name(key) {
            process.env(key, value);
        }
    }
    if let Some(scope) = std::env::var_os(MANAGED_PROCESS_SCOPE_ENV) {
        process.env(MANAGED_PROCESS_SCOPE_ENV, scope);
    }
}

fn is_valid_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn build_bwrap_args(
    command: &str,
    cwd: &Path,
    boundaries: &BoundaryConfig,
    _profile: &str,
) -> Result<Vec<String>, String> {
    let cwd_str = cwd.to_str().ok_or("sandbox cwd is not valid UTF-8")?;
    let mut args = vec![
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--unshare-pid".to_string(),
        "--unshare-ipc".to_string(),
        "--unshare-uts".to_string(),
        "--die-with-parent".to_string(),
        "--new-session".to_string(),
    ];

    if matches!(boundaries.network.as_str(), "restricted" | "disabled") {
        args.push("--unshare-net".to_string());
    }

    let home_isolation = append_home_isolation(&mut args, cwd)?;
    args.extend([
        "--bind".to_string(),
        cwd_str.to_string(),
        cwd_str.to_string(),
        "--chdir".to_string(),
        cwd_str.to_string(),
    ]);
    if !home_isolation.hidden {
        append_credential_masks(&mut args, cwd);
    }
    if let Some(cargo_home) = &home_isolation.cargo_home {
        append_cargo_credential_masks(&mut args, cargo_home)?;
    }

    for allowed in &boundaries.allow_write {
        let requested = PathBuf::from(allowed);
        let requested = if requested.is_absolute() {
            requested
        } else {
            cwd.join(requested)
        };
        let allowed_path = requested.canonicalize().map_err(|error| {
            format!(
                "configured writable path {} is unavailable: {error}",
                requested.display()
            )
        })?;
        if allowed_path == cwd {
            continue;
        }
        let allowed_str = allowed_path.to_str().ok_or_else(|| {
            format!(
                "writable path is not valid UTF-8: {}",
                allowed_path.display()
            )
        })?;
        args.extend([
            "--bind".to_string(),
            allowed_str.to_string(),
            allowed_str.to_string(),
        ]);
    }

    args.extend([
        "--".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        command.to_string(),
    ]);
    Ok(args)
}

#[derive(Debug)]
struct ToolchainHome {
    source: PathBuf,
    destination: PathBuf,
}

#[derive(Debug, Default)]
struct HomeIsolation {
    hidden: bool,
    cargo_home: Option<ToolchainHome>,
}

fn append_home_isolation(args: &mut Vec<String>, cwd: &Path) -> Result<HomeIsolation, String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(HomeIsolation::default());
    };
    let Ok(home) = home.canonicalize() else {
        return Ok(HomeIsolation::default());
    };
    if !home.is_dir() {
        return Ok(HomeIsolation::default());
    }

    let cargo_home = resolve_toolchain_home("CARGO_HOME", &home, ".cargo", cwd);
    let rustup_home = resolve_toolchain_home("RUSTUP_HOME", &home, ".rustup", cwd);
    let hidden = home != cwd && !home.starts_with(cwd);
    if hidden {
        let home_str = home
            .to_str()
            .ok_or_else(|| format!("home path is not valid UTF-8: {}", home.display()))?;
        args.extend(["--tmpfs".to_string(), home_str.to_string()]);
        for toolchain in [cargo_home.as_ref(), rustup_home.as_ref()]
            .into_iter()
            .flatten()
        {
            if toolchain.destination.starts_with(&home) {
                append_read_only_toolchain_mount(args, toolchain)?;
            }
        }
    }

    Ok(HomeIsolation { hidden, cargo_home })
}

fn resolve_toolchain_home(
    environment_key: &str,
    home: &Path,
    default_relative: &str,
    cwd: &Path,
) -> Option<ToolchainHome> {
    let configured = std::env::var_os(environment_key)
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(default_relative));
    let destination = if configured.is_absolute() {
        configured
    } else {
        cwd.join(configured)
    };
    let source = destination.canonicalize().ok()?;
    source.is_dir().then_some(ToolchainHome {
        source,
        destination,
    })
}

fn append_read_only_toolchain_mount(
    args: &mut Vec<String>,
    toolchain: &ToolchainHome,
) -> Result<(), String> {
    let source = toolchain.source.to_str().ok_or_else(|| {
        format!(
            "toolchain source is not valid UTF-8: {}",
            toolchain.source.display()
        )
    })?;
    let destination = toolchain.destination.to_str().ok_or_else(|| {
        format!(
            "toolchain destination is not valid UTF-8: {}",
            toolchain.destination.display()
        )
    })?;
    args.extend([
        "--dir".to_string(),
        destination.to_string(),
        "--ro-bind".to_string(),
        source.to_string(),
        destination.to_string(),
    ]);
    Ok(())
}

fn append_cargo_credential_masks(
    args: &mut Vec<String>,
    cargo_home: &ToolchainHome,
) -> Result<(), String> {
    for relative in ["credentials", "credentials.toml"] {
        let source = cargo_home.source.join(relative);
        if !source.is_file() {
            continue;
        }
        let destination = cargo_home.destination.join(relative);
        let destination = destination.to_str().ok_or_else(|| {
            format!(
                "Cargo credential path is not valid UTF-8: {}",
                destination.display()
            )
        })?;
        args.extend([
            "--ro-bind".to_string(),
            "/dev/null".to_string(),
            destination.to_string(),
        ]);
    }
    Ok(())
}

fn append_credential_masks(args: &mut Vec<String>, cwd: &Path) {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    append_credential_masks_for_home(args, &home, cwd);
}

fn append_credential_masks_for_home(args: &mut Vec<String>, home: &Path, cwd: &Path) {
    let candidates = [
        ".ssh",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        ".gnupg",
        ".password-store",
        ".config/gcloud",
        ".config/gh",
        ".config/op",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
    ];
    let mut masked = std::collections::HashSet::new();
    for relative in candidates {
        let candidate = home.join(relative);
        let Ok(target) = candidate.canonicalize() else {
            continue;
        };
        if target.starts_with(cwd) || cwd.starts_with(&target) || !masked.insert(target.clone()) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&target) else {
            continue;
        };
        let Some(target) = target.to_str() else {
            continue;
        };
        if metadata.is_dir() {
            args.extend(["--tmpfs".to_string(), target.to_string()]);
        } else if metadata.is_file() {
            args.extend([
                "--ro-bind".to_string(),
                "/dev/null".to_string(),
                target.to_string(),
            ]);
        }
    }
}

async fn run_bwrap(
    command: &str,
    cwd: &Path,
    boundaries: &BoundaryConfig,
    profile: &str,
    environment: &HashMap<String, String>,
) -> Result<(std::process::Output, Vec<String>), String> {
    let args = build_bwrap_args(command, cwd, boundaries, profile)?;
    let mut process = tokio::process::Command::new("bwrap");
    process
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_child_environment(&mut process, environment);
    let output = capture_output(process, false)
        .await
        .map_err(|error| format!("bwrap execution failed: {error}"))?;
    Ok((output, args))
}

pub fn doctor_report() -> String {
    let capabilities = detect_capabilities();
    let bwrap = if capabilities.bwrap_available {
        "available".to_string()
    } else if capabilities.bwrap_installed {
        format!(
            "installed but unusable ({})",
            capabilities
                .bwrap_error
                .as_deref()
                .unwrap_or("unknown error")
        )
    } else {
        "missing".to_string()
    };
    let supervised_subagents = match process::ProcessScopeBackend::production() {
        Ok(process::ProcessScopeBackend::LinuxPidNamespace) => {
            "available (Linux bwrap PID namespace)".to_string()
        }
        Ok(process::ProcessScopeBackend::WindowsJobObject) => unreachable!(),
        Ok(process::ProcessScopeBackend::MacosProcessGroup) => unreachable!(),
        Err(error) => format!("unavailable ({error})"),
    };
    format!(
        "Sandbox capabilities:\n  bwrap: {bwrap}\n  git: {}\n  supervised subagents: {supervised_subagents}\n  fallback: direct execution inside the session worktree when explicit boundaries permit",
        if capabilities.git_available {
            "available"
        } else {
            "missing"
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::{OsStr, OsString};
    #[cfg(unix)]
    use std::time::Duration;
    use tempfile::tempdir;

    struct EnvironmentVariableGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvironmentVariableGuard {
        fn set(key: &'static str, value: &OsStr) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        #[cfg(windows)]
        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvironmentVariableGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_exit_observer_leaves_the_child_waitable() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("spawn exit fixture");
        let pid = i32::try_from(child.id()).expect("numeric child pid");

        wait_for_unix_child_exit_without_reaping_blocking(pid)
            .expect("observe exit without reaping");

        let status = child
            .try_wait()
            .expect("child remains waitable")
            .expect("child has exited");
        assert_eq!(status.code(), Some(7));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_wait_discards_stale_group_authority_after_direct_reap() {
        let mut victim_command = tokio::process::Command::new("sh");
        victim_command.args(["-c", "sleep 60"]);
        let mut victim = spawn_managed_child(&mut victim_command).expect("spawn victim group");
        let victim_group = victim.process_group.expect("victim process group");

        let mut completed_command = tokio::process::Command::new("sh");
        completed_command.args(["-c", "exit 0"]);
        let mut completed =
            spawn_managed_child(&mut completed_command).expect("spawn completed child");
        assert!(completed.child.wait().await.expect("direct reap").success());
        assert!(completed.child.id().is_none());

        // Simulate the stored numeric PGID being reused after a caller directly
        // reaped the leader through DerefMut.
        completed.process_group = Some(victim_group);
        assert!(completed.wait().await.expect("cached wait").success());
        assert!(
            victim.try_wait().expect("inspect victim").is_none(),
            "wait signalled a process group after losing the leader identity"
        );

        victim.terminate_and_reap().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_wait_terminates_lingering_group_before_reaping_leader() {
        let root = tempdir().expect("pid marker root");
        let marker = root.path().join("descendant.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                "sleep 60 & printf '%s' \"$!\" > \"$NIB_TEST_DESCENDANT_PID\"",
            ])
            .env("NIB_TEST_DESCENDANT_PID", &marker);
        let mut managed = spawn_managed_child(&mut command).expect("spawn managed group");

        assert!(managed.wait().await.expect("wait managed group").success());
        let descendant_pid: i32 = std::fs::read_to_string(&marker)
            .expect("read descendant pid")
            .parse()
            .expect("numeric descendant pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let result = unsafe { libc::kill(descendant_pid, 0) };
            if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "managed wait returned while its lingering process-group member was still live"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    #[test]
    #[serial]
    fn windows_command_shell_resolution_finds_git_for_windows_installation() {
        let directory = tempdir().expect("Git for Windows fixture");
        let git_root = directory.path().join("Git");
        let git_bin = git_root.join("cmd");
        let shell = git_root.join("bin/sh.exe");
        std::fs::create_dir_all(&git_bin).expect("Git command directory");
        std::fs::create_dir_all(shell.parent().expect("shell parent"))
            .expect("Git shell directory");
        std::fs::write(git_bin.join("git.exe"), b"fixture").expect("Git fixture executable");
        std::fs::write(&shell, b"fixture").expect("shell fixture executable");
        let _shell_override = EnvironmentVariableGuard::remove(POSIX_SHELL_ENV);
        let _path = EnvironmentVariableGuard::set("PATH", git_bin.as_os_str());

        assert_eq!(
            command_shell_path().expect("resolve Git for Windows shell"),
            shell.canonicalize().expect("canonical shell fixture")
        );
    }

    #[tokio::test]
    #[serial]
    async fn child_process_inherits_only_allowlisted_and_configured_environment() {
        const INHERITED_SECRET: &str = "NIB_TEST_INHERITED_SECRET";
        let _secret =
            EnvironmentVariableGuard::set(INHERITED_SECRET, OsStr::new("must-not-reach-child"));
        let _managed_scope =
            EnvironmentVariableGuard::set(MANAGED_PROCESS_SCOPE_ENV, OsStr::new("scope-fixture"));

        let directory = tempdir().expect("tempdir");
        let environment = HashMap::from([
            ("NIB_CONFIGURED_VALUE".to_string(), "explicit".to_string()),
            (
                MANAGED_PROCESS_SCOPE_ENV.to_string(),
                "forged-scope".to_string(),
            ),
            ("INVALID=NAME".to_string(), "ignored".to_string()),
        ]);
        let result = run_sandboxed_with_environment(
            "printf '%s\\n%s\\n%s\\n%s' \"${NIB_TEST_INHERITED_SECRET-}\" \"${NIB_CONFIGURED_VALUE-}\" \"${INVALID-}\" \"${NIB_MANAGED_PROCESS_SCOPE-}\"",
            directory.path(),
            "internal",
            "internal",
            &BoundaryConfig::default(),
            &environment,
        )
        .await;

        let (output, arguments) = result.expect("direct execution");
        assert!(output.status.success());
        assert!(arguments.is_none());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "\nexplicit\n\nscope-fixture"
        );
    }

    #[test]
    #[serial]
    fn std_child_environment_preserves_authoritative_managed_scope() {
        let _managed_scope =
            EnvironmentVariableGuard::set(MANAGED_PROCESS_SCOPE_ENV, OsStr::new("scope-fixture"));
        let configured = HashMap::from([(
            MANAGED_PROCESS_SCOPE_ENV.to_string(),
            "forged-scope".to_string(),
        )]);
        let mut command = std::process::Command::new("git");

        apply_std_child_environment(&mut command, &configured);

        let inherited_scope = command
            .get_envs()
            .find_map(|(key, value)| {
                (key == OsStr::new(MANAGED_PROCESS_SCOPE_ENV)).then_some(value)
            })
            .flatten();
        assert_eq!(inherited_scope, Some(OsStr::new("scope-fixture")));
    }

    #[tokio::test]
    async fn unusable_bwrap_falls_back_to_direct_execution() {
        let capabilities = detect_capabilities();
        if capabilities.bwrap_available {
            return;
        }

        let directory = tempdir().expect("tempdir");
        let (output, args) = run_sandboxed(
            "printf fallback > fallback.txt",
            directory.path(),
            "restricted",
            &BoundaryConfig::default(),
        )
        .await
        .expect("fallback");
        assert!(output.status.success());
        assert!(args.is_none());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("fallback.txt")).expect("output"),
            "fallback"
        );
    }

    #[test]
    fn disabled_network_boundary_rejects_unisolated_hybrid_fallback() {
        let capabilities = SandboxCapabilities {
            bwrap_installed: true,
            bwrap_available: false,
            bwrap_error: Some("fixture unavailable".to_string()),
            managed_process_available: false,
            managed_process_error: Some("fixture unavailable".to_string()),
            git_available: true,
        };
        let boundaries = BoundaryConfig {
            network: "disabled".to_string(),
            ..BoundaryConfig::default()
        };

        let error = ensure_hybrid_fallback_allowed(&boundaries, &capabilities)
            .expect_err("disabled network must fail closed");

        assert!(error.contains("cannot enforce disabled network boundary"));
        assert!(ensure_hybrid_fallback_allowed(&BoundaryConfig::default(), &capabilities).is_ok());
    }

    #[tokio::test]
    async fn disabled_network_boundary_rejects_internal_direct_execution_before_spawn() {
        let directory = tempdir().expect("tempdir");
        let boundaries = BoundaryConfig {
            network: "disabled".to_string(),
            ..BoundaryConfig::default()
        };

        let error = run_sandboxed_with_provider(
            "touch must-not-exist",
            directory.path(),
            "internal",
            "internal",
            &boundaries,
        )
        .await
        .expect_err("disabled network must reject direct execution");

        assert!(error.contains("disabled network boundary"));
        assert!(!directory.path().join("must-not-exist").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_direct_execution_terminates_descendant_processes() {
        let directory = tempdir().expect("tempdir");
        let cwd = directory.path().to_path_buf();
        let ids_path = cwd.join("process-ids");
        let task = tokio::spawn(async move {
            run_sandboxed_with_provider(
                "sleep 60 & child=$!; printf '%s %s' \"$$\" \"$child\" > process-ids; wait",
                &cwd,
                "internal",
                "internal",
                &BoundaryConfig::default(),
            )
            .await
        });
        let ids = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(ids) = std::fs::read_to_string(&ids_path) {
                    break ids;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child process ids");
        let mut ids = ids.split_whitespace().map(|value| {
            value
                .parse::<i32>()
                .expect("fixture process id must be numeric")
        });
        let process_group = ids.next().expect("process group leader id");
        let descendant = ids.next().expect("descendant process id");

        task.abort();
        assert!(task.await.expect_err("sandbox task aborted").is_cancelled());
        let terminated = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !process_is_active(descendant) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        if !terminated {
            signal_process_group(process_group, 9);
        }
        assert!(
            terminated,
            "descendant process survived sandbox cancellation"
        );
    }

    #[cfg(unix)]
    fn process_is_active(pid: i32) -> bool {
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            return stat
                .rsplit_once(") ")
                .and_then(|(_, fields)| fields.chars().next())
                .is_some_and(|state| state != 'Z');
        }

        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[tokio::test]
    async fn explicit_bwrap_provider_fails_when_bwrap_is_unusable() {
        let capabilities = detect_capabilities();
        if capabilities.bwrap_available {
            return;
        }
        let directory = tempdir().expect("tempdir");
        let error = run_sandboxed_with_provider(
            "true",
            directory.path(),
            "bwrap",
            "restricted",
            &BoundaryConfig::default(),
        )
        .await
        .expect_err("strict bwrap provider");
        assert!(error.contains("bwrap"));
    }

    #[test]
    fn bwrap_arguments_bind_explicit_writable_paths() {
        let cwd = tempdir().expect("cwd");
        let allowed = tempdir().expect("allowed");
        let boundaries = BoundaryConfig {
            allow_write: vec![allowed.path().to_string_lossy().to_string()],
            ..BoundaryConfig::default()
        };
        let arguments = build_bwrap_args("true", cwd.path(), &boundaries, "restricted")
            .expect("bwrap arguments");
        let allowed = allowed.path().canonicalize().expect("canonical allowed");
        let allowed = allowed.to_string_lossy();
        assert!(arguments.windows(3).any(|window| {
            window[0] == "--bind" && window[1] == allowed && window[2] == allowed
        }));
    }

    #[test]
    fn bwrap_masks_common_host_credential_locations() {
        let home = tempdir().expect("home");
        let cwd = tempdir().expect("cwd");
        std::fs::create_dir(home.path().join(".ssh")).expect("ssh directory");
        std::fs::write(home.path().join(".netrc"), "machine example.invalid")
            .expect("netrc fixture");
        let ssh = home.path().join(".ssh").canonicalize().unwrap();
        let netrc = home.path().join(".netrc").canonicalize().unwrap();
        let mut arguments = Vec::new();

        append_credential_masks_for_home(&mut arguments, home.path(), cwd.path());

        assert!(arguments
            .windows(2)
            .any(|window| window[0] == "--tmpfs" && window[1] == ssh.to_string_lossy()));
        assert!(arguments.windows(3).any(|window| {
            window[0] == "--ro-bind"
                && window[1] == "/dev/null"
                && window[2] == netrc.to_string_lossy()
        }));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn bwrap_hides_home_but_preserves_toolchains_and_worktree() {
        if !detect_capabilities().bwrap_available {
            return;
        }

        let root = tempdir().expect("sandbox root");
        let home = root.path().join("home");
        let cargo_home = home.join(".cargo");
        let rustup_home = home.join(".rustup");
        let cwd = home.join("project");
        for directory in [&cargo_home, &rustup_home, &cwd] {
            std::fs::create_dir_all(directory).expect("fixture directory");
        }
        std::fs::write(home.join("private.txt"), "host-secret").expect("private fixture");
        std::fs::write(cargo_home.join("toolchain.txt"), "cargo").expect("Cargo fixture");
        std::fs::write(cargo_home.join("credentials.toml"), "cargo-secret")
            .expect("Cargo credential fixture");
        std::fs::write(rustup_home.join("toolchain.txt"), "rustup").expect("Rustup fixture");
        std::fs::write(cwd.join("worktree.txt"), "cwd").expect("worktree fixture");

        let _home = EnvironmentVariableGuard::set("HOME", home.as_os_str());
        let _cargo = EnvironmentVariableGuard::set("CARGO_HOME", cargo_home.as_os_str());
        let _rustup = EnvironmentVariableGuard::set("RUSTUP_HOME", rustup_home.as_os_str());
        let command = r#"
            test ! -e "$HOME/private.txt" &&
            printf ephemeral > "$HOME/private.txt" &&
            test "$(cat "$CARGO_HOME/toolchain.txt")" = cargo &&
            test ! -s "$CARGO_HOME/credentials.toml" &&
            test "$(cat "$RUSTUP_HOME/toolchain.txt")" = rustup &&
            test "$(cat worktree.txt)" = cwd &&
            printf writable > worktree-output.txt
        "#;

        let (output, arguments) = run_sandboxed_with_provider(
            command,
            &cwd,
            "bwrap",
            "restricted",
            &BoundaryConfig::default(),
        )
        .await
        .expect("strict bwrap execution");

        assert!(
            output.status.success(),
            "bwrap stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(arguments.is_some());
        assert_eq!(
            std::fs::read_to_string(home.join("private.txt")).expect("host secret remains"),
            "host-secret"
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("worktree-output.txt"))
                .expect("worktree remains writable"),
            "writable"
        );

        drop(_rustup);
        drop(_cargo);
        drop(_home);
        let (toolchain_output, _) = run_sandboxed_with_provider(
            "cargo --version && rustc --version",
            &cwd,
            "bwrap",
            "restricted",
            &BoundaryConfig::default(),
        )
        .await
        .expect("toolchain execution in strict bwrap");
        assert!(
            toolchain_output.status.success(),
            "toolchain stderr: {}",
            String::from_utf8_lossy(&toolchain_output.stderr)
        );
    }

    #[test]
    fn network_boundary_controls_namespace_independently_of_profile_name() {
        let cwd = tempdir().expect("cwd");
        for network in ["restricted", "disabled"] {
            let boundaries = BoundaryConfig {
                network: network.to_string(),
                ..BoundaryConfig::default()
            };
            assert!(
                build_bwrap_args("true", cwd.path(), &boundaries, "restricted")
                    .unwrap()
                    .contains(&"--unshare-net".to_string())
            );
        }
        let enabled = BoundaryConfig {
            network: "enabled".to_string(),
            ..BoundaryConfig::default()
        };
        assert!(
            !build_bwrap_args("true", cwd.path(), &enabled, "restricted")
                .unwrap()
                .contains(&"--unshare-net".to_string())
        );
    }

    #[test]
    fn missing_explicit_writable_path_fails_closed() {
        let cwd = tempdir().expect("cwd");
        let boundaries = BoundaryConfig {
            allow_write: vec![cwd.path().join("missing").to_string_lossy().to_string()],
            ..BoundaryConfig::default()
        };
        let error = build_bwrap_args("true", cwd.path(), &boundaries, "restricted")
            .expect_err("missing write boundary");
        assert!(error.contains("configured writable path"));
    }
}
