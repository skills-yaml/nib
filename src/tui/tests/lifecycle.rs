use super::*;

#[test]
fn repeated_tui_workers_reuse_the_same_active_session() {
    let directory = tempdir().expect("tempdir");
    save_config(directory.path(), &mock_config()).expect("save mock config");
    let store = SessionStore::for_project(directory.path()).expect("session store");
    let session = store.create_session();
    let (approval_tx, _approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, _question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);

    for goal in ["first TUI turn", "second TUI turn"] {
        let mut worker = spawn_tui_agent_worker(
            TuiAgentProfileScope {
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: store.sessions_dir().to_path_buf(),
            },
            session.id.clone(),
            goal.to_string(),
            InteractiveAgentMode::Execute,
            approval_tx.clone(),
            question_tx.clone(),
            stream_tx.clone(),
        )
        .expect("spawn TUI worker");
        let started = std::time::Instant::now();
        while !worker.is_finished() && started.elapsed() < std::time::Duration::from_secs(10) {
            while stream_rx.try_recv().is_ok() {}
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        worker.join().expect("join TUI worker");
        while stream_rx.try_recv().is_ok() {}
    }

    let persisted = store.load(&session.id).expect("reused session");
    for goal in ["first TUI turn", "second TUI turn"] {
        assert!(persisted
            .messages
            .iter()
            .any(|message| message.role == "user" && message.content == goal));
    }
}

#[test]
fn approval_prompt_states_the_command_without_a_metadata_dump() {
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({
            "command": "git status --short --branch && task check",
            "cwd": "/home/e/work/projects/nib",
            "background": false
        }),
        session_id: None,
        project_root: None,
    };
    let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);
    let prompt = approval_prompt(&call, &context);
    assert_eq!(prompt.statement, "Run this command");
    assert_eq!(prompt.subject, "git status --short --branch && task check");
    assert_eq!(
        prompt.location.as_deref(),
        Some("/home/e/work/projects/nib")
    );
    assert!(!prompt.subject.contains("command="));
    assert_eq!(
        compact_approval_risk("destructive / requires_approval"),
        "destructive"
    );

    let raw_secret = "raw-command-secret";
    let secret_call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({"command": format!("deploy --token={raw_secret}")}),
        session_id: None,
        project_root: None,
    };
    let mut safe_context =
        ApprovalContext::compatibility(&secret_call, PermissionLevel::Destructive);
    safe_context.display_subject = "deploy --token=[REDACTED]".to_string();
    let safe_prompt = approval_prompt(&secret_call, &safe_context);
    assert_eq!(safe_prompt.subject, "deploy --token=[REDACTED]");
    assert!(!safe_prompt.subject.contains(raw_secret));
}

#[test]
fn approval_prompt_discloses_omitted_patch_targets() {
    let patch = ["e.rs", "b.rs", "a.rs", "d.rs", "c.rs"]
        .into_iter()
        .map(|path| format!("*** Update File: {path}\n"))
        .collect::<String>();
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "apply_patch".to_string(),
        arguments: json!({"dry_run": false, "patch": patch}),
        session_id: None,
        project_root: None,
    };
    let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);

    let prompt = approval_prompt(&call, &context);

    assert_eq!(prompt.statement, "Apply a patch");
    assert_eq!(prompt.subject, "5 files (+1 more): a.rs,b.rs,c.rs,d.rs");
    assert!(!prompt.subject.contains("e.rs"));
}

#[test]
fn plan_approval_card_lists_numbered_steps() {
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "approve_plan".to_string(),
        arguments: json!({
            "plan_id": "plan-123",
            "goal": "inspect wrap",
            "steps": ["inspect files", "change parser", "run tests"],
        }),
        session_id: None,
        project_root: None,
    };
    let context = ApprovalContext::compatibility(&call, PermissionLevel::Plan);
    let prompt = approval_prompt(&call, &context);
    assert_eq!(prompt.statement, "Approve this plan");
    assert_eq!(
        context.details,
        vec![
            "1. inspect files".to_string(),
            "2. change parser".to_string(),
            "3. run tests".to_string(),
        ]
    );
    assert!(!prompt.subject.contains("plan_id="));

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let composer = Composer::default();
    let (approval_tx, _approval_rx) = oneshot::channel();
    let approval = approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "approve_plan".to_string(),
            arguments: json!({
                "plan_id": "plan-123",
                "goal": "inspect wrap",
                "steps": ["inspect files", "change parser", "run tests"],
            }),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Plan,
        approval_tx,
    );
    terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace  ·  sess dock-session  ·  local  ·  -",
                    "awaiting you  ·  mock/mock-model  ·  queue 0",
                    "you  inspect wrap\n\nthought  generated 3 steps\n1. inspect files\n2. change parser\n3. run tests",
                    &composer,
                    Some(&approval),
                    None,
                )
            })
            .expect("render plan approval");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Approve this plan"), "{rendered}");
    assert!(rendered.contains("1. inspect files"), "{rendered}");
    assert!(rendered.contains("2. change parser"), "{rendered}");
    assert!(rendered.contains("3. run tests"), "{rendered}");
    assert!(rendered.contains("generated 3 steps"), "{rendered}");
    assert!(rendered.contains("Approve once"), "{rendered}");
    assert!(rendered.contains("Deny"), "{rendered}");
    assert!(!rendered.contains("plan_id="), "{rendered}");
    assert!(!rendered.contains("command="), "{rendered}");
}

#[test]
fn plan_approval_card_marks_omitted_steps() {
    let rows = overlay_visual_rows(
        "Approve this plan",
        &(1..=8)
            .map(|index| format!("{index}. step {index}"))
            .collect::<Vec<_>>(),
        40,
    );
    assert!(rows.iter().any(|row| row.contains("Approve this plan")));
    assert!(rows.iter().any(|row| row.contains("1. step 1")));
    assert!(rows.iter().any(|row| row.contains("more")), "{rows:?}");
    assert_eq!(rows.len(), 6);
    assert!(!rows.iter().any(|row| row.contains("8. step 8")));
}

#[test]
fn ledger_keeps_transcript_visible_under_approval_and_question_docks() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let composer = Composer::default();
    let (approval_tx, _approval_rx) = oneshot::channel();
    let approval = approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": "task test"}),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        approval_tx,
    );
    terminal
        .draw(|frame| {
            render_current_session_view(
                frame,
                "workspace  ·  sess dock-session  ·  local  ·  -",
                "awaiting you  ·  mock/mock-model  ·  queue 0",
                "you  inspect wrap\n\nassistant  the tests fail because width is wrong",
                &composer,
                Some(&approval),
                None,
            )
        })
        .expect("render dock");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("workspace"));
    assert!(rendered.contains("inspect wrap"));
    assert!(rendered.contains("WAITING APPROVAL"));
    assert!(rendered.contains("mock-model"));
    assert!(rendered.contains("Would you like to run the following command?"));
    assert!(rendered.contains("task test"));
    assert!(rendered.contains("Yes, proceed"));
    assert!(rendered.contains("tell Codex"));
    assert!(!rendered.contains("Approval required"));
    assert!(!rendered.contains("command="));
    assert!(!rendered.contains("{\"command\""));
}

#[test]
fn question_card_states_the_ask_and_numbered_choices() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let (reply_tx, _reply_rx) = oneshot::channel();
    let question = PendingQuestion::new(TuiQuestionRequest::single(
        "Choose a mode".to_string(),
        None,
        vec!["plan".to_string(), "execute".to_string()],
        reply_tx,
    ));
    terminal
        .draw(|frame| {
            render_current_session_view(
                frame,
                "workspace  ·  sess dock-session  ·  local  ·  -",
                "awaiting you  ·  mock/mock-model  ·  queue 0",
                "you  inspect wrap\n\nnib  keep this visible",
                &Composer::default(),
                None,
                Some(&question),
            )
        })
        .expect("render question card");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Choose a mode"), "{rendered}");
    assert!(rendered.contains("› 1. plan"), "{rendered}");
    assert!(rendered.contains("2. execute"), "{rendered}");
    assert!(
        rendered.contains("Type something.") && rendered.contains("Chat about this"),
        "{rendered}"
    );
    assert!(rendered.contains("Enter"), "{rendered}");
    assert!(rendered.contains("Esc"), "{rendered}");
    assert!(rendered.contains("inspect wrap"), "{rendered}");
    assert!(rendered.contains("keep this visible"), "{rendered}");
    assert!(!rendered.contains("nib is asking"), "{rendered}");
    assert!(!rendered.contains("question  Choose a mode"), "{rendered}");
}

#[test]
fn approval_card_states_the_command_and_keeps_choices_on_a_narrow_terminal() {
    let backend = TestBackend::new(40, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let composer = Composer::default();
    let (approval_tx, _approval_rx) = oneshot::channel();
    let approval = approval_request(
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({
                "command": "git status --short --branch && task check",
                "cwd": "/home/e/work/projects/nib",
                "background": false
            }),
            session_id: None,
            project_root: None,
        },
        PermissionLevel::Destructive,
        approval_tx,
    );
    terminal
        .draw(|frame| {
            render_current_session_view(
                frame,
                "workspace  ·  sess dock-session  ·  local  ·  -",
                "awaiting you  ·  mock/mock-model  ·  queue 0",
                "you  inspect wrap\n\nnib  keep this visible",
                &composer,
                Some(&approval),
                None,
            )
        })
        .expect("render narrow approval");
    let rows = buffer_rows(&terminal);
    let joined = rows.concat();
    assert!(joined.contains("git status --short --branch"), "{joined}");
    assert!(rows.iter().any(|row| row.contains("task")), "{rows:?}");
    assert!(joined.contains("Yes, proceed"), "{joined}");
    assert!(joined.contains("tell Codex"), "{joined}");
    assert!(joined.contains("Press enter to confirm"), "{joined}");
    assert!(joined.contains("keep this visible"), "{joined}");
    assert!(!joined.contains("command="), "{joined}");
    assert!(!joined.contains("background=false"), "{joined}");
    let command = row_index_containing(&rows, "git status").expect("command row");
    let approve = row_index_containing(&rows, "Yes, proceed").expect("approve row");
    let deny = row_index_containing(&rows, "tell Codex").expect("deny row");
    assert!(
        command < approve,
        "command must sit above the choices: {rows:?}"
    );
    assert!(approve < deny, "yes must sit above no: {rows:?}");
}

#[test]
fn exact_id_preview_refreshes_listed_candidate_snapshot() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("session-a");
    store
        .try_append_message("session-a", "user", "original")
        .expect("message");
    let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
    let original_token = switcher.candidates[0].snapshot_token;
    store
        .try_append_message("session-a", "assistant", "changed")
        .expect("mutate");
    preview_exact_session(&store, &mut switcher, "session-a", "session-a").expect("refresh listed");
    assert_ne!(switcher.candidates[0].snapshot_token, original_token);
    assert!(switcher.candidates[0].preview.contains("changed"));
}

#[test]
fn switcher_activate_failure_stays_on_the_overlay() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("session-a");
    let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
    let mut active_id = "session-a".to_string();
    let mut timeline = ActiveTimeline::load(&store, &active_id).expect("timeline");
    let error = activate_selected_session(&store, &switcher, true, &mut active_id, &mut timeline)
        .expect_err("busy worker");
    switcher.error = Some(error);
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_session_switcher(frame, frame.area(), &switcher, "session-a"))
        .expect("render");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("switcher error"));
    assert!(rendered.contains("still running"));
}

#[test]
fn unicode_width_follow_tail_counts_wide_glyphs() {
    assert!(bottom_scroll_for_wrap("漢字漢字", 2, 1) >= 3);
    let mut composer = Composer::default();
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Char('j'), KeyModifiers::CONTROL),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "\n");
    assert_eq!(composer.cursor, 1);
}

#[test]
fn composer_moves_the_caret_and_inserts_in_the_middle() {
    let mut composer = Composer::default();
    for character in "abc".chars() {
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Char(character), KeyModifiers::NONE),
            ComposerAction::Pending
        );
    }
    assert_eq!(composer.cursor, 3);
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Left, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Char('X'), KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "abXc");
    assert_eq!(composer.cursor, 3);
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Backspace, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "abc");
}

#[test]
fn composer_delete_removes_one_unicode_scalar_at_the_caret() {
    let mut composer = Composer::from_text("a🙂漢b");
    composer.cursor = 1;
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Delete, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "a漢b");
    assert_eq!(composer.cursor, 1);
    assert!(composer.input.is_char_boundary(composer.cursor));

    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Delete, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "ab");
    assert_eq!(composer.cursor, 1);
}

#[test]
fn composer_paste_normalizes_lines_and_omits_unsafe_controls() {
    let mut composer = Composer::from_text("leftright");
    composer.cursor = "left".len();
    let outcome = composer.insert_paste("🙂\r\nline\rnext\t\0\u{1b}end");

    assert_eq!(composer.input, "left🙂\nline\nnext    endright");
    assert_eq!(composer.cursor, "left🙂\nline\nnext    end".len());
    assert!(!outcome.truncated);
    assert!(outcome.controls_omitted);
    assert_eq!(
        outcome.visible_status().as_deref(),
        Some("[composer] unsafe paste control characters omitted")
    );
    assert!(!composer.input.contains('\0'));
    assert!(!composer.input.contains('\u{1b}'));
}

#[test]
fn composer_paste_truncation_preserves_utf8_prefix_and_reports_status() {
    let mut composer = Composer::from_text("x".repeat(MAX_COMPOSER_BYTES - 5));
    let outcome = composer.insert_paste("🙂étail");

    assert_eq!(outcome.inserted_bytes, "🙂".len());
    assert!(outcome.truncated);
    assert!(composer.input.ends_with('🙂'));
    assert_eq!(composer.input.len(), MAX_COMPOSER_BYTES - 1);
    assert!(std::str::from_utf8(composer.input.as_bytes()).is_ok());
    assert_eq!(
        outcome.visible_status().as_deref(),
        Some("[composer] paste truncated at 16384 bytes")
    );
}

#[test]
fn transcript_scroll_keys_and_wheel_move_content() {
    assert_eq!(
        transcript_action_for_key(KeyCode::PageUp, KeyModifiers::NONE),
        Some(TranscriptViewportAction::PageUp)
    );
    assert_eq!(
        transcript_action_for_key(KeyCode::Up, KeyModifiers::SHIFT),
        Some(TranscriptViewportAction::Lines(-1))
    );
    assert_eq!(
        transcript_action_for_key(KeyCode::Down, KeyModifiers::CONTROL),
        Some(TranscriptViewportAction::Lines(1))
    );
    assert_eq!(
        transcript_action_for_key(KeyCode::Up, KeyModifiers::NONE),
        None,
        "unmodified Up remains draft history"
    );
    assert_eq!(
        transcript_action_for_mouse(MouseEventKind::ScrollUp),
        Some(TranscriptViewportAction::Lines(-3))
    );
    assert_eq!(
        transcript_action_for_mouse(MouseEventKind::ScrollDown),
        Some(TranscriptViewportAction::Lines(3))
    );

    let mut viewport = TranscriptViewport::default();
    viewport.observe_layout(40, 5);
    assert_eq!(viewport.top_row(), 35);
    viewport.apply(transcript_action_for_mouse(MouseEventKind::ScrollUp).expect("wheel"));
    assert!(!viewport.is_pinned_to_tail());
    assert_eq!(viewport.top_row(), 32);
    viewport.apply(TranscriptViewportAction::PageUp);
    assert_eq!(viewport.top_row(), 27);
    assert_eq!(
        transcript_action_for_key(KeyCode::Home, KeyModifiers::CONTROL),
        Some(TranscriptViewportAction::JumpToStart)
    );
}

#[test]
fn pointer_selection_copies_chat_rows_and_falls_back_to_the_last_reply() {
    let rows = vec![
        "● inspect wrap".to_string(),
        "● read_file running · src/lib.rs".to_string(),
        "● Here is the answer".to_string(),
    ];
    let mut selection = PointerSelection::at(0, 2);
    selection.drag_to(0, 14);
    assert_eq!(extract_pointer_text(&rows, selection), "inspect wrap");
    selection = PointerSelection::at(0, 0);
    selection.drag_to(2, 20);
    let copied = extract_pointer_text(&rows, selection);
    assert!(copied.contains("inspect wrap"), "{copied}");
    assert!(copied.contains("Here is the answer"), "{copied}");

    let view = TranscriptView {
        plain_rows: rows,
        ..TranscriptView::default()
    };
    let activities = vec![
        ActivityEntry::new(ActivityKind::User, "", "inspect wrap"),
        ActivityEntry::new(ActivityKind::Assistant, "", "Here is the answer"),
    ];
    let (text, status) = copy_chat_content(None, &view, None, &activities).expect("copy fallback");
    assert_eq!(text, "Here is the answer");
    assert_eq!(status, "Copied last reply.");
    let (block, block_status) =
        copy_chat_content(None, &view, Some(0), &activities).expect("copy block");
    assert_eq!(block, "inspect wrap");
    assert_eq!(block_status, "Copied.");

    let mut pointer = PointerSelection::at(0, 2);
    assert!(!pointer_selection_is_active(Some(pointer)));
    pointer.drag_to(0, 14);
    assert!(pointer_selection_is_active(Some(pointer)));
    let (selected, selected_status) =
        copy_chat_content(Some(pointer), &view, Some(0), &activities).expect("copy drag");
    assert_eq!(selected, "inspect wrap");
    assert_eq!(selected_status, "Copied selection.");

    let mut live = Some(pointer);
    let mut focus = TuiFocus::Transcript;
    let failed = copy_pointer_selection_and_clear_with(
        &mut live,
        &mut focus,
        &view,
        Some(0),
        &activities,
        |text| {
            assert_eq!(text, "inspect wrap");
            ClipboardDelivery::Failed
        },
    );
    assert_eq!(
        failed,
        Some("Copy failed; text remains available for manual selection")
    );
    assert_eq!(live, Some(pointer));
    assert_eq!(focus, TuiFocus::Transcript);

    let copied = copy_pointer_selection_and_clear_with(
        &mut live,
        &mut focus,
        &view,
        Some(0),
        &activities,
        |_| ClipboardDelivery::Native,
    );
    assert_eq!(copied, Some("Copied"));
    assert!(live.is_none());
    assert_eq!(focus, TuiFocus::Composer);
    assert!(ClipboardDelivery::Native.may_clear_selection());
    assert!(ClipboardDelivery::Osc52Unconfirmed.may_clear_selection());
    assert!(!ClipboardDelivery::Unavailable.may_clear_selection());
    assert!(!ClipboardDelivery::Failed.may_clear_selection());
}

#[test]
fn clipboard_delivery_reports_every_backend_outcome_without_fallthrough() {
    use std::cell::Cell;

    let native_calls = Cell::new(0);
    let osc52_calls = Cell::new(0);
    let delivery = deliver_text_to_clipboard_with(
        "copy me",
        false,
        |_| {
            native_calls.set(native_calls.get() + 1);
            true
        },
        |_| {
            osc52_calls.set(osc52_calls.get() + 1);
            true
        },
    );
    assert_eq!(delivery, ClipboardDelivery::Unavailable);
    assert_eq!((native_calls.get(), osc52_calls.get()), (0, 0));

    let delivery = deliver_text_to_clipboard_with(
        "copy me",
        true,
        |_| true,
        |_| panic!("OSC52 must not run after native success"),
    );
    assert_eq!(delivery, ClipboardDelivery::Native);
    assert_eq!(delivery.status(), "Copied");

    let delivery = deliver_text_to_clipboard_with("copy me", true, |_| false, |_| true);
    assert_eq!(delivery, ClipboardDelivery::Osc52Unconfirmed);
    assert_eq!(delivery.status(), "Copy requested via OSC52 (unconfirmed)");

    let delivery = deliver_text_to_clipboard_with("copy me", true, |_| false, |_| false);
    assert_eq!(delivery, ClipboardDelivery::Failed);
    assert_eq!(
        delivery.status(),
        "Copy failed; text remains available for manual selection"
    );
}

#[test]
fn transcript_hit_maps_mouse_cells_to_plain_rows() {
    let view = TranscriptView {
        area: Rect::new(0, 1, 40, 10),
        top_row: 0,
        plain_rows: vec!["● inspect wrap".to_string(), "● reply".to_string()],
        owners: vec![0, 1],
        ..TranscriptView::default()
    };
    let hit = transcript_hit(
        &view,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 1,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert_eq!(hit, Some((0, 2)));
    assert_eq!(
        transcript_hit(
            &view,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
        ),
        None
    );
}

#[test]
fn bracketed_paste_sequences_and_restore_guard_are_deterministic() {
    let mut enabled = Vec::new();
    enable_bracketed_paste_to(&mut enabled).expect("enable paste sequence");
    assert_eq!(enabled, b"\x1b[?2004h");

    #[cfg(not(windows))]
    let restored = {
        let mut restored = Vec::new();
        restore_terminal_to(&mut restored, || Ok(())).expect("restore sequences");
        String::from_utf8(restored).expect("terminal control UTF-8")
    };
    #[cfg(windows)]
    let restored = {
        // `execute!` invokes Win32 console APIs even for an in-memory writer.
        // Check encoding here; native ConPTY smokes verify real restoration.
        let mut restored = String::new();
        crossterm::Command::write_ansi(&DisableMouseCapture, &mut restored)
            .expect("disable mouse sequence");
        crossterm::Command::write_ansi(&DisableBracketedPaste, &mut restored)
            .expect("disable paste sequence");
        crossterm::Command::write_ansi(&LeaveAlternateScreen, &mut restored)
            .expect("leave alternate screen sequence");
        restored
    };
    let mouse = restored.find("\x1b[?1000l").expect("disable mouse capture");
    let paste = restored.find("\x1b[?2004l").expect("disable paste");
    let alternate = restored
        .find("\x1b[?1049l")
        .expect("leave alternate screen");
    assert!(mouse < paste);
    assert!(paste < alternate);

    TEST_TERMINAL_RESTORE_CALLS.store(0, Ordering::SeqCst);
    {
        let _guard = TerminalRestoreGuard::with_restore(record_test_terminal_restore);
    }
    assert_eq!(TEST_TERMINAL_RESTORE_CALLS.load(Ordering::SeqCst), 1);

    TEST_TERMINAL_RESTORE_CALLS.store(0, Ordering::SeqCst);
    {
        let mut guard = TerminalRestoreGuard::with_restore(record_test_terminal_restore);
        guard.restore().expect("explicit restoration");
    }
    assert_eq!(TEST_TERMINAL_RESTORE_CALLS.load(Ordering::SeqCst), 1);
}

#[cfg(not(windows))]
#[test]
fn raw_mode_restoration_runs_last_even_after_an_output_failure() {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    struct Output {
        bytes: Rc<RefCell<Vec<u8>>>,
        fail_first_flush: bool,
        flushes: usize,
    }

    impl io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.fail_first_flush && self.flushes == 1 {
                Err(io::Error::other("mouse output failed"))
            } else {
                Ok(())
            }
        }
    }

    for fail in [false, true] {
        let bytes = Rc::new(RefCell::new(Vec::new()));
        let mut output = Output {
            bytes: Rc::clone(&bytes),
            fail_first_flush: fail,
            flushes: 0,
        };
        let raw_called = Cell::new(false);
        let result = restore_terminal_to(&mut output, || {
            let captured = String::from_utf8(bytes.borrow().clone()).expect("ANSI output");
            for sequence in ["\x1b[?1000l", "\x1b[?2004l", "\x1b[?1049l"] {
                assert!(
                    captured.contains(sequence),
                    "raw cleanup ran before {sequence:?}"
                );
            }
            raw_called.set(true);
            if fail {
                Err(io::Error::other("raw cleanup failed"))
            } else {
                Ok(())
            }
        });
        assert!(raw_called.get());
        assert_eq!(output.flushes, 3);
        if fail {
            let error = result
                .expect_err("retain both cleanup failures")
                .to_string();
            assert!(error.contains("mouse output failed"));
            assert!(error.contains("raw cleanup failed"));
        } else {
            result.expect("successful restoration");
        }
    }
}

#[test]
fn missing_modal_state_renders_a_recoverable_error() {
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            render_interaction_overlay(
                frame,
                InteractionLayer::Model,
                None,
                None,
                None,
                "session-a",
            )
        })
        .expect("recoverable modal render");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Recoverable UI Error"));
    assert!(rendered.contains("state is unavailable"));
}

#[test]
fn composer_restores_bounded_draft_history_with_up_and_down() {
    let mut composer = Composer::default();
    for draft in ["first goal", "second goal"] {
        for character in draft.chars() {
            composer_action_for_key(&mut composer, KeyCode::Char(character), KeyModifiers::NONE);
        }
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Enter, KeyModifiers::NONE),
            ComposerAction::Submit(draft.to_string())
        );
    }
    composer.set_text("scratch".to_string());
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Up, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "second goal");
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Up, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "first goal");
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Down, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "second goal");
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Down, KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "scratch");
}

#[test]
fn composer_draft_history_drops_oldest_entries_beyond_the_bound() {
    let mut composer = Composer::default();
    for index in 0..=MAX_DRAFT_HISTORY {
        composer.remember_submission(&format!("goal-{index}"));
    }
    assert_eq!(composer.history.entries().len(), MAX_DRAFT_HISTORY);
    assert_eq!(composer.history.entries()[0], "goal-1");
    assert_eq!(
        composer.history.entries().last().map(String::as_str),
        Some("goal-50")
    );
}

#[test]
fn draft_history_search_restores_unicode_entry_and_preserves_current_draft() {
    let mut composer = Composer::from_text("current draft");
    composer.remember_submission("first");
    composer.remember_submission("fix 🙂 unicode");
    let mut search = PendingHistorySearch::new(&composer.history, Some("🙂".to_string()));
    assert_eq!(search.search.matches.len(), 1);
    let HistorySearchAction::Select(index) = history_search_action_for_key(
        &mut search,
        &composer.history,
        KeyCode::Enter,
        KeyModifiers::NONE,
    ) else {
        panic!("matching history entry must be selectable");
    };
    assert!(composer.select_history_entry(index));
    assert_eq!(composer.input, "fix 🙂 unicode");
    composer.recall_newer();
    assert_eq!(composer.input, "current draft");
}

#[test]
fn draft_history_search_empty_cancel_and_control_input_recover_in_overlay() {
    let history = DraftHistory::default();
    let mut search = PendingHistorySearch::new(&history, None);
    assert_eq!(
        search.error.as_deref(),
        Some("[history] no submitted drafts are available")
    );
    assert_eq!(
        history_search_action_for_key(&mut search, &history, KeyCode::Enter, KeyModifiers::NONE,),
        HistorySearchAction::Pending
    );
    assert_eq!(
        search.error.as_deref(),
        Some("[history error] select requires a matching draft")
    );
    search.insert('\0', &history);
    assert_eq!(search.query, "");
    assert_eq!(
        search.error.as_deref(),
        Some("[history error] control character ignored")
    );
    assert_eq!(
        history_search_action_for_key(&mut search, &history, KeyCode::Esc, KeyModifiers::NONE,),
        HistorySearchAction::Close
    );
}

#[test]
fn draft_history_overlay_is_control_safe_and_transcript_viewport_is_visible() {
    let mut history = DraftHistory::default();
    history.remember_submission("safe\0\u{1b} draft 🙂");
    let search = PendingHistorySearch::new(&history, None);
    let backend = TestBackend::new(84, 22);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut viewport = TranscriptViewport::default();
    let transcript = (0..30)
        .map(|index| format!("row-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let composer = Composer::default();
    let welcome = StartupWelcome::fixture();
    let activities = activities_from_timeline_text(&transcript);
    let band = InteractionBand::History(&search);
    terminal
        .draw(|frame| {
            render_session_activities(
                frame,
                &TuiChrome::fixture(),
                &activities,
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
            );
        })
        .expect("render history list");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("safe draft 🙂"), "{rendered}");
    assert!(rendered.contains("row-29"), "{rendered}");
    assert!(!rendered.contains("Draft History"), "{rendered}");
    assert!(!rendered.contains('\0'));
    assert!(!rendered.contains('\u{1b}'));
    assert!(viewport.is_pinned_to_tail());
}

#[test]
fn transcript_viewport_keeps_manual_row_on_append_and_submit_repins() {
    let backend = TestBackend::new(48, 16);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let composer = Composer::default();
    let mut viewport = TranscriptViewport::default();
    let first = (0..30)
        .map(|index| format!("row-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    terminal
        .draw(|frame| {
            render_current_session_view_with_viewport(
                frame,
                "header",
                "running",
                &first,
                &composer,
                None,
                None,
                true,
                &mut viewport,
            )
        })
        .expect("tail render");
    viewport.apply(TranscriptViewportAction::PageUp);
    let manual_top = viewport.top_row();
    assert!(!viewport.is_pinned_to_tail());

    let appended = format!("{first}\nrow-30\nrow-31");
    terminal
        .draw(|frame| {
            render_current_session_view_with_viewport(
                frame,
                "header",
                "running",
                &appended,
                &composer,
                None,
                None,
                true,
                &mut viewport,
            )
        })
        .expect("unpinned append render");
    assert_eq!(viewport.top_row(), manual_top);

    viewport.on_submission();
    assert!(viewport.is_pinned_to_tail());
    terminal
        .draw(|frame| {
            render_current_session_view_with_viewport(
                frame,
                "header",
                "idle",
                &appended,
                &composer,
                None,
                None,
                false,
                &mut viewport,
            )
        })
        .expect("repinned render");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("mode ask"));
    assert!(rendered.contains("row-31"));
}

#[test]
fn composer_height_follows_unicode_wrap_not_newline_count() {
    let composer = Composer::from_text("a".repeat(200));
    assert_eq!(composer_height(&composer, 80), 3);
    assert_eq!(composer_height(&Composer::default(), 80), 2);

    let combining = "e\u{301}";
    assert_eq!(composer_cursor_cell(combining, combining.len(), 4), (3, 0));
    assert_eq!(composer_cursor_cell("ab", 2, 4), (0, 1));
    assert_eq!(composer_cursor_cell("a\n漢", "a\n漢".len(), 4), (2, 1));

    let joined = "👩\u{200d}💻";
    assert_eq!(unicode_display_width(joined), 2);
    assert_eq!(composer_cursor_cell(joined, joined.len(), 4), (0, 1));

    let exact = Composer::from_text("ab");
    assert_eq!(composer_visual_rows(&exact, 4), ["> ab", ""]);
}

#[test]
fn composer_wraps_and_renders_complete_emoji_at_the_caret() {
    for grapheme in ["☺\u{fe0f}", "1\u{fe0f}\u{20e3}", "👩\u{200d}💻"] {
        let input = format!("a{grapheme}");
        let composer = Composer::from_text(input.as_str());
        assert_eq!(composer_cursor_cell(&input, input.len(), 4), (2, 1));
        assert_eq!(
            composer_visual_rows(&composer, 4),
            vec!["> a".to_string(), grapheme.to_string()]
        );
        let mut terminal = Terminal::new(TestBackend::new(4, 12)).expect("test terminal");
        terminal
            .draw(|frame| {
                render_current_session_view(frame, "nib", "idle", "", &composer, None, None)
            })
            .expect("render grapheme boundary");
        let cursor = terminal.get_cursor_position().expect("cursor");
        assert_eq!(cursor.x, 2);
        assert_eq!(
            terminal.backend().buffer()[(0, cursor.y)].symbol(),
            grapheme
        );
    }
}

#[test]
fn ledger_renders_exact_width_and_newline_carets_in_the_composer_viewport() {
    for (input, width, expected_x) in [("abcdef", 8, 0), ("a\n漢", 8, 2)] {
        let backend = TestBackend::new(width, 12);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let composer = Composer::from_text(input);
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "header",
                    "idle",
                    "you  hello",
                    &composer,
                    None,
                    None,
                )
            })
            .expect("render composer boundary");
        let position = terminal.get_cursor_position().expect("cursor");
        assert_eq!(position.x, expected_x, "input={input:?}");
        assert!(position.y < 11, "input={input:?}, position={position:?}");
    }
}

#[test]
fn ledger_places_the_caret_inside_the_composer_rect() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let composer = Composer::from_text("hi");
    terminal
        .draw(|frame| {
            render_current_session_view(
                frame,
                "header",
                "idle",
                "you  hello",
                &composer,
                None,
                None,
            )
        })
        .expect("render");
    let position = terminal.get_cursor_position().expect("cursor");
    assert!(
        position.y >= 21 && position.y <= 22,
        "caret y {} should be in the composer rows",
        position.y
    );
    assert_eq!(position.x, 4);
}

#[test]
fn tui_cancel_quit_and_switch_report_queue_disposition() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("session-a");
    store.create_session_with_id("session-b");
    persist_queued_follow_up(&store, "session-a", "next turn", "composer").expect("queue");

    let mut timeline = ActiveTimeline::load(&store, "session-a").expect("timeline");
    timeline.reconciled_terminal = Some(InteractionTerminalOutcome::Cancelled);
    let cancelled = tui_report_cancelled_run(&store, "session-a", &mut timeline).expect("cancel");
    assert!(cancelled.contains("cancelled;"));
    assert!(cancelled.contains("retained on session session-a"));
    assert!(timeline.rendered_text().contains(&cancelled));
    assert!(timeline
        .rendered_text()
        .contains("[cancelled] active agent run"));

    timeline.reconciled_terminal = Some(InteractionTerminalOutcome::Completed);
    let quit = tui_report_quit_run(&store, "session-a", &mut timeline).expect("quit run");
    assert!(quit.contains("[completed] active run completed before quit"));
    assert!(quit.contains("quit after completion;"));
    assert!(quit.contains("retained on session session-a"));
    assert!(timeline.rendered_text().contains(&quit));

    let exited = tui_exit_disposition(&store, "session-a").expect("exit");
    assert!(exited.contains("exited;"));
    assert!(exited.contains("retained on session session-a"));

    let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
    switcher.selected = switcher
        .candidates
        .iter()
        .position(|candidate| candidate.id == "session-b")
        .expect("target");
    let mut active_id = "session-a".to_string();
    let mut timeline = ActiveTimeline::load(&store, &active_id).expect("switch timeline");
    let switched =
        tui_complete_session_switch(&store, &switcher, false, &mut active_id, &mut timeline)
            .expect("switch");
    assert_eq!(active_id, "session-b");
    assert!(switched.contains("switched sessions;"));
    assert!(switched.contains("retained on session session-a"));
    assert!(timeline.rendered_text().contains(&switched));
}
