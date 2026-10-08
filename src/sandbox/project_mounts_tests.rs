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
    for allowed in [project.clone(), project.join(".git")] {
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
            denied("cat \"$HOME/private.txt\"", "no such file"),
            "printf ok > allowed.txt".to_string(),
        ]
        .join(" && ");
        let output = run(&worktree, &command, &boundaries).await;
        assert_success(&output);
    }
}

#[test]
#[serial]
fn project_mounts_reject_allow_write_covering_home() {
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (_project, worktree) = project_fixture(&home);
    let _home = HomeGuard::set(home.as_os_str());
    for allowed in [home.clone(), root.path().to_path_buf()] {
        let boundaries = BoundaryConfig {
            allow_write: vec![allowed.to_string_lossy().to_string()],
            ..BoundaryConfig::default()
        };
        let error = build_bwrap_args("true", &worktree, &boundaries, "restricted")
            .expect_err("home-covering writable path");
        assert!(error.contains("private home directory"), "{error}");
    }
}

#[tokio::test]
#[serial]
async fn project_mounts_protect_unmanaged_worktree_pointer() {
    if !strict_available() {
        return;
    }
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
    let pointer = std::fs::read_to_string(outside.join(".git")).unwrap();
    let _home = HomeGuard::set(home.as_os_str());
    let command = [
        denied("printf 'gitdir: /tmp/evil' > .git", READ_ONLY),
        denied("mv .git .git-old", BUSY_OR_READ_ONLY),
        "printf edit > edited.txt".to_string(),
    ]
    .join(" && ");
    let output = run(&outside, &command, &BoundaryConfig::default()).await;
    assert_success(&output);
    assert_eq!(
        std::fs::read_to_string(outside.join(".git")).unwrap(),
        pointer
    );
}

#[tokio::test]
#[serial]
async fn project_mounts_hide_ancestor_state_outside_home() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (project, _worktree) = project_fixture(&root.path().join("workspace"));
    let nested = project.join("vendor/nested");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    let _home = HomeGuard::set(home.as_os_str());
    let secret = format!("cat {}/.nib/config.toml", project.display());
    for cwd in [nested.clone(), project.clone(), project.join("vendor")] {
        let output = run(
            &cwd,
            &denied(&secret, "no such file"),
            &BoundaryConfig::default(),
        )
        .await;
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
    mounts
        .append_protections(&mut args, &nested, true, Some(&home), &[])
        .unwrap();
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

/// N1: a nib project whose own checkout is a linked worktree resolves to the
/// fallback plan; its session worktree must stay usable inside the state mask.
#[tokio::test]
#[serial]
async fn project_mounts_fallback_keeps_session_of_linked_project_usable() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    for base in [home.join("inside"), root.path().join("outside")] {
        let (main, _worktree) = project_fixture(&base);
        let linked = base.join("linked-project");
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        std::fs::create_dir_all(linked.join(".nib")).unwrap();
        std::fs::write(linked.join(".nib/config.toml"), "api_key = \"linked\"\n").unwrap();
        let session = linked.join(".nib/worktrees/sessions/s2");
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "s2",
                session.to_str().unwrap(),
            ],
        );
        let _home = HomeGuard::set(home.as_os_str());
        let command = [
            "printf edit > edited.txt".to_string(),
            denied(
                &format!("cat {}/.nib/config.toml", linked.display()),
                "no such file",
            ),
            denied("printf 'gitdir: /tmp/evil' > .git", READ_ONLY),
        ]
        .join(" && ");
        let output = run(
            &session.canonicalize().unwrap(),
            &command,
            &BoundaryConfig::default(),
        )
        .await;
        assert_success(&output);
        assert_eq!(
            std::fs::read_to_string(session.join("edited.txt")).unwrap(),
            "edit"
        );
    }
}

/// N2: a working directory above several projects must not expose their nib
/// state or make their Git metadata writable.
#[tokio::test]
#[serial]
async fn project_mounts_protect_nested_projects_below_workspace() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = root.path().join("work");
    let (project, _worktree) = project_fixture(&work);
    let inner = work.join("group/inner");
    std::fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "--quiet"]);
    let _home = HomeGuard::set(home.as_os_str());
    let name = project.file_name().unwrap().to_string_lossy().to_string();
    let command = [
        denied(&format!("cat {name}/.nib/config.toml"), "no such file"),
        denied(&format!("printf '[core]' >> {name}/.git/config"), READ_ONLY),
        denied(
            &format!("mv {name}/.git {name}/.git-old"),
            BUSY_OR_READ_ONLY,
        ),
        format!("printf edit > {name}/edited.txt"),
        denied(&format!("mv {name} renamed"), BUSY_OR_READ_ONLY),
        denied("mv group group-old", BUSY_OR_READ_ONLY),
        denied("printf '[core]' >> group/inner/.git/config", READ_ONLY),
    ]
    .join(" && ");
    let output = run(
        &work.canonicalize().unwrap(),
        &command,
        &BoundaryConfig::default(),
    )
    .await;
    assert_success(&output);
}

/// N4: credential masks are applied after allow_write.
#[tokio::test]
#[serial]
async fn project_mounts_keep_cargo_credentials_masked_over_allow_write() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (_project, worktree) = project_fixture(&home);
    let opt = root.path().join("opt");
    let cargo_home = opt.join("cargo");
    std::fs::create_dir_all(&cargo_home).unwrap();
    std::fs::write(
        cargo_home.join("credentials.toml"),
        "token = \"cargo-secret\"\n",
    )
    .unwrap();
    let _home = HomeGuard::set(home.as_os_str());
    let previous = std::env::var_os("CARGO_HOME");
    std::env::set_var("CARGO_HOME", &cargo_home);
    let boundaries = BoundaryConfig {
        allow_write: vec![opt.to_string_lossy().to_string()],
        ..BoundaryConfig::default()
    };
    let output = run(
        &worktree,
        "test ! -s \"$CARGO_HOME/credentials.toml\"",
        &boundaries,
    )
    .await;
    match previous {
        Some(value) => std::env::set_var("CARGO_HOME", value),
        None => std::env::remove_var("CARGO_HOME"),
    }
    assert_success(&output);
}

/// P1: workspace re-binds must not undo credential masks.
#[tokio::test]
#[serial]
async fn project_mounts_keep_cargo_credentials_masked_after_rebind() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let work = root.path().join("work");
    let cargo_home = work.join(".cargo");
    std::fs::create_dir_all(&cargo_home).unwrap();
    std::fs::write(
        cargo_home.join("credentials.toml"),
        "token = \"cargo-secret\"\n",
    )
    .unwrap();
    let _home = HomeGuard::set(home.as_os_str());
    let previous = std::env::var_os("CARGO_HOME");
    std::env::set_var("CARGO_HOME", &cargo_home);
    let output = run(
        &work.canonicalize().unwrap(),
        "test ! -s \"$CARGO_HOME/credentials.toml\"",
        &BoundaryConfig::default(),
    )
    .await;
    match previous {
        Some(value) => std::env::set_var("CARGO_HOME", value),
        None => std::env::remove_var("CARGO_HOME"),
    }
    assert_success(&output);
}

/// P3: allow_write areas get the same nested protections as the workspace.
#[tokio::test]
#[serial]
async fn project_mounts_protect_projects_inside_allow_write() {
    if !strict_available() {
        return;
    }
    let root = tempdir().unwrap();
    let home = root.path().join("home");
    let (_project, worktree) = project_fixture(&home);
    let other = home.join("other");
    let (other_project, _other_worktree) = project_fixture(&other);
    let _home = HomeGuard::set(home.as_os_str());
    let boundaries = BoundaryConfig {
        allow_write: vec![other.to_string_lossy().to_string()],
        ..BoundaryConfig::default()
    };
    let path = other_project.display();
    let command = [
        format!("printf ok > {}/allowed.txt", other.display()),
        denied(&format!("cat {path}/.nib/config.toml"), "no such file"),
        denied(&format!("printf '[core]' >> {path}/.git/config"), READ_ONLY),
        denied(&format!("mv {path} {path}-old"), BUSY_OR_READ_ONLY),
    ]
    .join(" && ");
    let output = run(&worktree, &command, &boundaries).await;
    assert_success(&output);
}
