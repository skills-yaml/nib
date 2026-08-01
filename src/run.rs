use clap::Args;
use std::sync::Arc;

use crate::console::{ConsoleApprovalHandler, ConsoleInput, ConsoleQuestionHandler};

#[derive(Args, Debug)]
pub struct RunArgs {
    pub goal: String,

    #[arg(short, long)]
    pub session: Option<String>,

    /// Override the configured agent.max_turns value (0 uses the configured value)
    #[arg(long, default_value_t = 0)]
    pub max_steps: u32,

    #[arg(long, default_value = "execute")]
    pub mode: String,

    #[arg(short, long)]
    pub provider: Option<String>,

    #[arg(short, long)]
    pub model: Option<String>,

    #[arg(short, long)]
    pub yes: bool,
}

pub fn run_agent(args: &RunArgs) -> Result<(), String> {
    run_agent_with_input(args, ConsoleInput::stdin())
}

fn run_agent_with_input(args: &RunArgs, input: ConsoleInput) -> Result<(), String> {
    let project = std::env::current_dir()
        .map_err(|error| format!("failed to resolve the current project directory: {error}"))?;
    println!("nib run: {}", args.goal);

    let session_store = nib::session::SessionStore::for_project(&project)?;
    let sid = if let Some(s) = &args.session {
        s.clone()
    } else {
        session_store
            .try_create_session()
            .map_err(|error| format!("failed to create session: {error}"))?
            .id
    };

    println!(
        "session={} mode={} max_steps={}",
        sid, args.mode, args.max_steps
    );

    // We use the Rust agent loop directly
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to initialize the async runtime: {error}"))?;

    let loop_cfg = nib::agent::AgentLoopConfig {
        max_steps: args.max_steps,
        mode: args.mode.clone(),
        provider: args.provider.clone(),
        model: args.model.clone(),
        auto_approve: args.yes,
        approval_handler: Some(Arc::new(ConsoleApprovalHandler::new(input.clone()))),
        question_handler: Some(Arc::new(ConsoleQuestionHandler::new(input))),
        ..Default::default()
    };

    let result = rt.block_on(nib::agent::run_agent_loop(
        project.clone(),
        &sid,
        &args.goal,
        loop_cfg,
    ));

    match result {
        Ok(summary) => {
            if summary.outcome == "waiting_for_user_input" {
                return Err(format!(
                    "agent stopped because console question input was unavailable; session {} was reconciled without continuing",
                    sid
                ));
            }
            if summary.is_failure() {
                return Err(format!(
                    "agent run failed for session {}: {}",
                    sid, summary.outcome
                ));
            }
            println!("[green]Agent run completed for session {}[/green]", sid);
            if let Some(msg) = summary.last_message {
                println!("Last: {}", msg.chars().take(300).collect::<String>());
            }
            Ok(())
        }
        Err(error) => Err(format!("failed to launch agent: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib::config::{save_nib_config_full, NibConfig};
    use nib::session::SessionStore;
    use serial_test::serial;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    struct CurrentDirGuard(PathBuf);

    impl CurrentDirGuard {
        fn enter(path: &Path) -> Self {
            let original = std::env::current_dir().expect("current directory");
            std::env::set_current_dir(path).expect("enter project");
            Self(original)
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("restore current directory");
        }
    }

    #[test]
    #[serial]
    fn run_agent_completes_with_mock_and_surfaces_provider_errors() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        config.skills.enabled = false;
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        save_nib_config_full(project.path(), &mut config).expect("mock config");
        let _cwd = CurrentDirGuard::enter(project.path());

        run_agent(&RunArgs {
            goal: "list project files".to_string(),
            session: None,
            max_steps: 4,
            mode: "execute".to_string(),
            provider: Some("mock".to_string()),
            model: Some("mock-model".to_string()),
            yes: true,
        })
        .expect("mock agent run");

        let store = SessionStore::for_project(project.path()).expect("session store");
        let session_ids = store.list();
        assert_eq!(session_ids.len(), 1);
        let session = store
            .load_result(&session_ids[0])
            .expect("load session")
            .expect("created session");
        assert!(session
            .messages
            .iter()
            .any(|message| message.role == "assistant"));

        let error = run_agent(&RunArgs {
            goal: "fail provider selection".to_string(),
            session: Some(session.id),
            max_steps: 1,
            mode: "execute".to_string(),
            provider: Some("not-a-provider".to_string()),
            model: None,
            yes: true,
        })
        .expect_err("unsupported provider");
        assert!(error.contains("unsupported LLM provider"));
    }

    #[test]
    #[serial]
    fn run_agent_routes_console_question_answers_back_into_the_same_session() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        config.skills.enabled = false;
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        save_nib_config_full(project.path(), &mut config).expect("mock config");
        let _cwd = CurrentDirGuard::enter(project.path());

        run_agent_with_input(
            &RunArgs {
                goal: "ask a question before continuing".to_string(),
                session: None,
                max_steps: 5,
                mode: "execute".to_string(),
                provider: Some("mock".to_string()),
                model: Some("mock-model".to_string()),
                yes: true,
            },
            ConsoleInput::new(Cursor::new(b"2\n".to_vec())),
        )
        .expect("question run");

        let store = SessionStore::for_project(project.path()).expect("session store");
        let session_ids = store.list_result().expect("sessions");
        assert_eq!(session_ids.len(), 1);
        let session = store
            .load_result(&session_ids[0])
            .expect("load session")
            .expect("question session");
        assert!(session.messages.iter().any(|message| {
            message.role == "tool" && message.content.contains("\"answer\":\"full\"")
        }));
    }

    #[test]
    #[serial]
    fn run_agent_reports_closed_question_input_after_reconciliation() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        config.skills.enabled = false;
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        save_nib_config_full(project.path(), &mut config).expect("mock config");
        let _cwd = CurrentDirGuard::enter(project.path());

        let error = run_agent_with_input(
            &RunArgs {
                goal: "ask a question before continuing".to_string(),
                session: None,
                max_steps: 5,
                mode: "execute".to_string(),
                provider: Some("mock".to_string()),
                model: Some("mock-model".to_string()),
                yes: true,
            },
            ConsoleInput::new(Cursor::new(Vec::<u8>::new())),
        )
        .expect_err("closed question input must be visible to the caller");
        assert!(error.contains("question input was unavailable"));

        let store = SessionStore::for_project(project.path()).expect("session store");
        let session_id = store
            .list_result()
            .expect("sessions")
            .into_iter()
            .next()
            .expect("question session");
        let session = store
            .load_result(&session_id)
            .expect("load session")
            .expect("question session");
        assert!(session.events.iter().any(|event| {
            event.kind == "reconciliation" && event.details["outcome"] == "waiting_for_user_input"
        }));
        assert_eq!(session.tool_calls.len(), 1, "{:#?}", session.tool_calls);
        let question = &session.tool_calls[0];
        assert_eq!(question.tool_name.as_deref(), Some("ask_question"));
        assert_eq!(question.result.as_ref().unwrap()["success"], false);
        assert!(question
            .error
            .as_deref()
            .is_some_and(|error| error.contains("console input closed")));
        assert!(session.tool_calls.iter().all(|record| {
            !matches!(
                record.tool_name.as_deref(),
                Some("apply_patch" | "run_terminal")
            )
        }));
    }
}
