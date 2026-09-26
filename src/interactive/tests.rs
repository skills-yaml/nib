use super::*;
use crate::config::{load_nib_config_full, save_nib_config_full, NibConfig, ProviderEntry};
use std::collections::HashSet;
use std::process::Command;
use tempfile::tempdir;

pub(crate) fn git_repository() -> tempfile::TempDir {
    let repository = tempdir().expect("repository");
    let run = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(repository.path())
            .args(args)
            .output()
            .expect("git fixture command");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init", "-q"]);
    std::fs::write(repository.path().join("tracked.txt"), "original\n").expect("tracked fixture");
    run(&["add", "tracked.txt"]);
    run(&[
        "-c",
        "user.name=nib tests",
        "-c",
        "user.email=nib@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    repository
}

pub(crate) fn recoverable_question_fixture() -> (
    tempfile::TempDir,
    SessionStore,
    String,
    crate::tools::ToolInvocationId,
    String,
) {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let mut session = store.try_create_session().expect("session");
    let mut plan = crate::session::Plan::new(
        "finish the exact plan",
        vec![crate::session::PlanStep {
            description: "use the clarification".to_string(),
            status: "Blocked".to_string(),
            outcome: None,
            attempts: 1,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    );
    plan.approve();
    let plan_id = plan.id.clone();
    let invocation_id = crate::tools::ToolInvocationId::new();
    session.plan = Some(plan);
    session.events.push(SessionEvent {
        index: 0,
        kind: "question_required".to_string(),
        details: serde_json::json!({
            "invocation_id": invocation_id,
            "question": "Which target?",
            "options": ["alpha", "beta"],
        }),
        timestamp: Some(Utc::now()),
    });
    session
        .clarifications
        .push(crate::session::ClarificationRecord {
            invocation_id,
            plan_id: Some(plan_id.clone()),
            question: "Which target?".to_string(),
            proposed_answer: None,
            options: vec!["alpha".to_string(), "beta".to_string()],
            dependent_paths: Vec::new(),
            status: crate::session::ClarificationStatus::Unresolved,
            answer: None,
            question_event_index: 0,
            answer_message_index: None,
            answer_event_index: None,
            reason: Some("left unanswered".to_string()),
            outcome: Some("left_unanswered".to_string()),
        });
    store.save(&mut session).expect("recoverable question");
    (directory, store, session.id, invocation_id, plan_id)
}

#[path = "test_part_0.rs"]
mod test_part_0;
#[path = "test_part_1.rs"]
mod test_part_1;
