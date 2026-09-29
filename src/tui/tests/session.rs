use super::*;

#[test]
fn session_switcher_stays_under_the_composer_like_slash_options() {
    let switcher = SessionSwitcher {
        candidates: vec![InteractiveSessionCandidate {
            id: "visible-session".to_string(),
            label: "visible-session".to_string(),
            preview: "Latest user message: visible content".to_string(),
            is_active: false,
            snapshot_token: [0; 32],
        }],
        selected: 0,
        omitted: 0,
        confirming: false,
        exact_id: String::new(),
        error: None,
    };
    let band = InteractionBand::Sessions {
        switcher: &switcher,
        active: "current-session",
    };
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut viewport = TranscriptViewport::default();
    let welcome = StartupWelcome::fixture();
    let composer = Composer::from_text("/session");
    terminal
        .draw(|frame| {
            render_session_activities(
                frame,
                &TuiChrome::fixture(),
                &[ActivityEntry::new(
                    ActivityKind::User,
                    "",
                    "hello conversation",
                )],
                &composer,
                None,
                None,
                false,
                &mut viewport,
                TuiFocus::Composer,
                None,
                0,
                None,
                None,
                &welcome,
                Some(&band),
                None,
                None,
            )
        })
        .expect("render under-composer sessions");
    let rows = buffer_rows(&terminal);
    let conversation = row_index_containing(&rows, "hello conversation").expect("conversation");
    let prompt = row_index_containing(&rows, "> /session").expect("composer");
    let option = row_index_containing(&rows, "visible-session").expect("session option");
    assert!(conversation < prompt, "{rows:?}");
    assert!(prompt < option, "{rows:?}");
    let slash = rows[prompt].find('/').expect("composer slash");
    let option_start = rows[option].find('v').expect("option text");
    assert_eq!(
        slash, option_start,
        "prompt={:?} option={:?}",
        rows[prompt], rows[option]
    );
}

#[test]
fn completion_and_session_overlays_render_on_small_terminals() {
    let mut completion = CompletionMenu::default();
    completion.sync("/");
    let switcher = SessionSwitcher {
        candidates: vec![InteractiveSessionCandidate {
            id: "small-session".to_string(),
            label: "small-session".to_string(),
            preview: "bounded preview".to_string(),
            is_active: true,
            snapshot_token: [0; 32],
        }],
        selected: 0,
        omitted: 0,
        confirming: true,
        exact_id: String::new(),
        error: None,
    };

    for (width, height) in [(20, 6), (40, 10)] {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("small test terminal");
        terminal
            .draw(|frame| {
                render_session_view_with_completion(
                    frame,
                    "sess small-session",
                    "idle",
                    "Session: small-session",
                    &Composer::default(),
                    Some(&completion),
                    None,
                    false,
                );
            })
            .expect("render completion on small terminal");
        terminal
            .draw(|frame| render_session_switcher(frame, frame.area(), &switcher, "small-session"))
            .expect("render switcher on small terminal");
    }
}

#[test]
fn renders_every_agent_lifecycle_event() {
    let mut output = LiveOutput::default();
    let read_invocation = crate::tools::ToolInvocationId::new();
    let terminal_invocation = crate::tools::ToolInvocationId::new();
    let events = vec![
        StreamEvent::StateTransition {
            state: "planning".to_string(),
        },
        StreamEvent::PlanGenerated {
            step_count: 2,
            steps: vec!["inspect wrap".to_string(), "write tests".to_string()],
        },
        StreamEvent::Content("Working".to_string()),
        StreamEvent::ToolCallChunk {
            invocation_id: read_invocation,
            index: 0,
            name: Some("read_file".to_string()),
            arguments: Some("{}".to_string()),
        },
        StreamEvent::ApprovalRequired {
            tool_name: "run_terminal".to_string(),
        },
        StreamEvent::QuestionRequired {
            question: "Choose a mode".to_string(),
            options: vec!["plan".to_string(), "execute".to_string()],
        },
        StreamEvent::ToolStarted {
            invocation_id: read_invocation,
            tool_name: "read_file".to_string(),
        },
        StreamEvent::TerminalOutput {
            invocation_id: terminal_invocation,
            tool_name: "run_terminal".to_string(),
            stream: "stderr".to_string(),
            chunk: "building\n".to_string(),
            background_task_id: None,
        },
        StreamEvent::ToolCompleted {
            invocation_id: read_invocation,
            tool_name: "read_file".to_string(),
            success: true,
            output: Some(json!({"content": "nib"})),
            error: None,
        },
        StreamEvent::ToolCompleted {
            invocation_id: terminal_invocation,
            tool_name: "run_terminal".to_string(),
            success: false,
            output: None,
            error: Some("exit 1".to_string()),
        },
        StreamEvent::Compression {
            before_tokens: 1_000,
            after_tokens: 250,
            summarized_through: 4,
        },
        StreamEvent::Reconciled {
            outcome: "completed".to_string(),
        },
        StreamEvent::Failure {
            failure: crate::llm::LlmError::new(
                crate::llm::LlmErrorClass::Authentication,
                crate::llm::LlmErrorPhase::HttpResponse,
                crate::llm::RetryDisposition::NotRetryable,
                crate::llm::LlmErrorMetadata::new(
                    "openai",
                    "responses",
                    Some("gpt-test"),
                    Some(401),
                    &[],
                ),
                "credential rejected",
            ),
            session_id: Some("failure-session".to_string()),
        },
        StreamEvent::End("stop".to_string()),
    ];

    for event in events {
        output.apply(event, &[]);
    }

    for expected in [
        "[state] planning",
        "[plan] generated 2 steps",
        "Working\n[tool call] read_file",
        "[approval required] run_terminal",
        "[question] Choose a mode (options: plan | execute)",
        "[tool started] read_file",
        "[terminal stderr] run_terminal: building",
        "[tool completed] read_file: ok - {\"content\":\"nib\"}",
        "[tool completed] run_terminal: failed - exit 1",
        "[compression] 1000 -> 250 tokens; summarized through message 4",
        "[reconciled] Run completed",
        "LLM request failed [LLM-AUTH]",
        "Provider: openai (responses), model: gpt-test",
        "Action: Refresh this provider's credential with `nib auth`, then retry.",
        "Session: failure-session",
        "[stream ended] Run stopped",
    ] {
        assert!(output.text.contains(expected), "missing: {expected}");
    }
}

#[test]
fn timeline_ignores_intermediate_plan_reconciliation_and_deduplicates_end() {
    let mut timeline = ActiveTimeline::default();
    for outcome in ["step_completed", "verification_recovery"] {
        timeline.apply_event(StreamEvent::Reconciled {
            outcome: outcome.to_string(),
        });
    }
    assert!(timeline.reconciled_terminal.is_none());
    assert!(timeline.activities.is_empty());

    timeline.apply_event(StreamEvent::Reconciled {
        outcome: "completed".to_string(),
    });
    timeline.apply_event(StreamEvent::End("completed".to_string()));
    assert_eq!(
        timeline.reconciled_terminal,
        Some(InteractionTerminalOutcome::Completed)
    );
    assert_eq!(timeline.activities.len(), 1);
    assert_eq!(timeline.activities[0].kind, ActivityKind::Reconcile);

    timeline.bind_run(Some("next-run".to_string()));
    timeline.apply_event(StreamEvent::End("local_error".to_string()));
    assert_eq!(timeline.activities.len(), 2);
    assert_eq!(timeline.activities[1].kind, ActivityKind::Failure);
    assert_eq!(timeline.activities[1].title, "Run stopped");
    assert!(!timeline.activities[1].title.contains("local_error"));
    assert!(timeline.activities[1].body.contains("/status"));
    assert_eq!(
        timeline.reconciled_terminal,
        Some(InteractionTerminalOutcome::Failed)
    );
}

#[test]
fn timeline_preserves_a_final_error_after_successful_reconciliation() {
    let mut timeline = ActiveTimeline::default();
    timeline.apply_event(StreamEvent::Reconciled {
        outcome: "completed".to_string(),
    });
    timeline.apply_event(StreamEvent::End("local_error".to_string()));

    assert_eq!(timeline.activities.len(), 2);
    assert_eq!(timeline.activities[1].title, "Run stopped");
    assert!(!timeline.activities[1].title.contains("local_error"));
    assert!(timeline.live.text.contains("[stream ended] Run stopped"));
    assert!(!timeline.live.text.contains("[stream ended] local_error"));
    assert_eq!(
        timeline.reconciled_terminal,
        Some(InteractionTerminalOutcome::Failed)
    );
    assert_eq!(
        tui_run_state(
            false,
            timeline.live.state.as_deref(),
            timeline.reconciled_terminal
        ),
        InteractionRunState::Failed
    );
}

#[test]
fn timeline_deduplicates_repeated_worker_end_without_reconciliation() {
    let mut timeline = ActiveTimeline::default();
    timeline.apply_event(StreamEvent::End("completed".to_string()));
    timeline.apply_event(StreamEvent::End("completed".to_string()));
    assert_eq!(timeline.activities.len(), 1);
    assert_eq!(timeline.activities[0].title, "Run completed");
}

#[test]
fn test_backend_renders_one_bounded_control_free_failure_detail() {
    const SECRET: &str = "tui/observer+secret";
    let failure = crate::llm::LlmError::new(
        crate::llm::LlmErrorClass::Authentication,
        crate::llm::LlmErrorPhase::HttpResponse,
        crate::llm::RetryDisposition::NotRetryable,
        crate::llm::LlmErrorMetadata::new(
            "openai",
            "responses",
            Some("fixture-model"),
            Some(401),
            &[SECRET.to_string()],
        ),
        format!(
            "{SECRET} <red>[bold] REMOTE_TUI_SENTINEL \u{1b}[31m {}",
            "x".repeat(MAX_LIVE_OUTPUT_BYTES)
        ),
    );
    let mut timeline = ActiveTimeline {
        session_id: "tui-failure-session".to_string(),
        ..ActiveTimeline::default()
    };
    timeline.apply_event(StreamEvent::Failure {
        failure,
        session_id: Some("tui-failure-session".to_string()),
    });
    let timeline_text = timeline.rendered_text();

    assert!(timeline_text.len() <= 1_024, "{}", timeline_text.len());
    assert_eq!(timeline_text.matches("LLM request failed").count(), 1);
    assert!(timeline_text.contains("LLM request failed [LLM-AUTH]"));
    assert!(timeline_text.contains("Session: tui-failure-session"));
    for forbidden in [SECRET, "REMOTE_TUI_SENTINEL", "[red]", "[bold]", "\u{1b}"] {
        assert!(!timeline_text.contains(forbidden), "{timeline_text}");
    }
    assert!(timeline_text
        .chars()
        .all(|character| { !character.is_control() || matches!(character, '\n' | '\r' | '\t') }));

    let backend = TestBackend::new(48, 12);
    let mut terminal = Terminal::new(backend).expect("failure detail terminal");
    terminal
        .draw(|frame| {
            render_current_session_view(
                frame,
                "workspace  ·  sess tui-failure-session",
                "idle  ·  openai/fixture-model",
                &timeline_text,
                &Composer::default(),
                None,
                None,
            )
        })
        .expect("render failure detail");
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.content.len(), 48 * 12);
    let rendered = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    // The deliberately short viewport is bottom-aligned, so the heading may
    // scroll out while the actionable tail remains visible. The full bounded
    // timeline above owns the exactly-once heading assertion.
    assert!(
        rendered.contains("Provider: openai (responses)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("HTTP: 401; retry: not retryable"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Session: tui-failure-session"),
        "{rendered}"
    );
    assert!(!rendered.chars().any(char::is_control));
    assert!(!rendered.contains("REMOTE_TUI_SENTINEL"));
}

#[test]
fn live_output_retains_a_bounded_utf8_tail() {
    let mut output = LiveOutput::default();
    for _ in 0..(MAX_LIVE_OUTPUT_BYTES / 8_192 + 2) {
        output.apply(StreamEvent::Content("é".repeat(4_096)), &[]);
    }

    assert!(output.text.len() <= MAX_LIVE_OUTPUT_BYTES);
    assert!(output.text.starts_with(OMITTED_OUTPUT_MARKER));
    assert!(output.text.is_char_boundary(output.text.len()));
    assert!(output.text.ends_with('é'));
}

#[test]
fn live_output_replaces_terminal_active_and_bidi_controls() {
    let mut output = LiveOutput::default();
    output.apply(
        StreamEvent::Content("safe\u{1b}[2J\rreplace\u{202e}tail".to_string()),
        &[],
    );

    assert!(!output.text.contains('\u{1b}'));
    assert!(!output.text.contains('\r'));
    assert!(!output.text.contains('\u{202e}'));
    assert!(output.text.contains("safe"));
    assert!(output.text.contains("tail"));
}

#[test]
fn exact_run_identity_ignores_prior_and_idle_run_events() {
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(4);
    stream_tx
        .try_send(SessionStreamEvent {
            session_id: "active-session".to_string(),
            run_id: "old-run".to_string(),
            event: StreamEvent::Content("must not leak".to_string()),
        })
        .expect("old event");
    stream_tx
        .try_send(SessionStreamEvent {
            session_id: "active-session".to_string(),
            run_id: "active-run".to_string(),
            event: StreamEvent::Content("visible active output".to_string()),
        })
        .expect("active event");
    let mut timeline = ActiveTimeline {
        session_id: "active-session".to_string(),
        active_run_id: Some("active-run".to_string()),
        ..ActiveTimeline::default()
    };

    drain_stream_events(&mut stream_rx, &mut timeline);

    assert_eq!(timeline.live.text, "visible active output");

    timeline.active_run_id = None;
    stream_tx
        .try_send(SessionStreamEvent {
            session_id: "active-session".to_string(),
            run_id: "active-run".to_string(),
            event: StreamEvent::Content("must not mutate idle timeline".to_string()),
        })
        .expect("idle late event");
    drain_stream_events(&mut stream_rx, &mut timeline);
    assert_eq!(timeline.live.text, "visible active output");
}

#[test]
fn exact_run_identity_worker_error_renders_only_safe_terminal_outcome() {
    let event =
        safe_agent_error_stream_event("PRIVATE_LOCAL_ERROR_SENTINEL\u{1b}[2J\nprovider payload");
    assert_eq!(event, StreamEvent::End("local_error".to_string()));
}

#[test]
fn approval_modal_only_resolves_on_explicit_decision() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": "task test"}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        reply_tx,
    ));

    assert!(handle_approval_key(&mut pending, KeyCode::Char('x')));
    assert!(pending.is_some());
    assert!(reply_rx.try_recv().is_err());

    assert!(handle_approval_key(&mut pending, KeyCode::Char('Y')));
    let decision = reply_rx.try_recv().unwrap();
    assert!(decision.granted);
    assert_eq!(decision.source, "user");
    assert!(decision.remember_command.is_none());
}

#[test]
fn approval_enter_and_number_keys_are_explicit_choices() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": "task test"}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        reply_tx,
    ));
    assert!(handle_approval_key(&mut pending, KeyCode::Enter));
    let granted = reply_rx.try_recv().unwrap();
    assert!(granted.granted);
    assert!(granted.remember_command.is_none());

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": "task test"}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        reply_tx,
    ));
    assert!(handle_approval_key(&mut pending, KeyCode::Char('p')));
    assert!(pending.as_ref().is_some_and(|req| req.selected_option == 1));
    assert!(reply_rx.try_recv().is_err());
    assert!(handle_approval_key(&mut pending, KeyCode::Enter));
    let remembered = reply_rx.try_recv().unwrap();
    assert_eq!(remembered.remember_command.as_deref(), Some("task test"));

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": "task test"}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        reply_tx,
    ));
    assert!(handle_approval_key(&mut pending, KeyCode::Down));
    assert!(handle_approval_key(&mut pending, KeyCode::Down));
    assert!(handle_approval_key(&mut pending, KeyCode::Enter));
    assert!(pending
        .as_ref()
        .is_some_and(|req| req.reason_draft.is_some()));
    assert!(reply_rx.try_recv().is_err());
    assert!(handle_approval_key(&mut pending, KeyCode::Esc));
    assert!(!reply_rx.try_recv().unwrap().granted);

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "apply_patch".to_string(),
            arguments: json!({}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        reply_tx,
    ));
    assert!(handle_approval_key(&mut pending, KeyCode::Enter));
    assert!(!reply_rx.try_recv().unwrap().granted);
}

#[test]
fn approval_modal_sends_denial() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "apply_patch".to_string(),
            arguments: json!({}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        reply_tx,
    ));

    assert!(handle_approval_key(&mut pending, KeyCode::Char('n')));
    assert!(handle_approval_key(&mut pending, KeyCode::Enter));
    let decision = reply_rx.try_recv().unwrap();
    assert!(!decision.granted);
    assert_eq!(decision.source, "denied");
}

#[test]
fn approval_details_are_labeled_scrollable_and_non_authorizing() {
    let details = vec![format!("Command: {} END", "界".repeat(80))];
    let first = approval_detail_view_rows(&details, 20, 0);
    let later = approval_detail_view_rows(&details, 20, 4);
    assert!(first.iter().any(|row| row.contains("Approval details")));
    assert!(first.iter().any(|row| row.contains("more rows")));
    assert_ne!(first, later);
    let end = approval_detail_view_rows(&details, 20, usize::MAX);
    assert!(end.iter().any(|row| row.contains("END")), "{end:?}");

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "apply_patch".to_string(),
            arguments: json!({"patch": "diff"}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        reply_tx,
    ));
    pending.as_mut().expect("approval").selected_option = 2;
    assert!(handle_approval_key(&mut pending, KeyCode::Enter));
    assert!(pending.as_ref().is_some_and(|req| req.details_open));
    assert!(reply_rx.try_recv().is_err());
    assert!(handle_approval_key(&mut pending, KeyCode::Esc));
    assert!(pending.as_ref().is_some_and(|req| !req.details_open));
    assert!(reply_rx.try_recv().is_err());
}

#[test]
fn approval_consumes_input_before_question_and_completion_layers() {
    let (approval_tx, mut approval_rx) = oneshot::channel();
    let mut approval = Some(approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        approval_tx,
    ));
    let (question_tx, mut question_rx) = oneshot::channel();
    let mut question = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Still here?".to_string(),
        proposed_answer: None,
        options: vec!["yes".to_string()],
        reply: question_tx,
    }));
    let composer = Composer::from_text("/");
    let mut completion = CompletionMenu::default();
    completion.sync(&composer.input);

    assert!(handle_pending_interaction_key(
        &mut approval,
        &mut question,
        KeyCode::Char('s')
    ));
    assert!(approval.is_some());
    assert!(approval_rx.try_recv().is_err());
    assert_eq!(composer.input, "/");

    assert!(handle_pending_interaction_key(
        &mut approval,
        &mut question,
        KeyCode::Esc
    ));
    assert!(approval.is_none());
    assert!(!approval_rx.try_recv().expect("approval reply").granted);
    assert!(question.is_some());
    assert!(question_rx.try_recv().is_err());
    assert_eq!(composer.input, "/");
    assert!(completion.is_open());

    assert!(handle_pending_interaction_key(
        &mut approval,
        &mut question,
        KeyCode::Char('y')
    ));
    assert!(handle_pending_interaction_key(
        &mut approval,
        &mut question,
        KeyCode::Enter
    ));
    assert_eq!(
        question_rx.try_recv().expect("question reply"),
        crate::agent::QuestionOutcome::Answered("y".to_string())
    );
    assert_eq!(composer.input, "/");
}

#[test]
fn modal_arriving_after_poll_refreshes_before_the_key_is_dispatched() {
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (_question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (reply_tx, mut reply_rx) = oneshot::channel();
    approval_tx
        .send(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            reply_tx,
        ))
        .expect("approval arrives while terminal poll is blocked");
    let mut pending_approval = None;
    let mut pending_question = None;

    refresh_pending_interactions(
        &mut pending_approval,
        &mut pending_question,
        &approval_rx,
        &question_rx,
    );
    let state = tui_interaction_state(
        pending_approval.is_some(),
        pending_question.is_some(),
        false,
        None,
        false,
        false,
        InteractionRunState::Running,
    );
    assert_eq!(
        reduce_interaction(&state, InteractionInput::SteerCurrent("private steering")),
        InteractionReduction::Consumed(InteractionConsumer::Approval)
    );
    assert!(handle_pending_interaction_key(
        &mut pending_approval,
        &mut pending_question,
        KeyCode::Char('s'),
    ));
    assert!(pending_approval.is_some());
    assert!(reply_rx.try_recv().is_err());
}

#[test]
fn fixed_status_projects_authoritative_terminal_state_after_worker_join() {
    for (terminal, expected) in [
        (
            InteractionTerminalOutcome::Completed,
            InteractionRunState::Completed,
        ),
        (
            InteractionTerminalOutcome::Cancelled,
            InteractionRunState::Cancelled,
        ),
        (
            InteractionTerminalOutcome::Failed,
            InteractionRunState::Failed,
        ),
        (
            InteractionTerminalOutcome::WaitingForInput,
            InteractionRunState::Failed,
        ),
    ] {
        assert_eq!(tui_run_state(false, None, Some(terminal)), expected);
    }
    assert_eq!(tui_run_state(false, None, None), InteractionRunState::Idle);
    assert_eq!(
        tui_run_state(
            true,
            Some("reconciliation"),
            Some(InteractionTerminalOutcome::Completed),
        ),
        InteractionRunState::Reconciling,
        "a bound worker remains authoritative until it is joined"
    );
}

#[test]
fn shared_interaction_reducer_maps_to_tui_renderer_layers() {
    let browsing = SessionSwitcher {
        candidates: Vec::new(),
        selected: 0,
        omitted: 0,
        confirming: false,
        exact_id: String::new(),
        error: None,
    };
    let confirming = SessionSwitcher {
        confirming: true,
        ..browsing.clone()
    };
    let cases = [
        (
            true,
            true,
            true,
            Some(&confirming),
            true,
            true,
            InteractionLayer::Approval,
        ),
        (
            false,
            true,
            true,
            Some(&confirming),
            true,
            true,
            InteractionLayer::Question,
        ),
        (
            false,
            false,
            true,
            Some(&confirming),
            true,
            true,
            InteractionLayer::SessionConfirmation,
        ),
        (
            false,
            false,
            false,
            Some(&confirming),
            true,
            true,
            InteractionLayer::SessionConfirmation,
        ),
        (
            false,
            false,
            false,
            Some(&browsing),
            false,
            true,
            InteractionLayer::SessionSwitcher,
        ),
        (
            false,
            false,
            false,
            None,
            true,
            true,
            InteractionLayer::HistorySearch,
        ),
        (
            false,
            false,
            false,
            None,
            false,
            true,
            InteractionLayer::Completion,
        ),
        (
            false,
            false,
            false,
            None,
            false,
            false,
            InteractionLayer::Composer,
        ),
    ];

    for (approval, question, model, switcher, history, completion, expected) in cases {
        assert_eq!(
            active_interaction_layer(approval, question, model, switcher, history, completion,),
            expected
        );
    }
}

#[test]
fn question_modal_submits_typed_response() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Branch name?".to_string(),
        proposed_answer: None,
        options: vec![],
        reply: reply_tx,
    }));

    assert!(!handle_question_key(&mut pending, KeyCode::Char('m')));
    assert!(!handle_question_key(&mut pending, KeyCode::Char('a')));
    assert!(!handle_question_key(&mut pending, KeyCode::Char('x')));
    assert!(!handle_question_key(&mut pending, KeyCode::Backspace));
    assert!(!handle_question_key(&mut pending, KeyCode::Char('i')));
    assert!(!handle_question_key(&mut pending, KeyCode::Char('n')));
    assert!(handle_question_key(&mut pending, KeyCode::Enter));

    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::Answered("main".to_string())
    );
    assert!(pending.is_none());
}

#[test]
fn proposed_question_requires_a_deliberate_decision() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Use the release branch?".to_string(),
        proposed_answer: Some("release".to_string()),
        options: vec![],
        reply: reply_tx,
    }));
    assert_eq!(pending.as_ref().unwrap().selected_decision, 1);
    assert!(!handle_question_key(&mut pending, KeyCode::Char('1')));
    assert!(matches!(
        reply_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::ApprovedProposal("release".to_string())
    );

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Use the release branch?".to_string(),
        proposed_answer: Some("release".to_string()),
        options: vec![],
        reply: reply_tx,
    }));
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::LeftUnanswered
    );
}

#[test]
fn question_modal_paste_preserves_unicode_multiline_and_filters_controls() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut question = PendingQuestion::new(TuiQuestionRequest {
        question: "Describe the result".to_string(),
        proposed_answer: None,
        options: vec!["short".to_string()],
        reply: reply_tx,
    });

    paste_question_answer(&mut question, "first🙂\r\nsecond\tvalue\u{1b}");
    assert_eq!(question.response, "first🙂\nsecond    value");
    assert!(question
        .error
        .as_deref()
        .is_some_and(|status| status.contains("unsafe paste control")));
    let mut pending = Some(question);
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::Answered("first🙂\nsecond    value".to_string())
    );
}

#[test]
fn f2_command_overlay_requires_a_prompt_and_preserves_question_draft() {
    let (reply_tx, _reply_rx) = oneshot::channel();
    let question = PendingQuestion::new(TuiQuestionRequest {
        question: "Which target?".to_string(),
        proposed_answer: None,
        options: vec!["alpha".to_string()],
        reply: reply_tx,
    });
    let mut overlay = None;
    assert!(!open_prompt_command_overlay(
        KeyCode::F(2),
        false,
        &mut overlay
    ));
    assert!(overlay.is_none(), "idle F2 must be a no-op");

    let before = question.response.clone();
    assert!(open_prompt_command_overlay(
        KeyCode::F(2),
        true,
        &mut overlay
    ));
    assert_eq!(overlay.take().as_deref(), Some(""));
    assert_eq!(question.response, before);
    assert_eq!(
        question.response, before,
        "Escape/close preserves the draft"
    );
}

#[test]
fn f2_context_inspection_is_read_only_during_a_pending_prompt() {
    let directory = tempfile::tempdir().expect("project");
    let mut config = crate::config::NibConfig::default();
    crate::config::save_nib_config_full(directory.path(), &mut config).expect("config");
    let store = crate::session::SessionStore::for_project(directory.path()).expect("store");
    let session = store.try_create_session().expect("session");
    let snapshot = crate::context::snapshot::snapshot_from_bounded_input(
        "execution",
        "sent",
        "configured",
        4_096,
        &crate::context::budget::BoundedLlmInput {
            messages: vec![serde_json::json!({"role":"user","content":"goal"})],
            tools: None,
            approximate_tokens: 88,
            raw_message_count: 1,
            raw_tool_count: 0,
            included_tool_count: 0,
        },
    );
    store
        .record_event(
            &session.id,
            "context_bounded",
            snapshot.to_event_details("run-f2"),
        )
        .expect("snapshot");
    let before = store.load(&session.id).expect("before");
    assert_eq!(
        crate::interactive::command_effect_class(
            &crate::interactive::InteractiveCommand::Context { details: true }
        ),
        crate::interactive::CommandEffectClass::ReadOnlyInspection
    );
    let crate::interactive::InteractiveEffect::Output(output) =
        crate::interactive::execute_interactive_command_in_state(
            crate::interactive::InteractiveCommand::Context { details: true },
            directory.path(),
            "default",
            &store,
            &session.id,
            "waiting_approval",
        )
        .expect("inspect")
    else {
        panic!("context inspection");
    };
    assert!(output.contains("ctx ~"));
    assert!(output.contains("allowance"));
    assert!(!output.contains("secret"));
    let after = store.load(&session.id).expect("after");
    assert_eq!(before.events.len(), after.events.len());
    assert_eq!(before.messages, after.messages);
    let mut overlay = None;
    assert!(open_prompt_command_overlay(
        KeyCode::F(2),
        true,
        &mut overlay
    ));
    assert_eq!(overlay.as_deref(), Some(""));
}

#[test]
fn recovered_question_modal_persists_before_it_closes() {
    let (_directory, store, session_id, invocation_id) = recoverable_question_session();
    let (reply_tx, reply_rx) = oneshot::channel();
    drop(reply_rx);
    let mut pending = Some(PendingQuestion::recovered(
        TuiQuestionRequest {
            question: "Which target?".to_string(),
            proposed_answer: None,
            options: vec!["alpha".to_string(), "beta".to_string()],
            reply: reply_tx,
        },
        RecoveredQuestionTarget {
            store: store.clone(),
            session_id: session_id.clone(),
            invocation_id: invocation_id.to_string(),
        },
    ));

    assert!(!handle_question_key(&mut pending, KeyCode::Char('2')));
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert!(pending.is_none());
    let persisted = store.load(&session_id).expect("answered session");
    assert_eq!(
        persisted.clarifications[0].status,
        crate::session::ClarificationStatus::Answered
    );
    assert_eq!(persisted.clarifications[0].answer.as_deref(), Some("beta"));
}

#[test]
fn question_modal_accepts_q_in_free_form_response() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Search term?".to_string(),
        proposed_answer: None,
        options: vec![],
        reply: reply_tx,
    }));
    let mut approval = None;

    for character in "query".chars() {
        assert!(handle_pending_interaction_key(
            &mut approval,
            &mut pending,
            KeyCode::Char(character)
        ));
    }
    assert!(handle_question_key(&mut pending, KeyCode::Enter));

    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::Answered("query".to_string())
    );
    assert!(pending.is_none());
}

#[test]
fn question_modal_submits_selected_option() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Mode?".to_string(),
        proposed_answer: None,
        options: vec!["plan".to_string(), "execute".to_string()],
        reply: reply_tx,
    }));

    assert!(!handle_question_key(&mut pending, KeyCode::Tab));
    assert!(!handle_question_key(&mut pending, KeyCode::Down));
    assert!(!handle_question_key(&mut pending, KeyCode::Down));
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::Answered("execute".to_string())
    );
}

#[test]
fn question_number_keys_choose_a_labeled_option() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Mode?".to_string(),
        proposed_answer: None,
        options: vec!["plan".to_string(), "execute".to_string()],
        reply: reply_tx,
    }));
    assert!(!handle_question_key(&mut pending, KeyCode::Char('1')));
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::Answered("plan".to_string())
    );
    assert!(pending.is_none());

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Mode?".to_string(),
        proposed_answer: None,
        options: vec!["plan".to_string(), "execute".to_string()],
        reply: reply_tx,
    }));
    assert!(!handle_question_key(&mut pending, KeyCode::Char('y')));
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::Answered("y".to_string())
    );

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Mode?".to_string(),
        proposed_answer: None,
        options: vec!["plan".to_string(), "execute".to_string()],
        reply: reply_tx,
    }));
    handle_question_key(&mut pending, KeyCode::Tab);
    handle_question_key(&mut pending, KeyCode::Tab);
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::LeftUnanswered
    );
}

#[test]
fn question_modal_reports_cancellation() {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Continue?".to_string(),
        proposed_answer: None,
        options: vec!["yes".to_string(), "no".to_string()],
        reply: reply_tx,
    }));

    assert!(handle_question_key(&mut pending, KeyCode::Esc));
    assert_eq!(
        reply_rx.try_recv().unwrap(),
        crate::agent::QuestionOutcome::LeftUnanswered
    );
}

#[test]
fn question_handler_round_trips_ui_response() {
    let (request_tx, request_rx) = mpsc::channel();
    let handler = TuiQuestionHandler { tx: request_tx };
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(crate::agent::QuestionHandler::ask(
            &handler,
            "Mode?",
            &["plan".to_string(), "execute".to_string()],
        ))
    });

    let request = request_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert_eq!(request.question, "Mode?");
    assert_eq!(request.options, ["plan", "execute"]);
    request
        .reply
        .send(crate::agent::QuestionOutcome::Answered(
            "execute".to_string(),
        ))
        .unwrap();

    assert_eq!(handle.join().unwrap(), Ok("execute".to_string()));
}

#[test]
fn tui_shutdown_cancels_and_joins_a_worker_blocked_on_approval() {
    let directory = tempdir().expect("tempdir");
    save_config(directory.path(), &mock_config()).expect("save mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let goal = "ask a question";
    let mut session = store.create_session();
    session.plan = Some(crate::session::Plan::new(
        goal,
        vec![crate::session::PlanStep {
            description: "ask a question".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    ));
    store.save(&mut session).expect("save pending plan");

    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
    let mut worker = Some(
        spawn_tui_agent_worker(
            TuiAgentProfileScope {
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: store.sessions_dir().to_path_buf(),
            },
            session.id.clone(),
            goal.to_string(),
            InteractiveAgentMode::Execute,
            approval_tx,
            question_tx,
            stream_tx,
        )
        .expect("spawn TUI worker"),
    );
    let question = question_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("worker reached a blocking question");
    let mut pending_approval = None;
    let mut pending_question = Some(PendingQuestion::new(question));
    let mut timeline = ActiveTimeline::load(&store, &session.id).expect("active timeline");
    timeline.active_run_id = worker.as_ref().map(|worker| worker.run_id.clone());

    shutdown_agent_worker(
        &mut worker,
        &mut pending_approval,
        &mut pending_question,
        &approval_rx,
        &question_rx,
        &mut stream_rx,
        &mut timeline,
    )
    .expect("cancel and join worker");

    assert!(worker.is_none(), "worker handle must be joined and cleared");
    assert!(timeline.active_run_id.is_none());
    assert_eq!(
        timeline.reconciled_terminal,
        Some(InteractionTerminalOutcome::Cancelled)
    );
    assert!(pending_approval.is_none());
    assert!(pending_question.is_none());
    let persisted = store.load(&session.id).expect("cancelled session");
    let plan = persisted.plan.expect("generated plan");
    assert_eq!(plan.outcome.as_deref(), Some("cancelled_by_user"));
    assert_eq!(plan.steps[plan.current_step_index].status, "Cancelled");
    assert_eq!(
        plan.steps[plan.current_step_index].outcome.as_deref(),
        Some("cancelled_by_user")
    );
    assert!(timeline.live.text.contains("[reconciled] Run cancelled"));
    assert!(!timeline.live.text.contains("[stream ended]"));
    assert_eq!(
        timeline
            .activities
            .iter()
            .filter(|entry| entry.kind == ActivityKind::Reconcile)
            .count(),
        1
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn tui_shutdown_rejects_modal_requests_published_after_initial_cleanup() {
    let session_id = "late-modal-session";
    let run_id = "0123456789abcdef0123456789abcdef";
    let cancellation = CancellationSignal::new();
    let worker_cancellation = cancellation.clone();
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (initial_reply_tx, initial_reply_rx) = oneshot::channel();
    let (approval_reply_tx, mut approval_reply_rx) = oneshot::channel();
    let (question_reply_tx, mut question_reply_rx) = oneshot::channel();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(1);
    stream_tx
        .try_send(SessionStreamEvent {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            event: StreamEvent::StateTransition {
                state: "planning".to_string(),
            },
        })
        .expect("fill the stream before shutdown");

    let handle = std::thread::spawn(move || {
        assert_eq!(
            initial_reply_rx
                .blocking_recv()
                .expect("early question rejection"),
            crate::agent::QuestionOutcome::Cancelled
        );
        assert!(worker_cancellation.is_cancelled());
        // This send cannot finish until shutdown drains the full stream. That
        // drain follows early modal cleanup, so both requests below are late
        // by construction, without a timing sleep or production test hook.
        stream_tx
            .blocking_send(SessionStreamEvent {
                session_id: session_id.to_string(),
                run_id: run_id.to_string(),
                event: StreamEvent::Reconciled {
                    outcome: "cancelled_by_user".to_string(),
                },
            })
            .expect("shutdown reached its post-cleanup stream drain");
        approval_tx
            .send(approval_request(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "run_terminal".to_string(),
                    arguments: json!({}),
                    session_id: Some(session_id.to_string()),
                    project_root: None,
                },
                PermissionLevel::Destructive,
                approval_reply_tx,
            ))
            .expect("publish late approval");
        question_tx
            .send(TuiQuestionRequest {
                question: "This cancelled question must not reopen".to_string(),
                proposed_answer: None,
                options: Vec::new(),
                reply: question_reply_tx,
            })
            .expect("publish late question");
    });
    let mut worker = Some(TuiAgentWorker {
        run_id: run_id.to_string(),
        mode: InteractiveAgentMode::Execute,
        cancellation,
        steering: None,
        handle: Some(handle),
    });
    let mut pending_approval = None;
    let mut pending_question = Some(PendingQuestion::new(TuiQuestionRequest {
        question: "Initial question".to_string(),
        proposed_answer: None,
        options: Vec::new(),
        reply: initial_reply_tx,
    }));
    let mut timeline = ActiveTimeline {
        session_id: session_id.to_string(),
        active_run_id: Some(run_id.to_string()),
        ..ActiveTimeline::default()
    };

    shutdown_agent_worker(
        &mut worker,
        &mut pending_approval,
        &mut pending_question,
        &approval_rx,
        &question_rx,
        &mut stream_rx,
        &mut timeline,
    )
    .expect("cancel and join the late producer");
    assert!(worker.is_none());
    assert!(timeline.active_run_id.is_none());
    assert_eq!(
        timeline.reconciled_terminal,
        Some(InteractionTerminalOutcome::Cancelled)
    );

    refresh_pending_interactions(
        &mut pending_approval,
        &mut pending_question,
        &approval_rx,
        &question_rx,
    );
    assert!(
        pending_approval.is_none(),
        "cancelled approval must not reopen"
    );
    assert!(
        pending_question.is_none(),
        "cancelled question must not reopen"
    );
    assert!(
        !approval_reply_rx
            .try_recv()
            .expect("late approval rejected")
            .granted
    );
    assert_eq!(
        question_reply_rx
            .try_recv()
            .expect("late question rejected"),
        crate::agent::QuestionOutcome::Cancelled
    );
    assert_eq!(
        active_interaction_layer(
            pending_approval.is_some(),
            pending_question.is_some(),
            false,
            None,
            false,
            false,
        ),
        InteractionLayer::Composer
    );
}

#[test]
fn tui_shutdown_times_out_an_unresponsive_worker_without_blocking_restoration() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let session = store.create_session();
    let cancellation = CancellationSignal::new();
    let run_id = "0123456789abcdef0123456789abcdef";
    store
        .record_event(&session.id, "run_started", json!({"run_id": run_id}))
        .expect("run start");
    let (steering, steering_receiver) =
        crate::agent::exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
            .expect("steering channel");
    crate::agent::r#loop::bind_exact_run_steering_receiver(
        &store,
        &session.id,
        run_id,
        &steering_receiver,
    )
    .expect("install exact receiver");
    steering
        .submit("pending credential sk-private-timeout-sentinel")
        .expect("accepted pending steering");
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let steering_receiver = steering_receiver;
        let _ = release_rx.recv();
        drop(steering_receiver);
        let _ = done_tx.send(());
    });
    let mut worker = Some(TuiAgentWorker {
        run_id: run_id.to_string(),
        mode: InteractiveAgentMode::Execute,
        cancellation,
        steering: Some(steering),
        handle: Some(handle),
    });
    let (_approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (_question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (_stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(1);
    let mut pending_approval = None;
    let mut pending_question = None;
    let mut timeline = ActiveTimeline::load(&store, &session.id).expect("timeline");
    timeline.active_run_id = Some(run_id.to_string());

    let started = std::time::Instant::now();
    let error = shutdown_agent_worker_with_timeout(
        &mut worker,
        &mut pending_approval,
        &mut pending_question,
        &approval_rx,
        &question_rx,
        &mut stream_rx,
        &mut timeline,
        std::time::Duration::from_millis(25),
    )
    .expect_err("unresponsive worker must time out");

    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("Run did not stop in time"));
    assert!(!error.to_string().contains("local_error"));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(worker.is_none());
    assert!(timeline.active_run_id.is_none());
    assert!(timeline
        .activities
        .iter()
        .any(|entry| entry.title == "Run did not stop in time"));
    release_tx.send(()).expect("release detached test worker");
    done_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("detached test worker exits");
    let persisted = store.load(&session.id).expect("shutdown evidence");
    assert!(persisted.events.iter().any(|event| {
        event.kind == "steering_delivery_failed"
            && event.details["sequence"] == 1
            && event.details["reason"] == "unresponsive_worker_shutdown"
    }));
}

#[test]
fn tui_steering_clears_the_draft_only_after_exact_run_persistence() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let session = store.create_session();
    let run_id = "0123456789abcdef0123456789abcdef";
    let mut rejected = Composer::from_text("retain this draft");
    assert!(submit_tui_steering_draft(None, &mut rejected).is_err());
    assert_eq!(rejected.input, "retain this draft");

    store
        .record_event(&session.id, "run_started", json!({"run_id": run_id}))
        .expect("run admission");
    let (steering, receiver) =
        crate::agent::exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
            .expect("steering channel");
    crate::agent::r#loop::bind_exact_run_steering_receiver(&store, &session.id, run_id, &receiver)
        .expect("install exact receiver");
    let worker = TuiAgentWorker {
        run_id: run_id.to_string(),
        mode: InteractiveAgentMode::Execute,
        cancellation: CancellationSignal::new(),
        steering: Some(steering),
        handle: None,
    };
    let mut accepted = Composer::from_text("change the verification approach");
    let (text, sequence) = submit_tui_steering_draft(Some(&worker), &mut accepted)
        .expect("durable exact-run steering");

    assert_eq!(text, "change the verification approach");
    assert_eq!(sequence, 1);
    assert!(accepted.input.is_empty());
    assert_eq!(
        accepted.history.entries().last().map(String::as_str),
        Some("change the verification approach")
    );
    let persisted = store.load(&session.id).expect("persisted steering");
    assert!(persisted.events.iter().any(|event| {
        event.kind == "steering_input"
            && event.details["run_id"] == run_id
            && event.details["source"] == "tui"
            && event.details["text"] == "change the verification approach"
    }));
    let detail = SessionDetail::new(&session.id, Some(&persisted), &[]);
    assert!(!detail.text.contains("change the verification approach"));
    let mut timeline = ActiveTimeline::load(&store, &session.id).expect("safe timeline");
    timeline.push_steering("sk-private-live-steering-sentinel", 2);
    let rendered = timeline.rendered_text();
    assert!(!rendered.contains("change the verification approach"));
    assert!(!rendered.contains("sk-private-live-steering-sentinel"));
    assert!(rendered.contains("instruction persisted for the exact active run"));
}

#[test]
fn tui_history_status_and_detail_redact_configured_encoded_secrets() {
    let directory = tempdir().expect("tempdir");
    let secret = "tui/history-secret";
    let encoded = "dHVpL2hpc3Rvcnktc2VjcmV0";
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    config.llm.providers.insert(
        "inactive-openai".to_string(),
        ProviderEntry {
            model: "safe-model".to_string(),
            api_key: Some(secret.to_string()),
            ..ProviderEntry::default()
        },
    );
    save_nib_config_full(directory.path(), &mut config).expect("sensitive config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let session = store.create_session();
    let unsafe_text =
        format!("raw={secret} json=tui\\/history-secret b64={encoded} \u{1b}[2J\u{202e}");
    store
        .try_append_message(&session.id, "user", &unsafe_text)
        .expect("legacy unsafe history");

    let persisted = store.load(&session.id).expect("persisted session");
    let detail = SessionDetail::new(
        &session.id,
        Some(&persisted),
        store.public_sensitive_values(),
    );
    let mut timeline = ActiveTimeline::load(&store, &session.id).expect("timeline");
    timeline.push_status(unsafe_text);
    let public = format!("{}\n{}", detail.text, timeline.rendered_text());
    for forbidden in [
        secret,
        r"tui\/history-secret",
        encoded,
        "\u{1b}",
        "\u{202e}",
    ] {
        assert!(!public.contains(forbidden), "TUI surface: {public:?}");
    }
    assert!(public.contains("[REDACTED]"));
}

#[test]
fn tui_session_detail_redacts_before_per_item_preview_truncation() {
    let directory = tempdir().expect("tempdir");
    let mut session = SessionStore::at_dir(directory.path().join("sessions"))
        .try_create_session()
        .expect("session");
    let secret = format!("detail/boundary/{}", "s".repeat(256));
    let content = format!(
        "{}{}-safe-tail",
        "p".repeat(MAX_SESSION_DETAIL_ITEM_CHARS - secret.len() / 2),
        secret
    );
    session.messages.push(crate::session::SessionMessage {
        index: 0,
        role: "user".to_string(),
        content,
        timestamp: None,
        attachments: Vec::new(),
    });

    let detail = SessionDetail::new(&session.id, Some(&session), std::slice::from_ref(&secret));
    assert!(detail.text.contains("[REDACTED]"), "{:?}", detail.text);
    assert!(
        !detail.text.contains(&secret[..secret.len() / 2 - 8]),
        "credential prefix survived preview truncation: {:?}",
        detail.text
    );
    assert!(detail.text.len() <= MAX_SESSION_DETAIL_BYTES);
}

#[test]
fn tui_plan_mode_worker_persists_an_unapproved_plan_without_interaction() {
    let directory = tempdir().expect("tempdir");
    save_config(directory.path(), &mock_config()).expect("save mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let session = store.create_session();
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (stream_tx, _stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
    let mut worker = spawn_tui_agent_worker(
        TuiAgentProfileScope {
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: store.sessions_dir().to_path_buf(),
        },
        session.id.clone(),
        "plan the requested work".to_string(),
        InteractiveAgentMode::Plan,
        approval_tx,
        question_tx,
        stream_tx,
    )
    .expect("spawn plan worker");

    while !worker.is_finished() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    worker.join().expect("join plan worker");

    assert!(approval_rx.try_recv().is_err());
    assert!(question_rx.try_recv().is_err());
    let persisted = store.load(&session.id).expect("planned session");
    let plan = persisted.plan.as_ref().expect("structured plan");
    assert!(plan.is_structured());
    assert!(!plan.approved);
    assert!(!persisted
        .events
        .iter()
        .any(|event| { matches!(event.kind.as_str(), "tool_started" | "tool_completed") }));
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "approval_required"));
    assert!(persisted.events.iter().any(|event| {
        event.kind == "reconciliation" && event.details["outcome"] == "plan_ready"
    }));
}

#[test]
fn tui_explicit_compaction_is_typed_activity_without_a_synthetic_user_row() {
    let directory = tempdir().expect("tempdir");
    save_config(directory.path(), &mock_config()).expect("save mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "retain this context")
        .expect("user message");
    store
        .try_append_message(&session.id, "assistant", "retain this answer")
        .expect("assistant message");
    let before = store.load(&session.id).expect("before compact").messages;
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
    let mut timeline = ActiveTimeline::load(&store, &session.id).expect("timeline");
    timeline.push_status("[compact] requested".to_string());
    let mut worker = spawn_tui_agent_worker(
        TuiAgentProfileScope {
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: store.sessions_dir().to_path_buf(),
        },
        session.id.clone(),
        String::new(),
        InteractiveAgentMode::Compact,
        approval_tx,
        question_tx,
        stream_tx,
    )
    .expect("spawn compact worker");
    timeline.active_run_id = Some(worker.run_id.clone());
    let mut rejected_steering = Composer::from_text("do not steer maintenance");
    assert!(submit_tui_steering_draft(Some(&worker), &mut rejected_steering).is_err());
    assert_eq!(rejected_steering.input, "do not steer maintenance");
    while !worker.is_finished() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    worker.join().expect("join compact worker");
    drain_stream_events(&mut stream_rx, &mut timeline);

    assert!(approval_rx.try_recv().is_err());
    assert!(question_rx.try_recv().is_err());
    let persisted = store.load(&session.id).expect("compacted session");
    assert_eq!(persisted.messages, before);
    assert!(!persisted.events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "steering_channel_bound" | "steering_admission" | "steering_input"
        )
    }));
    assert_eq!(
        persisted
            .events
            .iter()
            .filter(|event| event.kind == "compression")
            .count(),
        1
    );

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let composer = Composer::default();
    let transcript = timeline.rendered_text();
    terminal
        .draw(|frame| {
            render_current_session_view(
                frame,
                "workspace · sess compact",
                "idle · mock/mock-model",
                &transcript,
                &composer,
                None,
                None,
            )
        })
        .expect("render compact activity");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("compact"));
    assert!(rendered.contains("Context compacted"));
    assert!(!rendered.contains("[user]"));
    assert!(!rendered.contains("explicit context compression"));
}

#[test]
fn tui_background_commands_render_the_same_bounded_safe_projection() {
    let directory = tempdir().expect("tempdir");
    save_config(directory.path(), &mock_config()).expect("save mock config");
    let sessions = SessionStore::for_project(directory.path()).expect("session store");
    let session = sessions.create_session_with_id("tui-background-owner");
    let tasks = crate::daemons::workload::DurableTaskStore::for_project(directory.path())
        .expect("task store");
    for index in 0..=crate::interactive::MAX_INTERACTIVE_BACKGROUND_TASKS {
        tasks
            .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: format!("tui-bg-{index:03}"),
                command: format!("private-tui-command-{index}"),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: sessions.sessions_dir().to_path_buf(),
                session_id: session.id.clone(),
                execution: crate::config::ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1_024,
            })
            .expect("prepare background task");
    }

    for (command, expected_tail) in [
        (
            crate::interactive::InteractiveCommand::Ps,
            "additional tasks omitted",
        ),
        (
            crate::interactive::InteractiveCommand::Stop { task_id: None },
            "/stop <task-id>",
        ),
    ] {
        let InteractiveEffect::Output(output) =
            execute_interactive_command(command, directory.path(), &sessions, &session.id)
                .expect("background command")
        else {
            panic!("background command must be a local output effect");
        };
        assert_eq!(
            output
                .lines()
                .filter(|line| line.trim_start().starts_with("- tui-bg-"))
                .count(),
            crate::interactive::MAX_INTERACTIVE_BACKGROUND_TASKS
        );
        assert!(output.contains("1 additional tasks omitted"));
        assert!(!output.contains("private-tui-command"));

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let composer = Composer::default();
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace · sess tui-background-owner",
                    "idle · mock/mock-model",
                    &output,
                    &composer,
                    None,
                    None,
                )
            })
            .expect("render background command");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(expected_tail), "{rendered}");
        assert!(!rendered.contains("private-tui-command"));
        assert!(!rendered.contains("worker_pid"));
    }
}
