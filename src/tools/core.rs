//! Core tool implementations (called only after ToolExecutor gates pass).

use crate::config::ExecutionConfig;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_CONTENT_CHAR_LIMIT: usize = 50_000;
const MAX_CONTENT_CHAR_LIMIT: usize = 100_000;
const DEFAULT_READ_FILE_MAX_BYTES: usize = 64 * 1024;
const MAX_READ_FILE_MAX_BYTES: usize = 1024 * 1024;
const DEFAULT_READ_FILE_MAX_LINES: usize = 1_000;
const MAX_READ_FILE_MAX_LINES: usize = 10_000;
const MAX_READ_FILE_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES_SCANNED: usize = 20_000;
const MAX_GREP_FILES: usize = 10_000;
const MAX_GREP_FILE_SCAN_BYTES: usize = 8 * 1024 * 1024;
const MAX_GREP_TOTAL_SCAN_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_TERMINAL_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutputEvent {
    pub invocation_id: crate::tools::ToolInvocationId,
    pub tool_name: String,
    pub stream: TerminalOutputStream,
    pub chunk: Vec<u8>,
    pub background_task_id: Option<String>,
    pub eof: bool,
}

pub type TerminalOutputCallback = Arc<dyn Fn(TerminalOutputEvent) + Send + Sync>;

#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    tool_name: &str,
    invocation_id: crate::tools::ToolInvocationId,
    args: &Value,
    cwd: &Path,
    config: &ExecutionConfig,
    terminal_backend: &str,
    terminal_timeout_secs: u64,
    environment: &HashMap<String, String>,
    terminal_output_callback: Option<&TerminalOutputCallback>,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<Value, String> {
    match tool_name {
        "read_file" => read_file(args, cwd).await,
        "list_directory" => list_directory(args, cwd).await,
        "grep" => grep(args, cwd).await,
        "git_status" => git_status(cwd).await,
        "apply_patch" => apply_patch(args, cwd).await,
        "run_terminal" => {
            run_terminal(
                invocation_id,
                args,
                cwd,
                config,
                terminal_backend,
                terminal_timeout_secs,
                environment,
                terminal_output_callback,
            )
            .await
        }
        "write_plan" => write_plan(args, cwd).await,
        "spawn_subagent" => {
            crate::tools::delegation::spawn_subagent_cancellable(args, cwd, cancellation).await
        }
        "merge_subagent_worktree" => {
            crate::tools::delegation::merge_subagent_worktree(args, cwd).await
        }
        "invoke_subagent" => {
            crate::tools::delegation::spawn_subagent_cancellable(args, cwd, cancellation).await
        }
        "manage_subagents" => manage_subagents(args, cwd).await,
        "send_message" => send_message(args, cwd).await,
        "search_web" => search_web(args, cwd).await,
        "read_url_content" => read_url_content(args, cwd).await,
        "manage_task" => manage_task(args, cwd).await,
        "manage_memory" => manage_memory(args, cwd).await,
        "schedule" => schedule(args, cwd).await,
        "ask_question" => ask_question(args, cwd).await,
        "git_commit" => crate::tools::git_tools::git_commit(args, cwd).await,
        "git_push" => crate::tools::git_tools::git_push(args, cwd).await,
        other => Err(format!("No implementation for tool: {other}")),
    }
}

async fn git_status(cwd: &Path) -> Result<Value, String> {
    let mut command = crate::sandbox::read_only_git_status_command(cwd)?;
    let output = timeout(Duration::from_secs(15), async {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = crate::sandbox::spawn_managed_child(&mut command)
            .map_err(|_| "Git status could not start".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Git status output is unavailable")?;
        let mut bytes = Vec::new();
        stdout
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "Git status output could not be read")?;
        if bytes.len() > 64 * 1024 {
            child.terminate_and_reap().await;
            return Err("Git status output exceeds the 65536-byte limit".to_string());
        }
        let status = child
            .wait()
            .await
            .map_err(|_| "Git status could not finish")?;
        if !status.success() {
            return Err("Git status failed for the active repository".to_string());
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| "Git status timed out".to_string())??;
    let status =
        String::from_utf8(output).map_err(|_| "Git status output is not UTF-8".to_string())?;
    Ok(json!({"status": status}))
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn read_file(args: &Value, cwd: &Path) -> Result<Value, String> {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("missing path")?;
    let path = resolve_existing_path(cwd, path_str)?;
    if !path.is_file() {
        return Err(format!("Not a file: {}", path.display()));
    }

    let start = args
        .get("start_line")
        .and_then(Value::as_u64)
        .map(usize::try_from)
        .transpose()
        .map_err(|_| "start_line is too large".to_string())?
        .unwrap_or(0);
    let end = args
        .get("end_line")
        .and_then(Value::as_u64)
        .map(usize::try_from)
        .transpose()
        .map_err(|_| "end_line is too large".to_string())?;
    if end.is_some_and(|end| end < start) {
        return Err("end_line must be greater than or equal to start_line".to_string());
    }
    let max_bytes = bounded_usize_arg(
        args,
        "max_bytes",
        DEFAULT_READ_FILE_MAX_BYTES,
        1,
        MAX_READ_FILE_MAX_BYTES,
    )?;
    let max_lines = bounded_usize_arg(
        args,
        "max_lines",
        DEFAULT_READ_FILE_MAX_LINES,
        1,
        MAX_READ_FILE_MAX_LINES,
    )?;
    let total_bytes = tokio::fs::metadata(&path)
        .await
        .map_err(|error| error.to_string())?
        .len();
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| error.to_string())?;
    let mut read_buffer = [0u8; 8 * 1024];
    let mut selected = Vec::with_capacity(max_bytes.min(64 * 1024));
    let mut current_line = 0usize;
    let mut selected_lines = 0usize;
    let mut actual_end = start;
    let mut bytes_scanned = 0u64;
    let mut reached_eof = false;
    let mut byte_limit_reached = false;
    let mut line_limit_reached = false;
    let mut scan_limit_reached = false;
    let mut saw_byte = false;
    let mut last_byte_was_newline = false;

    'read: loop {
        if bytes_scanned >= MAX_READ_FILE_SCAN_BYTES {
            scan_limit_reached = bytes_scanned < total_bytes;
            break;
        }
        if end.is_some_and(|end| current_line >= end) {
            line_limit_reached = bytes_scanned < total_bytes;
            break;
        }
        if selected_lines >= max_lines {
            line_limit_reached = bytes_scanned < total_bytes;
            break;
        }

        let remaining_scan = (MAX_READ_FILE_SCAN_BYTES - bytes_scanned) as usize;
        let read_size = remaining_scan.min(read_buffer.len());
        let count = file
            .read(&mut read_buffer[..read_size])
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            reached_eof = true;
            break;
        }

        for byte in &read_buffer[..count] {
            bytes_scanned += 1;
            saw_byte = true;
            last_byte_was_newline = *byte == b'\n';
            let in_selected_range = current_line >= start
                && end.is_none_or(|requested_end| current_line < requested_end);
            if in_selected_range {
                if selected.len() >= max_bytes {
                    byte_limit_reached = true;
                    break 'read;
                }
                selected.push(*byte);
                actual_end = current_line.saturating_add(1);
            }

            if *byte == b'\n' {
                if in_selected_range {
                    selected_lines += 1;
                    actual_end = current_line.saturating_add(1);
                }
                current_line = current_line.saturating_add(1);
                if end.is_some_and(|requested_end| current_line >= requested_end)
                    || selected_lines >= max_lines
                {
                    line_limit_reached = bytes_scanned < total_bytes;
                    break 'read;
                }
            }
        }
    }

    if bytes_scanned >= total_bytes && !byte_limit_reached && !scan_limit_reached {
        reached_eof = true;
        line_limit_reached = false;
    }
    let scanned_total_lines = if saw_byte && !last_byte_was_newline {
        current_line.saturating_add(1)
    } else {
        current_line
    };
    let total_lines = if reached_eof {
        Some(scanned_total_lines)
    } else if total_bytes <= DEFAULT_READ_FILE_MAX_BYTES as u64 {
        let bounded_file = tokio::fs::File::open(&path)
            .await
            .map_err(|error| error.to_string())?;
        let mut bounded_bytes = Vec::with_capacity(DEFAULT_READ_FILE_MAX_BYTES.min(64 * 1024));
        bounded_file
            .take(DEFAULT_READ_FILE_MAX_BYTES as u64 + 1)
            .read_to_end(&mut bounded_bytes)
            .await
            .map_err(|error| error.to_string())?;
        if bounded_bytes.len() > DEFAULT_READ_FILE_MAX_BYTES {
            None
        } else {
            let bounded_file = std::str::from_utf8(&bounded_bytes)
                .map_err(|error| format!("file is not valid UTF-8: {error}"))?;
            Some(bounded_file.lines().count())
        }
    } else {
        None
    };
    let actual_start = total_lines.map_or(start, |lines| start.min(lines));
    if selected.is_empty() {
        actual_end = actual_start;
    }
    let selected = decode_bounded_utf8(selected)?;
    let content = selected.lines().collect::<Vec<_>>().join("\n");
    let omitted_before = actual_start > 0;
    let truncated_by_lines = omitted_before || line_limit_reached || scan_limit_reached;
    let truncated = byte_limit_reached || truncated_by_lines;

    Ok(json!({
        "path": path.to_string_lossy(),
        "content": content,
        "start_line": actual_start,
        "end_line": actual_end,
        "total_lines": total_lines,
        "total_lines_known": total_lines.is_some(),
        "total_bytes": total_bytes,
        "bytes_scanned": bytes_scanned,
        "bytes_returned": content.len(),
        "max_bytes": max_bytes,
        "max_lines": max_lines,
        "max_scan_bytes": MAX_READ_FILE_SCAN_BYTES,
        "truncated": truncated,
        "truncated_by_bytes": byte_limit_reached,
        "truncated_by_lines": truncated_by_lines,
        "scan_limit_reached": scan_limit_reached,
        "reached_eof": reached_eof,
    }))
}

fn decode_bounded_utf8(mut bytes: Vec<u8>) -> Result<String, String> {
    match String::from_utf8(bytes) {
        Ok(content) => Ok(content),
        Err(error) if error.utf8_error().error_len().is_none() => {
            let valid_up_to = error.utf8_error().valid_up_to();
            bytes = error.into_bytes();
            bytes.truncate(valid_up_to);
            String::from_utf8(bytes).map_err(|error| error.to_string())
        }
        Err(error) => Err(format!("file is not valid UTF-8: {}", error.utf8_error())),
    }
}

async fn list_directory(args: &Value, cwd: &Path) -> Result<Value, String> {
    let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let path = resolve_existing_path(cwd, rel)?;
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", path.display()));
    }

    let recursive = args
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let include_hidden = args
        .get("include_hidden")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let max_depth = args
        .get("max_depth")
        .and_then(|value| value.as_u64())
        .unwrap_or(5)
        .clamp(1, 20) as usize;
    let depth = if recursive { max_depth } else { 1 };
    let walk = walk_paths(&path, include_hidden, depth, 1_000)?;
    let mut entries = Vec::with_capacity(walk.paths.len());
    for entry_path in walk.paths {
        let metadata = std::fs::symlink_metadata(&entry_path).map_err(|error| error.to_string())?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        entries.push(json!({
            "path": entry_path.strip_prefix(&path).unwrap_or(&entry_path).to_string_lossy(),
            "type": if metadata.is_dir() { "dir" } else if metadata.file_type().is_symlink() { "symlink" } else { "file" },
            "size": metadata.len(),
            "modified_unix": modified,
        }));
    }

    Ok(json!({
        "path": path.to_string_lossy(),
        "entries": entries,
        "truncated": walk.truncated,
        "entries_scanned": walk.entries_scanned,
        "max_entries_scanned": MAX_DIRECTORY_ENTRIES_SCANNED,
    }))
}

struct WalkResult {
    paths: Vec<PathBuf>,
    entries_scanned: usize,
    truncated: bool,
}

fn walk_paths(
    root: &Path,
    include_hidden: bool,
    max_depth: usize,
    limit: usize,
) -> Result<WalkResult, String> {
    let mut output = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut entries_scanned = 0usize;
    while let Some((directory, depth)) = stack.pop() {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&directory).map_err(|error| error.to_string())? {
            if entries_scanned >= MAX_DIRECTORY_ENTRIES_SCANNED {
                return Ok(WalkResult {
                    paths: output,
                    entries_scanned,
                    truncated: true,
                });
            }
            entries.push(entry.map_err(|error| error.to_string())?);
            entries_scanned += 1;
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if !include_hidden && name.to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| error.to_string())?;
            output.push(path.clone());
            if output.len() >= limit {
                return Ok(WalkResult {
                    paths: output,
                    entries_scanned,
                    truncated: true,
                });
            }
            if file_type.is_dir() && depth + 1 < max_depth {
                stack.push((path, depth + 1));
            }
        }
    }
    Ok(WalkResult {
        paths: output,
        entries_scanned,
        truncated: false,
    })
}

async fn grep(args: &Value, cwd: &Path) -> Result<Value, String> {
    grep_with_limits(
        args,
        cwd,
        MAX_GREP_FILE_SCAN_BYTES,
        MAX_GREP_TOTAL_SCAN_BYTES,
    )
    .await
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn grep_with_limits(
    args: &Value,
    cwd: &Path,
    max_file_scan_bytes: usize,
    max_total_scan_bytes: usize,
) -> Result<Value, String> {
    let pattern = args
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or("missing pattern")?;
    let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let max_results = bounded_usize_arg(args, "max_results", 50, 1, 1_000)?;
    let path = resolve_existing_path(cwd, rel)?;
    let expression = Regex::new(pattern).map_err(|error| format!("invalid regex: {error}"))?;
    let glob = args.get("glob").and_then(|value| value.as_str());
    let mut matches = Vec::new();
    let (files, file_limit_reached, entries_scanned): (Vec<PathBuf>, bool, usize) = if path.is_dir()
    {
        let walk = walk_paths(&path, false, 20, MAX_GREP_FILES)?;
        let files = walk
            .paths
            .into_iter()
            .filter_map(|candidate| {
                let canonical = candidate.canonicalize().ok()?;
                (canonical.starts_with(&path) && canonical.is_file()).then_some(canonical)
            })
            .collect();
        (files, walk.truncated, walk.entries_scanned)
    } else {
        (vec![path.clone()], false, 1)
    };

    let files_considered = files.len();
    let mut files_scanned = 0usize;
    let mut files_skipped = 0usize;
    let mut files_truncated = 0usize;
    let mut bytes_scanned = 0usize;
    let mut aggregate_limit_reached = false;
    for (index, file) in files.into_iter().enumerate() {
        if matches.len() >= max_results {
            break;
        }
        let relative = if path.is_file() {
            file.file_name().map(Path::new).unwrap_or(&file)
        } else {
            file.strip_prefix(&path).unwrap_or(&file)
        };
        let glob_candidate = path_for_glob(relative);
        if glob.is_some_and(|pattern| !glob_matches(pattern, &glob_candidate)) {
            continue;
        }
        let remaining = max_total_scan_bytes.saturating_sub(bytes_scanned);
        if remaining == 0 {
            aggregate_limit_reached = true;
            files_skipped = files_skipped.saturating_add(files_considered.saturating_sub(index));
            break;
        }
        let Ok(metadata) = tokio::fs::metadata(&file).await else {
            files_skipped += 1;
            continue;
        };
        let requested = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let read_limit = requested.min(max_file_scan_bytes).min(remaining);
        let Ok(opened) = tokio::fs::File::open(&file).await else {
            files_skipped += 1;
            continue;
        };
        let mut reader = opened.take(read_limit as u64);
        let mut bytes = Vec::with_capacity(read_limit.min(64 * 1024));
        if reader.read_to_end(&mut bytes).await.is_err() {
            files_skipped += 1;
            continue;
        }
        files_scanned += 1;
        bytes_scanned = bytes_scanned.saturating_add(bytes.len());
        if requested > bytes.len() {
            files_truncated += 1;
            aggregate_limit_reached |= bytes_scanned >= max_total_scan_bytes;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            files_skipped += 1;
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if expression.is_match(line) {
                matches.push(json!({
                    "file": file.to_string_lossy(),
                    "line": i + 1,
                    "snippet": line.chars().take(200).collect::<String>(),
                }));
                if matches.len() >= max_results {
                    break;
                }
            }
        }
    }

    let max_results_reached = matches.len() >= max_results;
    let truncated =
        max_results_reached || file_limit_reached || aggregate_limit_reached || files_truncated > 0;
    Ok(json!({
        "pattern": pattern,
        "matches": matches,
        "truncated": truncated,
        "max_results_reached": max_results_reached,
        "file_limit_reached": file_limit_reached,
        "aggregate_limit_reached": aggregate_limit_reached,
        "files_considered": files_considered,
        "files_scanned": files_scanned,
        "files_skipped": files_skipped,
        "files_truncated": files_truncated,
        "entries_scanned": entries_scanned,
        "bytes_scanned": bytes_scanned,
        "max_file_scan_bytes": max_file_scan_bytes,
        "max_total_scan_bytes": max_total_scan_bytes,
    }))
}

async fn apply_patch(args: &Value, cwd: &Path) -> Result<Value, String> {
    let patch = args.get("patch").and_then(|v| v.as_str()).unwrap_or("");
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    if patch.is_empty() {
        return Err("empty patch".to_string());
    }

    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .arg("apply")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if dry_run {
        cmd.arg("--check");
    }
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("git apply failed to start: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("git apply stdin unavailable")?
        .write_all(patch.as_bytes())
        .await
        .map_err(|error| format!("failed to send patch to git apply: {error}"))?;
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("git apply failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git apply exited with {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(json!({
        "applied": !dry_run,
        "dry_run": dry_run,
        "exit_code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

async fn write_plan(args: &Value, cwd: &Path) -> Result<Value, String> {
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if content.is_empty() {
        return Err("Plan content is empty".to_string());
    }

    let nib_dir = cwd.join(".nib");
    let plans_dir = nib_dir.join("plans");
    tokio::fs::create_dir_all(&plans_dir)
        .await
        .map_err(|e| format!("Failed to create plans dir: {e}"))?;

    let plan_id = format!("plan-{}", uuid::Uuid::new_v4());
    let plan_path = plans_dir.join(format!("{}.md", plan_id));

    tokio::fs::write(&plan_path, content)
        .await
        .map_err(|e| format!("Failed to write plan file: {e}"))?;

    Ok(json!({
        "plan_id": plan_id,
        "path": plan_path.to_string_lossy(),
        "status": "saved",
    }))
}

#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn run_terminal(
    invocation_id: crate::tools::ToolInvocationId,
    args: &Value,
    cwd: &Path,
    config: &ExecutionConfig,
    terminal_backend: &str,
    terminal_timeout_secs: u64,
    environment: &HashMap<String, String>,
    terminal_output_callback: Option<&TerminalOutputCallback>,
) -> Result<Value, String> {
    if terminal_backend != "local" {
        return Err(format!(
            "terminal backend {terminal_backend:?} is not available in the local executor"
        ));
    }
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("missing command")?;
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(terminal_timeout_secs)
        .max(1);
    let max_output_bytes = terminal_output_limit(args)?;
    let run_cwd = args
        .get("cwd")
        .and_then(|value| value.as_str())
        .map(|path| resolve_existing_path(cwd, path))
        .transpose()?
        .unwrap_or_else(|| cwd.to_path_buf());
    if !run_cwd.is_dir() {
        return Err(format!(
            "terminal cwd is not a directory: {}",
            run_cwd.display()
        ));
    }

    if args
        .get("background")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        if std::env::var_os("NIB_MANAGED_PROCESS_SCOPE").is_some() {
            return Err(
                "durable background terminal jobs cannot be launched from a foreground managed-process scope"
                    .to_string(),
            );
        }
        let session_id = required_identifier(args, "_session_id")?.to_string();
        let sessions_dir = PathBuf::from(required_nonempty_string(args, "_sessions_dir")?);
        let (project_root, profile_id) =
            crate::daemons::workload::DurableTaskStore::resolve_profile_scope(&sessions_dir)?;
        let task_store =
            crate::daemons::workload::DurableTaskStore::from_sessions_dir(&sessions_dir)?;
        let task_id = format!("terminal-{}", uuid::Uuid::new_v4());
        let command = command.to_string();
        let mut environment_keys: Vec<_> = environment.keys().cloned().collect();
        environment_keys.sort();
        let prepared =
            task_store.prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: task_id.clone(),
                command: command.clone(),
                cwd: run_cwd.clone(),
                project_root,
                profile_id,
                sessions_dir,
                session_id,
                execution: config.clone(),
                timeout_secs,
                max_output_bytes,
            })?;
        register_prepared_durable_task(&crate::daemons::task::TASK_MANAGER, &task_id, &task_store)?;
        return Ok(json!({
            "status": "started",
            "task_id": task_id,
            "execution_id": prepared.execution_id,
            "command": command,
            "cwd": run_cwd.to_string_lossy(),
            "environment_keys": environment_keys,
        }));
    }

    let start = Instant::now();
    let output_callback =
        sandbox_output_callback(terminal_output_callback.cloned(), invocation_id, None);
    let run = crate::sandbox::run_sandboxed_streaming_with_environment(
        command,
        &run_cwd,
        &config.provider,
        &config.default_profile,
        &config.boundaries,
        environment,
        max_output_bytes,
        output_callback,
    );

    let run_result = timeout(Duration::from_secs(timeout_secs), run).await;
    flush_terminal_output_callback(terminal_output_callback, invocation_id, None);
    let (output, bwrap_args) = run_result
        .map_err(|_| format!("Command timed out after {timeout_secs}s"))?
        .map_err(|e| e.to_string())?;

    let mut environment_keys: Vec<_> = environment.keys().cloned().collect();
    environment_keys.sort();
    let command_error = (!output.status.success()).then(|| {
        format!(
            "command exited with {}\nstdout ({} bytes, {} retained, truncated={}):\n{}\nstderr ({} bytes, {} retained, truncated={}):\n{}",
            output.status.code().unwrap_or(-1),
            output.stdout_bytes,
            output.stdout.len(),
            output.stdout_truncated(),
            String::from_utf8_lossy(&output.stdout).trim(),
            output.stderr_bytes,
            output.stderr.len(),
            output.stderr_truncated(),
            String::from_utf8_lossy(&output.stderr).trim(),
        )
    });
    let mut res = json!({
        "command": command,
        "command_success": output.status.success(),
        "error": command_error,
        "cwd": run_cwd.to_string_lossy(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "stdout_bytes": output.stdout_bytes,
        "stderr_bytes": output.stderr_bytes,
        "stdout_bytes_retained": output.stdout.len(),
        "stderr_bytes_retained": output.stderr.len(),
        "stdout_truncated": output.stdout_truncated(),
        "stderr_truncated": output.stderr_truncated(),
        "max_output_bytes": max_output_bytes,
        "exit_code": output.status.code(),
        "duration": start.elapsed().as_secs_f64(),
        "provider": if bwrap_args.is_some() { "bwrap" } else { "internal" },
        "sandbox_profile": config.default_profile,
        "boundaries": config.boundaries,
        "environment_keys": environment_keys,
    });

    if let Some(args) = bwrap_args {
        res.as_object_mut()
            .unwrap()
            .insert("bwrap_args".to_string(), json!(args));
    }

    Ok(res)
}

pub(crate) fn terminal_output_limit(args: &Value) -> Result<usize, String> {
    bounded_usize_arg(
        args,
        "max_output_bytes",
        DEFAULT_TERMINAL_OUTPUT_BYTES,
        1,
        MAX_TERMINAL_OUTPUT_BYTES,
    )
}

fn sandbox_output_callback(
    callback: Option<TerminalOutputCallback>,
    invocation_id: crate::tools::ToolInvocationId,
    background_task_id: Option<String>,
) -> Option<crate::sandbox::OutputCallback> {
    callback.map(|callback| {
        Arc::new(move |stream, bytes: &[u8]| {
            let stream = match stream {
                crate::sandbox::OutputStream::Stdout => TerminalOutputStream::Stdout,
                crate::sandbox::OutputStream::Stderr => TerminalOutputStream::Stderr,
            };
            callback(TerminalOutputEvent {
                invocation_id,
                tool_name: "run_terminal".to_string(),
                stream,
                chunk: bytes.to_vec(),
                background_task_id: background_task_id.clone(),
                eof: false,
            });
        }) as crate::sandbox::OutputCallback
    })
}

fn flush_terminal_output_callback(
    callback: Option<&TerminalOutputCallback>,
    invocation_id: crate::tools::ToolInvocationId,
    background_task_id: Option<String>,
) {
    let Some(callback) = callback else {
        return;
    };
    for stream in [TerminalOutputStream::Stdout, TerminalOutputStream::Stderr] {
        callback(TerminalOutputEvent {
            invocation_id,
            tool_name: "run_terminal".to_string(),
            stream,
            chunk: Vec::new(),
            background_task_id: background_task_id.clone(),
            eof: true,
        });
    }
}

async fn manage_subagents(args: &Value, cwd: &Path) -> Result<Value, String> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("list");
    match action {
        "list" => Ok(json!({
            "subagents": crate::tools::delegation::list_subagents(cwd)?
        })),
        "get" => {
            let id = required_identifier(args, "subagent_id")?;
            Ok(json!({
                "subagent": crate::tools::delegation::get_subagent_record(cwd, id)?
            }))
        }
        "cancel" | "terminate" => {
            let id = required_identifier(args, "subagent_id")?;
            crate::tools::delegation::cancel_subagent_async(cwd, id).await
        }
        other => Err(format!("unsupported manage_subagents action: {other}")),
    }
}

async fn send_message(args: &Value, cwd: &Path) -> Result<Value, String> {
    crate::tools::delegation::send_message_to_subagent(args, cwd)
}

fn resolve_existing_path(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("invalid tool scope {}: {error}", root.display()))?;
    let requested = PathBuf::from(requested);
    let candidate = if requested.is_absolute() {
        requested
    } else {
        root.join(requested)
    };
    let candidate = candidate
        .canonicalize()
        .map_err(|error| format!("path not found {}: {error}", candidate.display()))?;
    if !candidate.starts_with(&root) {
        return Err(format!(
            "path outside tool scope: {} is not under {}",
            candidate.display(),
            root.display()
        ));
    }
    Ok(candidate)
}

fn path_for_glob(path: &Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn glob_matches(pattern: &str, candidate: &str) -> bool {
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                }
                regex.push_str(".*");
            }
            '?' => regex.push('.'),
            other => regex.push_str(&regex::escape(&other.to_string())),
        }
    }
    regex.push('$');
    Regex::new(&regex)
        .map(|expression| expression.is_match(candidate))
        .unwrap_or(false)
}

async fn search_web(args: &Value, _cwd: &Path) -> Result<Value, String> {
    let query = required_nonempty_string(args, "query")?;
    if query.chars().count() > 500 {
        return Err("search query must be at most 500 characters".to_string());
    }
    let max_results = bounded_usize_arg(args, "max_results", 5, 1, 10)?;
    let mut url = reqwest::Url::parse("https://html.duckduckgo.com/html/")
        .map_err(|error| format!("invalid search endpoint: {error}"))?;
    url.query_pairs_mut().append_pair("q", query);

    let document = fetch_bounded(url).await?;
    ensure_textual_content_type(document.content_type.as_deref())?;
    let html = String::from_utf8_lossy(&document.body);
    let results = parse_search_results(&html, max_results);
    Ok(json!({
        "status": "success",
        "provider": "duckduckgo_html",
        "query": query,
        "count": results.len(),
        "results": results,
    }))
}

async fn read_url_content(args: &Value, _cwd: &Path) -> Result<Value, String> {
    let requested_url = validate_http_url(required_nonempty_string(args, "url")?)?;
    let max_chars = bounded_usize_arg(
        args,
        "max_chars",
        DEFAULT_CONTENT_CHAR_LIMIT,
        1_000,
        MAX_CONTENT_CHAR_LIMIT,
    )?;
    let document = fetch_bounded(requested_url).await?;
    ensure_textual_content_type(document.content_type.as_deref())?;
    let decoded = String::from_utf8_lossy(&document.body);
    let is_html = document
        .content_type
        .as_deref()
        .is_some_and(|content_type| {
            let content_type = content_type.to_ascii_lowercase();
            content_type.contains("text/html") || content_type.contains("application/xhtml+xml")
        })
        || decoded.trim_start().starts_with('<');
    let title = is_html.then(|| extract_page_title(&decoded)).flatten();
    let extracted = if is_html {
        html_to_safe_markdown(&decoded, &document.final_url)
    } else {
        sanitize_text(&strip_dangerous_html_blocks(&decoded))
    };
    let (content, truncated) = truncate_chars(&extracted, max_chars);

    Ok(json!({
        "status": "success",
        "url": document.final_url.as_str(),
        "content_type": document.content_type,
        "title": title,
        "content": content,
        "truncated": truncated,
    }))
}

async fn manage_task(args: &Value, cwd: &Path) -> Result<Value, String> {
    let store = crate::daemons::workload::DurableTaskStore::for_project(cwd)?;
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("list");
    match action {
        "list" => Ok(json!({"tasks": store.list()?})),
        "get" => {
            let id = required_identifier(args, "task_id")?;
            let task = store
                .get(id)?
                .ok_or_else(|| format!("background task not found: {id}"))?;
            Ok(json!({"task": task}))
        }
        "cancel" => {
            let id = required_identifier(args, "task_id")?;
            Ok(json!({
                "task": store.cancel(id)?
            }))
        }
        "reconcile" => {
            let report = store.reconcile(chrono::Utc::now())?;
            Ok(json!({
                "tasks": report.tasks,
                "scanned_records": report.scanned_records,
                "reconciled_records": report.reconciled_records,
                "omitted_records": report.omitted_records,
            }))
        }
        other => Err(format!("unsupported manage_task action: {other}")),
    }
}

async fn manage_memory(args: &Value, cwd: &Path) -> Result<Value, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("manage_memory requires an action")?;
    let namespace = args
        .get("namespace")
        .and_then(Value::as_str)
        .ok_or("manage_memory requires a namespace")?;
    if !matches!(namespace, "environment" | "user") {
        return Err(format!("unsupported memory namespace: {namespace}"));
    }
    if !matches!(action, "list" | "get" | "set" | "delete") {
        return Err(format!("unsupported manage_memory action: {action}"));
    }
    let config = crate::config::load_nib_config_full(cwd).map_err(|error| error.to_string())?;
    let profiles = crate::profile::ProfileRegistry::load(cwd, &config.profiles)
        .map_err(|error| error.to_string())?;
    let profile = profiles
        .for_workspace(cwd)
        .unwrap_or_else(|| profiles.default_profile());
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;
    let store = profile.memory_store();

    match action {
        "list" => {
            let memory = store.load_result()?;
            let values = match namespace {
                "environment" => memory.environment,
                "user" => memory.user,
                _ => return Err(format!("unsupported memory namespace: {namespace}")),
            };
            Ok(json!({
                "action": action,
                "namespace": namespace,
                "values": values,
            }))
        }
        "get" => {
            let key = required_memory_key(args)?;
            let value = match namespace {
                "environment" => store.environment_result(key)?,
                "user" => store.user_result(key)?,
                _ => return Err(format!("unsupported memory namespace: {namespace}")),
            };
            Ok(json!({
                "action": action,
                "namespace": namespace,
                "key": key,
                "found": value.is_some(),
                "value": value,
            }))
        }
        "set" => {
            let key = required_memory_key(args)?;
            let value = args
                .get("value")
                .and_then(Value::as_str)
                .ok_or("manage_memory set requires a value")?;
            if value.chars().count() > 65_536 {
                return Err("manage_memory value must be at most 65536 characters".to_string());
            }
            match namespace {
                "environment" => store.set_environment(key, value)?,
                "user" => store.set_user(key, value)?,
                _ => return Err(format!("unsupported memory namespace: {namespace}")),
            }
            Ok(json!({
                "action": action,
                "namespace": namespace,
                "key": key,
                "updated": true,
            }))
        }
        "delete" => {
            let key = required_memory_key(args)?;
            let removed = match namespace {
                "environment" => store.remove_environment(key)?,
                "user" => store.remove_user(key)?,
                _ => return Err(format!("unsupported memory namespace: {namespace}")),
            };
            Ok(json!({
                "action": action,
                "namespace": namespace,
                "key": key,
                "removed": removed.is_some(),
            }))
        }
        _ => Err(format!("unsupported manage_memory action: {action}")),
    }
}

fn required_memory_key(args: &Value) -> Result<&str, String> {
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| "manage_memory action requires a non-empty key".to_string())?;
    if key.chars().count() > 256 {
        return Err("manage_memory key must be at most 256 characters".to_string());
    }
    Ok(key)
}

async fn schedule(args: &Value, _cwd: &Path) -> Result<Value, String> {
    if std::env::var_os("NIB_MANAGED_PROCESS_SCOPE").is_some() {
        return Err(
            "durable schedules cannot be launched from a foreground managed-process scope"
                .to_string(),
        );
    }
    let prompt = required_nonempty_string(args, "prompt")?.to_string();
    if prompt.chars().count() > 20_000 {
        return Err("scheduled prompt must be at most 20000 characters".to_string());
    }
    let duration_secs = bounded_u64_arg(args, "duration_secs", None, 1, 31_536_000)?;
    let interval_secs = bounded_u64_arg(args, "interval_secs", Some(duration_secs), 1, 31_536_000)?;
    let repeat_count = bounded_u64_arg(args, "repeat_count", Some(1), 1, 100)? as u32;
    let session_id = required_identifier(args, "_session_id")?.to_string();
    let sessions_dir = PathBuf::from(required_nonempty_string(args, "_sessions_dir")?);
    if !sessions_dir.is_dir() {
        return Err(format!(
            "originating session directory is unavailable: {}",
            sessions_dir.display()
        ));
    }
    let store = crate::session::SessionStore::at_dir(sessions_dir.clone());
    if store
        .load_result(&session_id)
        .map_err(|error| format!("failed to load originating session: {error}"))?
        .is_none()
    {
        return Err(format!("originating session not found: {session_id}"));
    }
    let audit_path = sessions_dir
        .parent()
        .ok_or("originating session directory has no profile state parent")?
        .join("daemons")
        .join("audit.jsonl");
    let audit_log = crate::daemons::task::DaemonAuditLog::at_path(audit_path);
    let (project_root, profile_id) =
        crate::daemons::workload::DurableTaskStore::resolve_profile_scope(&sessions_dir)?;
    let task_store = crate::daemons::workload::DurableTaskStore::from_sessions_dir(&sessions_dir)?;
    let id = format!("timer-{}", uuid::Uuid::new_v4());
    let preparation =
        task_store.prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
            id: id.clone(),
            initial_delay: Duration::from_secs(duration_secs),
            interval: Duration::from_secs(interval_secs),
            repeat_count,
            prompt: prompt.clone(),
            project_root,
            profile_id,
            sessions_dir,
            session_id: session_id.clone(),
        });
    let prepared = match preparation {
        Ok(prepared) => prepared,
        Err(error) => {
            let audit_error = record_schedule_failure(
                &store,
                &audit_log,
                &session_id,
                &id,
                duration_secs,
                interval_secs,
                repeat_count,
                &error,
            )
            .err();
            return Err(match audit_error {
                Some(audit_error) => {
                    format!("{error}; failed to record schedule admission failure: {audit_error}")
                }
                None => error,
            });
        }
    };
    register_and_audit_prepared_schedule(
        &crate::daemons::task::TASK_MANAGER,
        &task_store,
        &store,
        &audit_log,
        &id,
        &session_id,
        duration_secs,
        interval_secs,
        repeat_count,
        &prompt,
    )?;
    Ok(json!({
        "status": "prepared",
        "task_id": id,
        "execution_id": prepared.execution_id,
        "session_id": session_id,
        "duration_secs": duration_secs,
        "interval_secs": interval_secs,
        "repeat_count": repeat_count,
    }))
}

fn register_prepared_durable_task(
    manager: &crate::daemons::task::TaskManager,
    id: &str,
    store: &crate::daemons::workload::DurableTaskStore,
) -> Result<(), String> {
    manager.register_durable_task(id.to_string(), store.clone())
}

#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn register_and_audit_prepared_schedule(
    manager: &crate::daemons::task::TaskManager,
    task_store: &crate::daemons::workload::DurableTaskStore,
    session_store: &crate::session::SessionStore,
    audit_log: &crate::daemons::task::DaemonAuditLog,
    id: &str,
    session_id: &str,
    duration_secs: u64,
    interval_secs: u64,
    repeat_count: u32,
    prompt: &str,
) -> Result<(), String> {
    let execution_id = task_store
        .get(id)?
        .ok_or_else(|| format!("prepared schedule record is missing: {id}"))?
        .execution_id;
    if let Err(error) = register_prepared_durable_task(manager, id, task_store) {
        let audit_error = record_schedule_failure(
            session_store,
            audit_log,
            session_id,
            id,
            duration_secs,
            interval_secs,
            repeat_count,
            &error,
        )
        .err();
        return Err(match audit_error {
            Some(audit_error) => {
                format!("{error}; failed to record schedule admission failure: {audit_error}")
            }
            None => error,
        });
    }

    let mut session_success_recorded = false;
    let audit_result = (|| {
        session_store
            .record_event(
                session_id,
                "timer_scheduled",
                json!({
                    "timer_id": id,
                    "execution_id": execution_id,
                    "duration_secs": duration_secs,
                    "interval_secs": interval_secs,
                    "repeat_count": repeat_count,
                    "prompt": prompt,
                }),
            )
            .map_err(|error| format!("failed to audit timer schedule in session: {error}"))?;
        session_success_recorded = true;
        audit_log.append(&crate::daemons::task::DaemonAuditRecord {
            timestamp: chrono::Utc::now(),
            daemon: "timer".to_string(),
            action: "schedule".to_string(),
            target: Some(session_id.to_string()),
            outcome: "scheduled".to_string(),
            authorized: true,
            detail: Some(format!(
                "timer_id={id}; execution_id={execution_id}; repeat_count={repeat_count}; interval_secs={interval_secs}"
            )),
        })
    })();
    if let Err(error) = audit_result {
        let rollback_error = manager.rollback_prepared_durable_task(id).err();
        let terminalization_result = rollback_error.as_ref().map(|_| {
            manager
                .fail_prepared_durable_task(id, format!("schedule admission audit failed: {error}"))
        });
        let session_compensation_error = if session_success_recorded {
            remove_schedule_success_event(session_store, session_id, id, &execution_id).err()
        } else {
            None
        };
        let mut error = error;
        if let Some(rollback_error) = rollback_error {
            error.push_str(&format!(
                "; failed to roll back admitted schedule: {rollback_error}"
            ));
        }
        if let Some(terminalization_result) = terminalization_result {
            match terminalization_result {
                Ok(_) => error.push_str("; admitted schedule was marked failed"),
                Err(terminalization_error) => error.push_str(&format!(
                    "; failed to terminalize admitted schedule: {terminalization_error}"
                )),
            }
        }
        if let Some(session_compensation_error) = session_compensation_error {
            error.push_str(&format!(
                "; failed to compensate session schedule audit: {session_compensation_error}"
            ));
        }
        let failure_audit_error = record_schedule_failure(
            session_store,
            audit_log,
            session_id,
            id,
            duration_secs,
            interval_secs,
            repeat_count,
            &error,
        )
        .err();
        if let Some(failure_audit_error) = failure_audit_error {
            error.push_str(&format!(
                "; failed to record schedule admission failure: {failure_audit_error}"
            ));
        }
        return Err(error);
    }
    Ok(())
}

fn remove_schedule_success_event(
    session_store: &crate::session::SessionStore,
    session_id: &str,
    id: &str,
    execution_id: &str,
) -> Result<(), String> {
    let removed = session_store
        .update_session(session_id, |session| {
            let position = session.events.iter().rposition(|event| {
                event.kind == "timer_scheduled"
                    && event.details.get("timer_id").and_then(Value::as_str) == Some(id)
                    && event.details.get("execution_id").and_then(Value::as_str)
                        == Some(execution_id)
            });
            let Some(position) = position else {
                return Ok(false);
            };
            session.events.remove(position);
            for (index, event) in session.events.iter_mut().enumerate() {
                event.index = index;
            }
            Ok(true)
        })
        .map_err(|error| format!("failed to remove timer_scheduled event: {error}"))?;
    if !removed {
        return Err(format!(
            "timer_scheduled event was missing during compensation: {id}"
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_schedule_failure(
    session_store: &crate::session::SessionStore,
    audit_log: &crate::daemons::task::DaemonAuditLog,
    session_id: &str,
    id: &str,
    duration_secs: u64,
    interval_secs: u64,
    repeat_count: u32,
    error: &str,
) -> Result<(), String> {
    let session_error = session_store
        .record_event(
            session_id,
            "timer_schedule_failed",
            json!({
                "timer_id": id,
                "duration_secs": duration_secs,
                "interval_secs": interval_secs,
                "repeat_count": repeat_count,
                "error": error,
            }),
        )
        .err();
    let daemon_error = audit_log
        .append(&crate::daemons::task::DaemonAuditRecord {
            timestamp: chrono::Utc::now(),
            daemon: "timer".to_string(),
            action: "schedule".to_string(),
            target: Some(session_id.to_string()),
            outcome: "failed".to_string(),
            authorized: true,
            detail: Some(format!("timer_id={id}; admission_error={error}")),
        })
        .err();
    match (session_error, daemon_error) {
        (None, None) => Ok(()),
        (Some(error), None) => Err(format!("session audit failed: {error}")),
        (None, Some(error)) => Err(format!("daemon audit failed: {error}")),
        (Some(session_error), Some(daemon_error)) => Err(format!(
            "session audit failed: {session_error}; daemon audit failed: {daemon_error}"
        )),
    }
}

async fn ask_question(args: &Value, _cwd: &Path) -> Result<Value, String> {
    let mut public = args.clone();
    if let Some(object) = public.as_object_mut() {
        for key in ["answer", "answer_error", "_question_outcome"] {
            object.remove(key);
        }
    }
    let form = crate::interactive::parse_question_form(&public)?;
    if let Some(outcome) = args.get("_question_outcome") {
        let outcome =
            serde_json::from_value::<crate::interactive::QuestionFormOutcome>(outcome.clone())
                .map_err(|_| "invalid internal question outcome".to_string())?;
        return form.observation(&outcome);
    }
    if let Some(error) = args.get("answer_error").and_then(Value::as_str) {
        return Err(format!("question could not be answered: {error}"));
    }
    match args.get("answer") {
        Some(Value::String(answer)) if !answer.trim().is_empty() && form.questions.len() == 1 => {
            let question = &form.questions[0];
            let source = if question.proposed_answer.is_none()
                && question
                    .options
                    .iter()
                    .any(|option| option.label == *answer)
            {
                crate::interactive::QuestionAnswerSource::Option
            } else {
                crate::interactive::QuestionAnswerSource::Text
            };
            form.observation(&crate::interactive::QuestionFormOutcome::Answered(vec![
                crate::interactive::QuestionAnswer {
                    answer: answer.clone(),
                    source,
                },
            ]))
        }
        Some(_) => {
            Err("ask_question answer must be a non-empty string for one question".to_string())
        }
        None => {
            let mut value = form.tool_arguments();
            value["status"] = json!("pending_ui");
            Ok(value)
        }
    }
}

#[path = "core_http.rs"]
mod core_http;
use core_http::*;

#[cfg(test)]
#[path = "core_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "git_status_tests.rs"]
mod git_status_tests;
