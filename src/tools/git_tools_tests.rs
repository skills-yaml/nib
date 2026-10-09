//! T080 phase 2b fixtures for host-side Git tools.

use super::{
    approval_preview, branch_slug, git_commit, git_push, terminal_git_write, without_credentials,
};
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
    let mut command = std::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    // Never act on a repository named by an inherited GIT_DIR (for example
    // when tests run from a Git hook).
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(key);
    }
    let output = command.output().expect("fixture git");
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
    assert_eq!(
        git(repo, &["branch", "--show-current"]),
        "nib/update-the-readme"
    );
    assert_eq!(
        git(repo, &["log", "-1", "--format=%s"]),
        "Update the readme"
    );
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

    for paths in [
        json!(["../outside"]),
        json!(["/etc/passwd"]),
        json!(["--all"]),
    ] {
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
    git(
        directory.path(),
        &["init", "--quiet", "--bare", remote.to_str().unwrap()],
    );
    repository(&repo);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo, &["switch", "--quiet", "-c", "nib/feature"]);

    let result = git_push(&json!({}), &repo).await.expect("push");
    assert_eq!(result["branch"], "nib/feature");
    assert_eq!(
        git(
            &repo,
            &["rev-parse", "--abbrev-ref", "nib/feature@{upstream}"]
        ),
        "origin/nib/feature"
    );
    assert!(git_push(&json!({"remote": "--force"}), &repo)
        .await
        .is_err());
    assert!(git_push(&json!({"remote": "missing"}), &repo)
        .await
        .is_err());
    git(&repo, &["switch", "--quiet", "--detach"]);
    assert!(git_push(&json!({}), &repo).await.is_err());
}

/// Review F1: nib state is never staged, nested repositories are refused and
/// explicit paths cannot name nib state.
#[tokio::test]
#[serial]
async fn commit_never_stages_nib_state_or_nested_repositories() {
    let _environment = GitEnvironment::isolated();
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path();
    repository(repo);
    git(repo, &["switch", "--quiet", "-c", "feature"]);
    std::fs::create_dir_all(repo.join(".nib/profiles")).unwrap();
    std::fs::write(repo.join(".nib/config.toml"), "api_key = \"secret\"\n").unwrap();
    std::fs::write(repo.join("work.txt"), "work\n").unwrap();

    git_commit(&json!({"message": "Work"}), repo)
        .await
        .expect("commit");
    let tracked = git(repo, &["ls-files"]);
    assert!(tracked.contains("work.txt"), "{tracked}");
    assert!(!tracked.contains(".nib"), "{tracked}");

    let error = git_commit(
        &json!({"message": "x", "paths": [".nib/config.toml"]}),
        repo,
    )
    .await
    .expect_err("nib path");
    assert!(error.contains("nib state"), "{error}");

    let nested = repo.join("vendor/inner");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    std::fs::write(nested.join("file.txt"), "x\n").unwrap();
    let error = git_commit(&json!({"message": "vendor"}), repo)
        .await
        .expect_err("nested repository");
    assert!(error.contains("nested repositories"), "{error}");
}

/// Review F7: nothing changes before the change check, and a failing commit
/// returns to the original branch and removes the new one.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn failed_commit_rolls_back_the_new_branch() {
    use std::os::unix::fs::PermissionsExt;
    let _environment = GitEnvironment::isolated();
    let directory = tempfile::tempdir().unwrap();
    let repo = directory.path();
    repository(repo);

    let error = git_commit(&json!({"message": "Empty"}), repo)
        .await
        .expect_err("no changes");
    assert!(error.contains("nothing to commit"), "{error}");
    assert_eq!(git(repo, &["branch", "--show-current"]), "main");

    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(repo.join("README.md"), "changed\n").unwrap();
    git_commit(&json!({"message": "Rejected by hook"}), repo)
        .await
        .expect_err("hook rejects");
    assert_eq!(git(repo, &["branch", "--show-current"]), "main");
    assert!(git(repo, &["branch", "--list", "nib/*"]).is_empty());
}

/// Review F2: the approval preview shows what is committed or pushed, and
/// never the remote's credentials.
#[tokio::test]
#[serial]
async fn approval_previews_show_changes_and_commits() {
    let _environment = GitEnvironment::isolated();
    let directory = tempfile::tempdir().unwrap();
    let remote = directory.path().join("remote.git");
    let repo = directory.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(
        directory.path(),
        &["init", "--quiet", "--bare", remote.to_str().unwrap()],
    );
    repository(&repo);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    std::fs::write(repo.join("README.md"), "changed\n").unwrap();

    let preview = approval_preview("git_commit", &json!({"message": "Update readme"}), &repo).await;
    let text = preview.join("\n");
    assert!(text.contains("Message: Update readme"), "{text}");
    assert!(
        text.contains("Branch: new nib/update-readme (from main)"),
        "{text}"
    );
    assert!(text.contains("README.md"), "{text}");

    git(&repo, &["switch", "--quiet", "-c", "feature"]);
    git(&repo, &["commit", "--quiet", "-am", "Feature commit"]);
    let preview = approval_preview("git_push", &json!({}), &repo).await;
    let text = preview.join("\n");
    assert!(text.contains("Branch: feature -> origin/feature"), "{text}");
    assert!(text.contains("Feature commit"), "{text}");
    assert!(text.contains("start"), "{text}");

    assert_eq!(
        without_credentials("https://user:token@example.invalid/repo.git"),
        "https://example.invalid/repo.git"
    );
    assert_eq!(
        without_credentials("git@example.invalid:repo.git"),
        "git@example.invalid:repo.git"
    );
}
