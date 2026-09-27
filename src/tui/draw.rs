//! TUI internals split for T043 module size.

use super::*;

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
    let mut worker = if welcome.consent_directory.is_none() {
        if let Some(goal) = pending_goal.take() {
            Some(spawn_tui_agent_worker(
                agent_profile_scope.clone(),
                active_session_id.clone(),
                goal,
                InteractiveAgentMode::Execute,
                approval_tx.clone(),
                question_tx.clone(),
                stream_tx.clone(),
            )?)
        } else {
            None
        }
    } else {
        None
    };
    timeline.bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));

    let mut pending_approval: Option<TuiApprovalRequest> = None;
    let mut pending_question: Option<PendingQuestion> = None;
    let mut command_overlay: Option<String> = None;
    let mut pending_model: Option<PendingModelSelection> = None;
    let mut pending_switcher: Option<SessionSwitcher> = None;
    let mut pending_history_search: Option<PendingHistorySearch> = None;
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
    let mut exit_requested = false;
    let mut exit_requested_with_active_run = false;

    let loop_result = loop {
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
                job: live_job_label(&timeline.activities, timeline.live.state.as_deref()),
                step: plan_step_label(session),
                elapsed: timeline
                    .run_started_at
                    .map(|started| started.elapsed())
                    .unwrap_or_default(),
                tokens: session_display_cache.token_label.clone(),
                status: meter_status_label(waiting, worker_status),
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
            let is_control_q = matches!(
                &input,
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
            );
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
                let control_c = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SHIFT);
                let control_shift_c = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT);
                let control_q = matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                if welcome.consent_directory.is_some() {
                    if let Some(action) = transcript_action_for_key(key.code, key.modifiers) {
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
                                        match spawn_tui_agent_worker(
                                            agent_profile_scope.clone(),
                                            active_session_id.clone(),
                                            goal,
                                            InteractiveAgentMode::Execute,
                                            approval_tx.clone(),
                                            question_tx.clone(),
                                            stream_tx.clone(),
                                        ) {
                                            Ok(next) => {
                                                timeline.bind_run(Some(next.run_id.clone()));
                                                worker = Some(next);
                                            }
                                            Err(error) => break Err(error),
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
                let copy_requested = (matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
                    && key.modifiers.contains(KeyModifiers::CONTROL))
                    || control_shift_c;
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
                    pointer_selection = None;
                    composer.set_text(String::new());
                    completion.sync_for(&composer.input, Some(project_root));
                    quit_arm = None;
                    if let Some(question) = pending_question.as_mut() {
                        question.response.clear();
                        question.error = None;
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
                if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
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
                if let Some(action) = transcript_action_for_key(key.code, key.modifiers) {
                    transcript_viewport.apply(action);
                    continue;
                }
                let semantic_reduction = match key.code {
                    KeyCode::Char('r') | KeyCode::Char('R')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        Some(reduce_interaction(
                            &interaction_state,
                            InteractionInput::OpenHistorySearch,
                        ))
                    }
                    _ => None,
                };
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
                    handle_pending_interaction_key(
                        &mut pending_approval,
                        &mut pending_question,
                        key.code,
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
                            match set_active_model(project_root, &selected) {
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
                    match completion.handle_key(&mut composer, key.code) {
                        CompletionKeyResult::Ignored => {
                            match composer_action_for_key(&mut composer, key.code, key.modifiers) {
                                ComposerAction::Pending => {
                                    completion.sync_for(&composer.input, Some(project_root));
                                }
                                ComposerAction::Submit(submitted) => {
                                    composer.set_text(submitted);
                                    timeline.push_status(
                                        "[ui error] completion input could not be submitted"
                                            .to_string(),
                                    );
                                }
                            }
                            continue;
                        }
                        CompletionKeyResult::Consumed => continue,
                        CompletionKeyResult::Submit => {
                            tui_focus = TuiFocus::Composer;
                        }
                    }
                }
                if key.code == KeyCode::Tab
                    && !key.modifiers.contains(KeyModifiers::SHIFT)
                    && matches!(
                        interaction_layer,
                        InteractionLayer::Composer | InteractionLayer::Completion
                    )
                    && !completion.is_open()
                {
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
                    && pointer_selection_is_active(pointer_selection)
                    && matches!(interaction_layer, InteractionLayer::Composer)
                {
                    pointer_selection = None;
                    tui_focus = TuiFocus::Composer;
                    continue;
                }
                if key.code == KeyCode::Esc
                    && tui_focus == TuiFocus::Composer
                    && matches!(interaction_layer, InteractionLayer::Composer)
                {
                    if composer.input.is_empty() {
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
                if matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
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
                match composer_action_for_key(&mut composer, key.code, key.modifiers) {
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
                                composer.set_text(submitted);
                                completion.sync_for(&composer.input, Some(project_root));
                                timeline.push_status(format!(
                                    "[ui error] Enter cannot steer an active run: {text}"
                                ));
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
                                            TuiQuestionRequest {
                                                question,
                                                proposed_answer,
                                                options,
                                                reply: reply_tx,
                                            },
                                            RecoveredQuestionTarget {
                                                store: store.clone(),
                                                session_id: active_session_id.clone(),
                                                invocation_id: invocation_id.clone(),
                                            },
                                        ));
                                        timeline.push_status(format!(
                                            "Answer recovered question {invocation_id}"
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
                                worker = Some(spawn_tui_agent_worker(
                                    agent_profile_scope.clone(),
                                    active_session_id.clone(),
                                    goal,
                                    InteractiveAgentMode::Execute,
                                    approval_tx.clone(),
                                    question_tx.clone(),
                                    stream_tx.clone(),
                                )?);
                                timeline
                                    .bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));
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
