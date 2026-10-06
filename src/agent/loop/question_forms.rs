//! One-call form admission, exact reuse and durable observation.
use super::*;
use crate::interactive::{FormQuestion, QuestionAnswer, QuestionForm, QuestionFormOutcome};

pub(crate) struct QuestionFormBinding<'a> {
    pub store: &'a SessionStore,
    pub session_id: &'a str,
    pub plan_id: Option<&'a str>,
    pub run_id: &'a str,
    pub invocation_id: ToolInvocationId,
    pub dependent_paths: &'a [String],
    pub discussion_invocation_id: Option<ToolInvocationId>,
}

fn exact_reused_answer(
    session: &Session,
    plan_id: Option<&str>,
    question: &FormQuestion,
) -> Option<ClarificationRecord> {
    session
        .clarifications
        .iter()
        .rev()
        .find(|record| {
            record.plan_id.as_deref() == plan_id
                && record.status == ClarificationStatus::Answered
                && record.question == question.question
                && record.proposed_answer == question.proposed_answer
                && crate::session::normalized_record_options(record) == question.options
        })
        .cloned()
}

fn revision_origin(
    session: &Session,
    binding: &QuestionFormBinding<'_>,
    index: usize,
    question: &FormQuestion,
) -> Option<(ToolInvocationId, usize)> {
    let candidates = session
        .clarifications
        .iter()
        .filter(|record| {
            record.plan_id.as_deref() == binding.plan_id
                && record.status == ClarificationStatus::Discussed
                && (record.run_id.as_deref() == Some(binding.run_id)
                    || binding.discussion_invocation_id == Some(record.invocation_id))
                && match question.title.as_deref() {
                    // Titles identify questions across revised calls, whose order
                    // and cardinality may change. Keep the original index in the link.
                    Some(title) => record.title.as_deref() == Some(title),
                    None => {
                        record.title.is_none()
                            && record.question == question.question
                            && record.question_index == index
                    }
                }
        })
        .filter(|record| {
            !session.clarifications.iter().any(|child| {
                child.origin_invocation_id == Some(record.invocation_id)
                    && child.origin_question_index == Some(record.question_index)
                    && child.plan_id == record.plan_id
                    && child.status == ClarificationStatus::Discussed
            })
        })
        .collect::<Vec<_>>();
    (candidates.len() == 1).then(|| (candidates[0].invocation_id, candidates[0].question_index))
}

pub(crate) fn prepare_question_form(
    binding: QuestionFormBinding<'_>,
    form: &QuestionForm,
) -> Result<Vec<Option<QuestionAnswer>>, String> {
    binding.store.update_session(binding.session_id,|session| {
        if session.clarifications.iter().any(|record|record.invocation_id==binding.invocation_id) {
            return Err(crate::session::SessionError::InvalidMutation("question invocation identity was replayed".to_string()));
        }
        let mut initial = Vec::new();
        for (index,question) in form.questions.iter().enumerate() {
            let prior = exact_reused_answer(session,binding.plan_id,question);
            let answer = prior.as_ref().and_then(crate::session::record_answer);
            let origin = revision_origin(session,&binding,index,question);
            let event_index = crate::session::append_question_event(session,"question_required",json!({"invocation_id":binding.invocation_id,"run_id":binding.run_id,"question_index":index,"question":question.question,"proposed_answer":question.proposed_answer,"options":question.options,"title":question.title,"header":form.header,"answer_reused":answer.is_some(),"origin_invocation_id":origin.map(|value|value.0),"origin_question_index":origin.map(|value|value.1)}));
            session.clarifications.push(ClarificationRecord {
                invocation_id:binding.invocation_id, plan_id:binding.plan_id.map(str::to_string), run_id:Some(binding.run_id.to_string()),
                question_index:index, title:question.title.clone(), header:form.header.clone(),
                question:question.question.clone(), proposed_answer:question.proposed_answer.clone(),
                options:question.options.iter().map(|option|option.label.clone()).collect(),option_details:question.options.clone(),
                dependent_paths:binding.dependent_paths.to_vec(),status:if answer.is_some(){ClarificationStatus::Answered}else{ClarificationStatus::Pending},
                answer:answer.as_ref().map(|answer|answer.answer.clone()),answer_source:answer.as_ref().map(|answer|answer.source),
                question_event_index:event_index, answer_message_index:prior.as_ref().and_then(|record|record.answer_message_index), answer_event_index:prior.as_ref().and_then(|record|record.answer_event_index),
                origin_invocation_id:origin.map(|value|value.0),origin_question_index:origin.map(|value|value.1),
                reason:answer.as_ref().map(|_|"reused exact answered question in the same plan".to_string()),outcome:answer.as_ref().map(|_|"answered".to_string()),
            });
            // Reused answers resolve a revision only through its explicit origin link.
            if let (Some(answer),Some((invocation,question_index)),Some(prior)) = (&answer,origin,&prior) {
                let source_event=prior.answer_event_index.unwrap_or_else(||crate::session::append_question_event(session,"human_question_answer_received",json!({"invocation_id":binding.invocation_id,"question_index":index,"answer":answer.answer,"source_message_index":prior.answer_message_index,"reused":true})));
                crate::session::question_forms::apply_linked_answer(session,invocation,question_index,answer,source_event)?;
            }
            initial.push(answer);
        }
        Ok(initial)
    }).map_err(|error|error.to_string())
}

pub(crate) async fn request_form_answer(
    cfg: &AgentLoopConfig,
    invocation_id: ToolInvocationId,
    form: &QuestionForm,
    initial: &[Option<QuestionAnswer>],
    sensitive: &[String],
) -> QuestionFormOutcome {
    let outcome = if initial.iter().all(Option::is_some) {
        QuestionFormOutcome::Answered(initial.iter().flatten().cloned().collect())
    } else if let Some(handler) = &cfg.question_handler {
        handler
            .ask_form(QuestionFormRequestContext {
                invocation_id,
                form,
                initial_answers: initial,
            })
            .await
    } else {
        QuestionFormOutcome::InputUnavailable("no question handler configured".to_string())
    };
    let safe = crate::session::public_form_outcome(&outcome, sensitive);
    match crate::interactive::validate_form_outcome(form, &safe) {
        Ok(()) => safe,
        Err(error) => QuestionFormOutcome::InputUnavailable(error),
    }
}
pub(crate) fn persist_form_observation(
    store: &SessionStore,
    session_id: &str,
    invocation_id: ToolInvocationId,
    observations: &[Value],
    outcome: &QuestionFormOutcome,
) -> Result<(), String> {
    store
        .try_append_message_with_origin(
            session_id,
            "tool",
            &json!({"observations":observations}).to_string(),
            MessageOrigin::ToolOutput,
        )
        .map_err(|error| error.to_string())?;
    store
        .update_session(session_id, |session| {
            crate::session::apply_form_outcome(session, invocation_id, outcome)
        })
        .map_err(|error| error.to_string())
}
