use super::*;
use crate::session::ToolCallRecord;
#[cfg(unix)]
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use tempfile::tempdir;

pub(crate) struct EnvironmentGuard {
    pub(crate) name: &'static str,
    pub(crate) previous: Option<std::ffi::OsString>,
}

impl EnvironmentGuard {
    pub(crate) fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

pub(crate) const TASK_LOCK_CHILD_DAEMON_DIR: &str = "NIB_TEST_TASK_LOCK_DAEMON_DIR";
pub(crate) const TASK_LOCK_CHILD_KIND: &str = "NIB_TEST_TASK_LOCK_KIND";
pub(crate) const TASK_LOCK_CHILD_ID: &str = "NIB_TEST_TASK_LOCK_ID";
pub(crate) const TASK_LOCK_CHILD_EXPECTATION: &str = "NIB_TEST_TASK_LOCK_EXPECTATION";
pub(crate) const RECONCILE_CHILD_DAEMON_DIR: &str = "NIB_TEST_RECONCILE_DAEMON_DIR";
pub(crate) const RECONCILE_CHILD_TASK_ID: &str = "NIB_TEST_RECONCILE_TASK_ID";
pub(crate) const RECONCILE_CHILD_POINT: &str = "NIB_TEST_RECONCILE_POINT";
pub(crate) const RECONCILE_CHILD_READY: &str = "NIB_TEST_RECONCILE_READY";
pub(crate) const PUBLICATION_CHILD_DAEMON_DIR: &str = "NIB_TEST_PUBLICATION_DAEMON_DIR";
pub(crate) const PUBLICATION_CHILD_SESSIONS_DIR: &str = "NIB_TEST_PUBLICATION_SESSIONS_DIR";
pub(crate) const PUBLICATION_CHILD_TASK_ID: &str = "NIB_TEST_PUBLICATION_TASK_ID";
pub(crate) const PUBLICATION_CHILD_KIND: &str = "NIB_TEST_PUBLICATION_KIND";
pub(crate) const PUBLICATION_CHILD_OWNER_TOKEN: &str = "NIB_TEST_PUBLICATION_OWNER_TOKEN";
pub(crate) const PUBLICATION_CHILD_OWNER_PID: &str = "NIB_TEST_PUBLICATION_OWNER_PID";
#[cfg(unix)]
pub(crate) const TASK_COMMIT_CHILD_DAEMON_DIR: &str = "NIB_TASK_COMMIT_CHILD_DAEMON_DIR";
#[cfg(unix)]
pub(crate) const TASK_COMMIT_CHILD_ID: &str = "NIB_TASK_COMMIT_CHILD_ID";
#[cfg(unix)]
pub(crate) const TASK_COMMIT_CHILD_MODE: &str = "NIB_TASK_COMMIT_CHILD_MODE";
#[cfg(unix)]
pub(crate) const TASK_COMMIT_CHILD_READY: &str = "NIB_TASK_COMMIT_CHILD_READY";
#[cfg(unix)]
pub(crate) const TASK_COMMIT_CHILD_RELEASE: &str = "NIB_TASK_COMMIT_CHILD_RELEASE";

pub(crate) fn fixture() -> (tempfile::TempDir, DurableTaskStore, SessionStore) {
    let directory = tempdir().expect("tempdir");
    let daemon = directory.path().join("state/daemons");
    let sessions = directory.path().join("state/sessions");
    std::fs::create_dir_all(&sessions).expect("sessions");
    let session_store = SessionStore::at_dir(sessions);
    session_store.create_session_with_id("origin");
    let store = DurableTaskStore::at_daemon_dir(daemon).expect("store");
    (directory, store, session_store)
}

pub(crate) fn prepare_schedule_fixture(
    directory: &tempfile::TempDir,
    store: &DurableTaskStore,
    session_store: &SessionStore,
    id: &str,
) {
    store
        .prepare_schedule(DurableScheduleRequest {
            id: id.to_string(),
            prompt: "scheduled plan".to_string(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            initial_delay: Duration::from_secs(1),
            interval: Duration::from_secs(1),
            repeat_count: 1,
        })
        .expect("prepare schedule");
}

pub(crate) fn scheduled_agent_fixture() -> (tempfile::TempDir, DurableTaskStore, SessionStore) {
    let directory = tempdir().expect("tempdir");
    std::fs::write(directory.path().join(".gitignore"), ".nib/\n").expect("gitignore");
    std::fs::write(
        directory.path().join("README.md"),
        "scheduled agent fixture\n",
    )
    .expect("readme");
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "nib-tests@example.invalid"],
        vec!["config", "user.name", "nib tests"],
        vec!["add", ".gitignore", "README.md"],
        vec!["commit", "--quiet", "-m", "initial"],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(directory.path())
            .status()
            .expect("git command");
        assert!(status.success());
    }

    let mut config = crate::config::NibConfig::default();
    config.llm.active_provider = Some("mock".to_string());
    config.llm.providers.insert(
        "mock".to_string(),
        crate::config::ProviderEntry {
            model: "mock-model".to_string(),
            ..crate::config::ProviderEntry::default()
        },
    );
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    crate::config::save_nib_config_full(directory.path(), &mut config).expect("runtime config");

    let session_store = SessionStore::for_project(directory.path()).expect("session store");
    session_store.create_session_with_id("origin");
    let store = DurableTaskStore::for_project(directory.path()).expect("durable task store");
    (directory, store, session_store)
}

pub(crate) fn serve_failed_responses_once(secret: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("Responses fixture listener");
    let address = listener.local_addr().expect("Responses fixture address");
    std::thread::spawn(move || {
        for _ in 0..16 {
            let (mut stream, _) = listener.accept().expect("Responses fixture connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("Responses fixture timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).expect("Responses fixture read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let content_length = String::from_utf8_lossy(&request[..header_end])
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            if !request.starts_with(b"POST /v1/responses HTTP/1.1\r\n") {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            let body = format!(
                "data: {}\n\n",
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "scheduled-response-failed",
                        "status": "failed",
                        "error": {
                            "code": "scheduled_fixture_failure",
                            "message": format!("provider rejected {secret}")
                        },
                        "output": []
                    }
                })
            );
            let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
            stream
                .write_all(response.as_bytes())
                .expect("Responses fixture response");
            return;
        }
        panic!("Responses fixture did not receive its expected request");
    });
    format!("http://{address}/v1")
}

pub(crate) fn prepare_owned_due_schedule(
    directory: &tempfile::TempDir,
    store: &DurableTaskStore,
    session_store: &SessionStore,
    task_id: &str,
) -> (WorkerOwner, ScheduleWorkerJob, DateTime<Utc>) {
    prepare_schedule_fixture(directory, store, session_store, task_id);
    let owner = WorkerOwner {
        token: format!("{task_id}-owner"),
        pid: std::process::id(),
    };
    let stale_heartbeat = Utc::now() - ChronoDuration::seconds(10);
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.record.updated_at = stale_heartbeat;
            task.record.next_run_at = Some(Utc::now() - ChronoDuration::seconds(1));
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed due schedule worker");
    let job = ScheduleWorkerJob {
        prompt: "scheduled plan".to_string(),
        project_root: directory.path().to_path_buf(),
        profile_id: "default".to_string(),
        sessions_dir: session_store.sessions_dir().to_path_buf(),
        session_id: "origin".to_string(),
        interval_secs: 1,
        repeat_count: 1,
    };
    (owner, job, stale_heartbeat)
}

pub(crate) async fn wait_for_worker_heartbeat(
    store: &DurableTaskStore,
    task_id: &str,
    prior_heartbeat: DateTime<Utc>,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            let record = store
                .get(task_id)
                .expect("read schedule worker")
                .expect("schedule worker remains present");
            if record.updated_at > prior_heartbeat {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("schedule worker heartbeat");
}

pub(crate) fn terminal_request(
    directory: &tempfile::TempDir,
    session_store: &SessionStore,
    id: &str,
) -> DurableTerminalRequest {
    DurableTerminalRequest {
        id: id.to_string(),
        command: "printf ok".to_string(),
        cwd: directory.path().to_path_buf(),
        project_root: directory.path().to_path_buf(),
        profile_id: "default".to_string(),
        sessions_dir: session_store.sessions_dir().to_path_buf(),
        session_id: "origin".to_string(),
        execution: ExecutionConfig::default(),
        timeout_secs: 10,
        max_output_bytes: 1024,
    }
}

#[cfg(unix)]
pub(crate) fn run_task_commit_child(daemon_dir: &Path) {
    let id = std::env::var(TASK_COMMIT_CHILD_ID).expect("task commit child id must be configured");
    let mode =
        std::env::var(TASK_COMMIT_CHILD_MODE).expect("task commit child mode must be configured");
    let ready = PathBuf::from(
        std::env::var_os(TASK_COMMIT_CHILD_READY)
            .expect("task commit child ready path must be configured"),
    );
    let release = std::env::var_os(TASK_COMMIT_CHILD_RELEASE).map(PathBuf::from);
    let store = DurableTaskStore::at_daemon_dir(daemon_dir).expect("task commit child store");
    let _lock = store
        .acquire_task_lock(&id)
        .expect("task commit child lock");
    let path = store.task_path(&id);
    let mut opened = store
        .read_path_opened(&path)
        .expect("task commit child record");
    opened.task.record.error = Some("child must not publish".to_string());
    opened.task.record.updated_at = Utc::now();
    let result = store.write_path_after_effects_expected_with_commit_check(
        &path,
        &opened.task,
        crate::daemons::state::FileExpectation::Present(&opened.file),
        || {
            fs::write(&ready, b"ready").map_err(|error| error.to_string())?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                if release.as_ref().is_some_and(|path| path.exists()) {
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    return Err("task commit child timed out".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        },
    );

    match mode.as_str() {
        "replace" => {
            let error = result.expect_err("commit-barrier replacement must fail closed");
            assert!(error.contains("identity changed"), "{error}");
        }
        "kill" => panic!("task crash child unexpectedly left its commit barrier"),
        value => panic!("unsupported task commit child mode: {value}"),
    }
}

#[cfg(unix)]
pub(crate) fn spawn_task_commit_child(
    daemon_dir: &Path,
    id: &str,
    mode: &str,
    ready: &Path,
    release: Option<&Path>,
) -> std::process::Child {
    let _ = fs::remove_file(ready);
    if let Some(release) = release {
        let _ = fs::remove_file(release);
    }
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("current workload test binary"));
    command
        .args([
            "--exact",
            "daemons::workload::tests::real_child_task_commit_barrier_and_fsync_crash_recovery",
            "--nocapture",
        ])
        .env(TASK_COMMIT_CHILD_DAEMON_DIR, daemon_dir)
        .env(TASK_COMMIT_CHILD_ID, id)
        .env(TASK_COMMIT_CHILD_MODE, mode)
        .env(TASK_COMMIT_CHILD_READY, ready);
    if let Some(release) = release {
        command.env(TASK_COMMIT_CHILD_RELEASE, release);
    }
    command.spawn().expect("spawn task commit child")
}

#[cfg(unix)]
pub(crate) fn wait_for_task_commit_child(child: &mut std::process::Child, ready: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect task commit child") {
            panic!("task commit child exited before readiness: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "task commit child did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(unix)]
pub(crate) fn task_temporary_paths(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .expect("list task directory")
        .map(|entry| entry.expect("task directory entry").path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".task-") && name.ends_with(".tmp")
            })
        })
        .collect()
}

pub(crate) fn prepare_reconcilable_terminal(
    directory: &tempfile::TempDir,
    store: &DurableTaskStore,
    session_store: &SessionStore,
    id: &str,
) -> WorkerOwner {
    let prepared = store
        .prepare_terminal(terminal_request(directory, session_store, id))
        .expect("prepare reconcilable terminal");
    session_store
        .record_tool_call(ToolCallRecord {
            id: Some(format!("call-{id}")),
            session_id: Some("origin".to_string()),
            tool_name: Some("terminal".to_string()),
            result: Some(json!({
                "success": true,
                "output": {
                    "task_id": id,
                    "execution_id": prepared.execution_id,
                    "status": "started",
                },
            })),
            ..ToolCallRecord::default()
        })
        .expect("record originating background tool call");
    let owner = WorkerOwner {
        token: format!("lease-{id}"),
        pid: 42_100,
    };
    store
        .update(id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed stale worker");
    owner
}

#[cfg(unix)]
#[test]
pub(crate) fn real_child_task_commit_barrier_and_fsync_crash_recovery() {
    if let Some(daemon_dir) = std::env::var_os(TASK_COMMIT_CHILD_DAEMON_DIR) {
        run_task_commit_child(Path::new(&daemon_dir));
        return;
    }

    let (replacement_root, replacement_store, replacement_sessions) = fixture();
    let replacement_id = "child-task-commit-substitution";
    prepare_reconcilable_terminal(
        &replacement_root,
        &replacement_store,
        &replacement_sessions,
        replacement_id,
    );
    let replacement_path = replacement_store.task_path(replacement_id);
    let displaced = replacement_store
        .tasks_dir
        .join("child-task-commit-substitution.displaced");
    let ready = replacement_root.path().join("task-replacement.ready");
    let release = replacement_root.path().join("task-replacement.release");
    let mut child = spawn_task_commit_child(
        &replacement_store.daemon_dir,
        replacement_id,
        "replace",
        &ready,
        Some(&release),
    );
    wait_for_task_commit_child(&mut child, &ready);

    let mut replacement: DurableTaskFile =
        serde_json::from_slice(&fs::read(&replacement_path).expect("replacement task base"))
            .expect("decode replacement task base");
    replacement.record.error = Some("authoritative replacement".to_string());
    replacement.record.updated_at = Utc::now();
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize task");
    fs::rename(&replacement_path, &displaced).expect("displace expected task");
    fs::write(&replacement_path, &replacement_bytes).expect("install replacement task");
    fs::write(&release, b"release").expect("release task child");
    let status = child.wait().expect("wait for replacement child");
    assert!(status.success(), "replacement child failed: {status}");
    assert_eq!(
        fs::read(&replacement_path).expect("replacement task bytes"),
        replacement_bytes
    );
    assert!(displaced.exists(), "displaced expected task was lost");

    let (crash_root, crash_store, crash_sessions) = fixture();
    let crash_id = "child-task-fsync-crash";
    prepare_reconcilable_terminal(&crash_root, &crash_store, &crash_sessions, crash_id);
    let crash_path = crash_store.task_path(crash_id);
    let crash_before = fs::read(&crash_path).expect("task before crash");
    let crash_ready = crash_root.path().join("task-crash.ready");
    let mut crash_child = spawn_task_commit_child(
        &crash_store.daemon_dir,
        crash_id,
        "kill",
        &crash_ready,
        None,
    );
    wait_for_task_commit_child(&mut crash_child, &crash_ready);
    let temporary = task_temporary_paths(&crash_store.tasks_dir);
    assert_eq!(temporary.len(), 1, "expected one fsynced task temp");
    crash_child.kill().expect("kill task writer");
    crash_child.wait().expect("reap task writer");
    assert!(
        temporary[0].exists(),
        "killed writer temp disappeared early"
    );

    let daemon_dir = crash_store.daemon_dir.clone();
    let tasks_dir = crash_store.tasks_dir.clone();
    drop(crash_store);
    let recovered = DurableTaskStore::at_daemon_dir(&daemon_dir).expect("recover task store");
    assert_eq!(
        fs::read(&crash_path).expect("task after recovery"),
        crash_before
    );
    assert!(
        task_temporary_paths(&tasks_dir).is_empty(),
        "task recovery left the killed writer temp"
    );
    assert_eq!(
        recovered
            .get(crash_id)
            .expect("load recovered task")
            .expect("recovered task exists")
            .error,
        None
    );
}

pub(crate) fn background_delivery_counts(
    store: &DurableTaskStore,
    session_store: &SessionStore,
    task_id: &str,
) -> (usize, usize, usize) {
    let session = session_store.load("origin").expect("origin session");
    let events = session
        .events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "background_task_completed" | "background_task_failed"
            ) && event.details.get("task_id").and_then(Value::as_str) == Some(task_id)
        })
        .count();
    let messages = session
        .messages
        .iter()
        .filter(|message| {
            serde_json::from_str::<Value>(&message.content).is_ok_and(|content| {
                content.get("type").and_then(Value::as_str) == Some("background_task_result")
                    && content.get("task_id").and_then(Value::as_str) == Some(task_id)
            })
        })
        .count();
    let key = format!("task_id={task_id}");
    let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
        .read_all()
        .expect("daemon audit")
        .iter()
        .filter(|record| {
            record.daemon == "background_task"
                && record.action == "deliver_observation"
                && record
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.split(';').any(|field| field.trim() == key))
                && matches!(record.outcome.as_str(), "completed" | "failed")
        })
        .count();
    (events, messages, audits)
}

pub(crate) fn background_delivery_counts_for_execution(
    store: &DurableTaskStore,
    session_store: &SessionStore,
    task_id: &str,
    execution_id: &str,
) -> (usize, usize, usize) {
    let session = session_store.load("origin").expect("origin session");
    let events = session
        .events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "background_task_completed" | "background_task_failed"
            ) && event.details.get("task_id").and_then(Value::as_str) == Some(task_id)
                && event.details.get("execution_id").and_then(Value::as_str) == Some(execution_id)
        })
        .count();
    let messages = session
        .messages
        .iter()
        .filter(|message| {
            serde_json::from_str::<Value>(&message.content).is_ok_and(|content| {
                content.get("type").and_then(Value::as_str) == Some("background_task_result")
                    && content.get("task_id").and_then(Value::as_str) == Some(task_id)
                    && content.get("execution_id").and_then(Value::as_str) == Some(execution_id)
            })
        })
        .count();
    let detail_key = format!("task_id={task_id}; execution_id={execution_id}");
    let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
        .read_all()
        .expect("daemon audit")
        .iter()
        .filter(|record| {
            record.daemon == "background_task"
                && record.action == "deliver_observation"
                && record.detail.as_deref().is_some_and(|detail| {
                    detail_key.split(';').all(|key_field| {
                        detail
                            .split(';')
                            .any(|detail_field| detail_field.trim() == key_field.trim())
                    })
                })
                && matches!(record.outcome.as_str(), "completed" | "failed")
        })
        .count();
    (events, messages, audits)
}

pub(crate) fn terminate_reconciler_at(
    store: &DurableTaskStore,
    task_id: &str,
    point: ReconcileHookPoint,
) {
    let ready = store
        .daemon_dir()
        .join(format!(".{task_id}.reconciler-ready"));
    let point = match point {
        ReconcileHookPoint::BeforeDelivery => "before_delivery",
        ReconcileHookPoint::AfterDelivery => "after_delivery",
    };
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "daemons::workload::tests::test_part_1::reconciliation_process_loss_child",
            "--nocapture",
        ])
        .env(RECONCILE_CHILD_DAEMON_DIR, store.daemon_dir())
        .env(RECONCILE_CHILD_TASK_ID, task_id)
        .env(RECONCILE_CHILD_POINT, point)
        .env(RECONCILE_CHILD_READY, &ready)
        .spawn()
        .expect("spawn reconciler child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect reconciler child") {
            panic!("reconciler child exited before pause: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "reconciler child did not reach {point}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("terminate paused reconciler child");
    child.wait().expect("reap terminated reconciler child");
    std::fs::remove_file(ready).expect("remove reconciler readiness marker");
}

pub(crate) fn terminate_worker_publication_at(
    store: &DurableTaskStore,
    session_store: &SessionStore,
    task_id: &str,
    kind: &str,
    owner: &WorkerOwner,
    point: WorkerPublicationHookPoint,
) {
    let point_name = match point {
        WorkerPublicationHookPoint::BeforeEffects => "before",
        WorkerPublicationHookPoint::AfterEffects => "after",
    };
    let ready = store
        .daemon_dir()
        .join(format!(".{task_id}-{kind}-{point_name}.publication-ready"));
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "daemons::workload::tests::test_part_1::worker_publication_process_loss_child",
            "--nocapture",
        ])
        .env(PUBLICATION_CHILD_DAEMON_DIR, store.daemon_dir())
        .env(PUBLICATION_CHILD_SESSIONS_DIR, session_store.sessions_dir())
        .env(PUBLICATION_CHILD_TASK_ID, task_id)
        .env(PUBLICATION_CHILD_KIND, kind)
        .env(PUBLICATION_CHILD_OWNER_TOKEN, &owner.token)
        .env(PUBLICATION_CHILD_OWNER_PID, owner.pid.to_string())
        .env("NIB_TEST_WORKER_PUBLICATION_TASK_ID", task_id)
        .env("NIB_TEST_WORKER_PUBLICATION_POINT", point_name)
        .env("NIB_TEST_WORKER_PUBLICATION_READY", &ready)
        .spawn()
        .expect("spawn worker publication child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect publication child") {
            panic!("worker publication child exited before pause: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "worker publication child did not reach {point_name} effects"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("terminate paused publication child");
    child.wait().expect("reap terminated publication child");
    std::fs::remove_file(ready).expect("remove publication readiness marker");
}

#[path = "test_part_0.rs"]
mod test_part_0;
#[path = "test_part_1.rs"]
mod test_part_1;
#[path = "test_part_2.rs"]
mod test_part_2;
