//! Core tool implementations (called only after ToolExecutor gates pass).

use crate::config::ExecutionConfig;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HTTP_RESPONSE_BYTES: usize = 1_048_576;
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
const MAX_HTTP_REDIRECTS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutputEvent {
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
        "apply_patch" => apply_patch(args, cwd).await,
        "run_terminal" => {
            run_terminal(
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
        other => Err(format!("No implementation for tool: {other}")),
    }
}

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
        if glob.is_some_and(|pattern| !glob_matches(pattern, &relative.to_string_lossy())) {
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

async fn run_terminal(
    args: &Value,
    cwd: &Path,
    config: &ExecutionConfig,
    terminal_backend: &str,
    terminal_timeout_secs: u64,
    environment: &HashMap<String, String>,
    terminal_output_callback: Option<&TerminalOutputCallback>,
) -> Result<Value, String> {
    crate::sandbox::trace_terminal_startup("core.run_terminal.enter");
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
    let max_output_bytes = bounded_usize_arg(
        args,
        "max_output_bytes",
        DEFAULT_TERMINAL_OUTPUT_BYTES,
        1,
        MAX_TERMINAL_OUTPUT_BYTES,
    )?;
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
    let output_callback = sandbox_output_callback(terminal_output_callback.cloned(), None);
    crate::sandbox::trace_terminal_startup("core.sandbox.enter");
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
    flush_terminal_output_callback(terminal_output_callback, None);
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

fn sandbox_output_callback(
    callback: Option<TerminalOutputCallback>,
    background_task_id: Option<String>,
) -> Option<crate::sandbox::OutputCallback> {
    callback.map(|callback| {
        Arc::new(move |stream, bytes: &[u8]| {
            let stream = match stream {
                crate::sandbox::OutputStream::Stdout => TerminalOutputStream::Stdout,
                crate::sandbox::OutputStream::Stderr => TerminalOutputStream::Stderr,
            };
            callback(TerminalOutputEvent {
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
    background_task_id: Option<String>,
) {
    let Some(callback) = callback else {
        return;
    };
    for stream in [TerminalOutputStream::Stdout, TerminalOutputStream::Stderr] {
        callback(TerminalOutputEvent {
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
            crate::tools::delegation::cancel_subagent(cwd, id)
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
    let question = required_nonempty_string(args, "question")?;
    let options = match args.get("options") {
        None => Vec::new(),
        Some(Value::Array(options)) if options.len() <= 20 => options
            .iter()
            .map(|option| {
                option
                    .as_str()
                    .filter(|option| !option.trim().is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| "ask_question options must be non-empty strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(Value::Array(_)) => return Err("ask_question accepts at most 20 options".to_string()),
        Some(_) => return Err("ask_question options must be an array".to_string()),
    };
    if let Some(error) = args.get("answer_error").and_then(Value::as_str) {
        return Err(format!("question could not be answered: {error}"));
    }
    match args.get("answer") {
        Some(Value::String(answer)) if !answer.trim().is_empty() => Ok(json!({
            "status": "answered",
            "question": question,
            "options": options,
            "answer": answer,
        })),
        Some(_) => Err("ask_question answer must be a non-empty string".to_string()),
        None => Ok(json!({
            "status": "pending_ui",
            "question": question,
            "options": options,
        })),
    }
}

struct HttpDocument {
    final_url: reqwest::Url,
    content_type: Option<String>,
    body: Vec<u8>,
}

async fn fetch_bounded(url: reqwest::Url) -> Result<HttpDocument, String> {
    fetch_bounded_with_resolver(url, &SystemHostResolver).await
}

#[async_trait::async_trait]
trait HostResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String>;
}

struct SystemHostResolver;

#[async_trait::async_trait]
impl HostResolver for SystemHostResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let host = host.to_string();
        tokio::task::spawn_blocking(move || {
            (host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect())
                .map_err(|error| format!("DNS resolution failed for {host}: {error}"))
        })
        .await
        .map_err(|error| format!("DNS resolver task failed: {error}"))?
    }
}

struct ResolvedDestination {
    host: String,
    addresses: Vec<SocketAddr>,
}

async fn resolve_public_destination<R: HostResolver + ?Sized>(
    url: &reqwest::Url,
    resolver: &R,
) -> Result<ResolvedDestination, String> {
    let host = url.host_str().ok_or("URL must include a host")?;
    let host = host
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let port = url
        .port_or_known_default()
        .ok_or("URL has no known destination port")?;
    let mut addresses = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, port)]
    } else {
        resolver.resolve(&host, port).await?
    };
    if addresses.is_empty() {
        return Err(format!("DNS resolution returned no addresses for {host}"));
    }
    for address in &mut addresses {
        address.set_port(port);
        if ip_is_non_public(address.ip()) {
            return Err(format!(
                "resolved URL host {host} to non-public address {}",
                address.ip()
            ));
        }
    }
    addresses.sort_unstable();
    addresses.dedup();
    Ok(ResolvedDestination { host, addresses })
}

async fn fetch_bounded_with_resolver<R: HostResolver + ?Sized>(
    url: reqwest::Url,
    resolver: &R,
) -> Result<HttpDocument, String> {
    timeout(HTTP_TIMEOUT, async move {
        let mut current_url = validate_http_url(url.as_str())?;
        for redirect_count in 0..=MAX_HTTP_REDIRECTS {
            let destination = resolve_public_destination(&current_url, resolver).await?;
            let client = reqwest::Client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .timeout(HTTP_TIMEOUT)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .resolve_to_addrs(&destination.host, &destination.addresses)
                .user_agent("nib/0.1 (+https://github.com/skills-yaml/nib)")
                .build()
                .map_err(|error| format!("failed to create HTTP client: {error}"))?;
            let response = client
                .get(current_url.clone())
                .header(
                    "accept",
                    "text/html, application/xhtml+xml, text/markdown, text/plain;q=0.9",
                )
                .send()
                .await
                .map_err(|error| format!("HTTP request failed: {error}"))?;
            let status = response.status();
            if status.is_redirection() {
                if redirect_count >= MAX_HTTP_REDIRECTS {
                    return Err(format!(
                        "HTTP redirect limit exceeded ({MAX_HTTP_REDIRECTS})"
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .ok_or_else(|| format!("HTTP redirect status {status} omitted Location"))?
                    .to_str()
                    .map_err(|error| format!("HTTP redirect Location is invalid: {error}"))?;
                current_url = validated_redirect_target(&current_url, location)?;
                continue;
            }
            if !status.is_success() {
                return Err(format!("HTTP request returned status {status}"));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_HTTP_RESPONSE_BYTES as u64)
            {
                return Err(format!(
                    "HTTP response exceeds {} byte limit",
                    MAX_HTTP_RESPONSE_BYTES
                ));
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|error| format!("failed to read HTTP response: {error}"))?;
                if body.len().saturating_add(chunk.len()) > MAX_HTTP_RESPONSE_BYTES {
                    return Err(format!(
                        "HTTP response exceeds {} byte limit",
                        MAX_HTTP_RESPONSE_BYTES
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(HttpDocument {
                final_url: current_url,
                content_type,
                body,
            });
        }
        Err(format!(
            "HTTP redirect limit exceeded ({MAX_HTTP_REDIRECTS})"
        ))
    })
    .await
    .map_err(|_| format!("HTTP request timed out after {}s", HTTP_TIMEOUT.as_secs()))?
}

fn validated_redirect_target(
    current_url: &reqwest::Url,
    location: &str,
) -> Result<reqwest::Url, String> {
    let redirect_url = current_url
        .join(location)
        .map_err(|error| format!("invalid HTTP redirect target: {error}"))?;
    validate_http_url(redirect_url.as_str())
        .map_err(|error| format!("HTTP redirect target rejected: {error}"))
}

fn validate_http_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL scheme must be http or https".to_string());
    }
    let host = url.host_str().ok_or("URL must include a host")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL credentials are not allowed".to_string());
    }
    let normalized_host = host
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if normalized_host == "localhost" || normalized_host.ends_with(".localhost") {
        return Err("URL host is not allowed: localhost".to_string());
    }
    if let Ok(address) = normalized_host.parse::<IpAddr>() {
        if ip_is_non_public(address) {
            return Err(format!("URL host is not publicly routable: {address}"));
        }
    }
    Ok(url)
}

fn ip_is_non_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_is_non_public(address),
        IpAddr::V6(address) => ipv6_is_non_public(address),
    }
}

fn ipv4_is_non_public(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    first == 0
        || first == 10
        || first == 127
        || (first == 100 && (64..=127).contains(&second))
        || (first == 169 && second == 254)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 0 && third == 0)
        || (first == 192 && second == 0 && third == 2)
        || (first == 192 && second == 88 && third == 99)
        || (first == 192 && second == 168)
        || (first == 198 && matches!(second, 18 | 19))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        || first >= 224
}

fn ipv6_is_non_public(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    let unique_local = octets[0] & 0xfe == 0xfc;
    let link_local = octets[0] == 0xfe && octets[1] & 0xc0 == 0x80;
    let site_local = octets[0] == 0xfe && octets[1] & 0xc0 == 0xc0;
    let documentation = octets[..4] == [0x20, 0x01, 0x0d, 0xb8];
    let discard_only = octets[..8] == [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let transition_protocol = octets[..4] == [0x20, 0x01, 0x00, 0x00]
        || octets[..2] == [0x20, 0x02]
        || octets[..4] == [0x20, 0x01, 0x00, 0x10]
        || octets[..4] == [0x20, 0x01, 0x00, 0x20];
    let nat64_ipv4 = if octets[..12]
        == [
            0x00, 0x64, 0xff, 0x9b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]
        || octets[..6] == [0x00, 0x64, 0xff, 0x9b, 0x00, 0x01]
    {
        Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ))
    } else {
        None
    };
    let embedded_ipv4 = if octets[..10] == [0; 10]
        && (octets[10..12] == [0xff, 0xff] || octets[10..12] == [0, 0])
    {
        Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ))
    } else {
        None
    };
    address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || unique_local
        || link_local
        || site_local
        || documentation
        || discard_only
        || transition_protocol
        || nat64_ipv4.is_some_and(ipv4_is_non_public)
        || embedded_ipv4.is_some_and(ipv4_is_non_public)
}

fn ensure_textual_content_type(content_type: Option<&str>) -> Result<(), String> {
    let Some(content_type) = content_type else {
        return Ok(());
    };
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if media_type.starts_with("text/") || media_type == "application/xhtml+xml" {
        Ok(())
    } else {
        Err(format!("unsupported URL content type: {media_type}"))
    }
}

fn parse_search_results(html: &str, max_results: usize) -> Vec<Value> {
    let snippets: Vec<_> = SEARCH_SNIPPET_RE
        .captures_iter(html)
        .filter_map(|captures| captures.name("body"))
        .map(|body| html_fragment_text(body.as_str()))
        .collect();
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();
    for captures in HTML_ANCHOR_RE.captures_iter(html) {
        let Some(attributes) = captures.name("attrs").map(|value| value.as_str()) else {
            continue;
        };
        if !attribute_has_class(attributes, "result__a") {
            continue;
        }
        let Some(href) = quoted_attribute(attributes, &HREF_ATTRIBUTE_RE) else {
            continue;
        };
        let Some(url) = normalize_search_result_url(href) else {
            continue;
        };
        if !seen_urls.insert(url.clone()) {
            continue;
        }
        let title = captures
            .name("body")
            .map(|body| html_fragment_text(body.as_str()))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let snippet = snippets.get(results.len()).cloned().unwrap_or_default();
        results.push(json!({"title": title, "url": url, "snippet": snippet}));
        if results.len() >= max_results {
            break;
        }
    }
    results
}

fn normalize_search_result_url(raw: &str) -> Option<String> {
    let decoded = decode_html_entities(raw);
    let absolute = if decoded.starts_with("//") {
        format!("https:{decoded}")
    } else {
        decoded
    };
    let url = validate_http_url(&absolute).ok()?;
    if url
        .host_str()
        .is_some_and(|host| host.ends_with("duckduckgo.com"))
    {
        if let Some((_, target)) = url.query_pairs().find(|(key, _)| key == "uddg") {
            return validate_http_url(&target).ok().map(|url| url.to_string());
        }
    }
    Some(url.to_string())
}

fn html_to_safe_markdown(html: &str, base_url: &reqwest::Url) -> String {
    let without_dangerous = strip_dangerous_html_blocks(html);
    let without_comments = HTML_COMMENT_RE.replace_all(&without_dangerous, "");
    let with_headings =
        HTML_HEADING_RE.replace_all(&without_comments, |captures: &regex::Captures| {
            let level = captures
                .get(1)
                .and_then(|value| value.as_str().parse::<usize>().ok())
                .unwrap_or(1);
            let text = captures
                .get(2)
                .map(|value| html_fragment_text(value.as_str()))
                .unwrap_or_default();
            format!("\n{} {}\n", "#".repeat(level), text)
        });
    let with_links = HTML_ANCHOR_RE.replace_all(&with_headings, |captures: &regex::Captures| {
        let text = captures
            .name("body")
            .map(|value| html_fragment_text(value.as_str()))
            .unwrap_or_default();
        let href = captures
            .name("attrs")
            .and_then(|attrs| quoted_attribute(attrs.as_str(), &HREF_ATTRIBUTE_RE));
        match href.and_then(|href| safe_link_target(href, base_url)) {
            Some(target) if !text.is_empty() => {
                format!("[{}]({target})", escape_markdown_text(&text))
            }
            _ => text,
        }
    });
    let with_list_items =
        HTML_LIST_ITEM_RE.replace_all(&with_links, |captures: &regex::Captures| {
            let text = captures
                .get(1)
                .map(|value| html_fragment_text(value.as_str()))
                .unwrap_or_default();
            format!("\n- {text}\n")
        });
    let with_breaks = HTML_BREAK_RE.replace_all(&with_list_items, "\n");
    let with_blocks = HTML_BLOCK_RE.replace_all(&with_breaks, "\n");
    let without_tags = HTML_TAG_RE.replace_all(&with_blocks, "");
    let normalized =
        normalize_markdown_whitespace(&sanitize_text(&decode_html_entities(&without_tags)));
    normalized.replace('<', "&lt;").replace('>', "&gt;")
}

fn strip_dangerous_html_blocks(input: &str) -> String {
    DANGEROUS_HTML_RE.replace_all(input, "").into_owned()
}

fn safe_link_target(raw: &str, base_url: &reqwest::Url) -> Option<String> {
    let decoded = decode_html_entities(raw);
    let target = reqwest::Url::parse(&decoded)
        .or_else(|_| base_url.join(&decoded))
        .ok()?;
    validate_http_url(target.as_str())
        .ok()
        .map(|url| url.to_string())
}

fn extract_page_title(html: &str) -> Option<String> {
    HTML_TITLE_RE
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|title| html_fragment_text(title.as_str()))
        .filter(|title| !title.is_empty())
}

fn html_fragment_text(fragment: &str) -> String {
    let without_tags = HTML_TAG_RE.replace_all(fragment, " ");
    normalize_inline_whitespace(&sanitize_text(&decode_html_entities(&without_tags)))
}

fn attribute_has_class(attributes: &str, expected: &str) -> bool {
    quoted_attribute(attributes, &CLASS_ATTRIBUTE_RE).is_some_and(|classes| {
        classes
            .split_ascii_whitespace()
            .any(|class_name| class_name == expected)
    })
}

fn quoted_attribute<'a>(attributes: &'a str, expression: &Regex) -> Option<&'a str> {
    let captures = expression.captures(attributes)?;
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|value| value.as_str())
}

fn decode_html_entities(input: &str) -> String {
    HTML_ENTITY_RE
        .replace_all(input, |captures: &regex::Captures| {
            let entity = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            match entity {
                "amp" => "&".to_string(),
                "lt" => "<".to_string(),
                "gt" => ">".to_string(),
                "quot" => "\"".to_string(),
                "apos" => "'".to_string(),
                "nbsp" => " ".to_string(),
                numeric if numeric.starts_with("#x") || numeric.starts_with("#X") => {
                    u32::from_str_radix(&numeric[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map(|character| character.to_string())
                        .unwrap_or_default()
                }
                numeric if numeric.starts_with('#') => numeric[1..]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|character| character.to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            }
        })
        .into_owned()
}

fn sanitize_text(input: &str) -> String {
    input
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect()
}

fn normalize_inline_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_markdown_whitespace(input: &str) -> String {
    let mut lines = Vec::new();
    let mut previous_blank = true;
    for raw_line in input.lines() {
        let line = normalize_inline_whitespace(raw_line);
        if line.is_empty() {
            if !previous_blank {
                lines.push(String::new());
            }
            previous_blank = true;
        } else {
            lines.push(line);
            previous_blank = false;
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn escape_markdown_text(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn truncate_chars(input: &str, limit: usize) -> (String, bool) {
    if input.chars().count() <= limit {
        return (input.to_string(), false);
    }
    (input.chars().take(limit).collect(), true)
}

fn required_nonempty_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing or empty {key}"))
}

fn required_identifier<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    let value = required_nonempty_string(args, key)?;
    if value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(format!("invalid {key}"));
    }
    Ok(value)
}

fn bounded_usize_arg(
    args: &Value,
    key: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, String> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| format!("{key} must be an unsigned integer"))?,
    };
    if !(minimum..=maximum).contains(&value) {
        return Err(format!("{key} must be between {minimum} and {maximum}"));
    }
    Ok(value)
}

fn bounded_u64_arg(
    args: &Value,
    key: &str,
    default: Option<u64>,
    minimum: u64,
    maximum: u64,
) -> Result<u64, String> {
    let value = match args.get(key) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an unsigned integer"))?,
        None => default.ok_or_else(|| format!("missing {key}"))?,
    };
    if !(minimum..=maximum).contains(&value) {
        return Err(format!("{key} must be between {minimum} and {maximum}"));
    }
    Ok(value)
}

static HTML_ANCHOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a\b(?P<attrs>[^>]*)>(?P<body>.*?)</a\s*>"#).expect("anchor regex")
});
static SEARCH_SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<(?:a|div)\b(?P<attrs>[^>]*\bclass\s*=\s*(?:\"[^\"]*result__snippet[^\"]*\"|'[^']*result__snippet[^']*')[^>]*)>(?P<body>.*?)</(?:a|div)\s*>"#,
    )
    .expect("search snippet regex")
});
static CLASS_ATTRIBUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bclass\s*=\s*(?:\"([^\"]*)\"|'([^']*)')"#).expect("class regex")
});
static HREF_ATTRIBUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bhref\s*=\s*(?:\"([^\"]*)\"|'([^']*)')"#).expect("href regex")
});
static HTML_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<!--.*?-->").expect("comment regex"));
static DANGEROUS_HTML_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>|<noscript\b[^>]*>.*?</noscript\s*>|<iframe\b[^>]*>.*?</iframe\s*>|<object\b[^>]*>.*?</object\s*>|<svg\b[^>]*>.*?</svg\s*>|<template\b[^>]*>.*?</template\s*>",
    )
    .expect("dangerous HTML regex")
});
static HTML_HEADING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]\s*>").expect("heading regex")
});
static HTML_LIST_ITEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<li\b[^>]*>(.*?)</li\s*>").expect("list item regex"));
static HTML_BREAK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<br\b[^>]*>").expect("break regex"));
static HTML_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)</?(?:article|aside|blockquote|div|dl|dt|dd|footer|header|hr|main|nav|ol|p|pre|section|table|tbody|td|th|thead|tr|ul)\b[^>]*>",
    )
    .expect("block regex")
});
static HTML_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").expect("tag regex"));
static HTML_TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>").expect("title regex"));
static HTML_ENTITY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(#(?:x|X)[0-9a-fA-F]+|#[0-9]+|amp|lt|gt|quot|apos|nbsp);").expect("entity regex")
});

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvironmentGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvironmentGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    struct StaticResolver {
        addresses: HashMap<String, Vec<SocketAddr>>,
    }

    #[async_trait::async_trait]
    impl HostResolver for StaticResolver {
        async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<SocketAddr>, String> {
            self.addresses
                .get(host)
                .cloned()
                .ok_or_else(|| format!("missing fixture resolution for {host}"))
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn managed_foreground_scope_rejects_independently_owned_background_work() {
        let _environment = EnvironmentGuard::set("NIB_MANAGED_PROCESS_SCOPE", "sub-fixture");
        let root = tempfile::tempdir().expect("project root");
        let terminal_error = run_terminal(
            &json!({"command": "pwd", "background": true}),
            root.path(),
            &ExecutionConfig::default(),
            "local",
            10,
            &HashMap::new(),
            None,
        )
        .await
        .expect_err("background terminal must be rejected");
        assert!(terminal_error.contains("foreground managed-process scope"));

        let schedule_error = schedule(
            &json!({"prompt": "later", "duration_secs": 10}),
            root.path(),
        )
        .await
        .expect_err("schedule must be rejected");
        assert!(schedule_error.contains("foreground managed-process scope"));

        let subagent_error =
            crate::tools::delegation::spawn_subagent(&json!({"prompt": "nested"}), root.path())
                .expect_err("nested subagent must be rejected");
        assert!(subagent_error.contains("foreground managed-process scope"));
    }

    #[tokio::test]
    async fn read_file_bounds_bytes_without_loading_the_whole_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("large.txt");
        std::fs::write(&path, "0123456789abcdef\n".repeat(10_000)).expect("large fixture");

        let result = read_file(
            &json!({"path": "large.txt", "max_bytes": 32, "max_lines": 100}),
            directory.path(),
        )
        .await
        .expect("bounded read");

        assert!(result["content"].as_str().unwrap().len() <= 32);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["truncated_by_bytes"], true);
        assert_eq!(result["truncated_by_lines"], false);
        assert_eq!(result["total_lines_known"], false);
        assert!(
            result["bytes_scanned"].as_u64().unwrap() < result["total_bytes"].as_u64().unwrap()
        );
    }

    #[tokio::test]
    async fn read_file_reports_line_range_truncation_separately() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            directory.path().join("lines.txt"),
            "zero\none\ntwo\nthree\n",
        )
        .expect("line fixture");

        let result = read_file(
            &json!({"path": "lines.txt", "start_line": 1, "max_lines": 2}),
            directory.path(),
        )
        .await
        .expect("line bounded read");

        assert_eq!(result["content"], "one\ntwo");
        assert_eq!(result["start_line"], 1);
        assert_eq!(result["end_line"], 3);
        assert_eq!(result["total_lines"], 4);
        assert_eq!(result["truncated_by_bytes"], false);
        assert_eq!(result["truncated_by_lines"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_retains_bounded_tails_and_emits_progress_chunks() {
        let directory = tempfile::tempdir().expect("tempdir");
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let callback_events = Arc::clone(&events);
        let callback: TerminalOutputCallback = Arc::new(move |event| {
            callback_events.lock().unwrap().push(event);
        });
        let config = ExecutionConfig {
            provider: "internal".to_string(),
            default_profile: "internal".to_string(),
            ..ExecutionConfig::default()
        };

        let result = run_terminal(
            &json!({
                "command": "head -c 4096 /dev/zero | tr '\\0' x; printf 'STDOUT_TAIL'; head -c 4096 /dev/zero | tr '\\0' y >&2; printf 'STDERR_TAIL' >&2",
                "max_output_bytes": 64
            }),
            directory.path(),
            &config,
            "local",
            10,
            &HashMap::new(),
            Some(&callback),
        )
        .await
        .expect("terminal result");

        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], true);
        assert_eq!(result["stdout_bytes_retained"], 64);
        assert_eq!(result["stderr_bytes_retained"], 64);
        assert!(result["stdout"].as_str().unwrap().ends_with("STDOUT_TAIL"));
        assert!(result["stderr"].as_str().unwrap().ends_with("STDERR_TAIL"));
        {
            let events = events.lock().unwrap();
            assert!(events
                .iter()
                .any(|event| event.stream == TerminalOutputStream::Stdout));
            assert!(events
                .iter()
                .any(|event| event.stream == TerminalOutputStream::Stderr));
        }

        let failed = run_terminal(
            &json!({
                "command": "head -c 4096 /dev/zero | tr '\\0' z >&2; printf 'FAILED_TAIL' >&2; exit 7",
                "max_output_bytes": 64
            }),
            directory.path(),
            &config,
            "local",
            10,
            &HashMap::new(),
            None,
        )
        .await
        .expect("structured terminal failure");
        assert_eq!(failed["command_success"], false);
        assert_eq!(failed["exit_code"], 7);
        assert_eq!(failed["stderr_truncated"], true);
        assert!(failed["stderr"].as_str().unwrap().ends_with("FAILED_TAIL"));
        assert!(failed["error"]
            .as_str()
            .unwrap()
            .contains("command exited with 7"));
    }

    #[tokio::test]
    async fn profile_memory_tool_persists_lists_and_removes_namespaces() {
        let directory = tempfile::tempdir().expect("tempdir");

        let set_environment = manage_memory(
            &json!({
                "action": "set",
                "namespace": "environment",
                "key": "test_command",
                "value": "task test"
            }),
            directory.path(),
        )
        .await
        .expect("set environment memory");
        assert_eq!(set_environment["updated"], true);
        manage_memory(
            &json!({
                "action": "set",
                "namespace": "user",
                "key": "response_style",
                "value": "concise"
            }),
            directory.path(),
        )
        .await
        .expect("set user memory");

        let listed = manage_memory(
            &json!({"action": "list", "namespace": "environment"}),
            directory.path(),
        )
        .await
        .expect("list environment memory");
        assert_eq!(listed["values"]["test_command"], "task test");
        let fetched = manage_memory(
            &json!({
                "action": "get",
                "namespace": "user",
                "key": "response_style"
            }),
            directory.path(),
        )
        .await
        .expect("get user memory");
        assert_eq!(fetched["value"], "concise");

        let removed = manage_memory(
            &json!({
                "action": "delete",
                "namespace": "environment",
                "key": "test_command"
            }),
            directory.path(),
        )
        .await
        .expect("delete environment memory");
        assert_eq!(removed["removed"], true);
        let missing = manage_memory(
            &json!({
                "action": "get",
                "namespace": "environment",
                "key": "test_command"
            }),
            directory.path(),
        )
        .await
        .expect("get removed memory");
        assert_eq!(missing["found"], false);

        let oversized_key = "k".repeat(257);
        assert!(manage_memory(
            &json!({
                "action": "get",
                "namespace": "environment",
                "key": oversized_key
            }),
            directory.path(),
        )
        .await
        .unwrap_err()
        .contains("at most 256"));
        let oversized_value = "v".repeat(65_537);
        assert!(manage_memory(
            &json!({
                "action": "set",
                "namespace": "user",
                "key": "bounded",
                "value": oversized_value
            }),
            directory.path(),
        )
        .await
        .unwrap_err()
        .contains("at most 65536"));
    }

    #[test]
    fn parses_bounded_duckduckgo_fixture_without_network() {
        let fixture = r#"
            <div class="result">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fguide&amp;rut=abc">
                Example &amp; Guide
              </a>
              <a class="result__snippet">A <b>safe</b> guide.</a>
            </div>
            <div class="result">
              <a href="https://docs.example.org/page" class="result__a">Documentation</a>
              <div class="result__snippet">Reference material</div>
            </div>
        "#;

        let results = parse_search_results(fixture, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["title"], "Example & Guide");
        assert_eq!(results[0]["url"], "https://example.com/guide");
        assert_eq!(results[0]["snippet"], "A safe guide.");
    }

    #[test]
    fn extracts_safe_markdown_and_discards_active_content() {
        let base = reqwest::Url::parse("https://example.com/docs/").unwrap();
        let html = r#"
            <title>API &amp; Guide</title>
            <script>alert('no')</script><style>.hidden {}</style>
            <h1>API &amp; Guide</h1>
            <p>Read <a href="reference">the docs</a> and
               <a href="javascript:alert(1)">ignore this link</a>.</p>
            <p>&lt;script&gt;encoded&lt;/script&gt;</p>
            <ul><li>First</li><li>Second</li></ul>
        "#;

        let markdown = html_to_safe_markdown(html, &base);
        assert!(markdown.contains("# API & Guide"));
        assert!(markdown.contains("[the docs](https://example.com/docs/reference)"));
        assert!(markdown.contains("ignore this link"));
        assert!(markdown.contains("- First"));
        assert!(!markdown.contains("alert('no')"));
        assert!(!markdown.contains("javascript:"));
        assert!(markdown.contains("&lt;script&gt;encoded&lt;/script&gt;"));
        assert_eq!(extract_page_title(html).as_deref(), Some("API & Guide"));
    }

    #[test]
    fn rejects_non_http_and_credentialed_urls() {
        assert!(validate_http_url("file:///tmp/secret").is_err());
        assert!(validate_http_url("https://user:password@example.com").is_err());
        assert!(validate_http_url("https://example.com/docs").is_ok());
    }

    #[test]
    fn rejects_local_private_link_local_and_metadata_hosts() {
        for url in [
            "http://localhost/admin",
            "http://api.localhost./admin",
            "http://127.0.0.1/admin",
            "http://2130706433/admin",
            "http://10.1.2.3/admin",
            "http://172.16.0.1/admin",
            "http://192.168.1.1/admin",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/admin",
            "http://224.0.0.1/admin",
            "http://[::1]/admin",
            "http://[fc00::1]/admin",
            "http://[fe80::1]/admin",
            "http://[fec0::1]/admin",
            "http://[::ffff:127.0.0.1]/admin",
            "http://[64:ff9b::a00:1]/admin",
        ] {
            assert!(validate_http_url(url).is_err(), "must reject {url}");
        }
    }

    #[tokio::test]
    async fn resolver_rejects_private_or_mixed_dns_answers_and_pins_public_ports() {
        let resolver = StaticResolver {
            addresses: HashMap::from([
                (
                    "private.example".to_string(),
                    vec!["10.0.0.8:1".parse().unwrap()],
                ),
                (
                    "mixed.example".to_string(),
                    vec![
                        "93.184.216.34:1".parse().unwrap(),
                        "127.0.0.1:1".parse().unwrap(),
                    ],
                ),
                (
                    "public.example".to_string(),
                    vec!["93.184.216.34:1".parse().unwrap()],
                ),
            ]),
        };

        for host in ["private.example", "mixed.example"] {
            let url = reqwest::Url::parse(&format!("https://{host}/docs")).unwrap();
            let error = resolve_public_destination(&url, &resolver)
                .await
                .err()
                .expect("private resolution rejected");
            assert!(error.contains("non-public address"), "{error}");
        }

        let public = reqwest::Url::parse("https://public.example:8443/docs").unwrap();
        let destination = resolve_public_destination(&public, &resolver)
            .await
            .expect("public resolution");
        assert_eq!(destination.host, "public.example");
        assert_eq!(destination.addresses[0].port(), 8443);
    }

    #[tokio::test]
    async fn redirect_targets_receive_the_same_resolver_validation() {
        let resolver = StaticResolver {
            addresses: HashMap::from([(
                "internal.example".to_string(),
                vec!["192.168.1.20:443".parse().unwrap()],
            )]),
        };
        let origin = reqwest::Url::parse("https://public.example/start").unwrap();
        let redirect = validated_redirect_target(&origin, "https://internal.example/admin")
            .expect("syntactically valid redirect");
        let error = resolve_public_destination(&redirect, &resolver)
            .await
            .err()
            .expect("redirect resolution rejected");
        assert!(error.contains("non-public address"));
        assert!(validated_redirect_target(&origin, "http://127.0.0.1/admin").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_grep_does_not_follow_file_symlinks_outside_scope() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let project = directory.path().join("project");
        std::fs::create_dir(&project).expect("project directory");
        std::fs::write(project.join("inside.txt"), "needle inside\n").expect("inside file");
        let external = directory.path().join("external-secret.txt");
        std::fs::write(&external, "needle secret\n").expect("external file");
        symlink(&external, project.join("linked-secret.txt")).expect("external symlink");

        let result = grep(&json!({"pattern": "needle", "path": "."}), &project)
            .await
            .expect("grep result");
        let matches = result["matches"].as_array().expect("matches");
        assert_eq!(matches.len(), 1);
        assert!(matches[0]["file"]
            .as_str()
            .expect("match file")
            .ends_with("inside.txt"));
        assert_eq!(matches[0]["snippet"], "needle inside");
    }

    #[tokio::test]
    async fn grep_bounds_each_file_and_the_aggregate_scan() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            directory.path().join("a.txt"),
            "needle early\n0123456789abcdefghijklmnopqrstuvwxyz",
        )
        .expect("first fixture");
        std::fs::write(
            directory.path().join("b.txt"),
            "second file without a match but with a long tail",
        )
        .expect("second fixture");

        let result = grep_with_limits(
            &json!({"pattern": "needle", "path": "."}),
            directory.path(),
            20,
            28,
        )
        .await
        .expect("bounded grep");

        assert_eq!(result["matches"].as_array().unwrap().len(), 1);
        assert!(result["bytes_scanned"].as_u64().unwrap() <= 28);
        assert_eq!(result["files_scanned"], 2);
        assert_eq!(result["files_truncated"], 2);
        assert_eq!(result["aggregate_limit_reached"], true);
        assert_eq!(result["truncated"], true);
    }

    #[test]
    fn failed_in_memory_registration_removes_prepared_terminal_record() {
        let root = tempfile::tempdir().expect("root");
        let sessions_dir = root.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            root.path().join("state/daemons"),
        )
        .expect("task store");
        task_store
            .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: "duplicate-terminal".to_string(),
                command: "printf ok".to_string(),
                cwd: root.path().to_path_buf(),
                project_root: root.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir,
                session_id: "origin".to_string(),
                execution: ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
            .expect("prepare terminal");
        let manager = crate::daemons::task::TaskManager::new();
        manager
            .register_task("duplicate-terminal".to_string(), "terminal")
            .expect("seed duplicate");

        let error = register_prepared_durable_task(&manager, "duplicate-terminal", &task_store)
            .expect_err("duplicate registration fails");

        assert!(error.contains("already exists"), "{error}");
        assert!(task_store.get("duplicate-terminal").unwrap().is_none());
    }

    #[test]
    fn successful_schedule_admission_records_execution_generation() {
        let root = tempfile::tempdir().expect("root");
        let sessions_dir = root.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            root.path().join("state/daemons"),
        )
        .expect("task store");
        let prepared = task_store
            .prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
                id: "generation-schedule".to_string(),
                prompt: "later".to_string(),
                project_root: root.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir,
                session_id: "origin".to_string(),
                initial_delay: Duration::from_secs(60),
                interval: Duration::from_secs(60),
                repeat_count: 1,
            })
            .expect("prepare schedule");
        let manager = crate::daemons::task::TaskManager::new();
        let audit_log = crate::daemons::task::DaemonAuditLog::at_path(
            task_store.daemon_dir().join("audit.jsonl"),
        );

        register_and_audit_prepared_schedule(
            &manager,
            &task_store,
            &session_store,
            &audit_log,
            "generation-schedule",
            "origin",
            60,
            60,
            1,
            "later",
        )
        .expect("admit schedule");

        let session = session_store.load("origin").expect("origin session");
        assert!(session.events.iter().any(|event| {
            event.kind == "timer_scheduled"
                && event.details.get("execution_id").and_then(Value::as_str)
                    == Some(prepared.execution_id.as_str())
        }));
        assert!(audit_log
            .read_all()
            .expect("daemon audit")
            .iter()
            .any(|record| {
                record.action == "schedule"
                    && record.outcome == "scheduled"
                    && record.detail.as_deref().is_some_and(|detail| {
                        detail.contains(&format!("execution_id={}", prepared.execution_id))
                    })
            }));
    }

    #[test]
    fn rejected_schedule_records_failure_only_after_admission_attempt() {
        let root = tempfile::tempdir().expect("root");
        let sessions_dir = root.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            root.path().join("state/daemons"),
        )
        .expect("task store");
        task_store
            .prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
                id: "duplicate-schedule".to_string(),
                prompt: "later".to_string(),
                project_root: root.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir,
                session_id: "origin".to_string(),
                initial_delay: Duration::from_secs(60),
                interval: Duration::from_secs(60),
                repeat_count: 1,
            })
            .expect("prepare schedule");
        let manager = crate::daemons::task::TaskManager::new();
        manager
            .register_task("duplicate-schedule".to_string(), "timer")
            .expect("seed duplicate");
        let audit_log = crate::daemons::task::DaemonAuditLog::at_path(
            task_store.daemon_dir().join("audit.jsonl"),
        );

        register_and_audit_prepared_schedule(
            &manager,
            &task_store,
            &session_store,
            &audit_log,
            "duplicate-schedule",
            "origin",
            60,
            60,
            1,
            "later",
        )
        .expect_err("duplicate schedule is rejected");

        assert!(task_store.get("duplicate-schedule").unwrap().is_none());
        let session = session_store.load("origin").expect("origin session");
        assert!(session
            .events
            .iter()
            .any(|event| event.kind == "timer_schedule_failed"));
        assert!(!session
            .events
            .iter()
            .any(|event| event.kind == "timer_scheduled"));
        let records = audit_log.read_all().expect("daemon audit");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, "failed");
    }

    #[test]
    fn schedule_audit_failure_rolls_back_admitted_task() {
        let root = tempfile::tempdir().expect("root");
        let sessions_dir = root.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            root.path().join("state/daemons"),
        )
        .expect("task store");
        task_store
            .prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
                id: "audit-failure-schedule".to_string(),
                prompt: "later".to_string(),
                project_root: root.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir,
                session_id: "origin".to_string(),
                initial_delay: Duration::from_secs(60),
                interval: Duration::from_secs(60),
                repeat_count: 1,
            })
            .expect("prepare schedule");
        let manager = crate::daemons::task::TaskManager::new();
        let invalid_audit_path = task_store.daemon_dir().join("audit.jsonl");
        std::fs::create_dir(&invalid_audit_path).expect("invalid audit directory");
        let audit_log = crate::daemons::task::DaemonAuditLog::at_path(invalid_audit_path);

        register_and_audit_prepared_schedule(
            &manager,
            &task_store,
            &session_store,
            &audit_log,
            "audit-failure-schedule",
            "origin",
            60,
            60,
            1,
            "later",
        )
        .expect_err("audit failure rejects schedule");

        assert!(task_store.get("audit-failure-schedule").unwrap().is_none());
        assert!(manager.get_task("audit-failure-schedule").is_none());
        let session = session_store.load("origin").expect("origin session");
        assert!(session
            .events
            .iter()
            .any(|event| event.kind == "timer_schedule_failed"));
        assert!(!session
            .events
            .iter()
            .any(|event| event.kind == "timer_scheduled"));
        assert_eq!(
            session.events.last().map(|event| event.kind.as_str()),
            Some("timer_schedule_failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn schedule_compensation_surfaces_failure_to_terminalize_prepared_record() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("root");
        let sessions_dir = root.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            root.path().join("state/daemons"),
        )
        .expect("task store");
        task_store
            .prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
                id: "unresolved-audit-schedule".to_string(),
                prompt: "later".to_string(),
                project_root: root.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir,
                session_id: "origin".to_string(),
                initial_delay: Duration::from_secs(60),
                interval: Duration::from_secs(60),
                repeat_count: 1,
            })
            .expect("prepare schedule");
        let manager = crate::daemons::task::TaskManager::new();
        let invalid_audit_path = task_store.daemon_dir().join("audit.jsonl");
        std::fs::create_dir(&invalid_audit_path).expect("invalid audit directory");
        let audit_log = crate::daemons::task::DaemonAuditLog::at_path(invalid_audit_path);
        let tasks_dir = task_store.daemon_dir().join("tasks");
        let original_permissions = std::fs::metadata(&tasks_dir).unwrap().permissions();
        let mut blocked_permissions = original_permissions.clone();
        blocked_permissions.set_mode(0o500);
        std::fs::set_permissions(&tasks_dir, blocked_permissions).expect("block task removal");

        let result = register_and_audit_prepared_schedule(
            &manager,
            &task_store,
            &session_store,
            &audit_log,
            "unresolved-audit-schedule",
            "origin",
            60,
            60,
            1,
            "later",
        );
        std::fs::set_permissions(&tasks_dir, original_permissions).expect("restore task directory");
        let error = result.expect_err("unresolved compensation must be surfaced");

        assert!(
            error.contains("failed to roll back admitted schedule"),
            "{error}"
        );
        assert!(
            error.contains("failed to terminalize admitted schedule"),
            "{error}"
        );
        assert_eq!(
            task_store
                .get("unresolved-audit-schedule")
                .unwrap()
                .unwrap()
                .status,
            "prepared"
        );
        let session = session_store.load("origin").expect("origin session");
        assert!(!session
            .events
            .iter()
            .any(|event| event.kind == "timer_scheduled"));
        let failure = session
            .events
            .iter()
            .find(|event| event.kind == "timer_schedule_failed")
            .expect("failure evidence");
        assert!(failure.details["error"]
            .as_str()
            .is_some_and(|detail| detail.contains("failed to terminalize admitted schedule")));
    }
}
