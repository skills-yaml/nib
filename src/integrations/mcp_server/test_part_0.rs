use super::*;

#[test]
fn managed_process_scope_fixture_child() {
    if std::env::var_os(MANAGED_PROCESS_FIXTURE_CHILD_ENV).is_some() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

#[tokio::test]
async fn owned_response_writer_cancels_and_joins_a_blocked_write() {
    let polled = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let (mut writer, _failure_rx, _written_rx) = OwnedResponseWriter::from_writer(
        PendingWriter {
            polled: Arc::clone(&polled),
            dropped: Arc::clone(&dropped),
        },
        None,
    );
    writer
        .sender()
        .try_send(QueuedResponse {
            frame: vec![b'x'; 1024],
            request_key: Some("blocked".to_string()),
        })
        .expect("queue blocked response");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !polled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer reached blocked poll");

    tokio::time::timeout(std::time::Duration::from_secs(1), writer.shutdown())
        .await
        .expect("blocked writer shutdown is bounded");

    assert!(dropped.load(Ordering::Acquire));
    assert!(writer.task.is_none());
    assert!(writer.response_tx.is_none());
}

#[tokio::test]
async fn response_id_remains_owned_until_writer_flush_ack() {
    let (mut writer, _failure_rx, mut written_rx) =
        OwnedResponseWriter::from_writer(tokio::io::sink(), None);
    let mut pending = HashSet::new();
    let key = request_key(&json!("flush-owned"));
    queue_response(
        writer.sender(),
        &mut pending,
        &rpc_result(json!("flush-owned"), json!({})),
        Some(key.clone()),
    )
    .expect("queue response");
    assert!(pending.contains(&key));
    assert!(request_id_is_owned(&HashMap::new(), &pending, &key));

    let acknowledged = tokio::time::timeout(std::time::Duration::from_secs(1), written_rx.recv())
        .await
        .expect("writer ack is bounded")
        .expect("writer ack channel remains open");
    assert_eq!(acknowledged, key);
    assert!(
        pending.contains(&key),
        "ack must be consumed by coordinator"
    );
    pending.remove(&acknowledged);
    assert!(!request_id_is_owned(&HashMap::new(), &pending, &key));

    queue_response(
        writer.sender(),
        &mut pending,
        &rpc_result(json!("flush-owned"), json!({"second": true})),
        Some(key),
    )
    .expect("ID can be reused only after flush ack");
    writer.shutdown().await;
}

#[tokio::test]
async fn response_queue_accepts_full_completion_burst_and_keeps_parse_errors_unowned() {
    let (response_tx, mut response_rx) = mpsc::channel::<QueuedResponse>(MAX_ACTIVE_MCP_REQUESTS);
    let mut pending = HashSet::new();
    for index in 0..MAX_ACTIVE_MCP_REQUESTS {
        publish_completed_request(
            ActiveRequest {
                generation: index as u64,
                id: json!(index),
                task_kind: ActiveTaskKind::Execution,
                lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Completed(Some(
                    rpc_result(json!(index), json!({"index": index})),
                )))),
                cancellation_class: RequestCancellationClass::Protocol,
                cancellation_audit: Arc::new(CancellationAuditSlot::default()),
                cancellation: crate::agent::CancellationSignal::new(),
                task: tokio::spawn(async {}),
            },
            &response_tx,
            &mut pending,
        )
        .await
        .expect("every admitted request has response capacity");
    }
    assert_eq!(pending.len(), MAX_ACTIVE_MCP_REQUESTS);
    for _ in 0..MAX_ACTIVE_MCP_REQUESTS {
        assert!(response_rx.try_recv().is_ok());
    }

    let parse_error = rpc_error(Value::Null, -32700, "parse error");
    queue_response(&response_tx, &mut pending, &parse_error, None)
        .expect("first parse error response");
    queue_response(&response_tx, &mut pending, &parse_error, None)
        .expect("second parse error response with id:null");
    assert_eq!(pending.len(), MAX_ACTIVE_MCP_REQUESTS);
    let null_key = request_key(&Value::Null);
    queue_response(
        &response_tx,
        &mut pending,
        &rpc_result(Value::Null, json!({"legitimate": true})),
        Some(null_key.clone()),
    )
    .expect("parse errors do not reserve a legitimate null request ID");
    assert!(pending.contains(&null_key));
}

#[tokio::test]
async fn duplicate_active_or_pending_id_never_replaces_the_original_request() {
    let key = request_key(&json!("duplicate"));
    let task = tokio::spawn(std::future::pending::<()>());
    let mut active = HashMap::from([(
        key.clone(),
        ActiveRequest {
            generation: 7,
            id: json!("duplicate"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Running)),
            cancellation_class: RequestCancellationClass::Protocol,
            cancellation_audit: Arc::new(CancellationAuditSlot::default()),
            cancellation: crate::agent::CancellationSignal::new(),
            task,
        },
    )]);
    assert!(request_id_is_owned(&active, &HashSet::new(), &key));
    assert_eq!(active[&key].generation, 7);

    let original = active.remove(&key).expect("original remains active");
    let pending = HashSet::from([key.clone()]);
    assert!(request_id_is_owned(&active, &pending, &key));
    original.task.abort();
    assert!(original
        .task
        .await
        .expect_err("test request cancellation")
        .is_cancelled());
}

#[tokio::test]
async fn saturated_completion_fallback_is_level_triggered_and_hard_bounded() {
    let (completion_tx, _completion_rx) = mpsc::channel::<CompletedRequest>(1);
    completion_tx
        .try_send(CompletedRequest {
            key: "occupied".to_string(),
            generation: 0,
        })
        .expect("occupy completion channel");
    let missed = Arc::new(StdMutex::new(Vec::new()));
    let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let task_missed = Arc::clone(&missed);
    let task_overflow = Arc::clone(&overflow);
    let task_notify = Arc::clone(&notify);
    let task_release = Arc::clone(&release);
    let completion = CompletedRequest {
        key: request_key(&json!("missed")),
        generation: 3,
    };
    let task_completion = completion.clone();
    let task = tokio::spawn(async move {
        if completion_tx.try_send(task_completion.clone()).is_err() {
            store_missed_completion(&task_missed, &task_overflow, &task_notify, task_completion);
        }
        task_release.notified().await;
    });
    let mut active = HashMap::from([(
        completion.key.clone(),
        ActiveRequest {
            generation: completion.generation,
            id: json!("missed"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Completed(None))),
            cancellation_class: RequestCancellationClass::Protocol,
            cancellation_audit: Arc::new(CancellationAuditSlot::default()),
            cancellation: crate::agent::CancellationSignal::new(),
            task,
        },
    )]);

    notify.notified().await;
    assert!(take_ready_missed_requests(&mut active, &missed).is_empty());
    assert_eq!(
        missed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1,
        "wake-before-finish must retain the completion identity"
    );
    release.notify_one();
    while !active
        .get(&completion.key)
        .expect("request remains active")
        .task
        .is_finished()
    {
        tokio::task::yield_now().await;
    }
    let mut ready = take_ready_missed_requests(&mut active, &missed);
    assert_eq!(ready.len(), 1);
    ready.pop().expect("ready request").task.await.unwrap();
    assert!(active.is_empty());

    for index in 0..=MAX_ACTIVE_MCP_REQUESTS {
        store_missed_completion(
            &missed,
            &overflow,
            &notify,
            CompletedRequest {
                key: format!("overflow-{index}"),
                generation: 1,
            },
        );
    }
    assert_eq!(
        missed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        MAX_ACTIVE_MCP_REQUESTS
    );
    assert!(overflow.load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn list_contains_aliases_and_registry_schemas() {
    let tools = advertised_tools();
    assert!(tools.iter().any(|tool| tool["name"] == "nib_run"));
    assert!(tools.iter().any(|tool| tool["name"] == "read_file"));
    assert_eq!(
        tools
            .iter()
            .find(|tool| tool["name"] == "read_file")
            .unwrap()["inputSchema"]["required"][0],
        "path"
    );
}

#[tokio::test]
async fn nib_get_status_projects_running_subagent_ownership_authority() {
    let root = tempdir().expect("MCP status project");
    let config = save_profile_config(root.path());
    let id = format!("sub-public-mcp-{}", uuid::Uuid::new_v4());
    let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
        .expect("live owner lease");
    crate::tools::delegation::write_subagent_record(
        root.path(),
        &crate::tools::delegation::SubagentRecord {
            id: id.clone(),
            parent_session_id: Some("parent".to_string()),
            child_session_id: id.clone(),
            prompt: "MCP public projection fixture".to_string(),
            status: "running".to_string(),
            execution_generation: Some(owner_lease.execution_generation()),
            owner_lease: Some(owner_lease.lease_id().to_string()),
            worktree_path: root.path().join("worktree"),
            branch: format!("nib/subagent/{id}"),
            branch_oid: None,
            result: Some(json!({
                "_ownership_audit_target": {
                    "sessions_dir": root.path().join("private-mcp-audit-sessions"),
                    "directory_identity": "private-mcp-identity",
                }
            })),
            error: None,
            verification: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    )
    .expect("running subagent record");

    let response = handle_request(
        root.path(),
        &config,
        json!({
            "jsonrpc": "2.0",
            "id": "public-status",
            "method": "tools/call",
            "params": {
                "name": "nib_get_status",
                "arguments": {"session_id": id},
            }
        }),
    )
    .await
    .expect("MCP status response");
    assert_eq!(response["result"]["isError"], false, "{response}");
    let public = &response["result"]["structuredContent"];
    assert_eq!(public["status"], "running");
    assert!(public["result"].is_null());
    assert!(public.get("execution_generation").is_none());
    assert!(public.get("owner_lease").is_none());
    let encoded = response.to_string();
    assert!(!encoded.contains("_ownership_audit_target"));
    assert!(!encoded.contains("private-mcp-audit-sessions"));
    assert!(!encoded.contains("private-mcp-identity"));
}

#[test]
fn cancellation_classification_covers_all_subagent_aliases_and_effect_boundaries() {
    for name in ["nib_run", "spawn_subagent", "invoke_subagent"] {
        assert_eq!(
            subagent_start_tool(&tool_request(name, json!({}))),
            Some(name)
        );
        assert_eq!(
            classify_request_cancellation(&tool_request(name, json!({}))),
            RequestCancellationClass::SubagentStart {
                tool_name: name.to_string(),
            }
        );
    }
    assert_eq!(
        classify_request_cancellation(&tool_request("read_file", json!({"path": "x"}))),
        RequestCancellationClass::ReadOnlyTool {
            tool_name: "read_file".to_string(),
        }
    );
    assert_eq!(
        classify_request_cancellation(
            &tool_request("run_terminal", json!({"command": "sleep 1"}),)
        ),
        RequestCancellationClass::InterruptibleTool {
            tool_name: "run_terminal".to_string(),
        }
    );
    assert_eq!(
        classify_request_cancellation(&tool_request(
            "run_terminal",
            json!({"command": "sleep 1", "background": true}),
        )),
        RequestCancellationClass::EffectUnknownTool {
            tool_name: "run_terminal".to_string(),
        }
    );
    assert_eq!(
        classify_request_cancellation(&tool_request("schedule", json!({}))),
        RequestCancellationClass::EffectUnknownTool {
            tool_name: "schedule".to_string(),
        }
    );
    assert_eq!(
        classify_request_cancellation(&json!({
            "jsonrpc": "2.0",
            "id": "ping",
            "method": "ping"
        })),
        RequestCancellationClass::Protocol
    );
    assert!(subagent_start_tool(&tool_request("spawn_subagents", json!({}))).is_none());
}

#[test]
fn every_subagent_alias_uses_commit_aware_cancellation_reconciliation() {
    let root = tempdir().expect("alias cancellation repository");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    for tool_name in ["nib_run", "spawn_subagent", "invoke_subagent"] {
        let session = store.try_create_session().expect("audit session");
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
        let (audit, _state) = test_cancellation_audit(&store, &session.id, tool_name);
        finish_request_lifecycle(
            &lifecycle,
            1,
            root.path(),
            &subagent_start_class(tool_name),
            HandledRequest {
                response: Some(rpc_error(json!(tool_name), -32602, "precommit failure")),
                cancellation_audit: Some(audit),
            },
        );
        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::Cancelled
        ));
        let session = store.load(&session.id).expect("audit session remains");
        let event = session
            .events
            .iter()
            .find(|event| event.kind == "mcp_request_cancelled")
            .expect("alias cancellation audit");
        assert_eq!(event.details["tool_name"], tool_name);
        assert_eq!(event.details["phase"], "precommit");
    }
}

#[tokio::test]
async fn failed_subagent_audit_remains_a_shutdown_error() {
    let root = tempdir().expect("failed audit repository");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
    let (audit, state) = test_cancellation_audit(&store, &session.id, "nib_run");
    state.injected_failures.store(2, Ordering::Release);
    finish_request_lifecycle(
        &lifecycle,
        1,
        root.path(),
        &subagent_start_class("nib_run"),
        HandledRequest {
            response: Some(rpc_error(
                json!("failed-audit"),
                -32602,
                "precommit failure",
            )),
            cancellation_audit: Some(audit),
        },
    );
    assert!(matches!(
        &*lock_request_lifecycle(&lifecycle),
        RequestLifecycle::CancellationFailed { .. }
    ));
    let mut active = HashMap::from([(
        request_key(&json!("failed-audit")),
        ActiveRequest {
            generation: 1,
            id: json!("failed-audit"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle,
            cancellation_class: subagent_start_class("nib_run"),
            cancellation_audit: Arc::new(CancellationAuditSlot::default()),
            cancellation: crate::agent::CancellationSignal::new(),
            task: tokio::spawn(async {}),
        },
    )]);
    let error = cancel_all_requests(&mut active)
        .await
        .expect_err("shutdown must propagate cancellation audit failure");
    assert!(error.contains("cancellation audit failed"), "{error}");
}

#[test]
fn cancellation_reservation_can_persist_before_session_initialization() {
    let root = tempdir().expect("audit repository");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session_id = "reserved-before-initialization";
    let (mut guard, state) = test_cancellation_audit(&store, session_id, "read_file");

    state
        .claim_cancellation()
        .expect("claim cancellation reservation");
    state
        .finalize_cancelled(json!({
            "tool_name": "read_file",
            "outcome": "cancelled",
            "reconciled": true,
            "effect_state": "none",
        }))
        .expect("reservation creates its authoritative audit session");
    guard.armed = false;

    let initialized = store
        .try_create_session_with_id(session_id)
        .expect("normal initialization is idempotent after cancellation");
    let events = initialized
        .events
        .iter()
        .filter(|event| event.kind == "mcp_request_cancelled")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].details["tool_name"], "read_file");
}

#[test]
fn cancellation_and_drop_fallback_claim_exactly_one_owner() {
    let root = tempdir().expect("audit repository");
    let store = SessionStore::for_project(root.path()).expect("profile session store");

    for index in 0..32 {
        let (mut guard, state) =
            test_cancellation_audit(&store, &format!("ownership-race-{index}"), "read_file");
        guard.armed = false;
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let explicit_state = Arc::clone(&state);
        let explicit_barrier = Arc::clone(&barrier);
        let explicit = std::thread::spawn(move || {
            explicit_barrier.wait();
            explicit_state.claim_cancellation().is_ok()
        });
        let fallback_state = Arc::clone(&state);
        let fallback_barrier = Arc::clone(&barrier);
        let fallback = std::thread::spawn(move || {
            fallback_barrier.wait();
            fallback_state.claim_fallback()
        });
        barrier.wait();
        let explicit_won = explicit.join().expect("explicit ownership thread");
        let fallback_won = fallback.join().expect("fallback ownership thread");
        assert_ne!(explicit_won, fallback_won, "iteration {index}");
        let status = *state
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            status,
            if explicit_won {
                McpCancellationAuditStatus::CancellationOwned
            } else {
                McpCancellationAuditStatus::FallbackOwned
            },
            "iteration {index}"
        );
    }

    let session = store.try_create_session().expect("audit session");
    let (guard, state) = test_cancellation_audit(&store, &session.id, "read_file");
    state
        .claim_cancellation()
        .expect("explicit cancellation owns audit");
    drop(guard);
    state
        .finalize_cancelled(json!({
            "tool_name": "read_file",
            "source": "explicit_test_owner",
        }))
        .expect("explicit owner persists precise details");
    let session = store.load(&session.id).expect("audit session remains");
    let event = session
        .events
        .iter()
        .find(|event| event.kind == "mcp_request_cancelled")
        .expect("explicit cancellation audit");
    assert_eq!(event.details["source"], "explicit_test_owner");
}

#[test]
fn post_commit_audit_error_is_reconciled_without_duplicate_events() {
    let root = tempdir().expect("audit repository");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    let (guard, state) = test_cancellation_audit(&store, &session.id, "apply_patch");
    state
        .injected_post_commit_failures
        .store(1, Ordering::Release);

    let details = json!({
        "tool_name": "apply_patch",
        "outcome": "unresolved",
        "reconciled": false,
        "effect_state": "unknown",
    });
    state
        .finalize_cancelled(details.clone())
        .expect("authoritative reread observes committed audit");
    state
        .finalize_cancelled(details)
        .expect("idempotent repeated finalization");
    drop(guard);

    let session = store.load(&session.id).expect("audit session remains");
    let events = session
        .events
        .iter()
        .filter(|event| event.kind == "mcp_request_cancelled")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        events[0].details["cancellation_id"],
        Value::String(state.cancellation_id.clone())
    );
}

#[tokio::test]
async fn immediate_read_only_cancellation_waits_for_audit_initialization() {
    let root = tempdir().expect("immediate cancellation repository");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    let cancellation = crate::agent::CancellationSignal::new();
    let task_cancellation = cancellation.clone();
    let slot = Arc::new(CancellationAuditSlot::default());
    let task_slot = Arc::clone(&slot);
    let task_store = store.clone();
    let task_session_id = session.id.clone();
    let task = tokio::spawn(async move {
        task_cancellation.cancelled().await;
        let (guard, state) = test_cancellation_audit(&task_store, &task_session_id, "read_file");
        task_slot.set(state);
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    let mut active = HashMap::from([(
        request_key(&json!("immediate")),
        ActiveRequest {
            generation: 1,
            id: json!("immediate"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Running)),
            cancellation_class: RequestCancellationClass::ReadOnlyTool {
                tool_name: "read_file".to_string(),
            },
            cancellation_audit: slot,
            cancellation,
            task,
        },
    )]);

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        cancel_active_request(active.drain().next().expect("active request").1),
    )
    .await
    .expect("immediate cancellation is bounded");
    assert!(matches!(outcome, CancellationOutcome::Cancelled));
    let session = store.load(&session.id).expect("audit session remains");
    let event = session
        .events
        .iter()
        .find(|event| event.kind == "mcp_request_cancelled")
        .expect("cancellation audit event");
    assert_eq!(event.details["reconciled"], true);
    assert_eq!(event.details["effect_state"], "none");
}

#[tokio::test]
async fn effectful_cancellation_fails_closed_with_unknown_effect_state() {
    let root = tempdir().expect("effect cancellation repository");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    let (guard, state) = test_cancellation_audit(&store, &session.id, "apply_patch");
    let slot = Arc::new(CancellationAuditSlot::default());
    slot.set(Arc::clone(&state));
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Running));
    let outcome = cancel_active_request(ActiveRequest {
        generation: 4,
        id: json!("effectful"),
        task_kind: ActiveTaskKind::Execution,
        lifecycle: Arc::clone(&lifecycle),
        cancellation_class: RequestCancellationClass::EffectUnknownTool {
            tool_name: "apply_patch".to_string(),
        },
        cancellation_audit: slot,
        cancellation: crate::agent::CancellationSignal::new(),
        task: tokio::spawn(std::future::pending::<()>()),
    })
    .await;
    drop(guard);

    let CancellationOutcome::Failed(error) = outcome else {
        panic!("effectful cancellation must fail closed");
    };
    assert!(error.contains("effect state is unknown"), "{error}");
    assert!(matches!(
        &*lock_request_lifecycle(&lifecycle),
        RequestLifecycle::CancellationFailed { .. }
    ));
    let session = store.load(&session.id).expect("audit session remains");
    let event = session
        .events
        .iter()
        .find(|event| event.kind == "mcp_request_cancelled")
        .expect("cancellation audit event");
    assert_eq!(event.details["reconciled"], false);
    assert_eq!(event.details["effect_state"], "unknown");
}

#[tokio::test]
async fn shutdown_joins_requests_and_propagates_cancellation_failure() {
    let joined = Arc::new(AtomicBool::new(false));
    let task_joined = Arc::clone(&joined);
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = release_rx.await;
        task_joined.store(true, Ordering::Release);
    });
    let mut active = HashMap::from([(
        request_key(&json!("shutdown")),
        ActiveRequest {
            generation: 1,
            id: json!("shutdown"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle: Arc::new(StdMutex::new(RequestLifecycle::CancellationFailed {
                response: rpc_error(json!("shutdown"), -32603, "audit failed"),
                error: "audit failed".to_string(),
            })),
            cancellation_class: RequestCancellationClass::Protocol,
            cancellation_audit: Arc::new(CancellationAuditSlot::default()),
            cancellation: crate::agent::CancellationSignal::new(),
            task,
        },
    )]);

    let mut shutdown = Box::pin(cancel_all_requests(&mut active));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut shutdown)
            .await
            .is_err(),
        "shutdown must not return before the owned request task exits"
    );
    release_tx.send(()).expect("release request task");
    let shutdown = tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown is bounded after task release");
    let shutdown_error = shutdown.expect_err("audit failure propagates");
    assert_eq!(shutdown_error, "audit failed");
    assert!(
        joined.load(Ordering::Acquire),
        "request task must be joined"
    );
    assert_eq!(
        merge_server_shutdown_result(Ok(()), Err(shutdown_error)),
        Err("audit failed".to_string())
    );
    let combined = merge_server_shutdown_result(
        Err("transport failed".to_string()),
        Err("audit failed".to_string()),
    )
    .expect_err("both failures propagate");
    assert!(combined.contains("transport failed"));
    assert!(combined.contains("request shutdown failed: audit failed"));
}

#[tokio::test]
async fn shutdown_hands_off_a_permanently_pending_subagent_request() {
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Running));
    let cancellation = crate::agent::CancellationSignal::new();
    let observed_cancellation = cancellation.clone();
    let mut active = HashMap::from([(
        request_key(&json!("pending-subagent")),
        ActiveRequest {
            generation: 1,
            id: json!("pending-subagent"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle: Arc::clone(&lifecycle),
            cancellation_class: subagent_start_class("nib_run"),
            cancellation_audit: Arc::new(CancellationAuditSlot::default()),
            cancellation,
            task: tokio::spawn(std::future::pending::<()>()),
        },
    )]);

    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        cancel_all_requests(&mut active),
    )
    .await
    .expect("subagent shutdown handoff is bounded")
    .expect_err("an incomplete handoff remains a shutdown error");

    assert!(active.is_empty());
    assert!(observed_cancellation.is_cancelled());
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert!(error.contains("durable cancellation reconciliation was handed off"));
    assert!(matches!(
        &*lock_request_lifecycle(&lifecycle),
        RequestLifecycle::CancelRequested
    ));
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn subagent_shutdown_joins_held_lock_reconciliation_before_returning() {
    let root = tempdir().expect("held-lock cancellation repository");
    let id = format!("sub-held-cancel-{}", uuid::Uuid::new_v4());
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
        .expect("subagent owner lease");
    crate::tools::delegation::write_subagent_record(
        root.path(),
        &crate::tools::delegation::SubagentRecord {
            id: id.clone(),
            parent_session_id: Some(session.id.clone()),
            child_session_id: id.clone(),
            prompt: "held record lock".to_string(),
            status: "running".to_string(),
            execution_generation: Some(owner_lease.execution_generation()),
            owner_lease: Some(owner_lease.lease_id().to_string()),
            worktree_path: root.path().to_path_buf(),
            branch: format!("nib/subagent/{id}"),
            branch_oid: None,
            result: None,
            error: None,
            verification: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    )
    .expect("running subagent record");
    drop(owner_lease);
    let held = crate::tools::delegation::hold_subagent_record_lock_for_test(root.path(), &id)
        .expect("held subagent record lock");
    crate::daemons::task::TASK_MANAGER
        .register_task(id.clone(), "subagent")
        .expect("subagent task");
    let manager_task = tokio::spawn(std::future::pending::<()>());
    crate::daemons::task::TASK_MANAGER
        .attach_abort_handle(&id, manager_task.abort_handle())
        .expect("subagent abort handle");

    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Running));
    let cancellation = crate::agent::CancellationSignal::new();
    let task_cancellation = cancellation.clone();
    let task_lifecycle = Arc::clone(&lifecycle);
    let task_root = root.path().to_path_buf();
    let cancellation_class = subagent_start_class("nib_run");
    let task_cancellation_class = cancellation_class.clone();
    let response = rpc_result(
        json!("held-lock-cancel"),
        json!({
            "isError": false,
            "structuredContent": {"subagent_id": id.clone()}
        }),
    );
    let (audit, audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");
    let audit_slot = Arc::new(CancellationAuditSlot::default());
    audit_slot.set(audit_state);
    let task = tokio::spawn(async move {
        task_cancellation.cancelled().await;
        finish_request_lifecycle_async(
            &task_lifecycle,
            1,
            &task_root,
            &task_cancellation_class,
            HandledRequest {
                response: Some(response),
                cancellation_audit: Some(audit),
            },
        )
        .await;
    });

    let started = std::time::Instant::now();
    let outcome = cancel_active_request(ActiveRequest {
        generation: 1,
        id: json!("held-lock-cancel"),
        task_kind: ActiveTaskKind::Execution,
        lifecycle: Arc::clone(&lifecycle),
        cancellation_class,
        cancellation_audit: audit_slot,
        cancellation,
        task,
    })
    .await;
    let CancellationOutcome::Failed(error) = outcome else {
        panic!("held-lock reconciliation must fail closed");
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "MCP shutdown exceeded its owned reconciliation window"
    );
    assert!(!error.contains("handed off"), "{error}");
    assert!(
        error.contains("delegation state lock deadline elapsed"),
        "{error}"
    );
    assert!(matches!(
        &*lock_request_lifecycle(&lifecycle),
        RequestLifecycle::CancellationFailed { .. }
    ));
    assert!(manager_task
        .await
        .expect_err("subagent manager task is cancelled")
        .is_cancelled());

    drop(held);
    tokio::time::sleep(crate::tools::delegation::SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT * 2)
        .await;
    let record_path = root
        .path()
        .join(".nib")
        .join("subagents")
        .join(format!("{id}.json"));
    let persisted: crate::tools::delegation::SubagentRecord = serde_json::from_slice(
        &std::fs::read(record_path).expect("unreconciled subagent record bytes"),
    )
    .expect("unreconciled subagent record");
    assert_eq!(persisted.status, "running");
    assert!(
        persisted.result.is_none(),
        "MCP returned while a detached reconciler could still mutate durable state"
    );
}

#[test]
fn oversized_server_response_becomes_a_bounded_rpc_error() {
    let response = rpc_result(
        json!("oversized"),
        json!({"payload": "x".repeat(MAX_MCP_FRAME_BYTES)}),
    );

    let frame = bounded_response_frame(&response).expect("bounded fallback response");

    assert!(frame.len() <= MAX_MCP_FRAME_BYTES);
    assert_eq!(frame.last(), Some(&b'\n'));
    let decoded: Value = serde_json::from_slice(&frame).expect("valid JSON response");
    assert_eq!(decoded["id"], "oversized");
    assert_eq!(decoded["error"]["code"], -32603);
}

#[test]
fn oversized_tool_output_becomes_a_small_error_before_content_duplication() {
    let content = tool_result_content(ToolResult {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "fixture".to_string(),
        success: true,
        output: Some(json!({"payload": "x".repeat(MAX_MCP_TOOL_OUTPUT_BYTES)})),
        error: None,
        duration_seconds: 0.1,
        approval_granted: true,
        approval_source: Some("fixture".to_string()),
    });

    assert_eq!(content["isError"], true);
    assert!(content["structuredContent"].is_null());
    assert!(content["content"][0]["text"]
        .as_str()
        .expect("bounded error text")
        .contains("serialized MCP limit"));
    let response = rpc_result(json!(1), content);
    let frame = bounded_response_frame(&response).expect("small bounded response");
    assert!(frame.len() < 4096);
}

#[tokio::test]
async fn read_tool_uses_executor_and_records_audit() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("hello.txt"), "hello MCP").unwrap();
    let response = handle_request(
        root.path(),
        &NibConfig::default(),
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "read_file", "arguments": {"path": "hello.txt"}}
        }),
    )
    .await
    .unwrap();
    assert_eq!(response["result"]["isError"], false);
    let store = SessionStore::for_project(root.path()).expect("profile store");
    let audited = store
        .list()
        .into_iter()
        .filter_map(|id| store.load(&id))
        .any(|session| {
            session
                .tool_calls
                .iter()
                .any(|call| call.tool_name.as_deref() == Some("read_file"))
        });
    assert!(audited, "MCP calls must be recorded by ToolExecutor");
    assert!(!root.path().join(".nib/sessions").exists());
}

#[tokio::test]
async fn direct_executor_uses_selected_profile_store_and_environment() {
    let root = tempdir().unwrap();
    initialize_git_repository(root.path());
    std::fs::write(
        root.path().join("AGENTS.md"),
        "- nib-policy: allow run_terminal printf\n",
    )
    .expect("explicit noninteractive allow policy");
    let config = save_profile_config(root.path());

    let response = handle_request(
        root.path(),
        &config,
        json!({
            "jsonrpc": "2.0",
            "id": "profile-env",
            "method": "tools/call",
            "params": {
                "name": "run_terminal",
                "arguments": {"command": "printf %s \"$NIB_PROFILE_VALUE\""}
            }
        }),
    )
    .await
    .unwrap();

    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["stdout"],
        "profile-scoped"
    );
    let store = SessionStore::for_project(root.path()).expect("profile store");
    let expected_sessions = root.path().join(".nib/profiles/workspace/sessions");
    assert!(
        crate::fs_security::canonical_paths_match(store.sessions_dir(), &expected_sessions),
        "profile store {} does not match expected sessions path {}",
        store.sessions_dir().display(),
        expected_sessions.display()
    );
    assert!(store.list().into_iter().any(|id| {
        store.load(&id).is_some_and(|session| {
            session.tool_calls.iter().any(|call| {
                call.tool_name.as_deref() == Some("run_terminal")
                    && call.result.as_ref().is_some_and(|result| {
                        result["environment_keys"]
                            .as_array()
                            .is_some_and(|keys| keys.iter().any(|key| key == "NIB_PROFILE_VALUE"))
                    })
            })
        })
    }));
    assert!(!root.path().join(".nib/sessions").exists());
}

#[tokio::test]
async fn destructive_tool_fails_closed_without_interactive_approval() {
    let root = tempdir().unwrap();
    let response = handle_request(
        root.path(),
        &NibConfig::default(),
        json!({
            "jsonrpc": "2.0",
            "id": "deny",
            "method": "tools/call",
            "params": {"name": "run_terminal", "arguments": {"command": "touch denied"}}
        }),
    )
    .await
    .unwrap();
    assert_eq!(response["result"]["isError"], true);
    assert!(!root.path().join("denied").exists());
}

#[tokio::test]
async fn status_returns_only_the_requested_agent_run() {
    let root = tempdir().unwrap();
    let now = chrono::Utc::now();
    for id in ["first", "second"] {
        crate::tools::delegation::write_subagent_record(
            root.path(),
            &crate::tools::delegation::SubagentRecord {
                id: id.to_string(),
                parent_session_id: Some("parent".to_string()),
                child_session_id: format!("child-{id}"),
                prompt: "test".to_string(),
                status: "completed".to_string(),
                execution_generation: None,
                owner_lease: None,
                worktree_path: root.path().to_path_buf(),
                branch: format!("nib/{id}"),
                branch_oid: None,
                result: Some(json!({"id": id})),
                error: None,
                verification: None,
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();
    }

    let response = handle_request(
        root.path(),
        &NibConfig::default(),
        json!({
            "jsonrpc": "2.0",
            "id": "status",
            "method": "tools/call",
            "params": {
                "name": "nib_get_status",
                "arguments": {"session_id": "child-second"}
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(response["result"]["structuredContent"]["id"], "second");
    assert!(!response["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("first"));
}

#[tokio::test]
#[serial_test::serial]
async fn status_rejects_credential_derived_identifiers_before_audit_or_reflection() {
    const SECRET: &str = "mcp/status-secret";
    const ENVIRONMENT_SECRET: &str = "environment-status-secret";
    let root = tempdir().unwrap();
    let _environment = EnvironmentGuard::set("OPENAI_API_KEY", ENVIRONMENT_SECRET);
    let mut config = NibConfig::default();
    config.llm.providers.insert(
        "openai".to_string(),
        ProviderEntry {
            model: "fixture-model".to_string(),
            api_key: Some(SECRET.to_string()),
            ..ProviderEntry::default()
        },
    );

    for session_id in [
        SECRET.to_string(),
        format!("prefix-{SECRET}-suffix"),
        "mcp%2Fstatus-secret".to_string(),
        "bWNwL3N0YXR1cy1zZWNyZXQ=".to_string(),
        r"mcp\/status-secret".to_string(),
        ENVIRONMENT_SECRET.to_string(),
        "ZW52aXJvbm1lbnQtc3RhdHVzLXNlY3JldA==".to_string(),
    ] {
        let response = handle_request(
            root.path(),
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "sensitive-status",
                "method": "tools/call",
                "params": {
                    "name": "nib_get_status",
                    "arguments": {"session_id": session_id}
                }
            }),
        )
        .await
        .expect("sensitive status response");
        let rendered = response.to_string();
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(rendered.contains("session identifier conflicts with configured sensitive data"));
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(!rendered.contains("bWNwL3N0YXR1cy1zZWNyZXQ"), "{rendered}");
        assert!(!rendered.contains(ENVIRONMENT_SECRET), "{rendered}");
        assert!(
            !rendered.contains("ZW52aXJvbm1lbnQtc3RhdHVzLXNlY3JldA"),
            "{rendered}"
        );
    }

    assert!(
        !root.path().join(".nib").exists(),
        "rejected status calls must not initialize profile state or create audit sessions"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn schema_validation_errors_never_reflect_environment_credentials() {
    const SECRET: &str = "mcp/invalid-arg-secret";
    const BASE64_SECRET: &str = "bWNwL2ludmFsaWQtYXJnLXNlY3JldA==";
    let root = tempdir().expect("MCP project");
    let _environment = EnvironmentGuard::set("OPENAI_API_KEY", SECRET);

    for invalid_value in [
        SECRET.to_string(),
        r"mcp\/invalid-arg-secret".to_string(),
        BASE64_SECRET.to_string(),
        format!("{SECRET}\u{1b}[2J\u{202e}"),
    ] {
        let response = handle_request(
            root.path(),
            &NibConfig::default(),
            json!({
                "jsonrpc": "2.0",
                "id": "invalid-arguments",
                "method": "tools/call",
                "params": {
                    "name": "nib_run",
                    "arguments": {
                        "goal": "safe goal",
                        "max_steps": invalid_value
                    }
                }
            }),
        )
        .await
        .expect("schema error response");
        let message = response["error"]["message"]
            .as_str()
            .expect("bounded validation message");
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(message.contains("/max_steps"), "{message}");
        assert!(message.contains("schema constraint"), "{message}");
        for forbidden in [
            SECRET,
            r"mcp\/invalid-arg-secret",
            BASE64_SECRET,
            "\u{1b}",
            "\u{202e}",
        ] {
            assert!(!message.contains(forbidden), "{message:?}");
        }
        assert!(message.len() <= MAX_MCP_VALIDATION_ERROR_BYTES);
    }

    assert!(
        !root.path().join(".nib").exists(),
        "schema rejection must precede audit-session initialization"
    );
}

#[tokio::test]
async fn notifications_have_no_response_and_unknown_tools_are_errors() {
    let root = tempdir().unwrap();
    assert!(handle_request(
        root.path(),
        &NibConfig::default(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await
    .is_none());

    let response = handle_request(
        root.path(),
        &NibConfig::default(),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "not_real", "arguments": {}}
        }),
    )
    .await
    .unwrap();
    assert_eq!(response["error"]["code"], -32602);
}

#[tokio::test]
async fn postcommit_nib_run_completion_win_has_no_cancellation_audit() {
    let _timeout = crate::tools::delegation::SubagentCancellationTimeoutGuard::set(
        std::time::Duration::from_secs(5),
    );
    let root = tempdir().expect("completion-win repository");
    initialize_git_repository(root.path());
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    crate::tools::delegation::write_subagent_record(
        root.path(),
        &crate::tools::delegation::SubagentRecord {
            id: id.clone(),
            parent_session_id: Some(session.id.clone()),
            child_session_id: id.clone(),
            prompt: "already complete".to_string(),
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
    .expect("completed subagent record");
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
    let response = rpc_result(
        json!("postcommit-complete"),
        json!({
            "isError": false,
            "structuredContent": {
                "subagent_id": id.clone(),
                "parent_session_id": session.id.clone(),
            }
        }),
    );
    let (audit, _audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");
    let handled = HandledRequest {
        response: Some(response),
        cancellation_audit: Some(audit),
    };

    finish_request_lifecycle(
        &lifecycle,
        1,
        root.path(),
        &subagent_start_class("nib_run"),
        handled,
    );

    assert!(matches!(
        &*lock_request_lifecycle(&lifecycle),
        RequestLifecycle::Completed(Some(_))
    ));
    let session = store.load(&session.id).expect("audit session remains");
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "mcp_request_cancelled")
            .count(),
        0,
        "completion winner must not be audited as cancellation"
    );
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn one_worker_postcommit_nib_run_cancellation_records_exactly_one_rich_audit() {
    let root = tempdir().expect("cancellation-win repository");
    #[cfg(windows)]
    let cancellation_timeout = std::time::Duration::from_secs(15);
    #[cfg(not(windows))]
    let cancellation_timeout = std::time::Duration::from_secs(2);
    let _timeout =
        crate::tools::delegation::SubagentCancellationTimeoutGuard::set(cancellation_timeout);
    initialize_git_repository(root.path());
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let worktree =
        crate::sandbox::worktree::Worktree::create(root.path(), &id).expect("subagent worktree");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
        .expect("subagent owner lease");
    let cleanup_proof =
        ManagedProcessFixture::start(root.path(), &id, owner_lease.execution_generation())
            .complete("cancelled");
    crate::tools::delegation::write_subagent_record(
        root.path(),
        &crate::tools::delegation::SubagentRecord {
            id: id.clone(),
            parent_session_id: Some(session.id.clone()),
            child_session_id: id.clone(),
            prompt: "cancel me".to_string(),
            status: "running".to_string(),
            execution_generation: Some(owner_lease.execution_generation()),
            owner_lease: Some(owner_lease.lease_id().to_string()),
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: None,
            result: None,
            error: None,
            verification: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    )
    .expect("running subagent record");
    crate::daemons::task::TASK_MANAGER
        .register_task(id.clone(), "subagent")
        .expect("subagent task");
    let running = tokio::spawn(std::future::pending::<()>());
    crate::daemons::task::TASK_MANAGER
        .attach_abort_handle(&id, running.abort_handle())
        .expect("subagent abort handle");
    let owner_id = id.clone();
    let owner_release = tokio::spawn(async move {
        let started = std::time::Instant::now();
        while crate::daemons::task::TASK_MANAGER
            .get_status(&owner_id)
            .as_deref()
            != Some("cancelled")
        {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "subagent manager never entered cancelled state"
            );
            tokio::task::yield_now().await;
        }
        drop(owner_lease);
    });
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
    let response = rpc_result(
        json!("postcommit-cancel"),
        json!({
            "isError": false,
            "structuredContent": {
                "subagent_id": id.clone(),
                "parent_session_id": session.id.clone(),
            }
        }),
    );
    let (audit, audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");
    audit_state.injected_failures.store(1, Ordering::Release);
    crate::tools::delegation::inject_cancelled_record_write_failures(&id, 1);
    let handled = HandledRequest {
        response: Some(response),
        cancellation_audit: Some(audit),
    };

    finish_request_lifecycle_async(
        &lifecycle,
        1,
        root.path(),
        &subagent_start_class("nib_run"),
        handled,
    )
    .await;
    owner_release.await.expect("owner lease release task");

    assert!(matches!(
        &*lock_request_lifecycle(&lifecycle),
        RequestLifecycle::Cancelled
    ));
    assert!(
        running.await.expect_err("cancelled task").is_cancelled(),
        "subagent execution must be aborted"
    );
    let session = store.load(&session.id).expect("audit session remains");
    let cancellation_events = session
        .events
        .iter()
        .filter(|event| event.kind == "mcp_request_cancelled")
        .collect::<Vec<_>>();
    assert_eq!(cancellation_events.len(), 1, "{cancellation_events:?}");
    assert_eq!(cancellation_events[0].details["subagent_id"], id);
    assert_eq!(
        crate::tools::delegation::get_subagent_record(root.path(), &id)
            .expect("cancelled record reread")
            .status,
        "cancelled"
    );
    let cancelled_record = crate::tools::delegation::get_subagent_record(root.path(), &id)
        .expect("public cancelled record");
    assert!(cancelled_record.execution_generation.is_none());
    assert!(cancelled_record.owner_lease.is_none());
    assert!(cancelled_record.result.as_ref().is_none_or(|result| {
        result.get("ownership_reconciliation").is_none() && result.get("cleanup_proof").is_none()
    }));
    let persisted: crate::tools::delegation::SubagentRecord = serde_json::from_slice(
        &std::fs::read(
            root.path()
                .join(".nib/subagents")
                .join(format!("{id}.json")),
        )
        .expect("persisted cancelled record"),
    )
    .expect("persisted cancelled record JSON");
    assert_eq!(
        persisted.result.as_ref().and_then(|result| {
            result
                .get("ownership_reconciliation")
                .and_then(|evidence| evidence.get("cleanup_proof"))
        }),
        Some(
            &serde_json::to_value(&cleanup_proof)
                .expect("encode expected managed-process cleanup proof")
        )
    );
    assert!(
        crate::sandbox::process::ProcessScopeStore::open(root.path())
            .expect("managed-process fixture store")
            .try_load(&id)
            .expect("retired managed-process fixture lookup")
            .is_none(),
        "terminal workload proof must retire the completed process scope"
    );
    crate::sandbox::worktree::Worktree::remove(root.path(), &id)
        .expect("cancelled worktree cleanup");
}

#[tokio::test]
async fn unsafe_manager_cancellation_is_an_internal_error_and_remains_running() {
    let root = tempdir().expect("unresolved cancellation repository");
    initialize_git_repository(root.path());
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
        .expect("subagent owner lease");
    let process_scope =
        ManagedProcessFixture::start(root.path(), &id, owner_lease.execution_generation());
    crate::tools::delegation::write_subagent_record(
        root.path(),
        &crate::tools::delegation::SubagentRecord {
            id: id.clone(),
            parent_session_id: Some(session.id.clone()),
            child_session_id: id.clone(),
            prompt: "cannot cancel safely".to_string(),
            status: "running".to_string(),
            execution_generation: Some(owner_lease.execution_generation()),
            owner_lease: Some(owner_lease.lease_id().to_string()),
            worktree_path: root.path().to_path_buf(),
            branch: format!("nib/subagent/{id}"),
            branch_oid: None,
            result: None,
            error: None,
            verification: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    )
    .expect("running subagent record");
    crate::daemons::task::TASK_MANAGER
        .register_task(id.clone(), "subagent")
        .expect("unattached subagent task");
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
    let response = rpc_result(
        json!("unresolved-cancel"),
        json!({
            "isError": false,
            "structuredContent": {"subagent_id": id.clone()}
        }),
    );
    let (audit, _audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");

    finish_request_lifecycle(
        &lifecycle,
        7,
        root.path(),
        &subagent_start_class("nib_run"),
        HandledRequest {
            response: Some(response),
            cancellation_audit: Some(audit),
        },
    );

    let error_response = match &*lock_request_lifecycle(&lifecycle) {
        RequestLifecycle::CancellationFailed { response, .. } => response.clone(),
        state => panic!("unexpected lifecycle state: {state:?}"),
    };
    assert_eq!(error_response["error"]["code"], -32603);
    assert_eq!(
        crate::tools::delegation::get_subagent_record(root.path(), &id)
            .expect("running record remains readable")
            .status,
        "running"
    );
    let session = store.load(&session.id).expect("audit session remains");
    let events = session
        .events
        .iter()
        .filter(|event| event.kind == "mcp_request_cancelled")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].details["outcome"], "unresolved");
    assert_eq!(events[0].details["manager_stopped"], false);
    assert_eq!(
        crate::sandbox::process::ProcessScopeStore::open(root.path())
            .expect("managed-process fixture store")
            .cleanup_lease_state(process_scope.scope())
            .expect("live managed-process cleanup lease"),
        crate::sandbox::process::CleanupLeaseState::Live
    );
    crate::daemons::task::TASK_MANAGER
        .rollback_unattached_task(&id)
        .expect("unattached task cleanup");
    process_scope.complete("test_teardown");
    drop(owner_lease);
}

#[tokio::test]
async fn missing_successful_subagent_is_never_reported_as_cancelled() {
    let root = tempdir().expect("missing cancellation repository");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.try_create_session().expect("audit session");
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
    let missing_id = format!("sub-{}", uuid::Uuid::new_v4());
    let (audit, _audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");

    finish_request_lifecycle(
        &lifecycle,
        3,
        root.path(),
        &subagent_start_class("nib_run"),
        HandledRequest {
            response: Some(rpc_result(
                json!("missing-subagent"),
                json!({
                    "isError": false,
                    "structuredContent": {"subagent_id": missing_id}
                }),
            )),
            cancellation_audit: Some(audit),
        },
    );

    match &*lock_request_lifecycle(&lifecycle) {
        RequestLifecycle::CancellationFailed { response, .. } => {
            assert_eq!(response["error"]["code"], -32603);
        }
        state => panic!("unexpected lifecycle state: {state:?}"),
    }
    let session = store.load(&session.id).expect("audit session remains");
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "mcp_request_cancelled")
            .count(),
        1
    );
}

#[test]
fn stale_lifecycle_generation_cannot_publish() {
    let root = tempdir().expect("stale lifecycle repository");
    let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Reconciling {
        generation: 9,
    }));

    finish_request_lifecycle(
        &lifecycle,
        8,
        root.path(),
        &RequestCancellationClass::Protocol,
        HandledRequest::without_audit(Some(rpc_result(json!(8), json!({})))),
    );

    assert!(matches!(
        &*lock_request_lifecycle(&lifecycle),
        RequestLifecycle::Reconciling { generation: 9 }
    ));
}
