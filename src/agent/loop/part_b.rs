use super::*;

#[tokio::test]
async fn worktree_preflight_failure_reconciles_without_running_proposed_tool() {
    let directory = tempdir().expect("project without Git");
    save_config(directory.path(), &mock_config()).expect("mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let goal = "safe terminal approval";
    let mut session = store.create_session_with_id("failed-worktree-preflight");
    let mut plan = pending_plan(goal, goal);
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");

    let summary = run_agent_loop(
        directory.path().to_path_buf(),
        &session.id,
        goal,
        AgentLoopConfig {
            max_steps: 4,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .expect("local preflight must reconcile");

    assert_eq!(summary.outcome, "worktree_preparation_failed");
    assert_eq!(summary.tool_call_count, 0);
    let saved = store.load(&session.id).expect("saved failure");
    assert!(saved
        .messages
        .iter()
        .all(|message| !message.content.contains("worktree_preparation_failed")));
    assert!(!saved
        .events
        .iter()
        .any(|event| matches!(event.kind.as_str(), "tool_attempted" | "tool_started")));
    assert_eq!(saved.plan.as_ref().unwrap().steps[0].status, "Blocked");
    assert!(saved.events.iter().any(|event| {
        event.kind == "local_preflight_failed" && event.details["stage"] == "managed_worktree"
    }));
    assert!(saved.events.iter().any(|event| {
        event.kind == "run_terminal" && event.details["outcome"] == "worktree_preparation_failed"
    }));
    let report = summary.user_failure_report().expect("safe stop report");
    assert!(report.contains("Worktree preparation failed"));
    assert!(!report.contains("not a git repository"));
}

#[tokio::test]
async fn final_event_is_delivered_when_full_channel_drains() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    sender
        .send(StreamEvent::Content("prior event".to_string()))
        .await
        .expect("fill stream");
    let delivery = tokio::spawn(async move {
        emit_terminal_bounded(&Some(sender), StreamEvent::End("completed".to_string())).await;
    });
    assert!(matches!(
        receiver.recv().await,
        Some(StreamEvent::Content(_))
    ));
    delivery.await.expect("delivery task");
    assert_eq!(
        receiver.recv().await,
        Some(StreamEvent::End("completed".to_string()))
    );
}

#[test]
fn continuation_admission_keeps_required_checks_and_rejects_uncertain_prior_work() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::new(directory.path());
    let mut session = store.create_session_with_id("continue-admission");
    let mut plan = pending_plan("resume verified work", "finish and verify");
    plan.steps[0].status = "Blocked".to_string();
    let mut obligation = crate::session::VerificationObligation::pending_tool(
        "required-check",
        "run the required check",
        vec!["src/".to_string()],
        "run_terminal",
        json!({"command": "true", "affected_paths": ["src/"]}),
        crate::session::VerificationExpectedOutcome::Success,
    )
    .expect("valid verification contract");
    obligation.plan_id.clone_from(&plan.id);
    obligation.step_index = Some(0);
    plan.steps[0].verification_obligations.push(obligation);
    plan.approve();
    let plan_id = plan.id.clone();
    session.plan = Some(plan);

    assert_eq!(
        continue_admission_error(&session, &plan_id, "resume verified work", "current-run"),
        None,
        "pending verification remains executable continuation work"
    );

    session.plan.as_mut().unwrap().outcome = Some("provider_continuation_interrupted".to_string());
    assert!(
        continue_admission_error(&session, &plan_id, "resume verified work", "current-run")
            .is_some_and(|error| error.contains("uncertain interrupted"))
    );
}

#[test]
fn runtime_model_override_targets_selected_provider() {
    let mut config = crate::config::NibConfig {
        llm: mock_config(),
        ..Default::default()
    };

    apply_model_override(&mut config, Some("mock"), Some("override-model"))
        .expect("model override");

    assert_eq!(config.llm.providers["mock"].model, "override-model");
    assert!(apply_model_override(&mut config, Some("missing"), Some("model")).is_err());
}

#[test]
fn dropping_a_prepared_task_batch_fails_every_unstarted_task() {
    let task_id = format!("prepared-drop-{}", uuid::Uuid::new_v4());
    crate::daemons::task::TASK_MANAGER.register_subagent(task_id.clone());
    {
        let mut prepared = PreparedTaskBatch::default();
        prepared.track(task_id.clone(), "schedule".to_string());
    }

    let task = crate::daemons::task::TASK_MANAGER
        .get_task(&task_id)
        .expect("failed prepared task");
    assert_eq!(task["status"], "failed");
    assert!(task["error"]
        .as_str()
        .is_some_and(|error| error.contains("before its tool observation was persisted")));
}

#[test]
fn dropping_a_durable_prepared_batch_persists_compensation_failure_audit() {
    let directory = tempdir().expect("tempdir");
    let sessions_dir = directory.path().join("state/sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_store = SessionStore::at_dir(sessions_dir.clone());
    session_store.create_session_with_id("origin");
    let store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        directory.path().join("state/daemons"),
    )
    .expect("durable store");
    let id = format!("batch-compensation-{}", uuid::Uuid::new_v4().simple());
    store
        .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
            id: id.clone(),
            command: "printf ok".to_string(),
            cwd: directory.path().to_path_buf(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir,
            session_id: "origin".to_string(),
            execution: crate::config::ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        })
        .expect("prepare task");
    crate::daemons::task::TASK_MANAGER
        .register_durable_task(id.clone(), store.clone())
        .expect("register durable task");
    let record_path = store.daemon_dir().join("tasks").join(format!("{id}.json"));
    std::fs::remove_file(&record_path).expect("remove record");
    std::fs::create_dir(&record_path).expect("inject compensation failure");

    {
        let mut prepared = PreparedTaskBatch::default();
        prepared.track(id.clone(), "run_terminal".to_string());
    }

    let records =
        crate::daemons::task::DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("compensation audit");
    assert!(records.iter().any(|record| {
        record.action == "prepared_task_compensation"
            && record.target.as_deref() == Some(id.as_str())
            && record.outcome == "compensation_failed"
    }));
}

#[tokio::test]
async fn explicit_compaction_uses_exact_run_identity_without_synthetic_messages() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "retain this compact fact")
        .unwrap();
    store
        .try_append_message(&session.id, "assistant", "retained answer")
        .unwrap();
    let before = store.load(&session.id).unwrap().messages;
    let run_id = "cdef0123456789abcdef0123456789ab";
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(16);

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "explicit context compression",
        AgentLoopConfig {
            mode: "compact".to_string(),
            run_id: Some(run_id.to_string()),
            stream_tx: Some(stream_tx),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome, "context_compacted");
    assert_eq!(summary.run_id, run_id);
    assert_eq!(summary.last_message, None);
    let after = store.load(&session.id).unwrap();
    assert_eq!(after.messages, before);
    assert!(after.summary.is_some());
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.kind == "run_started" && event.details["run_id"] == run_id)
            .count(),
        1
    );
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.kind == "run_terminal" && event.details["run_id"] == run_id)
            .count(),
        1
    );
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.kind == "compression")
            .count(),
        1
    );
    let events = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::Compression { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::End(reason) if reason == "context_compacted")));
}

#[tokio::test]
async fn exact_run_steering_is_rejected_for_explicit_compaction() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    let run_id = "bcdef0123456789abcdef0123456789a";
    let (steering, receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("create uninstalled steering channel");

    let error = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "",
        AgentLoopConfig {
            mode: "compact".to_string(),
            run_id: Some(run_id.to_string()),
            steering: Some(receiver),
            ..Default::default()
        },
    )
    .await
    .expect_err("compact mode must reject exact-run steering");

    assert!(error.contains("not supported for agent mode: compact"));
    assert!(steering
        .submit("must never be accepted by maintenance")
        .expect_err("uninstalled compact steering")
        .contains("not installed"));
    let persisted = store.load(&session.id).expect("terminal compact rejection");
    assert!(persisted.events.iter().any(|event| {
        event.kind == "run_terminal"
            && event.details["run_id"] == run_id
            && event.details["outcome"] == "local_error"
    }));
    assert!(!persisted.events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "steering_channel_bound" | "steering_admission" | "steering_input" | "compression"
        )
    }));
}

#[tokio::test]
async fn committed_compaction_cannot_be_reclassified_by_blocked_presentation_cancellation() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "retain committed compact fact")
        .unwrap();
    store
        .try_append_message(&session.id, "assistant", "retain committed answer")
        .unwrap();
    let cancellation = CancellationSignal::new();
    let run_id = "def0123456789abcdef0123456789abc";
    // The initial Compression state fills this channel. No receiver is drained,
    // reproducing the presentation backpressure that formerly opened a window
    // between summary commit and operation terminalization.
    let (stream_tx, _stream_rx) = tokio::sync::mpsc::channel(1);
    let project_root = dir.path().to_path_buf();
    let session_id = session.id.clone();
    let run_cancellation = cancellation.clone();
    let handle = tokio::spawn(async move {
        run_agent_loop(
            project_root,
            &session_id,
            "",
            AgentLoopConfig {
                mode: "compact".to_string(),
                run_id: Some(run_id.to_string()),
                stream_tx: Some(stream_tx),
                cancellation: Some(run_cancellation),
                ..Default::default()
            },
        )
        .await
    });

    loop {
        let persisted = store.load(&session.id).unwrap();
        if persisted
            .events
            .iter()
            .any(|event| event.kind == "compression")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    cancellation.cancel();
    let summary = handle.await.unwrap().unwrap();
    assert_eq!(summary.outcome, "context_compacted");
    let persisted = store.load(&session.id).unwrap();
    assert!(persisted.events.iter().any(|event| {
        event.kind == "compression_request_terminal"
            && event.details["run_id"] == run_id
            && event.details["outcome"] == "context_compacted"
    }));
    assert!(!persisted.events.iter().any(|event| {
        event.kind == "run_terminal"
            && event.details["run_id"] == run_id
            && event.details["outcome"] == "cancelled_by_user"
    }));
}

#[tokio::test]
async fn interactive_profile_binding_prevents_compaction_drift_after_default_changes() {
    let dir = tempdir().unwrap();
    let mut config = crate::config::NibConfig {
        llm: mock_config(),
        ..Default::default()
    };
    config.profiles.default = "profile-a".to_string();
    config.profiles.active = vec![
        crate::config::ProfileConfig {
            id: "profile-a".to_string(),
            root: PathBuf::from("."),
            state_dir: Some(PathBuf::from(".nib/profiles/profile-a")),
            ..Default::default()
        },
        crate::config::ProfileConfig {
            id: "profile-b".to_string(),
            root: PathBuf::from("."),
            state_dir: Some(PathBuf::from(".nib/profiles/profile-b")),
            ..Default::default()
        },
    ];
    crate::config::save_nib_config_full(dir.path(), &mut config).unwrap();
    let profile_a = crate::interactive::resolve_interactive_profile_scope(dir.path()).unwrap();
    let profile_a_id = profile_a.profile_id().to_string();
    let store_a = profile_a.into_session_store();
    let session_id = "coincident-compaction-session";
    store_a.create_session_with_id(session_id);
    store_a
        .try_append_message(session_id, "user", "profile A private context")
        .unwrap();
    store_a
        .try_append_message(session_id, "assistant", "profile A private answer")
        .unwrap();

    config.profiles.default = "profile-b".to_string();
    crate::config::save_nib_config_full(dir.path(), &mut config).unwrap();
    let store_b = SessionStore::for_project(dir.path()).unwrap();
    store_b.create_session_with_id(session_id);
    let run_id = "ef0123456789abcdef0123456789abcd";

    let summary = run_agent_loop_for_profile(
        dir.path().to_path_buf(),
        &profile_a_id,
        store_a.sessions_dir(),
        session_id,
        "",
        AgentLoopConfig {
            mode: "compact".to_string(),
            run_id: Some(run_id.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome, "context_compacted");
    assert!(store_a.load(session_id).unwrap().summary.is_some());
    let untouched = store_b.load(session_id).unwrap();
    assert!(untouched.messages.is_empty());
    assert!(untouched.summary.is_none());
    assert!(!untouched
        .events
        .iter()
        .any(|event| event.details["run_id"] == run_id));
}

#[tokio::test]
async fn explicit_compaction_does_not_recover_or_render_an_interrupted_chat_turn() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "inspect safely")
        .unwrap();
    store
        .try_append_message(&session.id, "assistant", "normalized tool intent")
        .unwrap();
    record_provider_continuation_lifecycle(
        &store,
        &session.id,
        "provider_continuation_opened",
        "prior-run",
    )
    .unwrap();
    store
        .try_append_message(
            &session.id,
            "tool",
            &json!({"observations": [{"tool": "read_file", "success": true}]}).to_string(),
        )
        .unwrap();
    let before = store.load(&session.id).unwrap();

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "",
        AgentLoopConfig {
            mode: "compact".to_string(),
            run_id: Some("1234567890abcdef1234567890abcdef".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(matches!(
        summary.outcome.as_str(),
        "context_compacted" | "context_unchanged"
    ));
    let after = store.load(&session.id).unwrap();
    assert_eq!(after.messages, before.messages);
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.kind == "provider_continuation_interrupted")
            .count(),
        0
    );
}

#[tokio::test]
async fn pre_cancelled_explicit_compaction_reconciles_without_provider_or_chat_mutation() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "must remain raw")
        .unwrap();
    let before = store.load(&session.id).unwrap().messages;
    let cancellation = CancellationSignal::new();
    cancellation.cancel();
    let run_id = "234567890abcdef1234567890abcdef1";

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "",
        AgentLoopConfig {
            mode: "compact".to_string(),
            run_id: Some(run_id.to_string()),
            cancellation: Some(cancellation),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome, "cancelled_by_user");
    let after = store.load(&session.id).unwrap();
    assert_eq!(after.messages, before);
    assert!(after.summary.is_none());
    assert!(!after
        .events
        .iter()
        .any(|event| { matches!(event.kind.as_str(), "compression" | "compression_requested") }));
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.kind == "run_terminal"
                && event.details["run_id"] == run_id
                && event.details["outcome"] == "cancelled_by_user")
            .count(),
        1
    );
}

#[tokio::test]
async fn explicit_compaction_configuration_failure_is_safe_and_terminal() {
    let dir = tempdir().unwrap();
    let config = LlmConfig {
        active_provider: Some("meta".to_string()),
        providers: HashMap::from([(
            "meta".to_string(),
            ProviderEntry {
                model: "meta-test-model".to_string(),
                api_key: Some("private-test-key".to_string()),
                ..ProviderEntry::default()
            },
        )]),
        ..Default::default()
    };
    save_config(dir.path(), &config).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    store
        .try_append_message(
            &session.id,
            "user",
            "compress without a configured endpoint",
        )
        .unwrap();
    let before = store.load(&session.id).unwrap().messages;
    let run_id = "34567890abcdef1234567890abcdef12";

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "",
        AgentLoopConfig {
            mode: "compact".to_string(),
            run_id: Some(run_id.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome, "configuration_failed");
    let report = summary.user_failure_report().expect("safe failure report");
    assert!(!report.contains("private-test-key"));
    assert!(summary.failure.is_some());
    let after = store.load(&session.id).unwrap();
    assert_eq!(after.messages, before);
    assert!(after.summary.is_none());
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.kind == "run_terminal"
                && event.details["run_id"] == run_id
                && event.details["outcome"] == "configuration_failed")
            .count(),
        1
    );
}

#[tokio::test]
async fn explicit_compaction_provider_rejection_is_safe_and_terminal() {
    let dir = tempdir().unwrap();
    let remote_secret = "private-explicit-compression-provider-detail";
    let private_key = "private-explicit-compression-key";
    let (base_url, request_rx) = crate::llm::test_support::serve_once(
        "400 Bad Request",
        "application/json",
        json!({"error": {"message": remote_secret}}).to_string(),
    );
    let config = LlmConfig {
        active_provider: Some("openai".to_string()),
        providers: HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                model: "fixture-model".to_string(),
                api_key: Some(private_key.to_string()),
                base_url: Some(base_url),
                api: Some(crate::config::LlmApiMode::ChatCompletions),
                ..ProviderEntry::default()
            },
        )]),
        ..Default::default()
    };
    save_config(dir.path(), &config).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "compress against the fixture")
        .unwrap();
    store
        .try_append_message(&session.id, "assistant", "raw answer remains")
        .unwrap();
    let before = store.load(&session.id).unwrap().messages;
    let run_id = "4567890abcdef1234567890abcdef123";

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "",
        AgentLoopConfig {
            mode: "compact".to_string(),
            run_id: Some(run_id.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let request = request_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("provider request");
    assert!(request.starts_with("POST /chat/completions "));
    assert_eq!(summary.outcome, "compression_failed");
    let report = summary.user_failure_report().expect("safe provider report");
    assert!(!report.contains(remote_secret));
    assert!(!report.contains(private_key));
    let after = store.load(&session.id).unwrap();
    assert_eq!(after.messages, before);
    assert!(after.summary.is_none());
    let persisted = serde_json::to_string(&after).unwrap();
    assert!(!persisted.contains(remote_secret));
    assert!(!persisted.contains(private_key));
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.kind == "run_terminal"
                && event.details["run_id"] == run_id
                && event.details["outcome"] == "compression_failed")
            .count(),
        1
    );
}

#[tokio::test]
async fn exact_run_identity_has_audited_mock_lifecycle_and_replay_fails_closed() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "explore the project",
        AgentLoopConfig {
            max_steps: 6,
            auto_approve: true,
            run_id: Some("0123456789abcdef0123456789abcdef".to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.final_state, AgentState::Done);
    assert_eq!(summary.run_id, "0123456789abcdef0123456789abcdef");
    assert_eq!(summary.outcome, "completed");
    assert!(summary.tool_call_count >= 1);
    assert!(summary.trace.contains(&"plan_approval".to_string()));
    assert!(summary.trace.contains(&"tool_execute".to_string()));
    let loaded = store.load(&session.id).unwrap();
    assert_eq!(
        loaded
            .events
            .iter()
            .filter(|event| event.kind == "run_started")
            .count(),
        1
    );
    for kind in [
        "run_started",
        "provider_continuation_opened",
        "provider_continuation_closed",
        "run_terminal",
    ] {
        assert!(loaded
            .events
            .iter()
            .filter(|event| event.kind == kind)
            .all(|event| event.details["run_id"] == "0123456789abcdef0123456789abcdef"));
    }
    assert_eq!(
        loaded
            .events
            .iter()
            .filter(|event| event.kind == "run_terminal")
            .count(),
        1
    );
    loaded.validate_message_sequence().unwrap();
    assert!(loaded
        .events
        .iter()
        .filter(|event| matches!(event.kind.as_str(), "approval_required" | "tool_started"))
        .all(|event| event.details.get("arguments").is_none()));
    assert!(loaded.plan.unwrap().is_complete());

    let before_replay = store.load(&session.id).unwrap();
    let replay = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "must not execute",
        AgentLoopConfig {
            auto_approve: true,
            run_id: Some("0123456789abcdef0123456789abcdef".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect_err("replayed run ID must fail closed");
    assert!(replay.contains("duplicate or replayed run_id"));
    let after_replay = store.load(&session.id).unwrap();
    assert_eq!(before_replay.messages, after_replay.messages);
    assert_eq!(before_replay.tool_calls, after_replay.tool_calls);
    assert_eq!(before_replay.events, after_replay.events);
}

#[tokio::test]
async fn exact_run_identity_pre_cancel_records_one_matching_terminal() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    let cancellation = CancellationSignal::new();
    assert!(cancellation.cancel());
    let run_id = "fedcba9876543210fedcba9876543210";

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "must not reach the provider",
        AgentLoopConfig {
            cancellation: Some(cancellation),
            run_id: Some(run_id.to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("pre-cancelled run reconciles");

    assert_eq!(summary.run_id, run_id);
    assert_eq!(summary.outcome, "cancelled_by_user");
    let persisted = store.load(&session.id).expect("cancelled session");
    assert!(persisted.messages.is_empty());
    let starts = persisted
        .events
        .iter()
        .filter(|event| event.kind == "run_started")
        .collect::<Vec<_>>();
    let terminals = persisted
        .events
        .iter()
        .filter(|event| event.kind == "run_terminal")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1);
    assert_eq!(terminals.len(), 1);
    assert_eq!(starts[0].details["run_id"], run_id);
    assert_eq!(terminals[0].details["run_id"], run_id);
    assert_eq!(terminals[0].details["outcome"], "cancelled_by_user");
    assert!(starts[0].index < terminals[0].index);
}

#[tokio::test]
async fn exact_run_steering_binding_failure_terminalizes_the_admitted_run_once() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let target = store.create_session_with_id("steering-binding-target");
    let foreign = store.create_session_with_id("steering-binding-foreign");
    let run_id = "fedcba9876543210fedcba9876543210";
    let (_handle, foreign_receiver) =
        exact_run_steering_channel(store.clone(), foreign.id.clone(), run_id, "plain")
            .expect("foreign steering receiver");

    let error = run_agent_loop(
        dir.path().to_path_buf(),
        &target.id,
        "must not reach provider execution",
        AgentLoopConfig {
            run_id: Some(run_id.to_string()),
            steering: Some(foreign_receiver),
            ..Default::default()
        },
    )
    .await
    .expect_err("wrong-session receiver must fail the admitted run");
    assert!(error.contains("not bound to the exact active run"));

    let persisted = store.load(&target.id).expect("terminalized target session");
    assert_eq!(
        persisted
            .events
            .iter()
            .filter(|event| { event.kind == "run_started" && event.details["run_id"] == run_id })
            .count(),
        1
    );
    assert_eq!(
        persisted
            .events
            .iter()
            .filter(|event| {
                event.kind == "run_terminal"
                    && event.details["run_id"] == run_id
                    && event.details["outcome"] == "local_error"
            })
            .count(),
        1
    );
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "steering_channel_bound"));
    assert!(persisted.messages.is_empty());
    assert!(persisted.tool_calls.is_empty());
}

#[tokio::test]
async fn pre_cancelled_restart_reconciles_open_provider_continuation_before_cancellation() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "inspect safely")
        .unwrap();
    store
        .try_append_message(&session.id, "assistant", "normalized tool intent")
        .unwrap();
    record_provider_continuation_lifecycle(
        &store,
        &session.id,
        "provider_continuation_opened",
        "prior-run",
    )
    .unwrap();
    store
        .record_event(
            &session.id,
            "tool_completed",
            json!({"tool_name": "read_file", "success": true}),
        )
        .unwrap();
    store
        .try_append_message(
            &session.id,
            "tool",
            &json!({"observations": [{"tool": "read_file", "success": true}]}).to_string(),
        )
        .unwrap();
    let cancellation = CancellationSignal::new();
    assert!(cancellation.cancel());

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "must not reach the provider",
        AgentLoopConfig {
            cancellation: Some(cancellation),
            run_id: Some("abcdef0123456789abcdef0123456789".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("pre-cancelled restart reconciles");

    assert_eq!(summary.outcome, "cancelled_by_user");
    let persisted = store.load(&session.id).expect("reconciled session");
    persisted.validate_message_sequence().unwrap();
    let boundary = persisted.messages.last().expect("continuation boundary");
    assert_eq!(boundary.role, "assistant");
    assert_eq!(
        serde_json::from_str::<Value>(&boundary.content).unwrap(),
        json!({
            "type": "provider_continuation_boundary",
            "outcome": "provider_continuation_interrupted",
        })
    );
    let interrupted = persisted
        .events
        .iter()
        .find(|event| event.kind == "provider_continuation_interrupted")
        .expect("interrupted continuation event");
    let cancel_requested = persisted
        .events
        .iter()
        .find(|event| event.kind == "cancel_requested")
        .expect("cancel request event");
    assert!(interrupted.index < cancel_requested.index);
    assert_eq!(
        persisted
            .events
            .iter()
            .filter(|event| event.kind == "provider_continuation_interrupted")
            .count(),
        1
    );
}

#[tokio::test]
async fn continuation_recovery_failure_terminalizes_the_admitted_run_once() {
    fn fail_recovery(_store: &SessionStore, _session_id: &str) -> Result<bool, String> {
        Err("injected continuation recovery persistence failure".to_string())
    }

    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    let runtime = prepare_agent_loop_runtime(dir.path(), None, None, None, None).unwrap();
    let lease = runtime
        .session_store
        .try_acquire_run_lease(&session.id)
        .unwrap();
    let cancellation = CancellationSignal::new();
    assert!(cancellation.cancel());
    let run_id = "0123abcdef4567890123abcdef456789";

    let error = run_agent_loop_with_runtime_and_recovery(
        runtime,
        &session.id,
        "must not reach cancellation or provider execution",
        AgentLoopConfig {
            cancellation: Some(cancellation),
            run_id: Some(run_id.to_string()),
            ..Default::default()
        },
        lease,
        fail_recovery,
    )
    .await
    .expect_err("injected recovery failure must fail the run");
    assert!(error.contains("injected continuation recovery persistence failure"));

    let persisted = store.load(&session.id).expect("terminalized session");
    let starts = persisted
        .events
        .iter()
        .filter(|event| event.kind == "run_started" && event.details["run_id"] == run_id)
        .count();
    let terminals = persisted
        .events
        .iter()
        .filter(|event| {
            event.kind == "run_terminal"
                && event.details["run_id"] == run_id
                && event.details["outcome"] == "local_error"
        })
        .count();
    assert_eq!(starts, 1);
    assert_eq!(terminals, 1);
    assert!(persisted.messages.is_empty());
    assert!(persisted.tool_calls.is_empty());
    assert!(persisted.events.iter().all(|event| {
        event.kind != "cancel_requested" && !event.kind.starts_with("provider_continuation_")
    }));
}

#[tokio::test]
async fn llm_configuration_failure_reconciles_as_typed_non_network_evidence() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(16);

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "inspect configuration",
        AgentLoopConfig {
            provider: Some("missing-provider".to_string()),
            stream_tx: Some(stream_tx),
            ..Default::default()
        },
    )
    .await
    .expect("configuration failures reconcile safely");

    assert_eq!(summary.outcome, "configuration_failed");
    let failure = summary
        .failure
        .as_ref()
        .expect("typed configuration failure");
    assert_eq!(failure.class, LlmErrorClass::Configuration);
    assert_eq!(failure.phase, LlmErrorPhase::Configuration);
    assert_eq!(failure.retry, crate::llm::RetryDisposition::NotAttempted);
    assert_eq!(failure.provider, "missing-provider");
    assert!(summary
        .user_failure_report()
        .expect("configuration report")
        .contains("LLM-CONFIG"));

    let persisted = store.load(&session.id).expect("reconciled session");
    assert!(persisted.messages.is_empty());
    let reconciliation = persisted
        .events
        .iter()
        .find(|event| event.kind == "reconciliation")
        .expect("reconciliation event");
    assert_eq!(reconciliation.details["outcome"], "configuration_failed");
    assert_eq!(reconciliation.details["failure"]["class"], "configuration");

    let events = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Failure {
            failure,
            session_id: Some(id),
        } if failure.class == LlmErrorClass::Configuration && id == &session.id
    )));
}

#[tokio::test]
async fn same_goal_incomplete_plan_resumes_without_replanning_or_reapproval() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let mut session = store.create_session_with_id("resume-same-goal");
    let mut plan = pending_plan("explore the project", "explore the project");
    plan.approve();
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved resumable plan");

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "  explore\n the project ",
        AgentLoopConfig {
            max_steps: 4,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .expect("same-goal resume");

    assert_eq!(summary.outcome, "completed");
    assert!(!summary.trace.contains(&"planning".to_string()));
    assert!(!summary.trace.contains(&"plan_approval".to_string()));
    let persisted = store.load(&session.id).expect("resumed session");
    assert_eq!(persisted.plan.as_ref().unwrap().id, plan_id);
    assert!(persisted.plan.as_ref().unwrap().is_complete());
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "plan_invalidated"));
}

#[tokio::test]
#[serial_test::serial]
async fn exact_run_steering_during_a_provider_response_suppresses_its_uncommitted_tool_proposal() {
    let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
    let directory = tempdir().expect("project");
    save_config(directory.path(), &mock_config()).expect("mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let goal = "exact run steering response smoke";
    let mut session = store.create_session_with_id("steering-response-suppression");
    let mut plan = pending_plan(goal, "do not execute the obsolete proposal");
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");
    let run_id = "0123456789abcdef0123456789abcdef";
    let (steering, steering_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("steering channel");
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);
    let project_root = directory.path().to_path_buf();
    let session_id = session.id.clone();
    let run = tokio::spawn(async move {
        run_agent_loop(
            project_root,
            &session_id,
            goal,
            AgentLoopConfig {
                max_steps: 5,
                auto_approve: true,
                run_id: Some(run_id.to_string()),
                steering: Some(steering_receiver),
                stream_tx: Some(stream_tx),
                ..Default::default()
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    stream_rx.recv().await,
                    Some(StreamEvent::StateTransition { state }) if state == AgentState::InspectLlm.as_str()
                ) {
                    break;
                }
            }
        })
        .await
        .expect("run reached the provider request");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        steering
            .submit("replacement steering marker; answer without tools")
            .expect("durable steering"),
        1
    );

    let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("steered run completed")
        .expect("agent task joined")
        .expect("agent run succeeded");
    assert_eq!(summary.outcome, "completed");
    assert_eq!(summary.tool_call_count, 0);
    assert_eq!(
        summary.last_message.as_deref(),
        Some("Final answer: replacement steering marker observed.")
    );

    let persisted = store.load(&session.id).expect("steered session");
    assert!(persisted
        .tool_calls
        .iter()
        .all(|record| record.tool_name.as_deref() != Some("list_directory")));
    assert!(!persisted.events.iter().any(|event| matches!(
        event.kind.as_str(),
        "tool_requested" | "tool_started" | "tool_completed"
    )));
    assert!(persisted.events.iter().any(|event| {
        event.kind == "provider_continuation_abandoned_by_steering"
            && event.details["run_id"] == run_id
    }));
    let input_index = persisted
        .events
        .iter()
        .position(|event| event.kind == "steering_input")
        .expect("persisted steering input");
    let intake_index = persisted
        .events
        .iter()
        .position(|event| event.kind == "steering_intake")
        .expect("persisted steering intake");
    assert!(input_index < intake_index);
}

#[tokio::test]
#[serial_test::serial]
async fn exact_run_steering_rejects_during_a_final_provider_response_without_replacement_budget() {
    let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
    let directory = tempdir().expect("project");
    save_config(directory.path(), &mock_config()).expect("mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let goal = "exact run steering response smoke final response";
    let mut session = store.create_session_with_id("steering-final-response");
    let mut plan = pending_plan(goal, "complete the only provider turn");
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");
    let run_id = "11111111111111111111111111111111";
    let (steering, steering_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("steering channel");
    let project_root = directory.path().to_path_buf();
    let session_id = session.id.clone();
    let run = tokio::spawn(async move {
        run_agent_loop(
            project_root,
            &session_id,
            goal,
            AgentLoopConfig {
                max_steps: 1,
                auto_approve: true,
                run_id: Some(run_id.to_string()),
                steering: Some(steering_receiver),
                ..Default::default()
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store
                .load(&session.id)
                .expect("request state")
                .events
                .iter()
                .any(|event| {
                    event.kind == "steering_admission"
                        && event.details["phase"] == "final_provider_request"
                        && event.details["open"] == false
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("final provider request closes steering admission");
    assert!(steering
        .submit("cannot promise a replacement request")
        .expect_err("final response steering must fail synchronously")
        .contains("current run boundary"));

    tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("final provider run completed")
        .expect("agent task joined")
        .expect("agent run succeeded");
    let persisted = store.load(&session.id).expect("terminal session");
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "steering_input"));
}

#[tokio::test]
#[serial_test::serial]
async fn exact_run_steering_rejects_during_final_planning_without_replacement_budget() {
    let _interactive_smoke = EnvironmentGuard::set("NIB_ENABLE_INTERACTIVE_SMOKE", "1");
    let directory = tempdir().expect("project");
    save_config(directory.path(), &mock_config()).expect("mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let session = store.create_session_with_id("steering-final-planning");
    let goal = "interactive queue smoke final planning turn";
    let run_id = "22222222222222222222222222222222";
    let (steering, steering_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
            .expect("steering channel");
    let project_root = directory.path().to_path_buf();
    let session_id = session.id.clone();
    let run = tokio::spawn(async move {
        run_agent_loop(
            project_root,
            &session_id,
            goal,
            AgentLoopConfig {
                max_steps: 1,
                approval_handler: Some(Arc::new(DenyApproval)),
                run_id: Some(run_id.to_string()),
                steering: Some(steering_receiver),
                ..Default::default()
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store
                .load(&session.id)
                .expect("planning state")
                .events
                .iter()
                .any(|event| {
                    event.kind == "steering_admission"
                        && event.details["phase"] == "final_planning_request"
                        && event.details["open"] == false
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("final planning request closes steering admission");
    assert!(steering
        .submit("cannot promise replanning")
        .expect_err("final planning steering must fail synchronously")
        .contains("current run boundary"));

    tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("final planning run completed")
        .expect("agent task joined")
        .expect("agent run succeeded");
    let persisted = store.load(&session.id).expect("terminal session");
    assert_eq!(
        persisted
            .events
            .iter()
            .filter(|event| event.kind == "plan_generated")
            .count(),
        1
    );
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "steering_input"));
}

#[tokio::test]
#[serial_test::serial]
async fn exact_run_steering_stays_closed_when_the_final_provider_turn_starts_a_tool() {
    // Readiness includes durable session I/O on hosted Windows runners.
    const HOSTED_PROGRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
    let directory = tempdir().expect("project");
    initialize_git_repository(directory.path());
    save_config(directory.path(), &mock_config()).expect("mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let goal = "exact run steering tool smoke final turn";
    let mut session = store.create_session_with_id("steering-final-tool");
    let mut plan = pending_plan(goal, "execute the final-turn tool proposal");
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");
    let run_id = "33333333333333333333333333333333";
    let (steering, steering_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("steering channel");
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);
    let project_root = directory.path().to_path_buf();
    let session_id = session.id.clone();
    let run = tokio::spawn(async move {
        run_agent_loop(
            project_root,
            &session_id,
            goal,
            AgentLoopConfig {
                max_steps: 1,
                auto_approve: true,
                run_id: Some(run_id.to_string()),
                steering: Some(steering_receiver),
                stream_tx: Some(stream_tx),
                ..Default::default()
            },
        )
        .await
    });

    tokio::time::timeout(HOSTED_PROGRESS_TIMEOUT, async {
        loop {
            if matches!(
                stream_rx.recv().await.expect("stream closed before final-turn tool started"),
                StreamEvent::ToolStarted { tool_name, .. } if tool_name == "run_terminal"
            ) {
                break;
            }
        }
    })
    .await
    .expect("final-turn tool started");
    assert!(steering
        .submit("cannot be applied after the final-turn tool")
        .expect_err("post-tool steering needs replacement budget")
        .contains("current run boundary"));

    tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("final tool run completed")
        .expect("agent task joined")
        .expect("agent run succeeded");
    let persisted = store.load(&session.id).expect("terminal session");
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "steering_input"));
}
