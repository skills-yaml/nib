//! Hybrid sandbox: internal execution + optional bwrap (FT-003).

use crate::config::BoundaryConfig;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct SandboxCapabilities {
    pub bwrap_available: bool,
    pub git_available: bool,
}

pub fn detect_capabilities() -> SandboxCapabilities {
    SandboxCapabilities {
        bwrap_available: which("bwrap"),
        git_available: which("git"),
    }
}

fn which(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a command inside bwrap when available; otherwise run directly in cwd.
pub async fn run_sandboxed(
    command: &str,
    cwd: &Path,
    profile: &str,
    boundaries: &BoundaryConfig,
) -> Result<(std::process::Output, Option<Vec<String>>), String> {
    let caps = detect_capabilities();
    if caps.bwrap_available && profile != "internal" {
        run_bwrap(command, cwd, boundaries, profile)
            .await
            .map(|(out, args)| (out, Some(args)))
    } else {
        run_direct(command, cwd).await.map(|out| (out, None))
    }
}

async fn run_direct(command: &str, cwd: &Path) -> Result<std::process::Output, String> {
    tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .output()
        .await
        .map_err(|e| e.to_string())
}

async fn run_bwrap(
    command: &str,
    cwd: &Path,
    boundaries: &BoundaryConfig,
    profile: &str,
) -> Result<(std::process::Output, Vec<String>), String> {
    let cwd_str = cwd.to_str().ok_or("invalid cwd")?;
    let mut args = vec![
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--bind".to_string(),
        cwd_str.to_string(),
        cwd_str.to_string(),
        "--chdir".to_string(),
        cwd_str.to_string(),
    ];

    if profile == "restricted" || boundaries.network == "restricted" {
        args.push("--unshare-all".to_string());
    }

    for allow in &boundaries.allow_write {
        // Bind additional write paths (treat as absolute or relative to cwd)
        let path = cwd.join(allow);
        if let Some(p_str) = path.to_str() {
            args.push("--bind".to_string());
            args.push(p_str.to_string());
            args.push(p_str.to_string());
        }
    }

    args.push("--".to_string());
    args.push("sh".to_string());
    args.push("-c".to_string());
    args.push(command.to_string());

    let output = tokio::process::Command::new("bwrap")
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("bwrap execution failed: {e}"))?;

    Ok((output, args))
}

pub fn doctor_report() -> String {
    let caps = detect_capabilities();
    format!(
        "Sandbox capabilities:\n  bwrap: {}\n  git: {}\n  default: internal fallback when bwrap unavailable",
        if caps.bwrap_available { "available" } else { "missing" },
        if caps.git_available { "available" } else { "missing" },
    )
}
