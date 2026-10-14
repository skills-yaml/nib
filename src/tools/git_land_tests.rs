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
