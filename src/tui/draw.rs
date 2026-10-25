//! TUI internals split for T043 module size.

use super::*;
use crate::config::keymap::{KeyAction, KeymapPreset};

#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn draw_loop(
    mut terminal: DefaultTerminal,
    project_root: &Path,
    profile_id: String,
    store: SessionStore,
    run_goal: Option<String>,
    mut active_session_id: String,
    mut session_origin: String,
    session_notice: Option<String>,
    mut welcome: StartupWelcome,
) -> io::Result<Option<String>> {
    let agent_profile_scope = TuiAgentProfileScope {
        project_root: project_root.to_path_buf(),
        profile_id: profile_id.clone(),
        sessions_dir: store.sessions_dir().to_path_buf(),
    };
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (recovery_tx, recovery_rx) = mpsc::channel::<crate::interactive::QuestionRecoveryEffect>();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
    let (mut timeline, initial_session) =
        ActiveTimeline::load_with_session(&store, &active_session_id)?;
    if let Some(notice) = session_notice {
        timeline.push_status(notice);
    }
    welcome.consent_directory = pending_workspace_consent(project_root);
    let mut pending_goal = run_goal;
    if let Some(goal) = pending_goal.as_deref() {
        let _ = maybe_assign_session_display_name(&store, &active_session_id, goal);
    }
    let mut worker = None;
    // Permission mode shown when the active run started; a later change via
    // Shift+Tab or /mode applies only to the next request (T080).
    let mut run_started_mode: Option<String> = None;
    timeline.bind_run(None);

    let mut pending_approval: Option<TuiApprovalRequest> = None;
    let mut pending_question: Option<PendingQuestion> = None;
    let mut command_overlay: Option<String> = None;
    let mut pending_model: Option<PendingModelSelection> = None;
    let mut pending_switcher: Option<SessionSwitcher> = None;
    let mut pending_history_search: Option<PendingHistorySearch> = None;
    let mut keymap = crate::config::load_nib_config_full(project_root)
        .map_err(io::Error::other)?
        .tui;
    let mut composer = Composer::default();
    let mut completion = CompletionMenu::default();
    let mut transcript_viewport = TranscriptViewport::default();
    let mut tui_focus = TuiFocus::Composer;
    let mut selected_activity: Option<usize> = None;
    let mut pointer_selection: Option<PointerSelection> = None;
    let mut transcript_view = TranscriptView::default();
    let mut clear_armed_at: Option<Instant> = None;
    let mut quit_arm: Option<QuitArm> = None;
    let spinner_origin = Instant::now();
    let mut chrome_generation: u64 = 0;
    let mut chrome_cache: Option<ChromeCache> = None;
    let mut session_display_cache = SessionDisplayCache::with_session(initial_session);
    let mut presence = crate::session::presence::Presence::new(store.sessions_dir(), project_root);
    let mut exit_requested = false;
    let mut exit_requested_with_active_run = false;

    if welcome.consent_directory.is_none() {
        if let Some(goal) = pending_goal.take() {
            start_tui_conversation(
                goal,
                QuestionConversationContext {
                    scope: &agent_profile_scope,
                    store: &store,
                    session_id: &active_session_id,
                    pending: &mut pending_question,
                    worker: &mut worker,
                    timeline: &mut timeline,
                    approval_tx: &approval_tx,
                    question_tx: &question_tx,
                    stream_tx: &stream_tx,
                    recovery_tx: &recovery_tx,
                },
            )?;
        }
    }

    let loop_result = loop {
        while let Ok(effect) = recovery_rx.try_recv() {
            apply_question_recovery_effect(
                effect,
                QuestionConversationContext {
                    scope: &agent_profile_scope,
                    store: &store,
                    session_id: &active_session_id,
                    pending: &mut pending_question,
                    worker: &mut worker,
                    timeline: &mut timeline,
                    approval_tx: &approval_tx,
                    question_tx: &question_tx,
                    stream_tx: &stream_tx,
                    recovery_tx: &recovery_tx,
                },
            )?;
        }

        drain_stream_events_bounded(&mut stream_rx, &mut timeline, 64);
        let worker_finished = worker.as_ref().is_some_and(TuiAgentWorker::is_finished);
        if let Err(error) = reap_finished_worker(
            &mut worker,
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
            &mut stream_rx,
            &mut timeline,
        ) {
            break Err(error);
        }
        if worker_finished && worker.is_none() {
            if let Ok(Some(session)) = store.load_result(&active_session_id) {
                timeline.refresh_plan_from_session(&session);
            }
        }
        if worker_finished
            && worker.is_none()
            && timeline.reconciled_terminal == Some(InteractionTerminalOutcome::Completed)
        {
            match claim_next_queued_follow_up_after_startup(&store, &active_session_id, |_| {
                prepare_tui_agent_worker(
                    agent_profile_scope.clone(),
                    active_session_id.clone(),
                    approval_tx.clone(),
                    question_tx.clone(),
                    stream_tx.clone(),
                )
                .map_err(|error| error.to_string())
            }) {
                Ok(Some((queued, prepared))) => {
                    match prepared.start(queued.text.clone(), InteractiveAgentMode::Execute) {
                        Ok(next) => {
                            timeline.push_status(format!("[user] {}", queued.text));
                            worker = Some(next);
                            timeline.bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));
                        }
                        Err(error) => {
                            let queue_id = queued.id.clone();
                            let recovery = restore_queued_follow_up_after_start_failure(
                                &store,
                                &active_session_id,
                                queued,
                            );
                            match recovery {
                            Ok(()) => timeline.push_status(format!(
                                "[queue error] queued follow-up {queue_id} could not activate and remains queued: {error}"
                            )),
                            Err(recovery_error) => timeline.push_status(format!(
                                "[queue error] queued follow-up {queue_id} could not activate: {error}; {recovery_error}"
                            )),
                        }
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => timeline.push_status(format!("[queue error] {error}")),
            }
        }
        refresh_pending_interactions(
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
        );

        let interaction_layer = active_interaction_layer(
            pending_approval.is_some(),
            pending_question.is_some(),
            pending_model.is_some(),
            pending_switcher.as_ref(),
            pending_history_search.is_some(),
            completion.is_open(),
        );
        quit_arm = quit_arm_for_consumer(quit_arm, interaction_layer);
        let worker_status = tui_interaction_state(
            pending_approval.is_some(),
            pending_question.is_some(),
            pending_model.is_some(),
            pending_switcher.as_ref(),
            pending_history_search.is_some(),
            completion.is_open(),
            tui_run_state(
                worker.is_some(),
                timeline.live.state.as_deref(),
                timeline.reconciled_terminal,
            ),
        )
        .lifecycle()
        .status_label();
        session_display_cache.refresh(&store, &timeline.session_id, Instant::now());
        let session = session_display_cache.session.as_ref();
        let queued = session
            .map(|session| session.queued_follow_ups.len())
            .unwrap_or(0);
        if let Err(error) = terminal.size() {
            break Err(error);
        }
        let session_revision = session.map(|session| session.revision);
        let mut chrome = if chrome_cache.as_ref().is_some_and(|cache| {
            cache.matches(
                chrome_generation,
                &timeline.session_id,
                session_revision,
                &session_origin,
            )
        }) {
            chrome_cache.as_ref().expect("checked").chrome.clone()
        } else {
            let chrome = format_tui_interaction_chrome(project_root, session, &timeline.session_id)
                .unwrap_or_else(TuiChrome::error);
            chrome_cache = Some(ChromeCache {
                generation: chrome_generation,
                session_id: timeline.session_id.clone(),
                session_revision,
                origin: session_origin.clone(),
                chrome: chrome.clone(),
            });
            chrome
        };
        if worker.is_some() {
            match &run_started_mode {
                Some(started) if *started != chrome.approval => {
                    chrome.approval = format!("{} (next request)", chrome.approval);
                }
                Some(_) => {}
                None => run_started_mode = Some(chrome.approval.clone()),
            }
        } else {
            run_started_mode = None;
        }
        chrome.other_sessions = presence.tick(&timeline.session_id, Instant::now());
        chrome.agent_mode = agent_mode_label(
            if pending_approval.is_some() {
                WaitingKind::Approval
            } else if pending_question.is_some() {
                WaitingKind::Question
            } else if welcome.consent_directory.is_some() {
                WaitingKind::Workspace
            } else {
                WaitingKind::None
            },
            worker.as_ref().map(|worker| worker.mode),
        )
        .to_string();
        chrome.keymap = keymap.clone();
        chrome.agent_mode.push_str(&format!(
            " · {}",
            keymap_context_hint(
                &keymap,
                interaction_layer,
                tui_focus,
                worker.is_some(),
                welcome.consent_directory.is_some(),
            )
        ));
        if selected_activity.is_some_and(|index| index >= timeline.activities.len()) {
            selected_activity = timeline.activities.len().checked_sub(1);
        }
        let waiting = if pending_approval.is_some() {
            WaitingKind::Approval
        } else if pending_question.is_some() {
            WaitingKind::Question
        } else if welcome.consent_directory.is_some() {
            WaitingKind::Workspace
        } else {
            WaitingKind::None
        };
        let meter = if worker.is_some() || waiting != WaitingKind::None {
            Some(WaitingMeter {
                elapsed: timeline
                    .run_started_at
                    .map(|started| started.elapsed())
                    .unwrap_or_default(),
                tokens: timeline.generated_token_estimate(),
                status: meter_status_label(
                    waiting,
                    timeline
                        .live
                        .state
                        .as_deref()
                        .filter(|state| {
                            matches!(
                                *state,
                                "compression" | "compressing" | "cancelling" | "thinking"
                            )
                        })
                        .unwrap_or(worker_status),
                ),
                tick: spinner_origin.elapsed().as_millis(),
            })
        } else {
            None
        };
        let interaction_band = pending_history_search
            .as_ref()
            .map(InteractionBand::History)
            .or_else(|| pending_model.as_ref().map(InteractionBand::Model))
            .or_else(|| {
                pending_switcher
                    .as_ref()
                    .map(|switcher| InteractionBand::Sessions {
                        switcher,
                        active: &active_session_id,
                    })
            });
        transcript_view
            .source_session
            .clone_from(&timeline.session_id);
        transcript_view.source_generation = timeline.render_generation;
        if let Err(error) = terminal.draw(|f| {
            render_session_activities(
                f,
                &chrome,
                &timeline.activities,
                &composer,
                pending_approval.as_ref(),
                pending_question.as_ref(),
                worker.is_some(),
                &mut transcript_viewport,
                tui_focus,
                selected_activity,
                queued,
                Some(&completion).filter(|menu| menu.is_open()),
                meter.as_ref(),
                &welcome,
                interaction_band.as_ref(),
                pointer_selection,
                Some(&mut transcript_view),
            );
            render_interaction_overlay(
                f,
                interaction_layer,
                pending_model.as_ref(),
                pending_switcher.as_ref(),
                pending_history_search.as_ref(),
                &active_session_id,
            );
        }) {
            break Err(error);
        }

        let has_event = match event::poll(std::time::Duration::from_millis(200)) {
            Ok(has_event) => has_event,
            Err(error) => break Err(error),
        };
        if has_event {
            let input = match event::read() {
                Ok(input) => input,
                Err(error) => break Err(error),
            };
            let context = if tui_focus == TuiFocus::Transcript {
                "transcript"
            } else if worker.is_some() {
                "running"
            } else {
                "composer"
            };
            let is_control_q = matches!(&input, Event::Key(key)
                if keymap.resolve(context, key.code, key.modifiers) == Some(KeyAction::Quit));
            if !matches!(&input, Event::Key(key) if key.code == KeyCode::Esc) {
                clear_armed_at = None;
            }
            refresh_pending_interactions(
                &mut pending_approval,
                &mut pending_question,
                &approval_rx,
                &question_rx,
            );
            let interaction_layer = active_interaction_layer(
                pending_approval.is_some(),
                pending_question.is_some(),
                pending_model.is_some(),
                pending_switcher.as_ref(),
                pending_history_search.is_some(),
                completion.is_open(),
            );
            if interaction_layer != InteractionLayer::Composer {
                clear_armed_at = None;
            }
            quit_arm = quit_arm_for_consumer(quit_arm, interaction_layer);
            quit_arm = quit_arm_after_input(quit_arm, is_control_q);
            if let Event::Paste(pasted) = input {
                if welcome.consent_directory.is_some() {
                    continue;
                }
                if let Some(question) = pending_question.as_mut() {
                    paste_question_answer(question, &pasted);
                    continue;
                }
                if matches!(
                    interaction_layer,
                    InteractionLayer::Composer | InteractionLayer::Completion
                ) {
                    let outcome = composer.insert_paste(&pasted);
                    completion.sync_for(&composer.input, Some(project_root));
                    if let Some(status) = outcome.visible_status() {
                        timeline.push_status(status);
                    }
                } else {
                    timeline.push_status(
                        "[composer] paste ignored while a modal response is required".to_string(),
                    );
                }
                continue;
            }
            if let Event::Mouse(mouse) = input {
                if let Some(action) = transcript_action_for_mouse(mouse.kind) {
                    transcript_viewport.apply(action);
                    continue;
                }
                if let Some((row, col)) = transcript_hit(&transcript_view, mouse) {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            tui_focus = TuiFocus::Transcript;
                            pointer_selection = Some(PointerSelection::at(row, col));
                            selected_activity = transcript_view.owners.get(row).copied();
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            tui_focus = TuiFocus::Transcript;
                            if let Some(selection) = pointer_selection.as_mut() {
                                selection.drag_to(row, col);
                            } else {
                                pointer_selection = Some(PointerSelection::at(row, col));
                            }
                            selected_activity = transcript_view.owners.get(row).copied();
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            if let Some(selection) = pointer_selection.as_mut() {
                                selection.drag_to(row, col);
                            }
                            if pointer_selection_is_active(pointer_selection) {
                                if let Some(status) = copy_pointer_selection_and_clear(
                                    &mut pointer_selection,
                                    &mut tui_focus,
                                    &transcript_view,
                                    selected_activity,
                                    &timeline.activities,
                                ) {
                                    timeline.push_status(status.to_string());
                                }
                            } else {
                                tui_focus = TuiFocus::Transcript;
                                pointer_selection = None;
                                selected_activity = transcript_view.owners.get(row).copied();
                            }
                        }
                        _ => {}
                    }
                } else if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    tui_focus = TuiFocus::Composer;
                    pointer_selection = None;
                } else if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
                    && pointer_selection_is_active(pointer_selection)
                {
                    if let Some(status) = copy_pointer_selection_and_clear(
                        &mut pointer_selection,
                        &mut tui_focus,
                        &transcript_view,
                        selected_activity,
                        &timeline.activities,
                    ) {
                        timeline.push_status(status.to_string());
                    }
                }
                continue;
            }
            if let Event::Key(key) = input {
                if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                    continue;
                }
                let context = if tui_focus == TuiFocus::Transcript {
                    "transcript"
                } else if worker.is_some() {
                    "running"
                } else {
                    "composer"
                };
                let key_action = keymap.resolve(context, key.code, key.modifiers);
                let control_c = key_action == Some(KeyAction::Cancel);
                let control_q = key_action == Some(KeyAction::Quit);
                if welcome.consent_directory.is_some() {
                    if let Some(action) = transcript_keymap_action(key_action) {
                        transcript_viewport.apply(action);
                        continue;
                    }
                    match workspace_consent_action_for_key(
                        &mut welcome.consent_selected,
                        key.code,
                        key.modifiers,
                    ) {
                        WorkspaceConsentAction::SelectionChanged => continue,
                        WorkspaceConsentAction::Allow => {
                            match grant_workspace_access_and_release_goal(
                                project_root,
                                &mut welcome.consent_directory,
                                &mut pending_goal,
                            ) {
                                Ok(released_goal) => {
                                    timeline
                                        .push_status("Allowed work in this directory.".to_string());
                                    if let Some(goal) = released_goal {
                                        if let Err(error) = start_tui_conversation(
                                            goal,
                                            QuestionConversationContext {
                                                scope: &agent_profile_scope,
                                                store: &store,
                                                session_id: &active_session_id,
                                                pending: &mut pending_question,
                                                worker: &mut worker,
                                                timeline: &mut timeline,
                                                approval_tx: &approval_tx,
                                                question_tx: &question_tx,
                                                stream_tx: &stream_tx,
                                                recovery_tx: &recovery_tx,
                                            },
                                        ) {
                                            break Err(error);
                                        }
                                    }
                                }
                                Err(error) => {
                                    timeline.push_status(format!("[workspace error] {error}"))
                                }
                            }
                            continue;
                        }
                        WorkspaceConsentAction::Decline => {
                            exit_requested = true;
                            break Ok(());
                        }
                        WorkspaceConsentAction::Unhandled => continue,
                    }
                }
                let interaction_state = tui_interaction_state(
                    pending_approval.is_some(),
                    pending_question.is_some(),
                    pending_model.is_some(),
                    pending_switcher.as_ref(),
                    pending_history_search.is_some(),
                    completion.is_open(),
                    tui_run_state(
                        worker.is_some(),
                        timeline.live.state.as_deref(),
                        timeline.reconciled_terminal,
                    ),
                );
                let global_reduction = if control_c && worker.is_some() {
                    Some(reduce_interaction(
                        &interaction_state,
                        InteractionInput::CancelRun,
                    ))
                } else {
                    None
                };
                let copy_requested = key_action == Some(KeyAction::CopySelection);
                if key_action == Some(KeyAction::Copy) {
                    let output = store
                        .load_result(&active_session_id)
                        .map_err(|e| e.to_string())
                        .and_then(|session| {
                            crate::interactive::latest_assistant_output(session.as_ref())
                        });
                    timeline.push_status(match output {
                        Ok(text) => copy_text_to_clipboard(&text).to_string(),
                        Err(error) => format!("[copy error] {error}"),
                    });
                    continue;
                }
                if copy_requested {
                    if let Some(status) = publish_copied_chat(
                        pointer_selection,
                        &transcript_view,
                        selected_activity,
                        &timeline.activities,
                    ) {
                        timeline.push_status(status.to_string());
                    }
                    continue;
                }
                if global_reduction == Some(InteractionReduction::CancelRun) {
                    if let Err(error) = shutdown_agent_worker(
                        &mut worker,
                        &mut pending_approval,
                        &mut pending_question,
                        &approval_rx,
                        &question_rx,
                        &mut stream_rx,
                        &mut timeline,
                    ) {
                        break Err(error);
                    }
                    match tui_report_cancelled_run(&store, &active_session_id, &mut timeline) {
                        Ok(_) => {}
                        Err(error) => timeline.push_status(format!("[queue error] {error}")),
                    }
                    continue;
                }
                if control_c {
                    if keymap.preset == KeymapPreset::Codex
                        && composer.input.is_empty()
                        && pending_approval.is_none()
                        && pending_question.is_none()
                        && pending_model.is_none()
                        && pending_switcher.is_none()
                        && pending_history_search.is_none()
                    {
                        exit_requested = true;
                        break Ok(());
                    }
                    pointer_selection = None;
                    composer.set_text(String::new());
                    completion.sync_for(&composer.input, Some(project_root));
                    quit_arm = None;
                    if let Some(question) = pending_question.as_mut() {
                        if let Some(editor) = question.state.editor.as_mut() {
                            editor.text.clear();
                        }
                        question.state.error = None;
                    }
                    timeline.push_status("Draft cleared.".to_string());
                    continue;
                }
                if control_q {
                    let now = Instant::now();
                    match quit_confirm_action(quit_arm, now, interaction_layer) {
                        QuitConfirmAction::Confirm => {
                            exit_requested_with_active_run = worker.is_some();
                            exit_requested = true;
                            break Ok(());
                        }
                        QuitConfirmAction::Arm => {
                            quit_arm = Some(QuitArm {
                                armed_at: now,
                                consumer: interaction_layer,
                            });
                            timeline.push_status("Press Ctrl+Q again to quit.".to_string());
                        }
                    }
                    continue;
                }
                if key_action == Some(KeyAction::SelectChat)
                    && !transcript_view.plain_rows.is_empty()
                {
                    let last = transcript_view.plain_rows.len().saturating_sub(1);
                    pointer_selection = Some(PointerSelection {
                        anchor_row: 0,
                        anchor_col: 0,
                        focus_row: last,
                        focus_col: unicode_display_width(&transcript_view.plain_rows[last]),
                        moved: true,
                    });
                    tui_focus = TuiFocus::Transcript;
                    timeline.push_status("Chat selected.".to_string());
                    continue;
                }
                if let Some(action) = transcript_keymap_action(key_action) {
                    transcript_viewport.apply(action);
                    continue;
                }
                let semantic_reduction = (key_action == Some(KeyAction::History)).then(|| {
                    reduce_interaction(&interaction_state, InteractionInput::OpenHistorySearch)
                });
                if let Some(InteractionReduction::OpenHistorySearch { query }) = semantic_reduction
                {
                    pending_history_search =
                        Some(PendingHistorySearch::new(&composer.history, query));
                    continue;
                }
                let interaction_layer = active_interaction_layer(
                    pending_approval.is_some(),
                    pending_question.is_some(),
                    pending_model.is_some(),
                    pending_switcher.as_ref(),
                    pending_history_search.is_some(),
                    completion.is_open(),
                );
                if open_prompt_command_overlay(
                    key.code,
                    pending_approval.is_some() || pending_question.is_some(),
                    &mut command_overlay,
                ) {
                    timeline
                        .push_status("Command (F2): type a slash command · Esc cancel".to_string());
                    continue;
                }
                if let Some(buffer) = command_overlay.as_mut() {
                    match key.code {
                        KeyCode::Esc => {
                            command_overlay = None;
                            timeline.push_status("Command entry cancelled.".to_string());
                        }
                        KeyCode::Backspace => {
                            buffer.pop();
                        }
                        KeyCode::Enter => {
                            let raw = command_overlay.take().unwrap_or_default();
                            let command_line = if raw.trim().starts_with('/') {
                                raw
                            } else {
                                format!("/{}", raw.trim())
                            };
                            match crate::interactive::parse_interactive_command(&command_line) {
                                Ok(Some(command))
                                    if matches!(
                                        crate::interactive::command_effect_class(&command),
                                        crate::interactive::CommandEffectClass::ReadOnlyInspection
                                            | crate::interactive::CommandEffectClass::LiveControl
                                    ) =>
                                {
                                    let copy_command = matches!(
                                        &command,
                                        crate::interactive::InteractiveCommand::Copy
                                    );
                                    match execute_interactive_command_in_state(
                                        command,
                                        project_root,
                                        &profile_id,
                                        &store,
                                        &active_session_id,
                                        worker_status,
                                    ) {
                                        Ok(InteractiveEffect::Output(output)) => timeline
                                            .push_status(if copy_command {
                                                copy_text_to_clipboard(&output).to_string()
                                            } else {
                                                output
                                            }),
                                        Ok(_) => timeline.push_status(
                                            "command completed without changing the pending prompt"
                                                .to_string(),
                                        ),
                                        Err(error) => {
                                            timeline.push_status(format!("[command error] {error}"))
                                        }
                                    }
                                }
                                Ok(Some(crate::interactive::InteractiveCommand::Stop {
                                    task_id: None,
                                })) => timeline.push_status(
                                    crate::interactive::live_stop_requires_id_message().to_string(),
                                ),
                                Ok(Some(command)) => timeline.push_status(format!(
                                    "command /{} is unavailable while a prompt is pending",
                                    command.spec().name
                                )),
                                Ok(None) => {
                                    timeline.push_status("F2 requires a slash command".to_string())
                                }
                                Err(error) => {
                                    timeline.push_status(format!("[command error] {error}"))
                                }
                            }
                        }
                        KeyCode::Char(character) if !character.is_control() => {
                            buffer.push(character);
                        }
                        _ => {}
                    }
                    continue;
                }
                if matches!(
                    interaction_layer,
                    InteractionLayer::Approval | InteractionLayer::Question
                ) {
                    handle_pending_interaction_key_with_modifiers(
                        &mut pending_approval,
                        &mut pending_question,
                        key.code,
                        key.modifiers,
                    );
                    continue;
                }
                if interaction_layer == InteractionLayer::HistorySearch {
                    let Some(search) = pending_history_search.as_mut() else {
                        timeline.push_status(
                            "[ui error] draft history state was unavailable; returned to composer"
                                .to_string(),
                        );
                        continue;
                    };
                    match history_search_action_for_key(
                        search,
                        &composer.history,
                        key.code,
                        key.modifiers,
                    ) {
                        HistorySearchAction::Pending => {}
                        HistorySearchAction::Close => pending_history_search = None,
                        HistorySearchAction::Select(index) => {
                            pending_history_search = None;
                            if composer.select_history_entry(index) {
                                completion.sync_for(&composer.input, Some(project_root));
                            } else {
                                timeline.push_status(
                                    "[history error] selected draft is no longer available"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    continue;
                }
                if interaction_layer == InteractionLayer::Model {
                    let Some(model) = pending_model.as_mut() else {
                        timeline.push_status(
                            "[ui error] model selector state was unavailable; returned to composer"
                                .to_string(),
                        );
                        continue;
                    };
                    match model_action_for_key(model, key.code) {
                        ModelAction::Pending => {}
                        ModelAction::Cancel => {
                            pending_model = None;
                            timeline.push_status("Model selection cancelled.".to_string());
                        }
                        ModelAction::Submit(selected) => {
                            pending_model = None;
                            match set_provider_model(
                                project_root,
                                &selected.provider,
                                &selected.model,
                            ) {
                                Ok(output) => timeline.push_status(output),
                                Err(error) => {
                                    timeline.push_status(format!("[command error] {error}"))
                                }
                            }
                        }
                    }
                    continue;
                }
                if matches!(
                    interaction_layer,
                    InteractionLayer::SessionConfirmation | InteractionLayer::SessionSwitcher
                ) {
                    let Some(switcher) = pending_switcher.as_mut() else {
                        timeline.push_status(
                            "[ui error] session selector state was unavailable; returned to composer"
                                .to_string(),
                        );
                        continue;
                    };
                    match session_switcher_action_for_key(switcher, key.code) {
                        SwitcherAction::Pending => {}
                        SwitcherAction::Close => {
                            pending_switcher = None;
                            timeline.push_status("Session switch cancelled.".to_string());
                        }
                        SwitcherAction::PreviewExact(session_id) => {
                            if let Err(error) = preview_exact_session(
                                &store,
                                switcher,
                                &active_session_id,
                                &session_id,
                            ) {
                                switcher.error = Some(error);
                            }
                        }
                        SwitcherAction::Activate => {
                            match tui_complete_session_switch(
                                &store,
                                switcher,
                                worker.is_some(),
                                &mut active_session_id,
                                &mut timeline,
                            ) {
                                Ok(_) => {
                                    session_origin = "resumed".to_string();
                                    completion = CompletionMenu::default();
                                    pending_switcher = None;
                                    chrome_generation = chrome_generation.saturating_add(1);
                                    transcript_viewport.pin_to_tail();
                                }
                                Err(error) => {
                                    switcher.confirming = false;
                                    switcher.error = Some(error);
                                }
                            }
                        }
                    }
                    continue;
                }
                if interaction_layer == InteractionLayer::RecoverableError {
                    timeline.push_status(
                        "[ui error] interaction state was unavailable; input ignored".to_string(),
                    );
                    continue;
                }
                if interaction_layer == InteractionLayer::Completion {
                    match completion_keymap_action(
                        &mut completion,
                        &mut composer,
                        key_action,
                        key.code,
                        key.modifiers,
                    ) {
                        CompletionKeyResult::Ignored => {}
                        CompletionKeyResult::Consumed => continue,
                        CompletionKeyResult::Submit => {
                            tui_focus = TuiFocus::Composer;
                        }
                    }
                }
                if matches!(
                    interaction_layer,
                    InteractionLayer::Composer | InteractionLayer::Completion
                ) && tui_focus == TuiFocus::Composer
                {
                    match key_action {
                        Some(KeyAction::Start) => {
                            composer.cursor = 0;
                            continue;
                        }
                        Some(KeyAction::End) => {
                            composer.cursor = composer.input.len();
                            continue;
                        }
                        Some(KeyAction::Editor) if worker.is_none() => {
                            match open_draft_editor(&mut terminal, &composer.input) {
                                Ok(Ok(text)) => composer.set_text(text),
                                Err(error) => break Err(error),
                                Ok(Err(error)) => {
                                    timeline.push_status(format!("[editor error] {error}"))
                                }
                            }
                            completion.sync_for(&composer.input, Some(project_root));
                            continue;
                        }
                        Some(KeyAction::ClearView) if worker.is_none() => {
                            clear_visible_transcript(&mut timeline);
                            pointer_selection = None;
                            transcript_viewport.pin_to_tail();
                            continue;
                        }
                        Some(KeyAction::Queue) if worker.is_some() => {
                            let input = composer.input.trim();
                            let queued = if input.starts_with("queue:") {
                                crate::interactive::parse_queue_line(input).unwrap_or("")
                            } else {
                                input
                            };
                            match persist_queued_follow_up(
                                &store,
                                &active_session_id,
                                queued,
                                "composer",
                            ) {
                                Ok(_) => {
                                    composer.remember_submission(&composer.input.clone());
                                    composer.set_text(String::new());
                                    completion = CompletionMenu::default();
                                    timeline.push_status("Queued for the next turn.".to_string());
                                }
                                Err(error) => {
                                    timeline.push_status(format!("[queue error] {error}"))
                                }
                            }
                            continue;
                        }
                        _ => {}
                    }
                }
                // Shift+Tab cycles the session permission mode (T080):
                // ask → accept-edits → plan → ask.
                if key_action == Some(KeyAction::Mode)
                    && matches!(
                        interaction_layer,
                        InteractionLayer::Composer | InteractionLayer::Completion
                    )
                    && !completion.is_open()
                {
                    match crate::interactive::cycle_session_permission_mode(
                        project_root,
                        &store,
                        &active_session_id,
                    ) {
                        Ok(mode) => timeline.push_status(format!(
                            "Permission mode: {} (applies from the next request)",
                            crate::tools::executor::permission_mode_label(mode)
                        )),
                        Err(error) => timeline.push_status(error),
                    }
                    chrome_generation = chrome_generation.saturating_add(1);
                    continue;
                }
                if key_action == Some(KeyAction::Focus)
                    && matches!(
                        interaction_layer,
                        InteractionLayer::Composer | InteractionLayer::Completion
                    )
                {
                    if completion.is_open() {
                        completion.handle_key(&mut composer, KeyCode::Esc);
                    }
                    match tui_focus {
                        TuiFocus::Composer => {
                            tui_focus = TuiFocus::Transcript;
                            selected_activity = timeline.activities.len().checked_sub(1);
                        }
                        TuiFocus::Transcript => {
                            tui_focus = TuiFocus::Composer;
                            pointer_selection = None;
                        }
                    }
                    continue;
                }
                if tui_focus == TuiFocus::Transcript
                    && matches!(
                        interaction_layer,
                        InteractionLayer::Composer | InteractionLayer::Completion
                    )
                {
                    match key.code {
                        KeyCode::Up => {
                            let len = timeline.activities.len();
                            if len > 0 {
                                selected_activity =
                                    Some(selected_activity.unwrap_or(len - 1).saturating_sub(1));
                            }
                            continue;
                        }
                        KeyCode::Down => {
                            let len = timeline.activities.len();
                            if len > 0 {
                                let current = selected_activity.unwrap_or(len - 1);
                                selected_activity = Some((current + 1).min(len - 1));
                            }
                            continue;
                        }
                        KeyCode::Left | KeyCode::Right => {
                            if let Some(index) = selected_activity {
                                if let Some(entry) = timeline.activities.get_mut(index) {
                                    if !entry.body.is_empty() {
                                        entry.folded = !entry.folded;
                                        timeline.render_generation =
                                            timeline.render_generation.wrapping_add(1);
                                    }
                                }
                            }
                            continue;
                        }
                        KeyCode::Enter | KeyCode::Esc => {
                            tui_focus = TuiFocus::Composer;
                            pointer_selection = None;
                            continue;
                        }
                        KeyCode::Char(character)
                            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            tui_focus = TuiFocus::Composer;
                            pointer_selection = None;
                            composer.insert_str(&character.to_string());
                            completion.sync_for(&composer.input, Some(project_root));
                            continue;
                        }
                        _ => {}
                    }
                }
                if key.code == KeyCode::Esc
                    && key_action.is_none()
                    && pointer_selection_is_active(pointer_selection)
                    && matches!(interaction_layer, InteractionLayer::Composer)
                {
                    pointer_selection = None;
                    tui_focus = TuiFocus::Composer;
                    continue;
                }
                if key.code == KeyCode::Esc
                    && key_action.is_none()
                    && tui_focus == TuiFocus::Composer
                    && matches!(interaction_layer, InteractionLayer::Composer)
                {
                    if composer.input.is_empty() {
                        if keymap.preset == KeymapPreset::Codex && worker.is_none() {
                            let now = Instant::now();
                            if clear_armed_at.is_some_and(|armed| {
                                now.duration_since(armed) <= CLEAR_DRAFT_CONFIRM
                            }) {
                                match crate::interactive::fork_before_last_user(
                                    &store,
                                    &active_session_id,
                                ) {
                                    Ok((id, draft)) => {
                                        match replace_active_session(
                                            &store,
                                            id,
                                            "Editing previous message in a new fork.".to_string(),
                                            &mut active_session_id,
                                            &mut timeline,
                                        ) {
                                            Ok(_) => {
                                                composer.set_text(draft);
                                                session_origin = "forked".to_string();
                                                completion
                                                    .sync_for(&composer.input, Some(project_root));
                                                chrome_generation =
                                                    chrome_generation.saturating_add(1);
                                            }
                                            Err(error) => timeline
                                                .push_status(format!("[fork error] {error}")),
                                        }
                                    }
                                    Err(error) => {
                                        timeline.push_status(format!("[fork error] {error}"))
                                    }
                                }
                                clear_armed_at = None;
                            } else {
                                clear_armed_at = Some(now);
                                timeline.push_status(
                                    "Press Esc again to edit the previous message.".to_string(),
                                );
                            }
                        }
                        continue;
                    }
                    let now = Instant::now();
                    if clear_armed_at
                        .is_some_and(|armed| now.duration_since(armed) <= CLEAR_DRAFT_CONFIRM)
                    {
                        composer.set_text(String::new());
                        completion.sync_for(&composer.input, Some(project_root));
                        clear_armed_at = None;
                        timeline.push_status("Draft cleared.".to_string());
                    } else {
                        clear_armed_at = Some(now);
                        timeline.push_status("Press Esc again to clear.".to_string());
                    }
                    continue;
                }
                if key_action == Some(KeyAction::Steer)
                    && !composer.input.trim_start().starts_with('/')
                    && !composer.input.trim_start().starts_with("queue:")
                    && !composer.input.trim_start().starts_with("steer:")
                {
                    match submit_tui_steering_draft(worker.as_ref(), &mut composer) {
                        Ok((text, sequence)) => {
                            completion = CompletionMenu::default();
                            transcript_viewport.on_submission();
                            timeline.push_steering(&text, sequence);
                        }
                        Err(error) => {
                            timeline.push_status(format!("[steer error] {error}"));
                        }
                    }
                    continue;
                }
                match composer_keymap_action(&mut composer, key_action, key.code, key.modifiers) {
                    ComposerAction::Pending => {
                        completion.sync_for(&composer.input, Some(project_root))
                    }
                    ComposerAction::Submit(submitted) => {
                        transcript_viewport.on_submission();
                        completion = CompletionMenu::default();
                        let state = tui_interaction_state(
                            false,
                            false,
                            false,
                            None,
                            false,
                            false,
                            tui_run_state(
                                worker.is_some(),
                                timeline.live.state.as_deref(),
                                timeline.reconciled_terminal,
                            ),
                        );
                        match reduce_interaction(
                            &state,
                            InteractionInput::ComposerSubmit(&submitted),
                        ) {
                            InteractionReduction::NoOp(_) => {}
                            InteractionReduction::Error { message, .. } => {
                                composer.set_text(submitted);
                                completion.sync_for(&composer.input, Some(project_root));
                                timeline.push_status(format!("[command error] {message}"));
                            }
                            InteractionReduction::SteerCurrent(text) => {
                                composer.set_text(text);
                                match submit_tui_steering_draft(worker.as_ref(), &mut composer) {
                                    Ok((text, sequence)) => timeline.push_steering(&text, sequence),
                                    Err(error) => {
                                        composer.set_text(submitted);
                                        timeline.push_status(format!("[steer error] {error}"));
                                    }
                                }
                                completion.sync_for(&composer.input, Some(project_root));
                            }
                            InteractionReduction::QueueNext(queued) => {
                                match persist_queued_follow_up(
                                    &store,
                                    &active_session_id,
                                    &queued,
                                    "composer",
                                ) {
                                    Ok(_) => timeline.push_status(format!(
                                        "queued follow-up retained on session {active_session_id}"
                                    )),
                                    Err(error) => {
                                        composer.set_text(submitted);
                                        completion.sync_for(&composer.input, Some(project_root));
                                        timeline.push_status(format!("[queue error] {error}"));
                                    }
                                }
                            }
                            InteractionReduction::OpenHistorySearch { query } => {
                                composer.history.discard_latest_if(&submitted);
                                pending_history_search =
                                    Some(PendingHistorySearch::new(&composer.history, query));
                            }
                            InteractionReduction::Command(command) => {
                                let copy_command = matches!(
                                    &command,
                                    crate::interactive::InteractiveCommand::Copy
                                );
                                let changed_origin = match &command {
                                    crate::interactive::InteractiveCommand::New
                                    | crate::interactive::InteractiveCommand::Clear => Some("new"),
                                    crate::interactive::InteractiveCommand::Fork => Some("fork"),
                                    _ => None,
                                };
                                if matches!(
                                    command,
                                    crate::interactive::InteractiveCommand::Session
                                        | crate::interactive::InteractiveCommand::Resume
                                ) {
                                    match load_session_switcher(&store, &active_session_id) {
                                        Ok(switcher) => pending_switcher = Some(switcher),
                                        Err(error) => {
                                            composer.set_text(submitted);
                                            completion
                                                .sync_for(&composer.input, Some(project_root));
                                            timeline.push_status(format!(
                                                "[command error] could not open session switcher: {error}"
                                            ));
                                        }
                                    }
                                    continue;
                                }
                                match execute_interactive_command_in_state(
                                    command,
                                    project_root,
                                    &profile_id,
                                    &store,
                                    &active_session_id,
                                    worker_status,
                                ) {
                                    Ok(InteractiveEffect::Quit) => {
                                        exit_requested_with_active_run = worker.is_some();
                                        exit_requested = true;
                                        break Ok(());
                                    }
                                    Ok(InteractiveEffect::Output(output)) => {
                                        if let Ok(config) =
                                            crate::config::load_nib_config_full(project_root)
                                        {
                                            keymap = config.tui;
                                            welcome.keymap = keymap.clone();
                                        }
                                        chrome_generation = chrome_generation.saturating_add(1);
                                        timeline.push_status(if copy_command {
                                            copy_text_to_clipboard(&output).to_string()
                                        } else {
                                            output
                                        })
                                    }
                                    Ok(InteractiveEffect::SessionChanged {
                                        session_id,
                                        output,
                                    }) => {
                                        if let Err(error) = replace_active_session(
                                            &store,
                                            session_id,
                                            output,
                                            &mut active_session_id,
                                            &mut timeline,
                                        ) {
                                            timeline.push_status(format!(
                                            "[command error] {error}; the active session is unchanged"
                                            ));
                                        } else {
                                            if let Some(origin) = changed_origin {
                                                session_origin = origin.to_string();
                                            }
                                            chrome_generation = chrome_generation.saturating_add(1);
                                            transcript_viewport.pin_to_tail();
                                        }
                                    }
                                    Ok(InteractiveEffect::SelectSession(selection)) => {
                                        pending_switcher = Some(SessionSwitcher::from_selection(
                                            selection,
                                            &active_session_id,
                                        ));
                                    }
                                    Ok(InteractiveEffect::SelectModel(selection)) => {
                                        pending_model = Some(PendingModelSelection::new(selection));
                                    }
                                    Ok(InteractiveEffect::Compact) => {
                                        timeline.push_status("[compact] requested".to_string());
                                        worker = Some(spawn_tui_agent_worker(
                                            agent_profile_scope.clone(),
                                            active_session_id.clone(),
                                            String::new(),
                                            InteractiveAgentMode::Compact,
                                            approval_tx.clone(),
                                            question_tx.clone(),
                                            stream_tx.clone(),
                                        )?);
                                        timeline.bind_run(
                                            worker.as_ref().map(|worker| worker.run_id.clone()),
                                        );
                                    }
                                    Ok(InteractiveEffect::ContinuePlan { plan_id, goal }) => {
                                        timeline.push_status(format!("Continue plan {plan_id}"));
                                        worker = Some(
                                            prepare_tui_agent_worker(
                                                agent_profile_scope.clone(),
                                                active_session_id.clone(),
                                                approval_tx.clone(),
                                                question_tx.clone(),
                                                stream_tx.clone(),
                                            )?
                                            .start_with_continuation(
                                                goal,
                                                InteractiveAgentMode::Execute,
                                                Some(plan_id),
                                            )?,
                                        );
                                        timeline.bind_run(
                                            worker.as_ref().map(|worker| worker.run_id.clone()),
                                        );
                                    }
                                    Ok(InteractiveEffect::OpenQuestion {
                                        invocation_id,
                                        question,
                                        proposed_answer,
                                        options,
                                    }) => {
                                        let (reply_tx, reply_rx) = oneshot::channel();
                                        drop(reply_rx);
                                        pending_question = Some(PendingQuestion::recovered(
                                            TuiQuestionRequest::single(
                                                question,
                                                proposed_answer,
                                                options,
                                                reply_tx,
                                            ),
                                            RecoveredQuestionTarget {
                                                store: store.clone(),
                                                session_id: active_session_id.clone(),
                                                invocation_id: serde_json::from_value(
                                                    serde_json::Value::String(invocation_id),
                                                )
                                                .map_err(io::Error::other)?,
                                                completion: recovery_tx.clone(),
                                            },
                                        ));
                                    }
                                    Ok(InteractiveEffect::RunAgent { goal, mode }) => {
                                        if mode != InteractiveAgentMode::Compact {
                                            assign_session_title_from_goal(
                                                &store,
                                                &active_session_id,
                                                &goal,
                                                &mut chrome_generation,
                                            );
                                        }
                                        timeline.push_status(format!("[user] {goal}"));
                                        worker = Some(spawn_tui_agent_worker(
                                            agent_profile_scope.clone(),
                                            active_session_id.clone(),
                                            goal,
                                            mode,
                                            approval_tx.clone(),
                                            question_tx.clone(),
                                            stream_tx.clone(),
                                        )?);
                                        timeline.bind_run(
                                            worker.as_ref().map(|worker| worker.run_id.clone()),
                                        );
                                    }
                                    Err(error) => {
                                        composer.set_text(submitted);
                                        completion.sync_for(&composer.input, Some(project_root));
                                        timeline.push_status(format!("[command error] {error}"))
                                    }
                                }
                            }
                            InteractionReduction::IdleTurn(goal) => {
                                assign_session_title_from_goal(
                                    &store,
                                    &active_session_id,
                                    &goal,
                                    &mut chrome_generation,
                                );
                                timeline.push_status(format!("[user] {goal}"));
                                if let Err(error) = start_tui_conversation(
                                    goal.clone(),
                                    QuestionConversationContext {
                                        scope: &agent_profile_scope,
                                        store: &store,
                                        session_id: &active_session_id,
                                        pending: &mut pending_question,
                                        worker: &mut worker,
                                        timeline: &mut timeline,
                                        approval_tx: &approval_tx,
                                        question_tx: &question_tx,
                                        stream_tx: &stream_tx,
                                        recovery_tx: &recovery_tx,
                                    },
                                ) {
                                    composer.set_text(goal);
                                    timeline.push_status(format!("[recovery error] {error}"));
                                }
                            }
                            InteractionReduction::Consumed(_)
                            | InteractionReduction::ModalCommand(_)
                            | InteractionReduction::ApprovalDecision(_)
                            | InteractionReduction::ApprovalInputClosed
                            | InteractionReduction::QuestionAnswered(_)
                            | InteractionReduction::QuestionLeftUnanswered
                            | InteractionReduction::QuestionInputClosed
                            | InteractionReduction::ConfirmationDecision(_)
                            | InteractionReduction::ConfirmationInputClosed
                            | InteractionReduction::Reconciled { .. }
                            | InteractionReduction::Transcript(_)
                            | InteractionReduction::CancelRun
                            | InteractionReduction::Quit
                            | InteractionReduction::StaleEvent => {
                                timeline.push_status(
                                    "[ui error] submitted input had no valid consumer".to_string(),
                                );
                            }
                        }
                    }
                }
            }
        }
    };
    let shutdown = shutdown_agent_worker(
        &mut worker,
        &mut pending_approval,
        &mut pending_question,
        &approval_rx,
        &question_rx,
        &mut stream_rx,
        &mut timeline,
    );
    loop_result.and(shutdown).map(|()| {
        if !exit_requested {
            return None;
        }
        let notice = if exit_requested_with_active_run {
            tui_report_quit_run(&store, &active_session_id, &mut timeline)
                .unwrap_or_else(|error| error)
        } else {
            tui_exit_disposition(&store, &active_session_id).unwrap_or_else(|error| error)
        };
        Some(notice)
    })
}

pub(crate) fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    r: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
