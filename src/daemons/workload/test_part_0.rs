use super::*;

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn scheduled_plan_summary_accepts_only_the_exact_plan_ready_contract() {
    let failed = crate::agent::AgentRunSummary {
        session_id: "scheduled-provider-failure".to_string(),
        run_id: String::new(),
        steps_taken: 1,
        last_message: None,
        tool_call_count: 0,
        final_state: crate::agent::state::AgentState::Done,
        outcome: "planning_failed: bounded provider error".to_string(),
        failure: None,
        bound_reached: false,
        trace: Vec::new(),
    };
    let legacy = classify_agent_run_outcome(Ok(failed)).unwrap_err();
    assert_eq!(legacy.outcome, "planning_failed: bounded provider error");
    assert!(legacy
        .report
        .starts_with("LLM request failed [LLM-REJECTED]"));
    assert!(legacy.report.contains("Provider: unknown (legacy)"));
    assert!(legacy
        .report
        .contains("Session: scheduled-provider-failure"));
    assert!(!legacy.report.contains("bounded provider error"));
    assert_eq!(legacy.failure, None);

    let typed_failure = crate::llm::LlmError::new(
        crate::llm::LlmErrorClass::Authentication,
        crate::llm::LlmErrorPhase::HttpResponse,
        crate::llm::RetryDisposition::NotAttempted,
        crate::llm::LlmErrorMetadata::new("openai", "responses", Some("gpt-safe"), Some(401), &[]),
        "provider-supplied detail omitted",
    );
    let typed = classify_agent_run_outcome(Ok(crate::agent::AgentRunSummary {
        session_id: "scheduled-typed-failure".to_string(),
        run_id: String::new(),
        steps_taken: 1,
        last_message: None,
        tool_call_count: 0,
        final_state: crate::agent::state::AgentState::Done,
        outcome: "planning_failed".to_string(),
        failure: Some(typed_failure.clone()),
        bound_reached: false,
        trace: Vec::new(),
    }))
    .unwrap_err();
    assert_eq!(typed.outcome, "planning_failed");
    assert!(typed.report.starts_with("LLM request failed [LLM-AUTH]"));
    assert_eq!(typed.failure, Some(typed_failure));

    let refused = crate::agent::AgentRunSummary {
        session_id: "scheduled-provider-refusal".to_string(),
        run_id: String::new(),
        steps_taken: 1,
        last_message: None,
        tool_call_count: 0,
        final_state: crate::agent::state::AgentState::Done,
        outcome: "model_refusal".to_string(),
        failure: None,
        bound_reached: false,
        trace: Vec::new(),
    };
    assert_eq!(
            classify_agent_run_outcome(Ok(refused)).unwrap_err().report,
            "Model declined the request. No further work ran. Rephrase the request or select a different model.\nSession: scheduled-provider-refusal"
        );

    let completed = crate::agent::AgentRunSummary {
        session_id: "scheduled-success".to_string(),
        run_id: String::new(),
        steps_taken: 1,
        last_message: None,
        tool_call_count: 0,
        final_state: crate::agent::state::AgentState::Done,
        outcome: "plan_ready".to_string(),
        failure: None,
        bound_reached: false,
        trace: Vec::new(),
    };
    assert_eq!(
        classify_agent_run_outcome(Ok(completed)).unwrap().outcome,
        "plan_ready"
    );

    for (outcome, final_state, bound_reached, tool_call_count) in [
        (
            "transition_limit_reached",
            crate::agent::state::AgentState::Done,
            false,
            0,
        ),
        (
            "turn_limit_reached",
            crate::agent::state::AgentState::Done,
            true,
            0,
        ),
        (
            "plan_changed_during_generation",
            crate::agent::state::AgentState::Done,
            false,
            0,
        ),
        (
            "plan_binding_changed",
            crate::agent::state::AgentState::Done,
            false,
            0,
        ),
        (
            "plan_approval_denied",
            crate::agent::state::AgentState::Done,
            false,
            0,
        ),
        (
            "empty_model_response",
            crate::agent::state::AgentState::Done,
            false,
            0,
        ),
        (
            "tool_execution_failed",
            crate::agent::state::AgentState::Done,
            false,
            1,
        ),
        (
            "waiting_for_user_input",
            crate::agent::state::AgentState::Done,
            false,
            0,
        ),
        ("completed", crate::agent::state::AgentState::Done, false, 0),
        (
            "plan_ready",
            crate::agent::state::AgentState::Planning,
            false,
            0,
        ),
        ("plan_ready", crate::agent::state::AgentState::Done, true, 0),
        (
            "plan_ready",
            crate::agent::state::AgentState::Done,
            false,
            1,
        ),
    ] {
        let unexpected = crate::agent::AgentRunSummary {
            session_id: "scheduled-unexpected-outcome".to_string(),
            run_id: String::new(),
            steps_taken: 1,
            last_message: None,
            tool_call_count,
            final_state,
            outcome: outcome.to_string(),
            failure: None,
            bound_reached,
            trace: Vec::new(),
        };
        let expected_report = unexpected
            .user_failure_report()
            .unwrap_or_else(|| outcome.to_string());
        let failed = classify_agent_run_outcome(Ok(unexpected)).unwrap_err();
        assert_eq!(failed.outcome, outcome);
        assert_eq!(failed.report, expected_report);
        assert_eq!(failed.failure, None);
    }
}

#[tokio::test]
async fn due_schedule_waits_for_active_session_before_wake_and_completes_once() {
    let (directory, store, session_store) = scheduled_agent_fixture();
    let active_run = session_store
        .try_acquire_run_lease("origin")
        .expect("hold active session run");
    let task_id = "schedule-waits-for-active-run";
    let (owner, job, stale_heartbeat) =
        prepare_owned_due_schedule(&directory, &store, &session_store, task_id);
    let worker_store = store.clone();
    let worker_task_id = task_id.to_string();
    let mut worker = tokio::spawn(async move {
        run_schedule_worker(&worker_store, &owner, &worker_task_id, job).await
    });

    wait_for_worker_heartbeat(&store, task_id, stale_heartbeat).await;
    sleep(WORKER_POLL_INTERVAL).await;
    assert!(
        !worker.is_finished(),
        "due schedule must wait for the session run"
    );
    let deferred = store.get_file(task_id).expect("deferred schedule");
    assert_eq!(deferred.record.status, "running");
    assert_eq!(deferred.active_occurrence, None);
    let session = session_store.load("origin").expect("origin session");
    assert!(!session.events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "timer_fired" | "scheduled_agent_run_completed" | "scheduled_agent_run_failed"
        )
    }));

    drop(active_run);
    timeout(Duration::from_secs(10), &mut worker)
        .await
        .expect("scheduled worker completes after session release")
        .expect("scheduled worker task")
        .expect("scheduled worker result");

    let completed = store.get_file(task_id).expect("completed schedule");
    assert_eq!(completed.record.status, "completed");
    assert_eq!(completed.record.completed_occurrences, 1);
    assert_eq!(completed.record.error, None);
    assert_eq!(completed.active_occurrence, None);
    let session = session_store.load("origin").expect("origin session");
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "timer_fired")
            .count(),
        1
    );
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "scheduled_agent_run_completed")
            .count(),
        1
    );
    assert!(!session
        .events
        .iter()
        .any(|event| event.kind == "scheduled_agent_run_failed"));
}

#[tokio::test]
async fn scheduled_provider_failure_publishes_one_redacted_durable_failure() {
    const SECRET: &str = "scheduled-provider-secret";
    let (directory, store, session_store) = scheduled_agent_fixture();
    let base_url = serve_failed_responses_once(SECRET);
    let unrelated = reqwest::get(format!("{base_url}/unrelated"))
        .await
        .expect("unrelated fixture request");
    assert_eq!(unrelated.status(), reqwest::StatusCode::NOT_FOUND);
    let mut config = crate::config::load_nib_config_full(directory.path()).expect("runtime config");
    config.llm.active_provider = Some("openai".to_string());
    config.llm.providers.clear();
    config.llm.providers.insert(
        "openai".to_string(),
        crate::config::ProviderEntry {
            model: "fixture-reasoning-model".to_string(),
            api_key: Some(SECRET.to_string()),
            base_url: Some(base_url),
            api: Some(crate::config::LlmApiMode::Responses),
            ..crate::config::ProviderEntry::default()
        },
    );
    crate::config::save_nib_config_full(directory.path(), &mut config)
        .expect("failed Responses schedule config");
    let task_id = "schedule-provider-failure";
    let (owner, job, _) = prepare_owned_due_schedule(&directory, &store, &session_store, task_id);

    timeout(
        Duration::from_secs(10),
        run_schedule_worker(&store, &owner, task_id, job),
    )
    .await
    .expect("failed schedule worker completes")
    .expect("failed schedule worker reconciles");

    let failed = store.get_file(task_id).expect("failed schedule record");
    assert_eq!(failed.record.status, "failed");
    assert_eq!(failed.record.completed_occurrences, 0);
    let error = failed.record.error.expect("failed schedule error");
    assert_eq!(error, "scheduled agent run failed");
    assert!(!error.contains(SECRET), "{error}");
    assert!(!error.contains("Responses API did not complete"), "{error}");
    let run_failure = failed
        .record
        .result
        .as_ref()
        .and_then(|result| result.get("runs"))
        .and_then(Value::as_array)
        .and_then(|runs| runs.first())
        .and_then(|run| run.get("failure"))
        .expect("typed durable failure");
    assert_eq!(run_failure["class"], "provider_rejected");
    assert_eq!(run_failure["phase"], "planning");
    assert_eq!(run_failure["retry"], "not_retryable");
    assert_eq!(run_failure["provider"], "openai");
    assert_eq!(run_failure["transport"], "responses");
    assert_eq!(run_failure["model"], "fixture-reasoning-model");
    assert_eq!(run_failure["incident_code"], "LLM-REJECTED");

    let session = session_store
        .load("origin")
        .expect("failed schedule session");
    let failure_events = session
        .events
        .iter()
        .filter(|event| event.kind == "scheduled_agent_run_failed")
        .collect::<Vec<_>>();
    assert_eq!(failure_events.len(), 1);
    assert_eq!(
        failure_events[0].details["failure"]["incident_code"],
        "LLM-REJECTED"
    );
    assert!(!session
        .events
        .iter()
        .any(|event| event.kind == "scheduled_agent_run_completed"));
    assert!(!serde_json::to_string(&session).unwrap().contains(SECRET));

    let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
        .read_all()
        .expect("failed schedule audit");
    assert!(audits.iter().any(|record| {
        record.action == "wake_agent_loop"
            && record.outcome == "failed"
            && record.detail.as_deref().is_some_and(|detail| {
                detail.contains("error=scheduled agent run failed")
                    && !detail.contains("LLM request failed")
                    && !detail.contains(SECRET)
                    && !detail.contains("Responses API did not complete")
            })
    }));
}

#[tokio::test]
#[serial_test::serial]
async fn scheduled_failure_redacts_encoded_inactive_environment_credential() {
    const SECRET: &str = "scheduled/inactive-env-secret";
    const ENCODED_SECRET: &str = "scheduled%2Finactive-env-secret";
    let _environment = EnvironmentGuard::set("ANTHROPIC_API_KEY", SECRET);
    let (directory, store, session_store) = scheduled_agent_fixture();
    let base_url = serve_failed_responses_once("provider detail is omitted");
    let mut config = crate::config::load_nib_config_full(directory.path()).expect("runtime config");
    config.llm.active_provider = Some("openai".to_string());
    config.llm.providers.clear();
    config.llm.providers.insert(
        "openai".to_string(),
        crate::config::ProviderEntry {
            model: format!("fixture-{ENCODED_SECRET}"),
            api_key: Some("active-openai-key".to_string()),
            base_url: Some(base_url),
            api: Some(crate::config::LlmApiMode::Responses),
            ..crate::config::ProviderEntry::default()
        },
    );
    crate::config::save_nib_config_full(directory.path(), &mut config)
        .expect("failed Responses schedule config");
    let task_id = "schedule-inactive-env-redaction";
    let (owner, job, _) = prepare_owned_due_schedule(&directory, &store, &session_store, task_id);

    timeout(
        Duration::from_secs(10),
        run_schedule_worker(&store, &owner, task_id, job),
    )
    .await
    .expect("failed schedule worker completes")
    .expect("failed schedule worker reconciles");

    let failed = store.get_file(task_id).expect("failed schedule record");
    let serialized_record = serde_json::to_string(&failed).expect("schedule record JSON");
    let serialized_session = serde_json::to_string(
        &session_store
            .load("origin")
            .expect("failed schedule session"),
    )
    .expect("schedule session JSON");
    assert_eq!(failed.record.status, "failed");
    assert!(serialized_record.contains("[REDACTED]"));
    for serialized in [&serialized_record, &serialized_session] {
        assert!(!serialized.contains(SECRET), "{serialized}");
        assert!(!serialized.contains(ENCODED_SECRET), "{serialized}");
    }
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn scheduled_agent_run_keeps_the_exact_non_default_profile_scope() {
    let (directory, _default_task_store, default_session_store) = scheduled_agent_fixture();
    let mut config =
        crate::config::load_nib_config_full(directory.path()).expect("load runtime config");
    config.profiles.default = "default".to_string();
    config.profiles.active = vec![
        crate::config::ProfileConfig {
            id: "default".to_string(),
            root: PathBuf::from("."),
            state_dir: Some(PathBuf::from(".nib/profiles/default")),
            ..crate::config::ProfileConfig::default()
        },
        crate::config::ProfileConfig {
            id: "alternate".to_string(),
            root: PathBuf::from("."),
            state_dir: Some(PathBuf::from(".nib/profiles/alternate")),
            ..crate::config::ProfileConfig::default()
        },
    ];
    crate::config::save_nib_config_full(directory.path(), &mut config)
        .expect("save two-profile runtime config");
    let profiles =
        ProfileRegistry::load(directory.path(), &config.profiles).expect("load profile registry");
    let alternate = profiles.get("alternate").expect("alternate profile");
    alternate.ensure_state_dirs().expect("alternate state");
    let alternate_session_store = SessionStore::at_dir(alternate.sessions_dir().to_path_buf());
    alternate_session_store.create_session_with_id("origin");
    let store = DurableTaskStore::at_daemon_dir(alternate.daemon_dir().to_path_buf())
        .expect("alternate durable task store");
    let task_id = "schedule-exact-alternate-profile";
    store
        .prepare_schedule(DurableScheduleRequest {
            id: task_id.to_string(),
            prompt: "scheduled plan".to_string(),
            project_root: directory.path().to_path_buf(),
            profile_id: "alternate".to_string(),
            sessions_dir: alternate_session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            initial_delay: Duration::from_secs(1),
            interval: Duration::from_secs(1),
            repeat_count: 1,
        })
        .expect("prepare alternate schedule");
    let owner = WorkerOwner {
        token: "schedule-exact-alternate-profile-owner".to_string(),
        pid: std::process::id(),
    };
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.record.next_run_at = Some(Utc::now() - ChronoDuration::seconds(1));
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed alternate schedule worker");
    let default_before = default_session_store
        .load("origin")
        .expect("default session before schedule");
    let _default_run = default_session_store
        .try_acquire_run_lease("origin")
        .expect("hold default profile run lease");

    timeout(
        Duration::from_secs(10),
        run_schedule_worker(
            &store,
            &owner,
            task_id,
            ScheduleWorkerJob {
                prompt: "scheduled plan".to_string(),
                project_root: directory.path().to_path_buf(),
                profile_id: "alternate".to_string(),
                sessions_dir: alternate_session_store.sessions_dir().to_path_buf(),
                session_id: "origin".to_string(),
                interval_secs: 1,
                repeat_count: 1,
            },
        ),
    )
    .await
    .expect("alternate schedule completes")
    .expect("alternate schedule result");

    assert_eq!(
        default_session_store
            .load("origin")
            .expect("default session after schedule"),
        default_before
    );
    let alternate_session = alternate_session_store
        .load("origin")
        .expect("alternate session after schedule");
    assert!(alternate_session
        .messages
        .iter()
        .any(|message| { message.role == "user" && message.content == "scheduled plan" }));
    assert!(alternate_session.plan.is_some());
    assert_eq!(
        alternate_session
            .events
            .iter()
            .filter(|event| event.kind == "timer_fired")
            .count(),
        1
    );
    assert_eq!(
        alternate_session
            .events
            .iter()
            .filter(|event| event.kind == "scheduled_agent_run_completed")
            .count(),
        1
    );
}

#[tokio::test]
async fn cancelled_due_schedule_waiting_for_active_session_never_publishes_wake() {
    let (directory, store, session_store) = scheduled_agent_fixture();
    let _active_run = session_store
        .try_acquire_run_lease("origin")
        .expect("hold active session run");
    let task_id = "cancel-schedule-waiting-for-active-run";
    let (owner, job, stale_heartbeat) =
        prepare_owned_due_schedule(&directory, &store, &session_store, task_id);
    let worker_store = store.clone();
    let worker_task_id = task_id.to_string();
    let mut worker = tokio::spawn(async move {
        run_schedule_worker(&worker_store, &owner, &worker_task_id, job).await
    });

    wait_for_worker_heartbeat(&store, task_id, stale_heartbeat).await;
    sleep(WORKER_POLL_INTERVAL).await;
    assert!(!worker.is_finished(), "due schedule must still be waiting");
    store
        .update(task_id, |task| {
            task.record.cancel_requested = true;
            task.record.status = "cancelling".to_string();
            task.record.updated_at = Utc::now();
            Ok(())
        })
        .expect("request schedule cancellation");
    timeout(Duration::from_secs(10), &mut worker)
        .await
        .expect("deferred schedule observes cancellation")
        .expect("scheduled worker task")
        .expect("scheduled worker result");

    let cancelled = store.get_file(task_id).expect("cancelled schedule");
    assert_eq!(cancelled.record.status, "cancelled");
    assert_eq!(cancelled.active_occurrence, None);
    let session = session_store.load("origin").expect("origin session");
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "timer_cancelled")
            .count(),
        1
    );
    assert!(!session.events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "timer_fired" | "scheduled_agent_run_completed" | "scheduled_agent_run_failed"
        )
    }));
}

#[test]
fn session_owned_projection_and_cancellation_fail_closed_for_foreign_work() {
    let (directory, store, session_store) = fixture();
    session_store.create_session_with_id("other");
    store
        .prepare_terminal(terminal_request(&directory, &session_store, "owned-task"))
        .expect("prepare owned task");
    let mut foreign = terminal_request(&directory, &session_store, "foreign-task");
    foreign.session_id = "other".to_string();
    store
        .prepare_terminal(foreign)
        .expect("prepare foreign task");

    let owned = store.list_for_session("origin").expect("list owned tasks");
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].id, "owned-task");
    assert_eq!(owned[0].kind, "terminal");
    assert_eq!(owned[0].status, "prepared");
    let encoded = serde_json::to_string(&owned).expect("safe projection JSON");
    assert!(!encoded.contains("printf ok"));
    assert!(!encoded.contains("worker_pid"));
    assert!(!encoded.contains("result"));

    let foreign_before = store.get("foreign-task").unwrap().unwrap();
    let rejected = store
        .cancel_for_session("foreign-task", "origin")
        .expect_err("foreign cancellation must fail closed");
    let missing = store
        .cancel_for_session("missing-task", "origin")
        .expect_err("missing cancellation must fail closed");
    assert_eq!(rejected, SESSION_SCOPED_TASK_UNAVAILABLE);
    assert_eq!(
        rejected, missing,
        "foreign IDs must not be an existence oracle"
    );
    assert!(store.cancel_for_session("../malformed", "origin").is_err());
    assert_eq!(
        store.get("foreign-task").unwrap().unwrap(),
        foreign_before,
        "foreign cancellation must not mutate the record"
    );
    let mut corrupt_foreign = terminal_request(&directory, &session_store, "corrupt-foreign-task");
    corrupt_foreign.session_id = "other".to_string();
    store
        .prepare_terminal(corrupt_foreign)
        .expect("prepare corrupt foreign task");
    std::fs::write(store.task_path("corrupt-foreign-task"), b"{not-json")
        .expect("corrupt foreign task record");
    assert_eq!(
        store
            .cancel_for_session("corrupt-foreign-task", "origin")
            .expect_err("unproven corrupt ownership must fail closed"),
        SESSION_SCOPED_TASK_UNAVAILABLE
    );

    let cancelled = store
        .cancel_for_session("owned-task", "origin")
        .expect("cancel owned task");
    assert_eq!(cancelled.status, "cancelled");
    let terminal_before = store.get("owned-task").unwrap().unwrap();
    assert!(store.cancel_for_session("owned-task", "origin").is_err());
    assert_eq!(store.get("owned-task").unwrap().unwrap(), terminal_before);
    assert!(store.list_for_session("missing session").is_err());
}

#[test]
fn session_scoped_cancellation_reconciles_a_concurrent_terminal_transition() {
    let (directory, store, session_store) = fixture();
    let id = "owned-running-race";
    store
        .prepare_terminal(terminal_request(&directory, &session_store, id))
        .expect("prepare owned task");
    store
        .update(id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(42);
            task.record.updated_at = Utc::now();
            Ok(())
        })
        .expect("mark task running");

    let cancelling_store = store.clone();
    let cancellation =
        std::thread::spawn(move || cancelling_store.cancel_for_session(id, "origin"));
    for _ in 0..100 {
        if store
            .get(id)
            .expect("poll task")
            .is_some_and(|task| task.status == "cancelling")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(store.get(id).unwrap().unwrap().status, "cancelling");
    store
        .update(id, |task| {
            task.record.status = "completed".to_string();
            task.record.worker_pid = None;
            task.record.updated_at = Utc::now();
            scrub_completed_job(&mut task.job);
            Ok(())
        })
        .expect("publish concurrent terminal state");

    let reconciled = cancellation
        .join()
        .expect("cancellation thread")
        .expect("session cancellation");
    assert_eq!(reconciled.status, "completed");
    assert_eq!(store.get(id).unwrap().unwrap().status, "completed");
}

#[cfg(any(unix, windows))]
#[test]
fn point_reads_wait_for_an_in_flight_record_replacement() {
    let (directory, store, session_store) = fixture();
    let id = "read-during-replacement";
    store
        .prepare_terminal(terminal_request(&directory, &session_store, id))
        .expect("prepare durable task");
    let lock = store.acquire_task_lock(id).expect("hold task lock");
    let path = store.task_path(id);
    let evacuated = path.with_extension("json.evacuated");
    std::fs::rename(&path, &evacuated).expect("evacuate visible task record");

    let reader = store.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        sent.send(reader.get(id)).expect("return point read");
    });
    assert!(
        received.recv_timeout(Duration::from_millis(100)).is_err(),
        "point read must wait for the task publication lock"
    );

    std::fs::rename(&evacuated, &path).expect("restore visible task record");
    drop(lock);
    let record = received
        .recv_timeout(Duration::from_secs(2))
        .expect("point read resumed")
        .expect("point read succeeds")
        .expect("task remains present");
    assert_eq!(record.id, id);
    thread.join().expect("point reader");
}

#[cfg(unix)]
#[test]
fn enumeration_waits_for_a_live_atomic_writer_without_parsing_transaction_artifacts() {
    let (directory, store, session_store) = fixture();
    let id = "list-during-replacement";
    store
        .prepare_terminal(terminal_request(&directory, &session_store, id))
        .expect("prepare durable task");
    let path = store.task_path(id);
    let encoded = fs::read(&path).expect("task record");
    let writer_store = store.clone();
    let writer_path = path.clone();
    let (evacuated, evacuation_observed) = std::sync::mpsc::channel();

    let writer = std::thread::spawn(move || {
        let expected = writer_store
            .tasks_directory
            .open_read(&writer_path)
            .expect("open expected task record");
        writer_store
            .tasks_directory
            .save_bytes_atomically_expected_with_after_evacuation_hook(
                &writer_path,
                &encoded,
                ".task-",
                crate::daemons::state::FileExpectation::Present(&expected),
                || {
                    evacuated.send(()).expect("signal target evacuation");
                    std::thread::sleep(Duration::from_millis(50));
                },
            )
            .expect("complete atomic task publication");
    });

    evacuation_observed
        .recv_timeout(Duration::from_secs(1))
        .expect("writer reached target evacuation");
    let records = store
        .list()
        .expect("enumeration waits for the live writer and resumes");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, id);
    writer.join().expect("atomic task writer");
}

#[cfg(unix)]
#[test]
fn constructor_migration_waits_for_atomic_record_evacuation() {
    let (directory, store, session_store) = fixture();
    let id = "migration-during-replacement";
    store
        .prepare_terminal(terminal_request(&directory, &session_store, id))
        .expect("prepare durable task");

    let migration_store = store.clone();
    let (enumerated, enumeration_observed) = std::sync::mpsc::sync_channel(0);
    let (continue_migration, migration_released) = std::sync::mpsc::sync_channel(0);
    let (migration_result, result_observed) = std::sync::mpsc::sync_channel(1);
    let migration = std::thread::spawn(move || {
        let result = migration_store.migrate_legacy_execution_ids_with_hook(|| {
            enumerated
                .send(())
                .map_err(|error| format!("signal task enumeration: {error}"))?;
            migration_released
                .recv()
                .map_err(|error| format!("wait to continue task migration: {error}"))?;
            Ok(())
        });
        migration_result
            .send(result)
            .expect("return migration result");
    });
    enumeration_observed
        .recv_timeout(Duration::from_secs(1))
        .expect("migration enumerated the task record");

    let writer_store = store.clone();
    let path = store.task_path(id);
    let writer_path = path.clone();
    let (evacuated, evacuation_observed) = std::sync::mpsc::sync_channel(0);
    let (publish, publication_released) = std::sync::mpsc::sync_channel(0);
    let writer = std::thread::spawn(move || {
        let _lock = writer_store
            .acquire_task_lock(id)
            .expect("hold task publication lock");
        let opened = writer_store
            .read_path_opened(&writer_path)
            .expect("open task record for publication");
        let encoded = serde_json::to_vec_pretty(&opened.task).expect("encode task record");
        writer_store
            .tasks_directory
            .save_bytes_atomically_expected_with_after_evacuation_hook(
                &writer_path,
                &encoded,
                ".task-",
                crate::daemons::state::FileExpectation::Present(&opened.file),
                || {
                    evacuated.send(()).expect("signal target evacuation");
                    publication_released
                        .recv()
                        .expect("wait to publish replacement");
                },
            )
            .expect("publish replacement task record");
    });
    evacuation_observed
        .recv_timeout(Duration::from_secs(1))
        .expect("writer evacuated the enumerated task record");
    assert!(!path.exists(), "writer must hold the target evacuated");

    continue_migration
        .send(())
        .expect("continue migration while target is evacuated");
    assert_eq!(
        result_observed.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        "migration must wait for the task publication lock before its first read"
    );

    publish.send(()).expect("release task publication");
    writer.join().expect("atomic task writer");
    result_observed
        .recv_timeout(Duration::from_secs(2))
        .expect("migration resumed after publication")
        .expect("migration succeeds after publication");
    migration.join().expect("task migration");
}

#[test]
fn durable_admission_is_store_wide_and_exact_cap_remains_readable() {
    let (directory, store, session_store) = fixture();
    let first_store = store.clone().with_record_limit(1);
    let second_store = DurableTaskStore::at_daemon_dir(store.daemon_dir())
        .expect("second store handle")
        .with_record_limit(1);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut writers = Vec::new();
    for (id, store) in [
        ("bounded-a", first_store.clone()),
        ("bounded-b", second_store),
    ] {
        let barrier = barrier.clone();
        let request = terminal_request(&directory, &session_store, id);
        writers.push(std::thread::spawn(move || {
            barrier.wait();
            store.prepare_terminal(request)
        }));
    }

    let results: Vec<_> = writers
        .into_iter()
        .map(|writer| writer.join().expect("admission writer"))
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let rejected = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one admission is rejected");
    assert!(rejected.contains("1-record limit"), "{rejected}");
    assert_eq!(first_store.list().expect("list at exact cap").len(), 1);
    assert!(first_store
        .reconcile(Utc::now())
        .expect("reconcile at exact cap")
        .is_empty());
}

#[test]
fn listing_bounds_aggregate_large_records_while_reconcile_streams_them() {
    let (directory, store, session_store) = fixture();
    let store = store
        .with_enumeration_byte_limit(3 * 1024 * 1024)
        .with_reconciliation_report_limit(1);
    for id in ["large-a", "large-b", "large-c"] {
        store
            .prepare_terminal(terminal_request(&directory, &session_store, id))
            .expect("prepare large record");
        store
            .update(id, |task| {
                task.record.result = Some(json!({"output": "x".repeat(2 * 1024 * 1024)}));
                task.record.status = "running".to_string();
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                Ok(())
            })
            .expect("persist large valid record");
        assert!(store.task_path(id).metadata().unwrap().len() < MAX_TASK_RECORD_BYTES);
        assert_eq!(
            store
                .get(id)
                .expect("read individual large record")
                .and_then(|record| record.result)
                .and_then(|result| result["output"].as_str().map(str::len)),
            Some(2 * 1024 * 1024)
        );
    }

    let error = store
        .list()
        .expect_err("materialized listing must enforce aggregate byte budget");
    assert!(error.contains("enumeration"), "{error}");
    let report = store
        .reconcile(Utc::now())
        .expect("streamed reconcile remains usable");
    assert_eq!(report.scanned_records, 3);
    assert_eq!(report.reconciled_records, 3);
    assert_eq!(report.tasks.len(), 1);
    assert_eq!(report.omitted_records, 2);
    for id in ["large-a", "large-b", "large-c"] {
        assert_eq!(store.get(id).unwrap().unwrap().status, "failed");
    }
}

#[test]
fn terminal_capacity_evicts_oldest_with_audit_and_never_evicts_active() {
    let (directory, store, session_store) = fixture();
    let store = store.with_record_limit(2);
    for (id, age) in [("terminal-old", 120), ("terminal-new", 60)] {
        store
            .prepare_terminal(terminal_request(&directory, &session_store, id))
            .expect("prepare terminal candidate");
        store
            .update(id, |task| {
                task.record.status = "failed".to_string();
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(age);
                scrub_completed_job(&mut task.job);
                Ok(())
            })
            .expect("terminalize candidate");
    }

    store
        .prepare_terminal(terminal_request(&directory, &session_store, "active-third"))
        .expect("old terminal record is evicted");
    assert!(store.get("terminal-old").unwrap().is_none());
    assert!(store.get("terminal-new").unwrap().is_some());
    assert_eq!(
        store.get("active-third").unwrap().unwrap().status,
        "prepared"
    );

    store
        .prepare_terminal(terminal_request(
            &directory,
            &session_store,
            "active-fourth",
        ))
        .expect("remaining terminal record is evicted before active work");
    assert!(store.get("terminal-new").unwrap().is_none());
    assert!(store.get("active-third").unwrap().is_some());
    assert!(store.get("active-fourth").unwrap().is_some());
    let error = store
        .prepare_terminal(terminal_request(&directory, &session_store, "active-fifth"))
        .expect_err("active records cannot be evicted");
    assert!(
        error.contains("no terminal record can be evicted"),
        "{error}"
    );

    let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
    let records = audit.read_all().expect("capacity audit");
    for id in ["terminal-old", "terminal-new"] {
        assert!(records.iter().any(|record| {
            record.action == "evict_terminal_task"
                && record.target.as_deref() == Some(id)
                && record.outcome == "planned"
        }));
        assert!(records.iter().any(|record| {
            record.action == "evict_terminal_task"
                && record.target.as_deref() == Some(id)
                && record.outcome == "evicted"
        }));
    }
}

#[test]
fn terminal_eviction_does_not_remove_record_when_audit_cannot_start() {
    let (directory, store, session_store) = fixture();
    let store = store.with_record_limit(1);
    store
        .prepare_terminal(terminal_request(
            &directory,
            &session_store,
            "terminal-retained",
        ))
        .expect("prepare terminal candidate");
    store
        .fail_prepared("terminal-retained", "done".to_string())
        .expect("terminalize candidate");
    std::fs::create_dir(store.daemon_dir().join("audit.jsonl")).expect("block audit publication");

    let error = store
        .prepare_terminal(terminal_request(
            &directory,
            &session_store,
            "replacement-rejected",
        ))
        .expect_err("unaudited eviction is rejected");
    assert!(error.contains("regular local file"), "{error}");
    assert!(store.get("terminal-retained").unwrap().is_some());
    assert!(store.get("replacement-rejected").unwrap().is_none());
}

#[test]
fn prepared_rollback_removes_record_and_frees_admission_capacity() {
    let (directory, store, session_store) = fixture();
    let store = store.with_record_limit(1);
    store
        .prepare_terminal(terminal_request(
            &directory,
            &session_store,
            "rollback-first",
        ))
        .expect("first admission");

    assert!(store
        .remove_prepared("rollback-first")
        .expect("remove prepared record"));
    assert!(store.get("rollback-first").unwrap().is_none());
    store
        .prepare_terminal(terminal_request(
            &directory,
            &session_store,
            "rollback-second",
        ))
        .expect("capacity was released");
    assert_eq!(store.list().expect("list replacement").len(), 1);
}

#[cfg(unix)]
#[test]
fn detached_worker_configuration_uses_a_process_group_and_reaper() {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_worker_process(&mut command);
    let child = command.spawn().expect("detached child");
    let pid = child.id();
    let process = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .expect("inspect child process group");
    assert!(process.status.success());
    let process_group: u32 = String::from_utf8_lossy(&process.stdout)
        .trim()
        .parse()
        .expect("numeric process group");
    assert_eq!(process_group, pid);

    hand_off_worker(worker_reaper_sender().expect("worker reaper"), child)
        .expect("reaper accepts child");
    let started = Instant::now();
    while Command::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "child {pid} was not reaped"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
#[test]
fn reaper_kills_and_removes_children_after_poll_errors() {
    let child = Command::new("sh")
        .args(["-c", "sleep 60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("long-running child");
    let pid = child.id();
    let mut children = vec![child];

    reap_worker_children_once(&mut children, |_| {
        Err(std::io::Error::other("forced poll failure"))
    });

    assert!(children.is_empty());
    assert!(!Command::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success()));
}

#[test]
fn durable_records_roundtrip_and_reconcile_stale_workers() {
    let (directory, store, session_store) = fixture();
    let record = store
        .prepare_terminal(DurableTerminalRequest {
            id: "terminal-roundtrip".to_string(),
            command: "printf ok".to_string(),
            cwd: directory.path().to_path_buf(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            execution: ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        })
        .expect("prepare task");
    assert_eq!(record.status, "prepared");
    assert_eq!(store.list().unwrap().len(), 1);

    store
        .update("terminal-roundtrip", |task| {
            task.record.status = "running".to_string();
            task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
            Ok(())
        })
        .unwrap();
    let reconciled = store.reconcile(Utc::now()).unwrap();
    assert_eq!(reconciled.reconciled_records, 1);
    assert_eq!(reconciled.tasks[0].status, "failed");
    assert!(reconciled.tasks[0]
        .error
        .as_deref()
        .unwrap()
        .contains("not replayed"));
}

#[test]
fn prepared_task_can_be_cancelled_without_a_worker() {
    let (directory, store, session_store) = fixture();
    store
        .prepare_terminal(DurableTerminalRequest {
            id: "cancel-prepared".to_string(),
            command: "sleep 60".to_string(),
            cwd: directory.path().to_path_buf(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            execution: ExecutionConfig::default(),
            timeout_secs: 70,
            max_output_bytes: 1024,
        })
        .unwrap();
    let cancelled = store.cancel("cancel-prepared").unwrap();
    assert_eq!(cancelled.status, "cancelled");
}
