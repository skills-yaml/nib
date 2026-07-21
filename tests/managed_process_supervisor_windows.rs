#![cfg(windows)]

use nib::sandbox::process::{
    supervise_foreground_with_ready, CleanupLeaseState, ProcessIdentity, ProcessScopeBackend,
    ProcessScopeStatus, ProcessScopeStore, SupervisedCommand,
};
use serde_json::json;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const ROLE_ENV: &str = "NIB_WINDOWS_PROCESS_SUPERVISOR_TEST_ROLE";
const ROOT_ENV: &str = "NIB_WINDOWS_PROCESS_SUPERVISOR_TEST_ROOT";
const SCOPE_ID: &str = "fixture-windows-owner-loss";
const TEST_NAME: &str = "abrupt_owner_loss_reaps_job_descendant_before_terminal_publication";

#[tokio::test]
async fn production_delegation_rejects_windows_before_creating_state() {
    let root = tempdir().expect("fixture root");
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root.path())
        .status()
        .expect("git init")
        .success());
    let error = nib::tools::delegation::spawn_subagent(
        &serde_json::json!({"prompt": "must not start"}),
        root.path(),
    )
    .expect_err("Windows production delegation must fail closed");
    assert!(error.contains("unavailable on Windows"), "{error}");
    assert!(!root.path().join(".nib/worktrees").exists());
    assert!(!root.path().join(".nib/subagents").exists());
    assert!(!root.path().join(".nib/process-scopes").exists());
}

#[test]
fn abrupt_owner_loss_reaps_job_descendant_before_terminal_publication() {
    match std::env::var(ROLE_ENV).ok().as_deref() {
        Some("owner") => return owner_helper(),
        Some("supervisor") => return supervisor_helper(),
        Some("worker") => return worker_helper(),
        Some("descendant") => return descendant_helper(),
        _ => {}
    }

    let root = tempdir().expect("fixture root");
    let executable = std::env::current_exe().expect("test executable");
    let mut owner = Command::new(executable)
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(ROLE_ENV, "owner")
        .env(ROOT_ENV, root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("owner process");

    wait_for_file(
        &root.path().join("supervisor.ready"),
        Duration::from_secs(10),
    );
    wait_for_file(
        &root.path().join("descendant.started"),
        Duration::from_secs(10),
    );
    owner.kill().expect("kill owner");
    owner.wait().expect("reap owner");

    assert_terminal_after_cleanup(root.path());
}

fn owner_helper() {
    let root = fixture_root();
    let store = ProcessScopeStore::open(&root).expect("scope store");
    store
        .prepare(
            SCOPE_ID,
            "subagent",
            301,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::WindowsJobObject,
        )
        .expect("prepare scope");
    let executable = std::env::current_exe().expect("test executable");
    let mut supervisor = Command::new(executable)
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(ROLE_ENV, "supervisor")
        .env(ROOT_ENV, &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("supervisor process");
    let _owner_lifetime = supervisor.stdin.take().expect("owner lifetime pipe");
    loop {
        std::hint::black_box(&supervisor);
        std::hint::black_box(&_owner_lifetime);
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn supervisor_helper() {
    let root = fixture_root();
    let store = ProcessScopeStore::open(&root).expect("scope store");
    let prepared = store.load(SCOPE_ID).expect("prepared scope");
    let executable = std::env::current_exe().expect("test executable");
    let output = supervise_foreground_with_ready(
        &store,
        &prepared,
        std::io::stdin(),
        SupervisedCommand {
            program: executable,
            args: vec![
                OsString::from("--exact"),
                OsString::from(TEST_NAME),
                OsString::from("--nocapture"),
            ],
            cwd: root.clone(),
            stdin: Vec::new(),
            environment: vec![
                (OsString::from(ROLE_ENV), OsString::from("worker")),
                (OsString::from(ROOT_ENV), root.as_os_str().to_owned()),
            ],
        },
        |_| {
            std::fs::write(root.join("supervisor.ready"), b"ready")
                .map_err(|error| format!("failed to publish readiness: {error}"))
        },
    );
    match output {
        Ok(output) => publish_terminal(&root, &output.cleanup_proof.cleanup_lease_id),
        Err(error) => publish_supervisor_failure(&root, &error),
    }
}

fn worker_helper() {
    let root = fixture_root();
    let executable = std::env::current_exe().expect("test executable");
    let mut descendant = Command::new(executable)
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(ROLE_ENV, "descendant")
        .env(ROOT_ENV, &root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Job-contained descendant");
    let _ = descendant.wait();
}

fn descendant_helper() {
    let root = fixture_root();
    std::fs::write(root.join("descendant.started"), b"started").expect("descendant ready");
    std::thread::sleep(Duration::from_secs(2));
    std::fs::write(root.join("descendant.survived"), b"survived")
        .expect("descendant survival marker");
}

fn publish_terminal(root: &Path, cleanup_lease_id: &str) {
    let publication = serde_json::to_vec_pretty(&json!({
        "cleanup_lease_id": cleanup_lease_id,
    }))
    .expect("terminal JSON");
    let mut terminal =
        std::fs::File::create(root.join("terminal.published")).expect("terminal publication");
    terminal.write_all(&publication).expect("write terminal");
    terminal.sync_all().expect("durable terminal publication");
}

fn publish_supervisor_failure(root: &Path, error: &str) {
    let pending = root.join("supervisor.failed.pending");
    let published = root.join("supervisor.failed");
    let mut failure = std::fs::File::create(&pending).expect("supervisor failure staging");
    failure
        .write_all(error.as_bytes())
        .expect("write supervisor failure");
    failure.sync_all().expect("durable supervisor failure");
    drop(failure);
    std::fs::rename(&pending, &published).expect("supervisor failure publication");
}

fn assert_terminal_after_cleanup(root: &Path) {
    wait_for_terminal_or_failure(root, Duration::from_secs(30));
    let store = ProcessScopeStore::open(root).expect("scope store");
    let scope = store.load(SCOPE_ID).expect("completed scope");
    assert_eq!(scope.status, ProcessScopeStatus::Complete);
    assert_eq!(scope.backend, ProcessScopeBackend::WindowsJobObject);
    let proof = scope.cleanup_proof.as_ref().expect("cleanup proof");
    assert!(proof.descendants_reaped);
    assert_eq!(proof.outcome, "owner_eof");
    assert_eq!(
        store.cleanup_lease_state(&scope).expect("cleanup lease"),
        CleanupLeaseState::Missing
    );
    let publication: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("terminal.published")).expect("terminal publication"),
    )
    .expect("terminal JSON");
    assert_eq!(publication["cleanup_lease_id"], proof.cleanup_lease_id);
    std::thread::sleep(Duration::from_millis(2200));
    assert!(!root.join("descendant.survived").exists());
}

fn wait_for_terminal_or_failure(root: &Path, timeout: Duration) {
    let terminal = root.join("terminal.published");
    let failure = root.join("supervisor.failed");
    let deadline = Instant::now() + timeout;
    while !terminal.is_file() && !failure.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if failure.is_file() {
        let error = std::fs::read_to_string(&failure).expect("supervisor failure");
        panic!("managed-process supervisor failed: {error}");
    }
    assert!(
        terminal.is_file(),
        "timed out waiting for {}",
        terminal.display()
    );
}

fn fixture_root() -> PathBuf {
    PathBuf::from(std::env::var_os(ROOT_ENV).expect("fixture root environment"))
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.is_file(), "timed out waiting for {}", path.display());
}
