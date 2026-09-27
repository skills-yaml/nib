use super::*;

#[tokio::test]
async fn reconciliation_releases_mutex_and_cas_rejects_stale_publication() {
    let root = tempdir().expect("reconciliation barrier repository");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    crate::tools::delegation::write_subagent_record(
        root.path(),
        &crate::tools::delegation::SubagentRecord {
            id: id.clone(),
            parent_session_id: Some(session.id.clone()),
            child_session_id: id.clone(),
            prompt: "terminal reconciliation".to_string(),
            status: "completed".to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: root.path().to_path_buf(),
            branch: format!("nib/subagent/{id}"),
            branch_oid: None,
            result: Some(json!({"outcome": "completed"})),
            error: None,
            verification: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    )
    .expect("terminal subagent record");
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
    let barrier = Arc::new(ReconciliationBarrier {
        subagent_id: id.clone(),
        entered: std::sync::atomic::AtomicBool::new(false),
        release: std::sync::atomic::AtomicBool::new(false),
    });
    *RECONCILIATION_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&barrier));
    let (audit, _audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");
    let thread_lifecycle = Arc::clone(&lifecycle);
    let project_root = root.path().to_path_buf();
    let thread = std::thread::spawn(move || {
        finish_request_lifecycle(
            &thread_lifecycle,
            1,
            &project_root,
            &subagent_start_class("nib_run"),
            HandledRequest {
                response: Some(rpc_result(
                    json!("stale-reconciliation"),
                    json!({
                        "isError": false,
                        "structuredContent": {"subagent_id": id}
                    }),
                )),
                cancellation_audit: Some(audit),
            },
        );
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !barrier.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reconciliation reached barrier");

    {
        let mut state = lifecycle
            .try_lock()
            .expect("lifecycle mutex must be released during reconciliation I/O");
        *state = RequestLifecycle::Reconciling { generation: 2 };
    }
    barrier.release.store(true, Ordering::Release);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || thread.join()),
    )
    .await
    .expect("reconciliation thread finished")
    .expect("reconciliation join task")
    .expect("reconciliation thread did not panic");
    *RECONCILIATION_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

    assert!(matches!(
        &*lock_request_lifecycle(&lifecycle),
        RequestLifecycle::Reconciling { generation: 2 }
    ));
    let session = store.load(&session.id).expect("audit session remains");
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "mcp_request_cancelled")
            .count(),
        0,
        "terminal completion must not be audited as cancellation"
    );
}

#[tokio::test]
async fn stale_completion_cannot_remove_reused_active_request_id() {
    let key = request_key(&json!("reused"));
    let task = tokio::spawn(std::future::pending::<()>());
    let mut active = HashMap::from([(
        key.clone(),
        ActiveRequest {
            generation: 2,
            id: json!("reused"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Running)),
            cancellation_class: RequestCancellationClass::Protocol,
            cancellation_audit: Arc::new(CancellationAuditSlot::default()),
            cancellation: crate::agent::CancellationSignal::new(),
            task,
        },
    )]);

    assert!(take_completed_request(
        &mut active,
        &CompletedRequest {
            key: key.clone(),
            generation: 1,
        },
    )
    .is_none());
    assert_eq!(active.len(), 1);
    let current = take_completed_request(&mut active, &CompletedRequest { key, generation: 2 })
        .expect("current generation completes");
    assert!(active.is_empty());
    current.task.abort();
    assert!(current
        .task
        .await
        .expect_err("test request cancellation")
        .is_cancelled());
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn nib_run_provider_failure_reaches_mcp_status_as_typed_llm_error() {
    const SECRET: &str = "mcp/provider+secret";
    const SECRET_PERCENT: &str = "mcp%2Fprovider%2Bsecret";
    const SECRET_BASE64: &str = "bWNwL3Byb3ZpZGVyK3NlY3JldA==";
    const REMOTE_BODY: &str = "REMOTE_MCP_PROVIDER_BODY";
    let root = tempdir().expect("mcp llm-failure repository");
    initialize_git_repository(root.path());
    let (base_url, _) = serve_once(
        "401 Unauthorized",
        "application/json",
        serde_json::json!({
            "error": {
                "code": "invalid_api_key",
                "message": format!(
                    "{REMOTE_BODY} {SECRET} {SECRET_PERCENT} {SECRET_BASE64} <red>[bold] \u{1b}[31m"
                )
            }
        })
        .to_string(),
    );
    let mut config = NibConfig::default();
    config.execution.plan_mode = false;
    config.skills.enabled = false;
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    config.llm.active_provider = Some("openai".to_string());
    config.llm.providers.insert(
        "openai".to_string(),
        ProviderEntry {
            model: "fixture-model".to_string(),
            api_key: Some(SECRET.to_string()),
            base_url: Some(base_url),
            api: Some(LlmApiMode::ChatCompletions),
            ..ProviderEntry::default()
        },
    );
    save_nib_config_full(root.path(), &mut config).expect("save config");
    let config = load_nib_config_full(root.path()).expect("reload config");

    let started = handle_request(
        root.path(),
        &config,
        json!({
            "jsonrpc": "2.0",
            "id": "run",
            "method": "tools/call",
            "params": {
                "name": "nib_run",
                "arguments": {"goal": "inspect the workspace", "max_steps": 1}
            }
        }),
    )
    .await
    .expect("nib_run response");
    assert_eq!(started["result"]["isError"], false, "{started}");
    let subagent_id = started["result"]["structuredContent"]["subagent_id"]
        .as_str()
        .expect("subagent id")
        .to_string();
    let started_payload = started.to_string();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let record = loop {
        let record = crate::tools::delegation::get_subagent_record(root.path(), &subagent_id)
            .expect("subagent record");
        if record.status != "running" {
            assert_eq!(record.status, "failed", "{record:?}");
            break record;
        }
        if std::time::Instant::now() > deadline {
            panic!("subagent did not finish: {record:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let result = record.result.as_ref().expect("typed subagent result");
    assert_eq!(result["outcome"], "planning_failed");
    assert_eq!(result["failure"]["incident_code"], "LLM-AUTH");
    assert_eq!(result["failure"]["class"], "authentication");

    let child_store = crate::session::SessionStore::for_project(&record.worktree_path)
        .expect("MCP child session store");
    let child = child_store
        .load(&record.child_session_id)
        .expect("MCP child failure session");
    child
        .validate_message_sequence()
        .expect("MCP child message sequence");
    assert!(!child.messages.iter().any(|message| {
        message.role == "assistant"
            && (message.content.contains("LLM") || message.content.contains("failed"))
    }));

    let status = handle_request(
        root.path(),
        &config,
        json!({
            "jsonrpc": "2.0",
            "id": "status",
            "method": "tools/call",
            "params": {
                "name": "nib_get_status",
                "arguments": {"session_id": subagent_id}
            }
        }),
    )
    .await
    .expect("status response");
    let payload = status.to_string();
    assert_eq!(status["result"]["isError"], false, "{status}");
    let structured = &status["result"]["structuredContent"];
    assert_eq!(structured["status"], "failed", "{status}");
    assert_eq!(structured["result"]["outcome"], "planning_failed");
    assert_eq!(structured["result"]["failure"]["incident_code"], "LLM-AUTH");
    assert_eq!(structured["result"]["failure"]["class"], "authentication");

    let repeated = handle_request(
        root.path(),
        &config,
        json!({
            "jsonrpc": "2.0",
            "id": "status-repeat",
            "method": "tools/call",
            "params": {
                "name": "nib_get_status",
                "arguments": {"session_id": subagent_id}
            }
        }),
    )
    .await
    .expect("repeated status response");
    assert_eq!(repeated["result"]["isError"], false, "{repeated}");
    assert_eq!(
        repeated["result"]["structuredContent"],
        status["result"]["structuredContent"]
    );

    let persisted_record = serde_json::to_string(&record).expect("subagent record JSON");
    let persisted_session = serde_json::to_string(&child).expect("child session JSON");
    let repeated_payload = repeated.to_string();
    for surface in [
        &started_payload,
        &payload,
        &repeated_payload,
        &persisted_record,
        &persisted_session,
    ] {
        assert!(surface.len() <= 64 * 1024, "observer payload is unbounded");
        for forbidden in [
            SECRET,
            SECRET_PERCENT,
            SECRET_BASE64,
            REMOTE_BODY,
            "invalid_api_key",
            "<red>",
            "[red]",
            "[bold]",
            "\u{1b}",
            "\\u001b",
            "\\u001B",
        ] {
            assert!(!surface.contains(forbidden), "found {forbidden}: {surface}");
        }
        assert!(surface.chars().all(|character| {
            !character.is_control() || matches!(character, '\n' | '\r' | '\t')
        }));
    }
}

#[tokio::test]
async fn invalid_tool_calls_never_create_audit_sessions() {
    let root = tempdir().unwrap();
    let invalid_params = [
        json!({"name": "", "arguments": {}}),
        json!({"name": "not_real", "arguments": {}}),
        json!({"name": "read_file", "arguments": {}}),
        json!({"name": "read_file", "arguments": {"path": 7}}),
        json!({"name": "nib_run", "arguments": {"goal": ""}}),
        json!({"name": "nib_run", "arguments": {"goal": "x", "max_steps": 101}}),
        json!({"name": "nib_get_status", "arguments": {"session_id": ""}}),
    ];

    for index in 0..256 {
        let response = handle_request(
            root.path(),
            &NibConfig::default(),
            json!({
                "jsonrpc": "2.0",
                "id": index,
                "method": "tools/call",
                "params": invalid_params[index % invalid_params.len()].clone()
            }),
        )
        .await
        .expect("invalid call response");
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }

    assert!(
        !root.path().join(".nib").exists(),
        "invalid calls must not initialize profile state or persist sessions"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn mcp_subagent_flow_accepts_a_dos_short_project_root() {
    let root = tempdir().expect("MCP DOS-alias repository");
    let _timeout = crate::tools::delegation::SubagentCancellationTimeoutGuard::set(
        std::time::Duration::from_secs(10),
    );
    initialize_git_repository(root.path());
    let config = save_profile_config(root.path());
    let canonical_root = root.path().canonicalize().expect("canonical repository");
    let short_root = crate::fs_security::windows_dos_short_path_for_test(&canonical_root)
        .expect("DOS short project root");
    if short_root == crate::fs_security::path_without_windows_verbatim_prefix(&canonical_root) {
        return;
    }

    let started = {
        let _spawn_timeout = crate::tools::delegation::SpawnPositiveProgressTimeoutGuard::set(
            std::time::Duration::from_secs(15),
        );
        handle_request(
            &short_root,
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "dos-alias-run",
                "method": "tools/call",
                "params": {
                    "name": "nib_run",
                    "arguments": {"goal": "Return a bounded fixture response.", "max_steps": 1}
                }
            }),
        )
        .await
        .expect("MCP subagent start response")
    };
    assert_eq!(started["result"]["isError"], false, "{started}");
    let id = started["result"]["structuredContent"]["subagent_id"]
        .as_str()
        .expect("subagent id");
    let record = crate::tools::delegation::get_subagent_record(&short_root, id)
        .expect("MCP delegation record");
    assert!(record.worktree_path.starts_with(&canonical_root));
    assert!(!record.worktree_path.starts_with(&short_root));

    let status = handle_request(
        &short_root,
        &config,
        json!({
            "jsonrpc": "2.0",
            "id": "dos-alias-status",
            "method": "tools/call",
            "params": {
                "name": "nib_get_status",
                "arguments": {"session_id": id}
            }
        }),
    )
    .await
    .expect("MCP status response");
    assert_eq!(status["result"]["isError"], false, "{status}");

    match crate::tools::delegation::resolve_subagent_cancellation_async(&short_root, id).await {
        crate::tools::delegation::CancelSubagentResolution::Cancelled { .. }
        | crate::tools::delegation::CancelSubagentResolution::Terminal { .. } => {}
        crate::tools::delegation::CancelSubagentResolution::Unresolved { error, .. } => {
            panic!("DOS-alias MCP cancellation was unresolved: {error}")
        }
    }
    crate::sandbox::worktree::Worktree::remove(&short_root, id)
        .expect("remove MCP worktree through DOS short root");
}
