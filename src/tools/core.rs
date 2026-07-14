//! Core tool implementations (called only after ToolExecutor gates pass).

use crate::config::ExecutionConfig;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::timeout;

pub async fn dispatch(
    tool_name: &str,
    args: &Value,
    cwd: &Path,
    config: &ExecutionConfig,
) -> Result<Value, String> {
    match tool_name {
        "read_file" => read_file(args, cwd).await,
        "list_directory" => list_directory(args, cwd).await,
        "grep" => grep(args, cwd).await,
        "apply_patch" => apply_patch(args, cwd).await,
        "run_terminal" => run_terminal(args, cwd, config).await,
        "write_plan" => write_plan(args, cwd).await,
        "invoke_subagent" => std::future::ready(invoke_subagent(args, cwd)).await,
        "manage_subagents" => manage_subagents(args, cwd).await,
        "send_message" => send_message(args, cwd).await,
        "search_web" => search_web(args, cwd).await,
        "read_url_content" => read_url_content(args, cwd).await,
        "manage_task" => manage_task(args, cwd).await,
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
    let mut path = PathBuf::from(path_str);
    if path.is_relative() {
        path = cwd.join(path);
    }
    let path = path
        .canonicalize()
        .map_err(|e| format!("File not found: {path_str} ({e})"))?;
    if !path.starts_with(cwd) {
        return Err("Path outside project scope".to_string());
    }
    if !path.is_file() {
        return Err(format!("Not a file: {}", path.display()));
    }

    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| e.to_string())?;
    let lines: Vec<&str> = content.lines().collect();
    let start = args.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let end = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(lines.len());
    let slice = &lines[start.min(lines.len())..end.min(lines.len())];

    Ok(json!({
        "path": path.to_string_lossy(),
        "content": slice.join("\n"),
        "start_line": start,
        "end_line": end,
        "total_lines": lines.len(),
    }))
}

async fn list_directory(args: &Value, cwd: &Path) -> Result<Value, String> {
    let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let mut path = cwd.join(rel);
    path = path
        .canonicalize()
        .map_err(|e| format!("Directory not found: {rel} ({e})"))?;
    if !path.starts_with(cwd) {
        return Err("Path outside project scope".to_string());
    }

    let recursive = args
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut entries = Vec::new();

    if recursive {
        for entry in walkdir_light(&path)?.into_iter().take(100) {
            entries.push(entry);
        }
    } else {
        let mut rd = tokio::fs::read_dir(&path)
            .await
            .map_err(|e| e.to_string())?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let ft = entry.file_type().await.map_err(|e| e.to_string())?;
            entries.push(json!({
                "path": entry.file_name().to_string_lossy(),
                "type": if ft.is_dir() { "dir" } else { "file" },
            }));
        }
    }

    Ok(json!({
        "path": path.to_string_lossy(),
        "entries": entries,
    }))
}

fn walkdir_light(root: &Path) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let ft = entry.file_type().map_err(|e| e.to_string())?;
            let p = entry.path();
            let rel = p.strip_prefix(root).unwrap_or(&p);
            out.push(json!({
                "path": rel.to_string_lossy(),
                "type": if ft.is_dir() { "dir" } else { "file" },
            }));
            if ft.is_dir() {
                stack.push(p);
            }
            if out.len() >= 100 {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

async fn grep(args: &Value, cwd: &Path) -> Result<Value, String> {
    let pattern = args
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or("missing pattern")?;
    let rel = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as usize;
    let mut path = cwd.join(rel);
    if path.exists() {
        path = path.canonicalize().map_err(|e| e.to_string())?;
    }
    if !path.starts_with(cwd) {
        return Err("Path outside project scope".to_string());
    }

    let pattern_lower = pattern.to_lowercase();
    let mut matches = Vec::new();
    let files: Vec<PathBuf> = if path.is_dir() {
        walkdir_light(&path)?
            .into_iter()
            .filter_map(|e| {
                let p = e.get("path")?.as_str()?;
                let full = path.join(p);
                if full.is_file() {
                    Some(full)
                } else {
                    None
                }
            })
            .collect()
    } else {
        vec![path]
    };

    for file in files {
        if matches.len() >= max_results {
            break;
        }
        let Ok(text) = tokio::fs::read_to_string(&file).await else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&pattern_lower) {
                matches.push(json!({
                    "file": file.to_string_lossy(),
                    "line": i + 1,
                    "snippet": &line[..line.len().min(200)],
                }));
                if matches.len() >= max_results {
                    break;
                }
            }
        }
    }

    Ok(json!({
        "pattern": pattern,
        "matches": matches,
        "truncated": matches.len() >= max_results,
    }))
}

async fn apply_patch(args: &Value, cwd: &Path) -> Result<Value, String> {
    let patch = args.get("patch").and_then(|v| v.as_str()).unwrap_or("");
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if patch.is_empty() {
        return Err("empty patch".to_string());
    }

    let patch_file = cwd.join(".nib").join("_apply_patch.tmp");
    tokio::fs::create_dir_all(cwd.join(".nib"))
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::write(&patch_file, patch)
        .await
        .map_err(|e| e.to_string())?;

    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    cmd.arg("apply");
    if dry_run {
        cmd.arg("--check");
    }
    cmd.arg(&patch_file);

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("git apply failed to start: {e}"))?;
    let _ = tokio::fs::remove_file(&patch_file).await;

    Ok(json!({
        "applied": output.status.success() && !dry_run,
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

async fn run_terminal(args: &Value, cwd: &Path, config: &ExecutionConfig) -> Result<Value, String> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("missing command")?;
    let timeout_secs = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);

    let dangerous = ["rm -rf", "git reset --hard", "sudo", "DROP DATABASE"];
    for pat in dangerous {
        if command.contains(pat) {
            return Err(format!("Blocked dangerous pattern: {pat}"));
        }
    }

    let start = Instant::now();
    let run =
        crate::sandbox::run_sandboxed(command, cwd, &config.default_profile, &config.boundaries);

    let (output, bwrap_args) = timeout(Duration::from_secs(timeout_secs), run)
        .await
        .map_err(|_| format!("Command timed out after {timeout_secs}s"))?
        .map_err(|e| e.to_string())?;

    let mut res = json!({
        "command": command,
        "cwd": cwd.to_string_lossy(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "exit_code": output.status.code(),
        "duration": start.elapsed().as_secs_f64(),
        "provider": config.provider,
        "sandbox_profile": config.default_profile,
        "boundaries": config.boundaries,
    });

    if let Some(args) = bwrap_args {
        res.as_object_mut()
            .unwrap()
            .insert("bwrap_args".to_string(), json!(args));
    }

    Ok(res)
}

fn invoke_subagent(args: &Value, cwd: &Path) -> Result<Value, String> {
    let subagent_id = format!("sub-{}", uuid::Uuid::new_v4());
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("help")
        .to_string();

    // Create worktree
    let worktree = crate::sandbox::worktree::Worktree::create(cwd, &subagent_id)?;
    let wt_path = worktree.path.clone();

    crate::daemons::task::TASK_MANAGER.register_subagent(subagent_id.clone());

    let sid = subagent_id.clone();
    let cwd_buf = cwd.to_path_buf();
    tokio::spawn(async move {
        let cfg = crate::agent::AgentLoopConfig {
            max_steps: 10,
            ..Default::default()
        };
        let _ = crate::agent::run_agent_loop(wt_path.clone(), &sid, &prompt, cfg).await;
        crate::daemons::task::TASK_MANAGER.mark_completed(&sid);
        let _ = crate::sandbox::worktree::Worktree::remove(&cwd_buf, &sid);
    });

    Ok(json!({ "status": "started", "subagent_id": subagent_id }))
}

async fn manage_subagents(args: &Value, _cwd: &Path) -> Result<Value, String> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("list");
    if action == "list" {
        Ok(json!({ "subagents": crate::daemons::task::TASK_MANAGER.list_tasks() }))
    } else {
        Ok(json!({ "status": "unknown action" }))
    }
}

async fn send_message(args: &Value, cwd: &Path) -> Result<Value, String> {
    let subagent_id = args
        .get("subagent_id")
        .and_then(|v| v.as_str())
        .ok_or("missing subagent_id")?;
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or("missing message")?;

    // Append to subagent's session file so it picks it up in its loop
    let store = crate::session::SessionStore::new(cwd);
    store.append_message(subagent_id, "user", message);

    Ok(json!({ "status": "sent", "subagent_id": subagent_id }))
}

async fn search_web(args: &Value, _cwd: &Path) -> Result<Value, String> {
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    Ok(json!({ "status": "stub", "results": [format!("Results for {}", query)] }))
}

async fn read_url_content(args: &Value, _cwd: &Path) -> Result<Value, String> {
    let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
    match reqwest::get(url).await {
        Ok(res) => match res.text().await {
            Ok(text) => {
                Ok(json!({ "status": "success", "content": &text[..text.len().min(1000)] }))
            }
            Err(e) => Err(format!("Failed to read text: {}", e)),
        },
        Err(e) => Err(format!("Request failed: {}", e)),
    }
}

async fn manage_task(args: &Value, _cwd: &Path) -> Result<Value, String> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("list");
    if action == "list" {
        Ok(json!({ "tasks": crate::daemons::task::TASK_MANAGER.list_tasks() }))
    } else {
        Ok(json!({ "status": "unknown action" }))
    }
}

async fn schedule(args: &Value, _cwd: &Path) -> Result<Value, String> {
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("timer fired")
        .to_string();
    let duration = args
        .get("duration_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let (tx, _rx) = tokio::sync::mpsc::channel(1); // Dummy channel for now
    let id = format!("timer-{}", uuid::Uuid::new_v4());
    crate::daemons::task::TASK_MANAGER.spawn_timer(id.clone(), duration, prompt, tx);
    Ok(json!({ "status": "scheduled", "id": id }))
}

async fn ask_question(args: &Value, _cwd: &Path) -> Result<Value, String> {
    // In reality, this sets the loop state. For now, just return a prompt response
    Ok(json!({ "status": "pending_ui", "question": args.get("question") }))
}
