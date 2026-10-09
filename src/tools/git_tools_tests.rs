//! T080 phase 2b fixtures for host-side Git tools.

use super::git_tools::{branch_slug, git_commit, git_push, terminal_git_write};
use serde_json::json;
use serial_test::serial;
use std::path::Path;

/// Isolates fixtures from the developer's global and system Git config
/// (signing, hooks, templates), which the tools deliberately inherit.
struct GitEnvironment(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl GitEnvironment {
    fn isolated() -> Self {
        let keys = ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_NOSYSTEM"];
        let previous = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        Self(previous)
    }
}

impl Drop for GitEnvironment {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("fixture git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn repository(cwd: &Path) {
    git(cwd, &["init", "--quiet", "--initial-branch", "main"]);
    git(cwd, &["config", "user.name", "Fixture"]);
    git(cwd, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(cwd.join("README.md"), "start\n").unwrap();
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "--quiet", "-m", "start"]);
}

#[test]
fn branch_slugs_and_terminal_redirects() {
    assert_eq!(
        branch_slug("Fix: plan clearing (T081)\n\nbody"),
        "fix-plan-clearing-t081"
    );
    assert_eq!(branch_slug("!!!"), "change");
    assert!(branch_slug(&"x".repeat(200)).len() <= 40);

    for (command, tool) in [
        ("git commit -m wip", Some("git_commit")),
        ("git -C sub commit -am x", Some("git_commit")),
        ("git -c user.name=x commit", Some("git_commit")),
        ("cargo fmt && git push origin main", Some("git_push")),
        ("/usr/bin/git push", Some("git_push")),
        ("git status; git log", None),
        ("git log --grep push", None),
        ("echo commit push", None),
    ] {
        assert_eq!(terminal_git_write(command), tool, "{command}");
    }
}

#[tokio::test]
#[serial]
async fn commit_branches_first_on_main_and_commits_in_place_elsewhere() {
    let _environment = GitEnvironment::isolated();
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path();
    repository(repo);

    std::fs::write(repo.join("README.md"), "changed\n").unwrap();
    let result = git_commit(&json!({"message": "Update the readme"}), repo)
        .await
        .expect("commit on main branches first");
    assert_eq!(result["created_branch"], "nib/update-the-readme");
    assert_eq!(git(repo, &["branch", "--show-current"]), "nib/update-the-readme");
    assert_eq!(git(repo, &["log", "-1", "--format=%s"]), "Update the readme");
    assert_eq!(git(repo, &["log", "-1", "--format=%s", "main"]), "start");

    std::fs::write(repo.join("notes.txt"), "kept\n").unwrap();
    std::fs::write(repo.join("other.txt"), "not staged\n").unwrap();
    let result = git_commit(
        &json!({"message": "Add notes", "paths": ["notes.txt"]}),
        repo,
    )
    .await
    .expect("commit on a feature branch");
    assert!(result["created_branch"].is_null());
    assert_eq!(result["branch"], "nib/update-the-readme");
    let committed = git(repo, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(committed, "notes.txt");

    git(repo, &["switch", "--quiet", "main"]);
    std::fs::write(repo.join("README.md"), "again\n").unwrap();
    let result = git_commit(&json!({"message": "Update the readme"}), repo)
        .await
        .expect("collision gets a suffix");
    assert_eq!(result["created_branch"], "nib/update-the-readme-2");
}

#[tokio::test]
#[serial]
async fn commit_rejects_unsafe_input_and_empty_changes() {
    let _environment = GitEnvironment::isolated();
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path();
    repository(repo);
    git(repo, &["switch", "--quiet", "-c", "feature"]);

    for paths in [json!(["../outside"]), json!(["/etc/passwd"]), json!(["--all"])] {
        let error = git_commit(&json!({"message": "x", "paths": paths}), repo)
            .await
            .expect_err("unsafe path");
        assert!(error.contains("inside the workspace"), "{error}");
    }
    assert!(git_commit(&json!({"message": "   "}), repo).await.is_err());
    let error = git_commit(&json!({"message": "nothing"}), repo)
        .await
        .expect_err("no changes");
    assert!(error.contains("nothing to commit"), "{error}");
}

#[tokio::test]
#[serial]
async fn push_sets_upstream_on_a_local_remote_and_refuses_bad_input() {
    let _environment = GitEnvironment::isolated();
    let directory = tempfile::tempdir().unwrap();
    let remote = directory.path().join("remote.git");
    let repo = directory.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(directory.path(), &["init", "--quiet", "--bare", remote.to_str().unwrap()]);
    repository(&repo);
    git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&repo, &["switch", "--quiet", "-c", "nib/feature"]);

    let result = git_push(&json!({}), &repo).await.expect("push");
    assert_eq!(result["branch"], "nib/feature");
    assert_eq!(
        git(&repo, &["rev-parse", "--abbrev-ref", "nib/feature@{upstream}"]),
        "origin/nib/feature"
    );
    assert!(git_push(&json!({"remote": "--force"}), &repo).await.is_err());
    assert!(git_push(&json!({"remote": "missing"}), &repo).await.is_err());
    git(&repo, &["switch", "--quiet", "--detach"]);
    assert!(git_push(&json!({}), &repo).await.is_err());
}
