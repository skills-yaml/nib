use super::*;

#[tokio::test]
#[serial_test::serial]
async fn exact_run_steering_after_tool_start_applies_before_the_next_provider_request() {
    // Readiness includes durable session I/O on hosted Windows runners.
    const HOSTED_PROGRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
    let directory = tempdir().expect("project");
    initialize_git_repository(directory.path());
    save_config(directory.path(), &mock_config()).expect("mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let goal = "exact run steering tool smoke";
    let mut session = store.create_session_with_id("steering-after-tool-start");
    let mut plan = pending_plan(goal, goal);
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");
    let run_id = "fedcba9876543210fedcba9876543210";
    let (steering, steering_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
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

    tokio::time::timeout(HOSTED_PROGRESS_TIMEOUT, async {
        loop {
            if matches!(
                stream_rx.recv().await.expect("stream closed before tool started"),
                StreamEvent::ToolStarted { tool_name, .. } if tool_name == "run_terminal"
            ) {
                loop {
                    if store
                        .load(&session.id)
                        .expect("tool lifecycle")
                        .events
                        .iter()
                        .any(|event| event.kind == "tool_started")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                break;
            }
        }
    })
    .await
    .expect("tool started");
    steering
        .submit("replacement steering marker after the started tool")
        .expect("durable post-tool steering");

    let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("post-tool steered run completed")
        .expect("agent task joined")
        .expect("agent run succeeded");
    assert_eq!(summary.outcome, "completed");
    assert_eq!(summary.tool_call_count, 1);
    assert_eq!(
        summary.last_message.as_deref(),
        Some("Final answer: replacement steering marker observed.")
    );
    let persisted = store.load(&session.id).expect("post-tool steering state");
    assert_eq!(
        persisted
            .tool_calls
            .iter()
            .filter(|record| record.tool_name.as_deref() == Some("run_terminal"))
            .count(),
        1
    );
    let terminal = persisted
        .tool_calls
        .iter()
        .find(|record| record.tool_name.as_deref() == Some("run_terminal"))
        .expect("terminal execution audit");
    let result = terminal.result.as_ref().expect("terminal result");
    assert_eq!(result["success"], true, "terminal result: {result}");
    assert!(result["output"]["stdout"]
        .as_str()
        .is_some_and(|stdout| stdout.contains("completed before steering")));
    let event_index = |kind: &str| {
        persisted
            .events
            .iter()
            .position(|event| event.kind == kind)
            .unwrap_or_else(|| panic!("missing {kind} event"))
    };
    assert!(event_index("tool_started") < event_index("steering_input"));
    assert!(event_index("steering_input") < event_index("tool_completed"));
    assert!(event_index("tool_completed") < event_index("steering_intake"));
    assert!(persisted.events.iter().any(|event| {
        event.kind == "provider_continuation_abandoned_by_steering"
            && event.details["run_id"] == run_id
    }));
}

#[tokio::test]
#[serial_test::serial]
async fn exact_run_steering_drained_in_compression_abandons_the_tool_continuation() {
    const HOSTED_PROGRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
    let directory = tempdir().expect("project");
    initialize_git_repository(directory.path());
    save_config(directory.path(), &mock_config()).expect("mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let goal = "exact run steering tool smoke compression race";
    let mut session = store.create_session_with_id("steering-compression-race");
    let mut plan = pending_plan(goal, goal);
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");
    let run_id = "44444444444444444444444444444444";
    let (steering, steering_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("steering channel");
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(1);
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

    tokio::time::timeout(HOSTED_PROGRESS_TIMEOUT, async {
        loop {
            match stream_rx.recv().await {
                Some(StreamEvent::ToolCompleted { tool_name, .. })
                    if tool_name == "run_terminal" =>
                {
                    break
                }
                None => panic!("stream closed before the first terminal tool completed"),
                _ => {}
            }
        }
    })
    .await
    .expect("first tool completed");
    tokio::time::timeout(HOSTED_PROGRESS_TIMEOUT, async {
        loop {
            if store
                .load(&session.id)
                .expect("compression state")
                .events
                .iter()
                .any(|event| {
                    event.kind == "state_transition"
                        && event.details["to"] == AgentState::Compression.as_str()
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("agent is blocked after BuildContext drain at Compression transition");
    steering
        .submit("replacement steering marker after BuildContext drain")
        .expect("late pre-request steering is durable");
    let drain = tokio::spawn(async move { while stream_rx.recv().await.is_some() {} });

    let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("compression-race run completed")
        .expect("agent task joined")
        .expect("agent run succeeded");
    drain.await.expect("stream drain joined");
    assert_eq!(
        summary.last_message.as_deref(),
        Some("Final answer: replacement steering marker observed.")
    );
    let persisted = store.load(&session.id).expect("steered session");
    assert!(persisted.events.iter().any(|event| {
        event.kind == "provider_continuation_abandoned_by_steering"
            && event.details["run_id"] == run_id
    }));
    assert!(persisted
        .events
        .iter()
        .any(|event| event.kind == "steering_intake"));
}

#[tokio::test]
#[serial_test::serial]
async fn exact_run_steering_reserves_the_final_turn_instead_of_automatic_compression() {
    let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
    let directory = tempdir().expect("project");
    let mut config = NibConfig {
        llm: mock_config(),
        ..NibConfig::default()
    };
    config.compression.enabled = true;
    config.compression.threshold = 0.000_1;
    config.compression.target_ratio = 0.000_05;
    save_nib_config_full(directory.path(), &mut config).expect("compression config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let goal = "exact run steering final turn smoke compression threshold";
    let mut session = store.create_session_with_id("steering-final-turn-compression");
    let mut plan = pending_plan(goal, "use the final model turn for the task");
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");
    for (role, text) in [
        (
            "user",
            "historic user context that exceeds the tiny threshold",
        ),
        (
            "assistant",
            "historic assistant response retained for compression",
        ),
        (
            "user",
            "second historic user context for an eligible summary",
        ),
        (
            "assistant",
            "second historic assistant response before the active turn",
        ),
    ] {
        store
            .try_append_message(&session.id, role, text)
            .expect("historic message");
    }
    let run_id = "55555555555555555555555555555555";
    let (steering, steering_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("steering channel");
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(1);
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

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store
                .load(&session.id)
                .expect("build state")
                .events
                .iter()
                .any(|event| {
                    event.kind == "state_transition"
                        && event.details["to"] == AgentState::BuildContext.as_str()
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("run blocks before the BuildContext steering drain");
    steering
        .submit("replacement steering marker on the reserved final turn")
        .expect("pre-request steering accepted");
    let drain = tokio::spawn(async move { while stream_rx.recv().await.is_some() {} });

    let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("reserved final turn completed")
        .expect("agent task joined")
        .expect("agent run succeeded");
    drain.await.expect("stream drain joined");
    assert_eq!(
        summary.last_message.as_deref(),
        Some("Final answer: replacement steering marker observed.")
    );
    let persisted = store.load(&session.id).expect("terminal session");
    assert!(persisted
        .events
        .iter()
        .any(|event| event.kind == "steering_intake"));
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "compression"));
}

#[tokio::test]
#[serial_test::serial]
async fn exact_run_steering_supersedes_an_unapproved_plan_before_prompting() {
    let _interactive_smoke = EnvironmentGuard::set("NIB_ENABLE_INTERACTIVE_SMOKE", "1");
    let directory = tempdir().expect("project");
    save_config(directory.path(), &mock_config()).expect("mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let session = store.create_session_with_id("steering-plan-supersession");
    let goal = "interactive queue smoke with a steerable plan";
    let run_id = "abcdef0123456789abcdef0123456789";
    let (steering, steering_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
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
                approval_handler: Some(Arc::new(DenyApproval)),
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
                    Some(StreamEvent::StateTransition { state }) if state == AgentState::Planning.as_str()
                ) {
                    break;
                }
            }
        })
        .await
        .expect("planner request started");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    steering
        .submit("regenerate the plan with the new verification constraint")
        .expect("durable plan steering");

    let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("replanned run completed")
        .expect("agent task joined")
        .expect("agent run succeeded");
    assert_ne!(summary.outcome, "plan_approval_denied");
    let persisted = store.load(&session.id).expect("replanned session");
    assert_eq!(
        persisted
            .events
            .iter()
            .filter(|event| event.kind == "plan_generated")
            .count(),
        2
    );
    assert!(
        !persisted
            .events
            .iter()
            .any(|event| { event.kind == "approval_required" && event.details["kind"] == "plan" }),
        "plans are printed and auto-approved; they must not wait for Y/N"
    );
    assert!(persisted
        .events
        .iter()
        .any(|event| event.kind == "plan_superseded_by_steering"));
    assert!(persisted
        .events
        .iter()
        .any(|event| event.kind == "steering_intake"));
}

#[tokio::test]
async fn different_goal_invalidates_approved_plan_before_generating_a_new_one() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let mut session = store.create_session_with_id("replace-plan-goal");
    let mut plan = pending_plan("old goal", "perform old work");
    plan.approve();
    let old_plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).expect("old approved plan");

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "explore the project",
        AgentLoopConfig {
            max_steps: 6,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .expect("replacement plan run");

    assert_eq!(summary.outcome, "completed");
    assert!(summary.trace.contains(&"planning".to_string()));
    assert!(summary.trace.contains(&"plan_approval".to_string()));
    let persisted = store.load(&session.id).expect("replanned session");
    let replacement = persisted.plan.as_ref().expect("replacement plan");
    assert_ne!(replacement.id, old_plan_id);
    assert_eq!(replacement.goal, "explore the project");
    let invalidated = persisted
        .events
        .iter()
        .find(|event| event.kind == "plan_invalidated")
        .expect("plan invalidation audit");
    assert_eq!(invalidated.details["reason"], "goal_mismatch");
    assert_eq!(invalidated.details["previous_plan_id"], old_plan_id);
}

#[tokio::test]
async fn denied_plan_has_no_tool_side_effects() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "explore RAW_APPROVAL_LIFECYCLE_SENTINEL",
        AgentLoopConfig {
            max_steps: 4,
            approval_handler: Some(Arc::new(DenyApproval)),
            stream_tx: Some(stream_tx),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_ne!(summary.outcome, "plan_approval_denied");
    let persisted = store.load(&session.id).unwrap();
    assert!(persisted.plan.as_ref().is_some_and(|plan| plan.approved));
    assert!(!persisted
        .events
        .iter()
        .any(|event| { event.kind == "approval_required" && event.details["kind"] == "plan" }));
    let stream = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(stream
        .iter()
        .any(|event| matches!(event, StreamEvent::PlanGenerated { .. })));
    assert!(!stream.iter().any(|event| matches!(
        event,
        StreamEvent::ApprovalRequired { tool_name } if tool_name == "approve_plan"
    )));
    assert!(!format!("{stream:?}").contains("RAW_APPROVAL_LIFECYCLE_SENTINEL"));
}

#[tokio::test]
async fn due_profile_maintenance_is_mirrored_into_originating_session() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "explore the project",
        AgentLoopConfig {
            max_steps: 1,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.tool_call_count, 0);
    let loaded = store.load(&session.id).expect("originating session");
    let daemon_calls = loaded
        .tool_calls
        .iter()
        .filter(|call| call.tool_name.as_deref() == Some("daemon_curator"))
        .collect::<Vec<_>>();
    assert_eq!(daemon_calls.len(), 1);
    let call = daemon_calls[0];
    assert_eq!(call.session_id.as_deref(), Some(session.id.as_str()));
    assert_eq!(call.provider.as_deref(), Some("internal-daemon"));
    assert_eq!(call.arguments["profile_id"], "default");
    assert_eq!(
        call.arguments["policy_decision"],
        "destructive_cleanup_not_authorized"
    );
    assert_eq!(
        call.arguments["policy_source"],
        "daemons.allow_destructive_cleanup"
    );
    assert_eq!(call.result.as_ref().unwrap()["status"], "completed");
    assert_eq!(call.result.as_ref().unwrap()["deleted"], 0);
    assert!(call.error.is_none());
    assert!(call
        .duration_seconds
        .is_some_and(|duration| duration >= 0.0));
}

#[tokio::test]
async fn configured_turn_bound_reconciles_before_execution() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "explore the project",
        AgentLoopConfig {
            max_steps: 1,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert!(summary.bound_reached);
    assert_eq!(summary.outcome, "turn_limit_reached");
    assert_eq!(summary.tool_call_count, 0);
    assert!(!summary.trace.contains(&"tool_execute".to_string()));
}

#[tokio::test]
async fn question_handler_resumes_the_same_running_loop() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "ask a question before continuing",
        AgentLoopConfig {
            max_steps: 5,
            auto_approve: true,
            question_handler: Some(Arc::new(AnswerQuestion)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome, "completed");
    assert!(summary
        .trace
        .contains(&"waiting_for_user_input".to_string()));
    let loaded = store.load(&session.id).unwrap();
    assert!(loaded.messages.iter().any(|message| {
        message.role == "tool" && message.content.contains("\"answer\":\"full\"")
    }));
    loaded.validate_message_sequence().unwrap();
}

#[tokio::test]
async fn recovered_question_continues_the_exact_plan_with_answer_only_on_or_off() {
    for answer_only in [false, true] {
        let dir = tempdir().unwrap();
        let mut config = NibConfig {
            llm: mock_config(),
            ..NibConfig::default()
        };
        config.agent.answer_only = answer_only;
        config.skills.enabled = false;
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        save_nib_config_full(dir.path(), &mut config).expect("mock config");
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        let goal = "ask a question before continuing";

        let first = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            goal,
            AgentLoopConfig {
                max_steps: 5,
                auto_approve: true,
                // Seed a plan-first question; the continuation below exercises
                // both answer-route settings against that persisted plan.
                interactive_request: false,
                ..Default::default()
            },
        )
        .await
        .expect("initial question run");
        assert_eq!(first.outcome, "waiting_for_user_input");
        let waiting = store.load(&session.id).expect("waiting session");
        let plan_id = waiting.plan.as_ref().expect("persisted plan").id.clone();
        let question = waiting
            .clarifications
            .iter()
            .find(|record| record.status != ClarificationStatus::Answered)
            .expect("unresolved question");
        crate::interactive::persist_recovered_question_answer(
            &store,
            &session.id,
            &question.invocation_id.to_string(),
            "full",
        )
        .expect("recover exact answer");

        let continued = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            goal,
            AgentLoopConfig {
                max_steps: 5,
                auto_approve: true,
                interactive_request: true,
                continuation_plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("exact-plan continuation");
        assert_eq!(continued.outcome, "completed");
        let persisted = store.load(&session.id).expect("continued session");
        assert_eq!(
            persisted.plan.as_ref().map(|plan| plan.id.as_str()),
            Some(plan_id.as_str())
        );
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| event.kind == "plan_continue_requested")
                .count(),
            1
        );
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| event.kind == "human_question_answer_received")
                .count(),
            1
        );
        assert_eq!(
            persisted
                .tool_calls
                .iter()
                .filter(|call| call.tool_name.as_deref() == Some("list_directory"))
                .count(),
            1,
            "dependent continuation work must execute exactly once"
        );
        assert!(persisted
            .human_intent
            .iter()
            .any(|intent| { intent.kind == HumanIntentKind::Continue && intent.text == plan_id }));
        assert!(persisted.message_provenance.iter().any(|provenance| {
            provenance.origin == MessageOrigin::RuntimeContinuation
                && persisted.messages[provenance.message_index]
                    .content
                    .starts_with("Continue with approved plan step:")
        }));
        persisted.validate_message_sequence().unwrap();
    }
}

#[tokio::test]
async fn malformed_continuation_fails_closed_without_replanning_or_intent() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let mut session = store.create_session();
    let mut plan = pending_plan("continue the exact malformed plan", "invalid step");
    plan.approve();
    plan.steps[0].description.clear();
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    store
        .save(&mut session)
        .expect("malformed continuation fixture");

    let error = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "continue the exact malformed plan",
        AgentLoopConfig {
            max_steps: 5,
            auto_approve: true,
            interactive_request: true,
            continuation_plan_id: Some(plan_id.clone()),
            ..Default::default()
        },
    )
    .await
    .expect_err("malformed continuation must fail closed");
    assert!(error.contains("malformed"), "{error}");

    let persisted = store
        .load(&session.id)
        .expect("rejected continuation session");
    assert_eq!(
        persisted.plan.as_ref().map(|plan| plan.id.as_str()),
        Some(plan_id.as_str())
    );
    assert!(persisted
        .plan
        .as_ref()
        .is_some_and(|plan| { plan.steps[0].description.is_empty() }));
    assert!(!persisted.events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "plan_generated" | "plan_continue_requested"
        )
    }));
    assert!(!persisted
        .human_intent
        .iter()
        .any(|intent| intent.kind == HumanIntentKind::Continue));
}

#[tokio::test]
async fn mixed_question_batch_is_rejected_before_any_side_effect() {
    let dir = tempdir().unwrap();
    initialize_git_repository(dir.path());
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "mixed question batch",
        AgentLoopConfig {
            max_steps: 5,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome, "blocked_step_unresolved");
    assert!(summary.is_failure());
    assert_eq!(summary.tool_call_count, 0);
    assert!(!dir.path().join("mixed-side-effect.txt").exists());
    let loaded = store.load(&session.id).unwrap();
    assert!(loaded
        .events
        .iter()
        .any(|event| event.kind == "tool_batch_rejected"));
    assert!(loaded
        .tool_calls
        .iter()
        .all(|call| call.tool_name.as_deref() != Some("run_terminal")));
    // T081: the rejected batch interrupted the run, so its plan is cleared.
    assert!(loaded.plan.is_none());
    assert!(loaded.events.iter().any(|event| {
        event.kind == "plan_invalidated" && event.details["reason"] == "interrupted"
    }));
    loaded.validate_message_sequence().unwrap();
}

#[tokio::test]
async fn failed_terminal_observation_cannot_be_resolved_by_model_text_alone() {
    let dir = tempdir().unwrap();
    initialize_git_repository(dir.path());
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "recover from terminal failure",
        AgentLoopConfig {
            max_steps: 5,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome, "blocked_step_unresolved");
    assert!(summary.is_failure());
    assert_eq!(summary.tool_call_count, 1);
    assert!(summary
        .trace
        .windows(2)
        .any(|states| states == ["tool_execute", "build_context"]));
    let loaded = store.load(&session.id).unwrap();
    assert!(loaded.messages.iter().any(|message| {
        message.role == "tool" && message.content.contains("recoverable stderr")
    }));
    let terminal = loaded
        .tool_calls
        .iter()
        .find(|call| call.tool_name.as_deref() == Some("run_terminal"))
        .expect("terminal audit");
    assert!(terminal.error.as_deref().is_some_and(|error| {
        error.contains("recoverable stderr") && error.contains("command exited with 7")
    }));
    // T081: the blocked plan is cleared after the run and audited.
    assert!(loaded.plan.is_none());
    assert!(loaded.events.iter().any(|event| {
        event.kind == "plan_invalidated"
            && event.details["reason"] == "interrupted"
            && event.details["plan_outcome"] == "blocked_step_unresolved"
    }));
    assert!(loaded.events.iter().any(|event| {
        event.kind == "step_completion_rejected"
            && event.details["reason"] == "blocked_step_unresolved"
    }));
    loaded.validate_message_sequence().unwrap();
}

#[tokio::test]
async fn classifier_safe_terminal_does_not_claim_tool_approval_is_pending() {
    let dir = tempdir().unwrap();
    initialize_git_repository(dir.path());
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let session = store.create_session();
    let approval_calls = Arc::new(AtomicUsize::new(0));
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "safe terminal approval",
        AgentLoopConfig {
            max_steps: 5,
            approval_handler: Some(Arc::new(ApprovePlanOnly {
                calls: approval_calls.clone(),
            })),
            stream_tx: Some(stream_tx),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome, "completed");
    assert_eq!(approval_calls.load(Ordering::SeqCst), 0);
    let loaded = store.load(&session.id).unwrap();
    assert!(!loaded
        .events
        .iter()
        .any(|event| { event.kind == "approval_required" && event.details["kind"] == "tool" }));
    let terminal = loaded
        .tool_calls
        .iter()
        .find(|call| call.tool_name.as_deref() == Some("run_terminal"))
        .expect("terminal audit");
    assert_eq!(
        terminal.result.as_ref().unwrap()["approval"]["source"],
        "classifier"
    );
    let mut terminal_output = String::new();
    let mut saw_normal_end = false;
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(100), stream_rx.recv()).await {
            Ok(Some(StreamEvent::TerminalOutput { chunk, .. })) => {
                terminal_output.push_str(&chunk);
            }
            Ok(Some(StreamEvent::End(reason))) if reason == "completed" => {
                saw_normal_end = true;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(terminal_output.contains("ok"), "{terminal_output:?}");
    assert!(saw_normal_end, "normal completion must end the run stream");
}

#[tokio::test]
async fn concurrent_same_session_run_is_rejected_until_the_owner_releases_its_lease() {
    let directory = tempdir().unwrap();
    initialize_git_repository(directory.path());
    save_config(directory.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(directory.path()).unwrap();
    let session = store.create_session();
    let session_id = session.id.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let run_root = directory.path().to_path_buf();
    let run_session_id = session_id.clone();
    let run_entered = entered.clone();
    let run_release = release.clone();
    let first = tokio::spawn(async move {
        run_agent_loop(
            run_root,
            &run_session_id,
            "recover from terminal failure",
            AgentLoopConfig {
                max_steps: 6,
                approval_handler: Some(Arc::new(ControlledApproval {
                    entered: run_entered,
                    release: run_release,
                    granted: false,
                })),
                ..Default::default()
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .expect("first run reached tool approval");
    let before_conflict = store.load(&session_id).expect("blocked session");
    let conflict = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        run_agent_loop(
            directory.path().to_path_buf(),
            &session_id,
            "replace the active run",
            AgentLoopConfig {
                max_steps: 2,
                auto_approve: true,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("concurrent run failed closed promptly")
    .expect_err("concurrent run must not acquire the lease");
    assert!(
        conflict.contains("active agent run"),
        "unexpected conflict: {conflict}"
    );
    let after_conflict = store.load(&session_id).expect("unchanged blocked session");
    assert_eq!(after_conflict.messages, before_conflict.messages);
    assert_eq!(after_conflict.plan, before_conflict.plan);
    assert_eq!(after_conflict.events, before_conflict.events);
    assert_eq!(after_conflict.tool_calls, before_conflict.tool_calls);

    release.notify_one();
    let first_summary = tokio::time::timeout(std::time::Duration::from_secs(10), first)
        .await
        .expect("first run completed after release")
        .expect("first run joined")
        .expect("first run reconciled");
    assert_eq!(first_summary.outcome, "tool_execution_failed");

    let lease = store
        .try_acquire_run_lease(&session_id)
        .expect("lease is available after the owner finishes");
    lease.verify().expect("released lease is valid");
}

#[tokio::test]
async fn generated_plans_are_printed_and_auto_approved() {
    let directory = tempdir().unwrap();
    save_config(directory.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(directory.path()).unwrap();
    let session = store.create_session();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);

    let summary = run_agent_loop(
        directory.path().to_path_buf(),
        &session.id,
        "explore the project",
        AgentLoopConfig {
            max_steps: 4,
            approval_handler: Some(Arc::new(DenyApproval)),
            stream_tx: Some(stream_tx),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_ne!(summary.outcome, "plan_approval_denied");
    let persisted = store.load(&session.id).expect("auto-approved session");
    let plan = persisted.plan.expect("structured plan");
    assert!(plan.approved);
    let approved = persisted
        .events
        .iter()
        .find(|event| event.kind == "plan_approved")
        .expect("plan_approved");
    assert_eq!(approved.details["auto"], true);
    assert!(!persisted
        .events
        .iter()
        .any(|event| { event.kind == "approval_required" && event.details["kind"] == "plan" }));
    let stream = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(stream
        .iter()
        .any(|event| matches!(event, StreamEvent::PlanGenerated { .. })));
    assert!(!stream.iter().any(|event| matches!(
        event,
        StreamEvent::ApprovalRequired { tool_name } if tool_name == "approve_plan"
    )));
}

#[tokio::test]
async fn cancellation_interrupts_blocked_approval_and_reconciles_the_session() {
    let directory = tempdir().unwrap();
    initialize_git_repository(directory.path());
    save_config(directory.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(directory.path()).unwrap();
    let session = store.create_session();
    let session_id = session.id.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let cancellation = CancellationSignal::new();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);
    let run_root = directory.path().to_path_buf();
    let run_session_id = session_id.clone();
    let run_entered = entered.clone();
    let run_cancellation = cancellation.clone();
    let run = tokio::spawn(async move {
        run_agent_loop(
            run_root,
            &run_session_id,
            "recover from terminal failure",
            AgentLoopConfig {
                max_steps: 6,
                approval_handler: Some(Arc::new(BlockingApproval {
                    entered: run_entered,
                })),
                stream_tx: Some(stream_tx),
                cancellation: Some(run_cancellation),
                ..Default::default()
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .expect("agent reached blocked approval");
    let messages_before_cancel = store
        .load(&session_id)
        .expect("session before cancellation")
        .messages;
    assert!(cancellation.cancel());
    let summary = tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("cancelled run stopped promptly")
        .expect("agent task joined")
        .expect("cancelled run reconciled");

    assert_eq!(summary.outcome, "cancelled_by_user");
    assert_eq!(summary.final_state, AgentState::Done);
    assert!(summary.trace.ends_with(&[
        AgentState::Reconciliation.as_str().to_string(),
        AgentState::Done.as_str().to_string(),
    ]));
    let persisted = store.load(&session_id).expect("cancelled session");
    assert!(summary.last_message.is_none());
    assert_eq!(persisted.messages, messages_before_cancel);
    // T081: an interrupted plan is cleared and audited instead of kept.
    assert!(persisted.plan.is_none());
    assert!(persisted.events.iter().any(|event| {
        event.kind == "plan_invalidated"
            && event.details["reason"] == "interrupted"
            && event.details["outcome"] == "cancelled_by_user"
            && event.details["plan_outcome"] == "cancelled_by_user"
    }));
    persisted.validate_message_sequence().unwrap();
    let cancellation_event = persisted
        .events
        .iter()
        .position(|event| event.kind == "cancel_requested")
        .expect("cancellation audit");
    let reconciliation = persisted
        .events
        .iter()
        .position(|event| {
            event.kind == "reconciliation" && event.details["outcome"] == "cancelled_by_user"
        })
        .expect("cancellation reconciliation");
    let done = persisted
        .events
        .iter()
        .position(|event| event.kind == "state_transition" && event.details["to"] == "done")
        .expect("terminal transition");
    assert!(cancellation_event < reconciliation && reconciliation < done);

    let mut streamed = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        streamed.push(event);
    }
    let reconciled = streamed
        .iter()
        .position(|event| {
            matches!(
                event,
                StreamEvent::Reconciled { outcome } if outcome == "cancelled_by_user"
            )
        })
        .expect("reconciled stream event");
    let ended = streamed
        .iter()
        .position(
            |event| matches!(event, StreamEvent::End(reason) if reason == "cancelled_by_user"),
        )
        .expect("terminal stream event");
    assert!(reconciled < ended);
}

#[tokio::test]
async fn cancellation_marks_running_verification_cancelled_without_passing_it() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::new(directory.path());
    let mut session = store.create_session_with_id("cancel-running-verification");
    let mut plan = pending_plan("verify before completion", "run the required check");
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
    plan.begin_verification(
        "required-check",
        crate::tools::ToolInvocationId::new(),
        "run_terminal",
        &json!({"command": "true", "affected_paths": ["src/"]}),
        Some("worktree-a".to_string()),
    )
    .expect("start required verification");
    plan.approve();
    session.plan = Some(plan);
    store
        .save(&mut session)
        .expect("persist running verification");

    let summary = reconcile_cancelled_run(&store, &session.id, &None)
        .await
        .expect("reconcile cancellation");

    assert_eq!(summary.outcome, "cancelled_by_user");
    let persisted = store.load(&session.id).expect("cancelled session");
    let obligation = &persisted.plan.as_ref().expect("plan").steps[0].verification_obligations[0];
    assert_eq!(
        obligation.status,
        crate::session::VerificationStatus::Cancelled
    );
    assert_eq!(
        obligation.reason.as_deref(),
        Some("agent run cancelled by user")
    );
    let event = persisted
        .events
        .iter()
        .find(|event| event.kind == "verification_cancelled")
        .expect("verification cancellation audit");
    assert_eq!(event.details["verification_ids"], json!(["required-check"]));
}

#[test]
fn question_projection_requires_registry_schema_and_is_control_safe_and_bounded() {
    let invalid = json!({
        "question": "choose",
        "options": (0..21).map(|index| format!("option-{index}")).collect::<Vec<_>>()
    });
    assert!(
        crate::tools::executor::validate_registered_tool_arguments("ask_question", &invalid,)
            .is_err()
    );

    let secret = "active/credential".to_string();
    let valid = json!({
        "question": format!("mode {secret} active\\/credential\n\u{1b}[31m?"),
        "options": ["YWN0aXZlL2NyZWRlbnRpYWw=\r", "execute\t"]
    });
    crate::tools::executor::validate_registered_tool_arguments("ask_question", &valid)
        .expect("registry-valid question");
    let projected = safe_question_arguments(&valid, std::slice::from_ref(&secret));
    let encoded = serde_json::to_string(&projected).expect("safe question projection");
    assert!(!projected["question"]
        .as_str()
        .expect("question")
        .chars()
        .any(char::is_control));
    assert!(!encoded.contains(&secret));
    assert!(!encoded.contains(r"active\/credential"));
    assert!(!encoded.contains("YWN0aXZlL2NyZWRlbnRpYWw="));
    assert!(!encoded.contains("\\u001b"));
    assert!(encoded.len() < MAX_QUESTION_BYTES + 2 * MAX_QUESTION_OPTION_BYTES);
}

#[tokio::test]
async fn question_execution_and_audit_persist_only_the_public_projection() {
    let directory = tempdir().expect("question audit directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let session = store.create_session_with_id("question-audit");
    let secret = "active/credential".to_string();
    let raw_arguments = json!({
        "question": format!(
            "{secret} active\\/credential YWN0aXZlL2NyZWRlbnRpYWw= [2J {} QUESTION_PRIVATE_TAIL",
            "q".repeat(MAX_QUESTION_BYTES + 512),
        ),
        "options": [
            "active\\/credential\r",
            "YWN0aXZlL2NyZWRlbnRpYWw=\t",
        ],
    });
    let answer = Err(format!(
            "handler failed with {secret} active\\/credential YWN0aXZlL2NyZWRlbnRpYWw= [31m {} ANSWER_PRIVATE_TAIL",
            "e".repeat(MAX_QUESTION_BYTES + 512),
        ));
    let arguments =
        safe_question_execution_arguments(&raw_arguments, &answer, std::slice::from_ref(&secret));
    let mut executor = ToolExecutor::new(
        directory.path().to_path_buf(),
        crate::config::ExecutionConfig::default(),
    )
    .with_session_store(store.clone())
    .with_sensitive_values([secret.clone()])
    .with_auto_approve(true);

    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "ask_question".to_string(),
                arguments,
                session_id: Some(session.id.clone()),
                project_root: Some(directory.path().to_path_buf()),
            },
            Some(&session.id),
        )
        .await;

    assert!(!result.success);
    let persisted = store.load(&session.id).expect("question audit session");
    assert!(persisted
        .events
        .iter()
        .any(|event| event.kind == "tool_attempted"));
    assert_eq!(persisted.tool_calls.len(), 1);
    let audited_arguments = &persisted.tool_calls[0].arguments;
    assert!(audited_arguments["question"]
        .as_str()
        .is_some_and(|value| value.len() <= MAX_QUESTION_BYTES));
    assert!(audited_arguments["answer_error"]
        .as_str()
        .is_some_and(|value| value.len() <= MAX_QUESTION_BYTES));
    let public_value = json!({
        "output": result.output,
        "error": result.error,
        "session": persisted,
    });
    let public_surface =
        serde_json::to_string(&public_value).expect("serialize public question surfaces");
    for forbidden in [
        secret.as_str(),
        r"active\/credential",
        "YWN0aXZlL2NyZWRlbnRpYWw=",
        "QUESTION_PRIVATE_TAIL",
        "ANSWER_PRIVATE_TAIL",
        r"\u001b",
    ] {
        assert!(
            !public_surface.contains(forbidden),
            "public question surface contained {forbidden:?}"
        );
    }
    fn assert_public_strings_are_bounded(value: &Value) {
        match value {
            Value::Object(object) => {
                for value in object.values() {
                    assert_public_strings_are_bounded(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    assert_public_strings_are_bounded(value);
                }
            }
            Value::String(value) => {
                assert!(value.len() <= MAX_PERSISTED_PROVIDER_MESSAGE_BYTES)
            }
            _ => {}
        }
    }
    assert_public_strings_are_bounded(&public_value);
}

/// T081: which terminal outcomes clear an unfinished plan.
#[test]
fn interrupted_outcomes_clear_unfinished_plans_but_waiting_plans_survive() {
    for outcome in [
        "tool_execution_failed",
        "blocked_step_unresolved",
        "cancelled_by_user",
        "local_error",
        "turn_limit_reached",
        "plan_approval_denied",
        "llm_stream_failed: timeout",
    ] {
        assert!(clears_unfinished_plan(outcome), "{outcome}");
    }
    for outcome in [
        "completed",
        "plan_ready",
        "step_completed",
        "unresolved_clarification",
        "waiting_for_user_input",
        "provider_continuation_interrupted",
        "planning_required_active_plan",
        "planning_required_active_run",
    ] {
        assert!(!clears_unfinished_plan(outcome), "{outcome}");
    }

    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut session = store.create_session();
    let mut plan = pending_plan("keep waiting plan", "ask the user");
    plan.approve();
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    assert!(!clear_interrupted_plan_in_session(
        &mut session,
        "unresolved_clarification",
        None
    ));
    assert_eq!(
        session.plan.as_ref().map(|plan| plan.id.clone()),
        Some(plan_id.clone())
    );
    assert!(clear_interrupted_plan_in_session(
        &mut session,
        "tool_execution_failed",
        Some("run-1")
    ));
    assert!(session.plan.is_none());
    let event = session
        .events
        .iter()
        .find(|event| event.kind == "plan_invalidated")
        .expect("audit event");
    assert_eq!(event.details["reason"], "interrupted");
    assert_eq!(event.details["outcome"], "tool_execution_failed");
    assert_eq!(event.details["run_id"], "run-1");
    assert_eq!(event.details["previous_plan_id"], plan_id.as_str());
}

/// T081 AC-1: a plan left Blocked by an earlier run (the session shape that
/// trapped users in "Existing plan is still open") is cleared at the next run,
/// and the next chat request is not rejected by the planning gate.
#[tokio::test]
async fn blocked_plan_from_an_earlier_run_does_not_trap_the_next_request() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let mut session = store.create_session();
    let mut plan = pending_plan("review repo", "inspect and validate");
    plan.approve();
    plan.outcome = Some("tool_execution_failed".to_string());
    plan.steps[0].status = "Blocked".to_string();
    plan.steps[0].outcome = Some("tool_execution_failed".to_string());
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).expect("blocked plan fixture");

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "continue",
        AgentLoopConfig {
            max_steps: 5,
            auto_approve: true,
            interactive_request: true,
            ..Default::default()
        },
    )
    .await
    .expect("next request runs");
    assert_ne!(summary.outcome, "planning_required_active_plan");

    let persisted = store.load(&session.id).expect("session");
    assert!(persisted
        .plan
        .as_ref()
        .is_none_or(|plan| plan.id != plan_id));
    assert!(persisted.events.iter().any(|event| {
        event.kind == "plan_invalidated"
            && event.details["reason"] == "interrupted"
            && event.details["outcome"] == "tool_execution_failed"
            && event.details["previous_plan_id"] == plan_id.as_str()
    }));
}

fn t081_plan(outcome: Option<&str>) -> crate::session::Plan {
    let mut plan = pending_plan("finish the original plan", "do the original work");
    plan.approve();
    if let Some(outcome) = outcome {
        plan.outcome = Some(outcome.to_string());
        plan.steps[0].status = "Blocked".to_string();
        plan.steps[0].outcome = Some(outcome.to_string());
    }
    plan
}

async fn t081_request(dir: &std::path::Path, session_id: &str, goal: &str) -> AgentRunSummary {
    run_agent_loop(
        dir.to_path_buf(),
        session_id,
        goal,
        AgentLoopConfig {
            max_steps: 5,
            auto_approve: true,
            interactive_request: true,
            ..Default::default()
        },
    )
    .await
    .expect("request runs")
}

/// T081 M1: the fixture really reaches the planning gate when an unfinished
/// plan that was not interrupted stays open, and the same request is not
/// trapped once the plan's interruption is recorded.
#[tokio::test]
async fn planning_gate_fires_for_open_plans_and_not_for_interrupted_ones() {
    let goal = crate::llm::mock::PLANNING_GATE_FIXTURE_GOAL;
    for (outcome, gated) in [(None, true), (Some("tool_execution_failed"), false)] {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let mut session = store.create_session();
        let plan = t081_plan(outcome);
        let plan_id = plan.id.clone();
        session.plan = Some(plan);
        store.save(&mut session).unwrap();

        let summary = t081_request(dir.path(), &session.id, goal).await;
        let persisted = store.load(&session.id).unwrap();
        assert_eq!(
            summary.outcome == "planning_required_active_plan",
            gated,
            "{outcome:?}: {}",
            summary.outcome
        );
        let kept = persisted
            .plan
            .as_ref()
            .is_some_and(|plan| plan.id == plan_id);
        assert_eq!(kept, gated, "{outcome:?}");
    }
}

/// T081 AC-3 / M2: a plan waiting on a question survives a cancelled side
/// request and a later request; the question stays answerable.
#[tokio::test]
async fn waiting_plan_survives_cancelled_and_gated_side_requests() {
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let mut session = store.create_session();
    let plan = t081_plan(None);
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    let invocation_id = crate::tools::ToolInvocationId::new();
    let question_event_index = session.events.len();
    session.events.push(crate::session::SessionEvent {
        index: question_event_index,
        kind: "question_required".to_string(),
        details: serde_json::json!({
            "invocation_id": invocation_id,
            "question": "Which target?",
            "options": ["alpha", "beta"],
        }),
        timestamp: Some(chrono::Utc::now()),
    });
    session
        .clarifications
        .push(crate::session::ClarificationRecord {
            invocation_id,
            plan_id: Some(plan_id.clone()),
            question: "Which target?".to_string(),
            options: vec!["alpha".to_string(), "beta".to_string()],
            status: crate::session::ClarificationStatus::Unresolved,
            question_event_index,
            reason: Some("left unanswered".to_string()),
            outcome: Some("left_unanswered".to_string()),
            ..Default::default()
        });
    store.save(&mut session).unwrap();

    let cancellation = CancellationSignal::new();
    cancellation.cancel();
    let cancelled = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        "a side question",
        AgentLoopConfig {
            max_steps: 5,
            auto_approve: true,
            interactive_request: true,
            cancellation: Some(cancellation),
            ..Default::default()
        },
    )
    .await
    .expect("cancelled side request");
    assert_eq!(cancelled.outcome, "cancelled_by_user");

    let gated = t081_request(
        dir.path(),
        &session.id,
        crate::llm::mock::PLANNING_GATE_FIXTURE_GOAL,
    )
    .await;
    assert_eq!(gated.outcome, "planning_required_active_plan");

    let persisted = store.load(&session.id).unwrap();
    assert_eq!(
        persisted.plan.as_ref().map(|plan| plan.id.as_str()),
        Some(plan_id.as_str())
    );
    assert!(persisted.has_unresolved_clarification(Some(&plan_id)));
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "plan_invalidated"));
}

/// T081: only the run that bound a plan can clear it.
#[test]
fn clearing_requires_the_plan_bound_by_the_run() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut session = store.create_session();
    let plan = t081_plan(None);
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).unwrap();

    assert!(!clear_interrupted_plan_after_run(
        &store,
        &session.id,
        "run-a",
        "tool_execution_failed"
    )
    .unwrap());
    record_run_plan_binding(&store, &session.id, "run-b", &plan_id).unwrap();
    assert!(!clear_interrupted_plan_after_run(
        &store,
        &session.id,
        "run-a",
        "tool_execution_failed"
    )
    .unwrap());
    assert!(!clear_interrupted_plan_after_run(
        &store,
        &session.id,
        "run-b",
        "plan_binding_changed"
    )
    .unwrap());
    assert!(clear_interrupted_plan_after_run(
        &store,
        &session.id,
        "run-b",
        "tool_execution_failed"
    )
    .unwrap());
    assert!(store.load(&session.id).unwrap().plan.is_none());
}

/// T081 AC-4 / M2: the model receives the rejection reason and the declared
/// verification ids, and the run is not ended by the rejection itself.
#[tokio::test]
async fn undeclared_verification_reaches_the_model_with_declared_ids() {
    let goal = crate::llm::mock::UNDECLARED_VERIFICATION_FIXTURE_GOAL;
    let dir = tempdir().unwrap();
    save_config(dir.path(), &mock_config()).unwrap();
    let store = SessionStore::for_project(dir.path()).unwrap();
    let mut session = store.create_session();
    let mut plan = pending_plan(goal, goal);
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).unwrap();

    let summary = run_agent_loop(
        dir.path().to_path_buf(),
        &session.id,
        goal,
        AgentLoopConfig {
            max_steps: 4,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .expect("run reconciles");
    assert_ne!(summary.outcome, "tool_execution_failed");

    let saved = store.load(&session.id).unwrap();
    let observation = saved
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .map(|message| message.content.clone())
        .find(|content| content.contains("verification binding was rejected"))
        .expect("provider-facing rejection");
    assert!(observation.contains("review-task-check"), "{observation}");
    assert!(observation.contains("declared: none"), "{observation}");
}
