use super::*;

#[cfg(unix)]
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn terminal_worker_rejects_transient_config_with_reduced_redaction_set() {
    let directory = tempdir().expect("tempdir");
    let project_root = directory.path();
    let secret = "canonical-worker-secret-value";
    let mut canonical = crate::config::NibConfig::default();
    canonical.llm.providers.insert(
        "credential-source".to_string(),
        crate::config::ProviderEntry {
            model: "model".to_string(),
            api_key: Some(secret.to_string()),
            ..crate::config::ProviderEntry::default()
        },
    );
    canonical.profiles.default = "default".to_string();
    canonical.profiles.active = vec![crate::config::ProfileConfig {
        id: "default".to_string(),
        root: PathBuf::from("."),
        state_dir: Some(PathBuf::from("state")),
        ..crate::config::ProfileConfig::default()
    }];
    crate::config::save_nib_config_full(project_root, &mut canonical)
        .expect("canonical worker config");

    let sessions_dir = project_root.join("state/sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions directory");
    let session_store = SessionStore::at_dir(sessions_dir.clone());
    session_store.create_session_with_id("origin");
    let store =
        DurableTaskStore::at_daemon_dir(project_root.join("state/daemons")).expect("durable store");
    let task_id = "transient-config-redaction";
    let prepared = store
        .prepare_terminal(DurableTerminalRequest {
            id: task_id.to_string(),
            command: format!("printf %s {secret}"),
            cwd: project_root.to_path_buf(),
            project_root: project_root.to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: sessions_dir.clone(),
            session_id: "origin".to_string(),
            execution: ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        })
        .expect("prepare terminal worker");
    session_store
        .record_tool_call(ToolCallRecord {
            id: Some("call-transient-config-redaction".to_string()),
            session_id: Some("origin".to_string()),
            tool_name: Some("terminal".to_string()),
            result: Some(json!({
                "success": true,
                "output": {
                    "task_id": task_id,
                    "execution_id": prepared.execution_id,
                    "status": "started",
                },
            })),
            ..ToolCallRecord::default()
        })
        .expect("originating tool call");
    let owner = WorkerOwner {
        token: "transient-config-owner".to_string(),
        pid: std::process::id(),
    };
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("bind worker owner");

    let paths = crate::config::config_paths(project_root);
    let displaced = paths.nib_dir.join("config.toml.canonical");
    std::fs::rename(&paths.toml, &displaced).expect("displace canonical config");
    let mut forged = canonical.clone();
    forged
        .llm
        .providers
        .get_mut("credential-source")
        .expect("credential provider")
        .api_key = None;
    std::fs::write(
        &paths.toml,
        toml::to_string_pretty(&forged).expect("forged config"),
    )
    .expect("publish forged config");
    let restore_path = paths.toml.clone();
    let restore_displaced = displaced.clone();
    let _hook = crate::config::install_config_read_hook(paths.toml.clone(), move |_| {
        std::fs::remove_file(&restore_path).map_err(|error| error.to_string())?;
        std::fs::rename(&restore_displaced, &restore_path).map_err(|error| error.to_string())
    });
    let worker_job = || TerminalWorkerJob {
        command: format!("printf %s {secret}"),
        cwd: project_root.to_path_buf(),
        project_root: project_root.to_path_buf(),
        profile_id: "default".to_string(),
        sessions_dir: sessions_dir.clone(),
        session_id: "origin".to_string(),
        execution: ExecutionConfig::default(),
        timeout_secs: 10,
        max_output_bytes: 1024,
    };

    let error = run_terminal_worker(&store, &owner, task_id, worker_job())
        .await
        .expect_err("transient config must fail before terminal execution");
    assert!(error.contains("identity changed"), "{error}");
    let still_running = store.get(task_id).unwrap().expect("running record");
    assert_eq!(still_running.status, "running");
    assert!(still_running.result.is_none());

    run_terminal_worker(&store, &owner, task_id, worker_job())
        .await
        .expect("canonical worker run");
    let completed = store.get(task_id).unwrap().expect("completed record");
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.result.as_ref().unwrap()["stdout"], "[REDACTED]");
    assert!(!serde_json::to_string(&completed).unwrap().contains(secret));
    assert!(
        !serde_json::to_string(&session_store.load("origin").unwrap())
            .unwrap()
            .contains(secret)
    );
}

#[tokio::test]
async fn long_running_worker_heartbeats_before_concurrent_reconcile() {
    let (directory, store, session_store) = fixture();
    let task_id = "heartbeat-schedule";
    prepare_schedule_fixture(&directory, &store, &session_store, task_id);
    let owner = WorkerOwner {
        token: "heartbeat-lease".to_string(),
        pid: 42_001,
    };
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed stale running lease");

    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    let monitor_store = store.clone();
    let monitor_owner = owner.clone();
    let monitor_started = started.clone();
    let cancellation = crate::agent::CancellationSignal::new();
    let monitor = tokio::spawn(async move {
        monitor_worker_future(
            &monitor_store,
            &monitor_owner,
            task_id,
            &cancellation,
            async move {
                monitor_started.notify_one();
                sleep(Duration::from_millis(300)).await;
                json!({"outcome": "complete"})
            },
        )
        .await
    });

    started.notified().await;
    assert!(store
        .reconcile(Utc::now())
        .expect("concurrent reconcile")
        .is_empty());
    let run = monitor
        .await
        .expect("monitor joins")
        .expect("monitor result");
    let MonitoredRun::Completed(run) = run else {
        panic!("active worker lease was lost")
    };
    let completed = store
        .update_schedule_progress_owned(&owner, task_id, 1, None, run)
        .expect("lease owner commits progress");
    assert_eq!(completed.status, "completed");
}

#[test]
fn reconciler_revokes_lease_and_fences_late_worker_commit() {
    let (directory, store, session_store) = fixture();
    let task_id = "fenced-schedule";
    prepare_schedule_fixture(&directory, &store, &session_store, task_id);
    let owner = WorkerOwner {
        token: "expired-lease".to_string(),
        pid: 42_002,
    };
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed expired lease");

    let reconciled = store
        .reconcile(Utc::now())
        .expect("reconcile expired lease");
    assert_eq!(reconciled.reconciled_records, 1);
    assert_eq!(reconciled.tasks[0].status, "failed");
    let late_commit =
        store.update_schedule_progress_owned(&owner, task_id, 1, None, json!({"outcome": "late"}));
    assert!(late_commit
        .expect_err("revoked owner must be fenced")
        .contains("worker lease lost"));
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
}

#[test]
fn reconciliation_process_loss_child() {
    let Some(daemon_dir) = std::env::var_os(RECONCILE_CHILD_DAEMON_DIR) else {
        return;
    };
    let task_id = std::env::var(RECONCILE_CHILD_TASK_ID).expect("child task ID");
    let point = std::env::var(RECONCILE_CHILD_POINT).expect("child pause point");
    let ready =
        PathBuf::from(std::env::var_os(RECONCILE_CHILD_READY).expect("child readiness path"));
    let store = DurableTaskStore::at_daemon_dir(daemon_dir).expect("child durable store");
    let result = store.reconcile_with_hook(Utc::now(), |hook_point, id| {
        let selected = matches!(
            (point.as_str(), hook_point),
            ("before_delivery", ReconcileHookPoint::BeforeDelivery)
                | ("after_delivery", ReconcileHookPoint::AfterDelivery)
        );
        if selected && id == task_id {
            std::fs::write(&ready, b"ready")
                .map_err(|error| format!("failed to publish child readiness: {error}"))?;
            std::thread::sleep(Duration::from_secs(60));
        }
        Ok(())
    });
    panic!("reconciler child was not terminated at {point}: {result:?}");
}

#[test]
fn worker_publication_process_loss_child() {
    let Some(daemon_dir) = std::env::var_os(PUBLICATION_CHILD_DAEMON_DIR) else {
        return;
    };
    let sessions_dir = PathBuf::from(
        std::env::var_os(PUBLICATION_CHILD_SESSIONS_DIR).expect("child sessions directory"),
    );
    let task_id = std::env::var(PUBLICATION_CHILD_TASK_ID).expect("child task ID");
    let kind = std::env::var(PUBLICATION_CHILD_KIND).expect("child publication kind");
    let owner = WorkerOwner {
        token: std::env::var(PUBLICATION_CHILD_OWNER_TOKEN).expect("child owner token"),
        pid: std::env::var(PUBLICATION_CHILD_OWNER_PID)
            .expect("child owner pid")
            .parse()
            .expect("numeric child owner pid"),
    };
    let store = DurableTaskStore::at_daemon_dir(daemon_dir).expect("child durable store");
    let session_store = SessionStore::at_dir(sessions_dir);
    let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
    let result = match kind.as_str() {
        "terminal" => publish_terminal_outcome_owned(
            &store,
            &owner,
            &task_id,
            &BackgroundTaskSession {
                session_store,
                session_id: "origin".to_string(),
                audit_log: audit,
            },
            true,
            json!({"exit_code": 0, "stdout": "published before process loss"}),
            None,
        )
        .map(|_| ()),
        "schedule" => publish_schedule_completion_owned(
            &store,
            &owner,
            &task_id,
            &session_store,
            &audit,
            "origin",
            ScheduleCompletion {
                occurrence: 1,
                repeat_count: 1,
                outcome: "plan_ready",
                steps_taken: 1,
                next_run_at: None,
                run: json!({"occurrence": 1, "outcome": "plan_ready"}),
            },
        )
        .map(|_| ()),
        value => panic!("unsupported child publication kind: {value}"),
    };
    panic!("worker publication child was not terminated: {result:?}");
}

#[cfg(any(unix, windows))]
#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn actual_worker_publication_loss_is_reconciled_once_and_fences_late_owners() {
    for point in [
        WorkerPublicationHookPoint::BeforeEffects,
        WorkerPublicationHookPoint::AfterEffects,
    ] {
        let (directory, store, session_store) = fixture();
        let suffix = match point {
            WorkerPublicationHookPoint::BeforeEffects => "before",
            WorkerPublicationHookPoint::AfterEffects => "after",
        };
        let task_id = format!("terminal-worker-loss-{suffix}");
        let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, &task_id);
        let execution_id = store
            .get(&task_id)
            .expect("terminal record")
            .expect("terminal task")
            .execution_id;

        terminate_worker_publication_at(
            &store,
            &session_store,
            &task_id,
            "terminal",
            &owner,
            point,
        );
        let expected_before_reconcile = if point == WorkerPublicationHookPoint::BeforeEffects {
            (0, 0, 0)
        } else {
            (1, 1, 1)
        };
        assert_eq!(
            background_delivery_counts_for_execution(
                &store,
                &session_store,
                &task_id,
                &execution_id,
            ),
            expected_before_reconcile
        );

        let report = store
            .reconcile(Utc::now())
            .expect("reconcile killed terminal publisher");
        assert_eq!(report.reconciled_records, 1);
        assert_eq!(
            background_delivery_counts_for_execution(
                &store,
                &session_store,
                &task_id,
                &execution_id,
            ),
            (1, 1, 1)
        );
        let late = publish_terminal_outcome_owned(
            &store,
            &owner,
            &task_id,
            &BackgroundTaskSession {
                session_store: session_store.clone(),
                session_id: "origin".to_string(),
                audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
            },
            true,
            json!({"exit_code": 0, "stdout": "late"}),
            None,
        )
        .expect_err("reconciler must fence the killed terminal owner");
        assert!(late.contains("worker lease lost"), "{late}");
    }

    for point in [
        WorkerPublicationHookPoint::BeforeEffects,
        WorkerPublicationHookPoint::AfterEffects,
    ] {
        let (directory, store, session_store) = fixture();
        let suffix = match point {
            WorkerPublicationHookPoint::BeforeEffects => "before",
            WorkerPublicationHookPoint::AfterEffects => "after",
        };
        let task_id = format!("schedule-worker-loss-{suffix}");
        prepare_schedule_fixture(&directory, &store, &session_store, &task_id);
        let owner = WorkerOwner {
            token: format!("schedule-worker-loss-{suffix}-lease"),
            pid: 42_600,
        };
        store
            .update(&task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed schedule worker owner");
        let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
        let job = ScheduleWorkerJob {
            prompt: "scheduled plan".to_string(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            interval_secs: 1,
            repeat_count: 1,
        };
        assert_eq!(
            publish_schedule_wake_owned(&store, &owner, &task_id, &session_store, &audit, &job, 1,)
                .expect("publish schedule wake before terminal process loss"),
            ScheduleWakePublication::Started
        );
        store
            .update(&task_id, |task| {
                task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
                Ok(())
            })
            .expect("age schedule worker after wake");
        let execution_id = store
            .get(&task_id)
            .expect("schedule record")
            .expect("schedule task")
            .execution_id;
        let terminal_key = schedule_delivery_key(&task_id, &execution_id, 1, "terminal");

        terminate_worker_publication_at(
            &store,
            &session_store,
            &task_id,
            "schedule",
            &owner,
            point,
        );
        let terminal_counts = || {
            let session = session_store.load("origin").expect("origin session");
            let events = session
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind.as_str(),
                        "scheduled_agent_run_completed" | "scheduled_agent_run_failed"
                    ) && event.details.get("delivery_key").and_then(Value::as_str)
                        == Some(terminal_key.as_str())
                })
                .count();
            let audits = audit
                .read_all()
                .expect("schedule audit")
                .iter()
                .filter(|record| {
                    record
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.contains(&terminal_key))
                })
                .count();
            (events, audits)
        };
        assert_eq!(
            terminal_counts(),
            if point == WorkerPublicationHookPoint::BeforeEffects {
                (0, 0)
            } else {
                (1, 1)
            }
        );

        let report = store
            .reconcile(Utc::now())
            .expect("reconcile killed schedule publisher");
        assert_eq!(report.reconciled_records, 1);
        assert_eq!(terminal_counts(), (1, 1));
        let late = publish_schedule_completion_owned(
            &store,
            &owner,
            &task_id,
            &session_store,
            &audit,
            "origin",
            ScheduleCompletion {
                occurrence: 1,
                repeat_count: 1,
                outcome: "late",
                steps_taken: 1,
                next_run_at: None,
                run: json!({"occurrence": 1, "outcome": "late"}),
            },
        )
        .expect_err("reconciler must fence the killed schedule owner");
        assert!(late.contains("worker lease lost"), "{late}");
    }
}

#[test]
fn reconciling_record_resumes_after_loss_before_delivery_and_keeps_worker_fenced() {
    let (directory, store, session_store) = fixture();
    let task_id = "resume-before-delivery";
    let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);

    terminate_reconciler_at(&store, task_id, ReconcileHookPoint::BeforeDelivery);
    let claimed = store
        .get_file(task_id)
        .expect("durable reconciliation claim");
    assert_eq!(claimed.record.status, "reconciling");
    assert!(claimed.worker_lease.is_none());
    assert_eq!(
        background_delivery_counts(&store, &session_store, task_id),
        (0, 0, 0)
    );

    let late_commit = store.finish_owned(
        &owner,
        task_id,
        "completed",
        Some(json!({"outcome": "late"})),
        None,
    );
    assert!(late_commit
        .expect_err("revoked worker remains fenced after reconciler loss")
        .contains("worker lease lost"));

    let report = store
        .reconcile(Utc::now())
        .expect("later reconciler resumes durable claim");
    assert_eq!(report.reconciled_records, 1);
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
    assert_eq!(
        background_delivery_counts(&store, &session_store, task_id),
        (1, 1, 1)
    );
}

#[test]
fn reconciling_record_resumes_after_loss_without_repeating_delivered_side_effects() {
    let (directory, store, session_store) = fixture();
    let task_id = "resume-after-delivery";
    prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);

    terminate_reconciler_at(&store, task_id, ReconcileHookPoint::AfterDelivery);
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "reconciling");
    assert_eq!(
        background_delivery_counts(&store, &session_store, task_id),
        (1, 1, 1)
    );

    let report = store
        .reconcile(Utc::now())
        .expect("later reconciler resumes delivered claim");
    assert_eq!(report.reconciled_records, 1);
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
    assert_eq!(
        background_delivery_counts(&store, &session_store, task_id),
        (1, 1, 1)
    );
}

#[test]
fn schedule_reconciliation_resumes_after_killed_delivery_without_duplicates() {
    let (directory, store, session_store) = fixture();
    let task_id = "resume-schedule-after-delivery";
    prepare_schedule_fixture(&directory, &store, &session_store, task_id);
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(42_200);
            task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
            task.worker_lease = Some(WorkerLease {
                token: "schedule-reconcile-lease".to_string(),
            });
            Ok(())
        })
        .expect("seed stale schedule worker");

    terminate_reconciler_at(&store, task_id, ReconcileHookPoint::AfterDelivery);
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "reconciling");
    let count_deliveries = || {
        let session = session_store.load("origin").expect("origin session");
        let events = session
            .events
            .iter()
            .filter(|event| {
                event.kind == "scheduled_agent_run_failed"
                    && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                    && event.details.get("reconciled").and_then(Value::as_bool) == Some(true)
            })
            .count();
        let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("daemon audit")
            .iter()
            .filter(|record| {
                record.action == "reconcile_expired_worker"
                    && record
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.contains(&format!("timer_id={task_id}")))
            })
            .count();
        (events, audits)
    };
    assert_eq!(count_deliveries(), (1, 1));

    let report = store
        .reconcile(Utc::now())
        .expect("later reconciler resumes schedule claim");
    assert_eq!(report.reconciled_records, 1);
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
    assert_eq!(count_deliveries(), (1, 1));
}

#[test]
fn schedule_reconciliation_resumes_after_loss_before_delivery_and_fences_owner() {
    let (directory, store, session_store) = fixture();
    let task_id = "resume-schedule-before-delivery";
    prepare_schedule_fixture(&directory, &store, &session_store, task_id);
    let owner = WorkerOwner {
        token: "schedule-before-reconcile-lease".to_string(),
        pid: 42_225,
    };
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed stale schedule worker");

    terminate_reconciler_at(&store, task_id, ReconcileHookPoint::BeforeDelivery);
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "reconciling");
    let session = session_store.load("origin").expect("origin session");
    assert!(!session.events.iter().any(|event| {
        event.kind == "scheduled_agent_run_failed"
            && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
            && event.details.get("reconciled").and_then(Value::as_bool) == Some(true)
    }));
    let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
    assert!(!audit
        .read_all()
        .expect("daemon audit")
        .iter()
        .any(|record| {
            record.action == "reconcile_expired_worker"
                && record
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains(&format!("timer_id={task_id}")))
        }));

    let late = publish_schedule_failure_owned(
        &store,
        &owner,
        task_id,
        &session_store,
        &audit,
        ScheduleFailure {
            session_id: "origin",
            occurrence: 1,
            repeat_count: 1,
            outcome: "local_error",
            error: "late owner".to_string(),
            failure: None,
        },
    )
    .expect_err("reconciliation claim must fence the prior schedule owner");
    assert!(late.contains("worker lease lost"), "{late}");

    let report = store
        .reconcile(Utc::now())
        .expect("later reconciler resumes schedule claim");
    assert_eq!(report.reconciled_records, 1);
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
    let session = session_store.load("origin").expect("origin session");
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| {
                event.kind == "scheduled_agent_run_failed"
                    && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                    && event.details.get("reconciled").and_then(Value::as_bool) == Some(true)
            })
            .count(),
        1
    );
    assert_eq!(
        audit
            .read_all()
            .expect("daemon audit")
            .iter()
            .filter(|record| {
                record.action == "reconcile_expired_worker"
                    && record
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.contains(&format!("timer_id={task_id}")))
            })
            .count(),
        1
    );
}

#[test]
fn schedule_reconciliation_preserves_a_prior_terminal_observation() {
    let (directory, store, session_store) = fixture();
    let task_id = "reconcile-after-schedule-completion";
    prepare_schedule_fixture(&directory, &store, &session_store, task_id);
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(42_250);
            task.record.updated_at = Utc::now() - ChronoDuration::seconds(30);
            task.worker_lease = Some(WorkerLease {
                token: "schedule-completed-before-loss".to_string(),
            });
            task.active_occurrence = Some(1);
            Ok(())
        })
        .expect("seed stale schedule worker");
    let execution_id = store
        .get_file(task_id)
        .expect("schedule execution")
        .record
        .execution_id;
    let delivery_key = schedule_delivery_key(task_id, &execution_id, 1, "terminal");
    record_schedule_event_once(
        &session_store,
        "origin",
        "scheduled_agent_run_completed",
        json!({
            "timer_id": task_id,
            "execution_id": execution_id,
            "occurrence": 1,
            "repeat_count": 1,
            "outcome": "plan_ready",
            "steps_taken": 1,
            "mode": "plan",
        }),
        &delivery_key,
    )
    .expect("seed prior keyed completion event");
    let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));

    let report = store
        .reconcile(Utc::now())
        .expect("reconcile after completion publication");
    assert_eq!(report.reconciled_records, 1);
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "failed");
    let session = session_store.load("origin").expect("origin session");
    let terminal_events = session
        .events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "scheduled_agent_run_completed" | "scheduled_agent_run_failed"
            ) && event.details.get("timer_id").and_then(Value::as_str) == Some(task_id)
                && event.details.get("occurrence").and_then(Value::as_u64) == Some(1)
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal_events.len(), 1);
    assert_eq!(terminal_events[0].kind, "scheduled_agent_run_completed");
    let audits = audit.read_all().expect("daemon audit");
    assert_eq!(
        audits
            .iter()
            .filter(|record| {
                record
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains(&delivery_key))
            })
            .count(),
        1
    );
    assert!(audits.iter().any(|record| {
        record.action == "wake_agent_loop"
            && record.outcome == "completed"
            && record
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains(&delivery_key))
    }));
    assert!(!audits.iter().any(|record| {
        record.action == "reconcile_expired_worker"
            && record
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains(&format!("timer_id={task_id}")))
    }));
}

#[test]
fn terminal_publication_revalidates_owner_before_effect_and_transition() {
    let (directory, store, session_store) = fixture();
    let task_id = "revoked-terminal-publication";
    let owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
    store
        .poll_worker_owned(task_id, &owner, true)
        .expect("worker performs its pre-publication poll");
    store
        .update(task_id, |task| {
            task.record.status = "reconciling".to_string();
            task.record.worker_pid = None;
            task.worker_lease = None;
            Ok(())
        })
        .expect("revoke worker after prior poll");
    let target = BackgroundTaskSession {
        session_store: session_store.clone(),
        session_id: "origin".to_string(),
        audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
    };

    let error = publish_terminal_outcome_owned(
        &store,
        &owner,
        task_id,
        &target,
        true,
        json!({"exit_code": 0, "stdout": "late"}),
        None,
    )
    .expect_err("revoked worker cannot enter publication closure");
    assert!(error.contains("worker lease lost"), "{error}");
    assert_eq!(
        background_delivery_counts(&store, &session_store, task_id),
        (0, 0, 0)
    );
    assert_eq!(store.get(task_id).unwrap().unwrap().status, "reconciling");
}

#[test]
fn valid_terminal_success_and_cancel_publish_with_their_durable_transition() {
    let (directory, store, session_store) = fixture();
    let success_id = "owned-terminal-success";
    let success_owner =
        prepare_reconcilable_terminal(&directory, &store, &session_store, success_id);
    let target = BackgroundTaskSession {
        session_store: session_store.clone(),
        session_id: "origin".to_string(),
        audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
    };
    let completed = publish_terminal_outcome_owned(
        &store,
        &success_owner,
        success_id,
        &target,
        true,
        json!({"exit_code": 0, "stdout": "ok"}),
        None,
    )
    .expect("owned success publication");
    assert_eq!(completed.status, "completed");
    assert_eq!(
        background_delivery_counts(&store, &session_store, success_id),
        (1, 1, 1)
    );

    let cancel_id = "owned-terminal-cancel";
    let cancel_owner = prepare_reconcilable_terminal(&directory, &store, &session_store, cancel_id);
    store
        .update(cancel_id, |task| {
            task.record.cancel_requested = true;
            Ok(())
        })
        .expect("request terminal cancellation");
    let claimed = store
        .get_file(cancel_id)
        .expect("claimed cancellation task");
    let cancelled = publish_claimed_cancellation(&store, &cancel_owner, cancel_id, &claimed)
        .expect("owned cancellation publication");
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(
        background_delivery_counts(&store, &session_store, cancel_id),
        (1, 1, 1)
    );
}

#[test]
fn reused_terminal_id_delivers_each_execution_exactly_once() {
    let (directory, store, session_store) = fixture();
    let task_id = "reused-terminal";
    let target = BackgroundTaskSession {
        session_store: session_store.clone(),
        session_id: "origin".to_string(),
        audit_log: DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl")),
    };

    let first_owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
    let first_execution = store
        .get(task_id)
        .expect("first record")
        .expect("first task")
        .execution_id;
    publish_terminal_outcome_owned(
        &store,
        &first_owner,
        task_id,
        &target,
        true,
        json!({"exit_code": 0, "stdout": "first"}),
        None,
    )
    .expect("publish first execution");
    store
        .evict_terminal_record(task_id)
        .expect("evict first terminal execution");

    let second_owner = prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
    let second_execution = store
        .get(task_id)
        .expect("second record")
        .expect("second task")
        .execution_id;
    assert_ne!(first_execution, second_execution);
    publish_terminal_outcome_owned(
        &store,
        &second_owner,
        task_id,
        &target,
        true,
        json!({"exit_code": 0, "stdout": "second"}),
        None,
    )
    .expect("publish second execution");

    assert_eq!(
        background_delivery_counts_for_execution(&store, &session_store, task_id, &first_execution,),
        (1, 1, 1)
    );
    assert_eq!(
        background_delivery_counts_for_execution(
            &store,
            &session_store,
            task_id,
            &second_execution,
        ),
        (1, 1, 1)
    );
    assert_eq!(
        background_delivery_counts(&store, &session_store, task_id),
        (2, 2, 2)
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn legacy_execution_generation_is_deterministic_persisted_and_dedupes_old_evidence() {
    let (directory, store, session_store) = fixture();
    let task_id = "legacy-generation";
    prepare_reconcilable_terminal(&directory, &store, &session_store, task_id);
    let task_path = store.task_path(task_id);
    let mut encoded: Value =
        serde_json::from_slice(&std::fs::read(&task_path).expect("read modern task record"))
            .expect("decode modern task record");
    encoded
        .get_mut("record")
        .and_then(Value::as_object_mut)
        .expect("record object")
        .remove("execution_id");
    std::fs::write(
        &task_path,
        serde_json::to_vec_pretty(&encoded).expect("encode legacy task record"),
    )
    .expect("write legacy task record");

    session_store
        .update_session("origin", |session| {
            for tool_call in &mut session.tool_calls {
                if let Some(output) = tool_call
                    .result
                    .as_mut()
                    .and_then(|result| result.get_mut("output"))
                    .and_then(Value::as_object_mut)
                {
                    output.remove("execution_id");
                }
            }
            session.messages.push(crate::session::SessionMessage {
                index: session.messages.len(),
                role: "user".to_string(),
                content: "legacy background task context".to_string(),
                timestamp: Some(Utc::now()),
                attachments: Vec::new(),
            });
            session.messages.push(crate::session::SessionMessage {
                index: session.messages.len(),
                role: "assistant".to_string(),
                content: "legacy boundary".to_string(),
                timestamp: Some(Utc::now()),
                attachments: Vec::new(),
            });
            session.messages.push(crate::session::SessionMessage {
                index: session.messages.len(),
                role: "tool".to_string(),
                content: json!({
                    "type": "background_task_result",
                    "task_id": task_id,
                    "success": false,
                    "error": "legacy worker loss",
                })
                .to_string(),
                timestamp: Some(Utc::now()),
                attachments: Vec::new(),
            });
            session.events.push(SessionEvent {
                index: session.events.len(),
                kind: "background_task_failed".to_string(),
                details: json!({
                    "task_id": task_id,
                    "success": false,
                    "error": "legacy worker loss",
                }),
                timestamp: Some(Utc::now()),
            });
            Ok(())
        })
        .expect("seed pre-generation session evidence");
    let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
    audit
        .append(&DaemonAuditRecord {
            timestamp: Utc::now(),
            daemon: "background_task".to_string(),
            action: "deliver_observation".to_string(),
            target: Some("origin".to_string()),
            outcome: "failed".to_string(),
            authorized: true,
            detail: Some(format!("task_id={task_id}; error=legacy worker loss")),
        })
        .expect("seed pre-generation daemon audit");

    let migrated =
        DurableTaskStore::at_daemon_dir(store.daemon_dir()).expect("migrate legacy generation");
    let first_generation = migrated
        .get(task_id)
        .expect("migrated record")
        .expect("migrated task")
        .execution_id;
    assert!(first_generation.starts_with("legacy-"));
    let persisted: Value =
        serde_json::from_slice(&std::fs::read(&task_path).expect("read migrated task record"))
            .expect("decode migrated task record");
    assert_eq!(
        persisted
            .get("record")
            .and_then(|record| record.get("execution_id"))
            .and_then(Value::as_str),
        Some(first_generation.as_str())
    );
    let restarted = DurableTaskStore::at_daemon_dir(store.daemon_dir())
        .expect("restart after generation migration");
    assert_eq!(
        restarted
            .get(task_id)
            .expect("restarted record")
            .expect("restarted task")
            .execution_id,
        first_generation
    );

    let report = restarted
        .reconcile(Utc::now())
        .expect("reconcile migrated legacy task");
    assert_eq!(report.reconciled_records, 1);
    assert_eq!(
        background_delivery_counts(&restarted, &session_store, task_id),
        (1, 1, 1)
    );
    assert_eq!(
        background_delivery_counts_for_execution(
            &restarted,
            &session_store,
            task_id,
            &first_generation,
        ),
        (0, 0, 0),
        "legacy evidence remains unmodified and is not duplicated"
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn reused_schedule_id_keeps_wake_and_terminal_delivery_keys_generation_scoped() {
    fn publish_execution(
        directory: &tempfile::TempDir,
        store: &DurableTaskStore,
        session_store: &SessionStore,
        task_id: &str,
        owner_token: &str,
        stdout: &str,
    ) -> String {
        prepare_schedule_fixture(directory, store, session_store, task_id);
        let owner = WorkerOwner {
            token: owner_token.to_string(),
            pid: 42_700,
        };
        store
            .update(task_id, |task| {
                task.record.status = "running".to_string();
                task.record.worker_pid = Some(owner.pid);
                task.worker_lease = Some(WorkerLease {
                    token: owner.token.clone(),
                });
                Ok(())
            })
            .expect("seed schedule owner");
        let execution_id = store
            .get(task_id)
            .expect("schedule record")
            .expect("schedule task")
            .execution_id;
        let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
        let job = ScheduleWorkerJob {
            prompt: "scheduled plan".to_string(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: session_store.sessions_dir().to_path_buf(),
            session_id: "origin".to_string(),
            interval_secs: 1,
            repeat_count: 1,
        };
        assert_eq!(
            publish_schedule_wake_owned(store, &owner, task_id, session_store, &audit, &job, 1,)
                .expect("publish schedule wake"),
            ScheduleWakePublication::Started
        );
        publish_schedule_completion_owned(
            store,
            &owner,
            task_id,
            session_store,
            &audit,
            "origin",
            ScheduleCompletion {
                occurrence: 1,
                repeat_count: 1,
                outcome: "plan_ready",
                steps_taken: 1,
                next_run_at: None,
                run: json!({"occurrence": 1, "outcome": "plan_ready", "stdout": stdout}),
            },
        )
        .expect("publish schedule completion");
        execution_id
    }

    let (directory, store, session_store) = fixture();
    let task_id = "reused-schedule";
    let first_execution = publish_execution(
        &directory,
        &store,
        &session_store,
        task_id,
        "first-schedule-owner",
        "first",
    );
    store
        .evict_terminal_record(task_id)
        .expect("evict first schedule execution");
    let second_execution = publish_execution(
        &directory,
        &store,
        &session_store,
        task_id,
        "second-schedule-owner",
        "second",
    );
    assert_ne!(first_execution, second_execution);

    let session = session_store.load("origin").expect("origin session");
    let audits = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
        .read_all()
        .expect("daemon audit");
    for execution_id in [&first_execution, &second_execution] {
        for phase in ["start", "terminal"] {
            let key = schedule_delivery_key(task_id, execution_id, 1, phase);
            assert_eq!(
                session
                    .events
                    .iter()
                    .filter(|event| {
                        event.details.get("delivery_key").and_then(Value::as_str)
                            == Some(key.as_str())
                    })
                    .count(),
                1,
                "missing generation-scoped {phase} event for {execution_id}"
            );
            assert_eq!(
                audits
                    .iter()
                    .filter(|record| {
                        record
                            .detail
                            .as_deref()
                            .is_some_and(|detail| detail.contains(&key))
                    })
                    .count(),
                1,
                "missing generation-scoped {phase} audit for {execution_id}"
            );
        }
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn schedule_publications_are_fenced_and_phase_consistent() {
    let (directory, store, session_store) = fixture();
    let task_id = "owned-schedule-success";
    prepare_schedule_fixture(&directory, &store, &session_store, task_id);
    let owner = WorkerOwner {
        token: "owned-schedule-lease".to_string(),
        pid: 42_300,
    };
    store
        .update(task_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed owned schedule");
    let audit = DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"));
    let job = ScheduleWorkerJob {
        prompt: "scheduled plan".to_string(),
        project_root: directory.path().to_path_buf(),
        profile_id: "default".to_string(),
        sessions_dir: session_store.sessions_dir().to_path_buf(),
        session_id: "origin".to_string(),
        interval_secs: 1,
        repeat_count: 1,
    };
    assert_eq!(
        publish_schedule_wake_owned(&store, &owner, task_id, &session_store, &audit, &job, 1,)
            .expect("owned wake publication"),
        ScheduleWakePublication::Started
    );
    assert_eq!(store.get_file(task_id).unwrap().active_occurrence, Some(1));
    let completed = publish_schedule_completion_owned(
        &store,
        &owner,
        task_id,
        &session_store,
        &audit,
        "origin",
        ScheduleCompletion {
            occurrence: 1,
            repeat_count: 1,
            outcome: "plan_ready",
            steps_taken: 1,
            next_run_at: None,
            run: json!({"occurrence": 1, "outcome": "plan_ready"}),
        },
    )
    .expect("owned completion publication");
    assert_eq!(completed.status, "completed");
    assert!(store.get_file(task_id).unwrap().active_occurrence.is_none());

    let revoked_id = "revoked-schedule-wake";
    prepare_schedule_fixture(&directory, &store, &session_store, revoked_id);
    store
        .update(revoked_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed owned schedule before revocation");
    store
        .poll_worker_owned(revoked_id, &owner, true)
        .expect("schedule worker performs its pre-publication poll");
    store
        .update(revoked_id, |task| {
            task.record.status = "reconciling".to_string();
            task.record.worker_pid = None;
            task.worker_lease = None;
            Ok(())
        })
        .expect("revoke schedule after prior poll");
    let error =
        publish_schedule_wake_owned(&store, &owner, revoked_id, &session_store, &audit, &job, 1)
            .expect_err("revoked schedule cannot publish wake effects");
    assert!(error.contains("worker lease lost"), "{error}");
    let session = session_store.load("origin").expect("origin session");
    assert!(!session.events.iter().any(|event| {
        event.details.get("timer_id").and_then(Value::as_str) == Some(revoked_id)
    }));

    let cancelled_id = "owned-schedule-cancel";
    prepare_schedule_fixture(&directory, &store, &session_store, cancelled_id);
    store
        .update(cancelled_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.record.cancel_requested = true;
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed cancelled schedule");
    assert_eq!(
        publish_schedule_wake_owned(
            &store,
            &owner,
            cancelled_id,
            &session_store,
            &audit,
            &job,
            1,
        )
        .expect("owned cancellation publication"),
        ScheduleWakePublication::Finished
    );
    assert_eq!(
        store.get(cancelled_id).unwrap().unwrap().status,
        "cancelled"
    );

    let failed_id = "owned-schedule-failure";
    prepare_schedule_fixture(&directory, &store, &session_store, failed_id);
    store
        .update(failed_id, |task| {
            task.record.status = "running".to_string();
            task.record.worker_pid = Some(owner.pid);
            task.worker_lease = Some(WorkerLease {
                token: owner.token.clone(),
            });
            Ok(())
        })
        .expect("seed failed schedule");
    let secret = "durable-provider-secret";
    let sensitive_values = vec![secret.to_string()];
    let typed_failure = crate::llm::LlmError::new(
        crate::llm::LlmErrorClass::Authentication,
        crate::llm::LlmErrorPhase::HttpResponse,
        crate::llm::RetryDisposition::NotAttempted,
        crate::llm::LlmErrorMetadata::new(
            "openai",
            "responses",
            Some(secret),
            Some(401),
            &sensitive_values,
        ),
        format!("remote detail contained {secret}"),
    );
    let failed = publish_schedule_failure_owned(
        &store,
        &owner,
        failed_id,
        &session_store,
        &audit,
        ScheduleFailure {
            session_id: "origin",
            occurrence: 1,
            repeat_count: 1,
            outcome: "planning_failed",
            error: "scheduled agent run failed".to_string(),
            failure: Some(&typed_failure),
        },
    )
    .expect("owned failure publication");
    assert_eq!(failed.status, "failed");
    assert_eq!(
        failed.result.as_ref().and_then(|result| result
            .get("runs")
            .and_then(Value::as_array)
            .and_then(|runs| runs.last())
            .and_then(|run| run.get("failure"))
            .and_then(|failure| failure.get("class"))
            .and_then(Value::as_str)),
        Some("authentication")
    );
    assert_eq!(
        failed.result.as_ref().and_then(|result| result
            .get("runs")
            .and_then(Value::as_array)
            .and_then(|runs| runs.last())
            .and_then(|run| run.get("outcome"))
            .and_then(Value::as_str)),
        Some("planning_failed")
    );
    let session = session_store.load("origin").expect("origin session");
    let durable_failure_event = session
        .events
        .iter()
        .find(|event| {
            event.kind == "scheduled_agent_run_failed"
                && event.details.get("timer_id").and_then(Value::as_str) == Some(failed_id)
        })
        .expect("typed scheduled failure event");
    assert_eq!(
        durable_failure_event.details["failure"]["incident_code"],
        "LLM-AUTH"
    );
    assert_eq!(
        durable_failure_event.details["failure"]["class"],
        "authentication"
    );
    assert_eq!(durable_failure_event.details["outcome"], "planning_failed");
    let persisted = format!(
        "{}\n{}",
        serde_json::to_string(&failed).expect("serialize failed durable record"),
        serde_json::to_string(durable_failure_event).expect("serialize failure event")
    );
    assert!(!persisted.contains(secret), "{persisted}");
    assert!(!persisted.contains("LLM request failed"), "{persisted}");
    for id in [task_id, cancelled_id, failed_id] {
        assert!(session
            .events
            .iter()
            .any(|event| { event.details.get("timer_id").and_then(Value::as_str) == Some(id) }));
    }
}

#[test]
fn task_lock_keeps_a_stable_inode_and_never_steals_live_ownership() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("task.lock");
    let anchor_path = directory.path().join(".task.lock.anchor");
    let first = TaskLock::acquire(path.clone(), anchor_path.clone()).expect("first owner");
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("open stable lock inode");

    assert!(matches!(
        contender
            .try_lock()
            .expect_err("live owner cannot be displaced"),
        std::fs::TryLockError::WouldBlock
    ));
    assert!(path.exists(), "lock inode remains present while owned");

    drop(first);
    contender.try_lock().expect("dead owner releases OS lock");
    assert!(
        path.exists(),
        "stable lock inode is not unlinked on release"
    );
    assert!(
        anchor_path.exists(),
        "persistent lock anchor remains reachable"
    );
}

#[cfg(unix)]
#[test]
fn task_lock_rejects_regular_file_replacement_during_open() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("task.lock");
    let anchor_path = directory.path().join(".task.lock.anchor");
    let displaced = directory.path().join("displaced.lock");
    let error = TaskLock::acquire_with_hook(path.clone(), anchor_path, || {
        std::fs::rename(&path, &displaced)
            .map_err(|error| format!("failed to displace lock: {error}"))?;
        std::fs::write(&path, b"replacement")
            .map_err(|error| format!("failed to replace lock: {error}"))
    })
    .expect_err("replacement lock inode must be rejected");
    assert!(error.contains("identity changed"), "{error}");
}

#[test]
fn task_lock_replacement_child_process() {
    let Some(daemon_dir) = std::env::var_os(TASK_LOCK_CHILD_DAEMON_DIR) else {
        return;
    };
    let kind = std::env::var(TASK_LOCK_CHILD_KIND).expect("child lock kind");
    let id = std::env::var(TASK_LOCK_CHILD_ID).expect("child task ID");
    let expectation = std::env::var(TASK_LOCK_CHILD_EXPECTATION).expect("child lock expectation");
    let daemon_dir = PathBuf::from(daemon_dir);
    let tasks_dir = daemon_dir.join("tasks");
    let (path, anchor_path) = match kind.as_str() {
        "task" => {
            let stripe = task_lock_stripe(&id);
            (
                tasks_dir.join(format!(".task-stripe-{stripe:02}.lock")),
                daemon_dir.join(format!(".task-stripe-{stripe:02}.lock.anchor")),
            )
        }
        "admission" => (
            tasks_dir.join(".admission.lock"),
            daemon_dir.join(".admission.task.lock.anchor"),
        ),
        value => panic!("unsupported child lock kind: {value}"),
    };
    let error = TaskLock::acquire_with_retries(path, anchor_path, 10)
        .expect_err("replacement must not create a second task lock domain");
    match expectation.as_str() {
        "timeout" => assert!(error.contains("timed out acquiring task lock"), "{error}"),
        "identity" => assert!(
            error.contains("different identities") || error.contains("identity changed"),
            "{error}"
        ),
        value => panic!("unsupported child expectation: {value}"),
    }
}

#[cfg(unix)]
#[test]
fn persistent_anchors_protect_task_and_admission_locks_across_path_replacement() {
    let (_directory, store, _session_store) = fixture();
    let task_id = "anchored-owner";

    for kind in ["task", "admission"] {
        let (path, anchor_path) = match kind {
            "task" => (store.lock_path(task_id), store.lock_anchor_path(task_id)),
            "admission" => (
                store.admission_lock_path(),
                store.admission_lock_anchor_path(),
            ),
            _ => unreachable!(),
        };
        let held = TaskLock::acquire(path.clone(), anchor_path.clone())
            .expect("held persistent task lock");
        let run_child = |expectation: &str| {
            let output = Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "daemons::workload::tests::test_part_1::task_lock_replacement_child_process",
                    "--nocapture",
                ])
                .env(TASK_LOCK_CHILD_DAEMON_DIR, store.daemon_dir())
                .env(TASK_LOCK_CHILD_KIND, kind)
                .env(TASK_LOCK_CHILD_ID, task_id)
                .env(TASK_LOCK_CHILD_EXPECTATION, expectation)
                .output()
                .expect("run task lock child process");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run_child("timeout");

        let displaced_path = path.with_extension("lock.displaced");
        std::fs::rename(&path, &displaced_path).expect("displace visible task lock");
        std::fs::write(&path, b"replacement").expect("replace visible task lock");
        run_child("identity");
        std::fs::remove_file(&path).expect("remove replacement task lock");
        std::fs::rename(&displaced_path, &path).expect("restore anchored task lock");

        let displaced_tasks = store.daemon_dir().join("tasks.displaced");
        std::fs::rename(&store.tasks_dir, &displaced_tasks)
            .expect("displace durable tasks directory");
        std::fs::create_dir(&store.tasks_dir).expect("replace durable tasks directory");
        run_child("timeout");
        std::fs::remove_dir_all(&store.tasks_dir)
            .expect("remove replacement durable tasks directory");
        std::fs::rename(&displaced_tasks, &store.tasks_dir)
            .expect("restore durable tasks directory");

        drop(held);
        TaskLock::acquire(path, anchor_path).expect("restored anchored task lock remains usable");
    }
}

#[cfg(windows)]
#[test]
fn open_task_directory_capability_preserves_lock_domain_after_parent_replacement() {
    let (_directory, store, _session_store) = fixture();
    let task_id = "anchored-windows-owner";
    for kind in ["task", "admission"] {
        let (path, anchor_path) = match kind {
            "task" => (store.lock_path(task_id), store.lock_anchor_path(task_id)),
            "admission" => (
                store.admission_lock_path(),
                store.admission_lock_anchor_path(),
            ),
            _ => unreachable!(),
        };
        let held = TaskLock::acquire(path.clone(), anchor_path.clone())
            .expect("held persistent task lock");
        let displaced_tasks = store.daemon_dir().join("tasks.displaced");
        std::fs::rename(&store.tasks_dir, &displaced_tasks)
            .expect("displace share-compatible durable tasks directory");
        std::fs::create_dir(&store.tasks_dir).expect("replace durable tasks directory");
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "daemons::workload::tests::test_part_1::task_lock_replacement_child_process",
                "--nocapture",
            ])
            .env(TASK_LOCK_CHILD_DAEMON_DIR, store.daemon_dir())
            .env(TASK_LOCK_CHILD_KIND, kind)
            .env(TASK_LOCK_CHILD_ID, task_id)
            .env(TASK_LOCK_CHILD_EXPECTATION, "timeout")
            .output()
            .expect("run task lock child process");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(&store.tasks_dir)
            .expect("remove replacement durable tasks directory");
        std::fs::rename(&displaced_tasks, &store.tasks_dir)
            .expect("restore durable tasks directory");
        drop(held);
        TaskLock::acquire(path, anchor_path).expect("released lock remains usable");
    }
}

#[cfg(any(unix, windows))]
#[test]
fn fixed_stripes_bound_failed_id_artifacts_and_migrate_legacy_locks() {
    let (_directory, store, _session_store) = fixture();
    for index in 0..512 {
        let id = format!("missing-{index}");
        let error = store.cancel(&id).expect_err("missing task stays missing");
        assert!(error.contains("background task not found"), "{error}");
    }

    for index in 0..80 {
        let id = format!("legacy-{index}");
        let visible = store.tasks_dir.join(format!("{id}.lock"));
        let anchor = store.daemon_dir.join(format!(".{id}.task.lock.anchor"));
        std::fs::write(&visible, b"legacy").expect("legacy visible lock");
        std::fs::hard_link(&visible, &anchor).expect("legacy persistent anchor");
        let error = store
            .cancel(&id)
            .expect_err("legacy missing task stays missing");
        assert!(error.contains("background task not found"), "{error}");
        assert!(!visible.exists(), "legacy visible lock was removed");
        assert!(!anchor.exists(), "legacy persistent anchor was removed");
    }

    let live_id = "legacy-live-owner";
    let live_visible = store.tasks_dir.join(format!("{live_id}.lock"));
    let live_anchor = store
        .daemon_dir
        .join(format!(".{live_id}.task.lock.anchor"));
    std::fs::write(&live_visible, b"legacy live").expect("live legacy lock");
    std::fs::hard_link(&live_visible, &live_anchor).expect("live legacy anchor");
    let live_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&live_anchor)
        .expect("open live legacy anchor");
    live_file.try_lock().expect("hold legacy lock");
    let error = store
        .cancel(live_id)
        .expect_err("live legacy owner must fail migration closed");
    assert!(error.contains("still owned"), "{error}");
    assert!(live_visible.exists());
    assert!(live_anchor.exists());
    drop(live_file);
    store
        .cancel(live_id)
        .expect_err("released legacy lock migrates before missing result");
    assert!(!live_visible.exists());
    assert!(!live_anchor.exists());

    let visible_locks = std::fs::read_dir(&store.tasks_dir)
        .expect("task directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".task-stripe-")
        })
        .count();
    let persistent_anchors = std::fs::read_dir(&store.daemon_dir)
        .expect("daemon directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".task-stripe-") && name.ends_with(".lock.anchor")
        })
        .count();
    assert_eq!(visible_locks, TASK_LOCK_STRIPES);
    assert_eq!(persistent_anchors, TASK_LOCK_STRIPES);
    let admission = store.admission_lock_path();
    let admission_anchor = store.admission_lock_anchor_path();
    assert!(admission.exists());
    assert!(admission_anchor.exists());
    assert_eq!(
        task_file_identity(
            &OpenOptions::new()
                .read(true)
                .write(true)
                .open(&admission)
                .expect("admission lock"),
            &admission,
        )
        .expect("admission identity"),
        task_file_identity(
            &OpenOptions::new()
                .read(true)
                .write(true)
                .open(&admission_anchor)
                .expect("admission anchor"),
            &admission_anchor,
        )
        .expect("admission anchor identity")
    );
    assert_eq!(
        std::fs::read_dir(&store.tasks_dir)
            .expect("bounded task directory")
            .count(),
        TASK_LOCK_STRIPES + 1,
        "failed IDs must not grow task-directory inode use beyond the fixed controls"
    );
}
