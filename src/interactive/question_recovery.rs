//! Resume exact interrupted clarification operations from ordinary conversation.
use super::{
    bounded_public_text, load_continue_plan_effect, parse_question_line, public_question_form,
    FormQuestion, InteractiveEffect, QuestionAnswer, QuestionAnswerSource, QuestionFormOutcome,
    QuestionLineInput,
};
use crate::session::{pending_question_forms, PersistedQuestionForm, Session, SessionStore};
use crate::tools::ToolInvocationId;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionRecoveryEffect {
    Output(String),
    OpenForm(PersistedQuestionForm),
    OpenEditor {
        form: PersistedQuestionForm,
        question_index: Option<usize>,
    },
    ContinuePlan {
        plan_id: String,
        goal: String,
    },
    ContinueDiscussion {
        plan_id: String,
        goal: String,
        invocation_id: ToolInvocationId,
    },
}

fn load_session(store: &SessionStore, session_id: &str) -> Result<Session, String> {
    store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the clarification session was not found".to_string())
}

fn plan_admission(session: &Session) -> Result<(), String> {
    let plan = session
        .plan
        .as_ref()
        .ok_or_else(|| "no current plan can be resumed".to_string())?;
    if !plan.approved
        || !plan.is_structured()
        || plan.is_complete()
        || plan.id.trim().is_empty()
        || plan.goal.trim().is_empty()
    {
        return Err("the current plan is not eligible for clarification recovery".to_string());
    }
    if matches!(
        plan.outcome.as_deref(),
        Some("cancelled" | "cancelled_by_user" | "stopped" | "superseded")
    ) {
        return Err(
            "this question operation was stopped or superseded and cannot be resumed".to_string(),
        );
    }
    if plan.outcome.as_deref() == Some("provider_continuation_interrupted") {
        return Err("the provider continuation is uncertain and cannot be replayed".to_string());
    }
    Ok(())
}

fn record_run<'a>(
    session: &'a Session,
    record: &'a crate::session::ClarificationRecord,
) -> Option<&'a str> {
    record.run_id.as_deref().or_else(|| {
        session
            .events
            .iter()
            .rev()
            .find(|event| event.index <= record.question_event_index && event.kind == "run_started")
            .and_then(|event| {
                event
                    .details
                    .get("run_id")
                    .and_then(serde_json::Value::as_str)
            })
    })
}

fn recoverable_record(session: &Session, record: &crate::session::ClarificationRecord) -> bool {
    if record.plan_id.as_deref() != session.plan.as_ref().map(|plan| plan.id.as_str()) {
        return false;
    }
    let Some(run_id) = record_run(session, record) else {
        // Old sessions without run metadata remain readable; known runs require
        // their exact terminal waiting outcome rather than inferred UI idleness.
        return !session
            .events
            .iter()
            .any(|event| event.kind == "run_started");
    };
    session
        .events
        .iter()
        .rev()
        .find(|event| {
            event.kind == "run_terminal"
                && event
                    .details
                    .get("run_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(run_id)
        })
        .is_some_and(|event| {
            matches!(
                event
                    .details
                    .get("outcome")
                    .and_then(serde_json::Value::as_str),
                Some("waiting_for_user_input" | "unresolved_clarification")
            )
        })
}

/// Whether the session's current plan waits on the user through a question
/// that question recovery can actually reopen (T081). Plans whose questions
/// were cancelled or superseded are not waiting, so they cannot trap chat.
pub(crate) fn plan_has_recoverable_question(session: &Session) -> bool {
    plan_admission(session).is_ok()
        && pending_question_forms(session).iter().any(|form| {
            session
                .clarifications
                .iter()
                .filter(|record| record.invocation_id == form.invocation_id)
                .all(|record| recoverable_record(session, record))
        })
}

fn eligible_forms(
    session: &Session,
    store: &SessionStore,
) -> Result<Vec<PersistedQuestionForm>, String> {
    pending_question_forms(session)
        .into_iter()
        .filter(|form| {
            session
                .clarifications
                .iter()
                .filter(|record| record.invocation_id == form.invocation_id)
                .all(|record| recoverable_record(session, record))
        })
        .map(|mut form| {
            form.form = public_question_form(&form.form, store.public_sensitive_values())?;
            form.initial_answers = form
                .initial_answers
                .into_iter()
                .map(|answer| answer.map(|answer| safe_answer(store, answer)))
                .collect();
            Ok(form)
        })
        .collect()
}

fn resume_request(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "resume"
            | "continue"
            | "resume the plan"
            | "continue the plan"
            | "resume the question"
            | "answer the question"
            | "resume the questions"
            | "answer the questions"
    )
}

fn operation_choice(forms: &[PersistedQuestionForm]) -> QuestionRecoveryEffect {
    let mut text = String::from("Which interrupted question operation should I resume?");
    for form in forms {
        text.push_str(&format!(
            "\n  {}: {}",
            form.invocation_id,
            form.form
                .header
                .as_deref()
                .or_else(|| form
                    .form
                    .questions
                    .first()
                    .and_then(|question| question.title.as_deref()))
                .unwrap_or("Saved question")
        ));
    }
    text.push_str("\nReply with resume followed by the exact operation ID.");
    QuestionRecoveryEffect::Output(text)
}

fn explicit_operation<'a>(text: &'a str, form: &PersistedQuestionForm) -> Option<&'a str> {
    let id = form.invocation_id.to_string();
    for prefix in ["resume ", "continue ", "answer ", ""] {
        let Some(tail) = text
            .strip_prefix(prefix)
            .and_then(|tail| tail.strip_prefix(&id))
        else {
            continue;
        };
        if tail.is_empty() {
            return Some("");
        }
        if let Some(answer) = tail.strip_prefix(':') {
            return Some(answer.trim());
        }
    }
    None
}

fn question_response<'a>(
    text: &'a str,
    question: &FormQuestion,
    index: usize,
    single: bool,
    addressed: bool,
) -> Option<(&'a str, bool)> {
    for label in question
        .title
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(question.question.as_str()))
    {
        if let Some(answer) = text
            .strip_prefix(label)
            .and_then(|tail| tail.strip_prefix(':'))
        {
            return Some((answer.trim(), true));
        }
    }
    let number = format!("{}:", index + 1);
    if !single {
        if let Some(answer) = text.strip_prefix(&number) {
            return Some((answer.trim(), true));
        }
        return None;
    }
    let recognized = addressed
        || text.starts_with("text:")
        || text == "esc"
        || text == "chat"
        || (!text.is_empty() && text.chars().all(|character| character.is_ascii_digit()))
        || question.options.iter().any(|option| option.label == text)
        || (question.proposed_answer.is_some()
            && matches!(
                text.to_ascii_lowercase().as_str(),
                "yes" | "approve" | "approve proposed answer" | "reject" | "no" | "y"
            ));
    recognized.then_some((text, addressed))
}

fn conversational_input(
    question: &FormQuestion,
    text: &str,
    _addressed: bool,
) -> QuestionLineInput {
    if text == "chat"
        || text == "esc"
        || text.starts_with("text:")
        || (!text.is_empty() && text.chars().all(|character| character.is_ascii_digit()))
    {
        return parse_question_line(question, text);
    }
    if question.proposed_answer.is_some() {
        match text.to_ascii_lowercase().as_str() {
            "yes" | "approve" | "approve proposed answer" => {
                return QuestionLineInput::Answer(QuestionAnswer {
                    answer: question.proposed_answer.clone().unwrap_or_default(),
                    source: QuestionAnswerSource::ApprovedProposal,
                })
            }
            "reject" => return QuestionLineInput::Reject,
            _ => {}
        }
    }
    if question.proposed_answer.is_none()
        && question.options.iter().any(|option| option.label == text)
    {
        return QuestionLineInput::Answer(QuestionAnswer {
            answer: text.to_string(),
            source: QuestionAnswerSource::Option,
        });
    }
    parse_question_line(question, text)
}

fn safe_answer(store: &SessionStore, mut answer: QuestionAnswer) -> QuestionAnswer {
    answer.answer = bounded_public_text(
        &answer.answer,
        store.public_sensitive_values(),
        20_000,
        true,
    );
    answer
}

fn apply_response(
    store: &SessionStore,
    session_id: &str,
    mut form: PersistedQuestionForm,
    question_index: usize,
    input: QuestionLineInput,
) -> Result<QuestionRecoveryEffect, String> {
    match input {
        QuestionLineInput::Answer(answer) => {
            let answer = safe_answer(store, answer);
            if form.form.questions.len() > 1 {
                form.initial_answers[question_index] = Some(answer);
                return Ok(QuestionRecoveryEffect::OpenForm(form));
            }
            complete_question_recovery(
                store,
                session_id,
                form.invocation_id,
                QuestionFormOutcome::Answered(vec![answer]),
            )
        }
        QuestionLineInput::EnterText => Ok(QuestionRecoveryEffect::OpenEditor {
            form,
            question_index: Some(question_index),
        }),
        QuestionLineInput::EnterDiscussion => Ok(QuestionRecoveryEffect::OpenEditor {
            form,
            question_index: None,
        }),
        QuestionLineInput::Reject | QuestionLineInput::Interrupt => complete_question_recovery(
            store,
            session_id,
            form.invocation_id,
            QuestionFormOutcome::LeftUnanswered,
        ),
        QuestionLineInput::Retry(message) => Ok(QuestionRecoveryEffect::Output(message)),
    }
}

/// Unrelated prose returns None and remains an ordinary conversation turn.
/// A set answer opens an ephemeral draft; only the native Submit can persist it.
pub fn recover_question_conversation(
    store: &SessionStore,
    session_id: &str,
    text: &str,
) -> Result<Option<QuestionRecoveryEffect>, String> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('/') {
        return Ok(None);
    }
    let session = load_session(store, session_id)?;
    let forms = eligible_forms(&session, store)?;
    if forms.is_empty() {
        if resume_request(text)
            && session.clarifications.iter().any(|record| {
                record.status == crate::session::ClarificationStatus::Answered
                    && recoverable_record(&session, record)
            })
        {
            let plan_id = session
                .plan
                .as_ref()
                .map(|plan| plan.id.as_str())
                .ok_or_else(|| "no plan remains to resume".to_string())?;
            return continue_after_answers(store, session_id, plan_id).map(Some);
        }
        return Ok(None);
    }
    if let Err(message) = plan_admission(&session) {
        return Ok(resume_request(text).then_some(QuestionRecoveryEffect::Output(message)));
    }
    let explicitly_selected = forms
        .iter()
        .find_map(|form| explicit_operation(text, form).map(|response| (form.clone(), response)));
    if let Some((form, response)) = explicitly_selected {
        if response.is_empty() {
            return Ok(Some(QuestionRecoveryEffect::OpenForm(form)));
        }
        return matched_response(store, session_id, &[form], response, true);
    }
    if resume_request(text) {
        let operations = forms
            .iter()
            .map(|form| {
                form.run_id
                    .clone()
                    .unwrap_or_else(|| form.invocation_id.to_string())
            })
            .collect::<BTreeSet<_>>();
        return Ok(Some(if operations.len() > 1 {
            operation_choice(&forms)
        } else {
            QuestionRecoveryEffect::OpenForm(forms[0].clone())
        }));
    }
    matched_response(store, session_id, &forms, text, false)
}

fn matched_response(
    store: &SessionStore,
    session_id: &str,
    forms: &[PersistedQuestionForm],
    text: &str,
    addressed: bool,
) -> Result<Option<QuestionRecoveryEffect>, String> {
    let mut matches = Vec::new();
    for form in forms {
        for (index, question) in form.form.questions.iter().enumerate() {
            if form.initial_answers.get(index).is_some_and(Option::is_some) {
                continue;
            }
            let Some((response, targeted)) = question_response(
                text,
                question,
                index,
                form.form.questions.len() == 1,
                addressed,
            ) else {
                continue;
            };
            matches.push((
                form.clone(),
                index,
                conversational_input(question, response, targeted),
            ));
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => {
            let (form, index, input) = matches.remove(0);
            apply_response(store, session_id, form, index, input).map(Some)
        }
        _ => Ok(Some(operation_choice(forms))),
    }
}

fn continue_after_answers(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: &str,
) -> Result<QuestionRecoveryEffect, String> {
    let session = load_session(store, session_id)?;
    if session.plan.as_ref().map(|plan| plan.id.as_str()) != Some(expected_plan_id) {
        return Err(
            "the answered question plan is no longer current and cannot resume".to_string(),
        );
    }
    plan_admission(&session)?;
    let forms = eligible_forms(&session, store)?;
    if forms.len() > 1 {
        let operations = forms
            .iter()
            .map(|form| {
                form.run_id
                    .clone()
                    .unwrap_or_else(|| form.invocation_id.to_string())
            })
            .collect::<BTreeSet<_>>();
        if operations.len() > 1 {
            return Ok(operation_choice(&forms));
        }
    }
    if let Some(form) = forms.into_iter().next() {
        return Ok(QuestionRecoveryEffect::OpenForm(form));
    }
    match load_continue_plan_effect(store, session_id, expected_plan_id)? {
        InteractiveEffect::ContinuePlan { plan_id, goal } => {
            Ok(QuestionRecoveryEffect::ContinuePlan { plan_id, goal })
        }
        _ => Err("clarification recovery did not produce a continuation".to_string()),
    }
}

pub fn complete_question_recovery(
    store: &SessionStore,
    session_id: &str,
    invocation_id: ToolInvocationId,
    outcome: QuestionFormOutcome,
) -> Result<QuestionRecoveryEffect, String> {
    let session = load_session(store, session_id)?;
    plan_admission(&session)?;
    let form = eligible_forms(&session, store)?
        .into_iter()
        .find(|form| form.invocation_id == invocation_id)
        .ok_or_else(|| "that interrupted question operation is no longer eligible".to_string())?;
    let outcome = match outcome {
        QuestionFormOutcome::Answered(answers) => QuestionFormOutcome::Answered(
            answers
                .into_iter()
                .map(|answer| safe_answer(store, answer))
                .collect(),
        ),
        QuestionFormOutcome::Discussed(message) => QuestionFormOutcome::Discussed(
            bounded_public_text(&message, store.public_sensitive_values(), 20_000, true),
        ),
        other => other,
    };
    let persisted_plan_id =
        crate::session::persist_recovered_form_outcome(store, session_id, invocation_id, &outcome)?;
    match outcome {
        QuestionFormOutcome::Answered(_) => Ok(continue_after_answers(
            store,
            session_id,
            &persisted_plan_id,
        )
        .unwrap_or_else(|error| {
            QuestionRecoveryEffect::Output(format!(
                "Your answers were saved. Dependent work remains paused: {}",
                bounded_public_text(&error, store.public_sensitive_values(), 1_000, false)
            ))
        })),
        QuestionFormOutcome::Discussed(_) => {
            let plan = session
                .plan
                .as_ref()
                .ok_or_else(|| "no discussion plan remains".to_string())?;
            Ok(QuestionRecoveryEffect::ContinueDiscussion {
                plan_id: plan.id.clone(),
                goal: plan.goal.clone(),
                invocation_id: form.invocation_id,
            })
        }
        _ => Ok(QuestionRecoveryEffect::Output(
            "The question remains unanswered. Dependent work is paused; say resume to reopen it."
                .to_string(),
        )),
    }
}

#[cfg(test)]
#[path = "question_recovery_tests.rs"]
mod tests;
