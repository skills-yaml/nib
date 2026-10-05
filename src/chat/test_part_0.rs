use super::*;

#[test]
fn plain_signal_owner_exits_when_idle_and_cancels_only_the_registered_turn() {
    let owner = PlainSignalOwner::detached();
    assert!(owner.dispatch_for_test(), "idle Ctrl+C owns process exit");

    let cancellation = nib::agent::CancellationSignal::new();
    let registration = owner.register(cancellation.clone());
    assert!(!owner.dispatch_for_test(), "active Ctrl+C cancels the turn");
    assert!(cancellation.is_cancelled());

    drop(registration);
    assert!(
        owner.dispatch_for_test(),
        "turn completion restores idle exit"
    );
}

#[test]
fn shared_interaction_reducer_routes_plain_numbered_layers() {
    for (state, expected) in [
        (
            InteractionState {
                destructive_confirmation_pending: true,
                ..InteractionState::default()
            },
            InteractionConsumer::DestructiveConfirmation,
        ),
        (
            InteractionState {
                selector_or_detail: Some(SelectorDetailKind::Selector),
                ..InteractionState::default()
            },
            InteractionConsumer::Selector,
        ),
        (
            InteractionState {
                completion_pending: true,
                ..InteractionState::default()
            },
            InteractionConsumer::Completion,
        ),
    ] {
        require_plain_consumer(&state, "1\n", expected).expect("owned numbered input");
    }
}

#[test]
fn plain_draft_history_search_uses_bounded_numbered_selection_and_confirmation() {
    let mut history = DraftHistory::default();
    history.remember_submission("older draft");
    history.remember_submission("fix 🙂 unicode");
    let input = ConsoleInput::new(std::io::Cursor::new(b"1\ny\n"));

    let selected = select_draft_history_from_console(&input, &history, Some("🙂"))
        .expect("history search")
        .expect("selected draft");
    assert_eq!(selected, "fix 🙂 unicode");

    let no_input = ConsoleInput::new(std::io::Cursor::new(Vec::<u8>::new()));
    assert_eq!(
        select_draft_history_from_console(&no_input, &history, Some("no match"))
            .expect("bounded no-match search"),
        None
    );

    let invalid = ConsoleInput::new(std::io::Cursor::new(b"0\n"));
    assert!(
        select_draft_history_from_console(&invalid, &history, Some("draft"))
            .expect_err("zero is never a valid numbered selection")
            .contains("out of range")
    );
}

#[test]
fn plain_draft_history_empty_search_requires_no_input() {
    let input = ConsoleInput::new(std::io::Cursor::new(Vec::<u8>::new()));
    assert_eq!(
        select_draft_history_from_console(&input, &DraftHistory::default(), None)
            .expect("empty history"),
        None
    );
}

#[test]
fn interactive_mode_resolution_is_deterministic_and_explicit_modes_win() {
    let capable = terminal_capabilities(true, true, Some("xterm-256color"));
    assert_eq!(
        resolve_interactive_mode(&ChatArgs::default(), &capable).unwrap(),
        ModeSelection {
            mode: InteractiveMode::Tui,
            auto_fallback_notice: None,
        }
    );

    let redirected = terminal_capabilities(true, false, Some("xterm-256color"));
    assert_eq!(
        resolve_interactive_mode(&ChatArgs::default(), &redirected).unwrap(),
        ModeSelection {
            mode: InteractiveMode::Plain,
            auto_fallback_notice: None,
        }
    );

    let dumb = terminal_capabilities(true, true, Some("dumb"));
    let fallback = resolve_interactive_mode(&ChatArgs::default(), &dumb).unwrap();
    assert_eq!(fallback.mode, InteractiveMode::Plain);
    assert_eq!(
        fallback.auto_fallback_notice,
        Some("TERM=dumb does not support the full-screen TUI")
    );

    let plain = ChatArgs {
        plain: true,
        ..Default::default()
    };
    assert_eq!(
        resolve_interactive_mode(&plain, &capable).unwrap().mode,
        InteractiveMode::Plain
    );

    let tui = ChatArgs {
        tui: true,
        ..Default::default()
    };
    assert_eq!(
        resolve_interactive_mode(&tui, &capable).unwrap().mode,
        InteractiveMode::Tui
    );
    assert!(resolve_interactive_mode(&tui, &redirected)
        .unwrap_err()
        .contains("use --plain instead"));

    let conflict = ChatArgs {
        plain: true,
        tui: true,
        ..Default::default()
    };
    assert!(resolve_interactive_mode(&conflict, &capable).is_err());
}

#[test]
#[serial]
fn plain_initial_goal_is_submitted_exactly_once() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let _cwd = CurrentDirGuard::enter(project.path());
    let goal = "initial interactive goal";

    run_chat_with_input(
        &ChatArgs {
            run: Some(goal.to_string()),
            plain: true,
            ..Default::default()
        },
        Cursor::new(b"y\n\n/quit\n"),
    )
    .expect("plain initial goal");

    let store = SessionStore::for_project(project.path()).expect("session store");
    let sessions = store.list_result().expect("sessions");
    assert_eq!(sessions.len(), 1);
    let session = store
        .load_result(&sessions[0])
        .expect("load session")
        .expect("session");
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|message| message.role == "user" && message.content == goal)
            .count(),
        1
    );
}

#[test]
fn plain_plan_mode_persists_an_unapproved_plan_without_execution() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
    let signal_owner = PlainSignalOwner::detached();
    let scope = PlainAgentScope {
        project: project.path(),
        profile_id: "default",
        session_store: &store,
        signal_owner: &signal_owner,
    };

    execute_agent_step(
        &scope,
        &session.id,
        "plan a safe inspection",
        InteractiveAgentMode::Plan,
        &input,
    )
    .expect("plan-only run");

    let persisted = store.load(&session.id).expect("planned session");
    let plan = persisted.plan.expect("structured plan");
    assert!(plan.is_structured());
    assert!(!plan.approved);
    assert!(persisted.tool_calls.is_empty());
    assert!(!persisted
        .events
        .iter()
        .any(|event| event.kind == "approval_required"));
    assert!(persisted.events.iter().any(|event| {
        event.kind == "reconciliation" && event.details["outcome"] == "plan_ready"
    }));
}

#[test]
fn plain_compaction_rejects_steering_without_persisting_an_input_channel() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    store
        .try_append_message(&session.id, "user", "compact this retained context")
        .expect("user context");
    store
        .try_append_message(&session.id, "assistant", "retained answer")
        .expect("assistant context");
    let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
    let signal_owner = PlainSignalOwner::detached();
    let scope = PlainAgentScope {
        project: project.path(),
        profile_id: "default",
        session_store: &store,
        signal_owner: &signal_owner,
    };

    assert_eq!(
        execute_agent_step(
            &scope,
            &session.id,
            "",
            InteractiveAgentMode::Compact,
            &input,
        )
        .expect("plain compaction"),
        PlainAgentDisposition::Completed
    );
    assert!(submit_plain_steering(None, "must not steer maintenance")
        .expect_err("compact steering is unavailable")
        .contains("does not accept"));
    let persisted = store.load(&session.id).expect("compacted session");
    assert!(!persisted.events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "steering_channel_bound" | "steering_admission" | "steering_input"
        )
    }));
}

#[test]
#[serial]
fn plain_active_router_distinguishes_exact_steering_from_durable_queue_input() {
    let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let goal = "exact run steering response smoke";
    let mut session = store.create_session();
    let mut plan = nib::session::Plan::new(
        goal,
        vec![nib::session::PlanStep {
            description: "wait for exact steering".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    );
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");
    let input = ConsoleInput::new(ScriptedLineReader::new([
        (
            200,
            "steer: replacement steering marker; answer without tools\n",
        ),
        (100, "queue: verify the persisted result next\n"),
    ]));
    let signal_owner = PlainSignalOwner::detached();
    let scope = PlainAgentScope {
        project: project.path(),
        profile_id: "default",
        session_store: &store,
        signal_owner: &signal_owner,
    };

    assert_eq!(
        execute_agent_step(
            &scope,
            &session.id,
            goal,
            InteractiveAgentMode::Execute,
            &input,
        )
        .expect("plain steered run"),
        PlainAgentDisposition::Completed
    );
    let persisted = store.load(&session.id).expect("plain steering state");
    assert!(persisted.events.iter().any(|event| {
        event.kind == "steering_input"
            && event.details["source"] == "plain"
            && event.details["text"] == "replacement steering marker; answer without tools"
    }));
    assert!(persisted
        .events
        .iter()
        .any(|event| event.kind == "steering_intake"));
    assert_eq!(persisted.queued_follow_ups.len(), 1);
    assert_eq!(
        persisted.queued_follow_ups[0].text,
        "verify the persisted result next"
    );
    assert!(persisted
        .messages
        .iter()
        .all(|message| { message.content != "verify the persisted result next" }));
}

#[test]
fn plain_queue_startup_failure_retains_fifo_and_records_disposition() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    persist_queued_follow_up(&store, &session.id, "first retained", "plain")
        .expect("first queue item");
    persist_queued_follow_up(&store, &session.id, "second retained", "plain")
        .expect("second queue item");
    std::fs::write(project.path().join(".nib/config.toml"), "invalid = [")
        .expect("corrupt config fixture");
    let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
    let signal_owner = PlainSignalOwner::detached();
    let scope = PlainAgentScope {
        project: project.path(),
        profile_id: "default",
        session_store: &store,
        signal_owner: &signal_owner,
    };

    let error =
        drain_plain_queued_follow_ups(&scope, &session.id, &input, PlainModalState::default())
            .expect_err("invalid config prevents prepared startup");
    assert!(error.contains("remains queued"), "{error}");
    let persisted = store.load(&session.id).expect("retained queue");
    assert_eq!(
        persisted
            .queued_follow_ups
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["first retained", "second retained"]
    );
    assert!(persisted.events.iter().any(|event| {
        event.kind == "queued_follow_up_start_failed"
            && event.details["phase"] == "worker_startup"
            && event.details["disposition"] == "retained"
    }));
}

#[test]
#[serial]
fn plain_execution_failure_retains_queued_work_instead_of_launching_it() {
    let _interactive_smoke = EnvironmentGuard::set("NIB_ENABLE_INTERACTIVE_SMOKE", "1");
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    persist_queued_follow_up(
        &store,
        &session.id,
        "must remain after execution failure",
        "plain",
    )
    .expect("queued follow-up");
    let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
    let signal_owner = PlainSignalOwner::detached();
    let scope = PlainAgentScope {
        project: project.path(),
        profile_id: "default",
        session_store: &store,
        signal_owner: &signal_owner,
    };

    assert_eq!(
        execute_plain_turn_and_queued_follow_ups(
            &scope,
            &session.id,
            "interactive provider failure smoke",
            InteractiveAgentMode::Execute,
            &input,
            PlainModalState::default(),
        )
        .expect("typed failure reconciles"),
        PlainAgentDisposition::Failed
    );
    let persisted = store.load(&session.id).expect("failed session");
    assert_eq!(persisted.queued_follow_ups.len(), 1);
    assert_eq!(
        persisted.queued_follow_ups[0].text,
        "must remain after execution failure"
    );
    assert!(persisted
        .messages
        .iter()
        .all(|message| message.content != "must remain after execution failure"));
    assert!(persisted.events.iter().any(|event| {
        event.kind == "reconciliation" && event.details["outcome"] == "planning_failed"
    }));
}

#[test]
fn plain_modal_typeahead_is_rejected_instead_of_becoming_a_queued_goal() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    let modal_state = PlainModalState::default();
    let input = ConsoleInput::new(ModalSynchronizedReader::new(
        modal_state.clone(),
        [
            ModalInputStep::AfterModal(PLAIN_MODAL_APPROVAL, "y\n"),
            ModalInputStep::Delayed(25, "surplus modal paste must not execute\n"),
            ModalInputStep::Delayed(25, "\n"),
        ],
    ));
    let signal_owner = PlainSignalOwner::detached();
    let scope = PlainAgentScope {
        project: project.path(),
        profile_id: "default",
        session_store: &store,
        signal_owner: &signal_owner,
    };

    assert_eq!(
        execute_agent_step_with_modal_state(
            &scope,
            &session.id,
            "list workspace after provider recovery",
            InteractiveAgentMode::Execute,
            &input,
            modal_state,
        )
        .expect("approved run completes"),
        PlainAgentDisposition::Completed
    );
    let persisted = store.load(&session.id).expect("completed session");
    assert!(persisted.queued_follow_ups.is_empty());
    assert!(persisted.messages.iter().all(|message| {
        !message
            .content
            .contains("surplus modal paste must not execute")
    }));
}

#[test]
#[serial]
fn plain_cancellation_retains_queued_work_instead_of_launching_it() {
    let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    persist_queued_follow_up(
        &store,
        &session.id,
        "must remain after cancellation",
        "plain",
    )
    .expect("queued follow-up");
    let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
    let signal_owner = PlainSignalOwner::detached();
    let cancelling_owner = signal_owner.clone();
    let canceller = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if !cancelling_owner.dispatch_for_test() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("plain run did not register cancellation before the deadline");
    });
    let scope = PlainAgentScope {
        project: project.path(),
        profile_id: "default",
        session_store: &store,
        signal_owner: &signal_owner,
    };

    let disposition = execute_plain_turn_and_queued_follow_ups(
        &scope,
        &session.id,
        "exact run steering response smoke",
        InteractiveAgentMode::Execute,
        &input,
        PlainModalState::default(),
    )
    .expect("cancelled run reconciles");
    canceller.join().expect("canceller");
    assert_eq!(disposition, PlainAgentDisposition::Cancelled);
    let persisted = store.load(&session.id).expect("cancelled session");
    assert_eq!(persisted.queued_follow_ups.len(), 1);
    assert_eq!(
        persisted.queued_follow_ups[0].text,
        "must remain after cancellation"
    );
    assert!(persisted
        .messages
        .iter()
        .all(|message| { message.content != "must remain after cancellation" }));
}

#[test]
fn plain_successful_queued_turns_chain_in_fifo_order() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    persist_queued_follow_up(&store, &session.id, "second fifo turn", "plain")
        .expect("second turn");
    persist_queued_follow_up(&store, &session.id, "third fifo turn", "plain").expect("third turn");
    let modal_state = PlainModalState::default();
    let input = ConsoleInput::new(ModalSynchronizedReader::new(
        modal_state.clone(),
        [
            ModalInputStep::AfterModal(PLAIN_MODAL_APPROVAL, "y\n\n"),
            ModalInputStep::AfterModalCycle(PLAIN_MODAL_APPROVAL, "y\n\n"),
            ModalInputStep::AfterModalCycle(PLAIN_MODAL_APPROVAL, "y\n\n"),
        ],
    ));
    let signal_owner = PlainSignalOwner::detached();
    let scope = PlainAgentScope {
        project: project.path(),
        profile_id: "default",
        session_store: &store,
        signal_owner: &signal_owner,
    };

    assert_eq!(
        execute_plain_turn_and_queued_follow_ups(
            &scope,
            &session.id,
            "first fifo turn",
            InteractiveAgentMode::Execute,
            &input,
            modal_state,
        )
        .expect("FIFO turns complete"),
        PlainAgentDisposition::Completed
    );
    let persisted = store.load(&session.id).expect("completed FIFO session");
    assert!(persisted.queued_follow_ups.is_empty());
    assert_eq!(
        persisted
            .messages
            .iter()
            .filter(|message| { message.role == "user" && message.content.ends_with("fifo turn") })
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["first fifo turn", "second fifo turn", "third fifo turn"]
    );
}

#[test]
#[serial]
fn chat_routes_commands_and_persists_model_and_mcp_changes() {
    let project = tempdir().expect("project");
    let global_skills = tempdir().expect("global skills");
    save_mock_config(project.path());
    let previous_skills_dir = std::env::var_os("NIB_SKILLS_DIR");
    std::env::set_var("NIB_SKILLS_DIR", global_skills.path());
    let _cwd = CurrentDirGuard::enter(project.path());

    let commands = concat!(
        "\n",
        "/help\n",
        "/providers\n",
        "/session\n",
        "\n",
        "/clear\n",
        "/model\n",
        "2\n",
        "/model\n",
        "1\n",
        "/model custom-mock\n",
        "/skills list\n",
        "/skills invalid\n",
        "/mcp list\n",
        "/mcp add local echo --stdio\n",
        "/mcp list\n",
        "/mcp remove local\n",
        "/mcp invalid\n",
        "/unknown\n",
        "/quit\n"
    );
    run_chat_with_input(
        &ChatArgs {
            session: Some("missing-session".to_string()),
            auth: false,
            ..Default::default()
        },
        Cursor::new(commands.as_bytes()),
    )
    .expect("scripted chat");

    let config = load_nib_config_full(project.path()).expect("updated config");
    assert_eq!(config.llm.providers["mock"].model, "custom-mock");
    assert!(config.mcp.servers.is_empty());
    restore_env("NIB_SKILLS_DIR", previous_skills_dir);
}

#[test]
fn chat_completion_prompts_use_the_shared_registry() {
    let command = select_command_completion_from_console(
        &ConsoleInput::new(Cursor::new(b"1\n".to_vec())),
        "/pro",
    )
    .expect("root completion");
    assert_eq!(command.as_deref(), Some("/providers"));

    let command = select_command_completion_from_console(
        &ConsoleInput::new(Cursor::new(b"1\n./reviewed-skill\n".to_vec())),
        "/skills i",
    )
    .expect("fixed-subcommand completion");
    assert_eq!(command.as_deref(), Some("/skills install ./reviewed-skill"));

    let cancelled = select_command_completion_from_console(
        &ConsoleInput::new(Cursor::new(b"\n".to_vec())),
        "/",
    )
    .expect("cancelled completion");
    assert_eq!(cancelled, None);
}

#[test]
fn chat_session_confirmation_cancels_and_rejects_a_stale_preview() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let active = store
        .try_create_session_with_id("active-session")
        .expect("active session");
    let target = store
        .try_create_session_with_id("target-session")
        .expect("target session");
    let selection = nib::interactive::interactive_session_selection(&store, &active.id)
        .expect("session selection");
    let candidate = selection
        .candidates
        .iter()
        .find(|candidate| candidate.id == target.id)
        .expect("target candidate")
        .clone();

    assert_eq!(
        confirm_session_candidate(&store, candidate.clone(), "n\n").expect("cancel confirmation"),
        ChatSessionAction::Cancelled
    );
    store
        .try_append_message(&target.id, "user", "changed after preview")
        .expect("change target");
    let error = confirm_session_candidate(&store, candidate, "y\n")
        .expect_err("stale candidate must fail closed");
    assert!(error.contains("changed since it was previewed"));

    let scripted = format!("{}\nn\n", target.id);
    assert_eq!(
        select_session_from_console(
            &ConsoleInput::new(Cursor::new(scripted.into_bytes())),
            &store,
            &active.id,
            &nib::interactive::interactive_session_selection(&store, &active.id)
                .expect("refreshed selection"),
        )
        .expect("scripted cancellation"),
        ChatSessionAction::Cancelled
    );

    let numeric = store
        .try_create_session_with_id("7")
        .expect("numeric session ID");
    let numeric_selection = nib::interactive::interactive_session_selection(&store, &active.id)
        .expect("numeric selection");
    assert_eq!(
        select_session_from_console(
            &ConsoleInput::new(Cursor::new(b"7\ny\n".to_vec())),
            &store,
            &active.id,
            &numeric_selection,
        )
        .expect("numeric exact ID"),
        ChatSessionAction::Activated(numeric.id)
    );
}

#[test]
fn chat_accepts_an_exact_numeric_session_id_when_it_is_not_a_displayed_number() {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let active = store
        .try_create_session_with_id("active-session")
        .expect("active session");
    store
        .try_create_session_with_id("200")
        .expect("numeric target session");
    let selection = nib::interactive::interactive_session_selection(&store, &active.id)
        .expect("session selection");

    let action = select_session_from_console(
        &ConsoleInput::new(Cursor::new(b"200\ny\n".to_vec())),
        &store,
        &active.id,
        &selection,
    )
    .expect("exact numeric session selection");

    assert_eq!(action, ChatSessionAction::Activated("200".to_string()));
}

#[test]
#[serial]
fn chat_rejects_a_credential_derived_session_before_resolution() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let mut config = load_nib_config_full(project.path()).expect("config");
    config.llm.providers.insert(
        "inactive-openai".to_string(),
        nib::config::ProviderEntry {
            model: "fixture".to_string(),
            api_key: Some("private-chat-session".to_string()),
            ..Default::default()
        },
    );
    save_nib_config_full(project.path(), &mut config).expect("credential config");
    let _cwd = CurrentDirGuard::enter(project.path());

    let error = run_chat_with_input(
        &ChatArgs {
            session: Some("private-chat-session".to_string()),
            plain: true,
            ..Default::default()
        },
        Cursor::new(b"/quit\n".to_vec()),
    )
    .expect_err("credential-derived session id");

    assert_eq!(
        error,
        "session identifier conflicts with configured sensitive data"
    );
    assert!(!error.contains("private-chat-session"));
    assert!(SessionStore::for_project(project.path())
        .expect("session store")
        .list_result()
        .expect("session list")
        .is_empty());
}

#[test]
#[serial]
fn chat_confirms_session_switch_and_routes_the_next_turn_to_the_target() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let current = store.try_create_session().expect("current session");
    let target = store.try_create_session().expect("target session");
    store
        .try_append_message(&target.id, "user", "existing target context")
        .expect("target context");
    store
        .try_append_message(&target.id, "assistant", "target ready")
        .expect("target response");
    let _cwd = CurrentDirGuard::enter(project.path());
    let commands = format!(
        "/session\n{}\ny\nparity routing goal\ny\n\n/quit\n",
        target.id
    );

    run_chat_with_input(
        &ChatArgs {
            session: Some(current.id.clone()),
            auth: false,
            ..Default::default()
        },
        Cursor::new(commands.into_bytes()),
    )
    .expect("session-switching chat");

    let current = store
        .load_result(&current.id)
        .expect("load current")
        .expect("current session");
    let target = store
        .load_result(&target.id)
        .expect("load target")
        .expect("target session");
    assert!(!current
        .messages
        .iter()
        .any(|message| message.content == "parity routing goal"));
    assert!(target
        .messages
        .iter()
        .any(|message| message.role == "user" && message.content == "parity routing goal"));
}

#[test]
#[serial]
fn chat_accepts_exact_model_outside_provider_suggestions_without_mutating_override() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("openai".to_string(), "gpt-5.6-sol".to_string(), None);
    config.llm.providers.get_mut("openai").unwrap().models =
        Some(vec!["gateway/reviewed".to_string()]);
    config.skills.enabled = false;
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    save_nib_config_full(project.path(), &mut config).expect("OpenAI config");
    let _cwd = CurrentDirGuard::enter(project.path());

    run_chat_with_input(
        &ChatArgs {
            session: None,
            auth: false,
            ..Default::default()
        },
        Cursor::new(b"/model gateway/future-model\n/quit\n"),
    )
    .expect("scripted exact model selection");

    let config = load_nib_config_full(project.path()).expect("updated config");
    assert_eq!(config.llm.providers["openai"].model, "gateway/future-model");
    assert_eq!(
        config.llm.providers["openai"].models.as_deref(),
        Some(["gateway/reviewed".to_string()].as_slice())
    );
}

#[test]
#[serial]
fn chat_resumes_existing_session_and_renders_bounded_history() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.try_create_session().expect("session");
    store
        .try_append_message(&session.id, "user", &"x".repeat(240))
        .expect("long history message");
    store
        .try_append_message(&session.id, "assistant", "ready")
        .expect("assistant history message");
    let _cwd = CurrentDirGuard::enter(project.path());
    run_chat_with_input(
        &ChatArgs {
            session: Some(session.id),
            auth: false,
            ..Default::default()
        },
        Cursor::new(b"/quit\n"),
    )
    .expect("resume chat");
}

#[test]
#[serial]
fn chat_shares_approval_and_question_input_without_deadlocking() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let _cwd = CurrentDirGuard::enter(project.path());

    let modal_state = PlainModalState::default();
    run_chat_with_modal_state_input(
        &ChatArgs {
            session: None,
            auth: false,
            ..Default::default()
        },
        ModalSynchronizedReader::new(
            modal_state.clone(),
            [
                ModalInputStep::Immediate("ask a question before continuing\n"),
                ModalInputStep::AfterModal(PLAIN_MODAL_QUESTION, ":command /status\n"),
                ModalInputStep::AfterModal(PLAIN_MODAL_QUESTION, "2\n\n"),
            ],
        ),
        modal_state,
    )
    .expect("question chat");

    let store = SessionStore::for_project(project.path()).expect("session store");
    let session_id = store
        .list_result()
        .expect("sessions")
        .into_iter()
        .next()
        .expect("chat session");
    let session = store
        .load_result(&session_id)
        .expect("load session")
        .expect("chat session state");
    assert!(session.messages.iter().any(|message| {
        message.role == "tool" && message.content.contains("\"answer\":\"full\"")
    }));
    assert_eq!(
        session
            .tool_calls
            .iter()
            .filter(|record| record.tool_name.as_deref() == Some("ask_question"))
            .count(),
        1,
        "modal inspection must not consume or duplicate the question"
    );
}

#[test]
#[serial]
fn chat_reconciles_closed_question_input_without_a_role_violation() {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let _cwd = CurrentDirGuard::enter(project.path());

    let modal_state = PlainModalState::default();
    run_chat_with_modal_state_input(
        &ChatArgs {
            session: None,
            auth: false,
            ..Default::default()
        },
        ModalSynchronizedReader::new(
            modal_state.clone(),
            [ModalInputStep::Immediate(
                "ask a question before continuing\n",
            )],
        ),
        modal_state,
    )
    .expect("closed chat input exits after reconciliation");

    let store = SessionStore::for_project(project.path()).expect("session store");
    let session_id = store
        .list_result()
        .expect("sessions")
        .into_iter()
        .next()
        .expect("chat session");
    let session = store
        .load_result(&session_id)
        .expect("load session")
        .expect("chat session");
    session
        .validate_message_sequence()
        .expect("role-safe reconciled transcript");
    assert!(session.events.iter().any(|event| {
        event.kind == "reconciliation" && event.details["outcome"] == "waiting_for_user_input"
    }));
    let question = session
        .tool_calls
        .iter()
        .find(|record| record.tool_name.as_deref() == Some("ask_question"))
        .expect("question audit");
    assert!(question.error.as_deref().is_some_and(|error| {
        error.contains("console input closed") || error.contains("input closed")
    }));
}

fn plain_recovery_project() -> (tempfile::TempDir, SessionStore, String, nib::tools::ToolInvocationId) {
    let project = tempdir().expect("project");
    save_mock_config(project.path());
    let store = SessionStore::for_project(project.path()).expect("session store");
    let mut session = store.try_create_session().expect("session");
    let mut plan = nib::session::Plan::new(
        "finish after recovery",
        vec![nib::session::PlanStep {
            description: "use the selected target".to_string(),
            status: "Blocked".to_string(),
            outcome: None,
            attempts: 1,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    );
    plan.approve();
    let plan_id = plan.id.clone();
    let invocation_id = nib::tools::ToolInvocationId::new();
    session.plan = Some(plan);
    session.events.push(nib::session::SessionEvent {
        index: 0,
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
        .push(nib::session::ClarificationRecord {
            invocation_id,
            plan_id: Some(plan_id),
            question: "Which target?".to_string(),
            proposed_answer: None,
            options: vec!["alpha".to_string(), "beta".to_string()],
            dependent_paths: Vec::new(),
            status: nib::session::ClarificationStatus::Unresolved,
            answer: None,
            question_event_index: 0,
            answer_message_index: None,
            answer_event_index: None,
            reason: Some("left unanswered".to_string()),
            outcome: Some("left_unanswered".to_string()),
            ..Default::default()
        });
    store.save(&mut session).expect("recoverable question");
    let session_id = session.id.clone();
    (project, store, session_id, invocation_id)

}

#[test]
#[serial]
fn plain_recovered_question_retries_invalid_input_before_persisting() {
    let (project, store, session_id, invocation_id) = plain_recovery_project();
    let input = format!("resume {invocation_id}\n:command /status\n0\n2\n\n/quit\n");
    let _cwd = CurrentDirGuard::enter(project.path());

    run_chat_with_input(
        &ChatArgs {
            session: Some(session_id.clone()),
            plain: true,
            ..Default::default()
        },
        Cursor::new(input.into_bytes()),
    )
    .expect("plain recovery session");

    let persisted = store.load(&session_id).expect("answered session");
    assert_eq!(
        persisted.clarifications[0].status,
        nib::session::ClarificationStatus::Answered
    );
    assert_eq!(persisted.clarifications[0].answer.as_deref(), Some("beta"));
    assert_eq!(
        persisted
            .events
            .iter()
            .filter(|event| event.kind == "human_question_answer_received")
            .count(),
        1
    );
}

#[test]
fn chat_quit_and_session_switch_report_queue_disposition() {
    let directory = tempdir().expect("sessions");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let current = store.try_create_session().expect("current");
    persist_queued_follow_up(&store, &current.id, "follow up later", "composer").expect("queue");
    let exited = chat_queue_disposition(&store, &current.id, "exited");
    assert!(exited.contains("exited;"));
    assert!(exited.contains(&format!("retained on session {}", current.id)));

    let target = store.try_create_session().expect("target");
    let switched = chat_queue_disposition(&store, &current.id, "switched sessions");
    assert!(switched.contains("switched sessions;"));
    assert!(switched.contains(&current.id));
    assert!(!switched.contains(&target.id));
}

#[test]
fn plain_goodbye_projects_the_session_path_before_output() {
    let directory = tempdir().expect("project parent");
    let secret = "credential-project-basename".to_string();
    let store = SessionStore::at_dir(directory.path().join(&secret).join(".nib").join("sessions"));

    let goodbye = plain_goodbye(&store, "safe-session", std::slice::from_ref(&secret));
    assert!(goodbye.starts_with("Goodbye. Session saved to "));
    assert!(goodbye.contains("[REDACTED]"), "{goodbye}");
    assert!(!goodbye.contains(&secret), "{goodbye}");
    assert!(!goodbye
        .chars()
        .any(|character| { character.is_control() && !matches!(character, '\n' | '\t') }));
}


#[tokio::test]
async fn plain_native_form_retains_drafts_until_explicit_set_submit() {
    use nib::agent::{QuestionFormRequestContext, QuestionHandler};
    use nib::interactive::{QuestionAnswer, QuestionAnswerSource, QuestionFormOutcome};
    let form = nib::interactive::parse_question_form(&serde_json::json!({"questions": [
        {"title":"First", "question":"Choose first", "options":["alpha", "beta"]},
        {"title":"Second", "question":"Choose second", "options":["alpha", "beta"]}
    ]})).expect("form fixture");
    let modal_state = PlainModalState::default();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handler = BrokeredPlainQuestionHandler { tx, modal_state: modal_state.clone(), sensitive_values: Vec::new() };
    let worker = tokio::spawn(async move {
        handler.ask_form(QuestionFormRequestContext { invocation_id: nib::tools::ToolInvocationId::new(), form: &form, initial_answers: &[] }).await
    });
    let mut prompt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("native prompt ready").expect("form prompt");
    assert_eq!(modal_state.current(), PLAIN_MODAL_QUESTION);
    assert!(prompt.form.submit_line("1").is_none());
    assert!(prompt.form.submit_line("2").is_none());
    assert!(prompt.form.render(&[]).contains("Submit these answers?"));
    assert!(!worker.is_finished(), "drafts did not resume the worker");
    assert!(prompt.form.submit_line("1").is_none());
    assert!(prompt.form.submit_line("text: replacement").is_none());
    let outcome = prompt.form.submit_line("").expect("explicit set Submit");
    prompt.reply.send(outcome).expect("native form response");
    assert_eq!(worker.await.expect("handler completion"), QuestionFormOutcome::Answered(vec![
        QuestionAnswer { answer: "replacement".into(), source: QuestionAnswerSource::Text },
        QuestionAnswer { answer: "beta".into(), source: QuestionAnswerSource::Option }
    ]));
    assert_eq!(modal_state.current(), PLAIN_MODAL_IDLE);
}


#[test]
#[serial]
fn recovered_plain_question_rejects_delayed_surplus_before_resuming() {
    let (project, store, session_id, invocation_id) = plain_recovery_project();
    let _cwd = CurrentDirGuard::enter(project.path());
    let surplus = "surplus recovery line is not a new request";
    let reader = ScriptedLineReader {
        lines: [
            (std::time::Duration::ZERO, format!("resume {invocation_id}\n")),
            (std::time::Duration::ZERO, "1\n".into()),
            (std::time::Duration::from_millis(25), format!("{surplus}\n")),
            (std::time::Duration::ZERO, "\n".into()),
            (std::time::Duration::ZERO, "/quit\n".into()),
        ].into(), current: Vec::new(), offset: 0,
    };
    run_chat_with_input(&ChatArgs { session: Some(session_id.clone()), plain: true, ..Default::default() }, reader).expect("recovered plain frame");
    let session = store.load(&session_id).expect("framed recovered session");
    assert_eq!(session.clarifications[0].answer.as_deref(), Some("alpha"));
    assert!(!session.messages.iter().any(|message| message.role == "user" && message.content.contains(surplus)), "surplus input became a conversation turn");
    assert!(!session.events.iter().any(|event| event.details.to_string().contains(surplus)), "surplus input became queued work or steering");
}

#[test]
#[serial]
fn recovered_plain_question_preserves_literal_modal_command_answers() {
    let (project, store, session_id, invocation_id) = plain_recovery_project();
    let _cwd = CurrentDirGuard::enter(project.path());
    run_chat_with_input(&ChatArgs { session: Some(session_id.clone()), plain: true, ..Default::default() }, Cursor::new(format!("resume {invocation_id}\ntext: :command /status\n\n/quit\n").into_bytes())).expect("literal recovered answer");
    let session = store.load(&session_id).expect("literal answer session");
    assert_eq!(session.clarifications[0].answer.as_deref(), Some(":command /status"));
    assert_eq!(session.clarifications[0].answer_source, Some(nib::interactive::QuestionAnswerSource::Text));
}

#[test]
fn successful_recovery_frame_interrupts_on_esc_or_eof_without_submission() {
    let outcome = nib::interactive::QuestionFormOutcome::Discussed("draft discussion".into());
    assert_eq!(frame_plain_recovery_outcome(&ConsoleInput::new(Cursor::new(b"esc\n".to_vec())), outcome.clone()), nib::interactive::QuestionFormOutcome::LeftUnanswered);
    assert_eq!(frame_plain_recovery_outcome(&ConsoleInput::new(Cursor::new(Vec::<u8>::new())), outcome), nib::interactive::QuestionFormOutcome::InputClosed);
}
