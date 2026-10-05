//! Discussion recovery admits conversation only from an exact trusted human event.
use super::*;

fn discussion_evidence(
    session: &Session,
    invocation_id: ToolInvocationId,
    current_run_id: &str,
) -> Result<usize, String> {
    let record = session
        .clarifications
        .iter()
        .find(|record| {
            record.invocation_id == invocation_id && record.status == ClarificationStatus::Discussed
        })
        .ok_or_else(|| {
            "discussion continuation has no exact unanswered discussed call".to_string()
        })?;
    crate::session::recovery_eligible(session, record).map_err(|error| error.to_string())?;
    let event = session
        .events
        .iter()
        .rev()
        .find(|event| {
            event.kind == "human_question_discussion_received"
                && event.details["invocation_id"] == json!(invocation_id)
                && event.details["recovered"] == true
        })
        .ok_or_else(|| "discussion continuation has no recovered human message".to_string())?;
    if event.details["plan_id"].as_str() != record.plan_id.as_deref()
        || event.details["run_id"].as_str() != record.run_id.as_deref()
        || !session.human_intent.iter().any(|intent| {
            intent.kind == HumanIntentKind::Steering
                && intent.source_event_index == Some(event.index)
                && event.details["message"].as_str() == Some(intent.text.as_str())
        })
    {
        return Err(
            "discussion continuation evidence is not trusted or bound to this operation"
                .to_string(),
        );
    }
    if session.events.iter().any(|candidate| {
        candidate.kind == "question_discussion_continuation_started"
            && candidate.details["source_event_index"] == json!(event.index)
            && candidate.details["run_id"] != current_run_id
    }) {
        return Err(
            "discussion continuation evidence was already consumed by another run".to_string(),
        );
    }
    Ok(event.index)
}

pub(crate) fn discussion_admission_error(
    session: &Session,
    plan_id: &str,
    goal: &str,
    run_id: &str,
    invocation_id: ToolInvocationId,
) -> Option<String> {
    if let Some(error) =
        continue_admission_error(session, plan_id, &normalize_plan_goal(goal), run_id)
    {
        if error != "unresolved questions still block this plan" {
            return Some(error);
        }
    }
    if has_unterminated_prior_run(session, run_id) {
        return Some(
            "a prior run has not been reconciled and this plan cannot continue".to_string(),
        );
    }
    discussion_evidence(session, invocation_id, run_id).err()
}
pub(crate) fn validate_discussion_admission(
    store: &SessionStore,
    session_id: &str,
    plan_id: &str,
    goal: &str,
    run_id: &str,
    invocation_id: ToolInvocationId,
) -> Result<(), String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "discussion session disappeared".to_string())?;
    discussion_admission_error(&session, plan_id, goal, run_id, invocation_id).map_or(Ok(()), Err)
}
pub(crate) fn prepare_discussion_turn(
    store: &SessionStore,
    session_id: &str,
    plan_id: &str,
    goal: &str,
    run_id: &str,
    invocation_id: ToolInvocationId,
) -> Result<(), String> {
    store.update_session(session_id,|session| {
        if let Some(error)=discussion_admission_error(session,plan_id,goal,run_id,invocation_id) {return Err(crate::session::SessionError::InvalidMutation(error));}
        let source=discussion_evidence(session,invocation_id,run_id).map_err(crate::session::SessionError::InvalidMutation)?;
        append_session_event(session,"question_discussion_continuation_started",json!({"source_event_index":source,"invocation_id":invocation_id,"plan_id":plan_id,"run_id":run_id}));
        let message_index=session.messages.len();
        session.messages.push(SessionMessage {index:message_index,role:"user".to_string(),content:"Continue discussing the pending questions for this exact plan. Answers remain required before dependent work or completion.".to_string(),timestamp:Some(Utc::now()),attachments:Vec::new()});
        session.message_provenance.push(MessageProvenance {message_index,origin:MessageOrigin::RuntimeContinuation});
        let event=crate::session::append_question_event(session,"plan_continue_requested",json!({"plan_id":plan_id,"goal_provenance":"persisted_plan_goal","discussion_invocation_id":invocation_id}));
        session.human_intent.push(HumanIntentRecord {kind:HumanIntentKind::Continue,text:plan_id.to_string(),source_message_index:None,source_event_index:Some(event)});
        Ok(())
    }).map_err(|error|error.to_string())
}
