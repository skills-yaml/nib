use super::*;

#[test]
fn completion_revalidation_stales_content_changed_outside_the_tool_audit() {
    let directory = tempdir().expect("verification root");
    std::fs::write(directory.path().join("note.txt"), "verified\n").expect("fixture");
    let store = SessionStore::new(&directory.path().join("sessions"));
    let mut session = store.create_session_with_id("content-revalidation");
    let obligation = VerificationObligation::pending_tool(
        "content-check",
        "verify note content",
        vec!["note.txt".to_string()],
        "run_terminal",
        json!({"command": "true", "affected_paths": ["note.txt"]}),
        VerificationExpectedOutcome::Success,
    )
    .expect("verification contract");
    let mut plan = crate::session::Plan::new(
        "verify content",
        vec![crate::session::PlanStep {
            description: "verify the note".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: vec![obligation],
            content_generation: 0,
        }],
    );
    plan.approve();
    let plan_id = plan.id.clone();
    let invocation_id = crate::tools::ToolInvocationId::new();
    let root = directory.path().to_string_lossy().into_owned();
    plan.begin_verification(
        "content-check",
        invocation_id,
        "run_terminal",
        &json!({"command": "true", "affected_paths": ["note.txt"]}),
        Some(root.clone()),
    )
    .expect("begin verification");
    let identity =
        compute_verification_content_identity(directory.path(), &["note.txt".to_string()])
            .expect("content identity");
    plan.finish_verification(
        "content-check",
        invocation_id,
        Some(&root),
        Some(identity),
        true,
        None,
    )
    .expect("finish verification");
    session.plan = Some(plan);
    store.save(&mut session).expect("persist evidence");

    std::fs::write(directory.path().join("note.txt"), "changed externally\n")
        .expect("external change");
    assert_eq!(
        revalidate_plan_verification_content(&store, &session.id, Some(&plan_id))
            .expect("revalidate"),
        ["content-check"]
    );
    let persisted = store.load(&session.id).expect("stale evidence");
    assert_eq!(
        persisted.plan.unwrap().steps[0].verification_obligations[0].status,
        crate::session::VerificationStatus::Stale
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn clarification_answer_and_unresolved_state_persist_with_sources() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::new(directory.path());
    let session = store.create_session_with_id("clarification-provenance");
    store
        .try_append_message_with_origin(
            &session.id,
            "user",
            "implement the selected mode",
            MessageOrigin::HumanRequest,
        )
        .expect("request");
    store
        .try_append_message_with_origin(
            &session.id,
            "assistant",
            "question intent",
            MessageOrigin::ModelOutput,
        )
        .expect("assistant intent");

    let first = crate::tools::ToolInvocationId::new();
    assert!(persist_question_required(
        &store,
        &session.id,
        Some("plan-a"),
        first,
        ClarificationPrompt {
            question: "Which verification mode?",
            proposed_answer: None,
            options: &["fast".to_string(), "full".to_string()],
            dependent_paths: &["src".to_string()],
        },
    )
    .expect("persist question")
    .is_none());
    persist_question_observation(
        &store,
        &session.id,
        first,
        &[json!({"tool": "ask_question", "success": true})],
        &QuestionOutcome::Answered("full".to_string()),
        false,
    )
    .expect("persist answer");

    let reloaded = store.load(&session.id).expect("reload answer");
    let answered = reloaded.clarifications.last().expect("clarification");
    assert_eq!(answered.status, ClarificationStatus::Answered);
    assert_eq!(answered.answer.as_deref(), Some("full"));
    assert!(answered.answer_message_index.is_none());
    let source = answered.answer_event_index.expect("answer source");
    assert_eq!(
        reloaded.events[source].kind,
        "human_question_answer_received"
    );
    assert_eq!(
        reloaded.message_origin(reloaded.messages.len() - 1),
        MessageOrigin::ToolOutput
    );
    assert_eq!(
        reloaded.human_intent.last().expect("human answer").text,
        "full"
    );

    store
        .try_append_message_with_origin(
            &session.id,
            "assistant",
            "same question attempted again",
            MessageOrigin::ModelOutput,
        )
        .expect("assistant retry");
    let second = crate::tools::ToolInvocationId::new();
    let reused = persist_question_required(
        &store,
        &session.id,
        Some("plan-a"),
        second,
        ClarificationPrompt {
            question: "Which verification mode?",
            proposed_answer: None,
            options: &["fast".to_string(), "full".to_string()],
            dependent_paths: &["src".to_string()],
        },
    )
    .expect("reuse question")
    .expect("prior answer");
    assert_eq!(reused.answer, "full");
    assert_eq!(reused.answer_event_index, Some(source));

    let third = crate::tools::ToolInvocationId::new();
    persist_question_required(
        &store,
        &session.id,
        Some("plan-a"),
        third,
        ClarificationPrompt {
            question: "Which target file?",
            proposed_answer: None,
            options: &[],
            dependent_paths: &[],
        },
    )
    .expect("new question");
    persist_question_observation(
        &store,
        &session.id,
        third,
        &[json!({"tool": "ask_question", "success": false})],
        &QuestionOutcome::InputUnavailable("input unavailable".to_string()),
        false,
    )
    .expect("persist unresolved");
    let unresolved = store.load(&session.id).expect("reload unresolved");
    assert!(unresolved.has_unresolved_clarification(Some("plan-a")));
    assert_eq!(
        unresolved.clarifications.last().expect("unresolved").status,
        ClarificationStatus::Unresolved
    );

    store
        .try_append_message_with_origin(
            &session.id,
            "assistant",
            "retry the unanswered question",
            MessageOrigin::ModelOutput,
        )
        .expect("assistant retry after unresolved input");
    let fourth = crate::tools::ToolInvocationId::new();
    assert!(persist_question_required(
        &store,
        &session.id,
        Some("plan-a"),
        fourth,
        ClarificationPrompt {
            question: "Which target file?",
            proposed_answer: None,
            options: &[],
            dependent_paths: &[],
        },
    )
    .expect("repeat unresolved question")
    .is_none());
    persist_question_observation(
        &store,
        &session.id,
        fourth,
        &[json!({"tool": "ask_question", "success": true})],
        &QuestionOutcome::Answered("src/lib.rs".to_string()),
        false,
    )
    .expect("persist later valid answer");
    let resolved = store.load(&session.id).expect("reload resolved records");
    assert!(!resolved.has_unresolved_clarification(Some("plan-a")));
    assert!(resolved.clarifications.iter().rev().take(2).all(|record| {
        record.status == ClarificationStatus::Answered
            && record.answer.as_deref() == Some("src/lib.rs")
    }));

    store
        .try_append_message_with_origin(
            &session.id,
            "assistant",
            "ask a materially different choice set",
            MessageOrigin::ModelOutput,
        )
        .expect("assistant changed options");
    let fifth = crate::tools::ToolInvocationId::new();
    assert!(persist_question_required(
        &store,
        &session.id,
        Some("plan-a"),
        fifth,
        ClarificationPrompt {
            question: "Which verification mode?",
            proposed_answer: None,
            options: &["fast".to_string(), "release".to_string()],
            dependent_paths: &["src".to_string()],
        },
    )
    .expect("changed option set")
    .is_none());
    persist_question_observation(
        &store,
        &session.id,
        fifth,
        &[json!({"tool": "ask_question", "success": false})],
        &QuestionOutcome::InputUnavailable("input unavailable".to_string()),
        false,
    )
    .expect("persist changed-option unresolved state");
    let reloaded = store.load(&session.id).expect("reload option identity");
    assert!(reloaded.has_unresolved_clarification(Some("plan-a")));
    reloaded.validate().expect("valid persisted provenance");
}

#[test]
fn unresolved_clarification_blocks_overlapping_scope_and_allows_disjoint_scope() {
    let session: Session = serde_json::from_value(json!({
        "id": "clarification-dependency",
        "events": [{"index": 0, "kind": "question_required", "details": {}}],
        "clarifications": [{
            "invocation_id": crate::tools::ToolInvocationId::new(),
            "plan_id": "plan-a",
            "question": "Which target?",
            "options": [],
            "dependent_paths": ["src"],
            "status": "unresolved",
            "question_event_index": 0,
            "reason": "input unavailable"
        }]
    }))
    .expect("session fixture");
    let root = Path::new("/workspace");
    let blocked = unresolved_clarification_dependencies(
        &session,
        Some("plan-a"),
        root,
        &[ToolCallRequest::new(
            "read_file",
            json!({"path": "src/lib.rs"}),
        )],
    )
    .expect("dependency check");
    assert_eq!(blocked.len(), 1);
    let independent = unresolved_clarification_dependencies(
        &session,
        Some("plan-a"),
        root,
        &[ToolCallRequest::new(
            "read_file",
            json!({"path": "docs/README.md"}),
        )],
    )
    .expect("independent check");
    assert!(independent.is_empty());
}

#[test]
fn unchanged_failure_batches_ignore_new_invocation_ids() {
    let mut guard = FailedToolBatchGuard::default();
    for expected in [false, false, true] {
        let (requests, observations) = failed_batch_fixture();
        assert_eq!(guard.observe(&requests, &observations, false), expected);
    }
}

#[test]
fn changed_failure_requests_or_results_reset_the_streak() {
    for changed_field in ["name", "arguments", "output", "error"] {
        let mut guard = FailedToolBatchGuard::default();
        let (mut requests, mut observations) = failed_batch_fixture();
        assert!(!guard.observe(&requests, &observations, false));
        assert!(!guard.observe(&requests, &observations, false));
        match changed_field {
            "name" => requests[0].name = "list_directory".to_string(),
            "arguments" => requests[0].arguments = json!({"path": "different.txt"}),
            "output" => observations[0]["output"] = json!({"progress": "new evidence"}),
            "error" => observations[0]["error"] = json!("different error"),
            _ => unreachable!(),
        }
        assert!(
            !guard.observe(&requests, &observations, false),
            "{changed_field}"
        );
        assert!(
            !guard.observe(&requests, &observations, false),
            "{changed_field}"
        );
        assert!(
            guard.observe(&requests, &observations, false),
            "{changed_field}"
        );
    }
}

#[test]
fn successful_work_even_in_a_partly_failed_batch_resets_the_streak() {
    for whole_batch_success in [true, false] {
        let mut guard = FailedToolBatchGuard::default();
        let (requests, observations) = failed_batch_fixture();
        assert!(!guard.observe(&requests, &observations, false));
        assert!(!guard.observe(&requests, &observations, false));
        let mut progressed = observations.clone();
        progressed.push(json!({"tool": "list_directory", "success": true}));
        assert!(!guard.observe(&requests, &progressed, whole_batch_success));
        assert!(!guard.observe(&requests, &observations, false));
        assert!(!guard.observe(&requests, &observations, false));
        assert!(guard.observe(&requests, &observations, false));
    }
}

#[test]
fn provider_stream_projection_uses_only_the_validated_response() {
    assert_eq!(
        project_validated_llm_response(&LlmResponse::text("safe model text"), &[]),
        vec![StreamEvent::Content("safe model text".to_string())]
    );
    let mut refused = LlmResponse::text("private refusal detail");
    refused.terminal_status = LlmTerminalStatus::Refused;
    refused.finish_reason = crate::llm::LlmFinishReason::Refusal;
    assert!(project_validated_llm_response(&refused, &[]).is_empty());

    let secret = "stream/output-secret".to_string();
    let projected = project_validated_llm_response(
        &LlmResponse::text(format!(
            "{secret} stream\\/output-secret c3RyZWFtL291dHB1dC1zZWNyZXQ= \u{1b}[2J"
        )),
        std::slice::from_ref(&secret),
    );
    let StreamEvent::Content(content) = &projected[0] else {
        panic!("validated text response must project as content")
    };
    assert!(!content.contains(&secret));
    assert!(!content.contains(r"stream\/output-secret"));
    assert!(!content.contains("c3RyZWFtL291dHB1dC1zZWNyZXQ"));
    assert!(!content.contains('\u{1b}'));

    let tool = ToolCallRequest::new("read_file", json!({"path": "README.md"}));
    let invocation_id = tool.invocation_id;
    let projected = project_validated_llm_response(&LlmResponse::with_tools(vec![tool]), &[]);
    assert!(matches!(
        projected.as_slice(),
        [StreamEvent::ToolCallChunk {
            invocation_id: projected_id,
            name: Some(name),
            ..
        }] if *projected_id == invocation_id && name == "read_file"
    ));
}

#[test]
fn validated_provider_content_is_sanitized_before_session_persistence() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let secret = "persist/provider-secret".to_string();
    for raw in [
        format!("{secret} persist\\/provider-secret cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ= \u{1b}[2J"),
        json!({
            "content": secret.clone(),
            "tool_calls": [{
                "name": "read_file",
                "arguments": {"path": "persist\\/provider-secret"}
            }]
        })
        .to_string(),
    ] {
        let session = store.create_session();
        store
            .try_append_message(&session.id, "user", "safe request")
            .expect("user message");
        let projected = safe_persisted_provider_message(&raw, std::slice::from_ref(&secret), true);
        store
            .try_append_message(&session.id, "assistant", &projected)
            .expect("safe assistant message");
        let persisted = store.load(&session.id).expect("persisted session");
        let assistant = &persisted
            .messages
            .last()
            .expect("assistant message")
            .content;
        assert!(!assistant.contains(&secret));
        assert!(!assistant.contains(r"persist\/provider-secret"));
        assert!(!assistant.contains("cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ="));
        assert!(!assistant
            .chars()
            .any(|character| { character.is_control() && !matches!(character, '\n' | '\t') }));
        assert!(assistant.len() <= MAX_PERSISTED_PROVIDER_MESSAGE_BYTES);
    }

    let mut plan = crate::session::Plan::new(
        "safe goal",
        vec![crate::session::PlanStep {
            description: format!(
                "{secret} persist\\/provider-secret cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ= \u{1b}[2J"
            ),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    );
    sanitize_provider_plan(&mut plan, std::slice::from_ref(&secret));
    let description = &plan.steps[0].description;
    assert!(!description.contains(&secret));
    assert!(!description.contains(r"persist\/provider-secret"));
    assert!(!description.contains("cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ="));
    assert!(!description.contains('\u{1b}'));

    let raw_outcome = format!(
            "{secret} persist\\/provider-secret cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ= \u{1b}[2J {} OUTCOME_PRIVATE_TAIL",
            "o".repeat(MAX_PERSISTED_PROVIDER_MESSAGE_BYTES + 512),
        );
    let outcome = safe_provider_plan_outcome(Some(&raw_outcome), std::slice::from_ref(&secret));
    plan.complete_current_step(&outcome);
    let outcome = plan.steps[0].outcome.as_deref().expect("plan outcome");
    assert!(!outcome.contains(&secret));
    assert!(!outcome.contains(r"persist\/provider-secret"));
    assert!(!outcome.contains("cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ="));
    assert!(!outcome.contains('\u{1b}'));
    assert!(!outcome.contains("OUTCOME_PRIVATE_TAIL"));
    assert!(outcome.len() <= MAX_PERSISTED_PROVIDER_MESSAGE_BYTES);

    let expanding_controls = "\u{1b}".repeat(MAX_PERSISTED_PROVIDER_MESSAGE_BYTES / 2);
    let omitted = safe_persisted_provider_message(&expanding_controls, &[], true);
    assert!(omitted.contains("provider content omitted"));
    assert!(omitted.contains("was not retained"));
    assert!(!omitted.ends_with("..."));
}

#[tokio::test]
async fn provider_deltas_remain_private_until_terminal_validation() {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Ok(crate::llm::LlmStreamEvent::Delta(
        crate::llm::LlmDelta::Content("private-before-late-error\u{1b}[31m".to_string()),
    )))
    .await
    .expect("delta");
    tx.send(Err(crate::llm::LlmStreamFailure::from(
        "late provider rejection",
    )))
    .await
    .expect("error");
    drop(tx);

    let error = finish_private_provider_stream(LlmStream::from_public_receiver(rx), &[])
        .await
        .expect_err("late error rejects the private stream");
    assert_eq!(error.class, LlmErrorClass::Protocol);

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(Ok(crate::llm::LlmStreamEvent::Delta(
        crate::llm::LlmDelta::Content("refusal-private-content".to_string()),
    )))
    .await
    .expect("refusal delta");
    tx.send(Ok(crate::llm::LlmStreamEvent::Terminal(
        crate::llm::LlmFinishReason::Refusal,
    )))
    .await
    .expect("refusal terminal");
    drop(tx);

    let (response, projected) =
        finish_private_provider_stream(LlmStream::from_public_receiver(rx), &[])
            .await
            .expect("valid private refusal");
    assert_eq!(response.terminal_status, LlmTerminalStatus::Refused);
    assert!(projected.is_empty(), "refusal deltas must stay private");
}

#[test]
fn interrupted_provider_turn_is_reconciled_once_and_never_replays_tools() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let mut session = store.create_session_with_id("interrupted-provider-turn");
    let mut plan = pending_plan("inspect safely", "read the requested file");
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");
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
    let messages_before_reconciliation = store
        .load(&session.id)
        .expect("session before reconciliation")
        .messages;

    assert!(reconcile_interrupted_provider_continuation(&store, &session.id).unwrap());
    let reconciled = store.load(&session.id).expect("reconciled session");
    assert_eq!(
        reconciled.messages[..messages_before_reconciliation.len()],
        messages_before_reconciliation
    );
    let boundary = reconciled.messages.last().expect("reconciliation boundary");
    assert_eq!(boundary.role, "assistant");
    assert_eq!(
        serde_json::from_str::<Value>(&boundary.content).unwrap(),
        json!({
            "type": "provider_continuation_boundary",
            "outcome": "provider_continuation_interrupted",
        })
    );
    let plan = reconciled.plan.as_ref().expect("blocked plan");
    assert_eq!(
        plan.outcome.as_deref(),
        Some("provider_continuation_interrupted")
    );
    assert_eq!(plan.steps[plan.current_step_index].status, "Blocked");
    assert_eq!(
        reconciled
            .events
            .iter()
            .filter(|event| event.kind == "tool_completed")
            .count(),
        1
    );
    assert!(reconciled.events.iter().any(|event| {
        event.kind == "reconciliation"
            && event.details["outcome"] == "provider_continuation_interrupted"
            && event.details["continue"] == false
    }));

    invalidate_nonresumable_plan(&store, &session.id, "inspect safely").unwrap();
    assert!(store.load(&session.id).unwrap().plan.is_none());
    assert!(!reconcile_interrupted_provider_continuation(&store, &session.id).unwrap());
    assert_eq!(
        store
            .load(&session.id)
            .unwrap()
            .events
            .iter()
            .filter(|event| event.kind == "tool_completed")
            .count(),
        1
    );
}

#[test]
fn exact_run_identity_is_canonical_private_and_exactly_scoped() {
    for invalid in [
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcdeg",
    ] {
        let error = resolve_agent_run_id(Some(invalid.to_string()))
            .expect_err("non-canonical run ID must fail closed");
        assert!(error.contains("32 lowercase hexadecimal"));
    }

    let first = resolve_agent_run_id(None).expect("generated run ID");
    let second = resolve_agent_run_id(None).expect("second generated run ID");
    assert_ne!(first, second);
    for generated in [&first, &second] {
        assert_eq!(generated.len(), 32);
        assert!(generated
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }

    let supplied = "0123456789abcdef0123456789abcdef";
    let scope = request_scope_for_run("exact-scope-session", supplied).expect("request scope");
    assert_eq!(scope.session_id, "exact-scope-session");
    assert_eq!(scope.run_id, supplied);

    let summary = AgentRunSummary {
        session_id: "legacy-summary".to_string(),
        run_id: supplied.to_string(),
        steps_taken: 0,
        last_message: None,
        tool_call_count: 0,
        final_state: AgentState::Done,
        outcome: "completed".to_string(),
        failure: None,
        bound_reached: false,
        trace: Vec::new(),
    };
    let mut legacy = serde_json::to_value(summary).expect("serialize summary");
    legacy
        .as_object_mut()
        .expect("summary object")
        .remove("run_id");
    let decoded: AgentRunSummary =
        serde_json::from_value(legacy).expect("legacy summary remains readable");
    assert!(decoded.run_id.is_empty());

    let missing = AgentRunSummary {
            session_id: "instruction-session".to_string(),
            run_id: String::new(),
            steps_taken: 0,
            last_message: Some(
                "Project instructions could not be loaded, so this run stopped before dependent work.\nAGENTS.md exceeds the instruction byte limit.\nRestore readable project instructions within the size limit, or increase llm.context_length, then retry the same plan.".to_string(),
            ),
            tool_call_count: 0,
            final_state: AgentState::Done,
            outcome: "instruction_context_missing".to_string(),
            failure: None,
            bound_reached: false,
            trace: Vec::new(),
        };
    let report = missing.user_failure_report().expect("instruction report");
    assert!(report.contains("AGENTS.md exceeds"));
    assert!(report.contains("Session: instruction-session"));
    assert!(!report.contains("Agent run failed: instruction_context_missing"));
}

#[test]
fn exact_run_steering_is_persisted_ordered_and_bound_before_intake() {
    let directory = tempdir().expect("steering store");
    let store = SessionStore::new(directory.path());
    let session = store.create_session_with_id("steering-order");
    let run_id = "0123456789abcdef0123456789abcdef";
    let (handle, mut receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("steering channel");

    assert!(handle.submit("before admission").is_err());
    store
        .record_event(&session.id, "run_started", json!({"run_id": run_id}))
        .expect("run admission");
    bind_exact_run_steering_receiver(&store, &session.id, run_id, &receiver)
        .expect("install exact receiver");
    assert_eq!(handle.submit("first instruction").expect("first"), 1);
    assert_eq!(handle.submit("second instruction").expect("second"), 2);

    let persisted_before_intake = store.load(&session.id).expect("persisted steering");
    let inputs = persisted_before_intake
        .events
        .iter()
        .filter(|event| event.kind == "steering_input")
        .collect::<Vec<_>>();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].details["sequence"], 1);
    assert_eq!(inputs[0].details["source"], "plain");
    assert_eq!(inputs[0].details["text"], "first instruction");
    assert_eq!(inputs[1].details["sequence"], 2);
    assert!(!persisted_before_intake
        .events
        .iter()
        .any(|event| event.kind == "steering_intake"));

    let instructions = receiver.drain();
    assert_eq!(
        instructions
            .iter()
            .map(|instruction| (instruction.sequence, instruction.text.as_str()))
            .collect::<Vec<_>>(),
        [(1, "first instruction"), (2, "second instruction")]
    );
    assert!(record_steering_intake(
        &store,
        &session.id,
        run_id,
        &receiver.channel_id,
        &instructions[1..],
    )
    .expect_err("out-of-order intake must fail closed")
    .contains("skip an earlier"));
    record_steering_intake(
        &store,
        &session.id,
        run_id,
        &receiver.channel_id,
        &instructions,
    )
    .expect("durable intake");
    assert!(record_steering_intake(
        &store,
        &session.id,
        run_id,
        &receiver.channel_id,
        &instructions,
    )
    .is_err());

    let other_directory = tempdir().expect("other steering store");
    let other_store = SessionStore::new(other_directory.path());
    let other_session = other_store.create_session_with_id("other-steering-session");
    assert!(receiver
        .verify_binding(&other_store, &other_session.id, run_id)
        .is_err());
    assert!(receiver
        .verify_binding(&store, &session.id, "abcdef0123456789abcdef0123456789")
        .is_err());
}

#[test]
fn exact_run_steering_installs_one_channel_and_linearizes_action_admission() {
    let directory = tempdir().expect("steering store");
    let store = SessionStore::new(directory.path());
    let session = store.create_session_with_id("steering-channel-owner");
    let run_id = "0123456789abcdef0123456789abcdef";
    let (first, mut first_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("first steering channel");
    let (second, second_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
            .expect("second steering channel");
    store
        .record_event(&session.id, "run_started", json!({"run_id": run_id}))
        .expect("run start");
    bind_exact_run_steering_receiver(&store, &session.id, run_id, &first_receiver)
        .expect("install first channel");
    assert!(
        bind_exact_run_steering_receiver(&store, &session.id, run_id, &second_receiver,)
            .expect_err("second channel must not install")
            .contains("already has")
    );
    assert!(second
        .submit("must not enter the first channel")
        .expect_err("uninstalled handle")
        .contains("not installed"));

    assert_eq!(first.submit("wins before the action fence").unwrap(), 1);
    assert!(
        !close_steering_admission(true, &store, &session.id, run_id, "test_action_commit",)
            .expect("pending instruction blocks action")
    );
    let instructions = first_receiver.drain();
    record_steering_intake(
        &store,
        &session.id,
        run_id,
        &first_receiver.channel_id,
        &instructions,
    )
    .expect("account pending instruction");
    assert!(
        close_steering_admission(true, &store, &session.id, run_id, "test_action_commit",)
            .expect("action fence closes after intake")
    );
    assert!(first
        .submit("must lose after the action fence")
        .expect_err("closed admission")
        .contains("current run boundary"));
}

#[test]
fn exact_run_steering_survives_historical_reconciliation_for_later_active_steps() {
    let directory = tempdir().expect("steering store");
    let store = SessionStore::new(directory.path());
    let session = store.create_session_with_id("steering-after-reconciliation");
    let run_id = "0123456789abcdef0123456789abcdef";
    let (handle, receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("steering channel");
    store
        .record_event(&session.id, "run_started", json!({"run_id": run_id}))
        .expect("run start");
    bind_exact_run_steering_receiver(&store, &session.id, run_id, &receiver)
        .expect("install exact receiver");
    store
        .record_event(
            &session.id,
            "state_transition",
            json!({"from": "UpdateMemory", "to": "Reconciliation"}),
        )
        .expect("historical reconciliation");
    open_steering_admission(
        true,
        &store,
        &session.id,
        run_id,
        "continued_plan_build_context",
    )
    .expect("reopen admission");
    store
        .record_event(
            &session.id,
            "state_transition",
            json!({"from": "Reconciliation", "to": "BuildContext"}),
        )
        .expect("active continuation");
    assert_eq!(
        handle
            .submit("steer the later approved plan step")
            .expect("later active steering"),
        1
    );
}

#[test]
fn disabled_steering_admission_is_a_persistence_noop() {
    let directory = tempdir().expect("steering store");
    let store = SessionStore::new(directory.path());
    let session = store.create_session_with_id("steering-disabled-noop");
    let revision = store.load(&session.id).expect("persisted session").revision;

    assert!(close_steering_admission(
        false,
        &store,
        &session.id,
        "0123456789abcdef0123456789abcdef",
        "disabled_close",
    )
    .expect("disabled close is a no-op"));
    open_steering_admission(
        false,
        &store,
        &session.id,
        "0123456789abcdef0123456789abcdef",
        "disabled_open",
    )
    .expect("disabled open is a no-op");

    assert_eq!(
        store
            .load(&session.id)
            .expect("unchanged persisted session")
            .revision,
        revision
    );
}

#[test]
fn exact_run_steering_rejects_stale_terminal_and_unbounded_input() {
    let directory = tempdir().expect("steering store");
    let store = SessionStore::new(directory.path());
    let session = store.create_session_with_id("steering-bounds");
    let first_run = "0123456789abcdef0123456789abcdef";
    let second_run = "abcdef0123456789abcdef0123456789";
    let (first, first_receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), first_run, "tui")
            .expect("first steering channel");
    store
        .record_event(&session.id, "run_started", json!({"run_id": first_run}))
        .expect("first run");
    bind_exact_run_steering_receiver(&store, &session.id, first_run, &first_receiver)
        .expect("install first receiver");
    assert!(first.submit("\u{0007}").is_err());
    assert!(first
        .submit(&"x".repeat(MAX_STEERING_INPUT_BYTES + 1))
        .is_err());

    store
        .record_event(&session.id, "run_started", json!({"run_id": second_run}))
        .expect("replacement run");
    assert!(first
        .submit("must not reach the replacement run")
        .expect_err("stale handle")
        .contains("stale"));

    let bounded = store.create_session_with_id("steering-total-bound");
    let (bounded_handle, bounded_receiver) =
        exact_run_steering_channel(store.clone(), bounded.id.clone(), first_run, "plain")
            .expect("bounded channel");
    store
        .record_event(&bounded.id, "run_started", json!({"run_id": first_run}))
        .expect("bounded run");
    bind_exact_run_steering_receiver(&store, &bounded.id, first_run, &bounded_receiver)
        .expect("install bounded receiver");
    for _ in 0..4 {
        bounded_handle
            .submit(&"x".repeat(MAX_STEERING_INPUT_BYTES))
            .expect("within total bound");
    }
    assert!(bounded_handle
        .submit("one byte too many")
        .expect_err("total bound")
        .contains("byte limit"));
    store
        .record_event(
            &bounded.id,
            "run_terminal",
            json!({"run_id": first_run, "outcome": "completed"}),
        )
        .expect("terminal");
    assert!(bounded_handle.submit("too late").is_err());
}

#[test]
fn exact_run_steering_channel_loss_is_explicit_after_persistence() {
    let directory = tempdir().expect("steering store");
    let store = SessionStore::new(directory.path());
    let session = store.create_session_with_id("steering-channel-loss");
    let run_id = "0123456789abcdef0123456789abcdef";
    let (handle, receiver) =
        exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
            .expect("steering channel");
    store
        .record_event(&session.id, "run_started", json!({"run_id": run_id}))
        .expect("run admission");
    bind_exact_run_steering_receiver(&store, &session.id, run_id, &receiver)
        .expect("install exact receiver");
    drop(receiver);

    assert!(handle
        .submit("persist even when delivery loses the race")
        .expect_err("closed channel")
        .contains("persisted"));
    let persisted = store.load(&session.id).expect("delivery failure evidence");
    let input_index = persisted
        .events
        .iter()
        .position(|event| event.kind == "steering_input")
        .expect("steering input");
    let failure_index = persisted
        .events
        .iter()
        .position(|event| event.kind == "steering_delivery_failed")
        .expect("delivery failure");
    assert!(input_index < failure_index);
    assert_eq!(
        persisted.events[failure_index].details["reason"],
        "receiver_closed"
    );
}

#[test]
fn provider_failures_block_the_bound_plan_and_classify_the_run_as_failed() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let mut session = store.create_session_with_id("provider-failure");
    let mut plan = pending_plan("continue", "call the model");
    plan.approve();
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).unwrap();

    let failure = "llm_stream_failed";
    for failure in [failure, "compression_failed"] {
        block_active_plan_for_failure(&store, &session.id, Some(&plan_id), "continue", failure)
            .unwrap();
        let plan = store.load(&session.id).unwrap().plan.unwrap();
        assert_eq!(plan.steps[plan.current_step_index].status, "Blocked");
        assert_eq!(plan.outcome.as_deref(), Some(failure));
        assert!(is_agent_failure_outcome(failure));
    }

    let summary = AgentRunSummary {
        session_id: session.id,
        run_id: String::new(),
        steps_taken: 1,
        last_message: None,
        tool_call_count: 0,
        final_state: AgentState::Done,
        outcome: failure.to_string(),
        failure: None,
        bound_reached: false,
        trace: Vec::new(),
    };
    assert!(summary.is_failure());
    assert!(!AgentRunSummary {
        outcome: "completed".to_string(),
        ..summary
    }
    .is_failure());

    let mut config = crate::config::NibConfig::default();
    config.llm.providers.insert(
        "anthropic".to_string(),
        ProviderEntry {
            model: "fixture-model".to_string(),
            api_key: Some("inactive-provider-secret".to_string()),
            ..ProviderEntry::default()
        },
    );
    let redacted = redact_provider_failure(
        &config,
        LlmError::local(
            LlmErrorClass::Protocol,
            LlmErrorPhase::Stream,
            format!("inactive-provider-secret {}", "x".repeat(16 * 1024)),
        ),
    );
    assert!(!redacted.contains("inactive-provider-secret"));
    assert!(redacted.contains("[REDACTED]"));
    assert!(redacted.to_string().ends_with("..."));
    assert!(redacted.len() <= 8 * 1024);
}

#[test]
fn next_user_turn_after_llm_failure_does_not_create_assistant_content() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session_with_id("provider-failure-next-turn");
    store
        .try_append_message(&session.id, "user", "first request")
        .expect("first user turn");
    store
        .record_event(
            &session.id,
            "reconciliation",
            json!({
                "outcome": "llm_stream_failed",
                "continue": false,
                "failure": {
                    "class": "transport",
                    "phase": "stream",
                    "retry": "retryable",
                },
            }),
        )
        .expect("persist structured failure");

    prepare_user_turn(
        &store,
        &session.id,
        "second request",
        std::path::Path::new("."),
    )
    .expect("accept the next user turn");

    let persisted = store.load(&session.id).expect("persisted session");
    assert_eq!(
        persisted
            .messages
            .iter()
            .map(|message| (message.role.as_str(), message.content.as_str()))
            .collect::<Vec<_>>(),
        [("user", "first request"), ("user", "second request"),]
    );
    assert!(persisted.messages.iter().all(|message| {
        !message.content.contains("Previous run reconciled") && !message.content.contains("LLM-")
    }));
}

#[test]
fn next_user_turn_after_local_reconciliation_does_not_create_assistant_content() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session_with_id("local-reconciliation-next-turn");
    store
        .try_append_message(&session.id, "user", "interrupted request")
        .expect("first user turn");
    store
        .record_event(
            &session.id,
            "reconciliation",
            json!({"outcome": "cancelled_by_user", "continue": false}),
        )
        .expect("persist local reconciliation");

    prepare_user_turn(
        &store,
        &session.id,
        "replacement request",
        std::path::Path::new("."),
    )
    .expect("accept replacement turn");

    let persisted = store.load(&session.id).expect("persisted session");
    assert_eq!(
        persisted
            .messages
            .iter()
            .map(|message| (message.role.as_str(), message.content.as_str()))
            .collect::<Vec<_>>(),
        [
            ("user", "interrupted request"),
            ("user", "replacement request"),
        ]
    );
}

#[test]
fn prepare_user_turn_stores_structured_path_attachments() {
    let dir = tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("src");
    std::fs::write(dir.path().join("src/lib.rs"), "fn attached() {}").expect("file");
    let store = SessionStore::new(dir.path());
    let session = store.create_session();
    prepare_user_turn(&store, &session.id, "inspect @src/lib.rs", dir.path()).expect("attach");
    let persisted = store.load(&session.id).expect("session");
    let user = persisted
        .messages
        .iter()
        .find(|message| message.role == "user")
        .expect("user turn");
    assert_eq!(user.content, "inspect @src/lib.rs");
    assert!(!user.content.contains("fn attached"));
    assert_eq!(user.attachments.len(), 1);
    assert_eq!(user.attachments[0].path, "src/lib.rs");
}

#[test]
#[serial_test::serial]
fn provider_failure_persistence_redacts_inactive_environment_credentials() {
    const SECRET: &str = "inactive/env-provider-secret";
    let _environment = EnvironmentGuard::set("ANTHROPIC_API_KEY", SECRET);
    let config = crate::config::NibConfig::default();
    let error = LlmError::local(
        LlmErrorClass::Protocol,
        LlmErrorPhase::Stream,
        "request failed for model-inactive%2Fenv-provider-secret",
    );

    let redacted = redact_provider_failure(&config, error);

    assert_eq!(redacted, "request failed for model-[REDACTED]");
    assert!(!redacted.contains(SECRET));
    assert!(!redacted.contains("inactive%2Fenv-provider-secret"));
}

#[test]
fn plan_invalidation_is_audited_while_same_goal_resume_is_preserved() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());

    let mut resumed = store.create_session_with_id("resume");
    let mut resumable_plan = pending_plan("keep working", "continue the approved work");
    resumable_plan.approve();
    let resumable_id = resumable_plan.id.clone();
    resumed.plan = Some(resumable_plan);
    store.save(&mut resumed).expect("resumable plan");
    invalidate_nonresumable_plan(&store, "resume", "  keep\nworking ").expect("resume check");
    let resumed = store.load("resume").expect("resumed session");
    assert_eq!(resumed.plan.as_ref().unwrap().id, resumable_id);
    assert!(!resumed
        .events
        .iter()
        .any(|event| event.kind == "plan_invalidated"));

    let mut mismatched = store.create_session_with_id("mismatch");
    let mut mismatched_plan = pending_plan("old goal", "perform old work");
    mismatched_plan.approve();
    let mismatched_id = mismatched_plan.id.clone();
    mismatched.plan = Some(mismatched_plan);
    store.save(&mut mismatched).expect("mismatched plan");
    invalidate_nonresumable_plan(&store, "mismatch", "new goal").expect("mismatch invalidation");
    let mismatched = store.load("mismatch").expect("mismatched session");
    assert!(mismatched.plan.is_none());
    let mismatch_event = mismatched
        .events
        .iter()
        .find(|event| event.kind == "plan_invalidated")
        .expect("mismatch audit");
    assert_eq!(mismatch_event.details["reason"], "goal_mismatch");
    assert_eq!(mismatch_event.details["previous_plan_id"], mismatched_id);
    assert_eq!(mismatch_event.details["previous_goal"], "old goal");
    assert_eq!(mismatch_event.details["requested_goal"], "new goal");

    let mut completed = store.create_session_with_id("completed");
    let mut completed_plan = pending_plan("same goal", "finish old run");
    completed_plan.approve();
    completed_plan.complete_current_step("done");
    completed.plan = Some(completed_plan);
    store.save(&mut completed).expect("completed plan");
    invalidate_nonresumable_plan(&store, "completed", "same goal").expect("completed invalidation");
    let completed = store.load("completed").expect("completed session");
    assert!(completed.plan.is_none());
    assert_eq!(
        completed
            .events
            .iter()
            .find(|event| event.kind == "plan_invalidated")
            .unwrap()
            .details["reason"],
        "completed_plan"
    );

    let mut legacy = store.create_session_with_id("legacy");
    let mut legacy_plan = pending_plan("legacy goal", "legacy step");
    legacy_plan.id.clear();
    legacy_plan.goal.clear();
    legacy.plan = Some(legacy_plan);
    store.save(&mut legacy).expect("legacy plan");
    invalidate_nonresumable_plan(&store, "legacy", "legacy goal").expect("legacy invalidation");
    let legacy = store.load("legacy").expect("legacy session");
    assert!(legacy.plan.is_none());
    assert_eq!(
        legacy
            .events
            .iter()
            .find(|event| event.kind == "plan_invalidated")
            .unwrap()
            .details["reason"],
        "legacy_plan"
    );

    let mut malformed = store.create_session_with_id("malformed");
    let mut malformed_plan = pending_plan("same goal", "invalidated before execution");
    malformed_plan.approve();
    malformed_plan.steps[0].status = "Completed".to_string();
    malformed.plan = Some(malformed_plan);
    store.save(&mut malformed).expect("malformed plan");
    invalidate_nonresumable_plan(&store, "malformed", "same goal").expect("malformed invalidation");
    let malformed = store.load("malformed").expect("malformed session");
    assert!(malformed.plan.is_none());
    assert_eq!(
        malformed
            .events
            .iter()
            .find(|event| event.kind == "plan_invalidated")
            .unwrap()
            .details["reason"],
        "invalid_plan"
    );
}
