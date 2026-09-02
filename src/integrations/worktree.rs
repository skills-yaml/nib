//! Git worktree isolation for mutating tool execution.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::time::{Duration, Instant};
use uuid::Uuid;

const SESSION_WORKTREE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const SESSION_WORKTREE_DROP_JOIN_TIMEOUT: Duration = Duration::from_secs(8);

#[cfg(test)]
static SESSION_POST_ADD_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static SESSION_ADD_REPORTED_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

#[cfg(test)]
static SESSION_POST_ADD_BRANCH_MOVES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
struct SessionCreatePause {
    reached: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
static SESSION_AFTER_BRANCH_PAUSES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<SessionCreatePause>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
static SESSION_BEFORE_ADOPTION_PAUSES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<SessionCreatePause>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub struct WorktreeManager {
    repo_root: PathBuf,
    worktrees: HashMap<String, ManagedSessionWorktree>,
}

#[derive(Clone)]
struct ManagedSessionWorktree {
    path: PathBuf,
    ownership: std::sync::Arc<crate::sandbox::worktree::ManagedWorktreeReceipt>,
}

struct PendingSessionWorktree {
    worktree: ManagedSessionWorktree,
    adopted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for PendingSessionWorktree {
    fn drop(&mut self) {
        if self.adopted.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let deadline = Instant::now() + SESSION_WORKTREE_CLEANUP_TIMEOUT;
        let _ = crate::sandbox::worktree::cleanup_managed_worktree(
            &self.worktree.ownership,
            deadline,
            SESSION_WORKTREE_CLEANUP_TIMEOUT,
        );
    }
}

impl WorktreeManager {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root: repo_root.canonicalize().unwrap_or(repo_root),
            worktrees: HashMap::new(),
        }
    }

    pub fn create_for_session(&mut self, session_id: &str) -> Result<PathBuf, String> {
        if let Some(path) = self.cached_path(session_id)? {
            return Ok(path);
        }
        let worktree = create_session_worktree(&self.repo_root, session_id, None)?;
        let path = worktree.path.clone();
        self.worktrees.insert(session_id.to_string(), worktree);
        Ok(path)
    }

    pub async fn create_for_session_cancellable(
        &mut self,
        session_id: &str,
        upstream: Option<&crate::agent::CancellationSignal>,
    ) -> Result<PathBuf, String> {
        if let Some(path) = self.cached_path(session_id)? {
            return Ok(path);
        }

        let repo_root = self.repo_root.clone();
        let worker_session_id = session_id.to_string();
        let cancellation = crate::sandbox::worktree::BlockingGitCancellation::new(upstream);
        let worker_cancellation = cancellation.clone();
        let adopted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_adopted = adopted.clone();
        let (completion_sender, completion_receiver) = std::sync::mpsc::sync_channel(1);
        let _cancel_on_drop = BlockingCreateCancellationGuard {
            cancellation,
            completion: completion_receiver,
            adopted: adopted.clone(),
        };
        let pending = tokio::task::spawn_blocking(move || {
            let result =
                create_session_worktree(&repo_root, &worker_session_id, Some(&worker_cancellation))
                    .map(|worktree| PendingSessionWorktree {
                        worktree,
                        adopted: worker_adopted,
                    });
            let _ = completion_sender.send(());
            result
        })
        .await
        .map_err(|error| format!("session worktree worker failed: {error}"))??;
        #[cfg(test)]
        let before_adoption_pause = {
            SESSION_BEFORE_ADOPTION_PAUSES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(session_id)
        };
        #[cfg(test)]
        if let Some(pause) = before_adoption_pause {
            pause
                .reached
                .store(true, std::sync::atomic::Ordering::Release);
            std::future::pending::<()>().await;
        }
        let path = pending.worktree.path.clone();
        self.worktrees
            .insert(session_id.to_string(), pending.worktree.clone());
        adopted.store(true, std::sync::atomic::Ordering::Release);
        Ok(path)
    }

    fn cached_path(&mut self, session_id: &str) -> Result<Option<PathBuf>, String> {
        if !self.worktrees.contains_key(session_id) {
            if let Some(ownership) = crate::sandbox::worktree::load_managed_worktree_ownership(
                &self.repo_root,
                crate::sandbox::worktree::ManagedWorktreeKind::Session,
                session_id,
            )? {
                let path = crate::sandbox::worktree::managed_worktree_owned_path(&ownership);
                self.worktrees.insert(
                    session_id.to_string(),
                    ManagedSessionWorktree { path, ownership },
                );
            }
        }
        let Some(worktree) = self.worktrees.get(session_id) else {
            return Ok(None);
        };
        if worktree.path.starts_with(&self.repo_root)
            && crate::sandbox::worktree::validate_managed_worktree_ownership(&worktree.ownership)
                .is_ok()
        {
            return Ok(Some(worktree.path.clone()));
        }
        let ownership = worktree.ownership.clone();
        let deadline = Instant::now() + SESSION_WORKTREE_CLEANUP_TIMEOUT;
        crate::sandbox::worktree::cleanup_managed_worktree(
            &ownership,
            deadline,
            SESSION_WORKTREE_CLEANUP_TIMEOUT,
        )
        .map_err(|error| {
            format!(
                "cached session worktree ownership changed and exact cleanup was incomplete: {error}"
            )
        })?;
        self.worktrees.remove(session_id);
        Ok(None)
    }

    pub fn get_path(&self, session_id: &str) -> Option<PathBuf> {
        let worktree = self.worktrees.get(session_id)?;
        crate::sandbox::worktree::validate_managed_worktree_ownership(&worktree.ownership).ok()?;
        Some(worktree.path.clone())
    }
}

pub(crate) fn with_validated_session_worktree<T>(
    repo_root: &Path,
    session_id: &str,
    inspect: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<Option<T>, String> {
    let repo_root = repo_root
        .canonicalize()
        .map_err(|error| format!("failed to resolve the project root: {error}"))?;
    let Some(ownership) = crate::sandbox::worktree::load_managed_worktree_ownership(
        &repo_root,
        crate::sandbox::worktree::ManagedWorktreeKind::Session,
        session_id,
    )?
    else {
        return Ok(None);
    };
    let path = crate::sandbox::worktree::validate_managed_worktree_for_read(&ownership)
        .map_err(|error| format!("session worktree ownership is invalid: {error}"))?;
    if !path.starts_with(&repo_root) {
        return Err("session worktree ownership escapes the project root".to_string());
    }

    let inspected = inspect(&path);
    let revalidated = crate::sandbox::worktree::validate_managed_worktree_for_read(&ownership)
        .map_err(|error| format!("session worktree ownership changed during inspection: {error}"));
    match (inspected, revalidated) {
        (Ok(value), Ok(_)) => Ok(Some(value)),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(ownership_error)) => Err(format!("{error}; {ownership_error}")),
    }
}

struct BlockingCreateCancellationGuard {
    cancellation: crate::sandbox::worktree::BlockingGitCancellation,
    completion: std::sync::mpsc::Receiver<()>,
    adopted: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for BlockingCreateCancellationGuard {
    fn drop(&mut self) {
        if self.adopted.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        self.cancellation.cancel();
        let _ = self
            .completion
            .recv_timeout(SESSION_WORKTREE_DROP_JOIN_TIMEOUT);
    }
}

fn create_session_worktree(
    repo_root: &Path,
    session_id: &str,
    cancellation: Option<&crate::sandbox::worktree::BlockingGitCancellation>,
) -> Result<ManagedSessionWorktree, String> {
    validate_repository_root_controlled(repo_root, cancellation)?;
    let safe_session = sanitize_component(session_id);
    let suffix = &Uuid::new_v4().simple().to_string()[..8];
    let worktree_name = format!("{safe_session}-{suffix}");
    let worktree_path = repo_root
        .join(".nib")
        .join("worktrees")
        .join("sessions")
        .join(&worktree_name);
    let expected_worktree_path = worktree_path.clone();
    let parent = worktree_path
        .parent()
        .ok_or("session worktree has no parent directory")?;
    let canonical_parent = crate::fs_security::ensure_directory_without_symlinks(parent)
        .map_err(|error| format!("session worktree parent is unsafe: {error}"))?;
    if !canonical_parent.starts_with(repo_root) {
        return Err(format!(
            "session worktree parent escapes the repository: {}",
            parent.display()
        ));
    }
    prove_session_destination_absent(&canonical_parent, &worktree_path)?;

    let branch = format!("nib/session/{safe_session}-{suffix}");
    let mut reservation = crate::sandbox::worktree::reserve_managed_worktree_sync_controlled(
        repo_root,
        crate::sandbox::worktree::ManagedWorktreeKind::Session,
        session_id,
        &worktree_path,
        &branch,
        cancellation,
    )?;
    macro_rules! fail_reserved_session_create {
        ($primary:expr) => {
            return Err(
                crate::sandbox::worktree::reconcile_failed_managed_worktree_reservation(
                    reservation,
                    $primary,
                ),
            )
        };
    }
    let owned_branch =
        match crate::sandbox::worktree::create_reserved_worktree_branch_sync_controlled(
            &mut reservation,
            cancellation,
        ) {
            Ok(branch) => branch,
            Err(error) => fail_reserved_session_create!(error),
        };
    #[cfg(test)]
    if let Some(pause) = SESSION_AFTER_BRANCH_PAUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id)
    {
        pause
            .reached
            .store(true, std::sync::atomic::Ordering::Release);
        let pause_deadline = Instant::now() + Duration::from_secs(10);
        while !cancellation
            .is_some_and(crate::sandbox::worktree::BlockingGitCancellation::is_cancelled)
        {
            if Instant::now() >= pause_deadline {
                fail_reserved_session_create!(compensate_session_create(
                    repo_root,
                    &worktree_path,
                    &owned_branch,
                    None,
                    None,
                    "session worktree test pause timed out".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if cancellation.is_some_and(crate::sandbox::worktree::BlockingGitCancellation::is_cancelled) {
        fail_reserved_session_create!(compensate_session_create(
            repo_root,
            &worktree_path,
            &owned_branch,
            None,
            None,
            "session worktree creation cancelled".to_string(),
        ));
    }
    let path_receipt = match crate::sandbox::worktree::publish_reserved_empty_worktree_destination(
        &mut reservation,
        &canonical_parent,
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            fail_reserved_session_create!(compensate_session_create(
                repo_root,
                &worktree_path,
                &owned_branch,
                None,
                None,
                error,
            ));
        }
    };
    let registration_snapshot =
        crate::sandbox::worktree::reserved_worktree_registration_snapshot(&reservation);
    let create = crate::sandbox::worktree::run_git_bounded_sync_controlled(
        repo_root,
        [
            OsString::from("worktree"),
            OsString::from("add"),
            crate::fs_security::path_for_external_command(&worktree_path).into_os_string(),
            OsString::from(&branch),
        ],
        cancellation,
    )
    .and_then(|output| require_git_success(&output, "worktree add"));
    #[cfg(test)]
    let create = create.and_then(|()| {
        if SESSION_ADD_REPORTED_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id)
        {
            Err("injected failure reported after successful worktree add".to_string())
        } else {
            Ok(())
        }
    });
    if let Err(error) = create {
        fail_reserved_session_create!(compensate_session_create(
            repo_root,
            &worktree_path,
            &owned_branch,
            Some(&path_receipt),
            None,
            format!(
                "{error}; post-snapshot Git worktree registrations were preserved because a failed add provides no exact creation receipt"
            ),
        ));
    }

    let ownership = match crate::sandbox::worktree::capture_managed_worktree_receipt_sync_controlled(
        repo_root,
        &worktree_path,
        &path_receipt,
        &owned_branch,
        registration_snapshot,
        cancellation,
    ) {
        Ok(ownership) => ownership,
        Err(error) => {
            let crate::sandbox::worktree::ManagedWorktreeCaptureError { message, ownership } =
                error;
            fail_reserved_session_create!(compensate_session_create(
                repo_root,
                &worktree_path,
                &owned_branch,
                Some(&path_receipt),
                ownership.as_deref(),
                message,
            ));
        }
    };
    let ownership =
        match crate::sandbox::worktree::finish_managed_worktree_reservation(reservation, ownership)
        {
            Ok(ownership) => ownership,
            Err(error) => {
                let crate::sandbox::worktree::ManagedWorktreeCaptureError { message, ownership } =
                    error;
                return Err(compensate_session_create(
                    repo_root,
                    &worktree_path,
                    &owned_branch,
                    Some(&path_receipt),
                    ownership.as_deref(),
                    message,
                ));
            }
        };

    #[cfg(test)]
    if let Some(replacement) = SESSION_POST_ADD_BRANCH_MOVES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id)
    {
        let reference = format!("refs/heads/{branch}");
        let moved = Command::new("git")
            .current_dir(repo_root)
            .args(["update-ref", reference.as_str(), replacement.as_str()])
            .status()
            .expect("move session branch fixture");
        assert!(moved.success(), "move session branch fixture");
    }

    let validation = (|| {
        crate::fs_security::verify_directory_without_symlinks(parent)
            .map_err(|error| format!("session worktree parent changed: {error}"))?;
        let worktree_path = worktree_path.canonicalize().map_err(|error| {
            format!(
                "created worktree {} cannot be resolved: {error}",
                worktree_path.display()
            )
        })?;
        if worktree_path != expected_worktree_path
            || !worktree_path.starts_with(&canonical_parent)
            || !worktree_path.is_dir()
        {
            return Err(format!(
                "created session worktree escaped its repository-local parent: {}",
                worktree_path.display()
            ));
        }
        crate::fs_security::verify_directory_without_symlinks(&worktree_path)
            .map_err(|error| format!("created session worktree path is unsafe: {error}"))?;
        validate_repository_root_controlled(&worktree_path, cancellation)?;
        crate::sandbox::worktree::validate_owned_worktree_sync_controlled(
            &worktree_path,
            &owned_branch,
            cancellation,
        )?;
        if cancellation.is_some_and(crate::sandbox::worktree::BlockingGitCancellation::is_cancelled)
        {
            return Err("session worktree creation cancelled".to_string());
        }
        Ok(worktree_path)
    })();
    let worktree_path = match validation {
        Ok(path) => path,
        Err(error) => {
            return Err(compensate_session_create(
                repo_root,
                &expected_worktree_path,
                &owned_branch,
                Some(&path_receipt),
                Some(&ownership),
                error,
            ))
        }
    };
    #[cfg(test)]
    if SESSION_POST_ADD_FAILURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id)
    {
        return Err(compensate_session_create(
            repo_root,
            &worktree_path,
            &owned_branch,
            Some(&path_receipt),
            Some(&ownership),
            "injected session worktree post-add failure".to_string(),
        ));
    }
    Ok(ManagedSessionWorktree {
        path: worktree_path,
        ownership: std::sync::Arc::new(ownership),
    })
}

fn prove_session_destination_absent(parent: &Path, path: &Path) -> Result<(), String> {
    crate::fs_security::verify_directory_without_symlinks(parent)
        .map_err(|error| format!("session worktree destination parent changed: {error}"))?;
    if crate::fs_security::path_entry_exists(path)
        .map_err(|error| format!("failed to inspect session worktree destination: {error}"))?
    {
        return Err(format!(
            "session worktree destination appeared before creation and was preserved: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_repository_root_controlled(
    root: &Path,
    cancellation: Option<&crate::sandbox::worktree::BlockingGitCancellation>,
) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            root.display()
        ));
    }
    let output = crate::sandbox::worktree::run_git_bounded_sync_controlled(
        root,
        ["rev-parse", "--show-toplevel"],
        cancellation,
    )?;
    require_git_success(&output, "inspect repository")?;
    let reported = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .canonicalize()
        .map_err(|error| format!("failed to resolve git root: {error}"))?;
    if reported != root {
        return Err(format!(
            "configured project root {} is not the git top-level {}",
            root.display(),
            reported.display()
        ));
    }
    Ok(())
}

fn require_git_success(output: &std::process::Output, operation: &str) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        Err(git_failure(output, operation))
    }
}

fn git_failure(output: &std::process::Output, operation: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    format!("git {operation} failed: {detail}")
}

fn compensate_session_create(
    repo_root: &Path,
    worktree_path: &Path,
    owned_branch: &crate::sandbox::worktree::OwnedBranch,
    path_receipt: Option<&crate::fs_security::DirectoryRemovalReceipt>,
    ownership: Option<&crate::sandbox::worktree::ManagedWorktreeReceipt>,
    error: String,
) -> String {
    let deadline = Instant::now() + SESSION_WORKTREE_CLEANUP_TIMEOUT;
    match cleanup_partial_session_worktree(
        repo_root,
        worktree_path,
        owned_branch,
        path_receipt,
        ownership,
        deadline,
    ) {
        Ok(()) => error,
        Err(cleanup) => format!("{error}; partial session worktree cleanup failed: {cleanup}"),
    }
}

fn cleanup_partial_session_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    owned_branch: &crate::sandbox::worktree::OwnedBranch,
    path_receipt: Option<&crate::fs_security::DirectoryRemovalReceipt>,
    ownership: Option<&crate::sandbox::worktree::ManagedWorktreeReceipt>,
    deadline: Instant,
) -> Result<(), String> {
    if let Some(ownership) = ownership {
        return crate::sandbox::worktree::cleanup_managed_worktree(
            ownership,
            deadline,
            SESSION_WORKTREE_CLEANUP_TIMEOUT,
        );
    }
    let mut errors = Vec::new();
    if let Some(path_receipt) = path_receipt {
        if let Some(parent) = worktree_path.parent() {
            if let Err(error) =
                crate::fs_security::remove_directory_tree_capability_bound_if_matches(
                    parent,
                    worktree_path,
                    path_receipt.clone(),
                    deadline,
                )
            {
                errors.push(format!(
                    "failed to remove exact partial session worktree: {error}"
                ));
            }
        } else {
            errors.push("partial session worktree has no parent".to_string());
        }
    } else {
        match crate::fs_security::path_entry_exists(worktree_path) {
            Ok(true) => errors.push(format!(
                "partial session worktree has no exact ownership receipt and was preserved: {}",
                worktree_path.display()
            )),
            Ok(false) => {}
            Err(error) => errors.push(format!(
                "failed to inspect unowned session worktree path: {error}"
            )),
        }
    }
    match session_cleanup_remaining(deadline) {
        Ok(remaining) => {
            if let Err(error) = crate::sandbox::worktree::delete_owned_branch_sync_with_timeout(
                repo_root,
                owned_branch,
                remaining,
            ) {
                errors.push(error);
            }
        }
        Err(error) => errors.push(error),
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn session_cleanup_remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            format!(
                "partial session worktree cleanup exceeded {} seconds",
                SESSION_WORKTREE_CLEANUP_TIMEOUT.as_secs_f64()
            )
        })
}

fn sanitize_component(value: &str) -> String {
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
        "session".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn repository() -> tempfile::TempDir {
        let directory = tempdir().expect("repository");
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "nib@example.invalid"],
            vec!["config", "user.name", "nib"],
        ] {
            assert!(Command::new("git")
                .current_dir(directory.path())
                .args(args)
                .status()
                .expect("git fixture")
                .success());
        }
        std::fs::write(directory.path().join("README.md"), "fixture\n").expect("fixture file");
        assert!(Command::new("git")
            .current_dir(directory.path())
            .args(["add", "."])
            .status()
            .expect("git add")
            .success());
        assert!(Command::new("git")
            .current_dir(directory.path())
            .args(["commit", "-qm", "fixture"])
            .status()
            .expect("git commit")
            .success());
        directory
    }

    fn git_stdout(repository: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(repository)
            .args(args)
            .output()
            .expect("git fixture command");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn replacement_commit(repository: &Path) -> String {
        let initial = git_stdout(repository, &["rev-parse", "HEAD"]);
        std::fs::write(repository.join("session-replacement.txt"), "replacement\n")
            .expect("replacement file");
        git_stdout(repository, &["add", "session-replacement.txt"]);
        git_stdout(repository, &["commit", "-m", "session replacement fixture"]);
        let replacement = git_stdout(repository, &["rev-parse", "HEAD"]);
        git_stdout(repository, &["reset", "--hard", &initial]);
        replacement
    }

    #[test]
    fn sanitizes_branch_components() {
        assert_eq!(sanitize_component("../../bad session"), "------bad-session");
    }

    #[test]
    fn session_worktree_rehydrates_after_manager_restart() {
        let repository = repository();
        let session_id = "manager-restart";
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());
        let created = manager
            .create_for_session(session_id)
            .expect("session worktree");
        drop(manager);

        let mut restarted = WorktreeManager::new(repository.path().to_path_buf());
        let rehydrated = restarted
            .create_for_session(session_id)
            .expect("rehydrated session worktree");

        assert_eq!(rehydrated, created);
        assert_eq!(restarted.get_path(session_id), Some(created.clone()));
        let registrations = git_stdout(repository.path(), &["worktree", "list", "--porcelain"]);
        let matching_registrations = registrations
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .filter(|registered| {
                crate::fs_security::canonical_paths_match(&created, Path::new(registered))
            })
            .count();
        assert_eq!(
            matching_registrations, 1,
            "rehydrated worktree registration was not unique: {registrations}"
        );
    }

    #[test]
    fn post_add_failure_removes_session_path_registration_and_branch() {
        let repository = repository();
        let session_id = "post-add";
        SESSION_POST_ADD_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string());
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());

        let error = manager
            .create_for_session(session_id)
            .expect_err("injected post-add failure must be compensated");

        assert!(
            error.contains("injected session worktree post-add"),
            "{error}"
        );
        let sessions = repository.path().join(".nib/worktrees/sessions");
        assert!(
            !sessions.exists()
                || std::fs::read_dir(&sessions)
                    .expect("session worktree directory")
                    .next()
                    .is_none(),
            "partial session worktree path remains"
        );
        let worktrees = Command::new("git")
            .current_dir(repository.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("worktree list");
        assert!(!String::from_utf8_lossy(&worktrees.stdout).contains("/.nib/worktrees/sessions/"));
        let branches = Command::new("git")
            .current_dir(repository.path())
            .args(["branch", "--list", "nib/session/post-add-*"])
            .output()
            .expect("session branch list");
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "partial session branch remains"
        );
    }

    #[test]
    fn reported_add_failure_preserves_unproven_registration() {
        let repository = repository();
        let session_id = "reported-add-failure";
        SESSION_ADD_REPORTED_FAILURES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string());
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());

        let error = manager
            .create_for_session(session_id)
            .expect_err("reported worktree add failure must be compensated");

        assert!(error.contains("injected failure reported"), "{error}");
        assert!(error.contains("registrations were preserved"), "{error}");
        let sessions = repository.path().join(".nib/worktrees/sessions");
        assert!(
            !sessions.exists()
                || std::fs::read_dir(&sessions)
                    .expect("session worktree directory")
                    .next()
                    .is_none(),
            "partial session worktree path remains"
        );
        let registrations = repository.path().join(".git/worktrees");
        assert!(
            registrations.is_dir()
                && std::fs::read_dir(&registrations)
                    .expect("worktree registrations")
                    .next()
                    .is_some(),
            "unproven post-error registration must remain for explicit recovery"
        );
        let branches = Command::new("git")
            .current_dir(repository.path())
            .args(["branch", "--list", "nib/session/reported-add-failure-*"])
            .output()
            .expect("session branch list");
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "partial session branch remains"
        );
    }

    #[test]
    fn post_add_failure_preserves_a_moved_session_branch() {
        let repository = repository();
        let session_id = "moved-post-add";
        let replacement = replacement_commit(repository.path());
        SESSION_POST_ADD_BRANCH_MOVES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string(), replacement.clone());
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());

        let error = manager
            .create_for_session(session_id)
            .expect_err("moved branch compensation must fail closed");

        assert!(error.contains("refusing publication"), "{error}");
        assert!(error.contains("changed from"), "{error}");
        assert!(error.contains("preserving it"), "{error}");
        let sessions = repository.path().join(".nib/worktrees/sessions");
        assert!(
            !sessions.exists()
                || std::fs::read_dir(&sessions)
                    .expect("session worktree directory")
                    .next()
                    .is_none(),
            "partial session worktree path remains"
        );
        let branches = Command::new("git")
            .current_dir(repository.path())
            .args([
                "for-each-ref",
                "--format=%(objectname)",
                "refs/heads/nib/session/moved-post-add-*",
            ])
            .output()
            .expect("moved session branch lookup");
        assert!(branches.status.success());
        assert_eq!(
            String::from_utf8_lossy(&branches.stdout).trim(),
            replacement
        );
    }

    #[test]
    fn cached_session_path_replacement_is_never_returned_or_silently_discarded() {
        let repository = repository();
        let session_id = "cached-replacement";
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());
        let path = manager
            .create_for_session(session_id)
            .expect("session worktree");
        let displaced = path.with_extension("owned-away");
        std::fs::rename(&path, &displaced).expect("displace owned session path");
        std::fs::create_dir(&path).expect("install cached path replacement");
        std::fs::write(path.join("sentinel"), b"replacement")
            .expect("cached path replacement sentinel");

        let error = manager
            .create_for_session(session_id)
            .expect_err("cached replacement must fail exact cleanup");

        assert!(error.contains("ownership changed"), "{error}");
        assert!(error.contains("cleanup was incomplete"), "{error}");
        assert_eq!(
            std::fs::read(path.join("sentinel")).expect("replacement remains"),
            b"replacement"
        );
        assert!(manager.get_path(session_id).is_none());
    }

    async fn wait_for_session_pause(pause: &SessionCreatePause) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !pause.reached.load(std::sync::atomic::Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("session creation reached the post-branch pause");
    }

    fn session_creation_artifacts_exist(repository: &Path, session_id: &str) -> bool {
        let reference_prefix = format!("refs/heads/nib/session/{session_id}-");
        let branches = Command::new("git")
            .current_dir(repository)
            .args(["for-each-ref", "--format=%(refname)", &reference_prefix])
            .output()
            .expect("session branch lookup");
        assert!(branches.status.success());
        if !String::from_utf8_lossy(&branches.stdout).trim().is_empty() {
            return true;
        }
        let registration = Command::new("git")
            .current_dir(repository)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("session worktree registration lookup");
        assert!(registration.status.success());
        if String::from_utf8_lossy(&registration.stdout)
            .contains(&format!("/.nib/worktrees/sessions/{session_id}-"))
        {
            return true;
        }
        let sessions = repository.join(".nib/worktrees/sessions");
        sessions.is_dir()
            && std::fs::read_dir(sessions)
                .expect("session worktree directory")
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{session_id}-"))
                })
    }

    async fn assert_session_creation_compensated(repository: &Path, session_id: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while session_creation_artifacts_exist(repository, session_id) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled session creation was not compensated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_cancellation_reaps_blocking_session_creation_and_compensates() {
        let repository = repository();
        let session_id = "request-cancel-create";
        let pause = std::sync::Arc::new(SessionCreatePause {
            reached: std::sync::atomic::AtomicBool::new(false),
        });
        SESSION_AFTER_BRANCH_PAUSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string(), std::sync::Arc::clone(&pause));
        let cancellation = crate::agent::CancellationSignal::new();
        let worker_cancellation = cancellation.clone();
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());
        let task = tokio::spawn(async move {
            manager
                .create_for_session_cancellable(session_id, Some(&worker_cancellation))
                .await
        });
        wait_for_session_pause(&pause).await;

        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancelled session creation join timeout")
            .expect("session creation task")
            .expect_err("session creation must be cancelled");

        assert!(error.contains("cancelled"), "{error}");
        assert_session_creation_compensated(repository.path(), session_id).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_session_creation_future_signals_worker_and_compensates() {
        let repository = repository();
        let session_id = "drop-cancel-create";
        let pause = std::sync::Arc::new(SessionCreatePause {
            reached: std::sync::atomic::AtomicBool::new(false),
        });
        SESSION_AFTER_BRANCH_PAUSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string(), std::sync::Arc::clone(&pause));
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());
        let task = tokio::spawn(async move {
            manager
                .create_for_session_cancellable(session_id, None)
                .await
        });
        wait_for_session_pause(&pause).await;

        task.abort();
        assert!(task
            .await
            .expect_err("aborted session creation")
            .is_cancelled());
        assert_session_creation_compensated(repository.path(), session_id).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_completed_creation_before_adoption_compensates_exact_ownership() {
        let repository = repository();
        let session_id = "drop-before-adoption";
        let pause = std::sync::Arc::new(SessionCreatePause {
            reached: std::sync::atomic::AtomicBool::new(false),
        });
        SESSION_BEFORE_ADOPTION_PAUSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id.to_string(), std::sync::Arc::clone(&pause));
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());
        let task = tokio::spawn(async move {
            manager
                .create_for_session_cancellable(session_id, None)
                .await
        });
        wait_for_session_pause(&pause).await;

        task.abort();
        assert!(task
            .await
            .expect_err("aborted session creation before adoption")
            .is_cancelled());
        assert_session_creation_compensated(repository.path(), session_id).await;
    }

    #[cfg(unix)]
    #[test]
    fn session_worktree_creation_disables_repository_hooks() {
        use std::os::unix::fs::PermissionsExt;

        let repository = repository();
        let marker = repository.path().join("hook-ran");
        let hooks = repository.path().join(".git/hooks");
        std::fs::create_dir_all(&hooks).expect("hooks directory");
        let hook = hooks.join("post-checkout");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
        )
        .expect("post-checkout hook");
        let mut permissions = std::fs::metadata(&hook)
            .expect("hook metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).expect("executable hook");
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());

        manager
            .create_for_session("hook-disabled")
            .expect("session worktree");

        assert!(!marker.exists(), "repository hook ran during worktree add");
    }

    #[cfg(unix)]
    #[test]
    fn create_rejects_symlinked_worktree_ancestor_without_outside_mutation() {
        use std::os::unix::fs::symlink;

        let repository = repository();
        let outside = tempdir().expect("outside");
        std::fs::create_dir(repository.path().join(".nib")).expect("state");
        symlink(outside.path(), repository.path().join(".nib/worktrees"))
            .expect("worktrees symlink");
        let mut manager = WorktreeManager::new(repository.path().to_path_buf());

        let error = manager
            .create_for_session("hostile")
            .expect_err("symlinked worktree root must fail closed");

        assert!(error.contains("unsafe") || error.contains("symlink"));
        assert!(!outside.path().join("sessions").exists());
    }
}
