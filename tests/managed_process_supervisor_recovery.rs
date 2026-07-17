#![cfg(target_os = "linux")]

use nib::sandbox::process::{
    supervise_foreground_with_ready, CleanupLeaseState, ProcessIdentity, ProcessScopeBackend,
    ProcessScopeStatus, ProcessScopeStore, SupervisedCommand,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::tempdir;

const ROLE_ENV: &str = "NIB_PROCESS_RECOVERY_TEST_ROLE";
const ROOT_ENV: &str = "NIB_PROCESS_RECOVERY_TEST_ROOT";
const PRE_RUNNING_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_PRE_RUNNING_PAUSE";
const PRE_SPAWN_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_PRE_SPAWN_PAUSE";
const POST_BWRAP_SPAWN_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_POST_BWRAP_SPAWN_PAUSE";
const SCOPE_ID: &str = "fixture-supervisor-loss";
const EARLY_SCOPE_ID: &str = "fixture-supervisor-loss-before-init";
const PRE_SPAWN_SCOPE_ID: &str = "fixture-supervisor-loss-before-spawn";
const POST_SPAWN_SCOPE_ID: &str = "fixture-supervisor-loss-before-handshake";
const TEST_NAME: &str = "crashed_supervisor_is_recovered_only_after_pid_namespace_exit";
const EARLY_TEST_NAME: &str = "crashed_supervisor_before_running_publication_aborts_gated_workload";
const PRE_SPAWN_TEST_NAME: &str =
    "crashed_supervisor_before_spawn_proves_gated_workload_never_launched";
const POST_SPAWN_TEST_NAME: &str =
    "crashed_supervisor_after_bwrap_spawn_proves_gated_workload_never_launched";

#[test]
fn crashed_supervisor_is_recovered_only_after_pid_namespace_exit() {
    if std::env::var(ROLE_ENV).ok().as_deref() == Some("supervisor") {
        return supervisor_helper();
    }
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }

    let root = tempdir().expect("fixture root");
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    store
        .prepare(
            SCOPE_ID,
            "subagent",
            401,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");

    let executable = std::env::current_exe().expect("test executable");
    let mut supervisor = Command::new(executable)
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(ROLE_ENV, "supervisor")
        .env(ROOT_ENV, root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("supervisor process");
    let _owner_lifetime = supervisor.stdin.take().expect("owner lifetime pipe");

    wait_for_file(
        &root.path().join("supervisor.ready"),
        Duration::from_secs(10),
    );
    wait_for_file(
        &root.path().join("descendant.started"),
        Duration::from_secs(10),
    );
    let running = wait_for_scope_status(
        &store,
        SCOPE_ID,
        ProcessScopeStatus::Running,
        Duration::from_secs(10),
    );
    assert_eq!(
        store
            .cleanup_lease_state(&running)
            .expect("live cleanup lease"),
        CleanupLeaseState::Live
    );
    let namespace_init = running
        .direct_child
        .clone()
        .expect("registered namespace root");
    let mut namespace_guard = ProcessKillGuard::new(namespace_init.clone());
    assert_eq!(
        unsafe { libc::kill(namespace_init.pid as i32, libc::SIGSTOP) },
        0,
        "stop the namespace root so parent-death cleanup cannot win the recovery race"
    );

    assert_eq!(
        unsafe { libc::kill(supervisor.id() as i32, libc::SIGKILL) },
        0
    );
    supervisor.wait().expect("reap crashed supervisor");
    assert!(
        namespace_init.still_matches(),
        "the exact namespace root must still be live when recovery begins"
    );
    let interrupted = store.load(SCOPE_ID).expect("interrupted scope");
    assert_eq!(
        store
            .cleanup_lease_state(&interrupted)
            .expect("recoverable cleanup lease"),
        CleanupLeaseState::Recoverable
    );

    let recovered = store
        .recover_linux_supervisor_loss(&interrupted)
        .expect("recover Linux PID namespace");
    assert_eq!(recovered.status, ProcessScopeStatus::Complete);
    let proof = recovered.cleanup_proof.as_ref().expect("cleanup proof");
    assert_eq!(proof.outcome, "supervisor_lost_linux_pid_namespace");
    assert!(proof.descendants_reaped);
    assert_eq!(
        store
            .cleanup_lease_state(&recovered)
            .expect("released cleanup lease"),
        CleanupLeaseState::Missing
    );
    wait_for_identity_exit(&namespace_init, Duration::from_secs(10));
    namespace_guard.disarm();

    std::thread::sleep(Duration::from_millis(2200));
    assert!(!root.path().join("descendant.survived").exists());
}

#[test]
fn crashed_supervisor_before_running_publication_aborts_gated_workload() {
    if std::env::var(ROLE_ENV).ok().as_deref() == Some("early-supervisor") {
        return early_supervisor_helper();
    }
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }

    let root = tempdir().expect("fixture root");
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    store
        .prepare(
            EARLY_SCOPE_ID,
            "subagent",
            402,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");

    let executable = std::env::current_exe().expect("test executable");
    let pre_running_marker = root.path().join("supervisor.pre-running.json");
    let mut supervisor = Command::new(executable)
        .args(["--exact", EARLY_TEST_NAME, "--nocapture"])
        .env(ROLE_ENV, "early-supervisor")
        .env(ROOT_ENV, root.path())
        .env(PRE_RUNNING_PAUSE_ENV, &pre_running_marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("supervisor process");
    let _owner_lifetime = supervisor.stdin.take().expect("owner lifetime pipe");

    wait_for_file(&pre_running_marker, Duration::from_secs(10));
    let namespace_init: ProcessIdentity = serde_json::from_slice(
        &std::fs::read(&pre_running_marker).expect("pre-running namespace marker"),
    )
    .expect("pre-running namespace identity");
    let namespace_init_pid = namespace_init.pid;
    let outer_pid = process_parent_pid(namespace_init_pid);
    let outer_identity = ProcessIdentity::capture(outer_pid).expect("outer bwrap identity");
    let mut namespace_guard = ProcessKillGuard::new(namespace_init.clone());
    let namespace_pids = namespace_pids(namespace_init_pid);
    let outer_parent_is_supervisor = process_parent_pid(outer_pid) == supervisor.id();
    let namespace_pid_one = namespace_pids.len() >= 2
        && namespace_pids.first() == Some(&namespace_init_pid)
        && namespace_pids.last() == Some(&1);
    let prepared = store.load(EARLY_SCOPE_ID).expect("prepared scope");
    assert_eq!(prepared.status, ProcessScopeStatus::Prepared);
    assert!(prepared.supervisor.is_some());
    assert_eq!(prepared.direct_child.as_ref(), Some(&namespace_init));
    assert!(!root.path().join("descendant.started").exists());

    assert_eq!(
        unsafe { libc::kill(supervisor.id() as i32, libc::SIGKILL) },
        0
    );
    supervisor.wait().expect("reap crashed supervisor");
    let interrupted = store.load(EARLY_SCOPE_ID).expect("interrupted scope");
    assert_eq!(interrupted.status, ProcessScopeStatus::Prepared);
    assert_eq!(
        store
            .cleanup_lease_state(&interrupted)
            .expect("recoverable cleanup lease"),
        CleanupLeaseState::Recoverable
    );
    let recovered = store
        .recover_linux_supervisor_loss(&interrupted)
        .expect("recover gated Linux PID namespace");
    assert_eq!(recovered.status, ProcessScopeStatus::Complete);
    assert!(recovered.cleanup_proof.is_none());
    let proof = recovered
        .launch_abort_proof
        .as_ref()
        .expect("launch-abort proof");
    assert_eq!(proof.outcome, "gate_eof_before_running");
    assert!(proof.workload_never_launched);
    assert_eq!(proof.namespace_root.as_ref(), Some(&namespace_init));
    assert_eq!(
        store
            .cleanup_lease_state(&recovered)
            .expect("released launch-abort lease"),
        CleanupLeaseState::Missing
    );
    wait_for_identity_exit(&outer_identity, Duration::from_secs(10));
    wait_for_identity_exit(&namespace_init, Duration::from_secs(10));
    namespace_guard.disarm();

    assert!(
        outer_parent_is_supervisor,
        "the gated namespace init must be parented by the supervisor's bwrap monitor"
    );
    assert!(
        namespace_pid_one,
        "the gated scope root must be PID 1 in the nested namespace: {namespace_pids:?}"
    );
    std::thread::sleep(Duration::from_millis(2200));
    assert!(!root.path().join("descendant.started").exists());
    assert!(!root.path().join("descendant.survived").exists());
}

#[test]
fn crashed_supervisor_before_spawn_proves_gated_workload_never_launched() {
    if std::env::var(ROLE_ENV).ok().as_deref() == Some("pre-spawn-supervisor") {
        return pre_spawn_supervisor_helper();
    }
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }

    let root = tempdir().expect("fixture root");
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    store
        .prepare(
            PRE_SPAWN_SCOPE_ID,
            "subagent",
            403,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");

    let executable = std::env::current_exe().expect("test executable");
    let pre_spawn_marker = root.path().join("supervisor.pre-spawn.json");
    let mut supervisor = Command::new(executable)
        .args(["--exact", PRE_SPAWN_TEST_NAME, "--nocapture"])
        .env(ROLE_ENV, "pre-spawn-supervisor")
        .env(ROOT_ENV, root.path())
        .env(PRE_SPAWN_PAUSE_ENV, &pre_spawn_marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("supervisor process");
    let _owner_lifetime = supervisor.stdin.take().expect("owner lifetime pipe");

    wait_for_file(&pre_spawn_marker, Duration::from_secs(10));
    let supervisor_identity: ProcessIdentity = serde_json::from_slice(
        &std::fs::read(&pre_spawn_marker).expect("pre-spawn supervisor marker"),
    )
    .expect("pre-spawn supervisor identity");
    let prepared = store.load(PRE_SPAWN_SCOPE_ID).expect("prepared scope");
    assert_eq!(prepared.status, ProcessScopeStatus::Prepared);
    assert_eq!(prepared.supervisor.as_ref(), Some(&supervisor_identity));
    assert!(prepared.direct_child.is_none());
    assert!(!root.path().join("descendant.started").exists());

    assert_eq!(
        unsafe { libc::kill(supervisor.id() as i32, libc::SIGKILL) },
        0
    );
    supervisor.wait().expect("reap crashed supervisor");
    let interrupted = store.load(PRE_SPAWN_SCOPE_ID).expect("interrupted scope");
    assert_eq!(
        store
            .cleanup_lease_state(&interrupted)
            .expect("recoverable cleanup lease"),
        CleanupLeaseState::Recoverable
    );

    let recovered = store
        .recover_linux_supervisor_loss(&interrupted)
        .expect("recover pre-spawn launch");
    assert_eq!(recovered.status, ProcessScopeStatus::Complete);
    assert!(recovered.cleanup_proof.is_none());
    let proof = recovered
        .launch_abort_proof
        .as_ref()
        .expect("pre-spawn launch-abort proof");
    assert_eq!(proof.supervisor, supervisor_identity);
    assert!(proof.namespace_root.is_none());
    assert_eq!(proof.outcome, "gate_eof_before_running");
    assert!(proof.workload_never_launched);
    assert_eq!(
        store
            .cleanup_lease_state(&recovered)
            .expect("released launch-abort lease"),
        CleanupLeaseState::Missing
    );
    assert!(!root.path().join("descendant.started").exists());
}

#[test]
fn crashed_supervisor_after_bwrap_spawn_proves_gated_workload_never_launched() {
    if std::env::var(ROLE_ENV).ok().as_deref() == Some("post-spawn-supervisor") {
        return post_spawn_supervisor_helper();
    }
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }

    let root = tempdir().expect("fixture root");
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    store
        .prepare(
            POST_SPAWN_SCOPE_ID,
            "subagent",
            404,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");

    let executable = std::env::current_exe().expect("test executable");
    let post_spawn_marker = root.path().join("supervisor.post-spawn.json");
    let mut supervisor = Command::new(executable)
        .args(["--exact", POST_SPAWN_TEST_NAME, "--nocapture"])
        .env(ROLE_ENV, "post-spawn-supervisor")
        .env(ROOT_ENV, root.path())
        .env(POST_BWRAP_SPAWN_PAUSE_ENV, &post_spawn_marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("supervisor process");
    let _owner_lifetime = supervisor.stdin.take().expect("owner lifetime pipe");

    wait_for_file(&post_spawn_marker, Duration::from_secs(10));
    let outer_monitor: ProcessIdentity = serde_json::from_slice(
        &std::fs::read(&post_spawn_marker).expect("post-spawn monitor marker"),
    )
    .expect("post-spawn monitor identity");
    assert!(outer_monitor.still_matches());
    assert_eq!(process_parent_pid(outer_monitor.pid), supervisor.id());
    let prepared = store.load(POST_SPAWN_SCOPE_ID).expect("prepared scope");
    assert_eq!(prepared.status, ProcessScopeStatus::Prepared);
    assert!(prepared.supervisor.is_some());
    assert!(prepared.direct_child.is_none());
    assert!(!root.path().join("descendant.started").exists());

    assert_eq!(
        unsafe { libc::kill(supervisor.id() as i32, libc::SIGKILL) },
        0
    );
    supervisor.wait().expect("reap crashed supervisor");
    wait_for_identity_exit(&outer_monitor, Duration::from_secs(10));
    let interrupted = store.load(POST_SPAWN_SCOPE_ID).expect("interrupted scope");
    assert_eq!(
        store
            .cleanup_lease_state(&interrupted)
            .expect("recoverable cleanup lease"),
        CleanupLeaseState::Recoverable
    );

    let recovered = store
        .recover_linux_supervisor_loss(&interrupted)
        .expect("recover post-spawn launch");
    assert_eq!(recovered.status, ProcessScopeStatus::Complete);
    assert!(recovered.cleanup_proof.is_none());
    let proof = recovered
        .launch_abort_proof
        .as_ref()
        .expect("post-spawn launch-abort proof");
    assert!(proof.namespace_root.is_none());
    assert_eq!(proof.outcome, "gate_eof_before_running");
    assert!(proof.workload_never_launched);
    assert_eq!(
        store
            .cleanup_lease_state(&recovered)
            .expect("released launch-abort lease"),
        CleanupLeaseState::Missing
    );
    std::thread::sleep(Duration::from_millis(2200));
    assert!(!root.path().join("descendant.started").exists());
    assert!(!root.path().join("descendant.survived").exists());
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
    let _ = supervise_foreground_with_ready(
        &store,
        &prepared,
        std::io::stdin(),
        SupervisedCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
            cwd: root,
            stdin: Vec::new(),
            environment: Vec::new(),
        },
        |_| {
            std::fs::write(&ready, b"ready")
                .map_err(|error| format!("failed to publish readiness: {error}"))
        },
    );
}

fn early_supervisor_helper() {
    let root = fixture_root();
    let store = ProcessScopeStore::open(&root).expect("scope store");
    let prepared = store.load(EARLY_SCOPE_ID).expect("prepared scope");
    let descendant = root.join("descendant.started");
    let survived = root.join("descendant.survived");
    let command = format!(
        "setsid sh -c 'printf started > {}; sleep 2; printf survived > {}' & wait",
        descendant.display(),
        survived.display()
    );
    let _ = supervise_foreground_with_ready(
        &store,
        &prepared,
        std::io::stdin(),
        SupervisedCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
            cwd: root,
            stdin: Vec::new(),
            environment: Vec::new(),
        },
        |_| Ok(()),
    );
}

fn pre_spawn_supervisor_helper() {
    let root = fixture_root();
    let store = ProcessScopeStore::open(&root).expect("scope store");
    let prepared = store.load(PRE_SPAWN_SCOPE_ID).expect("prepared scope");
    let descendant = root.join("descendant.started");
    let command = format!("printf started > {}", descendant.display());
    let _ = supervise_foreground_with_ready(
        &store,
        &prepared,
        std::io::stdin(),
        SupervisedCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
            cwd: root,
            stdin: Vec::new(),
            environment: Vec::new(),
        },
        |_| Ok(()),
    );
}

fn post_spawn_supervisor_helper() {
    let root = fixture_root();
    let store = ProcessScopeStore::open(&root).expect("scope store");
    let prepared = store.load(POST_SPAWN_SCOPE_ID).expect("prepared scope");
    let descendant = root.join("descendant.started");
    let survived = root.join("descendant.survived");
    let command = format!(
        "printf started > {}; sleep 2; printf survived > {}",
        descendant.display(),
        survived.display()
    );
    let _ = supervise_foreground_with_ready(
        &store,
        &prepared,
        std::io::stdin(),
        SupervisedCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
            cwd: root,
            stdin: Vec::new(),
            environment: Vec::new(),
        },
        |_| Ok(()),
    );
}

fn fixture_root() -> PathBuf {
    PathBuf::from(std::env::var_os(ROOT_ENV).expect("fixture root environment"))
}

fn wait_for_scope_status(
    store: &ProcessScopeStore,
    scope_id: &str,
    status: ProcessScopeStatus,
    timeout: Duration,
) -> nib::sandbox::process::ProcessScopeRecord {
    let deadline = Instant::now() + timeout;
    loop {
        let scope = store.load(scope_id).expect("scope record");
        if scope.status == status {
            return scope;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {status:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn process_parent_pid(pid: u32) -> u32 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("process status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .expect("parent pid data")
        .trim()
        .parse::<u32>()
        .expect("valid parent pid")
}

fn namespace_pids(pid: u32) -> Vec<u32> {
    let status =
        std::fs::read_to_string(format!("/proc/{pid}/status")).expect("namespace init status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .expect("namespace pid data")
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid namespace pids")
}

fn wait_for_identity_exit(identity: &ProcessIdentity, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while identity.still_matches() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !identity.still_matches(),
        "timed out waiting for process {} to exit",
        identity.pid
    );
}

struct ProcessKillGuard {
    identity: Option<ProcessIdentity>,
}

impl ProcessKillGuard {
    fn new(identity: ProcessIdentity) -> Self {
        Self {
            identity: Some(identity),
        }
    }

    fn disarm(&mut self) {
        self.identity = None;
    }
}

impl Drop for ProcessKillGuard {
    fn drop(&mut self) {
        let Some(identity) = self.identity.as_ref() else {
            return;
        };
        if identity.still_matches() {
            let _ = unsafe { libc::kill(identity.pid as i32, libc::SIGKILL) };
        }
    }
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.is_file(), "timed out waiting for {}", path.display());
}
