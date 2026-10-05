//! Durable form identities and lease-fenced recovery. Drafts are never answers.
use super::*;
use crate::interactive::{
    FormQuestion, QuestionAnswer, QuestionAnswerSource, QuestionForm, QuestionFormOutcome,
    QuestionOption,
};
use crate::tools::ToolInvocationId;
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedQuestionForm {
    pub invocation_id: ToolInvocationId,
    pub run_id: Option<String>,
    pub plan_id: Option<String>,
    pub form: QuestionForm,
    pub initial_answers: Vec<Option<QuestionAnswer>>,
}

pub(crate) fn record_question(record: &ClarificationRecord) -> FormQuestion {
    FormQuestion {
        title: record.title.clone(),
        question: record.question.clone(),
        proposed_answer: record.proposed_answer.clone(),
        options: normalized_record_options(record),
    }
}
pub(crate) fn normalized_record_options(record: &ClarificationRecord) -> Vec<QuestionOption> {
    if record.option_details.is_empty() {
        record
            .options
            .iter()
            .map(|label| QuestionOption {
                label: label.clone(),
                description: None,
            })
            .collect()
    } else {
        record.option_details.clone()
    }
}
pub(crate) fn record_answer(record: &ClarificationRecord) -> Option<QuestionAnswer> {
    if record.status != ClarificationStatus::Answered {
        return None;
    }
    let answer = record.answer.clone()?;
    let source = record.answer_source.unwrap_or_else(|| {
        if record.outcome.as_deref() == Some("approved_proposal") {
            QuestionAnswerSource::ApprovedProposal
        } else if record.proposed_answer.is_none() && record.options.contains(&answer) {
            QuestionAnswerSource::Option
        } else {
            QuestionAnswerSource::Text
        }
    });
    Some(QuestionAnswer { answer, source })
}
pub(crate) fn record_run_id(session: &Session, record: &ClarificationRecord) -> Option<String> {
    record.run_id.clone().or_else(|| {
        session
            .events
            .iter()
            .take(record.question_event_index + 1)
            .rev()
            .find(|event| event.kind == "run_started")
            .and_then(|event| {
                event
                    .details
                    .get("run_id")
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_string)
    })
}
pub fn pending_question_forms(session: &Session) -> Vec<PersistedQuestionForm> {
    let active_plan = session.plan.as_ref().map(|plan| plan.id.as_str());
    let mut groups = BTreeMap::<String, Vec<&ClarificationRecord>>::new();
    for record in session
        .clarifications
        .iter()
        .filter(|record| record.plan_id.as_deref() == active_plan)
    {
        groups
            .entry(record.invocation_id.to_string())
            .or_default()
            .push(record);
    }
    let mut pending = groups
        .values()
        .filter_map(|records| {
            let unresolved = records
                .iter()
                .filter(|record| record.status != ClarificationStatus::Answered)
                .collect::<Vec<_>>();
            if unresolved.is_empty() {
                return None;
            }
            // A revised form covers only explicitly linked obligations. Untouched siblings remain visible.
            if unresolved.iter().all(|record| {
                session.clarifications.iter().any(|candidate| {
                    candidate.origin_invocation_id == Some(record.invocation_id)
                        && candidate.origin_question_index == Some(record.question_index)
                        && candidate.plan_id == record.plan_id
                        && candidate.status != ClarificationStatus::Answered
                })
            }) {
                return None;
            }
            let mut records = records.clone();
            records.sort_by_key(|record| record.question_index);
            let first = records[0];
            Some(PersistedQuestionForm {
                invocation_id: first.invocation_id,
                run_id: record_run_id(session, first),
                plan_id: first.plan_id.clone(),
                form: QuestionForm {
                    header: first.header.clone(),
                    questions: records
                        .iter()
                        .map(|record| record_question(record))
                        .collect(),
                },
                initial_answers: records.iter().map(|record| record_answer(record)).collect(),
            })
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|form| {
        session
            .clarifications
            .iter()
            .find(|record| record.invocation_id == form.invocation_id)
            .map(|record| record.question_event_index)
    });
    pending
}

pub(crate) fn recovery_eligible(
    session: &Session,
    record: &ClarificationRecord,
) -> Result<String, SessionError> {
    let invalid = |message: &str| SessionError::InvalidMutation(message.to_string());
    let plan = session
        .plan
        .as_ref()
        .ok_or_else(|| invalid("question recovery requires an active plan"))?;
    if record.plan_id.as_deref() != Some(plan.id.as_str())
        || !plan.approved
        || !plan.has_identity()
        || !plan.is_structured()
        || plan.is_complete()
    {
        return Err(invalid(
            "question does not belong to a current approved incomplete plan",
        ));
    }
    if matches!(
        plan.outcome.as_deref(),
        Some(
            "provider_continuation_interrupted"
                | "cancelled"
                | "cancelled_by_user"
                | "stopped"
                | "superseded"
        )
    ) {
        return Err(invalid("question operation cannot be resumed safely"));
    }
    if let Some(run_id) = record_run_id(session, record) {
        if let Some(terminal) = session
            .events
            .iter()
            .rev()
            .find(|event| event.kind == "run_terminal" && event.details["run_id"] == run_id)
        {
            if !matches!(
                terminal.details["outcome"].as_str(),
                Some("waiting_for_user_input" | "unresolved_clarification")
            ) {
                return Err(invalid(
                    "question operation is terminal and cannot be resumed",
                ));
            }
        } else {
            return Err(invalid(
                "question operation is still active or unreconciled",
            ));
        }
    }
    Ok(plan.id.clone())
}

pub(crate) fn append_question_event(
    session: &mut Session,
    kind: &str,
    details: serde_json::Value,
) -> usize {
    let index = session.events.len();
    session.events.push(SessionEvent {
        index,
        kind: kind.to_string(),
        details,
        timestamp: Some(Utc::now()),
    });
    index
}

pub(crate) fn apply_linked_answer(
    session: &mut Session,
    invocation_id: ToolInvocationId,
    question_index: usize,
    answer: &QuestionAnswer,
    event_index: usize,
) -> Result<(), SessionError> {
    let mut target = Some((invocation_id, question_index));
    let mut visited = std::collections::BTreeSet::new();
    while let Some((invocation, index)) = target {
        if !visited.insert((invocation.to_string(), index)) {
            return Err(SessionError::InvalidMutation(
                "question obligation link contains a cycle".to_string(),
            ));
        }
        let record = session
            .clarifications
            .iter_mut()
            .find(|record| record.invocation_id == invocation && record.question_index == index)
            .ok_or_else(|| {
                SessionError::InvalidMutation("linked question obligation is missing".to_string())
            })?;
        if record.status != ClarificationStatus::Answered || invocation == invocation_id {
            record.status = ClarificationStatus::Answered;
            record.answer = Some(answer.answer.clone());
            let source =
                if crate::interactive::validate_question_answer(&record_question(record), answer)
                    .is_ok()
                {
                    answer.source
                } else {
                    QuestionAnswerSource::Text
                };
            record.answer_source = Some(source);
            record.answer_event_index = Some(event_index);
            record.answer_message_index = None;
            record.outcome = Some(
                if source == QuestionAnswerSource::ApprovedProposal {
                    "approved_proposal"
                } else {
                    "answered"
                }
                .to_string(),
            );
            record.reason = (invocation != invocation_id)
                .then(|| "resolved by its explicitly linked revised question".to_string());
        }
        target = record
            .origin_invocation_id
            .zip(record.origin_question_index);
    }
    Ok(())
}

pub(crate) fn apply_form_outcome(
    session: &mut Session,
    invocation_id: ToolInvocationId,
    outcome: &QuestionFormOutcome,
) -> Result<(), SessionError> {
    let records = session
        .clarifications
        .iter()
        .filter(|record| record.invocation_id == invocation_id)
        .cloned()
        .collect::<Vec<_>>();
    if records.is_empty() {
        return Err(SessionError::InvalidMutation(
            "question invocation was not found".to_string(),
        ));
    }
    match outcome {
        QuestionFormOutcome::Answered(answers) => {
            if answers.len() != records.len() {
                return Err(SessionError::InvalidMutation(
                    "question form requires all answers before Submit".to_string(),
                ));
            }
            for record in &records {
                let answer = answers.get(record.question_index).ok_or_else(|| {
                    SessionError::InvalidMutation(
                        "question index is outside the submitted form".to_string(),
                    )
                })?;
                crate::interactive::validate_question_answer(&record_question(record), answer)
                    .map_err(SessionError::InvalidMutation)?;
            }
            for record in &records {
                if answers
                    .get(record.question_index)
                    .is_some_and(|answer| record_answer(record).as_ref() == Some(answer))
                {
                    continue;
                }
                let answer = answers.get(record.question_index).ok_or_else(|| {
                    SessionError::InvalidMutation(
                        "question index is outside the submitted form".to_string(),
                    )
                })?;
                let event = append_question_event(
                    session,
                    "human_question_answer_received",
                    json!({"invocation_id":invocation_id,"question_index":record.question_index,"question_event_index":record.question_event_index,"answer":answer.answer,"source":answer.source,"decision":if answer.source==QuestionAnswerSource::ApprovedProposal {"approved_proposal"} else {"answered"}}),
                );
                session.human_intent.push(HumanIntentRecord {
                    kind: HumanIntentKind::QuestionAnswer,
                    text: answer.answer.clone(),
                    source_message_index: None,
                    source_event_index: Some(event),
                });
                apply_linked_answer(session, invocation_id, record.question_index, answer, event)?;
            }
        }
        other => {
            if let QuestionFormOutcome::Discussed(message) = other {
                let first = &records[0];
                let event = append_question_event(
                    session,
                    "human_question_discussion_received",
                    json!({"invocation_id":invocation_id,"plan_id":first.plan_id,"run_id":first.run_id,"message":message}),
                );
                session.human_intent.push(HumanIntentRecord {
                    kind: HumanIntentKind::Steering,
                    text: message.clone(),
                    source_message_index: None,
                    source_event_index: Some(event),
                });
            }
            for record in session.clarifications.iter_mut().filter(|record| {
                record.invocation_id == invocation_id
                    && record.status != ClarificationStatus::Answered
            }) {
                record.status = if matches!(other, QuestionFormOutcome::Discussed(_)) {
                    ClarificationStatus::Discussed
                } else if matches!(other, QuestionFormOutcome::Cancelled) {
                    ClarificationStatus::Cancelled
                } else {
                    ClarificationStatus::Unresolved
                };
                record.outcome = Some(other.reason().to_string());
                record.reason = Some(match other {
                    QuestionFormOutcome::InputUnavailable(error) => error.clone(),
                    _ => other.reason().to_string(),
                });
            }
        }
    }
    Ok(())
}

pub fn persist_recovered_form_outcome(
    store: &SessionStore,
    session_id: &str,
    invocation_id: ToolInvocationId,
    outcome: &QuestionFormOutcome,
) -> Result<String, String> {
    let lease = store
        .try_acquire_run_lease(session_id)
        .map_err(|error| error.to_string())?;
    lease.verify().map_err(|error| error.to_string())?;
    let safe = public_form_outcome(outcome, store.public_sensitive_values());
    store
        .update_session(session_id, |session| {
            let record = session
                .clarifications
                .iter()
                .find(|record| record.invocation_id == invocation_id)
                .cloned()
                .ok_or_else(|| {
                    SessionError::InvalidMutation("question invocation was not found".to_string())
                })?;
            let plan_id = recovery_eligible(session, &record)?;
            let mut records = session
                .clarifications
                .iter()
                .filter(|record| record.invocation_id == invocation_id)
                .collect::<Vec<_>>();
            records.sort_by_key(|record| record.question_index);
            for sibling in &records {
                recovery_eligible(session, sibling)?;
                if sibling.plan_id != record.plan_id || sibling.run_id != record.run_id {
                    return Err(SessionError::InvalidMutation(
                        "question form indexes belong to different operations".to_string(),
                    ));
                }
            }
            if records
                .iter()
                .all(|record| record.status == ClarificationStatus::Answered)
            {
                return Err(SessionError::InvalidMutation(
                    "question form was already answered".to_string(),
                ));
            }
            let form = QuestionForm {
                header: record.header.clone(),
                questions: records
                    .iter()
                    .map(|record| record_question(record))
                    .collect(),
            };
            crate::interactive::validate_form_outcome(&form, &safe)
                .map_err(SessionError::InvalidMutation)?;
            let first_event = session.events.len();
            apply_form_outcome(session, invocation_id, &safe)?;
            for event in &mut session.events[first_event..] {
                event.details["recovered"] = json!(true);
            }
            Ok(plan_id)
        })
        .map_err(|error| error.to_string())
}
pub fn persist_recovered_form_answers(
    store: &SessionStore,
    session_id: &str,
    invocation_id: ToolInvocationId,
    answers: &[QuestionAnswer],
) -> Result<String, String> {
    persist_recovered_form_outcome(
        store,
        session_id,
        invocation_id,
        &QuestionFormOutcome::Answered(answers.to_vec()),
    )
}
pub fn persist_recovered_form_answer(
    store: &SessionStore,
    session_id: &str,
    invocation_id: ToolInvocationId,
    question_index: usize,
    answer: &QuestionAnswer,
) -> Result<String, String> {
    let lease = store
        .try_acquire_run_lease(session_id)
        .map_err(|error| error.to_string())?;
    lease.verify().map_err(|error| error.to_string())?;
    let answer = QuestionAnswer {
        answer: crate::interactive::bounded_public_text(
            &answer.answer,
            store.public_sensitive_values(),
            20_000,
            false,
        ),
        source: answer.source,
    };
    store.update_session(session_id,|session| {
        if session.clarifications.iter().filter(|record|record.invocation_id==invocation_id).count()!=1 {
            return Err(SessionError::InvalidMutation("question sets require explicit full-form Submit".to_string()));
        }

        let record = session.clarifications.iter().find(|record|record.invocation_id==invocation_id&&record.question_index==question_index).cloned().ok_or_else(||SessionError::InvalidMutation("exact question identity was not found".to_string()))?;
        let plan_id = recovery_eligible(session,&record)?;
        if record.status==ClarificationStatus::Answered { return Err(SessionError::InvalidMutation("question was already answered".to_string())); }
        crate::interactive::validate_question_answer(&record_question(&record),&answer).map_err(SessionError::InvalidMutation)?;
        let event = append_question_event(session,"human_question_answer_received",json!({"invocation_id":invocation_id,"question_index":question_index,"answer":answer.answer,"source":answer.source,"recovered":true,"decision":if answer.source==QuestionAnswerSource::ApprovedProposal {"approved_proposal"} else {"answered"}}));
        session.human_intent.push(HumanIntentRecord {kind:HumanIntentKind::QuestionAnswer,text:answer.answer.clone(),source_message_index:None,source_event_index:Some(event)});
        apply_linked_answer(session,invocation_id,question_index,&answer,event)?;
        Ok(plan_id)
    }).map_err(|error|error.to_string())
}
pub(crate) fn public_form_outcome(
    outcome: &QuestionFormOutcome,
    sensitive: &[String],
) -> QuestionFormOutcome {
    let bound =
        |text: &str| crate::interactive::bounded_public_text(text, sensitive, 20_000, false);
    match outcome {
        QuestionFormOutcome::Answered(answers) => QuestionFormOutcome::Answered(
            answers
                .iter()
                .map(|answer| QuestionAnswer {
                    answer: bound(&answer.answer),
                    source: answer.source,
                })
                .collect(),
        ),
        QuestionFormOutcome::Discussed(message) => QuestionFormOutcome::Discussed(bound(message)),
        QuestionFormOutcome::InputUnavailable(error) => {
            QuestionFormOutcome::InputUnavailable(bound(error))
        }
        other => other.clone(),
    }
}

pub(crate) fn validate_form_records(session: &Session) -> Result<(), SessionError> {
    let invalid = || {
        SessionError::InvalidMutation(
            "question form identities or fields are inconsistent".to_string(),
        )
    };
    let mut identities = std::collections::BTreeSet::new();
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    for record in &session.clarifications {
        if record.question_index >= 8
            || !identities.insert((record.invocation_id.to_string(), record.question_index))
            || record.question.len() > 20_000
            || record
                .proposed_answer
                .as_ref()
                .is_some_and(|value| value.len() > 20_000)
            || record
                .title
                .as_ref()
                .is_some_and(|value| value.trim().is_empty() || value.len() > 40)
            || record
                .header
                .as_ref()
                .is_some_and(|value| value.len() > 500)
            || record.option_details.len() > 20
            || record.option_details.iter().any(|option| {
                option.label.trim().is_empty()
                    || option.label.len() > 1_000
                    || option
                        .description
                        .as_ref()
                        .is_some_and(|value| value.len() > 1_000)
            })
            || record.origin_invocation_id.is_some() != record.origin_question_index.is_some()
        {
            return Err(invalid());
        }
        groups
            .entry(record.invocation_id.to_string())
            .or_default()
            .push(record.question_index);
        if let Some((origin, index)) = record
            .origin_invocation_id
            .zip(record.origin_question_index)
        {
            if !session.clarifications.iter().any(|candidate| {
                candidate.invocation_id == origin
                    && candidate.question_index == index
                    && candidate.plan_id == record.plan_id
                    && candidate.question_event_index < record.question_event_index
            }) {
                return Err(invalid());
            }
        }
        if record.status == ClarificationStatus::Answered {
            if let Some(answer) = record_answer(record) {
                crate::interactive::validate_question_answer(&record_question(record), &answer)
                    .map_err(SessionError::InvalidMutation)?;
            }
        }
    }
    for indices in groups.values_mut() {
        indices.sort_unstable();
        if indices
            .iter()
            .enumerate()
            .any(|(expected, index)| expected != *index)
        {
            return Err(invalid());
        }
    }
    Ok(())
}
