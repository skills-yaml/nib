use super::*;

#[test]
fn tui_environment_preflight_is_bounded_and_platform_neutral() {
    assert_eq!(
        tui_environment_rejection(true, true, Some("xterm-256color")),
        None
    );
    assert_eq!(tui_environment_rejection(true, true, None), None);
    assert_eq!(
        tui_environment_rejection(false, true, Some("xterm")),
        Some("the full-screen TUI requires terminal input and output")
    );
    assert_eq!(
        tui_environment_rejection(true, false, Some("xterm")),
        Some("the full-screen TUI requires terminal input and output")
    );
    assert_eq!(
        tui_environment_rejection(true, true, Some("DUMB")),
        Some("TERM=dumb does not support the full-screen TUI")
    );
}

#[test]
fn approval_dock_no_color_style_preserves_non_color_signaling() {
    assert_eq!(
        approval_dock_style(true),
        Style::default().add_modifier(Modifier::BOLD)
    );
    assert_eq!(
        composer_border_style(TuiFocus::Composer, true),
        Style::default().add_modifier(Modifier::BOLD)
    );
    assert_ne!(
        composer_border_style(TuiFocus::Composer, true),
        composer_border_style(TuiFocus::Transcript, true)
    );
    assert_eq!(
        approval_dock_style(false),
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(ratatui::style::Color::Yellow)
    );
}

#[test]
fn composer_accepts_chat_text_commands_and_enforces_its_bound() {
    let mut composer = Composer::default();
    for character in "/providers".chars() {
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Char(character), KeyModifiers::NONE,),
            ComposerAction::Pending
        );
    }
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Enter, KeyModifiers::NONE),
        ComposerAction::Submit("/providers".to_string())
    );
    assert!(composer.input.is_empty());
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Tab, KeyModifiers::NONE),
        ComposerAction::Pending
    );

    composer.set_text("x".repeat(MAX_COMPOSER_BYTES));
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Char('y'), KeyModifiers::NONE),
        ComposerAction::Pending
    );
    assert_eq!(composer.input.len(), MAX_COMPOSER_BYTES);

    let mut slash = Composer::from_text("/skills install ");
    assert_eq!(
        composer_action_for_key(&mut slash, KeyCode::Enter, KeyModifiers::NONE),
        ComposerAction::Submit("/skills install ".to_string())
    );
    assert!(slash.input.is_empty());
}

#[test]
fn shared_interaction_reducer_completion_owns_enter_tab_and_escape() {
    let mut composer = Composer::from_text("/");
    let mut completion = CompletionMenu::default();
    completion.sync(&composer.input);
    assert!(completion.is_open());
    assert!(completion.suggestions.len() <= 32);

    completion.selected = completion
        .suggestions
        .iter()
        .position(|item| item.insertion == "/session")
        .expect("session completion");
    assert_eq!(
        completion.handle_key(&mut composer, KeyCode::Tab),
        CompletionKeyResult::Consumed
    );
    assert_eq!(composer.input, "/session");
    assert!(!completion.is_open());

    composer.set_text("/".to_string());
    completion.sync(&composer.input);
    completion.selected = completion
        .suggestions
        .iter()
        .position(|item| item.insertion == "/help")
        .expect("help completion");
    assert_eq!(
        completion.handle_key(&mut composer, KeyCode::Enter),
        CompletionKeyResult::Submit
    );
    assert_eq!(composer.input, "/help");
    assert!(!completion.is_open());

    composer.input.push('x');
    completion.sync(&composer.input);
    assert!(!completion.is_open(), "unknown commands are not guessed");
    composer.set_text("/skills ".to_string());
    completion.sync(&composer.input);
    assert!(completion.is_open());
    let preserved = composer.input.clone();
    assert_eq!(
        completion.handle_key(&mut composer, KeyCode::Esc),
        CompletionKeyResult::Consumed
    );
    assert_eq!(composer.input, preserved);
    assert!(!completion.is_open());

    let mut skills = CompletionMenu::default();
    composer.set_text("/skills ".to_string());
    skills.sync(&composer.input);
    skills.selected = skills
        .suggestions
        .iter()
        .position(|item| item.insertion.ends_with(' '))
        .expect("free-form skill argument completion");
    assert_eq!(
        skills.handle_key(&mut composer, KeyCode::Enter),
        CompletionKeyResult::Consumed
    );
    assert!(composer.input.ends_with(' '));
}

#[test]
fn shift_enter_inserts_a_newline_instead_of_submitting() {
    let mut composer = Composer::from_text("hello");
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Enter, KeyModifiers::SHIFT),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "hello\n");
    assert_eq!(
        composer_action_for_key(&mut composer, KeyCode::Enter, KeyModifiers::ALT),
        ComposerAction::Pending
    );
    assert_eq!(composer.input, "hello\n\n");
}

#[test]
fn chrome_cache_skips_identical_idle_frames() {
    let cache = ChromeCache {
        generation: 1,
        session_id: "abc".to_string(),
        session_revision: Some(4),
        origin: "new".to_string(),
        chrome: TuiChrome::fixture(),
    };
    assert!(cache.matches(1, "abc", Some(4), "new"));
    assert!(!cache.matches(1, "abc", Some(5), "new"));
    assert!(!cache.matches(1, "other", Some(4), "new"));
    assert!(!cache.matches(2, "abc", Some(4), "new"));
    assert!(!cache.matches(1, "abc", Some(4), "resumed"));
}

#[test]
fn session_display_cache_refreshes_on_cadence_and_session_switch() {
    fn settle(
        cache: &mut SessionDisplayCache,
        store: &SessionStore,
        id: &str,
        now: Instant,
        min_revision: u64,
    ) {
        let start = Instant::now();
        let mut refresh_at = now;
        loop {
            cache.refresh(store, id, refresh_at);
            if cache.pending.is_none()
                && cache
                    .session
                    .as_ref()
                    .is_some_and(|session| session.id == id && session.revision >= min_revision)
            {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            if cache.pending.is_none() {
                refresh_at += Duration::from_secs(1);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("session-a");
    store.create_session_with_id("session-b");
    let now = Instant::now();
    let mut cache = SessionDisplayCache::default();
    cache.refresh(&store, "session-a", now);
    settle(&mut cache, &store, "session-a", now, 0);
    let initial_revision = cache.session.as_ref().unwrap().revision;
    store
        .try_append_message("session-a", "user", "new message")
        .expect("append message");
    cache.refresh(&store, "session-a", now + Duration::from_millis(999));
    assert_eq!(cache.session.as_ref().unwrap().revision, initial_revision);
    cache.refresh(&store, "session-a", now + Duration::from_secs(1));
    settle(
        &mut cache,
        &store,
        "session-a",
        now + Duration::from_secs(1),
        initial_revision + 1,
    );
    assert!(cache.session.as_ref().unwrap().revision > initial_revision);
    cache.refresh(&store, "session-b", now + Duration::from_secs(1));
    settle(
        &mut cache,
        &store,
        "session-b",
        now + Duration::from_secs(1),
        0,
    );
    assert_eq!(cache.session.as_ref().unwrap().id, "session-b");
}

#[test]
fn session_display_cache_waits_briefly_for_a_busy_session_lock() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("session-a");

    let (locked, ready) = mpsc::sync_channel(1);
    let held_store = store.clone();
    let holder = std::thread::spawn(move || {
        held_store
            .with_session_lock_for_testing("session-a", || {
                locked.send(()).expect("signal held lock");
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            })
            .expect("hold session lock");
    });
    ready
        .recv_timeout(Duration::from_secs(5))
        .expect("session lock acquired");

    let now = Instant::now();
    let mut cache = SessionDisplayCache::default();
    cache.refresh(&store, "session-a", now);
    holder.join().expect("release session lock");

    let start = Instant::now();
    while cache.pending.is_some() {
        cache.refresh(&store, "session-a", now);
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        cache.session.as_ref().map(|session| session.id.as_str()),
        Some("session-a")
    );
}

fn draw_cached_test_transcript(
    terminal: &mut Terminal<TestBackend>,
    activities: &[ActivityEntry],
    composer: &Composer,
    viewport: &mut TranscriptViewport,
    view: &mut TranscriptView,
    meter: &WaitingMeter,
) {
    terminal
        .draw(|frame| {
            render_session_activities(
                frame,
                &TuiChrome::fixture(),
                activities,
                composer,
                None,
                None,
                true,
                viewport,
                TuiFocus::Composer,
                None,
                0,
                None,
                Some(meter),
                &StartupWelcome::fixture(),
                None,
                None,
                Some(view),
            );
        })
        .expect("render test transcript");
}

fn long_test_activity(index: usize) -> ActivityEntry {
    ActivityEntry::new(
        ActivityKind::Assistant,
        "",
        format!("**reply {index}** with a wrapped markdown body"),
    )
}

#[test]
fn long_transcript_reuses_layout_until_source_or_live_second_changes() {
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    let mut viewport = TranscriptViewport::default();
    let mut view = TranscriptView {
        source_session: "session-a".to_string(),
        ..TranscriptView::default()
    };
    let mut composer = Composer::default();
    let mut activities = (0..300).map(long_test_activity).collect::<Vec<_>>();
    let mut meter = WaitingMeter {
        job: "working".to_string(),
        step: "1/2".to_string(),
        elapsed: Duration::ZERO,
        tokens: "tok 1k".to_string(),
        status: "running".to_string(),
        tick: 0,
    };
    let mut first = None;
    for frame_index in 0..2 {
        draw_cached_test_transcript(
            &mut terminal,
            &activities,
            &composer,
            &mut viewport,
            &mut view,
            &meter,
        );
        let rows = Arc::clone(&view.render_cache.as_ref().unwrap().rows);
        if let Some(initial) = first.as_ref() {
            assert!(Arc::ptr_eq(initial, &rows));
        } else {
            first = Some(rows);
        }
        if frame_index == 0 {
            meter.tick += 500;
            meter.elapsed += Duration::from_millis(500);
        }
    }
    let first = first.expect("initial transcript rows");
    composer.insert_str("typing while the agent runs");
    draw_cached_test_transcript(
        &mut terminal,
        &activities,
        &composer,
        &mut viewport,
        &mut view,
        &meter,
    );
    assert!(Arc::ptr_eq(
        &first,
        &view.render_cache.as_ref().unwrap().rows
    ));
    assert_eq!(view.plain_rows.len(), view.owners.len());
    assert!(view.plain_rows.iter().any(|row| row.contains("reply 299")));
    meter.tick += 500;
    meter.elapsed += Duration::from_millis(500);
    draw_cached_test_transcript(
        &mut terminal,
        &activities,
        &composer,
        &mut viewport,
        &mut view,
        &meter,
    );
    assert!(!Arc::ptr_eq(
        &first,
        &view.render_cache.as_ref().unwrap().rows
    ));
    let unchanged_first = Arc::clone(&view.render_cache.as_ref().unwrap().activity_rows[0].lines);
    let changed_last = Arc::clone(&view.render_cache.as_ref().unwrap().activity_rows[299].lines);
    activities[299].body = "updated final answer".to_string();
    view.source_generation += 1;
    draw_cached_test_transcript(
        &mut terminal,
        &activities,
        &composer,
        &mut viewport,
        &mut view,
        &meter,
    );
    let cache = view.render_cache.as_ref().unwrap();
    assert!(Arc::ptr_eq(&unchanged_first, &cache.activity_rows[0].lines));
    assert!(!Arc::ptr_eq(&changed_last, &cache.activity_rows[299].lines));
    assert!(view
        .plain_rows
        .iter()
        .any(|row| row.contains("updated final answer")));
    view.source_session = "session-b".to_string();
    assert!(!view
        .render_cache
        .as_ref()
        .unwrap()
        .matches(&view, 80, false, None,));
}

#[test]
fn bounded_stream_drain_keeps_remaining_events_ordered() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
    for index in 0..100 {
        tx.try_send(SessionStreamEvent {
            session_id: "session-a".to_string(),
            run_id: "run-a".to_string(),
            event: StreamEvent::Content(format!("{index:03},")),
        })
        .expect("enqueue event");
    }
    let mut timeline = ActiveTimeline {
        session_id: "session-a".to_string(),
        active_run_id: Some("run-a".to_string()),
        ..ActiveTimeline::default()
    };
    assert_eq!(drain_stream_events_bounded(&mut rx, &mut timeline, 64), 64);
    assert_eq!(timeline.render_generation, 64);
    assert_eq!(drain_stream_events_bounded(&mut rx, &mut timeline, 64), 36);
    assert_eq!(timeline.render_generation, 100);
    assert_eq!(
        timeline.live.text,
        (0..100)
            .map(|index| format!("{index:03},"))
            .collect::<String>()
    );
}

#[test]
fn selected_tool_block_can_expand_and_collapse() {
    let mut entry = ActivityEntry::new(
        ActivityKind::Tool,
        "list_directory ok · 1 entries",
        "{\"path\":\"README.md\"}",
    )
    .folded();
    assert!(entry.render_line().starts_with('›'));
    assert!(!entry.render_line().contains("README.md"));
    entry.folded = false;
    assert!(entry.render_line().contains("README.md"));
}

#[test]
fn osc52_copy_sequence_is_exact_and_utf8_safe() {
    assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(osc52_sequence("🙂"), "\x1b]52;c;8J+Zgg==\x07");
}

#[test]
fn composer_border_is_visible_and_focus_changes_its_style() {
    fn render_border_style(focus: TuiFocus) -> Style {
        let backend = TestBackend::new(72, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut viewport = TranscriptViewport::default();
        terminal
            .draw(|frame| {
                render_session_activities(
                    frame,
                    &TuiChrome::fixture(),
                    &[ActivityEntry::new(ActivityKind::Assistant, "", "ready")],
                    &Composer::default(),
                    None,
                    None,
                    false,
                    &mut viewport,
                    focus,
                    None,
                    0,
                    None,
                    None,
                    &StartupWelcome::fixture(),
                    None,
                    None,
                    None,
                )
            })
            .expect("render");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .find(|cell| cell.symbol() == "─")
            .expect("composer top border")
            .style()
    }

    let focused = render_border_style(TuiFocus::Composer);
    let transcript = render_border_style(TuiFocus::Transcript);
    assert_ne!(focused, transcript);
}

#[test]
fn selected_transcript_block_is_kept_inside_the_row_viewport() {
    let owners = (0..12).flat_map(|owner| [owner, owner]).collect::<Vec<_>>();
    let mut viewport = TranscriptViewport::default();
    viewport.observe_layout(owners.len(), 5);
    assert_eq!(viewport.top_row(), 19);

    ensure_selected_visible(&mut viewport, &owners, 2);
    assert_eq!(viewport.top_row(), 4);
    ensure_selected_visible(&mut viewport, &owners, 11);
    assert_eq!(viewport.top_row(), 19);
}

#[test]
fn empty_session_invites_a_conversation_and_keeps_the_prompt_visible() {
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render_current_session_view(
                frame,
                "nib  ·  project  ·  session abc12345  ·  new",
                "idle  ·  mock/mock-model  ·  approval manual  ·  sandboxed",
                "",
                &Composer::default(),
                None,
                None,
            )
        })
        .expect("render empty session");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(!rendered.contains("Your AI agent for this project."));
    assert!(rendered.contains(&format!("Nib {}", env!("CARGO_PKG_VERSION"))));
    assert!(rendered.contains("Working directory"));
    assert!(rendered.contains("~/project"));
    assert!(rendered.contains("/new"));
    assert!(rendered.contains("new session and worktree"));
    assert!(rendered.contains("/session"));
    assert!(rendered.contains("Ctrl+C"));
    assert!(rendered.contains("never copies or quits"));
    assert!(rendered.contains("> Ask nib anything…"));
    assert!(rendered.contains("approval manual"));
    assert!(rendered.contains("idle"));
    assert_eq!(terminal.get_cursor_position().expect("cursor").x, 2);
}

#[test]
fn startup_asks_permission_to_work_in_the_working_directory() {
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut viewport = TranscriptViewport::default();
    let welcome = StartupWelcome {
        version: format!("Nib {}", env!("CARGO_PKG_VERSION")),
        working_directory: "~/work/nib".to_string(),
        update_notice: None,
        consent_directory: Some("~/work/nib".to_string()),
        consent_selected: 0,
    };
    terminal
        .draw(|frame| {
            render_session_activities(
                frame,
                &TuiChrome::fixture(),
                &[],
                &Composer::default(),
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
                None,
                None,
                None,
            )
        })
        .expect("render workspace consent");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Work in this directory"), "{rendered}");
    assert!(rendered.contains("~/work/nib"), "{rendered}");
    assert!(rendered.contains("WAITING PERMISSION"), "{rendered}");
    assert!(rendered.contains("Allow"), "{rendered}");
    assert!(rendered.contains("Decline"), "{rendered}");
    assert!(!rendered.contains("Permission required"), "{rendered}");
    assert!(
        rendered.contains("Enter decline · select Allow then Enter"),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("Nib {}", env!("CARGO_PKG_VERSION"))),
        "{rendered}"
    );
}

#[test]
fn workspace_consent_key_reducer_persists_the_gate_before_a_goal_can_start() {
    let project = tempdir().expect("project");
    let mut consent_directory = pending_workspace_consent(project.path());
    let mut pending_goal = Some("inspect the project".to_string());
    assert!(consent_directory.is_some());

    let mut selected = 0;
    assert_eq!(
        workspace_consent_action_for_key(&mut selected, KeyCode::Down, KeyModifiers::NONE),
        WorkspaceConsentAction::SelectionChanged
    );
    assert_eq!(selected, 1);
    assert_eq!(
        workspace_consent_action_for_key(&mut selected, KeyCode::Enter, KeyModifiers::NONE),
        WorkspaceConsentAction::Decline
    );
    assert!(pending_workspace_consent(project.path()).is_some());
    assert_eq!(pending_goal.as_deref(), Some("inspect the project"));

    selected = 0;
    assert_eq!(
        workspace_consent_action_for_key(&mut selected, KeyCode::Char('y'), KeyModifiers::NONE),
        WorkspaceConsentAction::Allow
    );
    let released = grant_workspace_access_and_release_goal(
        project.path(),
        &mut consent_directory,
        &mut pending_goal,
    )
    .expect("persist workspace grant before releasing goal");
    assert_eq!(released.as_deref(), Some("inspect the project"));
    assert!(pending_goal.is_none());
    assert!(consent_directory.is_none());
    assert!(pending_workspace_consent(project.path()).is_none());
    assert!(crate::config::workspace_access_is_granted(project.path()).expect("read grant"));
    assert!(grant_workspace_access_and_release_goal(
        project.path(),
        &mut consent_directory,
        &mut pending_goal,
    )
    .expect("reload persisted workspace grant")
    .is_none());
}

#[test]
fn empty_state_includes_update_notice_and_install_instruction() {
    let welcome = StartupWelcome {
        version: "Nib 0.1.0".to_string(),
        working_directory: "~/work/nib".to_string(),
        update_notice: Some(
            "Channel update available: 0.1.1 (development, abcdef0). Run `nib update`.".to_string(),
        ),
        consent_directory: None,
        consent_selected: 0,
    };
    let rendered = empty_state_lines(&welcome, true)
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Nib 0.1.0"), "{rendered}");
    assert!(rendered.contains("~/work/nib"), "{rendered}");
    assert!(
        rendered.contains("Channel update available: 0.1.1"),
        "{rendered}"
    );
    assert!(rendered.contains("Run `nib update`"), "{rendered}");
    assert!(rendered.contains("/new"), "{rendered}");
    assert!(rendered.contains("Ctrl+C"), "{rendered}");
}

#[test]
fn ctrl_q_requires_two_uninterrupted_presses() {
    let now = Instant::now();
    let arm = QuitArm {
        armed_at: now,
        consumer: InteractionLayer::Composer,
    };
    assert_eq!(
        quit_confirm_action(
            Some(arm),
            now + Duration::from_millis(400),
            InteractionLayer::Composer,
        ),
        QuitConfirmAction::Confirm
    );
    assert_eq!(
        quit_confirm_action(None, now, InteractionLayer::Composer),
        QuitConfirmAction::Arm
    );
    assert_eq!(quit_arm_after_input(Some(arm), false), None);
    assert_eq!(quit_arm_after_input(Some(arm), true), Some(arm));
    assert_eq!(
        quit_confirm_action(
            quit_arm_after_input(Some(arm), false),
            now + Duration::from_millis(400),
            InteractionLayer::Composer,
        ),
        QuitConfirmAction::Arm
    );
    assert_eq!(
        quit_confirm_action(
            Some(arm),
            now + QUIT_CONFIRM + Duration::from_millis(1),
            InteractionLayer::Composer,
        ),
        QuitConfirmAction::Arm,
        "the confirmation window must expire"
    );
    assert_eq!(
        quit_arm_for_consumer(Some(arm), InteractionLayer::Approval),
        None,
        "a new modal consumer must disarm quit confirmation"
    );
}

#[test]
fn startup_welcome_strips_stderr_prefix_from_update_notice() {
    let welcome = StartupWelcome::new(
        Path::new("/tmp/project"),
        Some("[nib] Channel update available: 0.1.1. Run `nib update`.".to_string()),
    );
    assert_eq!(
        welcome.version,
        format!("Nib {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        welcome.update_notice.as_deref(),
        Some("Channel update available: 0.1.1. Run `nib update`.")
    );
}

#[test]
fn transcript_separates_thought_tools_and_user_facing_speech() {
    let activities = vec![
        ActivityEntry::new(ActivityKind::User, "", "inspect wrap"),
        ActivityEntry::new(ActivityKind::Thinking, "Thought for 14s", String::new()).folded(),
        ActivityEntry::new(
            ActivityKind::Tool,
            "read_file running · src/lib.rs",
            "line one",
        )
        .folded(),
        ActivityEntry::new(ActivityKind::Assistant, "", "Here is the answer"),
    ];
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut viewport = TranscriptViewport::default();
    let welcome = StartupWelcome::fixture();
    terminal
        .draw(|frame| {
            render_session_activities(
                frame,
                &TuiChrome::fixture(),
                &activities,
                &Composer::default(),
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
                None,
                None,
                None,
            )
        })
        .expect("render channels");
    let rows = buffer_rows(&terminal);
    let joined = rows.concat();
    assert!(joined.contains("inspect wrap"), "{joined}");
    assert!(joined.contains("Thought for 14s"), "{joined}");
    assert!(joined.contains("●"), "{joined}");
    assert!(joined.contains("read_file"), "{joined}");
    assert!(joined.contains("Reading…"), "{joined}");
    assert!(joined.contains("Here is the answer"), "{joined}");
    assert!(!joined.contains("planning"), "{joined}");
    assert!(!joined.contains(" running"), "{joined}");
    assert!(!joined.contains("line one"), "{joined}");
    assert!(!joined.contains("● you"), "no you role label: {joined}");
    assert!(!joined.contains("● nib"), "no nib role label: {joined}");
    assert!(!joined.contains("thought"), "{joined}");
    assert!(
        !rows.iter().any(|row| row.contains("● tool")),
        "tool rows use the tool name, not a tool role label: {rows:?}"
    );
    let you = row_index_containing(&rows, "inspect wrap").expect("user");
    let thought = row_index_containing(&rows, "Thought for 14s").expect("thought");
    let tool = row_index_containing(&rows, "read_file").expect("tool");
    let speech = rows
        .iter()
        .position(|row| row.contains("Here is the answer"))
        .expect("speech");
    assert!(you < thought, "{rows:?}");
    assert!(thought < tool, "{rows:?}");
    assert!(tool < speech, "{rows:?}");
    assert!(
        rows[you].contains('●'),
        "user input is marked with a colored dot: {}",
        rows[you]
    );
    assert!(
        rows[thought].contains('▸'),
        "thought is a folded chevron header: {}",
        rows[thought]
    );
    assert!(
        !rows[thought].contains('●'),
        "thought does not use the tool dot: {}",
        rows[thought]
    );
    assert!(
        rows[tool].contains('●'),
        "tool calls are marked with a colored dot: {}",
        rows[tool]
    );
    assert!(
        rows[speech].contains('●'),
        "assistant speech starts with a colored dot: {}",
        rows[speech]
    );
    assert!(
        rows[speech].contains("nib") || rows[speech].contains('●'),
        "assistant speech has a role or channel marker: {}",
        rows[speech]
    );
}

#[test]
fn quiet_tool_title_hides_running_and_ok() {
    assert_eq!(
        quiet_tool_title("read_file running · src/lib.rs"),
        "read_file  src/lib.rs"
    );
    assert_eq!(
        quiet_tool_title("read_file ok · src/lib.rs · 3 lines"),
        "read_file  src/lib.rs · 3 lines"
    );
    assert_eq!(
        quiet_tool_title("run_terminal failed · task backup · exit 1"),
        "run_terminal failed · task backup · exit 1"
    );
    assert_eq!(
        thought_header_title(Duration::from_secs(14), Some("267")),
        "Thought for 14s, 267 tokens"
    );
    assert_eq!(
        thought_header_title(Duration::from_secs(14), None),
        "Thought for 14s"
    );
    assert_eq!(meter_token_label("tok -"), None);
    assert_eq!(meter_token_label("tok 8k").as_deref(), Some("8k"));
}

#[test]
fn header_shows_folder_and_branch_left_and_model_right() {
    let chrome = TuiChrome {
        folder: "~/work/nib".to_string(),
        branch: "feat/t039".to_string(),
        model: "mock-model".to_string(),
        context: "ctx ~12/100".to_string(),
        approval: "manual".to_string(),
        agent_mode: "idle".to_string(),
    };
    let line = header_line(&chrome, 80, true);
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.starts_with("~/work/nib"), "{text}");
    assert!(text.contains("feat/t039"), "{text}");
    assert!(
        text.find("~/work/nib").expect("folder") < text.find("mock-model").expect("model"),
        "{text}"
    );
    assert!(text.trim_end().ends_with("ctx ~12/100"), "{text}");
    let narrow = header_line(&chrome, 28, true);
    let narrow_text: String = narrow
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(
        narrow_text.contains("ctx ~12%") || narrow_text.contains("ctx ~12/100"),
        "{narrow_text}"
    );
    let folder_end = text.find("feat/t039").expect("branch") + "feat/t039".len();
    let model_start = text.find("mock-model").expect("model");
    assert!(
        text[folder_end..model_start].chars().all(|ch| ch == ' '),
        "model must sit on the right of the header: {text:?}"
    );
}

#[test]
fn speech_renders_markdown_headings_lists_and_fenced_code() {
    let activities = vec![ActivityEntry::new(
        ActivityKind::Assistant,
        "",
        "# Fix\n\nUse `task check` then:\n\n```rust\nfn main() {}\n```\n\n- one\n- two\n",
    )];
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let mut viewport = TranscriptViewport::default();
    let welcome = StartupWelcome::fixture();
    terminal
        .draw(|frame| {
            render_session_activities(
                frame,
                &TuiChrome::fixture(),
                &activities,
                &Composer::default(),
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
                None,
                None,
                None,
            )
        })
        .expect("render markdown speech");
    let rows = buffer_rows(&terminal);
    let joined = rows.concat();
    assert!(joined.contains("# Fix"), "{joined}");
    assert!(joined.contains("task check"), "{joined}");
    assert!(joined.contains("fn main()"), "{joined}");
    assert!(joined.contains("• one"), "{joined}");
    assert!(joined.contains("• two"), "{joined}");
    assert!(
        !joined.contains("```"),
        "fenced markers should be rendered away: {joined}"
    );
}

#[test]
fn activity_styles_keep_text_labels_without_color() {
    let line = dotted_header_lines(ActivityKind::Assistant, "", "", 40, true)
        .into_iter()
        .next()
        .expect("header");
    assert_eq!(line.spans[0].content, "● ");
    assert_eq!(line.spans[0].style.fg, None);
    assert!(line.spans.get(1).is_none_or(|span| span.content != "nib"));
    let tool = dotted_header_lines(
        ActivityKind::Tool,
        "read_file ok · src/lib.rs",
        "",
        40,
        true,
    )
    .into_iter()
    .next()
    .expect("tool header");
    assert_eq!(tool.spans[0].content, "● ");
    assert_eq!(tool.spans[1].content, "read_file ok · src/lib.rs");
    let result = dotted_result_lines("line one", 40, ChannelInk::ToolResult, true);
    assert_eq!(result[0].spans[0].content, "· ");
    assert_eq!(result[0].spans[1].content, "line one");
    assert_eq!(result[0].spans[0].style.fg, None);
}

#[test]
fn footer_and_completion_follow_the_current_interaction() {
    let mut viewport = TranscriptViewport::default();
    let chrome = TuiChrome::fixture();
    assert_eq!(
        footer_line(&chrome, &viewport, 0, WaitingKind::None, None, false),
        "approval manual · idle"
    );
    let mut running = chrome.clone();
    running.agent_mode = "execute".to_string();
    assert_eq!(
        footer_line(&running, &viewport, 0, WaitingKind::None, None, false),
        "approval manual · execute"
    );
    assert_eq!(
        footer_line(&running, &viewport, 2, WaitingKind::None, None, false),
        "queue 2 · approval manual · execute"
    );
    let mut waiting = chrome.clone();
    waiting.agent_mode = "WAITING APPROVAL".to_string();
    assert_eq!(
            footer_line(&waiting, &viewport, 0, WaitingKind::Approval, None, false),
            "approval manual · WAITING APPROVAL · Enter deny · select Approve once then Enter · Esc deny"
        );
    let mut question = chrome.clone();
    question.agent_mode = "WAITING QUESTION".to_string();
    assert_eq!(
        footer_line(&question, &viewport, 0, WaitingKind::Question, None, false),
        "approval manual · WAITING QUESTION · Up/Down select · Enter choose · Esc interrupt operation"
    );
    let mut workspace = chrome.clone();
    workspace.agent_mode = "WAITING PERMISSION".to_string();
    assert_eq!(
            footer_line(
                &workspace,
                &viewport,
                0,
                WaitingKind::Workspace,
                None,
                false
            ),
            "approval manual · WAITING PERMISSION · Enter decline · select Allow then Enter · Esc decline"
        );
    assert!(footer_line(
        &chrome,
        &viewport,
        0,
        WaitingKind::None,
        Some("Tab insert · Enter run · Esc close"),
        false,
    )
    .contains("Tab insert"));
    assert!(
        footer_line(&chrome, &viewport, 0, WaitingKind::None, None, true)
            .contains("Ctrl+Y copy · Esc clear")
    );
    viewport.observe_layout(100, 10);
    viewport.apply(TranscriptViewportAction::PageUp);
    assert!(
        footer_line(&running, &viewport, 0, WaitingKind::None, None, false)
            .contains("Ctrl+End follow")
    );

    let area = Rect::new(0, 0, 100, 30);
    let composer_height = 2;
    let completion = completion_rect(area, MAX_VISIBLE_COMPLETIONS, composer_height);
    assert_eq!(completion.width, 92);
    let composer_bottom = completion.y;
    assert!(completion.height >= 2, "{completion:?}");
    assert_eq!(
        completion.y + completion.height,
        area.height.saturating_sub(1),
        "{completion:?}"
    );
    assert!(
        composer_bottom >= composer_height,
        "completion {completion:?} must sit under the composer"
    );

    let multiline = completion_rect(Rect::new(0, 0, 80, 24), MAX_VISIBLE_COMPLETIONS, 6);
    let composer_bottom = 24u16.saturating_sub(1).saturating_sub(multiline.height);
    assert_eq!(multiline.y, composer_bottom, "{multiline:?}");
    assert!(
        multiline.y >= 6,
        "completion must start at or below the composer bottom: {multiline:?}"
    );
    assert_eq!(multiline.y + multiline.height, 23, "{multiline:?}");

    let permissions = interactive_completions("/permissions ");
    let rows = permissions
        .iter()
        .map(|suggestion| completion_line(suggestion, 76))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(rows.len(), 4, "fixed subcommands must have distinct rows");
    assert!(rows.iter().any(|row| row.contains("/permissions manual")));
    assert!(rows
        .iter()
        .all(|row| !row.starts_with('>') && !row.starts_with("  /")));
    assert!(rows.iter().all(|row| unicode_display_width(row) <= 76));
}

#[test]
fn completion_truncation_preserves_complete_graphemes() {
    for grapheme in ["☺\u{fe0f}", "👩\u{200d}💻"] {
        let text = format!("a{grapheme}xy");
        assert_eq!(truncate_completion_text(&text, 3), "a…");
        assert_eq!(truncate_completion_text(&text, 4), format!("a{grapheme}…"));
    }
    assert_eq!(truncate_completion_text("e\u{301}xyz", 2), "e\u{301}…");
}

#[test]
fn slash_completion_renders_under_the_composer_without_covering_conversation() {
    let mut completion = CompletionMenu::default();
    completion.sync("/");
    assert!(completion.is_open());
    let composer = Composer::from_text("/");
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render_session_view_with_completion(
                frame,
                "nib · project · session abc",
                "idle · mock/mock-model",
                "you  hello conversation\n\nnib  keep this visible",
                &composer,
                Some(&completion),
                None,
                false,
            )
        })
        .expect("render slash completion");
    let rows = buffer_rows(&terminal);
    let conversation = row_index_containing(&rows, "hello conversation").expect("conversation row");
    let reply = row_index_containing(&rows, "keep this visible").expect("reply row");
    let prompt = row_index_containing(&rows, "> /").expect("composer row");
    let suggestion = row_index_containing(&rows, "/status").expect("completion row");
    assert!(
        conversation < prompt,
        "conversation must stay above the input: {rows:?}"
    );
    assert!(
        reply < prompt,
        "assistant text must stay above the input: {rows:?}"
    );
    assert!(
        prompt < suggestion,
        "slash options must render under the text input: {rows:?}"
    );
    let prompt_slash = rows[prompt].find('/').expect("composer slash");
    let option_slash = rows[suggestion].find('/').expect("option slash");
    assert_eq!(
        prompt_slash, option_slash,
        "options must align to the composer /: prompt={:?} option={:?}",
        rows[prompt], rows[suggestion]
    );
    assert!(
        !rows[suggestion].contains("> /") && !rows[suggestion].contains("> /status"),
        "completion list must not use a caret: {}",
        rows[suggestion]
    );
}

#[test]
fn waiting_meter_shows_job_step_time_tokens_and_status() {
    let meter = WaitingMeter {
        job: "read_file · src/lib.rs".to_string(),
        step: "2/5".to_string(),
        elapsed: Duration::from_secs(12),
        tokens: "tok 8k".to_string(),
        status: "running".to_string(),
        tick: 0,
    };
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            render_session_view_with_completion(
                frame,
                "nib · project · session abc",
                "running · mock/mock-model",
                "you  inspect wrap",
                &Composer::default(),
                None,
                Some(&meter),
                true,
            )
        })
        .expect("render waiting meter");
    let rows = buffer_rows(&terminal);
    let joined = rows.concat();
    assert!(joined.contains("read_file · src/lib.rs"), "{joined}");
    assert!(joined.contains("step 2/5"), "{joined}");
    assert!(joined.contains("12s"), "{joined}");
    assert!(joined.contains("tok 8k"), "{joined}");
    assert!(joined.contains("running"), "{joined}");
    let meter_row = row_index_containing(&rows, "step 2/5").expect("meter row");
    let prompt = rows
        .iter()
        .position(|row| row.contains("> Ask nib anything"))
        .expect("composer row");
    assert!(
        meter_row < prompt,
        "waiting meter must sit above the text input: {rows:?}"
    );
    assert_eq!(
        live_job_label(
            &[ActivityEntry::new(
                ActivityKind::Tool,
                "read_file running · src/lib.rs",
                String::new(),
            )],
            Some("planning"),
        ),
        "read_file · src/lib.rs"
    );
    assert_eq!(format_elapsed(Duration::from_secs(75)), "1m15s");

    let compact = waiting_meter_text(&meter, 40, true);
    assert!(unicode_display_width(&compact) <= 40, "{compact}");
    assert!(compact.contains("s:2/5"), "{compact}");
    assert!(compact.contains("12s"), "{compact}");
    assert!(compact.contains("t:8k"), "{compact}");
    assert!(compact.contains("running"), "{compact}");
}

#[test]
fn current_session_view_is_primary_and_has_no_permanent_browser() {
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    let composer = Composer::default();
    terminal
        .draw(|frame| {
            render_current_session_view(
                frame,
                "workspace  ·  sess active-one  ·  local  ·  -",
                "idle  ·  mock/mock-model  ·  manual/hybrid net restricted  ·  queue 0",
                "you  persisted request\n\nnib  persisted reply",
                &composer,
                None,
                None,
            )
        })
        .expect("render current session view");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("workspace"));
    assert!(rendered.contains("main"));
    assert!(rendered.contains("mock-model"));
    assert!(rendered.contains("idle"));
    assert!(rendered.contains("persisted request"));
    assert!(rendered.contains("persisted reply"));
    assert!(!rendered.contains("nibble sessions"));
}

#[test]
fn session_switch_dispatch_previews_confirms_and_strictly_reloads() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    for (id, message) in [("session-a", "from a"), ("session-b", "from b")] {
        store.create_session_with_id(id);
        store
            .try_append_message(id, "user", message)
            .expect("append persisted message");
    }
    let before_a = store.load("session-a").expect("session a");
    let before_b = store.load("session-b").expect("session b");
    let mut active_id = "session-a".to_string();
    let mut timeline = ActiveTimeline::load(&store, &active_id).expect("active timeline");
    timeline.push_status("live output owned by a".to_string());
    let composer = Composer::from_text("draft survives browsing");
    let mut switcher = load_session_switcher(&store, &active_id).expect("switcher");
    switcher.selected = switcher
        .candidates
        .iter()
        .position(|candidate| candidate.id == "session-b")
        .expect("target session");

    assert_eq!(
        session_switcher_action_for_key(&mut switcher, KeyCode::Enter),
        SwitcherAction::Pending
    );
    assert!(switcher.confirming);
    assert_eq!(active_id, "session-a");
    assert_eq!(composer.input, "draft survives browsing");
    assert_eq!(
        session_switcher_action_for_key(&mut switcher, KeyCode::Esc),
        SwitcherAction::Pending
    );
    assert!(!switcher.confirming);
    assert_eq!(active_id, "session-a");
    assert_eq!(composer.input, "draft survives browsing");

    assert!(
        activate_selected_session(&store, &switcher, true, &mut active_id, &mut timeline,).is_err()
    );
    assert_eq!(active_id, "session-a");
    assert!(timeline.rendered_text().contains("live output owned by a"));
    assert_eq!(composer.input, "draft survives browsing");

    activate_selected_session(&store, &switcher, false, &mut active_id, &mut timeline)
        .expect("confirmed switch");
    assert_eq!(active_id, "session-b");
    assert_eq!(timeline.session_id, "session-b");
    assert!(timeline.rendered_text().contains("from b"));
    assert!(!timeline.rendered_text().contains("live output owned by a"));
    assert_eq!(composer.input, "draft survives browsing");
    assert_eq!(
        store.load("session-a").expect("session a unchanged"),
        before_a
    );
    assert_eq!(
        store.load("session-b").expect("session b unchanged"),
        before_b
    );
}

#[test]
fn stale_switch_target_fails_without_changing_session_or_draft() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("session-a");
    store.create_session_with_id("session-b");
    let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
    switcher.selected = switcher
        .candidates
        .iter()
        .position(|candidate| candidate.id == "session-b")
        .expect("target");
    std::fs::write(store.sessions_dir().join("session-b.json"), "not json")
        .expect("corrupt stale target");

    let mut active_id = "session-a".to_string();
    let mut timeline = ActiveTimeline::load(&store, &active_id).expect("timeline");
    let original = timeline.rendered_text();
    let composer = Composer::from_text("keep this draft");
    let error = activate_selected_session(&store, &switcher, false, &mut active_id, &mut timeline)
        .expect_err("corrupt target must fail closed");
    assert!(error.contains("active session is unchanged"));
    assert_eq!(active_id, "session-a");
    assert_eq!(timeline.rendered_text(), original);
    assert_eq!(composer.input, "keep this draft");
}

#[test]
fn changed_switch_target_requires_a_fresh_preview() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("session-a");
    store.create_session_with_id("session-b");
    let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
    switcher.selected = switcher
        .candidates
        .iter()
        .position(|candidate| candidate.id == "session-b")
        .expect("target");
    store
        .try_append_message("session-b", "user", "changed after preview")
        .expect("change target");

    let mut active_id = "session-a".to_string();
    let mut timeline = ActiveTimeline::load(&store, &active_id).expect("timeline");
    let original = timeline.rendered_text();
    let error = activate_selected_session(&store, &switcher, false, &mut active_id, &mut timeline)
        .expect_err("changed target must fail closed");

    assert!(error.contains("changed since it was previewed"));
    assert_eq!(active_id, "session-a");
    assert_eq!(timeline.rendered_text(), original);
}

#[test]
fn bounded_switcher_retains_and_marks_an_old_active_session() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    for index in 0..=100 {
        store.create_session_with_id(format!("session-{index:03}"));
    }

    let mut switcher = load_session_switcher(&store, "session-000").expect("switcher");
    assert_eq!(switcher.candidates.len(), 100);
    assert_eq!(switcher.omitted, 1);
    assert_eq!(switcher.candidates[switcher.selected].id, "session-000");

    let omitted_id = (0..=100)
        .map(|index| format!("session-{index:03}"))
        .find(|id| {
            !switcher
                .candidates
                .iter()
                .any(|candidate| &candidate.id == id)
        })
        .expect("one omitted session");
    for character in omitted_id.chars() {
        assert_eq!(
            session_switcher_action_for_key(&mut switcher, KeyCode::Char(character)),
            SwitcherAction::Pending
        );
    }
    let SwitcherAction::PreviewExact(requested) =
        session_switcher_action_for_key(&mut switcher, KeyCode::Enter)
    else {
        panic!("exact ID entry must request a preview")
    };
    preview_exact_session(&store, &mut switcher, "session-000", &requested)
        .expect("preview omitted target");
    assert_eq!(switcher.candidates.len(), MAX_SWITCHER_CANDIDATES);
    assert!(switcher
        .candidates
        .iter()
        .any(|candidate| candidate.id == "session-000" && candidate.is_active));
    assert_eq!(switcher.candidates[switcher.selected].id, omitted_id);
    assert_eq!(
        session_switcher_action_for_key(&mut switcher, KeyCode::Enter),
        SwitcherAction::Pending
    );
    assert!(switcher.confirming);

    switcher.confirming = false;
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).expect("short switcher terminal");
    terminal
        .draw(|frame| render_session_switcher(frame, frame.area(), &switcher, "session-000"))
        .expect("render selected tail candidate");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains(&omitted_id));
}

#[test]
fn clear_effect_replaces_the_entire_visible_session_projection() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("old-session");
    store
        .try_append_message("old-session", "user", "old persisted text")
        .expect("old message");
    let mut active_id = "old-session".to_string();
    let mut timeline = ActiveTimeline::load(&store, &active_id).expect("timeline");
    timeline.push_status("old live text".to_string());

    let InteractiveEffect::SessionChanged { session_id, output } = execute_interactive_command(
        crate::interactive::InteractiveCommand::Clear,
        directory.path(),
        &store,
        &active_id,
    )
    .expect("clear effect") else {
        panic!("clear must create a session")
    };
    replace_active_session(
        &store,
        session_id.clone(),
        output,
        &mut active_id,
        &mut timeline,
    )
    .expect("replace timeline");

    assert_eq!(active_id, session_id);
    assert_eq!(timeline.session_id, session_id);
    assert!(!timeline.rendered_text().contains("old persisted text"));
    assert!(!timeline.rendered_text().contains("old live text"));
    assert!(timeline.rendered_text().contains("Started fresh session"));
}

#[test]
fn model_picker_accepts_selection_exact_ids_and_cancel() {
    let selection = ModelSelection {
        provider: "mock".to_string(),
        current: "mock-a".to_string(),
        available: vec!["mock-a".to_string(), "mock-b".to_string()],
        sensitive_values: Vec::new(),
    };
    let mut picker = PendingModelSelection::new(selection.clone());
    assert_eq!(picker.selected_option, 0);
    assert_eq!(
        model_action_for_key(&mut picker, KeyCode::Down),
        ModelAction::Pending
    );
    assert_eq!(
        model_action_for_key(&mut picker, KeyCode::Enter),
        ModelAction::Submit("mock-b".to_string())
    );

    let mut exact = PendingModelSelection::new(selection);
    for character in "gateway/custom".chars() {
        assert_eq!(
            model_action_for_key(&mut exact, KeyCode::Char(character)),
            ModelAction::Pending
        );
    }
    assert_eq!(
        model_action_for_key(&mut exact, KeyCode::Enter),
        ModelAction::Submit("gateway/custom".to_string())
    );
    assert_eq!(
        model_action_for_key(&mut exact, KeyCode::Esc),
        ModelAction::Cancel
    );
}

#[test]
fn tui_resolves_the_selected_profile_session_store() {
    let directory = tempdir().expect("tempdir");
    let mut config = NibConfig {
        profiles: ProfilesConfig {
            default: "workspace".to_string(),
            active: vec![ProfileConfig {
                id: "workspace".to_string(),
                root: PathBuf::from("."),
                ..ProfileConfig::default()
            }],
        },
        ..NibConfig::default()
    };
    save_nib_config_full(directory.path(), &mut config).expect("save config");

    let store = SessionStore::for_project(directory.path()).expect("profile store");
    let expected = directory.path().join(".nib/profiles/workspace/sessions");
    let actual_directory = crate::daemons::state::StableDirectory::open(store.sessions_dir())
        .expect("opened profile session store");
    let expected_directory = crate::daemons::state::StableDirectory::open(&expected)
        .expect("opened expected session store");
    assert!(
        actual_directory.same_identity(&expected_directory),
        "selected profile session store resolved to another directory"
    );
    assert!(!directory.path().join(".nib/sessions").exists());
}

#[test]
fn active_timeline_hydrates_persisted_history_with_explicit_bounds() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    store.create_session_with_id("detail-session");
    store
        .try_append_message("detail-session", "user", "inspect this session")
        .expect("user message");
    store
        .try_append_message("detail-session", "assistant", "inspection complete")
        .expect("assistant message");

    let timeline = ActiveTimeline::load(&store, "detail-session").expect("load timeline");
    let persisted = store.load("detail-session").expect("persisted session");
    let detail = SessionDetail::new("detail-session", Some(&persisted), &[]);
    assert_eq!(timeline.session_id, "detail-session");
    assert!(detail.text.contains("Session: detail-session"));
    assert!(detail.text.contains("Messages: 2"));
    assert!(detail.text.contains("inspect this session"));
    assert!(detail.text.len() <= MAX_SESSION_DETAIL_BYTES);
    assert!(detail.text.lines().count() <= MAX_SESSION_DETAIL_ROWS);
    assert!(timeline.activities.iter().any(
        |entry| entry.kind == ActivityKind::User && entry.body.contains("inspect this session")
    ));
}

#[test]
fn session_listing_surfaces_store_errors() {
    let directory = tempdir().expect("tempdir");
    let sessions = directory.path().join("sessions");
    let store = SessionStore::at_dir(sessions.clone());
    std::fs::write(sessions.join("invalid session id.json"), "{}").expect("invalid session");

    let error =
        interactive_session_selection(&store, "active").expect_err("invalid listing must fail");
    assert!(error.contains("failed to list sessions"));
    assert!(error.contains("invalid session id"));
}

#[test]
fn session_listing_surfaces_valid_named_corrupt_state() {
    let directory = tempdir().expect("tempdir");
    let sessions = directory.path().join("sessions");
    let store = SessionStore::at_dir(sessions.clone());
    std::fs::write(sessions.join("corrupt-session.json"), "not json").expect("corrupt session");

    let error =
        interactive_session_selection(&store, "active").expect_err("corrupt listing must fail");
    assert!(error.contains("failed to list sessions"));
    assert!(error.contains("parse session JSON"));
}

#[test]
fn session_switcher_renders_preview_and_confirmation_as_bounded_overlays() {
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
        confirming: true,
        exact_id: String::new(),
        error: None,
    };
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");

    terminal
        .draw(|frame| render_session_switcher(frame, frame.area(), &switcher, "current-session"))
        .expect("render switcher");

    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Resume visible-session"), "{rendered}");
    assert!(rendered.contains("Y / Enter resume"), "{rendered}");
    assert!(!rendered.contains("Resume Confirmation"), "{rendered}");
}

#[test]
fn session_list_matches_slash_option_style_without_a_caret() {
    let switcher = SessionSwitcher {
        candidates: vec![
            InteractiveSessionCandidate {
                id: "current-session".to_string(),
                label: "wrap-fix".to_string(),
                preview: "Latest user message: wrap".to_string(),
                is_active: true,
                snapshot_token: [0; 32],
            },
            InteractiveSessionCandidate {
                id: "other-session".to_string(),
                label: "inspect tests".to_string(),
                preview: "Latest user message: inspect".to_string(),
                is_active: false,
                snapshot_token: [1; 32],
            },
        ],
        selected: 1,
        omitted: 0,
        confirming: false,
        exact_id: String::new(),
        error: None,
    };
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| render_session_switcher(frame, frame.area(), &switcher, "current-session"))
        .expect("render session list");
    let rendered = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("wrap-fix"), "{rendered}");
    assert!(rendered.contains("inspect tests"), "{rendered}");
    assert!(rendered.contains("active"), "{rendered}");
    assert!(!rendered.contains(">wrap-fix"), "{rendered}");
    assert!(!rendered.contains("> inspect"), "{rendered}");
    assert!(!rendered.contains("* wrap-fix"), "{rendered}");
}
