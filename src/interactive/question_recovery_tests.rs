use super::*;
use crate::interactive::tests::recoverable_question_fixture;
use crate::session::{ClarificationStatus, SessionEvent};
use serde_json::json;

#[test]
fn exact_option_answer_recovers_and_resumes_the_saved_plan() {
    let (_directory, store, session_id, invocation_id, plan_id) = recoverable_question_fixture();
    let effect = recover_question_conversation(&store, &session_id, "beta")
        .unwrap()
        .unwrap();
    assert!(
        matches!(effect, QuestionRecoveryEffect::ContinuePlan {plan_id: id, ..} if id == plan_id)
    );
    let session = store.load(&session_id).unwrap();
    let record = &session.clarifications[0];
    assert_eq!(record.invocation_id, invocation_id);
    assert_eq!(record.status, ClarificationStatus::Answered);
    assert_eq!(record.answer.as_deref(), Some("beta"));
    assert_eq!(record.answer_source, Some(QuestionAnswerSource::Option));
    assert!(session
        .human_intent
        .iter()
        .any(|intent| intent.text == "beta"));
}

#[test]
fn unrelated_conversation_never_changes_a_pending_answer() {
    let (_directory, store, session_id, _, _) = recoverable_question_fixture();
    let before = serde_json::to_value(store.load(&session_id).unwrap()).unwrap();
    assert!(
        recover_question_conversation(&store, &session_id, "How does scheduling work?")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        serde_json::to_value(store.load(&session_id).unwrap()).unwrap(),
        before
    );
}

#[test]
fn resume_reopens_the_exact_form_without_answering_it() {
    let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
    let effect = recover_question_conversation(&store, &session_id, "resume")
        .unwrap()
        .unwrap();
    assert!(
        matches!(effect, QuestionRecoveryEffect::OpenForm(form) if form.invocation_id == invocation_id)
    );
    assert_eq!(
        store.load(&session_id).unwrap().clarifications[0].status,
        ClarificationStatus::Unresolved
    );
}

fn append_second_question(
    store: &SessionStore,
    session_id: &str,
    invocation: ToolInvocationId,
    grouped: bool,
) {
    store
        .update_session(session_id, |session| {
            let mut record = session.clarifications[0].clone();
            record.invocation_id = invocation;
            record.question = "Which environment?".to_string();
            if grouped {
                session.clarifications[0].title = Some("Target".to_string());
                record.title = Some("Environment".to_string());
                record.question_index = 1;
            }
            session.clarifications.push(record);
            Ok(())
        })
        .unwrap();
}

#[test]
fn ambiguous_answers_require_an_exact_operation_choice() {
    let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
    let other = ToolInvocationId::new();
    append_second_question(&store, &session_id, other, false);
    let effect = recover_question_conversation(&store, &session_id, "beta")
        .unwrap()
        .unwrap();
    let QuestionRecoveryEffect::Output(text) = effect else {
        panic!("ambiguous answer must ask which operation");
    };
    assert!(text.contains(&invocation_id.to_string()));
    assert!(text.contains(&other.to_string()));
    assert!(store
        .load(&session_id)
        .unwrap()
        .clarifications
        .iter()
        .all(|record| record.answer.is_none()));
    let effect = recover_question_conversation(&store, &session_id, &format!("resume {other}"))
        .unwrap()
        .unwrap();
    assert!(
        matches!(effect, QuestionRecoveryEffect::OpenForm(form) if form.invocation_id == other)
    );
}

#[test]
fn conversational_set_answer_is_ephemeral_until_explicit_submit() {
    let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
    append_second_question(&store, &session_id, invocation_id, true);
    let effect = recover_question_conversation(&store, &session_id, "Environment: beta")
        .unwrap()
        .unwrap();
    let QuestionRecoveryEffect::OpenForm(mut form) = effect else {
        panic!("a set requires review and Submit");
    };
    assert_eq!(form.initial_answers[1].as_ref().unwrap().answer, "beta");
    assert!(store
        .load(&session_id)
        .unwrap()
        .clarifications
        .iter()
        .all(|record| record.answer.is_none()));
    form.initial_answers[0] = Some(QuestionAnswer {
        answer: "alpha".to_string(),
        source: QuestionAnswerSource::Option,
    });
    let answers = form
        .initial_answers
        .into_iter()
        .map(Option::unwrap)
        .collect();
    let effect = complete_question_recovery(
        &store,
        &session_id,
        invocation_id,
        QuestionFormOutcome::Answered(answers),
    )
    .unwrap();
    assert!(matches!(
        effect,
        QuestionRecoveryEffect::ContinuePlan { .. }
    ));
    assert!(store
        .load(&session_id)
        .unwrap()
        .clarifications
        .iter()
        .all(|record| record.status == ClarificationStatus::Answered));
}

#[test]
fn recovery_lease_and_incomplete_submit_leave_every_record_unanswered() {
    let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
    append_second_question(&store, &session_id, invocation_id, true);
    let answer = QuestionAnswer {
        answer: "beta".to_string(),
        source: QuestionAnswerSource::Option,
    };
    assert!(complete_question_recovery(
        &store,
        &session_id,
        invocation_id,
        QuestionFormOutcome::Answered(vec![answer.clone()])
    )
    .is_err());
    let lease = store.try_acquire_run_lease(&session_id).unwrap();
    assert!(complete_question_recovery(
        &store,
        &session_id,
        invocation_id,
        QuestionFormOutcome::Answered(vec![answer.clone(), answer])
    )
    .is_err());
    drop(lease);
    assert!(store
        .load(&session_id)
        .unwrap()
        .clarifications
        .iter()
        .all(|record| record.answer.is_none()));
}

fn add_terminal_run(store: &SessionStore, session_id: &str, outcome: &str) {
    store
        .update_session(session_id, |session| {
            session.clarifications[0].run_id = Some("interrupted-run".to_string());
            let index = session.events.len();
            session.events.push(SessionEvent {
                index,
                kind: "run_started".to_string(),
                details: json!({"run_id":"interrupted-run"}),
                timestamp: None,
            });
            session.events.push(SessionEvent {
                index: index + 1,
                kind: "run_terminal".to_string(),
                details: json!({"run_id":"interrupted-run","outcome":outcome}),
                timestamp: None,
            });
            Ok(())
        })
        .unwrap();
}

#[test]
fn completed_cancelled_and_stopped_operations_never_auto_resume() {
    for outcome in [
        "completed",
        "cancelled_by_user",
        "stopped",
        "provider_continuation_interrupted",
    ] {
        let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
        add_terminal_run(&store, &session_id, outcome);
        assert!(recover_question_conversation(&store, &session_id, "beta")
            .unwrap()
            .is_none());
        assert!(complete_question_recovery(
            &store,
            &session_id,
            invocation_id,
            QuestionFormOutcome::Answered(vec![QuestionAnswer {
                answer: "beta".to_string(),
                source: QuestionAnswerSource::Option
            }])
        )
        .is_err());
        assert!(store.load(&session_id).unwrap().clarifications[0]
            .answer
            .is_none());
    }
}

#[test]
fn waiting_run_recovers_after_reloading_the_session() {
    let (_directory, store, session_id, _, _) = recoverable_question_fixture();
    add_terminal_run(&store, &session_id, "waiting_for_user_input");
    let restored = SessionStore::at_dir(store.sessions_dir().to_path_buf());
    assert!(matches!(
        recover_question_conversation(&restored, &session_id, "beta").unwrap(),
        Some(QuestionRecoveryEffect::ContinuePlan { .. })
    ));
}

#[test]
fn later_run_metadata_cannot_identify_an_older_legacy_question() {
    let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
    store
        .record_event(
            &session_id,
            "run_started",
            json!({"run_id":"later-unrelated-run"}),
        )
        .unwrap();
    store
        .record_event(
            &session_id,
            "run_terminal",
            json!({"run_id":"later-unrelated-run","outcome":"waiting_for_user_input"}),
        )
        .unwrap();
    let restored = SessionStore::at_dir(store.sessions_dir().to_path_buf());
    let before = serde_json::to_value(restored.load(&session_id).unwrap()).unwrap();
    for input in ["beta", "resume"] {
        assert!(recover_question_conversation(&restored, &session_id, input)
            .unwrap()
            .is_none());
    }
    assert!(complete_question_recovery(
        &restored,
        &session_id,
        invocation_id,
        QuestionFormOutcome::Answered(vec![QuestionAnswer {
            answer: "beta".to_string(),
            source: QuestionAnswerSource::Option,
        }]),
    )
    .is_err());
    assert_eq!(
        serde_json::to_value(restored.load(&session_id).unwrap()).unwrap(),
        before
    );
}

#[test]
fn discussion_preserves_all_blockers_and_has_trusted_human_provenance() {
    let (_directory, store, session_id, invocation_id, plan_id) = recoverable_question_fixture();
    append_second_question(&store, &session_id, invocation_id, true);
    let effect = complete_question_recovery(
        &store,
        &session_id,
        invocation_id,
        QuestionFormOutcome::Discussed("Please explain the tradeoff first".to_string()),
    )
    .unwrap();
    assert!(
        matches!(effect, QuestionRecoveryEffect::ContinueDiscussion {plan_id:id, invocation_id:inv, ..}
        if id == plan_id && inv == invocation_id)
    );
    let session = store.load(&session_id).unwrap();
    assert!(session.has_unresolved_clarification(Some(&plan_id)));
    assert!(session
        .clarifications
        .iter()
        .all(|record| record.answer.is_none()));
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "human_question_discussion_received"));
    assert!(load_continue_plan_effect(&store, &session_id, &plan_id).is_err());
}

#[test]
fn proposal_approval_requires_explicit_recognized_conversation() {
    let (_directory, store, session_id, _, _) = recoverable_question_fixture();
    store
        .update_session(&session_id, |session| {
            session.clarifications[0].proposed_answer = Some("alpha".to_string());
            Ok(())
        })
        .unwrap();
    assert!(
        recover_question_conversation(&store, &session_id, "Maybe later")
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        recover_question_conversation(&store, &session_id, "approve").unwrap(),
        Some(QuestionRecoveryEffect::ContinuePlan { .. })
    ));
    let session = store.load(&session_id).unwrap();
    assert_eq!(session.clarifications[0].answer.as_deref(), Some("alpha"));
    assert_eq!(
        session.clarifications[0].answer_source,
        Some(QuestionAnswerSource::ApprovedProposal)
    );
}

#[test]
fn recovery_opens_the_requested_editor_without_losing_intent() {
    let (_directory, store, session_id, _, _) = recoverable_question_fixture();
    assert!(matches!(
        recover_question_conversation(&store, &session_id, "chat").unwrap(),
        Some(QuestionRecoveryEffect::OpenEditor {
            question_index: None,
            ..
        })
    ));
    assert!(matches!(
        recover_question_conversation(&store, &session_id, "3").unwrap(),
        Some(QuestionRecoveryEffect::OpenEditor {
            question_index: Some(0),
            ..
        })
    ));
    assert!(store.load(&session_id).unwrap().clarifications[0]
        .answer
        .is_none());
}

#[test]
fn exact_text_escape_is_an_answer_but_does_not_approve_a_tool() {
    let (_directory, store, session_id, _, _) = recoverable_question_fixture();
    assert!(matches!(
        recover_question_conversation(&store, &session_id, "text: esc").unwrap(),
        Some(QuestionRecoveryEffect::ContinuePlan { .. })
    ));
    let session = store.load(&session_id).unwrap();
    assert_eq!(session.clarifications[0].answer.as_deref(), Some("esc"));
    assert_eq!(
        session.clarifications[0].answer_source,
        Some(QuestionAnswerSource::Text)
    );
    assert!(session.tool_calls.is_empty());
    assert!(!session
        .events
        .iter()
        .any(|event| event.kind.contains("approval")));
}

#[test]
fn changed_current_plan_and_uncertain_provider_state_never_recover() {
    for changed_plan in [false, true] {
        let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
        store
            .update_session(&session_id, |session| {
                let plan = session.plan.as_mut().unwrap();
                if changed_plan {
                    plan.id = "replacement-plan".to_string();
                } else {
                    plan.outcome = Some("provider_continuation_interrupted".to_string());
                }
                Ok(())
            })
            .unwrap();
        assert!(!matches!(
            recover_question_conversation(&store, &session_id, "beta").unwrap(),
            Some(QuestionRecoveryEffect::ContinuePlan { .. })
        ));
        assert!(complete_question_recovery(
            &store,
            &session_id,
            invocation_id,
            QuestionFormOutcome::Answered(vec![QuestionAnswer {
                answer: "beta".to_string(),
                source: QuestionAnswerSource::Option
            }])
        )
        .is_err());
        assert!(store.load(&session_id).unwrap().clarifications[0]
            .answer
            .is_none());
    }
}

#[test]
fn reserved_choice_labels_do_not_override_the_line_protocol() {
    for (input, expected_editor) in [("chat", Some(None)), ("3", Some(Some(0))), ("esc", None)] {
        let (_directory, store, session_id, _, _) = recoverable_question_fixture();
        store
            .update_session(&session_id, |session| {
                session.clarifications[0].options = vec!["esc".to_string(), "chat".to_string()];
                Ok(())
            })
            .unwrap();
        let effect = recover_question_conversation(&store, &session_id, input)
            .unwrap()
            .unwrap();
        if let Some(expected) = expected_editor {
            assert!(
                matches!(effect, QuestionRecoveryEffect::OpenEditor {question_index, ..} if question_index == expected)
            );
        } else {
            assert!(matches!(effect, QuestionRecoveryEffect::Output(_)));
        }
        assert!(store.load(&session_id).unwrap().clarifications[0]
            .answer
            .is_none());
    }
    let (_directory, store, session_id, _, _) = recoverable_question_fixture();
    store
        .update_session(&session_id, |session| {
            session.clarifications[0].options = vec!["alpha".to_string(), "1".to_string()];
            Ok(())
        })
        .unwrap();
    recover_question_conversation(&store, &session_id, "1").unwrap();
    assert_eq!(
        store.load(&session_id).unwrap().clarifications[0]
            .answer
            .as_deref(),
        Some("alpha")
    );
}

#[test]
fn addressed_set_chat_opens_discussion_and_leaves_drafts_unpersisted() {
    let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
    append_second_question(&store, &session_id, invocation_id, true);
    store
        .update_session(&session_id, |session| {
            for record in &mut session.clarifications {
                record.options = vec!["chat".to_string(), "esc".to_string()];
            }
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        recover_question_conversation(&store, &session_id, "Target: chat").unwrap(),
        Some(QuestionRecoveryEffect::OpenEditor {
            question_index: None,
            ..
        })
    ));
    assert!(store
        .load(&session_id)
        .unwrap()
        .clarifications
        .iter()
        .all(|record| record.answer.is_none()));
}

#[test]
fn committed_answers_are_reported_saved_when_another_operation_blocks_resume() {
    let (_directory, store, session_id, invocation_id, _) = recoverable_question_fixture();
    let other = ToolInvocationId::new();
    append_second_question(&store, &session_id, other, false);
    store
        .update_session(&session_id, |session| {
            session.clarifications[1].run_id = Some("cancelled-other".to_string());
            let index = session.events.len();
            session.events.push(SessionEvent {
                index,
                kind: "run_terminal".to_string(),
                details: json!({"run_id":"cancelled-other","outcome":"cancelled_by_user"}),
                timestamp: None,
            });
            Ok(())
        })
        .unwrap();
    let effect = complete_question_recovery(
        &store,
        &session_id,
        invocation_id,
        QuestionFormOutcome::Answered(vec![QuestionAnswer {
            answer: "beta".to_string(),
            source: QuestionAnswerSource::Option,
        }]),
    )
    .unwrap();
    let QuestionRecoveryEffect::Output(text) = effect else {
        panic!("saved answer must report blocked continuation");
    };
    assert!(text.contains("answers were saved"));
    let session = store.load(&session_id).unwrap();
    assert_eq!(
        session.clarifications[0].status,
        ClarificationStatus::Answered
    );
    assert_eq!(
        session.clarifications[1].status,
        ClarificationStatus::Unresolved
    );
}

#[test]
fn committed_question_answers_cannot_resume_or_reopen_a_replacement_plan() {
    for has_pending_question in [false, true] {
        let (_directory, store, session_id, invocation_id, plan_id) =
            recoverable_question_fixture();
        let original_record = store.load(&session_id).unwrap().clarifications[0].clone();
        let persisted_plan_id = crate::session::persist_recovered_form_outcome(
            &store,
            &session_id,
            invocation_id,
            &QuestionFormOutcome::Answered(vec![QuestionAnswer {
                answer: "beta".to_string(),
                source: QuestionAnswerSource::Option,
            }]),
        )
        .unwrap();
        assert_eq!(persisted_plan_id, plan_id);
        store
            .update_session(&session_id, |session| {
                let plan = session.plan.as_mut().unwrap();
                plan.id = uuid::Uuid::new_v4().to_string();
                plan.goal = "different approved work".to_string();
                if has_pending_question {
                    let mut record = original_record.clone();
                    record.invocation_id = ToolInvocationId::new();
                    record.plan_id = Some(plan.id.clone());
                    record.question_event_index = session.events.len();
                    session.events.push(SessionEvent {
                        index: record.question_event_index,
                        kind: "question_required".to_string(),
                        details: json!({"invocation_id":record.invocation_id,"plan_id":plan.id,"question":record.question,"options":record.options}),
                        timestamp: None,
                    });
                    session.clarifications.push(record);
                }
                Ok(())
            })
            .unwrap();
        let before = serde_json::to_value(store.load(&session_id).unwrap()).unwrap();
        assert!(
            continue_after_answers(&store, &session_id, &persisted_plan_id)
                .unwrap_err()
                .contains("no longer current")
        );
        assert_eq!(
            serde_json::to_value(store.load(&session_id).unwrap()).unwrap(),
            before
        );
    }
}

/// T081 AC-5: `/continue` explains that an interrupted plan was cleared.
#[test]
fn continue_reports_a_cleared_interrupted_plan() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut session = store.create_session();
    let mut plan = crate::session::Plan::new(
        "review repo",
        vec![crate::session::PlanStep {
            description: "inspect".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    );
    plan.approve();
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    assert!(crate::agent::r#loop::clear_interrupted_plan_in_session(
        &mut session,
        "tool_execution_failed",
        None
    ));
    store.save(&mut session).unwrap();
    let error = load_continue_plan_effect(&store, &session.id, &plan_id)
        .expect_err("cleared plan cannot continue");
    assert!(error.contains("was cleared"), "{error}");
    assert!(error.contains("tool_execution_failed"), "{error}");
}
