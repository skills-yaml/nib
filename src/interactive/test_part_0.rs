use super::*;

#[test]
fn recovered_question_answer_is_exact_durable_and_run_lease_fenced() {
    let (directory, store, session_id, invocation_id, plan_id) = recoverable_question_fixture();
    let blocked = execute_interactive_command(
        InteractiveCommand::Continue {
            plan_id: plan_id.clone(),
        },
        directory.path(),
        &store,
        &session_id,
    )
    .expect_err("unresolved clarification must block continuation");
    assert!(blocked.contains("unresolved questions"), "{blocked}");
    let lease = store
        .try_acquire_run_lease(&session_id)
        .expect("competing run lease");

    let error =
        persist_recovered_question_answer(&store, &session_id, &invocation_id.to_string(), "beta")
            .expect_err("concurrent owner must fence recovery");
    assert!(error.contains("active agent run"), "{error}");
    assert_eq!(
        store
            .load(&session_id)
            .expect("unchanged session")
            .clarifications[0]
            .status,
        crate::session::ClarificationStatus::Unresolved
    );

    drop(lease);
    assert_eq!(
        persist_recovered_question_answer(&store, &session_id, &invocation_id.to_string(), "beta",)
            .expect("persist exact recovery"),
        plan_id
    );
    let persisted = store.load(&session_id).expect("answered session");
    let clarification = &persisted.clarifications[0];
    assert_eq!(
        clarification.status,
        crate::session::ClarificationStatus::Answered
    );
    assert_eq!(clarification.answer.as_deref(), Some("beta"));
    assert!(persisted.events.iter().any(|event| {
        event.kind == "human_question_answer_received"
            && event.details["invocation_id"] == invocation_id.to_string()
            && event.details["recovered"] == true
    }));
    assert_eq!(
        persisted
            .human_intent
            .last()
            .map(|intent| intent.text.as_str()),
        Some("beta")
    );
    assert_eq!(
        execute_interactive_command(
            InteractiveCommand::Continue {
                plan_id: plan_id.clone(),
            },
            directory.path(),
            &store,
            &session_id,
        )
        .expect("exact plan is eligible after recovery"),
        InteractiveEffect::ContinuePlan {
            plan_id: plan_id.clone(),
            goal: "finish the exact plan".to_string(),
        }
    );
    assert!(execute_interactive_command(
        InteractiveCommand::Continue {
            plan_id: "plan-foreign".to_string(),
        },
        directory.path(),
        &store,
        &session_id,
    )
    .expect_err("foreign plan must fail closed")
    .contains("is not the current session plan"));
    assert!(persist_recovered_question_answer(
        &store,
        &session_id,
        &invocation_id.to_string(),
        "different",
    )
    .expect_err("duplicate answer must fail")
    .contains("already answered"));
}

#[test]
fn recovered_proposal_approval_requires_the_exact_saved_answer() {
    let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
    store
        .update_session(&session_id, |session| {
            session.clarifications[0].proposed_answer = Some("alpha".to_string());
            Ok(())
        })
        .expect("save proposal");
    assert!(persist_recovered_proposed_answer(
        &store,
        &session_id,
        &invocation_id.to_string(),
        "beta",
    )
    .is_err());
    persist_recovered_proposed_answer(&store, &session_id, &invocation_id.to_string(), "alpha")
        .expect("approve exact proposal");
    let session = store.load(&session_id).expect("reloaded session");
    assert_eq!(session.clarifications[0].answer.as_deref(), Some("alpha"));
    assert_eq!(
        session.clarifications[0].outcome.as_deref(),
        Some("approved_proposal")
    );
    assert!(session.events.iter().any(|event| {
        event.kind == "human_question_answer_received"
            && event.details["decision"] == "approved_proposal"
    }));
}

#[test]
fn shared_interaction_reducer_precedence_is_total_and_table_driven() {
    let cases = [
        (
            InteractionState {
                approval_pending: true,
                question_pending: true,
                destructive_confirmation_pending: true,
                selector_or_detail: Some(SelectorDetailKind::Selector),
                completion_pending: true,
                run: InteractionRunState::Running,
            },
            InteractionConsumer::Approval,
        ),
        (
            InteractionState {
                question_pending: true,
                destructive_confirmation_pending: true,
                selector_or_detail: Some(SelectorDetailKind::Selector),
                completion_pending: true,
                ..InteractionState::default()
            },
            InteractionConsumer::Question,
        ),
        (
            InteractionState {
                destructive_confirmation_pending: true,
                selector_or_detail: Some(SelectorDetailKind::Selector),
                completion_pending: true,
                ..InteractionState::default()
            },
            InteractionConsumer::DestructiveConfirmation,
        ),
        (
            InteractionState {
                selector_or_detail: Some(SelectorDetailKind::Selector),
                completion_pending: true,
                ..InteractionState::default()
            },
            InteractionConsumer::Selector,
        ),
        (
            InteractionState {
                selector_or_detail: Some(SelectorDetailKind::Detail),
                completion_pending: true,
                ..InteractionState::default()
            },
            InteractionConsumer::Detail,
        ),
        (
            InteractionState {
                completion_pending: true,
                ..InteractionState::default()
            },
            InteractionConsumer::Completion,
        ),
        (InteractionState::default(), InteractionConsumer::Composer),
    ];

    for (state, expected) in cases {
        assert_eq!(active_interaction_consumer(&state), expected);
        assert_eq!(
            reduce_interaction(&state, InteractionInput::UserAction),
            InteractionReduction::Consumed(expected)
        );
    }
}

#[test]
fn interaction_lifecycle_is_derived_from_authoritative_run_and_modal_state() {
    for (state, expected) in [
        (InteractionState::default(), InteractionLifecycle::Idle),
        (
            InteractionState {
                run: InteractionRunState::Planning,
                ..InteractionState::default()
            },
            InteractionLifecycle::Planning,
        ),
        (
            InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionLifecycle::Running,
        ),
        (
            InteractionState {
                run: InteractionRunState::Reconciling,
                ..InteractionState::default()
            },
            InteractionLifecycle::Reconciling,
        ),
        (
            InteractionState {
                approval_pending: true,
                question_pending: true,
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionLifecycle::WaitingApproval,
        ),
        (
            InteractionState {
                question_pending: true,
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionLifecycle::WaitingQuestion,
        ),
        (
            InteractionState {
                run: InteractionRunState::Completed,
                ..InteractionState::default()
            },
            InteractionLifecycle::Completed,
        ),
        (
            InteractionState {
                run: InteractionRunState::Cancelled,
                ..InteractionState::default()
            },
            InteractionLifecycle::Cancelled,
        ),
        (
            InteractionState {
                run: InteractionRunState::Failed,
                ..InteractionState::default()
            },
            InteractionLifecycle::Failed,
        ),
    ] {
        assert_eq!(state.lifecycle(), expected);
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn shared_interaction_reducer_yields_one_effect_without_fallthrough() {
    let approval_state = InteractionState {
        approval_pending: true,
        question_pending: true,
        completion_pending: true,
        ..InteractionState::default()
    };
    assert_eq!(
        reduce_interaction(
            &approval_state,
            InteractionInput::SubmittedLine("ordinary goal must not run"),
        ),
        InteractionReduction::Consumed(InteractionConsumer::Approval)
    );

    let command_error = reduce_interaction(
        &InteractionState::default(),
        InteractionInput::SubmittedLine("/unknown private-goal-sentinel"),
    );
    assert!(matches!(
        &command_error,
        InteractionReduction::Error {
            consumer: InteractionConsumer::Composer,
            ..
        }
    ));
    assert!(!matches!(&command_error, InteractionReduction::IdleTurn(_)));
    let InteractionReduction::Error { message, .. } = &command_error else {
        unreachable!();
    };
    assert!(!message.contains("private-goal-sentinel"));
    assert!(message.len() <= MAX_STATUS_VALUE_BYTES);
    assert!(!message.chars().any(char::is_control));

    assert_eq!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::SubmittedLine("next ordinary turn"),
        ),
        InteractionReduction::QueueNext("next ordinary turn".to_string())
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::SubmittedLine("/status"),
        ),
        InteractionReduction::Command(InteractiveCommand::Status)
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState::default(),
            InteractionInput::SubmittedLine("queue: durable follow-up"),
        ),
        InteractionReduction::QueueNext("durable follow-up".to_string())
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState::default(),
            InteractionInput::SubmittedLine("ordinary idle turn"),
        ),
        InteractionReduction::IdleTurn("ordinary idle turn".to_string())
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                question_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::QuestionAnswer {
                answer: "42",
                options: &[],
                selected_option: None,
            },
        ),
        InteractionReduction::QuestionAnswered("42".to_string())
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                question_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::QuestionAnswer {
                answer: "text: 42",
                options: &["plan".to_string()],
                selected_option: None,
            },
        ),
        InteractionReduction::QuestionAnswered("42".to_string())
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                question_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::QuestionAnswer {
                answer: "text: :command /status",
                options: &[],
                selected_option: None,
            },
        ),
        InteractionReduction::QuestionAnswered(":command /status".to_string())
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                question_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::QuestionAnswer {
                answer: ":command /status",
                options: &[],
                selected_option: None,
            },
        ),
        InteractionReduction::ModalCommand(InteractiveCommand::Status)
    );
    assert!(matches!(
        reduce_interaction(
            &InteractionState {
                approval_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::ApprovalAnswer("maybe"),
        ),
        InteractionReduction::Error {
            consumer: InteractionConsumer::Approval,
            ..
        }
    ));
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::SubmittedLine("/stop"),
        ),
        InteractionReduction::Error {
            consumer: InteractionConsumer::Composer,
            message: live_stop_requires_id_message().to_string(),
        }
    );
    assert!(matches!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::SubmittedLine("/history unicode"),
        ),
        InteractionReduction::Error {
            consumer: InteractionConsumer::Composer,
            ..
        }
    ));
    for (state, submitted) in [
        (InteractionState::default(), "queue:"),
        (InteractionState::default(), "queue:   "),
        (InteractionState::default(), "steer:"),
        (
            InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            "steer:   ",
        ),
    ] {
        let reduction = reduce_interaction(&state, InteractionInput::SubmittedLine(submitted));
        assert!(matches!(
            reduction,
            InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                ..
            }
        ));
    }
}

#[test]
fn plain_help_and_helpful_tasks_follow_the_normal_message_route() {
    for input in [
        "help",
        "Help?",
        "what can you do?",
        "how can you help",
        "help me refactor this file",
    ] {
        assert_eq!(
            reduce_interaction(
                &InteractionState::default(),
                InteractionInput::SubmittedLine(input)
            ),
            InteractionReduction::IdleTurn(input.to_string()),
        );
        assert_eq!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::SubmittedLine(input)
            ),
            InteractionReduction::QueueNext(input.to_string()),
        );
    }
    assert_eq!(
        reduce_interaction(
            &InteractionState::default(),
            InteractionInput::SubmittedLine("/help")
        ),
        InteractionReduction::Command(InteractiveCommand::Help),
    );
}

#[test]
fn proposed_question_decisions_preserve_approval_and_alternative_meanings() {
    assert_eq!(
        parse_proposed_question_input("1"),
        ProposedQuestionInput::Approve
    );
    assert_eq!(
        parse_proposed_question_input("reject"),
        ProposedQuestionInput::Reject
    );
    assert_eq!(
        parse_proposed_question_input("3"),
        ProposedQuestionInput::InstructOtherwise
    );
    assert_eq!(
        parse_proposed_question_input("text: approve"),
        ProposedQuestionInput::Answer("approve".to_string())
    );
    assert_eq!(
        parse_proposed_question_input("otherwise: beta"),
        ProposedQuestionInput::Answer("beta".to_string())
    );
    assert!(matches!(
        parse_proposed_question_input(""),
        ProposedQuestionInput::Retry(_)
    ));
}

#[test]
fn stream_end_maps_local_error_instead_of_showing_the_token() {
    let mut activities = Vec::new();
    let mut live_state = None;
    apply_stream_event(
        &mut activities,
        StreamEvent::End("local_error".to_string()),
        &mut live_state,
        &[],
    );
    assert_eq!(activities[0].title, "Run stopped");
    assert_eq!(
        activities[0].body,
        "The session was saved. Inspect /status, then retry or run nib doctor."
    );
    assert!(!activities[0].title.contains("local_error"));
    assert!(!activities[0].body.contains("local_error"));
    assert_eq!(
            stream_end_status_line("local_error"),
            "[stream ended] Run stopped. The session was saved. Inspect /status, then retry or run nib doctor."
        );
    let report = user_visible_stop_report("local_error", "sess-1");
    assert!(report.starts_with("Run stopped."));
    assert!(report.contains("Session: sess-1"));
    assert!(!report.contains("Agent run failed"));
    assert!(!report.contains("local_error"));
}

#[test]
fn terminal_outcomes_explain_the_next_action() {
    for outcome in [
        "tool_execution_failed",
        "repeated_tool_failure",
        "required_verification_unresolved",
        "turn_limit_reached",
        "waiting_for_user_input",
        "local_error",
        "tool_scope_outside_worktree",
        "unresponsive_worker_shutdown",
    ] {
        let message = terminal_outcome_message(outcome);
        assert!(!message.title.is_empty());
        assert!(message.detail.contains(['.', '/', ' ']), "{outcome}");
        assert!(
            !message.detail.contains(outcome),
            "internal token leaked: {outcome}"
        );
    }
    let outside = terminal_outcome_message("tool_scope_outside_worktree");
    assert!(outside.detail.contains("project path"));
    assert!(!outside.detail.contains("context_length"));
}

#[test]
fn prompt_prefix_grammar_is_exact_single_pass_and_fail_closed() {
    let question = InteractionState {
        question_pending: true,
        ..InteractionState::default()
    };
    let reduce = |answer| {
        reduce_interaction(
            &question,
            InteractionInput::QuestionAnswer {
                answer,
                options: &[],
                selected_option: None,
            },
        )
    };

    assert_eq!(
        reduce("  :command /status"),
        InteractionReduction::ModalCommand(InteractiveCommand::Status)
    );
    assert_eq!(
        reduce(":COMMAND /status"),
        InteractionReduction::QuestionAnswered(":COMMAND /status".to_string()),
        "the reserved prefix is case-sensitive"
    );
    for malformed in [
        ":command",
        ":command ",
        ":command\t/status",
        ":command/status",
        ":command :command /status",
    ] {
        assert!(matches!(
            reduce(malformed),
            InteractionReduction::Error {
                consumer: InteractionConsumer::Question,
                ..
            }
        ));
    }
    assert_eq!(
        reduce("text: :command /status"),
        InteractionReduction::QuestionAnswered(":command /status".to_string())
    );
    assert_eq!(
        reduce("text: text: retained once"),
        InteractionReduction::QuestionAnswered("text: retained once".to_string()),
        "prefixes are interpreted once rather than recursively"
    );
}

#[test]
fn shared_interaction_reducer_rejects_stale_events_and_recovers_invalid_actions() {
    let state = InteractionState {
        question_pending: true,
        ..InteractionState::default()
    };
    assert_eq!(
        reduce_interaction(
            &state,
            InteractionInput::SessionRunEvent {
                active_session_id: "session-a",
                active_run_id: Some("run-current"),
                event_session_id: "session-a",
                event_run_id: "run-current",
            },
        ),
        InteractionReduction::Consumed(InteractionConsumer::Timeline)
    );
    for (event_session_id, event_run_id, active_run_id) in [
        ("session-b", "run-current", Some("run-current")),
        ("session-a", "run-old", Some("run-current")),
        ("session-a", "run-current", None),
    ] {
        assert_eq!(
            reduce_interaction(
                &state,
                InteractionInput::SessionRunEvent {
                    active_session_id: "session-a",
                    active_run_id,
                    event_session_id,
                    event_run_id,
                },
            ),
            InteractionReduction::StaleEvent
        );
    }

    let invalid = reduce_interaction(&state, InteractionInput::InvalidAction);
    let InteractionReduction::Error { consumer, message } = invalid else {
        panic!("invalid action must become a typed bounded error");
    };
    assert_eq!(consumer, InteractionConsumer::Question);
    assert_eq!(message, "invalid interaction action; input was not applied");
    assert!(message.len() < 128);
    assert!(!message.chars().any(char::is_control));
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn shared_interaction_reducer_owns_modal_answers_and_terminal_outcomes() {
    let options = vec!["plan".to_string(), "execute".to_string()];
    let cases = [
        (
            InteractionState {
                approval_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::ApprovalAnswer("yes"),
            InteractionReduction::ApprovalDecision(InteractionDecision::Accept),
        ),
        (
            InteractionState {
                approval_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::ApprovalAnswer("no"),
            InteractionReduction::ApprovalDecision(InteractionDecision::Reject),
        ),
        (
            InteractionState {
                question_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::QuestionAnswer {
                answer: "2",
                options: &options,
                selected_option: None,
            },
            InteractionReduction::QuestionAnswered("execute".to_string()),
        ),
        (
            InteractionState {
                question_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::QuestionAnswer {
                answer: "",
                options: &options,
                selected_option: Some(0),
            },
            InteractionReduction::QuestionAnswered("plan".to_string()),
        ),
        (
            InteractionState {
                destructive_confirmation_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::ConfirmationAnswer("y"),
            InteractionReduction::ConfirmationDecision(InteractionDecision::Accept),
        ),
    ];
    for (state, input, expected) in cases {
        assert_eq!(reduce_interaction(&state, input), expected);
    }

    assert!(matches!(
        reduce_interaction(
            &InteractionState {
                question_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::QuestionAnswer {
                answer: "3",
                options: &options,
                selected_option: None,
            },
        ),
        InteractionReduction::Error {
            consumer: InteractionConsumer::Question,
            ..
        }
    ));
    for (outcome, failure, expected) in [
        ("completed", false, InteractionTerminalOutcome::Completed),
        (
            "cancelled_by_user",
            false,
            InteractionTerminalOutcome::Cancelled,
        ),
        (
            "waiting_for_user_input",
            false,
            InteractionTerminalOutcome::WaitingForInput,
        ),
        ("provider_error", true, InteractionTerminalOutcome::Failed),
        (
            "unknown_terminal",
            false,
            InteractionTerminalOutcome::Failed,
        ),
    ] {
        assert_eq!(
            reduce_interaction(
                &InteractionState::default(),
                InteractionInput::ReconciledOutcome { outcome, failure },
            ),
            InteractionReduction::Reconciled {
                outcome: outcome.to_string(),
                terminal: expected,
            }
        );
    }
}

#[test]
fn bounded_draft_history_preserves_policy_and_searches_unicode_safely() {
    let mut history = DraftHistory::default();
    assert!(history.search("").matches.is_empty());

    history.remember_submission("alpha 🙂");
    history.remember_submission("alpha 🙂");
    history.remember_submission("beta 界\nnext\0private");
    history.remember_submission("alpha 🙂");
    assert_eq!(
        history.entries().len(),
        3,
        "only consecutive duplicates collapse"
    );
    history.remember_submission("/history alpha");
    history.discard_latest_if("/history alpha");
    assert_eq!(
        history.entries().len(),
        3,
        "search control is not draft history"
    );

    let unicode = history.search("界");
    assert_eq!(unicode.matches.len(), 1);
    assert_eq!(unicode.matches[0].entry_index, 1);
    assert!(unicode.matches[0].display.contains("界"));
    assert!(unicode.matches[0].display.contains('↵'));
    assert!(!unicode.matches[0].display.contains('\0'));

    let query = format!("{}\0", "🙂".repeat(MAX_DRAFT_HISTORY_QUERY_BYTES));
    let bounded = history.search(&query);
    assert!(bounded.query_truncated);
    assert!(bounded.controls_omitted);
    assert!(bounded.query.len() <= MAX_DRAFT_HISTORY_QUERY_BYTES);
    assert!(bounded.query.is_char_boundary(bounded.query.len()));
    assert!(bounded.matches.iter().all(|result| result.display.len()
        <= MAX_DRAFT_HISTORY_DISPLAY_BYTES
        && !result.display.chars().any(char::is_control)));
    assert!(history.search("no-such-draft").matches.is_empty());
}

#[test]
fn bounded_draft_history_evicts_oldest_entries_at_fifty() {
    let mut history = DraftHistory::default();
    for index in 0..=MAX_DRAFT_HISTORY {
        history.remember_submission(&format!("goal-{index}"));
    }
    assert_eq!(history.entries().len(), MAX_DRAFT_HISTORY);
    assert_eq!(
        history.entries().first().map(String::as_str),
        Some("goal-1")
    );
    assert_eq!(
        history.entries().last().map(String::as_str),
        Some("goal-50")
    );
    assert_eq!(history.search("").matches.len(), MAX_DRAFT_HISTORY_RESULTS);
}

#[test]
fn transcript_viewport_clamps_resizes_and_preserves_unpinned_rows_on_append() {
    let mut viewport = TranscriptViewport::default();
    viewport.observe_layout(30, 5);
    assert!(viewport.is_pinned_to_tail());
    assert_eq!(viewport.top_row(), 25);

    viewport.apply(TranscriptViewportAction::PageUp);
    assert!(!viewport.is_pinned_to_tail());
    assert_eq!(viewport.top_row(), 20);
    viewport.observe_layout(40, 5);
    assert_eq!(
        viewport.top_row(),
        20,
        "append must preserve an unpinned viewport"
    );

    viewport.observe_layout(40, 25);
    assert_eq!(
        viewport.top_row(),
        15,
        "resize clamps to the last valid top row"
    );
    viewport.apply(TranscriptViewportAction::PageDown);
    assert_eq!(viewport.top_row(), 15);
    assert!(
        !viewport.is_pinned_to_tail(),
        "down does not implicitly repin"
    );
    viewport.apply(TranscriptViewportAction::JumpToEnd);
    assert!(viewport.is_pinned_to_tail());
    assert_eq!(viewport.top_row(), 15);

    viewport.observe_layout(3, 20);
    assert_eq!(viewport.top_row(), 0);
    viewport.apply(TranscriptViewportAction::PageUp);
    assert!(
        viewport.is_pinned_to_tail(),
        "no upward movement leaves pin state intact"
    );

    viewport.observe_layout(100_000, 7);
    assert_eq!(
        viewport.top_row(),
        99_993,
        "large row counts do not truncate to u16"
    );
}

#[test]
fn transcript_wrapping_and_scroll_reducer_are_unicode_and_narrow_width_safe() {
    assert_eq!(wrapped_display_rows("漢字a", 2), ["漢", "字", "a"]);
    assert_eq!(wrapped_line_count("e\u{301}", 1), 1);
    assert_eq!(bottom_scroll_for_wrap("漢字a", 2, 1), 2);
    assert_eq!(wrapped_display_rows("", 0), [""]);

    let state = InteractionState::default();
    assert_eq!(
        reduce_interaction(
            &state,
            InteractionInput::Transcript(TranscriptViewportAction::PageUp),
        ),
        InteractionReduction::Transcript(TranscriptViewportAction::PageUp)
    );
    assert_eq!(
        reduce_interaction(&state, InteractionInput::OpenHistorySearch),
        InteractionReduction::OpenHistorySearch { query: None }
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                completion_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::OpenHistorySearch,
        ),
        InteractionReduction::Consumed(InteractionConsumer::Completion)
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                approval_pending: true,
                ..InteractionState::default()
            },
            InteractionInput::Transcript(TranscriptViewportAction::PageUp),
        ),
        InteractionReduction::Transcript(TranscriptViewportAction::PageUp)
    );

    let mut viewport = TranscriptViewport::default();
    viewport.observe_layout(40, 5);
    viewport.apply(TranscriptViewportAction::Lines(-1));
    assert!(!viewport.is_pinned_to_tail());
    assert_eq!(viewport.top_row(), 34);
    viewport.apply(TranscriptViewportAction::Lines(3));
    assert_eq!(viewport.top_row(), 35);
}

#[test]
fn shared_parser_defines_the_complete_interactive_command_vocabulary() {
    let mut names = HashSet::new();
    let help = interactive_help();
    for spec in INTERACTIVE_COMMANDS {
        assert!(!spec.name.is_empty());
        assert!(!spec.usage.is_empty());
        assert!(!spec.summary.is_empty());
        assert!(help.contains(spec.usage));
        assert_eq!(spec.availability, InteractiveAvailability::Available);
        assert!(matches!(
            spec.mutability,
            InteractiveMutability::ReadOnly
                | InteractiveMutability::Runtime
                | InteractiveMutability::Session
                | InteractiveMutability::Configuration
        ));
        for candidate in spec.completion.candidates {
            assert!(spec.usage.contains(candidate));
        }
        for candidate in spec.completion.argument_after {
            assert!(spec.completion.candidates.contains(candidate));
        }
        for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
            assert!(names.insert(name), "duplicate command token: {name}");
            let command = if spec.arguments == InteractiveArgumentSchema::RequiredText {
                format!("/{name} example")
            } else {
                format!("/{name}")
            };
            assert!(
                parse_interactive_command(&command).is_ok(),
                "{command} must be shared by chat and TUI"
            );
        }
    }
    assert_eq!(parse_interactive_command("hello").unwrap(), None);
    assert!(parse_interactive_command("/unknown").is_err());
    assert!(parse_interactive_command("/skills invalid").is_err());
    assert!(parse_interactive_command("/mcp add only-name").is_err());
    for invalid in [
        "/status extra",
        "/context verbose",
        "/rename",
        "/permissions unsupported",
        "/model one two",
        "/stop one two",
    ] {
        assert!(
            parse_interactive_command(invalid).is_err(),
            "registry argument schema must reject {invalid}"
        );
    }
    assert_eq!(
        parse_interactive_command("/q")
            .unwrap()
            .unwrap()
            .spec()
            .name,
        "quit"
    );
}

#[test]
fn completion_is_bounded_case_insensitive_and_uses_registry_metadata() {
    let root = interactive_completions("/");
    assert!(root.len() <= 32);
    assert!(root.iter().any(|item| item.insertion == "/session"));
    assert!(root.iter().any(|item| item.insertion == "/q"));
    assert_eq!(
        interactive_completions("/PRO")
            .iter()
            .map(|item| item.insertion.as_str())
            .collect::<Vec<_>>(),
        vec!["/providers"]
    );

    let skills = interactive_completions("/skills ");
    assert_eq!(
        skills
            .iter()
            .map(|item| item.insertion.as_str())
            .collect::<Vec<_>>(),
        vec!["/skills list", "/skills install ", "/skills remove "]
    );
    assert!(parse_interactive_command("/skills list").is_ok());
    for incomplete in ["/skills install ", "/skills remove "] {
        assert!(
            parse_interactive_command(incomplete).is_err(),
            "free-form arguments must not be guessed or executed"
        );
    }
    assert!(interactive_completions("/skills install ").is_empty());
    assert!(interactive_completions("/unknown").is_empty());
    assert!(interactive_completions("not a command").is_empty());
}

#[test]
fn shared_session_selection_is_ordered_bounded_and_read_only() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let older = store
        .try_create_session_with_id("older-session")
        .expect("older session");
    let newer = store
        .try_create_session_with_id("newer-session")
        .expect("newer session");
    store
        .try_append_message(&older.id, "user", "older goal")
        .expect("older message");
    store
        .try_append_message(&newer.id, "user", &"newer goal ".repeat(400))
        .expect("newer message");

    let anchor = chrono::Utc::now();
    let mut older_state = store
        .load_result(&older.id)
        .expect("load older for timestamp")
        .expect("older session");
    older_state.messages[0].timestamp = Some(anchor - chrono::Duration::seconds(1));
    store.save(&mut older_state).expect("save older timestamp");
    let mut newer_state = store
        .load_result(&newer.id)
        .expect("load newer for timestamp")
        .expect("newer session");
    newer_state.messages[0].timestamp = Some(anchor);
    store.save(&mut newer_state).expect("save newer timestamp");

    let before_older = store
        .load_result(&older.id)
        .expect("load older")
        .expect("older session");
    let before_newer = store
        .load_result(&newer.id)
        .expect("load newer")
        .expect("newer session");
    let selection = interactive_session_selection(&store, &older.id).expect("selection");

    assert_eq!(selection.omitted, 0);
    assert_eq!(selection.candidates.len(), 2);
    assert_eq!(selection.candidates[0].id, newer.id);
    assert!(selection
        .candidates
        .iter()
        .find(|candidate| candidate.id == older.id)
        .is_some_and(|candidate| candidate.is_active));
    assert!(selection.candidates.iter().all(
        |candidate| candidate.preview.chars().count() <= MAX_INTERACTIVE_SESSION_PREVIEW_CHARS
    ));
    assert_eq!(
        store.load_result(&older.id).expect("reload older"),
        Some(before_older)
    );
    assert_eq!(
        store.load_result(&newer.id).expect("reload newer"),
        Some(before_newer.clone())
    );
    let previewed_newer = selection
        .candidates
        .iter()
        .find(|candidate| candidate.id == newer.id)
        .expect("previewed newer session")
        .clone();
    assert_eq!(
        validate_interactive_session_target(&store, &previewed_newer).expect("unchanged target"),
        before_newer
    );

    // A valid same-ID replacement with the same revision is still stale relative
    // to what the user previewed and must not be activated.
    let mut replaced = before_newer.clone();
    replaced.messages[0].content = "valid replacement content".to_string();
    std::fs::write(
        store.sessions_dir().join(format!("{}.json", newer.id)),
        serde_json::to_vec(&replaced).expect("serialize replacement"),
    )
    .expect("replace target");
    let stale = validate_interactive_session_target(&store, &previewed_newer)
        .expect_err("same-revision replacement must fail closed");
    assert!(stale.contains("changed since it was previewed"));
    assert!(interactive_session_candidate(&store, "missing", &older.id).is_err());
}

#[test]
fn legacy_sensitive_session_ids_are_rejected_before_selector_preview() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(project.path(), &mut config).expect("initial config");
    let unprotected = SessionStore::for_project(project.path()).expect("initial store");
    unprotected
        .try_create_session_with_id("legacy-private-session")
        .expect("legacy session");

    let mut config = load_nib_config_full(project.path()).expect("reload config");
    config.llm.providers.insert(
        "inactive-openai".to_string(),
        crate::config::ProviderEntry {
            model: "fixture".to_string(),
            api_key: Some("legacy-private-session".to_string()),
            ..Default::default()
        },
    );
    save_nib_config_full(project.path(), &mut config).expect("sensitive config");
    let scope = resolve_interactive_profile_scope(project.path()).expect("profile scope");
    let store = scope.session_store();
    let safe_active = store.try_create_session().expect("safe active session");

    let listing_error = interactive_session_selection(store, &safe_active.id)
        .expect_err("sensitive legacy listing");
    assert_eq!(
        listing_error,
        "failed to list sessions: session identifier conflicts with configured sensitive data"
    );
    assert!(!listing_error.contains("legacy-private-session"));
    let preview_error =
        interactive_session_candidate(store, "legacy-private-session", &safe_active.id)
            .expect_err("sensitive legacy preview");
    assert_eq!(
        preview_error,
        "session identifier conflicts with configured sensitive data"
    );
    assert!(!preview_error.contains("legacy-private-session"));
}

#[test]
fn structured_failure_events_render_once_with_separate_session_context() {
    let event = StreamEvent::Failure {
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
            "private diagnostic",
        ),
        session_id: Some("session-123".to_string()),
    };

    let StreamDisplay::Status(report) =
        display_stream_event(event.clone()).expect("failure display")
    else {
        panic!("failure event must be a status")
    };
    assert_eq!(report.matches("LLM request failed").count(), 1);
    assert!(report.contains("LLM-AUTH"));
    assert!(report.contains("Session: session-123"));
    assert!(!report.contains("private diagnostic"));
    assert!(!report.contains("agent run failed"));
    assert!(!report.contains("[red]"));
    assert!(!report.contains("\u{1b}"));

    let mut activities = Vec::new();
    let mut live_state = None;
    apply_stream_event(&mut activities, event, &mut live_state, &[]);
    let projected = activities[0].render_line();
    assert_eq!(projected.matches("LLM request failed").count(), 1);
    assert!(projected.contains("LLM request failed [LLM-AUTH]"));
    assert!(!projected.contains("private diagnostic"));
}

#[test]
fn public_stream_presentation_redacts_encoded_secrets_and_terminal_controls() {
    let secret = "plain/output-secret".to_string();
    let StreamDisplay::Content(content) = display_stream_event_with_sensitive_values(
            StreamEvent::Content(format!(
                "raw={secret} json=plain\\/output-secret b64=cGxhaW4vb3V0cHV0LXNlY3JldA== \u{1b}[2J\u{202e}tail"
            )),
            std::slice::from_ref(&secret),
        )
        .expect("content display")
        else {
            panic!("content event must remain content")
        };
    assert!(!content.contains(&secret));
    assert!(!content.contains(r"plain\/output-secret"));
    assert!(!content.contains("cGxhaW4vb3V0cHV0LXNlY3JldA"));
    assert!(!content.contains('\u{1b}'));
    assert!(!content.contains('\u{202e}'));
    assert!(content.contains("[REDACTED]"));
    assert!(content.len() <= MAX_PUBLIC_PRESENTATION_BYTES);
}

#[test]
fn approval_stream_presentations_never_render_raw_arguments() {
    let event = StreamEvent::ApprovalRequired {
        tool_name: "run_terminal".to_string(),
    };
    let StreamDisplay::Status(display) =
        display_stream_event(event.clone()).expect("approval display")
    else {
        panic!("approval is a status")
    };
    assert_eq!(display, "[approval required] run_terminal");
    let mut activities = Vec::new();
    let mut live_state = None;
    apply_stream_event(&mut activities, event, &mut live_state, &[]);
    let rendered = activities[0].render_line();
    assert!(rendered.contains("run_terminal"));
    assert!(!rendered.contains("RAW_APPROVAL_SENTINEL"));
    assert!(!rendered.contains("sk-privateapproval"));
    assert!(!rendered.contains('\u{1b}'));

    let StreamDisplay::Status(unsafe_name) = display_stream_event(StreamEvent::ApprovalRequired {
        tool_name: "run_terminal\u{1b}[2J\nsk-privateapproval123456".to_string(),
    })
    .expect("unsafe approval display") else {
        panic!("approval is a status")
    };
    assert!(!unsafe_name.contains('\u{1b}'));
    assert!(!unsafe_name.contains('\n'));
    assert!(!unsafe_name.contains("sk-privateapproval"));

    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let session = store.try_create_session().expect("session");
    store
        .record_event(
            &session.id,
            "approval_required",
            serde_json::json!({
                "kind": "tool",
                "tool_name": "run_terminal",
                "arguments": "RAW_APPROVAL_SENTINEL"
            }),
        )
        .expect("persist approval");
    store
        .record_event(
            &session.id,
            "approval_required",
            serde_json::json!({"kind": "plan", "step_count": 2}),
        )
        .expect("persist plan approval");
    let reloaded = store.load(&session.id).expect("reload approvals");
    let approvals = project_session_activities(&reloaded, &[])
        .into_iter()
        .filter(|entry| entry.kind == ActivityKind::Approval)
        .collect::<Vec<_>>();
    assert_eq!(approvals.len(), 2);
    assert_eq!(approvals[0].title, "run_terminal approval requested");
    assert_eq!(approvals[1].title, "plan approval requested");
    assert_eq!(approvals[1].body, "step_count=2");
    assert!(!format!("{approvals:?}").contains("RAW_APPROVAL_SENTINEL"));
}

#[test]
fn shared_effects_change_sessions_models_and_mcp_configuration() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(project.path(), &mut config).expect("config");
    let store = SessionStore::for_project(project.path()).expect("store");
    let resolution = resolve_session(&store, None).expect("session");
    let session_id = resolution.session_id().to_string();

    let effect = execute_interactive_command(
        InteractiveCommand::Clear,
        project.path(),
        &store,
        &session_id,
    )
    .expect("clear");
    assert!(matches!(effect, InteractiveEffect::SessionChanged { .. }));

    execute_interactive_command(
        InteractiveCommand::Model {
            selection: Some("custom-mock".to_string()),
        },
        project.path(),
        &store,
        &session_id,
    )
    .expect("model");
    assert_eq!(
        load_nib_config_full(project.path()).unwrap().llm.providers["mock"].model,
        "custom-mock"
    );

    execute_interactive_command(
        InteractiveCommand::Mcp(McpCommand::Add {
            name: "local".to_string(),
            command: "echo".to_string(),
            args: vec!["--stdio".to_string()],
        }),
        project.path(),
        &store,
        &session_id,
    )
    .expect("MCP add");
    assert!(load_nib_config_full(project.path())
        .unwrap()
        .mcp
        .servers
        .contains_key("local"));
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn live_input_distinguishes_queue_and_exact_run_steering() {
    assert_eq!(
        classify_composer_submit(false),
        ComposerSubmitKind::IdleTurn
    );
    assert_eq!(
        classify_composer_submit(true),
        ComposerSubmitKind::QueueNext
    );
    assert!(steer_hint_message().contains("Enter queues"));
    assert_eq!(parse_queue_line("queue: next goal"), Some("next goal"));
    assert_eq!(parse_steer_line("steer: adjust now"), Some("adjust now"));
    assert_eq!(parse_queue_line("hello"), None);
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::SubmittedLine("steer: adjust now"),
        ),
        InteractionReduction::SteerCurrent("adjust now".to_string())
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::ComposerSubmit("steer: adjust now"),
        ),
        InteractionReduction::SteerCurrent("adjust now".to_string())
    );
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::ComposerSubmit("/quit"),
        ),
        InteractionReduction::Command(InteractiveCommand::Quit)
    );
    assert!(matches!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::ComposerSubmit("/unknown must-never-be-a-goal"),
        ),
        InteractionReduction::Error {
            consumer: InteractionConsumer::Composer,
            ..
        }
    ));
    assert_eq!(
        reduce_interaction(
            &InteractionState {
                run: InteractionRunState::Running,
                ..InteractionState::default()
            },
            InteractionInput::ComposerSubmit("ordinary queued goal"),
        ),
        InteractionReduction::QueueNext("ordinary queued goal".to_string())
    );

    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let session = store.try_create_session().expect("session");
    let queued = persist_queued_follow_up(&store, &session.id, "run the follow-up", "composer")
        .expect("persist");
    assert_eq!(queued.text, "run the follow-up");
    assert_eq!(
        queued_follow_up_count(&store, &session.id).expect("count"),
        1
    );
    let disposition =
        queue_disposition_message(&store, &session.id, "cancelled").expect("disposition");
    assert!(disposition.contains("retained on session"));
    let taken = take_next_queued_follow_up(&store, &session.id)
        .expect("take")
        .expect("queued text");
    assert_eq!(taken, "run the follow-up");
    assert_eq!(
        queued_follow_up_count(&store, &session.id).expect("empty"),
        0
    );

    let private_run_id = "0123456789abcdef0123456789abcdef";
    store
        .record_event(
            &session.id,
            "steering_input",
            serde_json::json!({
                "run_id": private_run_id,
                "sequence": 1,
                "source": "plain",
                "text": "credential sk-private-activity-sentinel\u{1b}[2J",
            }),
        )
        .expect("legacy steering evidence");
    let persisted = store.load(&session.id).expect("steering session");
    let steering = project_session_activities(&persisted, &[])
        .into_iter()
        .find(|activity| activity.title == "steer")
        .expect("typed steering activity");
    assert_eq!(steering.kind, ActivityKind::User);
    assert!(steering.body.contains("sequence=1"));
    assert!(steering.body.contains("source=plain"));
    assert!(!steering.body.contains("sk-private-activity-sentinel"));
    assert!(!steering.body.contains('\u{1b}'));
    assert!(!steering.render_line().contains(private_run_id));
}

#[test]
fn plan_prompt_returns_a_typed_plan_mode_effect() {
    let directory = tempdir().expect("project");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let session = store.try_create_session().expect("session");

    let effect = execute_interactive_command(
        InteractiveCommand::Plan {
            prompt: Some("inspect without mutation".to_string()),
        },
        directory.path(),
        &store,
        &session.id,
    )
    .expect("plan effect");

    assert_eq!(
        effect,
        InteractiveEffect::RunAgent {
            goal: "inspect without mutation".to_string(),
            mode: InteractiveAgentMode::Plan,
        }
    );
    assert_eq!(InteractiveAgentMode::Execute.as_str(), "execute");
    assert_eq!(InteractiveAgentMode::Plan.as_str(), "plan");
    assert_eq!(InteractiveAgentMode::Compact.as_str(), "compact");
}

#[test]
fn diff_uses_project_fallback_and_truncates_on_a_utf8_boundary() {
    let repository = git_repository();
    let store = SessionStore::for_project(repository.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    std::fs::write(
        repository.path().join("tracked.txt"),
        format!("project fallback\n{}\n", "界".repeat(MAX_DIFF_BYTES)),
    )
    .expect("project change");

    let InteractiveEffect::Output(diff) = execute_interactive_command(
        InteractiveCommand::Diff,
        repository.path(),
        &store,
        &session.id,
    )
    .expect("project diff") else {
        panic!("diff output");
    };

    assert!(diff.contains("project fallback"));
    assert!(diff.ends_with("[diff truncated]"));
    assert!(diff.len() <= MAX_DIFF_BYTES + "\n[diff truncated]".len());
    assert!(std::str::from_utf8(diff.as_bytes()).is_ok());
}

#[test]
fn diff_redacts_credentials_before_the_raw_output_bound() {
    let repository = git_repository();
    let secret = format!("diff/boundary/{}", "s".repeat(512));
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    config.llm.providers.insert(
        "inactive-openai".to_string(),
        ProviderEntry {
            model: "safe-model".to_string(),
            api_key: Some(secret.clone()),
            ..ProviderEntry::default()
        },
    );
    save_nib_config_full(repository.path(), &mut config).expect("sensitive config");
    let store = SessionStore::for_project(repository.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    std::fs::write(
        repository.path().join("tracked.txt"),
        format!("{}{}-safe-tail\n", "p".repeat(MAX_DIFF_BYTES - 256), secret),
    )
    .expect("boundary diff");

    let InteractiveEffect::Output(diff) = execute_interactive_command(
        InteractiveCommand::Diff,
        repository.path(),
        &store,
        &session.id,
    )
    .expect("safe diff") else {
        panic!("diff output")
    };

    assert!(diff.contains("[REDACTED]"), "{diff:?}");
    assert!(
        !diff.contains(&secret[..128]),
        "credential prefix survived diff truncation: {diff:?}"
    );
    assert!(diff.len() <= MAX_DIFF_BYTES + "\n[diff truncated]".len());
}

#[test]
fn oversized_diff_fails_closed_for_long_percent_encoded_credentials() {
    let repository = git_repository();
    let secret = "A ".repeat(10_000);
    let percent_secret = "A%20".repeat(10_000);
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    config.llm.providers.insert(
        "inactive-openai".to_string(),
        ProviderEntry {
            model: "safe-model".to_string(),
            api_key: Some(secret),
            ..ProviderEntry::default()
        },
    );
    save_nib_config_full(repository.path(), &mut config).expect("sensitive config");
    let store = SessionStore::for_project(repository.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    std::fs::write(
        repository.path().join("tracked.txt"),
        format!("{}{}\n", "p".repeat(30 * 1024), percent_secret),
    )
    .expect("oversized encoded diff");

    let InteractiveEffect::Output(diff) = execute_interactive_command(
        InteractiveCommand::Diff,
        repository.path(),
        &store,
        &session.id,
    )
    .expect("safe diff") else {
        panic!("diff output")
    };

    assert_eq!(diff, "[REDACTED]");
    assert!(!diff.contains("A%20"));
}

#[test]
fn review_selects_the_owned_session_worktree() {
    let repository = git_repository();
    let store = SessionStore::for_project(repository.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    let mut manager =
        crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf());
    let worktree = manager
        .create_for_session(&session.id)
        .expect("owned session worktree");
    std::fs::write(
        repository.path().join("tracked.txt"),
        "project root change\n",
    )
    .expect("project change");
    std::fs::write(worktree.join("tracked.txt"), "owned worktree change\n")
        .expect("worktree change");

    let InteractiveEffect::Output(review) = execute_interactive_command(
        InteractiveCommand::Review,
        repository.path(),
        &store,
        &session.id,
    )
    .expect("owned review") else {
        panic!("review output");
    };

    assert!(review.contains("owned worktree change"));
    assert!(!review.contains("project root change"));
}

#[test]
fn diff_rejects_a_replaced_owned_session_worktree() {
    let repository = git_repository();
    let store = SessionStore::for_project(repository.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    let mut manager =
        crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf());
    let worktree = manager
        .create_for_session(&session.id)
        .expect("owned session worktree");
    let displaced = worktree.with_extension("owned-away");
    std::fs::rename(&worktree, &displaced).expect("displace owned worktree");
    std::fs::create_dir(&worktree).expect("replacement worktree");
    std::fs::write(worktree.join("sentinel"), "replacement").expect("replacement sentinel");

    let error = execute_interactive_command(
        InteractiveCommand::Diff,
        repository.path(),
        &store,
        &session.id,
    )
    .expect_err("replaced worktree must fail closed");

    assert!(
        error.contains("ownership") || error.contains("identity"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(worktree.join("sentinel")).expect("replacement retained"),
        "replacement"
    );
}

#[test]
fn review_rejects_an_ownership_record_that_escapes_managed_state() {
    let repository = git_repository();
    let store = SessionStore::for_project(repository.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    let mut manager =
        crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf());
    manager
        .create_for_session(&session.id)
        .expect("owned session worktree");
    let ownership_directory = repository.path().join(".nib/worktree-ownership");
    let ownership_path = std::fs::read_dir(&ownership_directory)
        .expect("ownership directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("ownership record");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ownership_path).expect("read ownership record"))
            .expect("ownership JSON");
    record["worktree_path"] = serde_json::Value::String("/tmp/nib-escape".to_string());
    std::fs::write(
        &ownership_path,
        serde_json::to_vec(&record).expect("encode ownership record"),
    )
    .expect("replace ownership contents");

    let error = execute_interactive_command(
        InteractiveCommand::Review,
        repository.path(),
        &store,
        &session.id,
    )
    .expect_err("escaped durable ownership must fail closed");

    assert!(error.contains("escapes managed state"), "{error}");
}

#[test]
fn queued_startup_failure_retains_fifo_and_records_audit() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let session = store.try_create_session().expect("session");
    let first = persist_queued_follow_up(&store, &session.id, "first", "composer")
        .expect("first queued item");
    let second = persist_queued_follow_up(&store, &session.id, "second", "composer")
        .expect("second queued item");

    let error = claim_next_queued_follow_up_after_startup::<()>(&store, &session.id, |_| {
        Err("deterministic worker startup failure".to_string())
    })
    .expect_err("startup must fail");
    assert!(error.contains("remains queued"));

    let persisted = store
        .load_result(&session.id)
        .expect("load session")
        .expect("session");
    assert_eq!(persisted.queued_follow_ups, vec![first.clone(), second]);
    let audit = persisted.events.last().expect("startup failure audit");
    assert_eq!(audit.kind, "queued_follow_up_start_failed");
    assert_eq!(audit.details["queue_id"], first.id);
    assert_eq!(audit.details["phase"], "worker_startup");
    assert_eq!(audit.details["disposition"], "retained");
    assert!(!audit.details.to_string().contains("deterministic worker"));
}

#[test]
fn queued_claim_is_fifo_and_activation_failure_restores_once() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let session = store.try_create_session().expect("session");
    let first = persist_queued_follow_up(&store, &session.id, "first", "composer")
        .expect("first queued item");
    let second = persist_queued_follow_up(&store, &session.id, "second", "composer")
        .expect("second queued item");

    let (claimed, prepared) =
        claim_next_queued_follow_up_after_startup(&store, &session.id, |_| Ok("prepared worker"))
            .expect("claim after startup")
            .expect("queued item");
    assert_eq!(prepared, "prepared worker");
    assert_eq!(claimed, first);
    assert_eq!(
        store
            .load_result(&session.id)
            .expect("load claimed session")
            .expect("claimed session")
            .queued_follow_ups,
        vec![second.clone()]
    );

    restore_queued_follow_up_after_start_failure(&store, &session.id, claimed.clone())
        .expect("restore failed activation");
    restore_queued_follow_up_after_start_failure(&store, &session.id, claimed.clone())
        .expect("idempotent restore");
    let restored = store
        .load_result(&session.id)
        .expect("load restored session")
        .expect("restored session");
    assert_eq!(restored.queued_follow_ups, vec![claimed, second]);
    assert_eq!(
        restored
            .events
            .iter()
            .filter(|event| event.kind == "queued_follow_up_start_committed")
            .count(),
        1
    );
    assert_eq!(
        restored
            .events
            .iter()
            .filter(|event| event.kind == "queued_follow_up_start_failed")
            .count(),
        2
    );
}
