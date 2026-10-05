use super::*;
use crate::config::{
    save_config, save_nib_config_full, LlmConfig, NibConfig, ProfileConfig, ProfilesConfig,
    ProviderEntry,
};
use crate::interactive::{bottom_scroll_for_wrap, execute_interactive_command, MAX_DRAFT_HISTORY};
use ratatui::{backend::TestBackend, Terminal};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::tempdir;

static TEST_TERMINAL_RESTORE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn record_test_terminal_restore() -> io::Result<()> {
    TEST_TERMINAL_RESTORE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

fn buffer_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            let mut row = String::new();
            for x in 0..buffer.area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            row
        })
        .collect()
}

fn row_index_containing(rows: &[String], needle: &str) -> Option<usize> {
    rows.iter().position(|row| row.contains(needle))
}

fn approval_request(
    call: ToolCall,
    level: PermissionLevel,
    reply: oneshot::Sender<ApprovalDecision>,
) -> TuiApprovalRequest {
    let context = ApprovalContext::compatibility(&call, level);
    TuiApprovalRequest {
        call: call.clone(),
        level,
        context,
        selected_option: usize::from(call.tool_name != "run_terminal"),
        typed: String::new(),
        reason_draft: None,
        details_open: false,
        detail_offset: 0,
        error: None,
        reply,
    }
}

fn recoverable_question_session() -> (
    tempfile::TempDir,
    SessionStore,
    String,
    crate::tools::ToolInvocationId,
) {
    let directory = tempdir().expect("session directory");
    let store = SessionStore::at_dir(directory.path().join("sessions"));
    let mut session = store.try_create_session().expect("session");
    let plan = crate::session::Plan::new(
        "resume after answering",
        vec![crate::session::PlanStep {
            description: "use answer".to_string(),
            status: "Blocked".to_string(),
            outcome: None,
            attempts: 1,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    );
    let plan_id = plan.id.clone();
    let invocation_id = crate::tools::ToolInvocationId::new();
    session.plan = Some(plan);
    session.events.push(crate::session::SessionEvent {
        index: 0,
        kind: "question_required".to_string(),
        details: json!({
            "invocation_id": invocation_id,
            "question": "Which target?",
            "options": ["alpha", "beta"],
        }),
        timestamp: Some(chrono::Utc::now()),
    });
    session
        .clarifications
        .push(crate::session::ClarificationRecord {
            invocation_id,
            plan_id: Some(plan_id),
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
    (directory, store, session.id, invocation_id)
}

fn mock_config() -> LlmConfig {
    LlmConfig {
        active_provider: Some("mock".to_string()),
        providers: HashMap::from([(
            "mock".to_string(),
            ProviderEntry {
                model: "mock-model".to_string(),
                api_key: None,
                api_keys: Vec::new(),
                base_url: None,
                ..ProviderEntry::default()
            },
        )]),
        ..Default::default()
    }
}

mod lifecycle;
mod render;
mod session;

fn form_answer(answer: String, source: crate::interactive::QuestionAnswerSource) -> crate::interactive::QuestionFormOutcome {
    crate::interactive::QuestionFormOutcome::Answered(vec![crate::interactive::QuestionAnswer { answer, source }])
}

mod question_form;
