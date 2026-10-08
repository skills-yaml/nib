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

/// Runs `command` and requires its stderr to mention `expected`, so a
/// negative check cannot pass for an unrelated reason.
fn denied(command: &str, expected: &str) -> String {
    format!("{{ ! out=$( ( {command} ) 2>&1 ) && printf '%s' \"$out\" | grep -qiE '{expected}'; }}")
}

const READ_ONLY: &str = "read-only file system";
const BUSY_OR_READ_ONLY: &str = "busy|read-only file system";

#[tokio::test]
#[serial]
async fn project_mounts_enable_git_reads_in_managed_worktree_under_home() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    let _home = HomeGuard::set(home.as_os_str());
    let command = [
        "git status --short --branch | grep -q 'nib/session/s1'".to_string(),
        "git log --oneline -1 | grep -q fixture".to_string(),
        "printf change > edited.txt".to_string(),
        "git status --short | grep -q edited.txt".to_string(),
        "git diff --stat HEAD >/dev/null".to_string(),
        format!(
            "test \"$(cat {}/README.md)\" = 'main checkout'",
            project.display()
        ),
        denied(
            &format!("printf x > {}/README.md", project.display()),
            READ_ONLY,
        ),
        denied(
            &format!("cat {}/.nib/config.toml", project.display()),
            "no such file",
        ),
        denied("cat \"$HOME/private.txt\"", "no such file"),
        denied("git add edited.txt", READ_ONLY),
    ]
    .join(" && ");
    let output = run(&worktree, &command, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(worktree.join("edited.txt")).unwrap(),
        "change"
    );
}

#[tokio::test]
#[serial]
async fn project_mounts_keep_main_git_metadata_read_only() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    let _home = HomeGuard::set(home.as_os_str());
    let git = project.join(".git");
    let git = git.display();
    let command = [
        denied(&format!("printf hook > {git}/hooks/pre-commit"), READ_ONLY),
        denied(&format!("printf '[core]' >> {git}/config"), READ_ONLY),
        denied(&format!("printf ../evil > {git}/commondir"), READ_ONLY),
        denied(
            &format!("printf ../evil > {git}/worktrees/s1/commondir"),
            READ_ONLY,
        ),
        denied(&format!("printf x > {git}/refs/heads/main"), READ_ONLY),
        denied(&format!("mkdir {git}/rebase-merge"), READ_ONLY),
    ]
    .join(" && ");
    let output = run(&worktree, &command, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert!(!project.join(".git/hooks/pre-commit").exists());
    assert!(!project.join(".git/commondir").exists());
}

#[tokio::test]
#[serial]
async fn project_mounts_prevent_replacing_git_metadata() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    let _home = HomeGuard::set(home.as_os_str());
    let pointer = std::fs::read_to_string(worktree.join(".git")).unwrap();
    let command = [
        denied("printf 'gitdir: /tmp/evil' > .git", READ_ONLY),
        denied("mv .git .git-old", BUSY_OR_READ_ONLY),
        denied("rm -f .git", BUSY_OR_READ_ONLY),
    ]
    .join(" && ");
    let output = run(&worktree, &command, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(worktree.join(".git")).unwrap(),
        pointer
    );

    let in_place = [
        denied("mv .git .git-old", BUSY_OR_READ_ONLY),
        denied("mv .nib .nib-old", BUSY_OR_READ_ONLY),
    ]
    .join(" && ");
    let output = run(&project, &in_place, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert!(project.join(".git/HEAD").is_file());
    assert!(project.join(".nib/config.toml").is_file());
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
    let command = [
        denied("cat .nib/config.toml", "no such file"),
        "printf edit > in-place.txt".to_string(),
        "git status --short | grep -q in-place.txt".to_string(),
        denied("printf hook > .git/hooks/pre-commit", READ_ONLY),
    ]
    .join(" && ");
    let output = run(&project, &command, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(project.join("in-place.txt")).unwrap(),
        "edit"
    );
    assert!(std::fs::read_to_string(project.join(".nib/config.toml"))
        .unwrap()
        .contains("fixture-secret"));
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
    for allowed in [
        project.clone(),
        root.path().canonicalize().unwrap(),
        project.join(".git"),
    ] {
        let boundaries = BoundaryConfig {
            allow_write: vec![allowed.to_string_lossy().to_string()],
            ..BoundaryConfig::default()
        };
        let command = [
            denied(
                &format!("cat {}/.nib/config.toml", project.display()),
                "no such file",
            ),
            denied(
                &format!("printf hook > {}/.git/hooks/pre-commit", project.display()),
                READ_ONLY,
            ),
            denied("printf 'gitdir: /tmp/evil' > .git", READ_ONLY),
            "printf ok > allowed.txt".to_string(),
        ]
        .join(" && ");
        let output = run(&worktree, &command, &boundaries).await;
        assert_success(&output);
    }
}

#[test]
#[serial]
fn project_mounts_reject_working_directory_inside_state() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    std::fs::create_dir_all(worktree.join(".nib/profiles")).unwrap();
    let _home = HomeGuard::set(home.as_os_str());
    for cwd in [project.join(".nib"), worktree.join(".nib/profiles")] {
        let error = build_bwrap_args("true", &cwd, &BoundaryConfig::default(), "restricted")
            .expect_err("state working directory");
        assert!(error.contains("nib runtime state"), "{error}");
    }
}

#[cfg(unix)]
#[test]
#[serial]
fn project_mounts_fail_closed_on_symlinked_state() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, _worktree) = project_fixture(&home);
    let elsewhere = root.path().join("elsewhere-state");
    std::fs::rename(project.join(".nib"), &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, project.join(".nib")).unwrap();
    let _home = HomeGuard::set(home.as_os_str());
    let error = build_bwrap_args("true", &project, &BoundaryConfig::default(), "restricted")
        .expect_err("symlinked state");
    assert!(error.contains("symbolic link"), "{error}");
}

#[test]
fn project_mounts_order_protections_after_workspace() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (project, worktree) = project_fixture(&home);
    let home = home.canonicalize().unwrap();
    let nested = worktree.join("src");
    std::fs::create_dir_all(&nested).unwrap();
    let mounts = ProjectMounts::resolve(&nested, Some(&home)).expect("managed project");
    let mut args = Vec::new();
    mounts.append_binds(&mut args, &nested, true).unwrap();
    mounts.append_protections(&mut args, &nested, true).unwrap();
    let text = |path: &Path| path.to_string_lossy().to_string();
    let (project_text, worktree_text) = (text(&project), text(&worktree));
    let state = format!("{project_text}/.nib");
    let git_dir = format!("{project_text}/.git");
    let pointer = format!("{worktree_text}/.git");
    let position = |needle: &[&str]| {
        args.windows(needle.len())
            .rposition(|window| window == needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in {args:?}"))
    };
    let project_bind = position(&["--ro-bind", &project_text, &project_text]);
    let state_mask = position(&["--tmpfs", &state]);
    let worktree_bind = position(&["--bind", &worktree_text, &worktree_text]);
    let pointer_bind = position(&["--ro-bind", &pointer, &pointer]);
    let git_bind = position(&["--ro-bind", &git_dir, &git_dir]);
    assert!(project_bind < state_mask && state_mask < worktree_bind);
    assert!(worktree_bind < pointer_bind && worktree_bind < git_bind);
    assert!(!args
        .windows(3)
        .any(|window| window[0] == "--bind" && window[1] == git_dir));
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
