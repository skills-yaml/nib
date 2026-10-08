//! T080 phase 1 fixtures for the project-aware bwrap mount plan.

use super::project_mounts::ProjectMounts;
use super::{build_bwrap_args, detect_capabilities, run_sandboxed_with_provider, BoundaryConfig};
use serial_test::serial;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use tempfile::tempdir;

struct HomeGuard(Option<OsString>);

impl HomeGuard {
    fn set(value: &OsStr) -> Self {
        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", value);
        Self(previous)
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(cwd)
        .output()
        .expect("fixture Git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn strict_available() -> bool {
    let available = detect_capabilities().bwrap_available;
    if std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_some() {
        assert!(
            available,
            "native bwrap isolation is required on this runner"
        );
    }
    available
}

/// Creates `<home>/project` with one commit, a secret nib config and a managed
/// session worktree at `.nib/worktrees/sessions/s1`.
fn project_fixture(home: &Path) -> (PathBuf, PathBuf) {
    let project = home.join("project");
    std::fs::create_dir_all(project.join(".nib")).unwrap();
    git(&project, &["init", "--quiet", "--initial-branch", "main"]);
    git(&project, &["config", "user.name", "Fixture"]);
    git(
        &project,
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(project.join("README.md"), "main checkout\n").unwrap();
    std::fs::write(project.join(".gitignore"), ".nib/\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "--quiet", "-m", "fixture"]);
    std::fs::write(
        project.join(".nib/config.toml"),
        "api_key = \"fixture-secret\"\n",
    )
    .unwrap();
    std::fs::write(home.join("private.txt"), "host-secret\n").unwrap();
    let worktree = project.join(".nib/worktrees/sessions/s1");
    git(
        &project,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "nib/session/s1",
            worktree.to_str().unwrap(),
        ],
    );
    (
        project.canonicalize().unwrap(),
        worktree.canonicalize().unwrap(),
    )
}

async fn run(cwd: &Path, command: &str, boundaries: &BoundaryConfig) -> std::process::Output {
    let (output, arguments) =
        run_sandboxed_with_provider(command, cwd, "bwrap", "restricted", boundaries)
            .await
            .expect("strict bwrap execution");
    assert!(arguments.is_some());
    output
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[serial]
async fn project_mounts_enable_git_in_managed_worktree_under_home() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    let _home = HomeGuard::set(home.as_os_str());
    let command = format!(
        r#"
        git status --short --branch | grep -q 'nib/session/s1' &&
        git log --oneline -1 | grep -q fixture &&
        printf change > edited.txt && git add edited.txt &&
        git -c user.name=Agent -c user.email=agent@example.invalid commit --quiet -m agent &&
        test "$(cat {project}/README.md)" = 'main checkout' &&
        ! printf x > {project}/README.md &&
        ! cat {project}/.nib/config.toml &&
        ! cat "$HOME/private.txt"
        "#,
        project = project.display()
    );
    let output = run(&worktree, &command, &BoundaryConfig::default()).await;
    assert_success(&output);
    git(&project, &["log", "--oneline", "-1", "nib/session/s1"]);
    assert_eq!(
        std::fs::read_to_string(project.join("README.md")).unwrap(),
        "main checkout\n"
    );
}

#[tokio::test]
#[serial]
async fn project_mounts_protect_executable_git_surfaces() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    git(&project, &["config", "core.hooksPath", ".husky"]);
    std::fs::remove_dir_all(project.join(".git/info")).ok();
    let _home = HomeGuard::set(home.as_os_str());
    let git_dir = project.join(".git");
    let command = format!(
        r#"
        ! printf hook > {git}/hooks/pre-commit &&
        ! printf '[core]' >> {git}/config &&
        ! mkdir -p {git}/info/x &&
        ! printf x > {git}/info/attributes &&
        ! mkdir -p .husky/x &&
        ! printf hook > .husky/pre-commit
        "#,
        git = git_dir.display()
    );
    let output = run(&worktree, &command, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert!(!git_dir.join("hooks/pre-commit").exists());
    assert!(!worktree.join(".husky/pre-commit").exists());
}

#[tokio::test]
#[serial]
async fn project_mounts_keep_protections_over_allow_write() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    let _home = HomeGuard::set(home.as_os_str());
    let boundaries = BoundaryConfig {
        allow_write: vec![project.join(".git").to_string_lossy().to_string()],
        ..BoundaryConfig::default()
    };
    let command = format!(
        "! printf hook > {}/hooks/pre-commit",
        project.join(".git").display()
    );
    let output = run(&worktree, &command, &boundaries).await;
    assert_success(&output);
}

#[tokio::test]
#[serial]
async fn project_mounts_ignore_rewritten_git_pointer() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (_project, worktree) = project_fixture(&home);
    let secret = home.join("secret-gitdir");
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("token"), "pointer-secret").unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", secret.display()),
    )
    .unwrap();
    let _home = HomeGuard::set(home.as_os_str());
    let command = format!(
        "! cat {secret}/token && ! git status --short",
        secret = secret.display()
    );
    let output = run(&worktree, &command, &BoundaryConfig::default()).await;
    assert_success(&output);
}

#[tokio::test]
#[serial]
async fn project_mounts_hide_state_in_place() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, _worktree) = project_fixture(&home);
    let _home = HomeGuard::set(home.as_os_str());
    let command = r#"
        ! cat .nib/config.toml &&
        printf edit > in-place.txt &&
        git status --short | grep -q in-place.txt &&
        ! printf hook > .git/hooks/pre-commit
    "#;
    let output = run(&project, command, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(project.join("in-place.txt")).unwrap(),
        "edit"
    );
    assert!(std::fs::read_to_string(project.join(".nib/config.toml"))
        .unwrap()
        .contains("fixture-secret"));
}

#[test]
#[serial]
fn project_mounts_reject_working_directory_inside_state() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, _worktree) = project_fixture(&home);
    let _home = HomeGuard::set(home.as_os_str());
    let error = build_bwrap_args(
        "true",
        &project.join(".nib"),
        &BoundaryConfig::default(),
        "restricted",
    )
    .expect_err("state working directory");
    assert!(error.contains("nib runtime state"), "{error}");
}

#[cfg(unix)]
#[test]
#[serial]
fn project_mounts_fail_closed_on_symlinked_hooks() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    let hooks = project.join(".git/hooks");
    std::fs::remove_dir_all(&hooks).unwrap();
    std::os::unix::fs::symlink(root.path(), &hooks).unwrap();
    let _home = HomeGuard::set(home.as_os_str());
    let error = build_bwrap_args("true", &worktree, &BoundaryConfig::default(), "restricted")
        .expect_err("symlinked hooks");
    assert!(error.contains("symbolic link"), "{error}");
}

#[test]
fn project_mounts_resolve_managed_worktree_to_project() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    let home = home.canonicalize().unwrap();
    let nested = worktree.join("src");
    std::fs::create_dir_all(&nested).unwrap();
    let mounts = ProjectMounts::resolve(&nested, Some(&home)).expect("managed project");
    let mut args = Vec::new();
    mounts.append_binds(&mut args, &nested, true).unwrap();
    let project = project.to_string_lossy().to_string();
    let state = format!("{project}/.nib");
    let git_dir = format!("{project}/.git");
    let nested = nested.to_string_lossy().to_string();
    let position = |needle: &[&str]| {
        args.windows(needle.len())
            .position(|window| window == needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in {args:?}"))
    };
    let project_bind = position(&["--ro-bind", &project, &project]);
    let git_bind = position(&["--bind", &git_dir, &git_dir]);
    let state_mask = position(&["--tmpfs", &state]);
    let cwd_bind = position(&["--bind", &nested, &nested]);
    assert!(project_bind < git_bind && git_bind < state_mask && state_mask < cwd_bind);
}

#[test]
fn project_mounts_never_mount_home_or_its_ancestors() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("notes")).unwrap();
    git(&home, &["init", "--quiet"]);
    let home = home.canonicalize().unwrap();
    assert_eq!(
        ProjectMounts::resolve(&home.join("notes"), Some(&home)),
        None
    );
    assert_eq!(ProjectMounts::resolve(&home, Some(&home)), None);
}

#[test]
fn project_mounts_ignore_unmanaged_linked_worktrees() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, _worktree) = project_fixture(&home);
    let outside = root.path().join("outside-worktree");
    git(
        &project,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "other",
            outside.to_str().unwrap(),
        ],
    );
    let home = home.canonicalize().unwrap();
    assert_eq!(
        ProjectMounts::resolve(&outside.canonicalize().unwrap(), Some(&home)),
        None
    );
}

#[tokio::test]
#[serial]
async fn project_mounts_protect_writable_included_config() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    std::fs::write(project.join("shared.gitconfig"), "[core]\n").unwrap();
    std::fs::write(worktree.join("nested.gitconfig"), "[core]\n").unwrap();
    std::fs::write(
        project.join("shared.gitconfig"),
        format!(
            "[include]\n\tpath = {}\n",
            worktree.join("nested.gitconfig").display()
        ),
    )
    .unwrap();
    git(&project, &["config", "include.path", "../shared.gitconfig"]);
    let _home = HomeGuard::set(home.as_os_str());
    let command = "! printf '[core]\\n\\thooksPath = /tmp\\n' >> nested.gitconfig";
    let output = run(&worktree, command, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(worktree.join("nested.gitconfig")).unwrap(),
        "[core]\n"
    );
}
