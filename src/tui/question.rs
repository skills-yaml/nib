//! Thin terminal rendering for the shared question form state.

use super::*;

pub(crate) fn question_form_reserved_rows(question: &PendingQuestion) -> usize {
    question_form_lines(question, 40).len().saturating_add(1)
}

pub(crate) fn question_tab_line(state: &crate::interactive::QuestionFormState, width: u16) -> Line<'static> {
    if state.form.questions.len() < 2 { return Line::default(); }
    let mut tabs = state.form.questions.iter().enumerate().map(|(index, question)| {
        let mark = if state.drafts[index].is_some() { "✔" } else { "☐" };
        format!("{mark} {}", question.title.as_deref().unwrap_or("Question"))
    }).collect::<Vec<_>>();
    tabs.push(format!("{} Submit", if state.complete() { "✔" } else { "☐" }));
    let full_width = tabs.iter().map(|tab| unicode_display_width(tab) + 2).sum::<usize>();
    let overflow = full_width > usize::from(width);
    let mut start = 0;
    if overflow {
        let available = usize::from(width).saturating_sub(4);
        while start < state.tab && tabs[start..=state.tab].iter().map(|tab| unicode_display_width(tab) + 2).sum::<usize>() > available { start += 1; }
    }
    let mut spans = Vec::new();
    let mut remaining = usize::from(width);
    if overflow { spans.push(Span::raw("← ")); remaining = remaining.saturating_sub(4); }
    for (index, label) in tabs.iter().enumerate().skip(start) {
        let cost = unicode_display_width(label) + 2;
        if cost > remaining && index != state.tab { break; }
        let rendered = truncate_display_cells(label, remaining.saturating_sub(2));
        spans.push(Span::styled(format!("{rendered}  "), if index == state.tab { selected_option_style(std::env::var_os("NO_COLOR").is_some()) } else { Style::default() }));
        remaining = remaining.saturating_sub(cost);
    }
    if overflow { spans.push(Span::raw("→")); }
    Line::from(spans)
}

pub(crate) fn question_form_lines(question: &PendingQuestion, width: u16) -> Vec<(String, bool)> {
    let state = &question.state;
    if state.is_submit() {
        let mut lines = Vec::new();
        for (index, question) in state.form.questions.iter().enumerate() {
            let answer = state.drafts[index].as_ref().map(|answer| answer.answer.as_str()).unwrap_or("Unanswered");
            lines.extend(wrapped_display_rows(&format!("{}: {answer}", question.title.as_deref().unwrap_or("Question")), width).into_iter().map(|line| (line, false)));
        }
        lines.push(("› Submit these answers".to_string(), true));
        return lines;
    }
    let Some(current) = state.current_question() else { return Vec::new(); };
    let choices = if current.proposed_answer.is_some() {
        vec![("Approve proposed answer", None), ("Reject and leave unanswered", None), ("Instruct otherwise", None)]
    } else {
        let mut choices = current.options.iter().map(|option| (option.label.as_str(), option.description.as_deref())).collect::<Vec<_>>();
        choices.push(("Type something.", None));
        choices
    };
    let mut lines = Vec::new();
    for (index, (label, description)) in choices.iter().enumerate() {
        let selected = state.selected_row == index && state.editor.is_none();
        let marker = if selected { "› " } else { "  " };
        let prefix = format!("{marker}{}. ", index + 1);
        let indent_width = unicode_display_width(&prefix);
        let label_rows = wrapped_display_rows(label, width.saturating_sub(indent_width as u16).max(1));
        for (offset, line) in label_rows.into_iter().enumerate() {
            lines.push((format!("{}{line}", if offset == 0 { prefix.clone() } else { " ".repeat(indent_width) }), selected && offset == 0));
        }
        if let Some(description) = description {
            let indent = " ".repeat(format!("  {}. ", index + 1).len());
            lines.extend(wrapped_display_rows(description, width.saturating_sub(indent.len() as u16).max(1)).into_iter().map(|line| (format!("{indent}{line}"), false)));
        }
    }
    lines.push(("─".repeat(usize::from(width)), false));
    let selected = state.selected_row == choices.len() && state.editor.is_none();
    lines.push((format!("{}{}. Chat about this", if selected { "› " } else { "  " }, choices.len() + 1), selected));
    lines
}

pub(crate) fn render_question_band(frame: &mut ratatui::Frame<'_>, area: Rect, question: &PendingQuestion) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 { return; }
    let tabs = u16::from(question.state.form.questions.len() > 1);
    if tabs > 0 {
        frame.render_widget(Paragraph::new(question_tab_line(&question.state, inner.width)), Rect { height: 1, ..inner });
    }
    let rows = question_form_lines(question, inner.width);
    let capacity = usize::from(inner.height.saturating_sub(tabs));
    let selected = rows.iter().position(|(_, selected)| *selected).unwrap_or(0);
    let selected_end = rows.iter().enumerate().skip(selected + 1)
        .find(|(_, (line, _))| line.starts_with("  ") && line.trim_start().chars().next().is_some_and(|character| character.is_ascii_digit()) || line.starts_with('─'))
        .map_or(rows.len(), |(index, _)| index);
    let start = selected_end.saturating_sub(capacity).min(selected)
        .min(rows.len().saturating_sub(capacity));
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let lines = rows.into_iter().skip(start).take(capacity).map(|(line, selected)| Line::from(Span::styled(line, if selected { selected_option_style(no_color) } else { Style::default() }))).collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), Rect { y: inner.y + tabs, height: inner.height.saturating_sub(tabs), ..inner });
}

pub(crate) fn question_composer_rows(question: &PendingQuestion, width: u16) -> (Vec<String>, bool) {
    let state = &question.state;
    let mut text = String::new();
    if let Some(header) = &state.form.header { text.push_str(header); text.push('\n'); }
    if let Some(current) = state.current_question() {
        text.push_str(&current.question);
        if let Some(proposal) = &current.proposed_answer { text.push_str(&format!("\nProposed answer: {proposal}")); }
    } else { text.push_str("Submit all question answers"); }
    if let Some(editor) = &state.editor {
        let name = if editor.kind == crate::interactive::QuestionEditorKind::Discussion { "Chat about this" } else { "Your answer" };
        text.push_str(&format!("\n{name}: {}", editor.text));
    }
    if let Some(error) = &state.error { text.push_str(&format!("\nInput error: {error}")); }
    (wrapped_display_rows(&format!("> {text}"), width.max(1)), state.editor.is_some())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_question_recovery_effect(
    effect: crate::interactive::QuestionRecoveryEffect,
    scope: &TuiAgentProfileScope,
    store: &SessionStore,
    session_id: &str,
    pending: &mut Option<PendingQuestion>,
    worker: &mut Option<TuiAgentWorker>,
    timeline: &mut ActiveTimeline,
    approval_tx: &mpsc::Sender<TuiApprovalRequest>,
    question_tx: &mpsc::Sender<TuiQuestionRequest>,
    stream_tx: &tokio::sync::mpsc::Sender<SessionStreamEvent>,
    recovery_tx: &mpsc::Sender<crate::interactive::QuestionRecoveryEffect>,
) -> io::Result<()> {
    use crate::interactive::QuestionRecoveryEffect as Effect;
    match effect {
        Effect::Output(output) => timeline.push_status(output),
        Effect::OpenForm(recovery) => {
            let (reply, response) = oneshot::channel();
            drop(response);
            *pending = Some(PendingQuestion::recovered(
                TuiQuestionRequest { form: recovery.form, initial_answers: recovery.initial_answers, reply },
                RecoveredQuestionTarget { store: store.clone(), session_id: session_id.to_string(), invocation_id: recovery.invocation_id, completion: recovery_tx.clone() },
            ));
        }
        Effect::OpenEditor { form, question_index } => {
            apply_question_recovery_effect(Effect::OpenForm(form), scope, store, session_id, pending, worker, timeline, approval_tx, question_tx, stream_tx, recovery_tx)?;
            if let Some(question) = pending.as_mut() {
                question.state.focus_tab(question_index.unwrap_or(0));
                question.state.open_editor(if question_index.is_some() { crate::interactive::QuestionEditorKind::Answer } else { crate::interactive::QuestionEditorKind::Discussion });
            }
        }
        Effect::ContinuePlan { plan_id, goal } => {
            *worker = Some(prepare_tui_agent_worker(scope.clone(), session_id.to_string(), approval_tx.clone(), question_tx.clone(), stream_tx.clone())?
                .start_with_continuation(goal, InteractiveAgentMode::Execute, Some(plan_id))?);
            timeline.bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));
        }
        Effect::ContinueDiscussion { plan_id, goal, invocation_id } => {
            *worker = Some(prepare_tui_agent_worker(scope.clone(), session_id.to_string(), approval_tx.clone(), question_tx.clone(), stream_tx.clone())?
                .start_with_recovery(goal, InteractiveAgentMode::Execute, Some(plan_id), Some(invocation_id))?);
            timeline.bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));
        }
    }
    Ok(())
}
