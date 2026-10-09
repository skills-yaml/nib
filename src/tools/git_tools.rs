//! Host-side Git write tools (T080 phase 2b).
//!
//! The sandbox keeps Git metadata read-only, so commits and pushes run here,
//! on the host, only after approval. They use the user's own Git environment,
//! so commit signing, credential helpers and the user's hooks behave exactly
//! as when the user runs Git; the sandbox has kept the agent from changing
//! hooks or configuration. Arguments are structured: no free-form flags, no
//! shell, literal pathspecs and a `--` separator before paths.

use serde_json::{json, Value};
use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;

const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_PATHS: usize = 256;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const COMMIT_TIMEOUT: Duration = Duration::from_secs(180);
const PUSH_TIMEOUT: Duration = Duration::from_secs(300);
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Branches nib never commits to directly; it branches first (user decision D6).
const PROTECTED_BRANCHES: [&str; 2] = ["main", "master"];
const BRANCH_PREFIX: &str = "nib/";

struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn git(cwd: &Path, args: &[&str], limit: Duration) -> Result<GitOutput, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(limit, command.output())
        .await
        .map_err(|_| format!("git {} timed out", args.first().copied().unwrap_or("")))?
        .map_err(|error| format!("git could not start: {error}"))?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: bounded(&output.stdout),
        stderr: bounded(&output.stderr),
    })
}

fn bounded(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.trim().to_string();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", text[..end].trim_end())
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

/// Workspace-relative paths only: no absolute paths, parent components or
/// option-like names.
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
            if safe {
                Ok(path.to_string())
            } else {
                Err(format!(
                    "git_commit path must stay inside the workspace: {path}"
                ))
            }
        })
        .collect()
}

/// Commits staged work on the current branch, branching first when HEAD is
/// detached or on a default or protected branch.
pub(crate) async fn git_commit(args: &Value, cwd: &Path) -> Result<Value, String> {
    let message = validate_message(args)?;
    let paths = validate_paths(args)?;

    let current = current_branch(cwd).await;
    let default = default_branch(cwd).await;
    let protected = current.as_deref().is_none_or(|branch| {
        PROTECTED_BRANCHES.contains(&branch) || default.as_deref() == Some(branch)
    });
    let mut created_branch = None;
    if protected {
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

    if paths.is_empty() {
        require(cwd, &["add", "--all"], QUERY_TIMEOUT).await?;
    } else {
        let mut add = vec!["add", "--"];
        add.extend(paths.iter().map(String::as_str));
        require(cwd, &add, QUERY_TIMEOUT).await?;
    }
    let staged = git(cwd, &["diff", "--cached", "--quiet"], QUERY_TIMEOUT).await?;
    if staged.success {
        return Err("nothing to commit: no changes are staged".to_string());
    }
    require(cwd, &["commit", "--quiet", "-m", &message], COMMIT_TIMEOUT).await?;
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

/// Pushes the current branch to a configured remote and sets its upstream.
/// No force or refspec arguments are accepted.
pub(crate) async fn git_push(args: &Value, cwd: &Path) -> Result<Value, String> {
    let remote = args
        .get("remote")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|remote| !remote.is_empty())
        .unwrap_or("origin");
    let valid_remote = !remote.starts_with('-')
        && remote.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        });
    if !valid_remote {
        return Err(format!("invalid remote name: {remote}"));
    }
    require(cwd, &["remote", "get-url", remote], QUERY_TIMEOUT)
        .await
        .map_err(|_| format!("remote {remote} is not configured"))?;
    let branch = current_branch(cwd)
        .await
        .ok_or("git_push requires a checked-out branch, not a detached HEAD")?;
    let output = git(
        cwd,
        &["push", "--set-upstream", "--porcelain", remote, &branch],
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
