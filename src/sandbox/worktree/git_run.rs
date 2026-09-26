//! Bounded git process helpers.

use super::*;

pub(crate) fn drain_sync_bounded<R: Read>(mut reader: R) -> Result<Vec<u8>, String> {
    let mut captured = Vec::with_capacity(MAX_GIT_OUTPUT_BYTES.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("failed to read git output: {error}"))?;
        if read == 0 {
            break;
        }
        let remaining = MAX_GIT_OUTPUT_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(captured)
}

#[cfg(unix)]
pub(crate) fn terminate_sync_process_tree(pid: u32) {
    unsafe extern "C" {
        pub(crate) fn kill(pid: i32, signal: i32) -> i32;
    }
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            let _ = kill(-pid, 9);
        }
    }
}

#[cfg(unix)]
pub(crate) fn sync_child_has_exited_without_reaping(pid: u32) -> std::io::Result<bool> {
    let pid = i32::try_from(pid)
        .map_err(|_| std::io::Error::other("git child process identifier exceeds pid_t"))?;
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
                "waitid returned an unexpected git child process identifier",
            ));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(crate) fn require_git_success(output: &Output, operation: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(output, operation))
    }
}

pub(crate) fn git_failure(output: &Output, operation: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    format!("git {operation} failed: {detail}")
}

pub(crate) fn format_git_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn repository_root(project_root: &Path) -> Result<PathBuf, String> {
    repository_root_bounded_sync(project_root)
}

pub(crate) fn validate_created_worktree(
    path: &Path,
    owned_branch: &OwnedBranch,
) -> Result<(), String> {
    #[cfg(test)]
    {
        let id = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        if let Some(replacement) = SYNC_POST_ADD_BRANCH_MOVES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            let reference = format!("refs/heads/{}", branch_name(id));
            let moved = Command::new("git")
                .current_dir(path)
                .args(["update-ref", reference.as_str(), replacement.as_str()])
                .status()
                .expect("move subagent branch fixture");
            assert!(moved.success(), "move subagent branch fixture");
        }
        if let Some(target) = SYNC_POST_ADD_BRANCH_SYMREFS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            let reference = format!("refs/heads/{}", branch_name(id));
            let replaced = Command::new("git")
                .current_dir(path)
                .args(["symbolic-ref", reference.as_str(), target.as_str()])
                .status()
                .expect("replace subagent branch with symref fixture");
            assert!(
                replaced.success(),
                "replace subagent branch with symref fixture"
            );
        }
    }
    let output = run_git_bounded_sync(path, ["rev-parse", "--show-toplevel"])?;
    require_git_success(&output, "inspect created worktree")?;
    let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .canonicalize()
        .map_err(|error| format!("failed to resolve created git worktree: {error}"))?;
    if reported != path {
        return Err(format!(
            "created git worktree root {} does not match {}",
            reported.display(),
            path.display()
        ));
    }
    validate_owned_worktree_sync(path, owned_branch)?;
    #[cfg(test)]
    {
        let id = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
        let mut failures = SYNC_POST_ADD_VALIDATION_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if failures.remove(id) {
            return Err("injected post-add worktree validation failure".to_string());
        }
    }
    Ok(())
}

pub(crate) fn validate_owned_worktree_sync(path: &Path, owned: &OwnedBranch) -> Result<(), String> {
    validate_owned_worktree_sync_controlled(path, owned, None)
}

pub(crate) fn validate_owned_worktree_sync_controlled(
    path: &Path,
    owned: &OwnedBranch,
    cancellation: Option<&BlockingGitCancellation>,
) -> Result<(), String> {
    let symbolic = run_git_bounded_sync_with_timeout_controlled(
        path,
        ["symbolic-ref", "--quiet", "HEAD"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let symbolic = parse_symbolic_head(&symbolic)?;
    if symbolic != owned.reference {
        return Err(format!(
            "created worktree symbolic HEAD {symbolic} does not match owned branch {}",
            owned.reference
        ));
    }

    let head = run_git_bounded_sync_with_timeout_controlled(
        path,
        ["rev-parse", "--verify", "HEAD^{commit}"],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let head = parse_git_oid(&head, "inspect created worktree HEAD")?;
    validate_owned_worktree_oid("HEAD", &head, owned)?;

    validate_owned_ref_receipt(owned)?;
    let reference = run_git_bounded_sync_with_timeout_controlled(
        path,
        ["show-ref", "--hash", "--verify", owned.reference.as_str()],
        GIT_COMMAND_TIMEOUT,
        cancellation,
    )?;
    let reference = parse_git_oid(&reference, "inspect created worktree branch")?;
    validate_owned_worktree_oid(&owned.reference, &reference, owned)?;
    validate_owned_ref_receipt(owned)
}

pub(crate) fn branch_name(id: &str) -> String {
    format!("nib/subagent/{id}")
}

pub(crate) fn sanitize_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "subagent".to_string()
    } else {
        sanitized
    }
}
