use nib::config::{
    load_nib_config_full, save_nib_config_full, NibConfig, ProfileConfig, ProviderEntry,
};
use nib::session::SessionStore;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::{Duration, Instant};
use tempfile::{tempdir, TempDir};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> TempDir {
    let root = tempdir().expect("tempdir");
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "durable@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "durable test"]);
    std::fs::write(root.path().join(".gitignore"), ".nib/\n").expect("gitignore");
    std::fs::write(root.path().join("README.md"), "durable fixture\n").expect("readme");
    git(root.path(), &["add", ".gitignore", "README.md"]);
    git(root.path(), &["commit", "-qm", "fixture"]);

    let mut config = NibConfig::default();
    config.llm.active_provider = Some("mock".to_string());
    config.llm.providers = HashMap::from([(
        "mock".to_string(),
        ProviderEntry {
            model: "mock-model".to_string(),
            api_key: None,
            api_keys: Vec::new(),
            base_url: None,
        },
    )]);
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    save_nib_config_full(root.path(), &mut config).expect("runtime config");
    root
}

fn nib(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nib"))
        .current_dir(root)
        .args(args)
        .output()
        .expect("nib starts")
}

fn successful_nib(root: &Path, args: &[&str]) -> Output {
    let output = nib(root, args);
    assert!(
        output.status.success(),
        "nib {} failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn task_json(root: &Path, args: &[&str]) -> Value {
    let output = successful_nib(root, args);
    serde_json::from_slice(&output.stdout).expect("task command returns JSON")
}

fn only_task(root: &Path) -> Value {
    let listed = task_json(root, &["task", "list"]);
    assert_eq!(listed["tasks"].as_array().unwrap().len(), 1);
    listed["tasks"][0].clone()
}

#[cfg(unix)]
fn process_group_id(pid: u32) -> u32 {
    let output = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .expect("inspect worker process group");
    assert!(
        output.status.success(),
        "failed to inspect worker {pid}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("numeric worker process group")
}

fn wait_for_terminal_status(root: &Path, task_id: &str, expected: &str) -> Value {
    let started = Instant::now();
    loop {
        let task = task_json(root, &["task", "get", task_id])["task"].clone();
        if task["status"] == expected {
            return task;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "task {task_id} did not reach {expected}: {task}"
        );
        sleep(Duration::from_millis(100));
    }
}

#[test]
fn detached_terminal_survives_parent_and_is_visible_to_later_processes() {
    let root = fixture();
    let parent = successful_nib(
        root.path(),
        &[
            "run",
            "durable background terminal",
            "--yes",
            "--max-steps",
            "8",
        ],
    );
    assert!(String::from_utf8_lossy(&parent.stdout).contains("Agent run completed"));

    let task = only_task(root.path());
    let task_id = task["id"].as_str().unwrap();
    assert_eq!(task["kind"], "terminal");
    assert!(matches!(
        task["status"].as_str(),
        Some("running" | "completed")
    ));

    let completed = wait_for_terminal_status(root.path(), task_id, "completed");
    assert_eq!(completed["result"]["stdout"], "durable worker complete\n");
    assert_eq!(completed["result"]["exit_code"], 0);

    let store = SessionStore::for_project(root.path()).expect("profile sessions");
    let session = store
        .load(&store.get_latest_id().expect("session id"))
        .expect("session");
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "background_task_completed"));
    assert!(session.tool_calls.iter().any(|record| {
        record
            .result
            .as_ref()
            .and_then(|result| result.get("output"))
            .and_then(|output| output.get("task_id"))
            .and_then(Value::as_str)
            == Some(task_id)
            && record.error.is_none()
    }));
}

#[test]
fn a_later_process_can_cancel_and_reconcile_a_detached_terminal() {
    let root = fixture();
    successful_nib(
        root.path(),
        &[
            "run",
            "durable cancellable background terminal",
            "--yes",
            "--max-steps",
            "8",
        ],
    );
    let task = only_task(root.path());
    let task_id = task["id"].as_str().unwrap();

    #[cfg(unix)]
    {
        let worker_pid = task["worker_pid"].as_u64().expect("running worker pid") as u32;
        assert_eq!(process_group_id(worker_pid), worker_pid);
    }

    let cancelled = task_json(root.path(), &["task", "cancel", task_id]);
    assert!(matches!(
        cancelled["task"]["status"].as_str(),
        Some("cancelled" | "cancelling")
    ));
    let cancelled = wait_for_terminal_status(root.path(), task_id, "cancelled");
    assert!(cancelled["error"]
        .as_str()
        .unwrap()
        .contains("cancelled by user"));

    let reconciled = task_json(root.path(), &["task", "reconcile"]);
    assert!(reconciled["tasks"].as_array().unwrap().is_empty());
}

#[test]
fn persisted_schedule_wakes_the_real_agent_loop_after_parent_exit() {
    let root = fixture();
    successful_nib(
        root.path(),
        &["run", "durable scheduled wake", "--yes", "--max-steps", "8"],
    );
    let task = only_task(root.path());
    let task_id = task["id"].as_str().unwrap();
    assert_eq!(task["kind"], "schedule");

    let completed = wait_for_terminal_status(root.path(), task_id, "completed");
    assert_eq!(completed["completed_occurrences"], 1);
    assert_eq!(completed["result"]["execution_mode"], "plan");
    assert_eq!(completed["result"]["runs"][0]["outcome"], "plan_ready");

    let store = SessionStore::for_project(root.path()).expect("profile sessions");
    let session = store
        .load(&store.get_latest_id().expect("session id"))
        .expect("session");
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "timer_fired"));
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "scheduled_agent_run_completed"));
    assert!(session
        .messages
        .iter()
        .any(|message| message.role == "user" && message.content == "scheduled wake plan"));
}

#[test]
fn detached_terminal_redacts_profile_and_config_secrets_before_persistence() {
    let root = fixture();
    let env_path = root.path().join(".nib/durable.env");
    let secret = "durable-secret-value-42";
    let config_secret = "provider-config-secret-value-84";
    std::fs::write(&env_path, format!("NIB_DURABLE_TOKEN={secret}\n")).expect("profile env");
    let mut config = load_nib_config_full(root.path()).expect("load config");
    config
        .llm
        .providers
        .get_mut("mock")
        .expect("mock provider")
        .api_key = Some(config_secret.to_string());
    config.profiles.active = vec![ProfileConfig {
        id: "default".to_string(),
        root: PathBuf::from("."),
        env_file: Some(PathBuf::from(".nib/durable.env")),
        ..ProfileConfig::default()
    }];
    save_nib_config_full(root.path(), &mut config).expect("save profile config");

    successful_nib(
        root.path(),
        &[
            "run",
            "durable background secret terminal",
            "--yes",
            "--max-steps",
            "8",
        ],
    );
    let task = only_task(root.path());
    let task_id = task["id"].as_str().unwrap();
    let completed = wait_for_terminal_status(root.path(), task_id, "completed");
    let stdout = completed["result"]["stdout"].as_str().expect("stdout");
    assert!(stdout.starts_with("[REDACTED]\n"));
    assert!(stdout.contains("[REDACTED]"));
    assert!(!stdout.contains(secret));
    assert!(!stdout.contains(config_secret));

    let store = SessionStore::for_project(root.path()).expect("profile sessions");
    let session_path = store.sessions_dir().join(format!(
        "{}.json",
        store.get_latest_id().expect("session id")
    ));
    let raw_session = std::fs::read_to_string(session_path).expect("session JSON");
    assert!(!raw_session.contains(secret));
    assert!(!raw_session.contains(config_secret));
    let raw_task = std::fs::read_to_string(root.path().join(format!(
        ".nib/profiles/default/daemons/tasks/{task_id}.json"
    )))
    .expect("task JSON");
    assert!(!raw_task.contains(secret));
    assert!(!raw_task.contains(config_secret));
    assert!(raw_task.contains("[removed after task completion]"));
}
