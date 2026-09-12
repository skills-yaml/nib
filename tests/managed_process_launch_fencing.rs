#![cfg(all(target_os = "linux", debug_assertions))]

use nib::sandbox::process::{ProcessIdentity, ProcessScopeStatus, ProcessScopeStore};
use nib::tools::delegation::{cancel_subagent, list_subagents, spawn_subagent, SubagentRecord};
use serde_json::json;
use serial_test::serial;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const FAILPOINT_ENV: &str = "NIB_TEST_SUBAGENT_LAUNCH_FAILPOINT";
const PRE_RUNNING_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_PRE_RUNNING_PAUSE";
const READY_CRASH_PROJECT_ENV: &str = "NIB_TEST_READY_CRASH_PROJECT";
const READY_BEFORE_COMMIT_ENV: &str = "NIB_TEST_SUBAGENT_READY_BEFORE_COMMIT_PATH";
const WORKER_STARTED_ENV: &str = "NIB_TEST_SUBAGENT_WORKER_STARTED_PATH";
const WORKER_DELAY_MS_ENV: &str = "NIB_TEST_SUBAGENT_WORKER_DELAY_MS";
const SCOPE_PREPARED_ENV: &str = "NIB_TEST_SUBAGENT_SCOPE_PREPARED_PATH";
const SUPERVISOR_PRE_REGISTER_ENV: &str = "NIB_TEST_SUBAGENT_SUPERVISOR_PRE_REGISTER_PATH";
const SUPERVISOR_REGISTER_RELEASE_ENV: &str = "NIB_TEST_SUBAGENT_SUPERVISOR_REGISTER_RELEASE";

struct EnvironmentGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(key: &'static str, value: &OsStr) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn ready_before_commit_crash_child() {
    let Some(project_root) = std::env::var_os(READY_CRASH_PROJECT_ENV) else {
        return;
    };
    std::env::set_var("NIB_EXECUTABLE", env!("CARGO_BIN_EXE_nib"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("READY crash child runtime");
    let result = runtime.block_on(async {
        spawn_subagent(
            &json!({"prompt": "production READY-before-COMMIT crash fixture", "max_steps": 3}),
            &PathBuf::from(project_root),
        )
    });
    panic!("READY-before-COMMIT crash child unexpectedly returned: {result:?}");
}

#[test]
#[serial]
fn parent_sigkill_after_ready_never_releases_worker_and_restart_cleans_everything() {
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }
    let project = git_project();
    let ready = project.path().join("supervisor-ready-before-commit");
    let worker_started = project.path().join("worker-started");
    let mut parent = Command::new(std::env::current_exe().expect("integration test executable"))
        .args(["--exact", "ready_before_commit_crash_child", "--nocapture"])
        .env(READY_CRASH_PROJECT_ENV, project.path())
        .env(READY_BEFORE_COMMIT_ENV, &ready)
        .env(WORKER_STARTED_ENV, &worker_started)
        .spawn()
        .expect("spawn production READY-before-COMMIT parent");
    wait_for_file(&ready);
    let id = std::fs::read_to_string(&ready).expect("READY subagent identity");
    let record = read_persisted_record(project.path(), &id).expect("pending running record");
    let worktree = record.worktree_path.clone();
    let store = ProcessScopeStore::open(project.path()).expect("scope store");
    let gated = store.load(&id).expect("durable gated process scope");
    assert_eq!(gated.status, ProcessScopeStatus::Running);
    assert_eq!(gated.launch_committed, Some(false));
    assert!(!worker_started.exists(), "worker ran before COMMIT");

    parent.kill().expect("kill parent after READY");
    parent.wait().expect("reap killed parent");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !worker_started.exists(),
        "OS-gated worker executed after parent death before COMMIT"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    let listed = loop {
        match list_subagents(project.path()) {
            Ok(listed) => break listed,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("restart reconciliation did not converge: {error}"),
        }
    };
    assert!(
        listed.is_empty(),
        "pre-handoff workload became public: {listed:?}"
    );
    assert!(
        !worker_started.exists(),
        "worker executed during restart cleanup"
    );
    assert!(!worktree.exists(), "restart left the prepared worktree");
    assert!(
        !project
            .path()
            .join(".nib/profiles/default/sessions")
            .join(format!("{id}.json"))
            .exists(),
        "restart left the prepared audit session"
    );
    wait_for_absence(
        &project
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json")),
    );
    wait_for_absence(
        &project
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.cleanup.lease")),
    );
    assert!(
        !project
            .path()
            .join(".nib/subagent-owner-leases")
            .join(format!(
                "{}.lease",
                record.owner_lease.expect("owner lease")
            ))
            .exists(),
        "restart left owner authority"
    );
    assert!(
        !project
            .path()
            .join(".nib/subagents/.preparations")
            .join(format!("{id}.json"))
            .exists(),
        "restart retired external state before its preparation intent"
    );
}

#[test]
#[serial]
fn prepared_scope_restart_orders_late_supervisor_against_external_cleanup() {
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }

    for boundary in ["before-spawn", "before-self-registration"] {
        let project = git_project();
        let barrier = project.path().join(format!("{boundary}.barrier"));
        let release = project.path().join(format!("{boundary}.release"));
        let worker_started = project.path().join(format!("{boundary}.worker-started"));
        let mut command =
            Command::new(std::env::current_exe().expect("integration test executable"));
        command
            .args(["--exact", "ready_before_commit_crash_child", "--nocapture"])
            .env(READY_CRASH_PROJECT_ENV, project.path())
            .env(WORKER_STARTED_ENV, &worker_started);
        match boundary {
            "before-spawn" => {
                command.env(SCOPE_PREPARED_ENV, &barrier);
            }
            "before-self-registration" => {
                command
                    .env(SUPERVISOR_PRE_REGISTER_ENV, &barrier)
                    .env(SUPERVISOR_REGISTER_RELEASE_ENV, &release);
            }
            _ => unreachable!(),
        }
        let mut parent = command.spawn().expect("spawn production launch parent");
        wait_for_file(&barrier);
        let id = if boundary == "before-spawn" {
            std::fs::read_to_string(&barrier).expect("prepared scope subagent id")
        } else {
            only_subagent_id(project.path())
        };
        let late_supervisor = (boundary == "before-self-registration").then(|| {
            serde_json::from_slice::<ProcessIdentity>(
                &std::fs::read(&barrier).expect("pre-registration barrier identity"),
            )
            .expect("pre-registration supervisor identity")
        });
        let record = read_persisted_record(project.path(), &id).expect("pending record");
        let worktree = record.worktree_path.clone();
        let owner_lease = record.owner_lease.clone().expect("owner lease");
        let scope_path = project
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json"));
        let cleanup_lease_path = project
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.cleanup.lease"));
        let owner_path = project
            .path()
            .join(".nib/subagent-owner-leases")
            .join(format!("{owner_lease}.lease"));
        let audit_path = project
            .path()
            .join(".nib/profiles/default/sessions")
            .join(format!("{id}.json"));
        let intent_path = project
            .path()
            .join(".nib/subagents/.preparations")
            .join(format!("{id}.json"));
        let store = ProcessScopeStore::open(project.path()).expect("scope store");
        let prepared = store.load(&id).expect("prepared launch scope");
        assert_eq!(prepared.status, ProcessScopeStatus::Prepared);
        assert_eq!(prepared.launch_committed, Some(false));
        assert!(prepared.supervisor_registration_nonce.is_some());
        assert!(prepared.supervisor.is_none());
        assert!(prepared.direct_child.is_none());
        assert!(!cleanup_lease_path.exists());
        assert!(worktree.exists());
        assert!(owner_path.exists());
        assert!(audit_path.exists());
        assert!(intent_path.exists());
        assert!(!worker_started.exists());

        parent
            .kill()
            .expect("kill launch parent at prepared boundary");
        parent.wait().expect("reap launch parent");
        let listed = reconcile_until_empty(project.path());
        assert!(
            listed.is_empty(),
            "unregistered prepared launch became public: {listed:?}"
        );
        assert!(!scope_path.exists(), "prepared scope was not retired first");
        assert!(!cleanup_lease_path.exists());
        assert!(!worktree.exists(), "restart left the prepared worktree");
        assert!(!owner_path.exists(), "restart left owner authority");
        assert!(
            !audit_path.exists(),
            "restart left the prepared audit session"
        );
        assert!(!intent_path.exists(), "restart left preparation authority");
        assert!(
            !worker_started.exists(),
            "worker ran before self-registration"
        );

        if let Some(late_supervisor) = late_supervisor {
            std::fs::write(&release, b"release").expect("release late supervisor");
            wait_for_identity_exit(&late_supervisor);
            assert!(
                !scope_path.exists()
                    && !cleanup_lease_path.exists()
                    && !worktree.exists()
                    && !owner_path.exists()
                    && !audit_path.exists()
                    && !intent_path.exists(),
                "late supervisor recreated or retained launch resources"
            );
            assert!(
                !worker_started.exists(),
                "late supervisor reached worker execution after losing its exact CAS"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn post_handoff_failures_never_publish_without_cleanup_proof() {
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_nib"));
    let _executable = EnvironmentGuard::set("NIB_EXECUTABLE", executable.as_os_str());

    for failpoint in [
        "missing-stdout",
        "readiness-monitor-failure",
        "wait-monitor-failure",
    ] {
        let unstarted = git_project();
        let _failpoint = EnvironmentGuard::set(FAILPOINT_ENV, OsStr::new(failpoint));
        spawn_subagent(
            &json!({"prompt": "launch fencing fixture", "max_steps": 3}),
            unstarted.path(),
        )
        .expect_err("pre-delivery failpoint");
        let id = only_subagent_id(unstarted.path());
        let record = wait_for_record(unstarted.path(), &id, |record| record.status != "running");
        assert_eq!(record.status, "failed");
        let store = ProcessScopeStore::open(unstarted.path()).expect("scope store");
        assert!(store.try_load(&id).expect("scope lookup").is_none());
        assert!(!unstarted
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.cleanup.lease"))
            .exists());
    }

    let incomplete = git_project();
    let _failpoint = EnvironmentGuard::set(FAILPOINT_ENV, OsStr::new("after-first-byte"));
    let error = spawn_subagent(
        &json!({"prompt": "launch fencing fixture", "max_steps": 3}),
        incomplete.path(),
    )
    .expect_err("first-byte failpoint");
    assert!(error.contains("first request byte"), "{error}");
    let incomplete_id = only_subagent_id(incomplete.path());
    let record = wait_for_record(incomplete.path(), &incomplete_id, |record| {
        record.status == "failed" && launch_abort_verified(record)
    });
    assert_eq!(record.status, "failed");
    assert_ne!(
        record
            .result
            .as_ref()
            .and_then(|result| result.get("cleanup_verified")),
        Some(&json!(true))
    );
    let store = ProcessScopeStore::open(incomplete.path()).expect("scope store");
    wait_for_absence(
        &incomplete
            .path()
            .join(".nib/process-scopes")
            .join(format!("{incomplete_id}.json")),
    );
    assert!(store
        .try_load(&incomplete_id)
        .expect("scope lookup")
        .is_none());

    drop(_failpoint);
    let body_only = git_project();
    let _failpoint = EnvironmentGuard::set(FAILPOINT_ENV, OsStr::new("after-request-body"));
    let error = spawn_subagent(
        &json!({"prompt": "launch fencing fixture", "max_steps": 3}),
        body_only.path(),
    )
    .expect_err("request-body failpoint");
    assert!(error.contains("before newline"), "{error}");
    let body_id = only_subagent_id(body_only.path());
    let record = wait_for_record(body_only.path(), &body_id, |record| {
        record.status == "failed" && launch_abort_verified(record)
    });
    assert_eq!(record.status, "failed");
    assert_ne!(
        record
            .result
            .as_ref()
            .and_then(|result| result.get("cleanup_verified")),
        Some(&json!(true))
    );
    let store = ProcessScopeStore::open(body_only.path()).expect("scope store");
    wait_for_absence(
        &body_only
            .path()
            .join(".nib/process-scopes")
            .join(format!("{body_id}.json")),
    );
    assert!(store.try_load(&body_id).expect("scope lookup").is_none());

    drop(_failpoint);
    for (failpoint, expected_error) in [
        ("after-request-write", "after request write"),
        ("after-request-flush", "after request flush"),
        ("readiness-timeout", "readiness timeout"),
        ("abort-handle-failure", "abort-handle attachment failure"),
        ("commit-partial", "partial supervisor COMMIT"),
        ("commit-eof", "supervisor COMMIT EOF"),
        ("commit-timeout", "supervisor COMMIT timeout"),
        ("commit-identity-mismatch", "STARTED"),
    ] {
        let completed = git_project();
        let _failpoint = EnvironmentGuard::set(FAILPOINT_ENV, OsStr::new(failpoint));
        let error = spawn_subagent(
            &json!({"prompt": "launch fencing fixture", "max_steps": 3}),
            completed.path(),
        )
        .expect_err("post-delivery failpoint");
        assert!(error.contains(expected_error), "{error}");
        let completed_id = only_subagent_id(completed.path());
        let record = wait_for_record(completed.path(), &completed_id, |record| {
            record.status != "running" && launch_abort_verified(record)
        });
        assert!(matches!(record.status.as_str(), "cancelled" | "failed"));
        assert_eq!(
            record
                .result
                .as_ref()
                .and_then(|result| result.get("ownership_reconciliation"))
                .and_then(|proof| proof.get("workload_never_launched")),
            Some(&json!(true)),
            "pre-COMMIT failpoint must prove the gated worker never executed"
        );
        assert!(cleanup_proof(&record).is_none());
        wait_for_absence(
            &completed
                .path()
                .join(".nib/process-scopes")
                .join(format!("{completed_id}.json")),
        );
        wait_for_absence(
            &completed
                .path()
                .join(".nib/process-scopes")
                .join(format!("{completed_id}.cleanup.lease")),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn supervisor_rebinds_scope_deadlines_after_long_running_handoff() {
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_nib"));
    let _executable = EnvironmentGuard::set("NIB_EXECUTABLE", executable.as_os_str());
    let _delay = EnvironmentGuard::set(WORKER_DELAY_MS_ENV, OsStr::new("6500"));

    for cancellation in [false, true] {
        let project = git_project();
        let _worker_started =
            EnvironmentGuard::set(WORKER_STARTED_ENV, OsStr::new("worker-started"));
        let started = spawn_subagent(
            &json!({"prompt": "explore the project", "max_steps": 20}),
            project.path(),
        )
        .expect("production long-running spawn");
        let id = started["subagent_id"]
            .as_str()
            .expect("public subagent id")
            .to_string();
        let running_record = read_persisted_record(project.path(), &id).expect("running record");
        let worker_started = running_record.worktree_path.join("worker-started");
        let sentinel_deadline = Instant::now() + Duration::from_secs(10);
        while !worker_started.is_file() {
            if let Ok(record) = read_persisted_record(project.path(), &id) {
                assert_eq!(
                    record.status, "running",
                    "worker did not start before terminal state: {record:#?}"
                );
            }
            assert!(
                Instant::now() < sentinel_deadline,
                "timed out waiting for delayed worker sentinel"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::remove_file(&worker_started).expect("remove observed worker sentinel");
        let worker_started_at = Instant::now();
        if cancellation {
            std::thread::sleep(Duration::from_millis(5_500));
            assert_eq!(
                read_persisted_record(project.path(), &id)
                    .expect("running delayed record")
                    .status,
                "running",
                "delayed worker terminalized before cancellation"
            );
            cancel_subagent(project.path(), &id).expect("cancel after startup timeout");
        }
        let record = wait_for_record(project.path(), &id, |record| {
            record.status != "running" && cleanup_proof(record).is_some()
        });
        assert!(
            worker_started_at.elapsed() > Duration::from_secs(5),
            "worker did not outlive the startup scope-lock deadline"
        );
        assert_eq!(
            record.status,
            if cancellation {
                "cancelled"
            } else {
                "completed"
            },
            "long-running terminal record: {record:#?}"
        );
        let proof = cleanup_proof(&record).expect("exact cleanup proof");
        assert_eq!(proof["descendants_reaped"], true);
        wait_for_absence(
            &project
                .path()
                .join(".nib/process-scopes")
                .join(format!("{id}.json")),
        );
        wait_for_absence(
            &project
                .path()
                .join(".nib/process-scopes")
                .join(format!("{id}.cleanup.lease")),
        );
        wait_for_absence(
            &project
                .path()
                .join(".nib/subagent-owner-leases")
                .join(format!(
                    "{}.lease",
                    record
                        .owner_lease
                        .as_deref()
                        .expect("owner lease authority")
                )),
        );
        assert!(
            !project
                .path()
                .join(".nib/subagents/.preparations")
                .join(format!("{id}.json"))
                .exists(),
            "successful handoff retained preparation authority"
        );
        assert!(
            record.worktree_path.is_dir(),
            "terminal record lost its authoritative worktree"
        );
        assert!(
            project
                .path()
                .join(".nib/profiles/default/sessions")
                .join(format!("{id}.json"))
                .is_file(),
            "terminal record lost its authoritative parent audit session"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn pre_running_supervisor_loss_reconciles_as_verified_launch_abort() {
    if !nib::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_nib"));
    let _executable = EnvironmentGuard::set("NIB_EXECUTABLE", executable.as_os_str());
    let project = git_project();
    let pre_running_marker = project.path().join("delegation.pre-running.json");
    let _pause = EnvironmentGuard::set(PRE_RUNNING_PAUSE_ENV, pre_running_marker.as_os_str());
    let project_root = project.path().to_path_buf();
    let launch = tokio::spawn(async move {
        spawn_subagent(
            &json!({"prompt": "launch-abort reconciliation fixture", "max_steps": 3}),
            &project_root,
        )
    });

    tokio::time::sleep(Duration::from_millis(250)).await;
    if launch.is_finished() {
        panic!(
            "subagent launch ended before publishing its record: {:?}",
            launch.await.expect("early launch task")
        );
    }
    let id = only_subagent_id(project.path());
    wait_for_file(&pre_running_marker);
    let store = ProcessScopeStore::open(project.path()).expect("scope store");
    let prepared = store.load(&id).expect("gated prepared scope");
    assert_eq!(prepared.status, ProcessScopeStatus::Prepared);
    let supervisor = prepared
        .supervisor
        .clone()
        .expect("durable supervisor identity");
    let namespace_root = prepared
        .direct_child
        .clone()
        .expect("durable gated namespace root");
    assert!(supervisor.still_matches());
    assert!(namespace_root.still_matches());

    assert_eq!(
        unsafe { libc::kill(supervisor.pid as i32, libc::SIGKILL) },
        0
    );
    let launch_error = launch
        .await
        .expect("launch task")
        .expect_err("killed supervisor cannot acknowledge readiness");
    assert!(
        launch_error.contains("readiness")
            || launch_error.contains("supervisor")
            || launch_error.contains("control"),
        "{launch_error}"
    );

    let record = wait_for_record(project.path(), &id, |record| {
        record.status == "failed"
            && record
                .result
                .as_ref()
                .and_then(|result| result.get("ownership_reconciliation"))
                .and_then(|evidence| evidence.get("launch_abort_verified"))
                == Some(&json!(true))
    });
    let evidence = record
        .result
        .as_ref()
        .and_then(|result| result.get("ownership_reconciliation"))
        .expect("launch-abort reconciliation evidence");
    assert_eq!(evidence["cleanup_verified"], false);
    assert_eq!(evidence["launch_abort_verified"], true);
    assert_eq!(evidence["workload_never_launched"], true);
    assert!(evidence.get("cleanup_proof").is_none());
    assert_eq!(
        evidence["launch_abort_proof"]["namespace_root"],
        serde_json::to_value(&namespace_root).expect("namespace-root evidence")
    );

    wait_for_absence(
        &project
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.json")),
    );
    wait_for_absence(
        &project
            .path()
            .join(".nib/process-scopes")
            .join(format!("{id}.cleanup.lease")),
    );
    assert!(!namespace_root.still_matches());
}

fn launch_abort_verified(record: &SubagentRecord) -> bool {
    record.result.as_ref().is_some_and(|result| {
        result
            .get("launch_abort_verified")
            .and_then(|value| value.as_bool())
            == Some(true)
            || result
                .get("ownership_reconciliation")
                .and_then(|evidence| evidence.get("launch_abort_verified"))
                .and_then(|value| value.as_bool())
                == Some(true)
    })
}

fn cleanup_proof(record: &SubagentRecord) -> Option<&serde_json::Value> {
    let result = record.result.as_ref()?;
    result.get("cleanup_proof").or_else(|| {
        result
            .get("ownership_reconciliation")
            .and_then(|evidence| evidence.get("cleanup_proof"))
    })
}

fn git_project() -> TempDir {
    let root = tempfile::tempdir().expect("project root");
    run_git(root.path(), &["init", "--quiet"]);
    run_git(
        root.path(),
        &["config", "user.email", "nib-test@example.invalid"],
    );
    run_git(root.path(), &["config", "user.name", "nib test"]);
    std::fs::write(root.path().join(".gitignore"), ".nib/\n").expect("gitignore");
    std::fs::write(root.path().join("README.md"), "fixture\n").expect("readme");
    run_git(root.path(), &["add", ".gitignore", "README.md"]);
    run_git(root.path(), &["commit", "--quiet", "-m", "initial"]);
    std::fs::create_dir_all(root.path().join(".nib")).expect("state root");
    std::fs::write(
        root.path().join(".nib/config.toml"),
        "[llm]\nactive_provider = \"mock\"\n\n[llm.providers.mock]\nmodel = \"mock-model\"\n",
    )
    .expect("mock config");
    root
}

fn run_git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git command");
    assert!(status.success(), "git command failed: {args:?}");
}

fn only_subagent_id(root: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(entries) = std::fs::read_dir(root.join(".nib/subagents")) {
            let ids: Vec<_> = entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let path = entry.path();
                    (path.extension().and_then(|extension| extension.to_str()) == Some("json")
                        && !path
                            .file_name()
                            .is_some_and(|name| name.as_encoded_bytes().starts_with(b".")))
                    .then(|| {
                        path.file_stem()
                            .and_then(|name| name.to_str())
                            .map(str::to_string)
                    })
                    .flatten()
                })
                .collect();
            if ids.len() == 1 {
                return ids[0].clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for subagent record"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_record(
    root: &Path,
    id: &str,
    predicate: impl Fn(&SubagentRecord) -> bool,
) -> SubagentRecord {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let last_observation = match read_persisted_record(root, id) {
            Ok(record) => {
                if predicate(&record) {
                    return record;
                }
                serde_json::to_string(&record)
                    .unwrap_or_else(|error| format!("record serialization failed: {error}"))
            }
            Err(error) => format!("reconciliation error: {error}"),
        };
        assert!(
            Instant::now() < deadline,
            "timed out waiting for subagent state: {last_observation}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_persisted_record(root: &Path, id: &str) -> Result<SubagentRecord, String> {
    let path = root.join(".nib/subagents").join(format!("{id}.json"));
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn wait_for_absence(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !path.exists(),
        "timed out waiting for {} retirement",
        path.display()
    );
}

fn reconcile_until_empty(root: &Path) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match list_subagents(root) {
            Ok(listed) => return listed,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("restart reconciliation did not converge: {error}"),
        }
    }
}

fn wait_for_identity_exit(identity: &ProcessIdentity) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while identity.still_matches() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !identity.still_matches(),
        "timed out waiting for late supervisor to exit"
    );
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.is_file(), "timed out waiting for {}", path.display());
}
