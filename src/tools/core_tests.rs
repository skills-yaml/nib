use super::*;
use std::net::SocketAddr;

struct EnvironmentGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvironmentGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

struct StaticResolver {
    addresses: HashMap<String, Vec<SocketAddr>>,
}

#[async_trait::async_trait]
impl HostResolver for StaticResolver {
    async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<SocketAddr>, String> {
        self.addresses
            .get(host)
            .cloned()
            .ok_or_else(|| format!("missing fixture resolution for {host}"))
    }
}

#[test]
fn glob_candidates_use_slash_separators() {
    let candidate = Path::new("src").join("nested").join("lib.rs");
    let normalized = path_for_glob(&candidate);

    assert_eq!(normalized, "src/nested/lib.rs");
    assert!(glob_matches("**/*.rs", &normalized));
}

#[tokio::test]
#[serial_test::serial]
async fn managed_foreground_scope_rejects_independently_owned_background_work() {
    let _environment = EnvironmentGuard::set("NIB_MANAGED_PROCESS_SCOPE", "sub-fixture");
    let root = tempfile::tempdir().expect("project root");
    let terminal_error = run_terminal(
        crate::tools::ToolInvocationId::new(),
        &json!({"command": "pwd", "background": true}),
        root.path(),
        &ExecutionConfig::default(),
        "local",
        10,
        &HashMap::new(),
        None,
    )
    .await
    .expect_err("background terminal must be rejected");
    assert!(terminal_error.contains("foreground managed-process scope"));

    let schedule_error = schedule(
        &json!({"prompt": "later", "duration_secs": 10}),
        root.path(),
    )
    .await
    .expect_err("schedule must be rejected");
    assert!(schedule_error.contains("foreground managed-process scope"));

    let subagent_error =
        crate::tools::delegation::spawn_subagent(&json!({"prompt": "nested"}), root.path())
            .expect_err("nested subagent must be rejected");
    assert!(subagent_error.contains("foreground managed-process scope"));
}

#[tokio::test]
async fn manage_subagents_projects_running_internal_ownership_from_list_and_get() {
    let root = tempfile::tempdir().expect("project root");
    let id = format!("sub-public-core-{}", uuid::Uuid::new_v4());
    let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
        .expect("live owner lease");
    crate::tools::delegation::write_subagent_record(
        root.path(),
        &crate::tools::delegation::SubagentRecord {
            id: id.clone(),
            parent_session_id: Some("parent".to_string()),
            child_session_id: format!("child-{id}"),
            prompt: "public projection fixture".to_string(),
            status: "running".to_string(),
            execution_generation: Some(owner_lease.execution_generation()),
            owner_lease: Some(owner_lease.lease_id().to_string()),
            worktree_path: root.path().join("worktree"),
            branch: format!("nib/subagent/{id}"),
            branch_oid: None,
            result: Some(json!({
                "_ownership_audit_target": {
                    "sessions_dir": root.path().join("private-core-audit-sessions"),
                    "directory_identity": "private-core-identity",
                }
            })),
            error: None,
            verification: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    )
    .expect("running subagent record");

    let listed = manage_subagents(&json!({"action": "list"}), root.path())
        .await
        .expect("core list");
    let listed_record = &listed["subagents"][0];
    let fetched = manage_subagents(&json!({"action": "get", "subagent_id": id}), root.path())
        .await
        .expect("core get");
    for public in [listed_record, &fetched["subagent"]] {
        assert_eq!(public["status"], "running");
        assert!(public["result"].is_null());
        assert!(public.get("execution_generation").is_none());
        assert!(public.get("owner_lease").is_none());
        let encoded = serde_json::to_string(public).expect("public core record");
        assert!(!encoded.contains("_ownership_audit_target"));
        assert!(!encoded.contains("private-core-audit-sessions"));
        assert!(!encoded.contains("private-core-identity"));
    }
}

#[tokio::test]
async fn read_file_bounds_bytes_without_loading_the_whole_file() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("large.txt");
    std::fs::write(&path, "0123456789abcdef\n".repeat(10_000)).expect("large fixture");

    let result = read_file(
        &json!({"path": "large.txt", "max_bytes": 32, "max_lines": 100}),
        directory.path(),
    )
    .await
    .expect("bounded read");

    assert!(result["content"].as_str().unwrap().len() <= 32);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["truncated_by_bytes"], true);
    assert_eq!(result["truncated_by_lines"], false);
    assert_eq!(result["total_lines_known"], false);
    assert!(result["bytes_scanned"].as_u64().unwrap() < result["total_bytes"].as_u64().unwrap());
}

#[tokio::test]
async fn read_file_reports_line_range_truncation_separately() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        directory.path().join("lines.txt"),
        "zero\none\ntwo\nthree\n",
    )
    .expect("line fixture");

    let result = read_file(
        &json!({"path": "lines.txt", "start_line": 1, "max_lines": 2}),
        directory.path(),
    )
    .await
    .expect("line bounded read");

    assert_eq!(result["content"], "one\ntwo");
    assert_eq!(result["start_line"], 1);
    assert_eq!(result["end_line"], 3);
    assert_eq!(result["total_lines"], 4);
    assert_eq!(result["truncated_by_bytes"], false);
    assert_eq!(result["truncated_by_lines"], true);
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_retains_bounded_tails_and_emits_progress_chunks() {
    let directory = tempfile::tempdir().expect("tempdir");
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let callback_events = Arc::clone(&events);
    let callback: TerminalOutputCallback = Arc::new(move |event| {
        callback_events.lock().unwrap().push(event);
    });
    let config = ExecutionConfig {
        provider: "internal".to_string(),
        default_profile: "internal".to_string(),
        ..ExecutionConfig::default()
    };
    let invocation_id = crate::tools::ToolInvocationId::new();

    let result = run_terminal(
            invocation_id,
            &json!({
                "command": "head -c 4096 /dev/zero | tr '\\0' x; printf 'STDOUT_TAIL'; head -c 4096 /dev/zero | tr '\\0' y >&2; printf 'STDERR_TAIL' >&2",
                "max_output_bytes": 64
            }),
            directory.path(),
            &config,
            "local",
            10,
            &HashMap::new(),
            Some(&callback),
        )
        .await
        .expect("terminal result");

    assert_eq!(result["stdout_truncated"], true);
    assert_eq!(result["stderr_truncated"], true);
    assert_eq!(result["stdout_bytes_retained"], 64);
    assert_eq!(result["stderr_bytes_retained"], 64);
    assert!(result["stdout"].as_str().unwrap().ends_with("STDOUT_TAIL"));
    assert!(result["stderr"].as_str().unwrap().ends_with("STDERR_TAIL"));
    {
        let events = events.lock().unwrap();
        assert!(events
            .iter()
            .any(|event| event.stream == TerminalOutputStream::Stdout));
        assert!(events
            .iter()
            .any(|event| event.stream == TerminalOutputStream::Stderr));
        assert!(events
            .iter()
            .all(|event| event.invocation_id == invocation_id));
    }

    let failed = run_terminal(
        crate::tools::ToolInvocationId::new(),
        &json!({
            "command": "head -c 4096 /dev/zero | tr '\\0' z >&2; printf 'FAILED_TAIL' >&2; exit 7",
            "max_output_bytes": 64
        }),
        directory.path(),
        &config,
        "local",
        10,
        &HashMap::new(),
        None,
    )
    .await
    .expect("structured terminal failure");
    assert_eq!(failed["command_success"], false);
    assert_eq!(failed["exit_code"], 7);
    assert_eq!(failed["stderr_truncated"], true);
    assert!(failed["stderr"].as_str().unwrap().ends_with("FAILED_TAIL"));
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains("command exited with 7"));
}

#[tokio::test]
async fn profile_memory_tool_persists_lists_and_removes_namespaces() {
    let directory = tempfile::tempdir().expect("tempdir");

    let set_environment = manage_memory(
        &json!({
            "action": "set",
            "namespace": "environment",
            "key": "test_command",
            "value": "task test"
        }),
        directory.path(),
    )
    .await
    .expect("set environment memory");
    assert_eq!(set_environment["updated"], true);
    manage_memory(
        &json!({
            "action": "set",
            "namespace": "user",
            "key": "response_style",
            "value": "concise"
        }),
        directory.path(),
    )
    .await
    .expect("set user memory");

    let listed = manage_memory(
        &json!({"action": "list", "namespace": "environment"}),
        directory.path(),
    )
    .await
    .expect("list environment memory");
    assert_eq!(listed["values"]["test_command"], "task test");
    let fetched = manage_memory(
        &json!({
            "action": "get",
            "namespace": "user",
            "key": "response_style"
        }),
        directory.path(),
    )
    .await
    .expect("get user memory");
    assert_eq!(fetched["value"], "concise");

    let removed = manage_memory(
        &json!({
            "action": "delete",
            "namespace": "environment",
            "key": "test_command"
        }),
        directory.path(),
    )
    .await
    .expect("delete environment memory");
    assert_eq!(removed["removed"], true);
    let missing = manage_memory(
        &json!({
            "action": "get",
            "namespace": "environment",
            "key": "test_command"
        }),
        directory.path(),
    )
    .await
    .expect("get removed memory");
    assert_eq!(missing["found"], false);

    let oversized_key = "k".repeat(257);
    assert!(manage_memory(
        &json!({
            "action": "get",
            "namespace": "environment",
            "key": oversized_key
        }),
        directory.path(),
    )
    .await
    .unwrap_err()
    .contains("at most 256"));
    let oversized_value = "v".repeat(65_537);
    assert!(manage_memory(
        &json!({
            "action": "set",
            "namespace": "user",
            "key": "bounded",
            "value": oversized_value
        }),
        directory.path(),
    )
    .await
    .unwrap_err()
    .contains("at most 65536"));
}

#[test]
fn parses_bounded_duckduckgo_fixture_without_network() {
    let fixture = r#"
            <div class="result">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fguide&amp;rut=abc">
                Example &amp; Guide
              </a>
              <a class="result__snippet">A <b>safe</b> guide.</a>
            </div>
            <div class="result">
              <a href="https://docs.example.org/page" class="result__a">Documentation</a>
              <div class="result__snippet">Reference material</div>
            </div>
        "#;

    let results = parse_search_results(fixture, 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], "Example & Guide");
    assert_eq!(results[0]["url"], "https://example.com/guide");
    assert_eq!(results[0]["snippet"], "A safe guide.");
}

#[test]
fn extracts_safe_markdown_and_discards_active_content() {
    let base = reqwest::Url::parse("https://example.com/docs/").unwrap();
    let html = r#"
            <title>API &amp; Guide</title>
            <script>alert('no')</script><style>.hidden {}</style>
            <h1>API &amp; Guide</h1>
            <p>Read <a href="reference">the docs</a> and
               <a href="javascript:alert(1)">ignore this link</a>.</p>
            <p>&lt;script&gt;encoded&lt;/script&gt;</p>
            <ul><li>First</li><li>Second</li></ul>
        "#;

    let markdown = html_to_safe_markdown(html, &base);
    assert!(markdown.contains("# API & Guide"));
    assert!(markdown.contains("[the docs](https://example.com/docs/reference)"));
    assert!(markdown.contains("ignore this link"));
    assert!(markdown.contains("- First"));
    assert!(!markdown.contains("alert('no')"));
    assert!(!markdown.contains("javascript:"));
    assert!(markdown.contains("&lt;script&gt;encoded&lt;/script&gt;"));
    assert_eq!(extract_page_title(html).as_deref(), Some("API & Guide"));
}

#[test]
fn rejects_non_http_and_credentialed_urls() {
    assert!(validate_http_url("file:///tmp/secret").is_err());
    assert!(validate_http_url("https://user:password@example.com").is_err());
    assert!(validate_http_url("https://example.com/docs").is_ok());
}

#[test]
fn rejects_local_private_link_local_and_metadata_hosts() {
    for url in [
        "http://localhost/admin",
        "http://api.localhost./admin",
        "http://127.0.0.1/admin",
        "http://2130706433/admin",
        "http://10.1.2.3/admin",
        "http://172.16.0.1/admin",
        "http://192.168.1.1/admin",
        "http://169.254.169.254/latest/meta-data",
        "http://100.64.0.1/admin",
        "http://224.0.0.1/admin",
        "http://[::1]/admin",
        "http://[fc00::1]/admin",
        "http://[fe80::1]/admin",
        "http://[fec0::1]/admin",
        "http://[::ffff:127.0.0.1]/admin",
        "http://[64:ff9b::a00:1]/admin",
    ] {
        assert!(validate_http_url(url).is_err(), "must reject {url}");
    }
}

#[tokio::test]
async fn resolver_rejects_private_or_mixed_dns_answers_and_pins_public_ports() {
    let resolver = StaticResolver {
        addresses: HashMap::from([
            (
                "private.example".to_string(),
                vec!["10.0.0.8:1".parse().unwrap()],
            ),
            (
                "mixed.example".to_string(),
                vec![
                    "93.184.216.34:1".parse().unwrap(),
                    "127.0.0.1:1".parse().unwrap(),
                ],
            ),
            (
                "public.example".to_string(),
                vec!["93.184.216.34:1".parse().unwrap()],
            ),
        ]),
    };

    for host in ["private.example", "mixed.example"] {
        let url = reqwest::Url::parse(&format!("https://{host}/docs")).unwrap();
        let error = resolve_public_destination(&url, &resolver)
            .await
            .err()
            .expect("private resolution rejected");
        assert!(error.contains("non-public address"), "{error}");
    }

    let public = reqwest::Url::parse("https://public.example:8443/docs").unwrap();
    let destination = resolve_public_destination(&public, &resolver)
        .await
        .expect("public resolution");
    assert_eq!(destination.host, "public.example");
    assert_eq!(destination.addresses[0].port(), 8443);
}

#[tokio::test]
async fn redirect_targets_receive_the_same_resolver_validation() {
    let resolver = StaticResolver {
        addresses: HashMap::from([(
            "internal.example".to_string(),
            vec!["192.168.1.20:443".parse().unwrap()],
        )]),
    };
    let origin = reqwest::Url::parse("https://public.example/start").unwrap();
    let redirect = validated_redirect_target(&origin, "https://internal.example/admin")
        .expect("syntactically valid redirect");
    let error = resolve_public_destination(&redirect, &resolver)
        .await
        .err()
        .expect("redirect resolution rejected");
    assert!(error.contains("non-public address"));
    assert!(validated_redirect_target(&origin, "http://127.0.0.1/admin").is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn recursive_grep_does_not_follow_file_symlinks_outside_scope() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("tempdir");
    let project = directory.path().join("project");
    std::fs::create_dir(&project).expect("project directory");
    std::fs::write(project.join("inside.txt"), "needle inside\n").expect("inside file");
    let external = directory.path().join("external-secret.txt");
    std::fs::write(&external, "needle secret\n").expect("external file");
    symlink(&external, project.join("linked-secret.txt")).expect("external symlink");

    let result = grep(&json!({"pattern": "needle", "path": "."}), &project)
        .await
        .expect("grep result");
    let matches = result["matches"].as_array().expect("matches");
    assert_eq!(matches.len(), 1);
    assert!(matches[0]["file"]
        .as_str()
        .expect("match file")
        .ends_with("inside.txt"));
    assert_eq!(matches[0]["snippet"], "needle inside");
}

#[tokio::test]
async fn grep_bounds_each_file_and_the_aggregate_scan() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        directory.path().join("a.txt"),
        "needle early\n0123456789abcdefghijklmnopqrstuvwxyz",
    )
    .expect("first fixture");
    std::fs::write(
        directory.path().join("b.txt"),
        "second file without a match but with a long tail",
    )
    .expect("second fixture");

    let result = grep_with_limits(
        &json!({"pattern": "needle", "path": "."}),
        directory.path(),
        20,
        28,
    )
    .await
    .expect("bounded grep");

    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    assert!(result["bytes_scanned"].as_u64().unwrap() <= 28);
    assert_eq!(result["files_scanned"], 2);
    assert_eq!(result["files_truncated"], 2);
    assert_eq!(result["aggregate_limit_reached"], true);
    assert_eq!(result["truncated"], true);
}

#[test]
fn failed_in_memory_registration_removes_prepared_terminal_record() {
    let root = tempfile::tempdir().expect("root");
    let sessions_dir = root.path().join("state/sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
    session_store.create_session_with_id("origin");
    let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        root.path().join("state/daemons"),
    )
    .expect("task store");
    task_store
        .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
            id: "duplicate-terminal".to_string(),
            command: "printf ok".to_string(),
            cwd: root.path().to_path_buf(),
            project_root: root.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir,
            session_id: "origin".to_string(),
            execution: ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        })
        .expect("prepare terminal");
    let manager = crate::daemons::task::TaskManager::new();
    manager
        .register_task("duplicate-terminal".to_string(), "terminal")
        .expect("seed duplicate");

    let error = register_prepared_durable_task(&manager, "duplicate-terminal", &task_store)
        .expect_err("duplicate registration fails");

    assert!(error.contains("already exists"), "{error}");
    assert!(task_store.get("duplicate-terminal").unwrap().is_none());
}

#[test]
fn successful_schedule_admission_records_execution_generation() {
    let root = tempfile::tempdir().expect("root");
    let sessions_dir = root.path().join("state/sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
    session_store.create_session_with_id("origin");
    let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        root.path().join("state/daemons"),
    )
    .expect("task store");
    let prepared = task_store
        .prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
            id: "generation-schedule".to_string(),
            prompt: "later".to_string(),
            project_root: root.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir,
            session_id: "origin".to_string(),
            initial_delay: Duration::from_secs(60),
            interval: Duration::from_secs(60),
            repeat_count: 1,
        })
        .expect("prepare schedule");
    let manager = crate::daemons::task::TaskManager::new();
    let audit_log =
        crate::daemons::task::DaemonAuditLog::at_path(task_store.daemon_dir().join("audit.jsonl"));

    register_and_audit_prepared_schedule(
        &manager,
        &task_store,
        &session_store,
        &audit_log,
        "generation-schedule",
        "origin",
        60,
        60,
        1,
        "later",
    )
    .expect("admit schedule");

    let session = session_store.load("origin").expect("origin session");
    assert!(session.events.iter().any(|event| {
        event.kind == "timer_scheduled"
            && event.details.get("execution_id").and_then(Value::as_str)
                == Some(prepared.execution_id.as_str())
    }));
    assert!(audit_log
        .read_all()
        .expect("daemon audit")
        .iter()
        .any(|record| {
            record.action == "schedule"
                && record.outcome == "scheduled"
                && record.detail.as_deref().is_some_and(|detail| {
                    detail.contains(&format!("execution_id={}", prepared.execution_id))
                })
        }));
}

#[test]
fn rejected_schedule_records_failure_only_after_admission_attempt() {
    let root = tempfile::tempdir().expect("root");
    let sessions_dir = root.path().join("state/sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
    session_store.create_session_with_id("origin");
    let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        root.path().join("state/daemons"),
    )
    .expect("task store");
    task_store
        .prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
            id: "duplicate-schedule".to_string(),
            prompt: "later".to_string(),
            project_root: root.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir,
            session_id: "origin".to_string(),
            initial_delay: Duration::from_secs(60),
            interval: Duration::from_secs(60),
            repeat_count: 1,
        })
        .expect("prepare schedule");
    let manager = crate::daemons::task::TaskManager::new();
    manager
        .register_task("duplicate-schedule".to_string(), "timer")
        .expect("seed duplicate");
    let audit_log =
        crate::daemons::task::DaemonAuditLog::at_path(task_store.daemon_dir().join("audit.jsonl"));

    register_and_audit_prepared_schedule(
        &manager,
        &task_store,
        &session_store,
        &audit_log,
        "duplicate-schedule",
        "origin",
        60,
        60,
        1,
        "later",
    )
    .expect_err("duplicate schedule is rejected");

    assert!(task_store.get("duplicate-schedule").unwrap().is_none());
    let session = session_store.load("origin").expect("origin session");
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "timer_schedule_failed"));
    assert!(!session
        .events
        .iter()
        .any(|event| event.kind == "timer_scheduled"));
    let records = audit_log.read_all().expect("daemon audit");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outcome, "failed");
}

#[test]
fn schedule_audit_failure_rolls_back_admitted_task() {
    let root = tempfile::tempdir().expect("root");
    let sessions_dir = root.path().join("state/sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
    session_store.create_session_with_id("origin");
    let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        root.path().join("state/daemons"),
    )
    .expect("task store");
    task_store
        .prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
            id: "audit-failure-schedule".to_string(),
            prompt: "later".to_string(),
            project_root: root.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir,
            session_id: "origin".to_string(),
            initial_delay: Duration::from_secs(60),
            interval: Duration::from_secs(60),
            repeat_count: 1,
        })
        .expect("prepare schedule");
    let manager = crate::daemons::task::TaskManager::new();
    let invalid_audit_path = task_store.daemon_dir().join("audit.jsonl");
    std::fs::create_dir(&invalid_audit_path).expect("invalid audit directory");
    let audit_log = crate::daemons::task::DaemonAuditLog::at_path(invalid_audit_path);

    register_and_audit_prepared_schedule(
        &manager,
        &task_store,
        &session_store,
        &audit_log,
        "audit-failure-schedule",
        "origin",
        60,
        60,
        1,
        "later",
    )
    .expect_err("audit failure rejects schedule");

    assert!(task_store.get("audit-failure-schedule").unwrap().is_none());
    assert!(manager.get_task("audit-failure-schedule").is_none());
    let session = session_store.load("origin").expect("origin session");
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "timer_schedule_failed"));
    assert!(!session
        .events
        .iter()
        .any(|event| event.kind == "timer_scheduled"));
    assert_eq!(
        session.events.last().map(|event| event.kind.as_str()),
        Some("timer_schedule_failed")
    );
}

#[cfg(unix)]
#[test]
fn schedule_compensation_surfaces_failure_to_terminalize_prepared_record() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("root");
    let sessions_dir = root.path().join("state/sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
    session_store.create_session_with_id("origin");
    let task_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        root.path().join("state/daemons"),
    )
    .expect("task store");
    task_store
        .prepare_schedule(crate::daemons::workload::DurableScheduleRequest {
            id: "unresolved-audit-schedule".to_string(),
            prompt: "later".to_string(),
            project_root: root.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir,
            session_id: "origin".to_string(),
            initial_delay: Duration::from_secs(60),
            interval: Duration::from_secs(60),
            repeat_count: 1,
        })
        .expect("prepare schedule");
    let manager = crate::daemons::task::TaskManager::new();
    let invalid_audit_path = task_store.daemon_dir().join("audit.jsonl");
    std::fs::create_dir(&invalid_audit_path).expect("invalid audit directory");
    let audit_log = crate::daemons::task::DaemonAuditLog::at_path(invalid_audit_path);
    let tasks_dir = task_store.daemon_dir().join("tasks");
    let original_permissions = std::fs::metadata(&tasks_dir).unwrap().permissions();
    let mut blocked_permissions = original_permissions.clone();
    blocked_permissions.set_mode(0o500);
    std::fs::set_permissions(&tasks_dir, blocked_permissions).expect("block task removal");

    let result = register_and_audit_prepared_schedule(
        &manager,
        &task_store,
        &session_store,
        &audit_log,
        "unresolved-audit-schedule",
        "origin",
        60,
        60,
        1,
        "later",
    );
    std::fs::set_permissions(&tasks_dir, original_permissions).expect("restore task directory");
    let error = result.expect_err("unresolved compensation must be surfaced");

    assert!(
        error.contains("failed to roll back admitted schedule"),
        "{error}"
    );
    assert!(
        error.contains("failed to terminalize admitted schedule"),
        "{error}"
    );
    assert_eq!(
        task_store
            .get("unresolved-audit-schedule")
            .unwrap()
            .unwrap()
            .status,
        "prepared"
    );
    let session = session_store.load("origin").expect("origin session");
    assert!(!session
        .events
        .iter()
        .any(|event| event.kind == "timer_scheduled"));
    let failure = session
        .events
        .iter()
        .find(|event| event.kind == "timer_schedule_failed")
        .expect("failure evidence");
    assert!(failure.details["error"]
        .as_str()
        .is_some_and(|detail| detail.contains("failed to terminalize admitted schedule")));
}
