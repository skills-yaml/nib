//! Host-side Git write tools (T080 phase 2b).
//!
//! The sandbox keeps Git metadata read-only, so commits and pushes run here,
//! on the host, only after approval. They use the user's own Git environment,
//! so commit signing, credential helpers and the user's hooks behave exactly
//! as when the user runs Git; the sandbox has kept the agent from changing
//! hooks or configuration. Arguments are structured: no free-form flags, no
//! shell, literal pathspecs and a `--` separator before paths. nib's own state
//! (`.nib`) is never staged.

use serde_json::{json, Value};
use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_PATHS: usize = 256;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_PREVIEW_LINES: usize = 40;
const COMMIT_TIMEOUT: Duration = Duration::from_secs(180);
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Branches nib never commits to directly; it branches first (user decision D6).
const PROTECTED_BRANCHES: [&str; 2] = ["main", "master"];
const BRANCH_PREFIX: &str = "nib/";
/// Pathspec that keeps nib state out of every commit.
const EXCLUDE_NIB_STATE: &str = ":(exclude,top).nib";
/// Inherited variables that would point Git at another repository or index.
const REPOSITORY_OVERRIDES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Runs Git in its own session without a controlling terminal, so SSH,
/// pinentry or credential prompts that open `/dev/tty` fail immediately
/// instead of hanging or drawing over the TUI. Askpass programs and SSH or GPG
/// agents keep working. The whole process group is killed on timeout and
/// after Git exits, and output is capped while it streams.
async fn git(cwd: &Path, args: &[&str], limit: Duration) -> Result<GitOutput, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in REPOSITORY_OVERRIDES {
        command.env_remove(key);
    }
    #[cfg(unix)]
    // SAFETY: the closure only calls the async-signal-safe setsid before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("git could not start: {error}"))?;
    let process_group = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let finished = tokio::time::timeout(limit, async {
        tokio::join!(read_capped(stdout), read_capped(stderr), child.wait())
    })
    .await;
    kill_process_group(process_group);
    let Ok((stdout, stderr, status)) = finished else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(format!(
            "git {} timed out",
            args.first().copied().unwrap_or("")
        ));
    };
    let status = status.map_err(|error| format!("git did not finish: {error}"))?;
    Ok(GitOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(unix)]
fn kill_process_group(leader: Option<u32>) {
    if let Some(group) = leader.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: signals only the process group created for this Git call.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_leader: Option<u32>) {}

/// Keeps the first bytes of a stream and drains the rest, so a noisy hook
/// cannot exhaust memory or block on a full pipe.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>) -> String {
    let Some(mut reader) = reader else {
        return String::new();
    };
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&buffer[..count.min(room)]);
                truncated |= count > room;
            }
        }
    }
    let text = String::from_utf8_lossy(&kept).trim().to_string();
    if truncated {
        format!("{text}\n[output truncated]")
    } else {
        text
    }
}

async fn require(cwd: &Path, args: &[&str], limit: Duration) -> Result<String, String> {
    let output = git(cwd, args, limit).await?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            if output.stderr.is_empty() {
                output.stdout
            } else {
                output.stderr
            }
        ))
    }
}

async fn current_branch(cwd: &Path) -> Option<String> {
    let output = git(
        cwd,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        QUERY_TIMEOUT,
    )
    .await
    .ok()?;
    (output.success && !output.stdout.is_empty()).then_some(output.stdout)
}

async fn default_branch(cwd: &Path) -> Option<String> {
    let output = git(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        QUERY_TIMEOUT,
    )
    .await
    .ok()?;
    let name = output.stdout.strip_prefix("origin/")?.to_string();
    (output.success && !name.is_empty()).then_some(name)
}

async fn branch_exists(cwd: &Path, name: &str) -> bool {
    git(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
        QUERY_TIMEOUT,
    )
    .await
    .is_ok_and(|output| output.success)
}

async fn is_protected(cwd: &Path, branch: Option<&str>) -> bool {
    let default = default_branch(cwd).await;
    branch.is_none_or(|branch| {
        PROTECTED_BRANCHES.contains(&branch) || default.as_deref() == Some(branch)
    })
}

/// Lowercase, dash-separated slug of the message's first line.
pub(crate) fn branch_slug(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or_default();
    let mut slug = String::new();
    for character in first_line.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "change".to_string()
    } else {
        slug
    }
}

fn validate_message(args: &Value) -> Result<String, String> {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .ok_or("git_commit requires a non-empty message")?;
    if message.len() > MAX_MESSAGE_BYTES || message.contains('\0') {
        return Err("commit message is too long or contains NUL".to_string());
    }
    Ok(message.to_string())
}

/// Workspace-relative paths only: no absolute paths, parent components,
/// option-like names or nib state.
fn validate_paths(args: &Value) -> Result<Vec<String>, String> {
    let Some(paths) = args.get("paths") else {
        return Ok(Vec::new());
    };
    let paths = paths
        .as_array()
        .ok_or("git_commit paths must be an array of workspace-relative paths")?;
    if paths.len() > MAX_PATHS {
        return Err(format!("git_commit accepts at most {MAX_PATHS} paths"));
    }
    paths
        .iter()
        .map(|path| {
            let path = path
                .as_str()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .ok_or("git_commit paths must be non-empty strings")?;
            let relative = Path::new(path);
            let safe = !path.starts_with('-')
                && !path.contains('\0')
                && relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
            if !safe {
                return Err(format!(
                    "git_commit path must stay inside the workspace: {path}"
                ));
            }
            if relative
                .components()
                .any(|component| component.as_os_str() == ".nib")
            {
                return Err(format!("git_commit never commits nib state: {path}"));
            }
            Ok(path.to_string())
        })
        .collect()
}

/// Pathspecs for the requested paths (literal) or the whole workspace, always
/// excluding nib state.
fn pathspecs(paths: &[String]) -> Vec<String> {
    let mut specs: Vec<String> = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths
            .iter()
            .map(|path| format!(":(literal){path}"))
            .collect()
    };
    specs.push(EXCLUDE_NIB_STATE.to_string());
    specs
}

/// Changes the commit would include, as short status lines.
async fn pending_changes(cwd: &Path, specs: &[String]) -> Result<Vec<String>, String> {
    let mut args = vec!["status", "--porcelain=v1", "--untracked-files=all", "--"];
    args.extend(specs.iter().map(String::as_str));
    let output = require(cwd, &args, QUERY_TIMEOUT).await?;
    Ok(output
        .lines()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
        .collect())
}

/// Untracked directories that are themselves repositories would be committed
/// as opaque gitlinks; they are refused so nothing unseen is published.
/// Without `--directory`, Git lists each untracked nested repository as one
/// entry ending in `/`, while ordinary untracked files are listed by name.
async fn untracked_repositories(cwd: &Path, specs: &[String]) -> Result<Vec<String>, String> {
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "--"];
    args.extend(specs.iter().map(String::as_str));
    let output = require(cwd, &args, QUERY_TIMEOUT).await?;
    Ok(output
        .lines()
        .filter(|entry| entry.ends_with('/'))
        .map(str::to_string)
        .collect())
}

/// Commits workspace changes on the current branch, branching first when
/// HEAD is detached or on a default or protected branch. A failure after
/// branching returns to the original branch and removes the new one.
pub(crate) async fn git_commit(args: &Value, cwd: &Path) -> Result<Value, String> {
    let message = validate_message(args)?;
    let paths = validate_paths(args)?;
    let specs = pathspecs(&paths);
    if pending_changes(cwd, &specs).await?.is_empty() {
        return Err("nothing to commit: the requested paths have no changes".to_string());
    }
    let nested = untracked_repositories(cwd, &specs).await?;
    if !nested.is_empty() {
        return Err(format!(
            "refusing to commit untracked nested repositories: {}",
            nested.join(", ")
        ));
    }

    let original_branch = current_branch(cwd).await;
    let original_commit = require(cwd, &["rev-parse", "HEAD"], QUERY_TIMEOUT).await?;
    let mut created_branch = None;
    if is_protected(cwd, original_branch.as_deref()).await {
        let base = format!("{BRANCH_PREFIX}{}", branch_slug(&message));
        let mut name = base.clone();
        let mut suffix = 2;
        while branch_exists(cwd, &name).await {
            if suffix > 50 {
                return Err("could not choose a free branch name".to_string());
            }
            name = format!("{base}-{suffix}");
            suffix += 1;
        }
        require(cwd, &["switch", "--quiet", "-c", &name], QUERY_TIMEOUT).await?;
        created_branch = Some(name);
    }

    if let Err(error) = stage_and_commit(cwd, &specs, &message).await {
        if let Some(created) = &created_branch {
            let back = original_branch.as_deref().unwrap_or(&original_commit);
            let mut switch = vec!["switch", "--quiet"];
            if original_branch.is_none() {
                switch.push("--detach");
            }
            switch.push(back);
            let _ = require(cwd, &switch, QUERY_TIMEOUT).await;
            let _ = require(cwd, &["branch", "--quiet", "-D", created], QUERY_TIMEOUT).await;
        }
        return Err(error);
    }
    let commit = require(cwd, &["rev-parse", "HEAD"], QUERY_TIMEOUT).await?;
    let summary = require(
        cwd,
        &["show", "--stat", "--format=", "--no-color", "HEAD"],
        QUERY_TIMEOUT,
    )
    .await?;
    Ok(json!({
        "commit": commit,
        "branch": created_branch.clone().or(current_branch(cwd).await),
        "created_branch": created_branch,
        "summary": summary,
    }))
}

async fn stage_and_commit(cwd: &Path, specs: &[String], message: &str) -> Result<(), String> {
    let mut add = vec!["add", "--all", "--"];
    add.extend(specs.iter().map(String::as_str));
    require(cwd, &add, QUERY_TIMEOUT).await?;
    let staged = git(cwd, &["diff", "--cached", "--quiet"], QUERY_TIMEOUT).await?;
    if staged.success {
        return Err("nothing to commit: no changes are staged".to_string());
    }
    require(cwd, &["commit", "--quiet", "-m", message], COMMIT_TIMEOUT)
        .await
        .map(|_| ())
}

fn validate_remote(args: &Value) -> Result<&str, String> {
    let remote = args
        .get("remote")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|remote| !remote.is_empty())
        .unwrap_or("origin");
    let valid = !remote.starts_with('-')
        && remote.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        });
    if valid {
        Ok(remote)
    } else {
        Err(format!("invalid remote name: {remote}"))
    }
}

/// Pushes the current branch to a configured remote and sets its upstream.
/// The refspec is pinned to the same branch name; no force or refspec
/// arguments are accepted.
pub(crate) async fn git_push(args: &Value, cwd: &Path) -> Result<Value, String> {
    let remote = validate_remote(args)?;
    require(cwd, &["remote", "get-url", remote], QUERY_TIMEOUT)
        .await
        .map_err(|_| format!("remote {remote} is not configured"))?;
    let branch = current_branch(cwd)
        .await
        .ok_or("git_push requires a checked-out branch, not a detached HEAD")?;
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let output = git(
        cwd,
        &[
            "push",
            "--set-upstream",
            "--porcelain",
            "--",
            remote,
            &refspec,
        ],
        PUSH_TIMEOUT,
    )
    .await?;
    if !output.success {
        return Err(format!("git push failed: {}", output.stderr));
    }
    Ok(json!({
        "remote": remote,
        "branch": branch,
        "result": output.stdout,
    }))
}

/// Removes `user:password@` from a URL so previews never show credentials.
pub(crate) fn without_credentials(url: &str) -> String {
    let Some(scheme) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme + 3;
    let authority_end = url[authority_start..]
        .find('/')
        .map_or(url.len(), |offset| authority_start + offset);
    match url[authority_start..authority_end].rfind('@') {
        Some(at) => format!(
            "{}{}",
            &url[..authority_start],
            &url[authority_start + at + 1..]
        ),
        None => url.to_string(),
    }
}

fn bounded_lines(lines: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut lines: Vec<String> = lines.into_iter().collect();
    if lines.len() > MAX_PREVIEW_LINES {
        let omitted = lines.len() - MAX_PREVIEW_LINES;
        lines.truncate(MAX_PREVIEW_LINES);
        lines.push(format!("… {omitted} more"));
    }
    lines
}

/// What the user approves: for a commit, the branch, message and changed
/// files with a diff stat; for a push, the remote, branch and commits to be
/// published. Best effort: an empty preview falls back to the arguments.
pub(crate) async fn approval_preview(tool: &str, args: &Value, cwd: &Path) -> Vec<String> {
    match tool {
        "git_commit" => commit_preview(args, cwd).await,
        "git_push" => push_preview(args, cwd).await,
        _ => Ok(Vec::new()),
    }
    .unwrap_or_default()
}

async fn commit_preview(args: &Value, cwd: &Path) -> Result<Vec<String>, String> {
    let message = validate_message(args)?;
    let specs = pathspecs(&validate_paths(args)?);
    let branch = current_branch(cwd).await;
    let branch_line = if is_protected(cwd, branch.as_deref()).await {
        format!(
            "Branch: new {BRANCH_PREFIX}{} (from {})",
            branch_slug(&message),
            branch.as_deref().unwrap_or("detached HEAD")
        )
    } else {
        format!("Branch: {}", branch.unwrap_or_default())
    };
    let mut lines = vec![
        format!("Message: {}", message.lines().next().unwrap_or_default()),
        branch_line,
        "Changes:".to_string(),
    ];
    lines.extend(bounded_lines(
        pending_changes(cwd, &specs)
            .await?
            .into_iter()
            .map(|line| format!("  {line}")),
    ));
    let mut stat = vec!["diff", "HEAD", "--stat", "--no-color", "--"];
    stat.extend(specs.iter().map(String::as_str));
    if let Ok(summary) = require(cwd, &stat, QUERY_TIMEOUT).await {
        lines.extend(bounded_lines(summary.lines().map(str::to_string)));
    }
    Ok(lines)
}

async fn push_preview(args: &Value, cwd: &Path) -> Result<Vec<String>, String> {
    let remote = validate_remote(args)?;
    let url = require(cwd, &["remote", "get-url", remote], QUERY_TIMEOUT).await?;
    let branch = current_branch(cwd)
        .await
        .ok_or("detached HEAD cannot be pushed")?;
    let tracking = format!("refs/remotes/{remote}/{branch}");
    let tracked = git(
        cwd,
        &["rev-parse", "--verify", "--quiet", &tracking],
        QUERY_TIMEOUT,
    )
    .await
    .is_ok_and(|output| output.success);
    let range = format!("{tracking}..HEAD");
    let not_remote = format!("--remotes={remote}");
    let log: Vec<&str> = if tracked {
        vec!["log", "--oneline", "--no-color", "-n", "41", &range]
    } else {
        vec![
            "log",
            "--oneline",
            "--no-color",
            "-n",
            "41",
            "HEAD",
            "--not",
            &not_remote,
        ]
    };
    let commits = require(cwd, &log, QUERY_TIMEOUT).await?;
    let mut lines = vec![
        format!("Remote: {remote} {}", without_credentials(&url)),
        format!("Branch: {branch} -> {remote}/{branch}"),
        "Commits:".to_string(),
    ];
    lines.extend(bounded_lines(
        commits.lines().map(|line| format!("  {line}")),
    ));
    Ok(lines)
}

/// Whether a terminal command tries to commit or push, which must go through
/// the approved host-side tools instead. Git's global options (`-C dir`,
/// `-c key=value`, `--git-dir=...`) are skipped to find the subcommand. The
/// sandbox keeps Git metadata read-only regardless; this gives the model the
/// right tool instead of a read-only error.
pub(crate) fn terminal_git_write(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ';' | '|' | '&' | '(' | ')')
        })
        .filter(|word| !word.is_empty())
        .collect();
    let mut index = 0;
    while index < words.len() {
        let program = words[index].rsplit('/').next().unwrap_or(words[index]);
        index += 1;
        if program != "git" {
            continue;
        }
        while let Some(word) = words.get(index) {
            if matches!(
                *word,
                "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace"
            ) {
                index += 2;
            } else if word.starts_with('-') {
                index += 1;
            } else {
                break;
            }
        }
        match words.get(index).copied() {
            Some("commit") => return Some("git_commit"),
            Some("push") => return Some("git_push"),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
#[path = "git_tools_tests.rs"]
mod tests;
