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
