//! T080 phase 5d: session worktrees after commits, landing and discarding.

use super::*;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git fixture");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    git(
        directory.path(),
        &["init", "--quiet", "--initial-branch", "main"],
    );
    git(directory.path(), &["config", "user.name", "Fixture"]);
    git(
        directory.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(directory.path().join("README.md"), "start\n").unwrap();
    git(directory.path(), &["add", "."]);
    git(directory.path(), &["commit", "--quiet", "-m", "start"]);
    directory
}

#[test]
fn a_session_worktree_with_a_commit_still_validates() {
    let repository = repository();
    let worktree = WorktreeManager::new(repository.path().to_path_buf())
        .create_for_session("committed")
        .unwrap();
    std::fs::write(worktree.join("a.txt"), "one\n").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(&worktree, &["commit", "--quiet", "-m", "session commit"]);

    let found = with_validated_session_worktree(repository.path(), "committed", |path| {
        Ok(path.to_path_buf())
    });
    assert!(found.is_ok(), "{found:?}");
    let mut manager = WorktreeManager::new(repository.path().to_path_buf());
    let existing = manager.existing_for_session("committed");
    assert!(existing.is_ok(), "{existing:?}");
}

#[test]
fn a_commit_during_a_run_keeps_the_cached_worktree_valid() {
    let repository = repository();
    let mut manager = WorktreeManager::new(repository.path().to_path_buf());
    let worktree = manager.create_for_session("in-run").unwrap();
    std::fs::write(worktree.join("a.txt"), "one\n").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(&worktree, &["commit", "--quiet", "-m", "first"]);
    assert_eq!(
        manager.existing_for_session("in-run").unwrap(),
        Some(worktree.clone())
    );
    std::fs::write(worktree.join("b.txt"), "two\n").unwrap();
    git(&worktree, &["add", "b.txt"]);
    git(&worktree, &["commit", "--quiet", "-m", "second"]);
    assert_eq!(manager.create_for_session("in-run").unwrap(), worktree);
}

#[test]
fn landing_helpers_name_the_branch_and_remove_worktree_and_branch() {
    let repository = repository();
    assert_eq!(
        session_worktree_branch(repository.path(), "landed").unwrap(),
        None
    );
    let worktree = WorktreeManager::new(repository.path().to_path_buf())
        .create_for_session("landed")
        .unwrap();
    std::fs::write(worktree.join("a.txt"), "one\n").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(&worktree, &["commit", "--quiet", "-m", "session commit"]);
    std::fs::write(worktree.join("draft.txt"), "uncommitted\n").unwrap();

    let (path, branch) = session_worktree_branch(repository.path(), "landed")
        .unwrap()
        .unwrap();
    assert_eq!(path, worktree);
    assert!(branch.starts_with("nib/session/landed-"), "{branch}");

    remove_session_worktree(repository.path(), "landed", None).unwrap();
    assert!(!worktree.exists());
    assert_eq!(git(repository.path(), &["branch", "--list", &branch]), "");
    assert_eq!(
        session_worktree_branch(repository.path(), "landed").unwrap(),
        None
    );
    assert!(!git(repository.path(), &["worktree", "list"]).contains("landed"));
    // The folder is untouched.
    assert_eq!(
        git(
            repository.path(),
            &["status", "--porcelain", "--", ".", ":(exclude).nib"]
        ),
        ""
    );
}

#[test]
fn a_worktree_off_its_branch_is_not_adopted_and_recovers_on_it() {
    let repository = repository();
    let worktree = WorktreeManager::new(repository.path().to_path_buf())
        .create_for_session("rebasing")
        .unwrap();
    let (_, branch) = session_worktree_branch(repository.path(), "rebasing")
        .unwrap()
        .unwrap();
    std::fs::write(worktree.join("a.txt"), "one\n").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(&worktree, &["commit", "--quiet", "-m", "first"]);
    // Mid-rebase or detached: the branch moved, but the worktree is off it.
    git(&worktree, &["switch", "--quiet", "--detach"]);
    let error = session_worktree_branch(repository.path(), "rebasing").unwrap_err();
    assert!(error.contains("not on its branch"), "{error}");

    git(&worktree, &["switch", "--quiet", &branch]);
    std::fs::write(worktree.join("b.txt"), "two\n").unwrap();
    git(&worktree, &["add", "b.txt"]);
    git(&worktree, &["commit", "--quiet", "-m", "second"]);
    assert!(session_worktree_branch(repository.path(), "rebasing")
        .unwrap()
        .is_some());
}

#[test]
fn removal_is_refused_when_the_branch_moved_since_the_preview() {
    let repository = repository();
    let worktree = WorktreeManager::new(repository.path().to_path_buf())
        .create_for_session("moved")
        .unwrap();
    let previewed = git(&worktree, &["rev-parse", "HEAD"]);
    std::fs::write(worktree.join("a.txt"), "one\n").unwrap();
    git(&worktree, &["add", "a.txt"]);
    git(&worktree, &["commit", "--quiet", "-m", "after the preview"]);
    let error = remove_session_worktree(repository.path(), "moved", Some(&previewed)).unwrap_err();
    assert!(error.contains("moved since the preview"), "{error}");
    assert!(worktree.join("a.txt").exists());
}
