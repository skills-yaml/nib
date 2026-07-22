#![cfg(target_os = "linux")]

use nib::sandbox::process::{
    supervise_foreground_with_ready, CleanupLeaseState, ProcessIdentity, ProcessScopeBackend,
    ProcessScopeStatus, ProcessScopeStore, SupervisedCommand,
};
use serde_json::json;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const ROLE_ENV: &str = "NIB_PROCESS_SUPERVISOR_TEST_ROLE";
const ROOT_ENV: &str = "NIB_PROCESS_SUPERVISOR_TEST_ROOT";
const SCOPE_ID: &str = "fixture-owner-loss";

#[test]
fn abrupt_owner_group_loss_reaps_setsid_before_terminal_publication() {
    match std::env::var(ROLE_ENV).ok().as_deref() {
        Some("owner") => return owner_helper(),
        Some("supervisor") => return supervisor_helper(),
        _ => {}
    }
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }

    let root = tempdir().expect("fixture root");
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(root.path())
        .status()
        .expect("git init");
    let executable = std::env::current_exe().expect("test executable");
    let mut owner_command = Command::new(executable);
    owner_command
        .arg("--exact")
        .arg("abrupt_owner_group_loss_reaps_setsid_before_terminal_publication")
        .arg("--nocapture")
        .env(ROLE_ENV, "owner")
        .env(ROOT_ENV, root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut owner = owner_command.spawn().expect("owner process");

    wait_for_file(
        &root.path().join("supervisor.ready"),
        Duration::from_secs(10),
    );
    wait_for_file(
        &root.path().join("descendant.started"),
        Duration::from_secs(10),
    );
    let owner_group = i32::try_from(owner.id()).expect("owner pid");
    assert_eq!(unsafe { libc::kill(-owner_group, libc::SIGKILL) }, 0);
    owner.wait().expect("reap owner");

    wait_for_file(
        &root.path().join("terminal.published"),
        Duration::from_secs(10),
    );
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let scope = store.load(SCOPE_ID).expect("completed scope");
    assert_eq!(scope.status, ProcessScopeStatus::Complete);
    let proof = scope.cleanup_proof.as_ref().expect("cleanup proof");
    assert!(proof.descendants_reaped);
    assert_eq!(
        store.cleanup_lease_state(&scope).expect("cleanup lease"),
        CleanupLeaseState::Missing
    );
    let publication: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("terminal.published")).expect("terminal publication"),
    )
    .expect("terminal JSON");
    assert_eq!(publication["cleanup_lease_id"], proof.cleanup_lease_id);
    std::thread::sleep(Duration::from_millis(2200));
    assert!(!root.path().join("descendant.survived").exists());
}

#[allow(clippy::zombie_processes)]
fn owner_helper() {
    let root = fixture_root();
    let store = ProcessScopeStore::open(&root).expect("scope store");
    store
        .prepare(
            SCOPE_ID,
            "subagent",
            101,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let executable = std::env::current_exe().expect("test executable");
    let mut supervisor_command = Command::new(executable);
    supervisor_command
        .arg("--exact")
        .arg("abrupt_owner_group_loss_reaps_setsid_before_terminal_publication")
        .arg("--nocapture")
        .env(ROLE_ENV, "supervisor")
        .env(ROOT_ENV, &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    // The parent test deliberately SIGKILLs this owner process to exercise the
    // no-destructor path; the independent supervisor is reaped by init.
    let mut supervisor = supervisor_command.spawn().expect("supervisor process");
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
    let descendant = root.join("descendant.started");
    let survived = root.join("descendant.survived");
    let command = format!(
        "setsid sh -c 'printf started > {}; sleep 2; printf survived > {}' & wait",
        descendant.display(),
        survived.display()
    );
    let ready = root.join("supervisor.ready");
    let output = supervise_foreground_with_ready(
        &store,
        &prepared,
        std::io::stdin(),
        SupervisedCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
            cwd: root.clone(),
            stdin: Vec::new(),
            environment: Vec::new(),
        },
        |_| {
            std::fs::write(&ready, b"ready")
                .map_err(|error| format!("failed to publish readiness: {error}"))
        },
    )
    .expect("supervised fixture output");
    let publication = serde_json::to_vec_pretty(&json!({
        "outcome": output.cleanup_proof.outcome,
        "cleanup_lease_id": output.cleanup_proof.cleanup_lease_id,
    }))
    .expect("terminal JSON");
    let mut terminal =
        std::fs::File::create(root.join("terminal.published")).expect("terminal publication file");
    terminal
        .write_all(&publication)
        .expect("terminal publication");
    terminal.sync_all().expect("durable terminal publication");
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
