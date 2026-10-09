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
    /// Enable a discovered skill without reinstalling it
    Enable { name: String },
    /// Disable a discovered skill without deleting it
    Disable { name: String },
    /// Select a discovered workflow for subsequent turns
    Use { name: String },
    /// Clear workflows selected with skill use
    Clear,
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
    pub enabled: bool,
    pub allow_implicit: bool,
}

pub fn run_skill_cmd(args: &SkillArgs, project_root: &Path) -> Result<(), String> {
    match &args.command {
        SkillCommands::List => list_skills(project_root),
        SkillCommands::Enable { name } => {
            println!("{}", manage_skill(project_root, "enable", name)?);
            Ok(())
        }
        SkillCommands::Disable { name } => {
            println!("{}", manage_skill(project_root, "disable", name)?);
            Ok(())
        }
        SkillCommands::Use { name } => {
            println!("{}", manage_skill(project_root, "use", name)?);
            Ok(())
        }
        SkillCommands::Clear => {
            println!("{}", manage_skill(project_root, "clear", "")?);
            Ok(())
        }
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
    println!("{}", format_installed_skills(project_root)?);
    Ok(())
}

pub fn format_installed_skills(project_root: &Path) -> Result<String, String> {
    let skills = installed_skills(project_root)?;
    let mut output = String::from("Installed Skills:");
    if skills.is_empty() {
        output.push_str("\n  No skills found.");
    }
    for skill in skills {
        output.push_str(&format!(
            "\n  [{}] {} - {}\n    path: {} (enabled: {}, implicit: {})",
            skill.location,
            skill.name,
            skill.description,
            skill.path.display(),
            skill.enabled,
            skill.allow_implicit
        ));
    }
    Ok(output)
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
    let (catalog, _) = project_skill_catalog(project_root)?;
    let global = global_skills_dir()?.canonicalize().ok();
    let mut installed = catalog
        .entries
        .into_iter()
        .map(|entry| InstalledSkill {
            name: entry.metadata.name,
            description: entry.metadata.description,
            location: if global
                .as_ref()
                .is_some_and(|root| entry.canonical_path.starts_with(root))
            {
                "global"
            } else {
                "local"
            },
            path: entry.path,
            enabled: entry.enabled,
            allow_implicit: entry.allow_implicit,
        })
        .collect::<Vec<_>>();
    installed.sort_by(|a, b| {
        a.location
            .cmp(b.location)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(installed)
}

fn catalog_for_config(
    project_root: &Path,
    config: &nib::config::NibConfig,
) -> Result<nib::context::skill_catalog::SkillCatalog, String> {
    let profiles = nib::profile::ProfileRegistry::load(project_root, &config.profiles)
        .map_err(|error| error.to_string())?;
    let profile = profiles
        .for_workspace(project_root)
        .unwrap_or_else(|| profiles.default_profile());
    nib::context::skill_catalog::SkillCatalog::discover(project_root, config, profile)
}

pub fn project_skill_catalog(
    project_root: &Path,
) -> Result<
    (
        nib::context::skill_catalog::SkillCatalog,
        nib::config::NibConfig,
    ),
    String,
> {
    let config =
        nib::config::load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    Ok((catalog_for_config(project_root, &config)?, config))
}

pub fn manage_skill(project_root: &Path, action: &str, selector: &str) -> Result<String, String> {
    nib::config::update_nib_config(project_root, |config| {
        if action == "clear" {
            config.skills.active.clear();
            return Ok(());
        }
        let mut catalog = catalog_for_config(project_root, config)?;
        // Disabled entries remain visible and selectable for re-enabling.
        if action == "enable" {
            for entry in &mut catalog.entries {
                entry.enabled = true;
            }
        }
        let entry = catalog.resolve(selector)?;
        let path = entry.path.clone();
        let identity = &entry.canonical_path;
        match action {
            "enable" | "disable" => {
                config.skills.config.retain(|control| {
                    let declared = if control.path.is_absolute() {
                        control.path.clone()
                    } else {
                        project_root.join(&control.path)
                    };
                    declared.canonicalize().ok().as_ref() != Some(identity)
                });
                config.skills.config.push(nib::config::SkillConfig {
                    path: path.clone(),
                    enabled: action == "enable",
                });
                if action == "disable" {
                    config.skills.active.retain(|active| {
                        active != &entry.metadata.name
                            && Path::new(active).canonicalize().ok().as_ref() != Some(identity)
                    });
                }
            }
            "use" => {
                config.skills.active = vec![path.to_string_lossy().to_string()];
            }
            _ => return Err("unknown skill management action".into()),
        }
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    Ok(format!(
        "Skill selection updated ({action}). Changes apply on the next user turn."
    ))
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
        let mut policy_probe = git_command();
        policy_probe
            .current_dir(&checkout)
            .args(["cat-file", "-e", "HEAD:agents/openai.yaml"]);
        let policy_exists = run_bounded_command_with_staging(
            &mut policy_probe,
            "git inspect skill invocation policy",
            SKILL_COMMAND_TIMEOUT,
            Some((&checkout, limits)),
        )?;
        if policy_exists.status.success() {
            resources.insert(PathBuf::from("agents/openai.yaml"));
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
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
    let invocation_policy = root.join("agents/openai.yaml");
    if invocation_policy.exists() {
        resources.insert(PathBuf::from("agents/openai.yaml"));
    }
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
#[path = "skill_cmd_tests.rs"]
mod tests;
