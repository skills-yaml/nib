use clap::{Args, Subcommand};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

const SKILL_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const COMMAND_OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(windows)]
const WINDOWS_JOB_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT_EVENTS_PER_POLL: usize = 8;
const STAGING_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_INSTALLED_RESOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_INSTALLED_TOTAL_BYTES: u64 = 9 * 1024 * 1024;
const MAX_INSTALLED_ENTRIES: usize = 97;
const MAX_GIT_STAGING_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_GIT_STAGING_BYTES: u64 = 32 * 1024 * 1024;
const MAX_GIT_STAGING_ENTRIES: usize = 4_096;
const MAX_GIT_STAGING_DEPTH: usize = 32;

#[derive(Clone, Copy)]
struct StagingLimits {
    max_file_bytes: u64,
    max_bytes: u64,
    max_entries: usize,
    max_depth: usize,
}

const GIT_STAGING_LIMITS: StagingLimits = StagingLimits {
    max_file_bytes: MAX_GIT_STAGING_FILE_BYTES,
    max_bytes: MAX_GIT_STAGING_BYTES,
    max_entries: MAX_GIT_STAGING_ENTRIES,
    max_depth: MAX_GIT_STAGING_DEPTH,
};

#[derive(Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommands,
}

#[derive(Subcommand)]
pub enum SkillCommands {
    /// List installed skills
    List,
    /// Install a skill from a local path, HTTP URL, or Git repository
    Install {
        /// Local SKILL.md/directory, raw HTTP URL, or Git repository URL
        source: String,
    },
    /// Remove a globally installed skill
    Remove {
        /// Name of the skill to remove
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledSkill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub location: &'static str,
}

pub fn run_skill_cmd(args: &SkillArgs, project_root: &Path) -> Result<(), String> {
    match &args.command {
        SkillCommands::List => list_skills(project_root),
        SkillCommands::Install { source } => {
            let target = install_skill(source)?;
            println!("Installed skill at {}", target.display());
            Ok(())
        }
        SkillCommands::Remove { name } => {
            remove_skill(name)?;
            println!("Removed skill '{name}'.");
            Ok(())
        }
    }
}

pub fn list_skills(project_root: &Path) -> Result<(), String> {
    let skills = installed_skills(project_root)?;
    println!("Installed Skills:");
    if skills.is_empty() {
        println!("  No skills found.");
    }
    for skill in skills {
        println!(
            "  [{}] {} - {}",
            skill.location, skill.name, skill.description
        );
    }
    Ok(())
}

fn global_skills_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("NIB_SKILLS_DIR") {
        return Ok(PathBuf::from(path));
    }
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".config/nib/skills"))
        .map_err(|_| "could not determine the global skills directory".to_string())
}

pub fn installed_skills(project_root: &Path) -> Result<Vec<InstalledSkill>, String> {
    let global = global_skills_dir()?;
    let roots = vec![
        global.clone(),
        project_root.join(".nib").join("skills"),
        project_root.join(".skills"),
        project_root.join("skills"),
    ];
    let paths = nib::context::skills::find_skills_in_paths_strict(&roots)
        .map_err(|error| format!("failed to discover installed skills: {error}"))?;
    let mut installed = Vec::with_capacity(paths.len());
    for path in paths {
        let skill = nib::context::skills::parse_skill_file(&path).map_err(|error| {
            format!("failed to load installed skill {}: {error}", path.display())
        })?;
        installed.push(InstalledSkill {
            name: skill.frontmatter.name,
            description: skill.frontmatter.description,
            location: if path.starts_with(&global) {
                "global"
            } else {
                "local"
            },
            path,
        });
    }
    installed.sort_by(|left, right| {
        left.location
            .cmp(right.location)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(installed)
}

fn safe_skill_name(name: &str) -> Result<String, String> {
    nib::context::skills::canonical_skill_id(name).map_err(|error| error.to_string())
}

enum PreparedSource {
    Directory(PathBuf),
    File(PathBuf),
}

fn prepare_source(source: &str, staging: &Path) -> Result<PreparedSource, String> {
    let local = PathBuf::from(source);
    if local.exists() {
        return if local.is_file() {
            Ok(PreparedSource::File(local))
        } else {
            Ok(PreparedSource::Directory(local))
        };
    }

    let is_http = source.starts_with("http://") || source.starts_with("https://");
    let is_raw_manifest = source.ends_with(".md")
        || source.contains("/raw/")
        || source.contains("raw.githubusercontent");
    if is_http && is_raw_manifest {
        let output = Command::new("curl")
            .args([
                "-fsSL",
                "--proto",
                "=http,https",
                "--max-time",
                "30",
                "--max-filesize",
                "1048576",
                source,
            ])
            .output()
            .map_err(|error| format!("failed to start curl: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "failed to download skill: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let path = staging.join("SKILL.md");
        fs::write(&path, output.stdout)
            .map_err(|error| format!("failed to stage SKILL.md: {error}"))?;
        return Ok(PreparedSource::File(path));
    }

    if is_http
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.starts_with("file://")
    {
        return prepare_git_source(source, staging);
    }

    Err(format!("skill source does not exist: {source}"))
}

fn prepare_git_source(source: &str, staging: &Path) -> Result<PreparedSource, String> {
    prepare_git_source_with_limits(source, staging, GIT_STAGING_LIMITS)
}

fn prepare_git_source_with_limits(
    source: &str,
    staging: &Path,
    limits: StagingLimits,
) -> Result<PreparedSource, String> {
    let checkout = staging.join("checkout");
    let preparation = (|| {
        let mut clone = git_command();
        clone
            .args([
                "clone",
                "--depth",
                "1",
                "--filter=blob:none",
                "--no-checkout",
                "--no-local",
                source,
            ])
            .arg(&checkout);
        run_git_command(&mut clone, "git clone", &checkout, limits)?;

        let mut manifest_checkout = git_command();
        manifest_checkout.current_dir(&checkout).args([
            "checkout",
            "HEAD",
            "--",
            ":(literal)SKILL.md",
        ]);
        run_git_command(
            &mut manifest_checkout,
            "git checkout SKILL.md",
            &checkout,
            limits,
        )?;

        let manifest = checkout.join("SKILL.md");
        let frontmatter = nib::context::skills::parse_skill_frontmatter_file(&manifest)
            .map_err(|error| format!("invalid SKILL.md: {error}"))?;
        let mut resources = BTreeSet::new();
        for configured in frontmatter
            .references
            .iter()
            .chain(frontmatter.assets.iter())
        {
            let relative = nib::context::skills::validated_skill_resource_path(configured)
                .map_err(|error| format!("invalid SKILL.md: {error}"))?;
            resources.insert(relative);
        }
        if !resources.is_empty() {
            let mut resource_checkout = git_command();
            resource_checkout
                .current_dir(&checkout)
                .args(["checkout", "HEAD", "--"]);
            for relative in resources {
                resource_checkout.arg(format!(":(literal){}", relative.to_string_lossy()));
            }
            run_git_command(
                &mut resource_checkout,
                "git checkout declared skill resources",
                &checkout,
                limits,
            )?;
        }
        validate_staging_tree(&checkout, limits)?;
        Ok(PreparedSource::Directory(checkout.clone()))
    })();
    if preparation.is_err() {
        let _ = fs::remove_dir_all(&checkout);
    }
    preparation
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("SSH_ASKPASS", "true");
    command
}

fn run_git_command(
    command: &mut Command,
    action: &str,
    checkout: &Path,
    limits: StagingLimits,
) -> Result<(), String> {
    let output = run_bounded_command_with_staging(
        command,
        action,
        SKILL_COMMAND_TIMEOUT,
        Some((checkout, limits)),
    )?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{action} failed under enforced staging limits: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[derive(Debug)]
struct BoundedCommandOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

struct CommandProcessOwnership {
    #[cfg(unix)]
    process_group: Option<i32>,
    #[cfg(windows)]
    windows_job: Option<nib::sandbox::windows_job::WindowsJob>,
}

impl CommandProcessOwnership {
    #[cfg(not(windows))]
    fn from_spawned_child(
        child: &std::process::Child,
        created_process_group: bool,
    ) -> Result<Self, String> {
        #[cfg(unix)]
        {
            let process_group =
                if created_process_group {
                    Some(i32::try_from(child.id()).map_err(|_| {
                        "skill command process identifier exceeds pid_t".to_string()
                    })?)
                } else {
                    None
                };
            Ok(Self { process_group })
        }
        #[cfg(not(unix))]
        {
            let _ = (child, created_process_group);
            Ok(Self {})
        }
    }

    #[cfg(windows)]
    fn from_windows_job(windows_job: nib::sandbox::windows_job::WindowsJob) -> Self {
        Self {
            windows_job: Some(windows_job),
        }
    }

    fn poll_exit(
        &mut self,
        child: &mut std::process::Child,
    ) -> std::io::Result<Option<ExitStatus>> {
        #[cfg(unix)]
        if self.process_group.is_some() {
            match unix_child_has_exited_without_reaping(child.id()) {
                Ok(false) => return Ok(None),
                Ok(true) => {
                    self.signal_owned_process_group();
                    return child.wait().map(Some);
                }
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    // The child was already reaped, so its numeric PGID is no
                    // longer an owned signalling target.
                    self.process_group.take();
                }
                Err(error) => return Err(error),
            }
        }
        child.try_wait()
    }

    fn terminate_and_reap(&mut self, child: &mut std::process::Child) -> Result<(), String> {
        #[cfg(unix)]
        {
            self.signal_owned_process_group();
            let _ = child.kill();
            let _ = child.wait();
            Ok(())
        }
        #[cfg(windows)]
        {
            let job_cleanup = self.terminate_windows_job();
            let _ = child.kill();
            let direct_wait = child
                .wait()
                .map(|_| ())
                .map_err(|error| format!("failed to reap direct skill command child: {error}"));
            combine_cleanup_results(job_cleanup, direct_wait)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child.kill();
            let _ = child.wait();
            Ok(())
        }
    }

    fn cleanup_after_exit(&mut self) -> Result<(), String> {
        #[cfg(windows)]
        {
            return self.terminate_windows_job();
        }
        #[cfg(not(windows))]
        {
            Ok(())
        }
    }

    #[cfg(unix)]
    fn signal_owned_process_group(&mut self) {
        if let Some(process_group) = self.process_group.take() {
            // SAFETY: this authority exists only when this launcher requested
            // process_group(0), and it is consumed before the leader is reaped.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }

    #[cfg(windows)]
    fn terminate_windows_job(&mut self) -> Result<(), String> {
        let cleanup = self
            .windows_job
            .as_mut()
            .ok_or_else(|| "bounded skill command lost its Windows Job Object".to_string())?
            .terminate_and_wait(WINDOWS_JOB_CLEANUP_TIMEOUT)
            .map_err(|error| format!("Windows Job Object cleanup failed: {error}"));
        if cleanup.is_ok() {
            self.windows_job.take();
        }
        cleanup
    }
}

#[cfg(any(windows, test))]
fn combine_cleanup_results(
    first: Result<(), String>,
    second: Result<(), String>,
) -> Result<(), String> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(format!("{first}; {second}")),
    }
}

fn error_after_cleanup(primary: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => format!("{primary}; process cleanup failed: {cleanup}"),
    }
}

#[cfg(test)]
fn run_bounded_command(
    command: &mut Command,
    action: &str,
    timeout: Duration,
) -> Result<BoundedCommandOutput, String> {
    run_bounded_command_with_staging(command, action, timeout, None)
}

fn run_bounded_command_with_staging(
    command: &mut Command,
    action: &str,
    timeout: Duration,
    staging: Option<(&Path, StagingLimits)>,
) -> Result<BoundedCommandOutput, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    let creates_process_group = should_create_unix_process_group(
        cfg!(target_os = "macos"),
        std::env::var_os("NIB_MANAGED_PROCESS_SCOPE").is_some(),
    );
    #[cfg(not(any(unix, windows)))]
    let creates_process_group = false;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if creates_process_group {
            command.process_group(0);
        }
        if let Some((_, limits)) = staging {
            let file_limit: libc::rlim_t = limits.max_file_bytes;
            // SAFETY: the closure only applies a process-local resource limit before exec.
            unsafe {
                command.pre_exec(move || {
                    let limit = libc::rlimit {
                        rlim_cur: file_limit,
                        rlim_max: file_limit,
                    };
                    if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
        }
    }
    #[cfg(windows)]
    let (mut child, windows_job) = nib::sandbox::windows_job::spawn_contained_std(command)
        .map_err(|error| format!("failed to start {action}: {error}"))?;
    #[cfg(not(windows))]
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start {action}: {error}"))?;
    #[cfg(windows)]
    let mut ownership = CommandProcessOwnership::from_windows_job(windows_job);
    #[cfg(not(windows))]
    let mut ownership =
        match CommandProcessOwnership::from_spawned_child(&child, creates_process_group) {
            Ok(ownership) => ownership,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let error = format!("failed to capture {action} stderr");
            return Err(error_after_cleanup(
                error,
                ownership.terminate_and_reap(&mut child),
            ));
        }
    };
    let stderr_receiver = match spawn_output_reader(stderr, action) {
        Ok(receiver) => receiver,
        Err(error) => {
            return Err(error_after_cleanup(
                error,
                ownership.terminate_and_reap(&mut child),
            ));
        }
    };
    let started = Instant::now();
    let mut next_staging_check = started;
    let mut retained_stderr = Vec::new();
    let mut stderr_truncated = false;
    let status = loop {
        drain_output(
            &stderr_receiver,
            &mut retained_stderr,
            &mut stderr_truncated,
        );
        if let Some((root, limits)) = staging {
            if Instant::now() >= next_staging_check {
                if let Err(error) = validate_staging_tree(root, limits) {
                    let cleanup = ownership.terminate_and_reap(&mut child);
                    finish_output_capture(
                        &stderr_receiver,
                        &mut retained_stderr,
                        &mut stderr_truncated,
                    );
                    return Err(error_after_cleanup(
                        format!("{action} exceeded staging limits: {error}"),
                        cleanup,
                    ));
                }
                next_staging_check = Instant::now() + STAGING_POLL_INTERVAL;
            }
        }
        if started.elapsed() >= timeout {
            let cleanup = ownership.terminate_and_reap(&mut child);
            finish_output_capture(
                &stderr_receiver,
                &mut retained_stderr,
                &mut stderr_truncated,
            );
            return Err(error_after_cleanup(
                format!("{action} timed out after {}s", timeout.as_secs_f64()),
                cleanup,
            ));
        }
        match ownership.poll_exit(&mut child) {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(COMMAND_POLL_INTERVAL),
            Err(error) => {
                let cleanup = ownership.terminate_and_reap(&mut child);
                finish_output_capture(
                    &stderr_receiver,
                    &mut retained_stderr,
                    &mut stderr_truncated,
                );
                return Err(error_after_cleanup(
                    format!("failed while waiting for {action}: {error}"),
                    cleanup,
                ));
            }
        }
    };
    if let Err(error) = ownership.cleanup_after_exit() {
        finish_output_capture(
            &stderr_receiver,
            &mut retained_stderr,
            &mut stderr_truncated,
        );
        return Err(format!("failed to clean up {action}: {error}"));
    }
    if let Some((root, limits)) = staging {
        validate_staging_tree(root, limits)
            .map_err(|error| format!("{action} exceeded staging limits: {error}"))?;
    }
    finish_output_capture(
        &stderr_receiver,
        &mut retained_stderr,
        &mut stderr_truncated,
    );
    if staging.is_some() && exited_for_file_size_limit(&status) {
        return Err(format!(
            "{action} exceeded staging limits: child exceeded the per-file limit"
        ));
    }
    Ok(BoundedCommandOutput {
        status,
        stderr: retained_stderr,
    })
}

#[cfg(unix)]
fn exited_for_file_size_limit(status: &ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;

    status.signal() == Some(libc::SIGXFSZ) || status.code() == Some(128 + libc::SIGXFSZ)
}

#[cfg(not(unix))]
fn exited_for_file_size_limit(_status: &ExitStatus) -> bool {
    false
}

enum OutputEvent {
    Chunk(Vec<u8>),
    Closed,
}

fn spawn_output_reader(
    mut reader: impl Read + Send + 'static,
    action: &str,
) -> Result<Receiver<OutputEvent>, String> {
    let (sender, receiver) = mpsc::sync_channel(8);
    std::thread::Builder::new()
        .name(format!("nib-{action}-stderr"))
        .spawn(move || {
            let mut chunk = [0_u8; 8 * 1024];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if sender
                            .send(OutputEvent::Chunk(chunk[..read].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            let _ = sender.send(OutputEvent::Closed);
        })
        .map_err(|error| format!("failed to start {action} output reader: {error}"))?;
    Ok(receiver)
}

fn drain_output(
    receiver: &Receiver<OutputEvent>,
    retained: &mut Vec<u8>,
    truncated: &mut bool,
) -> bool {
    for _ in 0..MAX_OUTPUT_EVENTS_PER_POLL {
        match receiver.try_recv() {
            Ok(OutputEvent::Chunk(chunk)) => retain_output_chunk(retained, truncated, &chunk),
            Ok(OutputEvent::Closed) | Err(TryRecvError::Disconnected) => return true,
            Err(TryRecvError::Empty) => return false,
        }
    }
    false
}

fn finish_output_capture(
    receiver: &Receiver<OutputEvent>,
    retained: &mut Vec<u8>,
    truncated: &mut bool,
) {
    let deadline = Instant::now() + COMMAND_OUTPUT_DRAIN_TIMEOUT;
    while !drain_output(receiver, retained, truncated) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match receiver.recv_timeout((deadline - now).min(COMMAND_POLL_INTERVAL)) {
            Ok(OutputEvent::Chunk(chunk)) => retain_output_chunk(retained, truncated, &chunk),
            Ok(OutputEvent::Closed) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    if *truncated {
        const MARKER: &[u8] = b"\n...[command output bounded]...";
        retained.truncate(MAX_COMMAND_OUTPUT_BYTES.saturating_sub(MARKER.len()));
        retained.extend_from_slice(MARKER);
    }
}

fn retain_output_chunk(retained: &mut Vec<u8>, truncated: &mut bool, chunk: &[u8]) {
    let available = MAX_COMMAND_OUTPUT_BYTES.saturating_sub(retained.len());
    retained.extend_from_slice(&chunk[..chunk.len().min(available)]);
    *truncated |= chunk.len() > available;
}

#[cfg(unix)]
fn should_create_unix_process_group(is_macos: bool, managed_scope_present: bool) -> bool {
    !(is_macos && managed_scope_present)
}

#[cfg(unix)]
fn unix_child_has_exited_without_reaping(pid: u32) -> std::io::Result<bool> {
    let pid = i32::try_from(pid)
        .map_err(|_| std::io::Error::other("child process identifier exceeds pid_t"))?;
    loop {
        // SAFETY: a zeroed siginfo_t is a valid output buffer for waitid.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: P_PID restricts observation to the exact child. WNOWAIT pins
        // its PID until the owned process group is signalled and Child::wait
        // performs the final reap.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: waitid initialized siginfo_t, and zero means no exited
            // child was available for this WNOHANG observation.
            let observed_pid = unsafe { info.si_pid() };
            if observed_pid == 0 {
                return Ok(false);
            }
            if observed_pid == pid {
                return Ok(true);
            }
            return Err(std::io::Error::other(
                "waitid returned an unexpected child process identifier",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn validate_staging_tree(root: &Path, limits: StagingLimits) -> Result<(), String> {
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "failed to inspect Git staging directory {}: {error}",
                root.display()
            ))
        }
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(format!(
            "Git staging root must be a local directory: {}",
            root.display()
        ));
    }

    let mut entries = 0usize;
    let mut bytes = 0u64;
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let directory_entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "failed to inspect Git staging directory {}: {error}",
                    directory.display()
                ))
            }
        };
        for entry in directory_entries {
            let entry = entry.map_err(|error| {
                format!(
                    "failed to inspect Git staging directory {}: {error}",
                    directory.display()
                )
            })?;
            entries = entries.saturating_add(1);
            if entries > limits.max_entries {
                return Err(format!(
                    "Git staging exceeds the {}-entry limit",
                    limits.max_entries
                ));
            }
            let entry_depth = depth.saturating_add(1);
            if entry_depth > limits.max_depth {
                return Err(format!(
                    "Git staging exceeds the {}-component depth limit at {}",
                    limits.max_depth,
                    entry.path().display()
                ));
            }
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "failed to inspect Git staging entry {}: {error}",
                        entry.path().display()
                    ))
                }
            };
            if metadata.is_dir() {
                pending.push((entry.path(), entry_depth));
                continue;
            }
            if !metadata.is_file() && !metadata.file_type().is_symlink() {
                return Err(format!(
                    "Git staging contains a non-file entry: {}",
                    entry.path().display()
                ));
            }
            if metadata.len() > limits.max_file_bytes {
                return Err(format!(
                    "Git staging entry exceeds the {}-byte per-file limit: {}",
                    limits.max_file_bytes,
                    entry.path().display()
                ));
            }
            bytes = bytes
                .checked_add(metadata.len())
                .ok_or_else(|| "Git staging byte count overflowed".to_string())?;
            if bytes > limits.max_bytes {
                return Err(format!(
                    "Git staging exceeds the {}-byte aggregate limit",
                    limits.max_bytes
                ));
            }
        }
    }
    Ok(())
}

fn source_manifest(source: &PreparedSource) -> PathBuf {
    match source {
        PreparedSource::Directory(path) => path.join("SKILL.md"),
        PreparedSource::File(path) => path.clone(),
    }
}

pub fn install_skill(source: &str) -> Result<PathBuf, String> {
    install_skill_to(source, &global_skills_dir()?)
}

pub fn install_skill_to(source: &str, global_dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(global_dir)
        .map_err(|error| format!("failed to create global skills directory: {error}"))?;
    let staging = tempfile::tempdir().map_err(|error| format!("failed to stage skill: {error}"))?;
    let prepared = prepare_source(source, staging.path())?;
    let manifest = source_manifest(&prepared);
    if manifest.file_name().and_then(|name| name.to_str()) != Some("SKILL.md") {
        return Err("a skill file must be named SKILL.md".to_string());
    }
    let skill = nib::context::skills::parse_skill_file(&manifest)
        .map_err(|error| format!("invalid SKILL.md: {error}"))?;
    let target = global_dir.join(safe_skill_name(&skill.frontmatter.name)?);
    if target.exists() {
        return Err(format!(
            "skill '{}' is already installed at {}",
            skill.frontmatter.name,
            target.display()
        ));
    }

    let temporary = global_dir.join(format!(".nib-skill-{}.tmp", uuid::Uuid::new_v4().simple()));
    let installation = (|| -> Result<(), String> {
        copy_selected_skill(&manifest, &skill, &temporary)?;
        nib::context::skills::parse_skill_file(&temporary.join("SKILL.md"))
            .map_err(|error| format!("staged skill is invalid: {error}"))?;
        fs::rename(&temporary, &target)
            .map_err(|error| format!("failed to publish skill atomically: {error}"))
    })();
    if let Err(error) = installation {
        return match fs::remove_dir_all(&temporary) {
            Ok(()) => Err(error),
            Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => {
                Err(error)
            }
            Err(cleanup_error) => Err(format!(
                "{error}; failed to clean unpublished skill staging directory {}: {cleanup_error}",
                temporary.display()
            )),
        };
    }
    Ok(target)
}

pub fn remove_skill(name: &str) -> Result<(), String> {
    remove_skill_from(name, &global_skills_dir()?)
}

pub fn remove_skill_from(name: &str, global_dir: &Path) -> Result<(), String> {
    let target = global_dir.join(safe_skill_name(name)?);
    if !target.exists() {
        return Err(format!("skill '{name}' is not installed globally"));
    }
    let metadata = fs::symlink_metadata(&target)
        .map_err(|error| format!("failed to inspect skill '{name}': {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "skill '{name}' is not a local directory and will not be removed"
        ));
    }
    fs::remove_dir_all(&target).map_err(|error| format!("failed to remove skill: {error}"))
}

fn copy_selected_skill(
    manifest: &Path,
    skill: &nib::context::skills::Skill,
    destination: &Path,
) -> Result<(), String> {
    let declared_root = manifest
        .parent()
        .ok_or_else(|| "skill manifest has no parent directory".to_string())?;
    let root = verify_existing_directory_without_symlinks(declared_root, "skill source")?;
    let mut resources = BTreeSet::new();
    resources.insert(PathBuf::from("SKILL.md"));
    resources.extend(
        skill
            .references
            .iter()
            .map(|reference| reference.path.clone()),
    );
    resources.extend(skill.assets.iter().cloned());
    if resources.len() > MAX_INSTALLED_ENTRIES {
        return Err(format!(
            "skill installation exceeds the {MAX_INSTALLED_ENTRIES}-entry limit"
        ));
    }
    let mut total_bytes = 0_u64;
    for relative in resources {
        let relative = nib::context::skills::validated_skill_resource_path(
            relative
                .to_str()
                .ok_or_else(|| "skill resource path is not UTF-8".to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let declared_source = root.join(&relative);
        let source_parent = declared_source
            .parent()
            .ok_or_else(|| "skill resource has no parent directory".to_string())?;
        verify_existing_directory_without_symlinks(source_parent, "skill resource")?;
        let declared_metadata = fs::symlink_metadata(&declared_source)
            .map_err(|error| format!("failed to inspect skill resource: {error}"))?;
        if declared_metadata.file_type().is_symlink() || !declared_metadata.is_file() {
            return Err(format!(
                "skill resource is not a regular local file: {}",
                relative.display()
            ));
        }
        let source = declared_source
            .canonicalize()
            .map_err(|error| format!("failed to resolve skill resource: {error}"))?;
        if !source.starts_with(&root) {
            return Err(format!(
                "skill resource escapes its source root: {}",
                relative.display()
            ));
        }
        let metadata = fs::symlink_metadata(&source)
            .map_err(|error| format!("failed to inspect skill resource: {error}"))?;
        let declared_identity = nib::fs_security::FileIdentity::from_file(
            open_resource_without_following_links(&declared_source)
                .map_err(|error| format!("failed to open skill resource: {error}"))?,
        )
        .map_err(|error| format!("failed to identify skill resource: {error}"))?;
        let source_identity = nib::fs_security::FileIdentity::from_file(
            open_resource_without_following_links(&source)
                .map_err(|error| format!("failed to open skill resource: {error}"))?,
        )
        .map_err(|error| format!("failed to identify skill resource: {error}"))?;
        let same_source = declared_identity == source_identity;
        if metadata.file_type().is_symlink() || !metadata.is_file() || !same_source {
            return Err(format!(
                "skill resource is not a regular local file: {}",
                relative.display()
            ));
        }
        let copied = copy_bounded_resource(&source, &destination.join(&relative))?;
        total_bytes = total_bytes
            .checked_add(copied)
            .ok_or_else(|| "skill installation byte count overflowed".to_string())?;
        if total_bytes > MAX_INSTALLED_TOTAL_BYTES {
            return Err(format!(
                "skill installation exceeds the {MAX_INSTALLED_TOTAL_BYTES}-byte aggregate limit"
            ));
        }
    }
    Ok(())
}

fn verify_existing_directory_without_symlinks(
    path: &Path,
    description: &str,
) -> Result<PathBuf, String> {
    nib::fs_security::canonicalize_existing_directory_without_symlinks(path).map_err(|error| {
        format!(
            "failed to validate {description} {}: {error}",
            path.display()
        )
    })
}

fn copy_bounded_resource(source: &Path, target: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("failed to inspect skill resource: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "skill resource is not a regular local file: {}",
            source.display()
        ));
    }
    if metadata.len() > MAX_INSTALLED_RESOURCE_BYTES {
        return Err(format!(
            "skill resource exceeds the {MAX_INSTALLED_RESOURCE_BYTES}-byte install limit: {}",
            source.display()
        ));
    }
    let input = open_resource_without_following_links(source)
        .map_err(|error| format!("failed to open skill resource: {error}"))?;
    let opened = input
        .metadata()
        .map_err(|error| format!("failed to inspect open skill resource: {error}"))?;
    if !opened.is_file()
        || opened.len() > MAX_INSTALLED_RESOURCE_BYTES
        || !unix_metadata_identity_matches(&metadata, &opened)
    {
        return Err(format!(
            "skill resource changed or exceeds the install limit: {}",
            source.display()
        ));
    }
    let opened_identity = nib::fs_security::FileIdentity::from_file(
        input
            .try_clone()
            .map_err(|error| format!("failed to clone skill resource handle: {error}"))?,
    )
    .map_err(|error| format!("failed to identify open skill resource: {error}"))?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create skill resource directory: {error}"))?;
    }
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|error| format!("failed to create staged skill resource: {error}"))?;
    let copied = std::io::copy(
        &mut input.take(MAX_INSTALLED_RESOURCE_BYTES + 1),
        &mut output,
    )
    .map_err(|error| format!("failed to copy skill resource: {error}"))?;
    if copied > MAX_INSTALLED_RESOURCE_BYTES {
        let _ = fs::remove_file(target);
        return Err(format!(
            "skill resource exceeds the {MAX_INSTALLED_RESOURCE_BYTES}-byte install limit: {}",
            source.display()
        ));
    }
    output
        .sync_all()
        .map_err(|error| format!("failed to sync staged skill resource: {error}"))?;
    let post_probe = open_resource_without_following_links(source)
        .map_err(|error| format!("failed to re-open skill resource: {error}"))?;
    let post_opened = post_probe
        .metadata()
        .map_err(|error| format!("failed to inspect re-opened skill resource: {error}"))?;
    let post_identity = nib::fs_security::FileIdentity::from_file(post_probe)
        .map_err(|error| format!("failed to identify re-opened skill resource: {error}"))?;
    let post = fs::symlink_metadata(source)
        .map_err(|error| format!("failed to re-inspect skill resource: {error}"))?;
    if post.file_type().is_symlink()
        || !post.is_file()
        || post.len() != copied
        || !post_opened.is_file()
        || post_opened.len() != copied
        || opened_identity != post_identity
        || !unix_metadata_identity_matches(&opened, &post)
    {
        let _ = fs::remove_file(target);
        return Err(format!(
            "skill resource changed while it was copied: {}",
            source.display()
        ));
    }
    Ok(copied)
}

fn open_resource_without_following_links(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(unix)]
fn unix_metadata_identity_matches(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.is_file() && right.is_file() && left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn unix_metadata_identity_matches(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.is_file() && right.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use tempfile::tempdir;

    #[cfg(windows)]
    const WINDOWS_JOB_ROLE_ENV: &str = "NIB_SKILL_WINDOWS_JOB_TEST_ROLE";
    #[cfg(windows)]
    const WINDOWS_JOB_PID_PATH_ENV: &str = "NIB_SKILL_WINDOWS_JOB_TEST_PID_PATH";
    #[cfg(windows)]
    const WINDOWS_JOB_ACK_PATH_ENV: &str = "NIB_SKILL_WINDOWS_JOB_TEST_ACK_PATH";
    #[cfg(windows)]
    const WINDOWS_JOB_FIXTURE_TEST: &str =
        "skill_cmd::tests::bounded_command_windows_job_terminates_descendant_before_return";

    fn create_skill(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("create source");
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\nBody\n"),
        )
        .expect("write skill");
        dir
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn cleanup_failures_remain_visible_with_primary_and_secondary_errors() {
        assert_eq!(
            error_after_cleanup(
                "fixture timed out".to_string(),
                Err("job did not empty".to_string())
            ),
            "fixture timed out; process cleanup failed: job did not empty"
        );
        assert_eq!(
            combine_cleanup_results(
                Err("job termination failed".to_string()),
                Err("direct reap failed".to_string())
            ),
            Err("job termination failed; direct reap failed".to_string())
        );
    }

    #[test]
    fn installs_directory_and_removes_skill() {
        let source = tempdir().expect("source tempdir");
        let global = tempdir().expect("global tempdir");
        let skill = create_skill(source.path(), "safe-rust");

        let installed = install_skill_to(skill.to_str().expect("source path"), global.path())
            .expect("install skill");
        assert!(installed.join("SKILL.md").is_file());
        assert!(install_skill_to(skill.to_str().expect("source path"), global.path()).is_err());

        remove_skill_from("safe-rust", global.path()).expect("remove skill");
        assert!(!installed.exists());
        assert!(remove_skill_from("safe-rust", global.path()).is_err());
    }

    #[test]
    fn installs_direct_manifest_and_rejects_invalid_sources() {
        let source = tempdir().expect("source tempdir");
        let global = tempdir().expect("global tempdir");
        let skill = create_skill(source.path(), "direct-skill");
        let manifest = skill.join("SKILL.md");

        let installed = install_skill_to(manifest.to_str().expect("manifest"), global.path())
            .expect("install direct manifest");
        assert!(installed.join("SKILL.md").is_file());
        assert!(install_skill_to("missing-skill", global.path()).is_err());
    }

    #[test]
    fn direct_manifest_install_preserves_declared_references_and_assets() {
        let source = tempdir().expect("source tempdir");
        let global = tempdir().expect("global tempdir");
        let skill = source.path().join("resource-skill");
        fs::create_dir_all(skill.join("references")).expect("references");
        fs::create_dir_all(skill.join("templates")).expect("templates");
        fs::write(skill.join("references/guide.md"), "guide").expect("reference");
        fs::write(skill.join("templates/report.md"), "report").expect("asset");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: resource-skill\nreferences: [references/guide.md]\nassets: [templates/report.md]\n---\nBody\n",
        )
        .expect("manifest");

        let installed = install_skill_to(
            skill.join("SKILL.md").to_str().expect("manifest path"),
            global.path(),
        )
        .expect("install direct resource skill");

        assert_eq!(
            fs::read_to_string(installed.join("references/guide.md")).unwrap(),
            "guide"
        );
        assert_eq!(
            fs::read_to_string(installed.join("templates/report.md")).unwrap(),
            "report"
        );
        nib::context::skills::parse_skill_file(&installed.join("SKILL.md"))
            .expect("installed skill remains valid");
    }

    #[test]
    fn directory_install_copies_only_bounded_declared_resources() {
        let source = tempdir().expect("source tempdir");
        let global = tempdir().expect("global tempdir");
        let skill = source.path().join("selected-resources");
        fs::create_dir_all(skill.join("assets")).expect("asset directory");
        fs::write(skill.join("assets/selected.txt"), "selected").expect("selected asset");
        fs::File::create(skill.join("undeclared-large.bin"))
            .and_then(|file| file.set_len(MAX_INSTALLED_TOTAL_BYTES * 2))
            .expect("sparse undeclared file");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: selected-resources\nassets: [assets/selected.txt]\n---\nBody\n",
        )
        .expect("manifest");

        let installed = install_skill_to(skill.to_str().expect("source path"), global.path())
            .expect("bounded install");
        assert_eq!(
            fs::read_to_string(installed.join("assets/selected.txt")).unwrap(),
            "selected"
        );
        assert!(!installed.join("undeclared-large.bin").exists());
        assert_eq!(
            fs::read_dir(&installed)
                .expect("installed directory")
                .count(),
            2
        );
    }

    #[test]
    fn missing_directory_validation_does_not_create_source_components() {
        let source = tempdir().expect("source tempdir");
        let missing = source.path().join("missing/nested");

        verify_existing_directory_without_symlinks(&missing, "skill resource")
            .expect_err("missing resource parent must fail validation");

        assert!(!source.path().join("missing").exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_symlinked_declared_resource_ancestor_without_publication() {
        use std::os::unix::fs::symlink;

        let source = tempdir().expect("source tempdir");
        let global = tempdir().expect("global tempdir");
        let skill = source.path().join("ancestor-link");
        let outside = source.path().join("outside");
        fs::create_dir_all(&skill).expect("skill directory");
        fs::create_dir_all(&outside).expect("outside directory");
        fs::write(outside.join("secret.md"), "secret").expect("outside resource");
        symlink(&outside, skill.join("references")).expect("ancestor symlink");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: ancestor-link\nreferences: [references/secret.md]\n---\nBody\n",
        )
        .expect("manifest");

        let error = install_skill_to(skill.to_str().expect("source path"), global.path())
            .expect_err("ancestor symlink must be rejected");
        assert!(error.contains("invalid SKILL.md"), "{error}");
        assert!(fs::read_dir(global.path()).unwrap().next().is_none());
    }

    #[test]
    fn oversized_declared_asset_fails_without_partial_install() {
        let source = tempdir().expect("source tempdir");
        let global = tempdir().expect("global tempdir");
        let skill = source.path().join("oversized-asset");
        fs::create_dir(&skill).expect("skill directory");
        fs::File::create(skill.join("asset.bin"))
            .and_then(|file| file.set_len(MAX_INSTALLED_RESOURCE_BYTES + 1))
            .expect("sparse oversized asset");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: oversized-asset\nassets: [asset.bin]\n---\nBody\n",
        )
        .expect("manifest");

        let error = install_skill_to(skill.to_str().expect("source path"), global.path())
            .expect_err("oversized asset must fail");
        assert!(error.contains("2097152-byte limit"));
        assert!(fs::read_dir(global.path()).unwrap().next().is_none());
    }

    #[test]
    fn installs_skill_from_bounded_http_manifest() {
        let global = tempdir().expect("global tempdir");
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let body = "---\nname: http-skill\ndescription: remote fixture\n---\nHTTP body\n";
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).expect("read request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write response");
        });

        let installed = install_skill_to(&format!("http://{address}/SKILL.md"), global.path())
            .expect("HTTP install");
        server.join().expect("server thread");
        assert!(installed.ends_with("http-skill"));
        assert!(installed.join("SKILL.md").is_file());
    }

    #[test]
    fn installs_skill_from_git_repository_url() {
        let source = tempdir().expect("source tempdir");
        let global = tempdir().expect("global tempdir");
        fs::write(
            source.path().join("SKILL.md"),
            "---\nname: git-skill\ndescription: git fixture\n---\nGit body\n",
        )
        .expect("write Git skill");
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "nib-tests@example.invalid"],
            vec!["config", "user.name", "nib tests"],
            vec!["add", "SKILL.md"],
            vec!["commit", "--quiet", "-m", "skill"],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(source.path())
                .status()
                .expect("git command");
            assert!(status.success());
        }

        let installed = install_skill_to(
            &format!("file://{}", source.path().display()),
            global.path(),
        )
        .expect("Git install");
        assert!(installed.ends_with("git-skill"));
        assert!(installed.join("SKILL.md").is_file());
        assert!(!installed.join(".git").exists());
    }

    #[test]
    fn oversized_git_resource_fails_without_partial_install() {
        let source = tempdir().expect("source tempdir");
        let global = tempdir().expect("global tempdir");
        fs::File::create(source.path().join("asset.bin"))
            .and_then(|file| file.set_len(MAX_INSTALLED_RESOURCE_BYTES + 1))
            .expect("sparse oversized asset");
        fs::write(
            source.path().join("SKILL.md"),
            "---\nname: oversized-git\nassets: [asset.bin]\n---\nBody\n",
        )
        .expect("manifest");
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "nib-tests@example.invalid"],
            vec!["config", "user.name", "nib tests"],
            vec!["add", "SKILL.md", "asset.bin"],
            vec!["commit", "--quiet", "-m", "skill"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(source.path())
                .status()
                .expect("git command")
                .success());
        }

        let error = install_skill_to(
            &format!("file://{}", source.path().display()),
            global.path(),
        )
        .expect_err("oversized cloned resource must fail");
        assert!(error.contains("2097152-byte limit"));
        assert!(fs::read_dir(global.path()).unwrap().next().is_none());
    }

    #[test]
    fn git_staging_limit_failure_removes_the_checkout() {
        let source = tempdir().expect("source tempdir");
        let staging = tempdir().expect("staging tempdir");
        fs::write(
            source.path().join("SKILL.md"),
            "---\nname: bounded-git\n---\nBody\n",
        )
        .expect("manifest");
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "nib-tests@example.invalid"],
            vec!["config", "user.name", "nib tests"],
            vec!["add", "SKILL.md"],
            vec!["commit", "--quiet", "-m", "skill"],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(source.path())
                .status()
                .expect("git command")
                .success());
        }
        let limits = StagingLimits {
            max_file_bytes: 1,
            max_bytes: 1,
            max_entries: 100,
            max_depth: 16,
        };

        let error = match prepare_git_source_with_limits(
            &format!("file://{}", source.path().display()),
            staging.path(),
            limits,
        ) {
            Ok(_) => panic!("staging limits must reject the clone"),
            Err(error) => error,
        };

        assert!(error.contains("staging limits"));
        assert!(!staging.path().join("checkout").exists());
    }

    #[test]
    fn staging_tree_enforces_entry_depth_per_file_and_aggregate_budgets() {
        let entries = tempdir().expect("entry tempdir");
        fs::write(entries.path().join("one"), "1").expect("entry one");
        fs::write(entries.path().join("two"), "2").expect("entry two");
        let entry_error = validate_staging_tree(
            entries.path(),
            StagingLimits {
                max_file_bytes: 8,
                max_bytes: 16,
                max_entries: 1,
                max_depth: 4,
            },
        )
        .expect_err("entry limit");
        assert!(entry_error.contains("entry limit"));

        let depth = tempdir().expect("depth tempdir");
        fs::create_dir_all(depth.path().join("one/two")).expect("deep staging tree");
        let depth_error = validate_staging_tree(
            depth.path(),
            StagingLimits {
                max_file_bytes: 8,
                max_bytes: 16,
                max_entries: 4,
                max_depth: 1,
            },
        )
        .expect_err("depth limit");
        assert!(depth_error.contains("depth limit"));

        let per_file = tempdir().expect("per-file tempdir");
        fs::write(per_file.path().join("large"), "123456789").expect("large file");
        let per_file_error = validate_staging_tree(
            per_file.path(),
            StagingLimits {
                max_file_bytes: 8,
                max_bytes: 16,
                max_entries: 2,
                max_depth: 1,
            },
        )
        .expect_err("per-file limit");
        assert!(per_file_error.contains("per-file limit"));

        let aggregate = tempdir().expect("aggregate tempdir");
        fs::write(aggregate.path().join("one"), "123456").expect("aggregate one");
        fs::write(aggregate.path().join("two"), "123456").expect("aggregate two");
        let aggregate_error = validate_staging_tree(
            aggregate.path(),
            StagingLimits {
                max_file_bytes: 8,
                max_bytes: 10,
                max_entries: 2,
                max_depth: 1,
            },
        )
        .expect_err("aggregate limit");
        assert!(aggregate_error.contains("aggregate limit"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_kills_a_process_that_exceeds_live_staging_limits() {
        let staging = tempdir().expect("staging tempdir");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "mkdir -p \"$NIB_TEST_STAGING\"; printf 123456789 > \"$NIB_TEST_STAGING/large\"; sleep 10",
            ])
            .env("NIB_TEST_STAGING", staging.path());
        let started = Instant::now();
        let error = run_bounded_command_with_staging(
            &mut command,
            "live staging fixture",
            Duration::from_secs(5),
            Some((
                staging.path(),
                StagingLimits {
                    max_file_bytes: 8,
                    max_bytes: 16,
                    max_entries: 2,
                    max_depth: 1,
                },
            )),
        )
        .expect_err("live staging limit must stop the command");

        assert!(error.contains("per-file limit"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_watchdog_stops_aggregate_staging_overage() {
        let staging = tempdir().expect("staging tempdir");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "printf 123456 > \"$NIB_TEST_STAGING/one\"; printf 123456 > \"$NIB_TEST_STAGING/two\"; sleep 10",
            ])
            .env("NIB_TEST_STAGING", staging.path());
        let started = Instant::now();
        let error = run_bounded_command_with_staging(
            &mut command,
            "aggregate staging fixture",
            Duration::from_secs(5),
            Some((
                staging.path(),
                StagingLimits {
                    max_file_bytes: 8,
                    max_bytes: 10,
                    max_entries: 2,
                    max_depth: 1,
                },
            )),
        )
        .expect_err("aggregate staging limit must stop the command");

        assert!(error.contains("aggregate limit"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_applies_a_child_file_size_limit() {
        let staging = tempdir().expect("staging tempdir");
        let output_path = staging.path().join("large");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "dd if=/dev/zero of=\"$NIB_TEST_OUTPUT\" bs=1024 count=1024 2>/dev/null",
            ])
            .env("NIB_TEST_OUTPUT", &output_path);
        let limit = 4 * 1024;
        let error = run_bounded_command_with_staging(
            &mut command,
            "file size limit fixture",
            Duration::from_secs(2),
            Some((
                staging.path(),
                StagingLimits {
                    max_file_bytes: limit,
                    max_bytes: limit * 2,
                    max_entries: 2,
                    max_depth: 1,
                },
            )),
        )
        .expect_err("kernel file limit terminates the writer");

        assert!(error.contains("per-file limit"), "{error}");
        assert!(fs::metadata(output_path).unwrap().len() <= limit);
    }

    #[cfg(unix)]
    #[test]
    fn managed_macos_scope_never_claims_an_inner_process_group() {
        assert!(!should_create_unix_process_group(true, true));
        assert!(should_create_unix_process_group(true, false));
        assert!(should_create_unix_process_group(false, true));
    }

    #[cfg(unix)]
    #[test]
    fn command_wait_discards_stale_group_authority_after_direct_reap() {
        use std::os::unix::process::CommandExt;

        let mut victim_command = Command::new("sh");
        victim_command.args(["-c", "sleep 60"]).process_group(0);
        let mut victim = victim_command.spawn().expect("spawn victim group");
        let mut victim_ownership =
            CommandProcessOwnership::from_spawned_child(&victim, true).expect("victim ownership");
        let victim_group = victim_ownership
            .process_group
            .expect("victim process group");

        let mut completed_command = Command::new("sh");
        completed_command.args(["-c", "exit 0"]).process_group(0);
        let mut completed = completed_command.spawn().expect("spawn completed child");
        let mut completed_ownership = CommandProcessOwnership::from_spawned_child(&completed, true)
            .expect("completed ownership");
        assert!(completed.wait().expect("direct reap").success());

        // Simulate reuse of the stored numeric PGID after an external wait
        // consumed the process identity that established ownership.
        completed_ownership.process_group = Some(victim_group);
        assert!(completed_ownership
            .poll_exit(&mut completed)
            .expect("cached wait")
            .expect("completed status")
            .success());
        assert!(
            victim.try_wait().expect("inspect victim").is_none(),
            "wait signalled a process group after losing the leader identity"
        );

        let _ = victim_ownership.terminate_and_reap(&mut victim);
    }

    #[cfg(windows)]
    #[test]
    fn bounded_command_windows_job_terminates_descendant_before_return() {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };

        match std::env::var(WINDOWS_JOB_ROLE_ENV).as_deref() {
            Ok("leader") => {
                let pid_path =
                    PathBuf::from(std::env::var_os(WINDOWS_JOB_PID_PATH_ENV).expect("pid path"));
                let ack_path =
                    PathBuf::from(std::env::var_os(WINDOWS_JOB_ACK_PATH_ENV).expect("ack path"));
                let mut descendant =
                    Command::new(std::env::current_exe().expect("current test executable"));
                descendant
                    .args(["--exact", WINDOWS_JOB_FIXTURE_TEST, "--nocapture"])
                    .env(WINDOWS_JOB_ROLE_ENV, "descendant")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let descendant = descendant.spawn().expect("spawn descendant fixture");
                fs::write(&pid_path, descendant.id().to_string()).expect("write descendant pid");
                let deadline = Instant::now() + Duration::from_secs(10);
                while !ack_path.is_file() {
                    assert!(
                        Instant::now() < deadline,
                        "descendant observer did not acknowledge its process handle"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                return;
            }
            Ok("descendant") => loop {
                thread::sleep(Duration::from_secs(60));
            },
            _ => {}
        }

        let root = tempdir().expect("Windows Job fixture root");
        let pid_path = root.path().join("descendant.pid");
        let ack_path = root.path().join("descendant.ack");
        let observed_pid_path = pid_path.clone();
        let observed_ack_path = ack_path.clone();
        let observer = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !observed_pid_path.is_file() {
                assert!(
                    Instant::now() < deadline,
                    "leader did not publish its descendant pid"
                );
                thread::sleep(Duration::from_millis(10));
            }
            let process_id: u32 = fs::read_to_string(&observed_pid_path)
                .expect("read descendant pid")
                .parse()
                .expect("numeric descendant pid");
            let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, process_id) };
            assert!(!process.is_null(), "open descendant process handle");
            let process = unsafe { OwnedHandle::from_raw_handle(process.cast()) };
            fs::write(observed_ack_path, b"observed").expect("acknowledge descendant handle");
            process
        });

        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .args(["--exact", WINDOWS_JOB_FIXTURE_TEST, "--nocapture"])
            .env(WINDOWS_JOB_ROLE_ENV, "leader")
            .env(WINDOWS_JOB_PID_PATH_ENV, &pid_path)
            .env(WINDOWS_JOB_ACK_PATH_ENV, &ack_path);
        let output = run_bounded_command(
            &mut command,
            "Windows Job descendant fixture",
            Duration::from_secs(15),
        )
        .expect("bounded leader command");
        let descendant = observer.join().expect("descendant observer");

        assert!(output.status.success());
        let wait_result = unsafe { WaitForSingleObject(descendant.as_raw_handle() as HANDLE, 0) };
        assert_eq!(
            wait_result, WAIT_OBJECT_0,
            "bounded command returned before its Windows Job became empty"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_terminates_a_hung_process_group() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10"]);
        let started = Instant::now();
        let error = run_bounded_command(&mut command, "hung fixture", Duration::from_millis(50))
            .expect_err("command must time out");

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_deadline_is_not_starved_by_stderr_flooding() {
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do printf 12345678901234567890 >&2; done"]);
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            "stderr flood fixture",
            Duration::from_millis(50),
        )
        .expect_err("command must time out");

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_command_cleans_up_a_lingering_process_group_after_success() {
        let root = tempdir().expect("pid marker root");
        let marker = root.path().join("descendant.pid");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sleep 10 & printf '%s' \"$!\" > \"$NIB_TEST_DESCENDANT_PID\"",
            ])
            .env("NIB_TEST_DESCENDANT_PID", &marker);
        let started = Instant::now();
        let output = run_bounded_command(
            &mut command,
            "successful leader fixture",
            Duration::from_secs(1),
        )
        .expect("leader succeeds");

        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(1));
        let descendant_pid: i32 = fs::read_to_string(&marker)
            .expect("read descendant pid")
            .parse()
            .expect("numeric descendant pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = unsafe { libc::kill(descendant_pid, 0) };
            if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bounded command returned while its lingering process-group member was still live"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_does_not_wait_for_a_detached_stderr_holder() {
        let mut command = Command::new("sh");
        command.args(["-c", "setsid sh -c 'sleep 3' &"]);
        let deadline = Duration::from_millis(250);
        let started = Instant::now();
        let output = run_bounded_command(&mut command, "detached holder fixture", deadline)
            .expect("leader succeeds while detached holder remains");

        assert!(output.status.success());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    #[serial]
    fn installed_skills_classifies_global_and_project_local_roots() {
        let home = tempdir().expect("home");
        let project = tempdir().expect("project");
        let previous_home = std::env::var_os("HOME");
        let previous_skills = std::env::var_os("NIB_SKILLS_DIR");
        std::env::remove_var("NIB_SKILLS_DIR");
        std::env::set_var("HOME", home.path());
        create_skill(&home.path().join(".config/nib/skills"), "global-skill");
        create_skill(&project.path().join(".nib/skills"), "local-skill");

        let installed = installed_skills(project.path());

        restore_env("HOME", previous_home);
        restore_env("NIB_SKILLS_DIR", previous_skills);
        let installed = installed.expect("list installed skills");
        assert!(installed
            .iter()
            .any(|skill| skill.name == "global-skill" && skill.location == "global"));
        assert!(installed
            .iter()
            .any(|skill| skill.name == "local-skill" && skill.location == "local"));
    }

    #[test]
    #[serial]
    fn installed_skills_are_sorted_by_location_name_and_path() {
        let global = tempdir().expect("global");
        let project = tempdir().expect("project");
        let previous = std::env::var_os("NIB_SKILLS_DIR");
        std::env::set_var("NIB_SKILLS_DIR", global.path());
        create_skill(global.path(), "zeta-global");
        create_skill(global.path(), "alpha-global");
        create_skill(&project.path().join(".nib/skills"), "zeta-local");
        create_skill(&project.path().join(".skills"), "alpha-local");

        let installed = installed_skills(project.path());

        restore_env("NIB_SKILLS_DIR", previous);
        let installed = installed.expect("list installed skills");
        let order = installed
            .iter()
            .map(|skill| (skill.location, skill.name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                ("global", "alpha-global"),
                ("global", "zeta-global"),
                ("local", "alpha-local"),
                ("local", "zeta-local"),
            ]
        );
    }

    #[test]
    #[serial]
    fn skill_list_reports_malformed_discovered_manifest() {
        let global = tempdir().expect("global");
        let project = tempdir().expect("project");
        let previous = std::env::var_os("NIB_SKILLS_DIR");
        std::env::set_var("NIB_SKILLS_DIR", global.path());
        let broken = project.path().join(".nib/skills/broken");
        fs::create_dir_all(&broken).expect("broken skill directory");
        fs::write(broken.join("SKILL.md"), "name: broken\n").expect("broken manifest");

        let result = list_skills(project.path());

        restore_env("NIB_SKILLS_DIR", previous);
        let error = result.expect_err("malformed installed skill must fail listing");
        let malformed_path = PathBuf::from("broken").join("SKILL.md");
        assert!(error.contains(malformed_path.to_string_lossy().as_ref()));
        assert!(error.contains("must start with YAML frontmatter"));
    }

    #[test]
    #[serial]
    fn skill_list_reports_truncated_discovery() {
        let global = tempdir().expect("global");
        let project = tempdir().expect("project");
        let previous = std::env::var_os("NIB_SKILLS_DIR");
        std::env::set_var("NIB_SKILLS_DIR", global.path());
        let local = project.path().join(".nib/skills");
        for index in 0..257 {
            create_skill(&local, &format!("skill-{index:03}"));
        }

        let result = list_skills(project.path());

        restore_env("NIB_SKILLS_DIR", previous);
        let error = result.expect_err("truncated installed skill inventory must fail");
        assert!(error.contains("skill discovery was truncated"));
        assert!(error.contains("skill count"));
        assert!(error.contains("256"));
    }

    #[test]
    #[serial]
    fn command_dispatch_installs_lists_and_removes_from_configured_global_root() {
        let source = tempdir().expect("source");
        let global = tempdir().expect("global");
        let project = tempdir().expect("project");
        let skill = create_skill(source.path(), "dispatch-skill");
        let previous = std::env::var_os("NIB_SKILLS_DIR");
        std::env::set_var("NIB_SKILLS_DIR", global.path());

        run_skill_cmd(
            &SkillArgs {
                command: SkillCommands::List,
            },
            project.path(),
        )
        .expect("list empty skills");
        run_skill_cmd(
            &SkillArgs {
                command: SkillCommands::Install {
                    source: skill.to_string_lossy().into_owned(),
                },
            },
            project.path(),
        )
        .expect("install through dispatcher");
        assert!(global.path().join("dispatch-skill/SKILL.md").is_file());
        let installed = installed_skills(project.path()).expect("list installed skill");
        assert!(installed.iter().any(|skill| {
            skill.name == "dispatch-skill"
                && skill.location == "global"
                && skill.path.starts_with(global.path())
        }));
        run_skill_cmd(
            &SkillArgs {
                command: SkillCommands::List,
            },
            project.path(),
        )
        .expect("list installed skill");
        run_skill_cmd(
            &SkillArgs {
                command: SkillCommands::Remove {
                    name: "dispatch-skill".to_string(),
                },
            },
            project.path(),
        )
        .expect("remove through dispatcher");
        assert!(!global.path().join("dispatch-skill").exists());

        restore_env("NIB_SKILLS_DIR", previous);
    }

    #[test]
    #[serial]
    fn global_directory_resolution_uses_home_and_reports_missing_environment() {
        let home = tempdir().expect("home");
        let previous_skills = std::env::var_os("NIB_SKILLS_DIR");
        let previous_home = std::env::var_os("HOME");
        std::env::remove_var("NIB_SKILLS_DIR");
        std::env::set_var("HOME", home.path());
        assert_eq!(
            global_skills_dir().expect("HOME fallback"),
            home.path().join(".config/nib/skills")
        );

        std::env::remove_var("HOME");
        assert!(global_skills_dir()
            .expect_err("missing global root")
            .contains("could not determine"));

        restore_env("HOME", previous_home);
        restore_env("NIB_SKILLS_DIR", previous_skills);
    }

    #[test]
    fn skill_name_and_manifest_guards_reject_unsafe_sources() {
        assert_eq!(safe_skill_name("  Rust Tool!  ").unwrap(), "rust-tool");
        assert!(safe_skill_name("---").is_err());
        assert!(safe_skill_name(&"x".repeat(129)).is_err());

        let source = tempdir().expect("source");
        let global = tempdir().expect("global");
        let wrong_name = source.path().join("manifest.md");
        fs::write(
            &wrong_name,
            "---\nname: wrong-file\ndescription: test\n---\nBody\n",
        )
        .expect("manifest");
        assert!(
            install_skill_to(wrong_name.to_str().unwrap(), global.path())
                .expect_err("manifest filename")
                .contains("named SKILL.md")
        );

        let missing_manifest = source.path().join("missing-manifest");
        fs::create_dir(&missing_manifest).expect("directory");
        assert!(
            install_skill_to(missing_manifest.to_str().unwrap(), global.path())
                .expect_err("missing manifest")
                .contains("invalid SKILL.md")
        );

        fs::write(global.path().join("not-a-directory"), "file").expect("guard file");
        assert!(remove_skill_from("not-a-directory", global.path())
            .expect_err("file removal guard")
            .contains("not a local directory"));
    }

    #[test]
    fn http_manifest_failure_is_reported_without_installing() {
        let global = tempdir().expect("global");
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).expect("read request");
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing"
            )
            .expect("write response");
        });

        let error = install_skill_to(&format!("http://{address}/SKILL.md"), global.path())
            .expect_err("HTTP failure");
        server.join().expect("server thread");
        assert!(error.contains("failed to download skill"));
        assert!(fs::read_dir(global.path()).unwrap().next().is_none());
    }
}
