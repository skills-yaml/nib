use super::git_status;
use std::path::Path;

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .current_dir(cwd)
        .output()
        .expect("fixture Git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init(cwd: &Path) {
    git(cwd, &["init", "--quiet"]);
    git(cwd, &["config", "user.name", "Fixture"]);
    git(cwd, &["config", "user.email", "fixture@example.invalid"]);
}

#[cfg(target_os = "linux")]
fn strict_available() -> bool {
    let available = crate::sandbox::detect_capabilities().bwrap_available;
    if std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_some() {
        assert!(
            available,
            "native Git-status isolation is required on this runner"
        );
    }
    available
}

#[cfg(target_os = "linux")]
fn script(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, content).expect("script fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("executable fixture");
}

#[cfg(target_os = "linux")]
fn modify_tracked(cwd: &Path) {
    std::fs::write(cwd.join("tracked.txt"), "changed\n").expect("changed fixture");
    std::fs::File::open(cwd.join("tracked.txt"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
        .expect("force index comparison");
}

#[cfg(target_os = "linux")]
fn tracked_fixture(cwd: &Path) {
    init(cwd);
    std::fs::write(cwd.join("tracked.txt"), "initial\n").unwrap();
    std::fs::write(cwd.join(".gitattributes"), "tracked.txt filter=hostile\n").unwrap();
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "--quiet", "-m", "fixture"]);
}

#[tokio::test]
async fn git_status_requires_strict_isolation_when_unavailable() {
    let root = tempfile::tempdir().unwrap();
    init(root.path());
    if !cfg!(target_os = "linux") || !crate::sandbox::detect_capabilities().bwrap_available {
        let error = git_status(root.path()).await.expect_err("must fail closed");
        assert!(error.contains("strict Linux bwrap isolation"));
        assert!(error.contains("approved isolated terminal"));
    }
    assert!(!root.path().join(".nib/worktrees").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial]
async fn git_status_disables_fsmonitor_and_preserves_index() {
    if !strict_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _home = super::tests::EnvironmentGuard::set("HOME", home.path().to_str().unwrap());
    tracked_fixture(root.path());
    script(
        &root.path().join(".git/hooks/hostile"),
        "#!/bin/sh\ntouch fsmonitor-ran\nexit 1\n",
    );
    git(
        root.path(),
        &["config", "core.fsmonitor", ".git/hooks/hostile"],
    );
    modify_tracked(root.path());
    let before = std::fs::read(root.path().join(".git/index")).unwrap();
    let output = git_status(root.path()).await.expect("isolated status");
    assert!(output["status"].as_str().unwrap().contains("tracked.txt"));
    assert!(!root.path().join("fsmonitor-ran").exists());
    assert_eq!(
        before,
        std::fs::read(root.path().join(".git/index")).unwrap()
    );
    assert!(!root.path().join(".git/index.lock").exists());
}

#[cfg(target_os = "linux")]
fn clean_filter(cwd: &Path) {
    script(
        &cwd.join("hostile-filter"),
        concat!(
            "#!/bin/sh\n",
            "test -z \"$NIB_GIT_STATUS_SECRET\" || exit 21\n",
            "test ! -r \"$HOME/private.txt\" || exit 22\n",
            "touch filter-ran 2>/dev/null && exit 23\n",
            "cat\n"
        ),
    );
    git(cwd, &["config", "filter.hostile.clean", "./hostile-filter"]);
    git(cwd, &["config", "filter.hostile.required", "true"]);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial]
async fn git_status_isolates_clean_filter_and_populated_submodule() {
    if !strict_available() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("private.txt"), "synthetic private fixture").unwrap();
    let _home = super::tests::EnvironmentGuard::set("HOME", home.path().to_str().unwrap());
    let _secret = super::tests::EnvironmentGuard::set("NIB_GIT_STATUS_SECRET", "synthetic");
    let source = tempfile::tempdir().unwrap();
    tracked_fixture(source.path());
    let root = tempfile::tempdir().unwrap();
    tracked_fixture(root.path());
    git(
        root.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            source.path().to_str().unwrap(),
            "sub",
        ],
    );
    git(root.path(), &["commit", "--quiet", "-am", "submodule"]);
    let sub = root.path().join("sub");
    clean_filter(root.path());
    clean_filter(&sub);
    modify_tracked(root.path());
    modify_tracked(&sub);
    let index = root.path().join(".git/index");
    let sub_index = root.path().join(".git/modules/sub/index");
    let before = std::fs::read(&index).unwrap();
    let sub_before = std::fs::read(&sub_index).unwrap();
    let output = git_status(root.path())
        .await
        .expect("isolated filter and submodule");
    let text = output["status"].as_str().unwrap();
    assert!(text.contains("tracked.txt"));
    assert!(text.contains("sub"));
    assert!(!root.path().join("filter-ran").exists());
    assert!(!sub.join("filter-ran").exists());
    assert_eq!(before, std::fs::read(index).unwrap());
    assert_eq!(sub_before, std::fs::read(sub_index).unwrap());
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial]
async fn git_status_bounds_output_without_mutating_repository() {
    if !strict_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let _home = super::tests::EnvironmentGuard::set("HOME", home.path().to_str().unwrap());
    init(root.path());
    for number in 0..700 {
        std::fs::write(
            root.path().join(format!("{number:04}-{}", "x".repeat(100))),
            "",
        )
        .unwrap();
    }
    assert!(git_status(root.path())
        .await
        .expect_err("bounded output")
        .contains("65536-byte limit"));
    assert!(!root.path().join(".nib/worktrees").exists());
    assert!(!root.path().join(".git/index").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial]
async fn git_status_isolates_process_filter_and_host_network() {
    if !strict_available() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("private.txt"), "synthetic private fixture").unwrap();
    let _home = super::tests::EnvironmentGuard::set("HOME", home.path().to_str().unwrap());
    let _secret = super::tests::EnvironmentGuard::set("NIB_GIT_STATUS_SECRET", "synthetic");
    let root = tempfile::tempdir().unwrap();
    tracked_fixture(root.path());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let source = PROCESS_FILTER.replace("HOST_PORT", &port.to_string());
    script(&root.path().join("process-filter"), &source);
    git(
        root.path(),
        &["config", "filter.hostile.process", "./process-filter"],
    );
    git(root.path(), &["config", "filter.hostile.required", "true"]);
    modify_tracked(root.path());
    let before = std::fs::read(root.path().join(".git/index")).unwrap();
    let output = git_status(root.path())
        .await
        .expect("isolated process filter");
    // The filter returns the committed content: this proves Git executed the
    // protocol fixture and accepted its result inside the isolation boundary.
    assert!(!output["status"].as_str().unwrap().contains("tracked.txt"));
    assert!(!root.path().join("process-ran").exists());
    assert_eq!(
        before,
        std::fs::read(root.path().join(".git/index")).unwrap()
    );
}

#[cfg(target_os = "linux")]
const PROCESS_FILTER: &str = r"#!/usr/bin/python3
import os, socket, sys
if os.getenv('NIB_GIT_STATUS_SECRET') or os.path.isfile(os.path.join(os.environ['HOME'], 'private.txt')):
    sys.exit(21)
try:
    open('process-ran', 'w').close()
    sys.exit(22)
except OSError:
    pass
try:
    socket.create_connection(('127.0.0.1', HOST_PORT), timeout=0.5).close()
    sys.exit(23)
except OSError:
    pass
def packet():
    header = sys.stdin.buffer.read(4)
    if not header:
        sys.exit(0)
    size = int(header, 16)
    return None if size == 0 else sys.stdin.buffer.read(size - 4)
def section():
    result = []
    while True:
        item = packet()
        if item is None:
            return result
        result.append(item)
def write(data):
    sys.stdout.buffer.write(('%04x' % (len(data) + 4)).encode() + data)
def flush():
    sys.stdout.buffer.write(b'0000')
    sys.stdout.buffer.flush()
section()
write(b'git-filter-server\n')
write(b'version=2\n')
flush()
section()
write(b'capability=clean\n')
flush()
while True:
    section()
    section()
    write(b'status=success\n')
    flush()
    write(b'initial\n')
    flush()
    flush()
";

/// T080: managed session worktrees live under `<project>/.nib/worktrees`, and
/// the project is normally inside `$HOME`. Status must reach the common Git
/// directory while every other home path and nib state stay hidden.
#[cfg(target_os = "linux")]
#[tokio::test]
#[serial_test::serial]
async fn git_status_reports_managed_worktree_under_home() {
    if !strict_available() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = home.join("project");
    std::fs::create_dir_all(project.join(".nib")).unwrap();
    init(&project);
    std::fs::write(project.join("tracked.txt"), "initial\n").unwrap();
    std::fs::write(project.join(".gitignore"), ".nib/\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "--quiet", "-m", "fixture"]);
    std::fs::write(project.join(".nib/config.toml"), "api_key = \"secret\"\n").unwrap();
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
    let _home = super::tests::EnvironmentGuard::set("HOME", home.to_str().unwrap());
    modify_tracked(&worktree);
    let before = std::fs::read(project.join(".git/index")).unwrap();
    let output = git_status(&worktree).await.expect("isolated linked status");
    let status = output["status"].as_str().unwrap();
    assert!(status.contains("nib/session/s1"), "{status}");
    assert!(status.contains("tracked.txt"), "{status}");
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    let nested = git_status(&worktree.join("src"))
        .await
        .expect("status from a worktree subdirectory");
    assert!(nested["status"].as_str().unwrap().contains("tracked.txt"));
    assert_eq!(before, std::fs::read(project.join(".git/index")).unwrap());
}
