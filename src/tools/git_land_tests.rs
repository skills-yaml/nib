//! T080 phase 5d: `/land` previews and merges a session worktree.

use super::super::tests::{git, repository, GitEnvironment};
use super::{merge, preview, MergeOutcome};
use serial_test::serial;
use std::path::{Path, PathBuf};

const BRANCH: &str = "nib/session/fixture";

/// A folder on `folder_branch` with a session worktree on `BRANCH`.
fn fixture(folder_branch: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    repository(&folder);
    if folder_branch != "main" {
        git(&folder, &["switch", "--quiet", "-c", folder_branch]);
    }
    let worktree = directory.path().join("worktree");
    git(
        &folder,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            BRANCH,
            worktree.to_str().unwrap(),
        ],
    );
    (directory, folder, worktree)
}

fn commit(cwd: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(cwd.join(file), contents).unwrap();
    git(cwd, &["add", "--", file]);
    git(cwd, &["commit", "--quiet", "-m", message]);
}

#[tokio::test]
#[serial]
async fn preview_lists_commits_changes_and_a_code_that_follows_the_state() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("feature");
    commit(&worktree, "a.txt", "one\n", "add a");
    std::fs::write(worktree.join("b.txt"), "draft\n").unwrap();

    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    assert_eq!(shown.folder_branch.as_deref(), Some("feature"));
    assert!(!shown.folder_protected);
    assert_eq!(shown.commit_count, 1);
    assert!(shown.commits[0].ends_with(" add a"), "{:?}", shown.commits);
    assert_eq!(shown.uncommitted, vec!["?? b.txt".to_string()]);
    assert!(shown.stat.contains("a.txt"), "{}", shown.stat);
    assert_eq!(shown.code.len(), 7);
    assert_eq!(
        preview(&folder, &worktree, BRANCH).await.unwrap().code,
        shown.code
    );

    // Any change to what was shown changes the code.
    std::fs::write(worktree.join("b.txt"), "draft 2\n").unwrap();
    git(&worktree, &["add", "b.txt"]);
    let staged = preview(&folder, &worktree, BRANCH).await.unwrap();
    assert_ne!(staged.code, shown.code);
    commit(&folder, "c.txt", "folder\n", "folder moves");
    assert_ne!(
        preview(&folder, &worktree, BRANCH).await.unwrap().code,
        staged.code
    );

    let main = fixture("main");
    assert!(
        preview(&main.1, &main.2, BRANCH)
            .await
            .unwrap()
            .folder_protected
    );
    let error = preview(&folder, &worktree, "nib/session/other")
        .await
        .unwrap_err();
    assert!(error.contains("no longer on its branch"), "{error}");
}

#[tokio::test]
#[serial]
async fn merge_fast_forwards_or_merges_into_a_feature_branch() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("feature");
    commit(&worktree, "a.txt", "one\n", "add a");
    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    let MergeOutcome::Merged {
        commit: merged,
        branch,
        created_branch,
    } = merge(&folder, &shown, "topic").await.unwrap()
    else {
        panic!("expected a merge");
    };
    assert_eq!(merged, shown.tip, "a fast-forward");
    assert_eq!(branch.as_deref(), Some("feature"));
    assert_eq!(created_branch, None);
    assert_eq!(
        std::fs::read_to_string(folder.join("a.txt")).unwrap(),
        "one\n"
    );

    // The folder moved on: a true merge, and the folder's unrelated
    // uncommitted change survives it.
    commit(&worktree, "b.txt", "two\n", "add b");
    commit(&folder, "c.txt", "three\n", "add c");
    std::fs::write(folder.join("README.md"), "edited by the user\n").unwrap();
    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    let MergeOutcome::Merged { commit: merged, .. } =
        merge(&folder, &shown, "topic").await.unwrap()
    else {
        panic!("expected a merge");
    };
    assert_eq!(git(&folder, &["rev-parse", "HEAD^2"]), shown.tip);
    assert_ne!(merged, shown.tip);
    assert_eq!(
        std::fs::read_to_string(folder.join("README.md")).unwrap(),
        "edited by the user\n"
    );

    let again = preview(&folder, &worktree, BRANCH).await.unwrap();
    assert_eq!(again.commit_count, 0);
    assert_eq!(
        merge(&folder, &again, "topic").await.unwrap(),
        MergeOutcome::AlreadyMerged
    );
}

#[tokio::test]
#[serial]
async fn merge_on_main_branches_first() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("main");
    commit(&worktree, "a.txt", "one\n", "add a");
    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    let MergeOutcome::Merged {
        branch,
        created_branch,
        ..
    } = merge(&folder, &shown, "Land session abc").await.unwrap()
    else {
        panic!("expected a merge");
    };
    assert_eq!(created_branch.as_deref(), Some("nib/land-session-abc"));
    assert_eq!(branch, created_branch);
    assert_ne!(
        git(&folder, &["rev-parse", "main"]),
        git(&folder, &["rev-parse", "HEAD"])
    );
}

#[tokio::test]
#[serial]
async fn merge_refuses_uncommitted_worktree_changes_overlap_and_staged_files() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("feature");
    commit(&worktree, "a.txt", "one\n", "add a");
    std::fs::write(worktree.join("draft.txt"), "draft\n").unwrap();
    let error = merge(
        &folder,
        &preview(&folder, &worktree, BRANCH).await.unwrap(),
        "topic",
    )
    .await
    .unwrap_err();
    assert!(error.contains("/land commit"), "{error}");
    std::fs::remove_file(worktree.join("draft.txt")).unwrap();

    // The user has their own a.txt in the folder.
    std::fs::write(folder.join("a.txt"), "mine\n").unwrap();
    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    let error = merge(&folder, &shown, "topic").await.unwrap_err();
    assert!(error.contains("a.txt"), "{error}");
    assert_eq!(
        std::fs::read_to_string(folder.join("a.txt")).unwrap(),
        "mine\n"
    );
    std::fs::remove_file(folder.join("a.txt")).unwrap();

    std::fs::write(folder.join("README.md"), "staged\n").unwrap();
    git(&folder, &["add", "README.md"]);
    let error = merge(&folder, &shown, "topic").await.unwrap_err();
    assert!(error.contains("staged"), "{error}");
}

#[tokio::test]
#[serial]
async fn a_conflict_is_undone_and_returns_to_the_original_branch() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("main");
    commit(&worktree, "README.md", "session\n", "session edit");
    commit(&folder, "README.md", "folder\n", "folder edit");
    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    let error = merge(&folder, &shown, "topic").await.unwrap_err();
    assert!(error.contains("conflicts in README.md"), "{error}");
    assert!(error.contains("were kept"), "{error}");
    assert_eq!(git(&folder, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(git(&folder, &["branch", "--list", "nib/topic"]), "");
    assert_eq!(git(&folder, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(folder.join("README.md")).unwrap(),
        "folder\n"
    );
}

#[tokio::test]
#[serial]
async fn merge_never_overwrites_an_ignored_folder_file() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("feature");
    commit(&folder, ".gitignore", ".env\n", "ignore env");
    std::fs::write(folder.join(".env"), "real secret\n").unwrap();
    git(&worktree, &["merge", "--quiet", "feature"]);
    // The session stops ignoring .env and commits its own.
    std::fs::write(worktree.join(".gitignore"), "").unwrap();
    std::fs::write(worktree.join(".env"), "agent value\n").unwrap();
    git(&worktree, &["add", ".gitignore", ".env"]);
    git(&worktree, &["commit", "--quiet", "-m", "track env"]);

    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    let error = merge(&folder, &shown, "topic").await.unwrap_err();
    assert!(error.contains("were kept"), "{error}");
    assert_eq!(
        std::fs::read_to_string(folder.join(".env")).unwrap(),
        "real secret\n"
    );
    assert_eq!(git(&folder, &["rev-parse", "HEAD"]), shown.folder_head);
}

#[tokio::test]
#[serial]
async fn preview_shows_ignored_worktree_files_and_large_sessions_merge() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("feature");
    std::fs::write(worktree.join(".gitignore"), "build/\n").unwrap();
    std::fs::create_dir(worktree.join("build")).unwrap();
    std::fs::write(worktree.join("build/output"), "generated\n").unwrap();
    // Well over the 16 KiB output cap of the other Git tools.
    let directory = worktree.join("a-directory-with-a-reasonably-long-name");
    std::fs::create_dir(&directory).unwrap();
    for index in 0..400 {
        std::fs::write(
            directory.join(format!(
                "generated-source-file-with-a-long-name-{index:04}.txt"
            )),
            format!("{index}\n"),
        )
        .unwrap();
    }
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "--quiet", "-m", "many files"]);

    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    assert_eq!(shown.ignored, vec!["build/".to_string()]);
    std::fs::write(worktree.join("build/more"), "more\n").unwrap();
    assert_eq!(
        preview(&folder, &worktree, BRANCH).await.unwrap().code,
        shown.code,
        "a collapsed ignored directory lists the same"
    );
    std::fs::write(worktree.join("local.log"), "x\n").unwrap();
    std::fs::write(worktree.join(".gitignore"), "build/\n*.log\n").unwrap();
    git(&worktree, &["commit", "--quiet", "-am", "ignore logs"]);
    let with_log = preview(&folder, &worktree, BRANCH).await.unwrap();
    assert!(with_log.ignored.contains(&"local.log".to_string()));

    // The folder has many untracked files of its own.
    for index in 0..400 {
        std::fs::write(
            folder.join(format!(
                "untracked-user-file-with-a-long-name-{index:04}.txt"
            )),
            "mine\n",
        )
        .unwrap();
    }
    assert!(matches!(
        merge(&folder, &with_log, "topic").await.unwrap(),
        MergeOutcome::Merged { .. }
    ));
}

#[tokio::test]
#[serial]
async fn a_detached_folder_branches_first_and_a_failed_merge_returns_to_it() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("feature");
    git(&folder, &["switch", "--quiet", "--detach"]);
    let detached = git(&folder, &["rev-parse", "HEAD"]);
    commit(&worktree, "README.md", "session\n", "session edit");
    commit(&folder, "README.md", "folder\n", "folder edit");
    let folder_head = git(&folder, &["rev-parse", "HEAD"]);
    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    assert!(shown.folder_protected);
    assert_eq!(shown.folder_branch, None);
    let error = merge(&folder, &shown, "topic").await.unwrap_err();
    assert!(error.contains("conflicts in README.md"), "{error}");
    assert_ne!(folder_head, detached);
    assert_eq!(git(&folder, &["rev-parse", "HEAD"]), folder_head);
    let symbolic = std::process::Command::new("git")
        .current_dir(&folder)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .status()
        .unwrap();
    assert!(!symbolic.success(), "still detached");
    assert_eq!(git(&folder, &["branch", "--list", "nib/topic"]), "");
}

#[tokio::test]
#[serial]
async fn a_failing_merge_hook_undoes_the_merge_and_keeps_unrelated_changes() {
    let _environment = GitEnvironment::isolated();
    let (_directory, folder, worktree) = fixture("feature");
    commit(&worktree, "a.txt", "session\n", "session file");
    commit(&folder, "b.txt", "folder\n", "folder file");
    std::fs::write(folder.join("README.md"), "user edit\n").unwrap();
    let hook = folder.join(".git/hooks/pre-merge-commit");
    std::fs::write(&hook, "#!/bin/sh\necho refused by hook >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let shown = preview(&folder, &worktree, BRANCH).await.unwrap();
    let error = merge(&folder, &shown, "topic").await.unwrap_err();
    assert!(error.contains("refused by hook"), "{error}");
    assert_eq!(git(&folder, &["rev-parse", "HEAD"]), shown.folder_head);
    assert!(!folder.join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(folder.join("README.md")).unwrap(),
        "user edit\n"
    );
}
