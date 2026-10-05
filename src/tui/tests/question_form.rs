use super::*;

fn form_request(reply: oneshot::Sender<crate::interactive::QuestionFormOutcome>) -> TuiQuestionRequest {
    let mut request = TuiQuestionRequest::single("Are prices final?".to_string(), None, vec!["Final".to_string(), "Review".to_string()], reply);
    request.form.header = Some("Pricing decisions".to_string());
    request.form.questions[0].title = Some("Prices/VAT".to_string());
    request.form.questions[0].options[0].description = Some("Keep the displayed prices and include VAT.".to_string());
    let mut second = request.form.questions[0].clone();
    second.title = Some("Interval".to_string());
    second.question = "Which interval?".to_string();
    request.form.questions.push(second);
    request
}

#[test]
fn form_tabs_keep_drafts_and_send_only_from_submit() {
    let (reply, mut response) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(form_request(reply)));
    assert!(!handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(pending.as_ref().unwrap().state.tab, 1);
    assert!(!handle_question_key(&mut pending, KeyCode::Char('2')));
    assert!(!handle_question_key(&mut pending, KeyCode::Enter));
    assert!(pending.as_ref().unwrap().state.is_submit());
    assert!(response.try_recv().is_err());
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    let crate::interactive::QuestionFormOutcome::Answered(answers) = response.try_recv().unwrap() else { panic!("answers") };
    assert_eq!(answers.iter().map(|answer| answer.answer.as_str()).collect::<Vec<_>>(), ["Final", "Review"]);
}

#[test]
fn form_card_renders_header_tabs_description_and_visible_editor() {
    let (reply, _response) = oneshot::channel();
    let mut pending = PendingQuestion::new(form_request(reply));
    let backend = TestBackend::new(80, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render_current_session_view(frame, "workspace", "mock", "you  context remains", &Composer::default(), None, Some(&pending))).unwrap();
    let rendered = buffer_text(terminal.backend().buffer());
    for expected in ["Pricing decisions", "Are prices final?", "☐ Prices/VAT", "☐ Interval", "☐ Submit", "› 1. Final", "Keep the displayed", "Type something.", "Chat about this"] { assert!(rendered.contains(expected), "missing {expected}: {rendered}"); }
    question_action_for_key(&mut pending, KeyCode::Char('3'));
    question_action_for_key(&mut pending, KeyCode::Enter);
    for character in "custom 1Y".chars() { question_action_for_key(&mut pending, KeyCode::Char(character)); }
    terminal.draw(|frame| render_current_session_view(frame, "workspace", "mock", "you  context remains", &Composer::default(), None, Some(&pending))).unwrap();
    let rendered = buffer_text(terminal.backend().buffer());
    assert!(rendered.contains("Your answer: custom 1Y"), "{rendered}");
    assert!(rendered.contains("Are prices final?"), "{rendered}");
}

#[test]
fn form_discussion_returns_message_and_escape_interrupts_either_editor() {
    for discussed in [false, true] {
        let (reply, mut response) = oneshot::channel();
        let mut pending = Some(PendingQuestion::new(form_request(reply)));
        handle_question_key(&mut pending, KeyCode::Char(if discussed { '4' } else { '3' }));
        handle_question_key(&mut pending, KeyCode::Enter);
        handle_question_key(&mut pending, KeyCode::Char('Y'));
        assert!(handle_question_key(&mut pending, KeyCode::Esc));
        assert_eq!(response.try_recv().unwrap(), crate::interactive::QuestionFormOutcome::LeftUnanswered);
    }
    let (reply, mut response) = oneshot::channel();
    let mut pending = Some(PendingQuestion::new(form_request(reply)));
    handle_question_key(&mut pending, KeyCode::Enter);
    handle_question_key(&mut pending, KeyCode::Char('4'));
    handle_question_key(&mut pending, KeyCode::Enter);
    for character in "Why these prices?".chars() { handle_question_key(&mut pending, KeyCode::Char(character)); }
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    assert_eq!(response.try_recv().unwrap(), crate::interactive::QuestionFormOutcome::Discussed("Why these prices?".to_string()));
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    buffer.content.iter().map(|cell| cell.symbol()).collect()
}

#[test]
fn proposed_form_rows_hide_model_options_and_preserve_exact_sources() {
    let (reply, mut response) = oneshot::channel();
    let mut request = form_request(reply);
    request.form.questions[1].proposed_answer = Some("Exact monthly proposal".to_string());
    let mut pending = Some(PendingQuestion::new(request));
    handle_question_key(&mut pending, KeyCode::Enter);
    let question = pending.as_ref().unwrap();
    let rows = question_form_lines(question, 80).into_iter().map(|(line, _)| line).collect::<Vec<_>>().join("\n");
    for expected in ["› 1. Approve proposed answer", "2. Reject and leave unanswered", "3. Instruct otherwise", "4. Chat about this"] { assert!(rows.contains(expected), "{rows}"); }
    assert!(!rows.contains("Final"));
    assert!(!rows.contains("Type something."));
    assert!(!handle_question_key(&mut pending, KeyCode::Enter));
    assert!(handle_question_key(&mut pending, KeyCode::Enter));
    let crate::interactive::QuestionFormOutcome::Answered(answers) = response.try_recv().unwrap() else { panic!("submitted answers") };
    assert_eq!(answers[1].answer, "Exact monthly proposal");
    assert_eq!(answers[1].source, crate::interactive::QuestionAnswerSource::ApprovedProposal);
}

#[test]
fn narrow_tab_strip_shows_overflow_and_keeps_the_focused_submit_visible() {
    let (reply, _response) = oneshot::channel();
    let mut question = PendingQuestion::new(form_request(reply));
    question.state.focus_tab(2);
    let line = question_tab_line(&question.state, 18);
    let text = line.spans.iter().map(|span| span.content.as_ref()).collect::<String>();
    assert!(text.contains('←') && text.contains('→'), "{text}");
    assert!(text.contains("☐ Submit"), "{text}");
    assert!(line.spans.iter().any(|span| span.content.contains("Submit") && span.style != Style::default()));
}

#[test]
fn tui_question_escape_stops_worker_with_waiting_outcome_without_global_cancel() {
    let directory = tempdir().unwrap();
    let mut config = NibConfig { llm: mock_config(), ..Default::default() };
    config.agent.answer_only = false;
    save_nib_config_full(directory.path(), &mut config).unwrap();
    let store = SessionStore::for_project(directory.path()).unwrap();
    let goal = "ask a question";
    let mut session = store.create_session();
    session.plan = Some(crate::session::Plan::new(goal, vec![crate::session::PlanStep {
        description: goal.to_string(), status: "Pending".to_string(), outcome: None,
        attempts: 0, updated_at: None, verification_obligations: Vec::new(), content_generation: 0,
    }]));
    store.save(&mut session).unwrap();
    let (approval_tx, _approval_rx) = mpsc::channel();
    let (question_tx, question_rx) = mpsc::channel();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(100);
    let mut worker = spawn_tui_agent_worker(TuiAgentProfileScope {
        project_root: directory.path().to_path_buf(), profile_id: "default".to_string(), sessions_dir: store.sessions_dir().to_path_buf(),
    }, session.id.clone(), goal.to_string(), InteractiveAgentMode::Execute, approval_tx, question_tx, stream_tx).unwrap();
    let request = question_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let mut pending = Some(PendingQuestion::new(request));
    assert!(handle_question_key(&mut pending, KeyCode::Esc));
    assert!(!worker.cancellation.is_cancelled());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !worker.is_finished() && Instant::now() < deadline {
        while stream_rx.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(worker.is_finished(), "Esc must stop the active worker");
    worker.join().unwrap();
    let persisted = store.load(&session.id).unwrap();
    assert_eq!(persisted.plan.as_ref().unwrap().outcome.as_deref(), Some("waiting_for_user_input"));
    assert!(persisted.clarifications.iter().all(|record| record.answer.is_none()));
    assert!(persisted.events.iter().any(|event| event.kind == "run_terminal" && event.details["outcome"] == "waiting_for_user_input"));
}

#[test]
fn startup_conversation_reopens_recovered_form_before_spawning_worker() {
    let (directory, store, session_id, invocation_id) = recoverable_question_session();
    let scope = TuiAgentProfileScope { project_root: directory.path().to_path_buf(), profile_id: "default".to_string(), sessions_dir: store.sessions_dir().to_path_buf() };
    let (approval_tx, _approval_rx) = mpsc::channel();
    let (question_tx, _question_rx) = mpsc::channel();
    let (stream_tx, _stream_rx) = tokio::sync::mpsc::channel(100);
    let (recovery_tx, _recovery_rx) = mpsc::channel();
    let mut pending = None;
    let mut worker = None;
    let mut timeline = ActiveTimeline::load(&store, &session_id).unwrap();
    start_tui_conversation("resume".to_string(), &scope, &store, &session_id, &mut pending, &mut worker, &mut timeline, &approval_tx, &question_tx, &stream_tx, &recovery_tx).unwrap();
    assert!(worker.is_none());
    assert_eq!(pending.as_ref().unwrap().recovery.as_ref().unwrap().invocation_id, invocation_id);
    assert!(store.load(&session_id).unwrap().clarifications[0].answer.is_none());
}

#[test]
fn long_description_scroll_keeps_question_and_selected_label_visible() {
    let (reply, _response) = oneshot::channel();
    let mut request = form_request(reply);
    request.form.questions.truncate(1);
    request.form.questions[0].options[0].description = Some((0..40).map(|index| format!("Description line {index}\n")).collect());
    let mut pending = PendingQuestion::new(request);
    let mut terminal = Terminal::new(TestBackend::new(50, 24)).unwrap();
    for _ in 0..12 { question_action_for_key(&mut pending, KeyCode::PageDown); }
    terminal.draw(|frame| render_current_session_view(frame, "workspace", "mock", "you  context", &Composer::default(), None, Some(&pending))).unwrap();
    let rendered = buffer_text(terminal.backend().buffer());
    for expected in ["Are prices final?", "› 1. Final", "Description line 39"] { assert!(rendered.contains(expected), "{rendered}"); }
}
