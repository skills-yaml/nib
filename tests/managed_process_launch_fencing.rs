#![cfg(all(target_os = "linux", debug_assertions))]

use nib::sandbox::process::{ProcessScopeStatus, ProcessScopeStore};
use nib::tools::delegation::{get_subagent_record, spawn_subagent, SubagentRecord};
use serde_json::json;
use serial_test::serial;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const FAILPOINT_ENV: &str = "NIB_TEST_SUBAGENT_LAUNCH_FAILPOINT";
const PRE_RUNNING_PAUSE_ENV: &str = "NIB_TEST_PROCESS_SCOPE_PRE_RUNNING_PAUSE";

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
            record.status != "running" && cleanup_verified(record)
        });
        assert!(matches!(record.status.as_str(), "cancelled" | "failed"));
        assert_eq!(
            cleanup_proof(&record).and_then(|proof| proof.get("descendants_reaped")),
            Some(&json!(true))
        );
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

fn cleanup_verified(record: &SubagentRecord) -> bool {
    record.result.as_ref().is_some_and(|result| {
        result
            .get("cleanup_verified")
            .and_then(|value| value.as_bool())
            == Some(true)
            || result
                .get("ownership_reconciliation")
                .and_then(|evidence| evidence.get("cleanup_verified"))
                .and_then(|value| value.as_bool())
                == Some(true)
    })
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
                    (path.extension().and_then(|extension| extension.to_str()) == Some("json"))
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
        let last_observation = match get_subagent_record(root, id) {
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

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.is_file(), "timed out waiting for {}", path.display());
}
