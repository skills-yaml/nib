use super::*;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

pub(crate) const LOCK_CHILD_PATH: &str = "NIB_DAEMON_LOCK_CHILD_PATH";
pub(crate) const LOCK_CHILD_READY: &str = "NIB_DAEMON_LOCK_CHILD_READY";
pub(crate) const LOCK_CHILD_ENTERED: &str = "NIB_DAEMON_LOCK_CHILD_ENTERED";
pub(crate) const LOCK_CHILD_EXPECTATION: &str = "NIB_DAEMON_LOCK_CHILD_EXPECTATION";
#[cfg(unix)]
pub(crate) const ATOMIC_CRASH_CHILD_ROOT: &str = "NIB_ATOMIC_CRASH_CHILD_ROOT";
#[cfg(unix)]
pub(crate) const ATOMIC_CRASH_CHILD_MODE: &str = "NIB_ATOMIC_CRASH_CHILD_MODE";
#[cfg(unix)]
pub(crate) const ATOMIC_CRASH_CHILD_READY: &str = "NIB_ATOMIC_CRASH_CHILD_READY";

pub(crate) fn namespace_snapshot(path: &Path) -> Vec<(OsString, Vec<u8>)> {
    let mut snapshot = fs::read_dir(path)
        .expect("read namespace")
        .map(|entry| {
            let entry = entry.expect("namespace entry");
            let name = entry.file_name();
            let bytes = fs::read(entry.path()).expect("namespace file bytes");
            (name, bytes)
        })
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

#[cfg(any(unix, windows))]
pub(crate) fn spawn_lock_child(
    lock_path: &Path,
    ready_path: &Path,
    entered_path: &Path,
    expectation: &str,
) -> Child {
    let _ = fs::remove_file(ready_path);
    let _ = fs::remove_file(entered_path);
    Command::new(std::env::current_exe().expect("current test binary"))
        .args([
            "--exact",
            "daemons::state::tests::test_part_0::daemon_file_lock_replacement_child_process",
            "--nocapture",
        ])
        .env(LOCK_CHILD_PATH, lock_path)
        .env(LOCK_CHILD_READY, ready_path)
        .env(LOCK_CHILD_ENTERED, entered_path)
        .env(LOCK_CHILD_EXPECTATION, expectation)
        .spawn()
        .expect("spawn daemon lock child")
}

#[cfg(unix)]
pub(crate) fn run_identity_failure_child(lock_path: &Path, ready_path: &Path, entered_path: &Path) {
    let status = spawn_lock_child(lock_path, ready_path, entered_path, "identity")
        .wait()
        .expect("wait for daemon lock child");
    assert!(status.success(), "identity failure child failed: {status}");
    assert!(ready_path.exists(), "identity child did not run");
    assert!(!entered_path.exists(), "identity failure entered operation");
}

#[cfg(unix)]
pub(crate) fn assert_child_remains_blocked(
    child: &mut Child,
    ready_path: &Path,
    entered_path: &Path,
) {
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    while !ready_path.exists() {
        if let Some(status) = child.try_wait().expect("inspect daemon lock child") {
            panic!("daemon lock child exited before readiness: {status}");
        }
        assert!(
            Instant::now() < ready_deadline,
            "daemon lock child did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let blocked_until = Instant::now() + Duration::from_millis(250);
    while Instant::now() < blocked_until {
        assert!(
            !entered_path.exists(),
            "contender entered while lock was held"
        );
        if let Some(status) = child.try_wait().expect("inspect blocked lock child") {
            panic!("daemon lock child failed instead of blocking: {status}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("terminate blocked lock child");
    child.wait().expect("reap blocked lock child");
    assert!(
        !entered_path.exists(),
        "terminated contender entered operation"
    );
}

#[cfg(unix)]
pub(crate) fn run_atomic_crash_child(root: &Path) {
    let ready = PathBuf::from(
        std::env::var_os(ATOMIC_CRASH_CHILD_READY)
            .expect("atomic crash child ready path must be configured"),
    );
    let mode =
        std::env::var(ATOMIC_CRASH_CHILD_MODE).expect("atomic crash child mode must be configured");
    let target = root.join("record.json");
    let directory = StableDirectory::open(root).expect("atomic crash child capability");
    let expected = directory
        .open_read(&target)
        .expect("atomic crash child expected target");
    match mode.as_str() {
        "before" => {
            let _ = directory.save_bytes_atomically_expected_with_recovery_hooks(
                &target,
                b"new-state",
                ".child-crash-",
                AtomicSaveExpectation {
                    require_attached_before_commit: true,
                    file: FileExpectation::Present(&expected),
                    retain_publication_lock: false,
                },
                || -> Result<(), String> {
                    fs::write(&ready, b"ready").expect("publish atomic crash readiness");
                    let deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        if Instant::now() >= deadline {
                            return Err("atomic crash child timed out".to_string());
                        }
                        thread::sleep(Duration::from_secs(1));
                    }
                },
                || {},
            );
        }
        "after" => {
            let _ = directory.save_bytes_atomically_expected_with_recovery_hooks(
                &target,
                b"new-state",
                ".child-crash-",
                AtomicSaveExpectation {
                    require_attached_before_commit: true,
                    file: FileExpectation::Present(&expected),
                    retain_publication_lock: false,
                },
                || Ok(()),
                || {
                    fs::write(&ready, b"ready").expect("publish atomic crash readiness");
                    let deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        assert!(Instant::now() < deadline, "atomic crash child timed out");
                        thread::sleep(Duration::from_secs(1));
                    }
                },
            );
        }
        value => panic!("unsupported atomic crash child mode: {value}"),
    }
    panic!("atomic crash child unexpectedly left its commit barrier");
}

#[cfg(unix)]
pub(crate) fn spawn_atomic_crash_child(root: &Path, mode: &str, ready: &Path) -> Child {
    let _ = fs::remove_file(ready);
    Command::new(std::env::current_exe().expect("current test binary"))
        .args([
            "--exact",
            "daemons::state::tests::test_part_1::real_child_atomic_fsync_crash_recovery_matrix",
            "--nocapture",
        ])
        .env(ATOMIC_CRASH_CHILD_ROOT, root)
        .env(ATOMIC_CRASH_CHILD_MODE, mode)
        .env(ATOMIC_CRASH_CHILD_READY, ready)
        .spawn()
        .expect("spawn atomic crash child")
}

#[cfg(unix)]
pub(crate) fn wait_for_atomic_child(child: &mut Child, ready: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect atomic crash child") {
            panic!("atomic crash child exited before readiness: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "atomic crash child did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[path = "test_part_0.rs"]
mod test_part_0;
#[path = "test_part_1.rs"]
mod test_part_1;
