use clap::Args;
use std::sync::Arc;

use crate::console::{ConsoleApprovalHandler, ConsoleInput, ConsoleQuestionHandler};

const MAX_RUN_GOAL_BYTES: usize = 20_000;
const MAX_RUN_SUMMARY_BYTES: usize = 512;

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
    let config = nib::config::load_nib_config_full(&project).map_err(|error| error.to_string())?;
    if args.goal.len() > MAX_RUN_GOAL_BYTES {
        return Err(format!(
            "agent goal exceeds the {MAX_RUN_GOAL_BYTES}-byte limit"
        ));
    }
    if let Some(session_id) = args.session.as_deref() {
        config.validate_public_session_id(session_id)?;
    }
    let sensitive_values = config.public_session_sensitive_values();
    println!("nib run: starting");

    let session_store = nib::session::SessionStore::for_project(&project)?;
    let sid = if let Some(s) = &args.session {
        s.clone()
    } else {
        session_store
            .try_create_session()
            .map_err(|error| format!("failed to create session: {error}"))?
            .id
    };

    let mode = nib::interactive::bounded_public_text(&args.mode, &sensitive_values, 32, false);
    println!("session={} mode={} max_steps={}", sid, mode, args.max_steps);

    // We use the Rust agent loop directly
    let rt = nib::agent::build_agent_runtime("failed to initialize the async runtime")?;

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

    let worker_project = project.clone();
    let worker_session_id = sid.clone();
    let worker_goal = args.goal.clone();
    let result = nib::agent::block_on_agent_runtime_worker(
        &rt,
        async move {
            nib::agent::run_agent_loop(worker_project, &worker_session_id, &worker_goal, loop_cfg)
                .await
        },
        "agent runtime worker",
    )?;

    match result {
        Ok(summary) => {
            if summary.outcome == "waiting_for_user_input" {
                return Err(format!(
                    "agent stopped because console question input was unavailable; session {} was reconciled without continuing",
                    sid
                ));
            }
            if summary.is_failure() {
                return Err(summary.user_failure_report().unwrap_or_else(|| {
                    format!("Agent run failed: {}\nSession: {sid}", summary.outcome)
                }));
            }
            println!("Agent run completed for session {}", sid);
            if let Some(msg) = summary.last_message {
                let msg = nib::interactive::bounded_public_text(
                    &msg,
                    &sensitive_values,
                    MAX_RUN_SUMMARY_BYTES,
                    false,
                );
                println!("Last: {msg}");
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
        let message_count = session.messages.len();
        let session_id = session.id.clone();

        let error = run_agent(&RunArgs {
            goal: "fail provider selection".to_string(),
            session: Some(session_id.clone()),
            max_steps: 1,
            mode: "execute".to_string(),
            provider: Some("not-a-provider".to_string()),
            model: None,
            yes: true,
        })
        .expect_err("unsupported provider");
        assert!(error.contains("LLM request failed [LLM-CONFIG]"), "{error}");
        assert!(error.contains("Provider: not-a-provider"), "{error}");
        assert!(error.contains("Retry: not attempted"), "{error}");
        assert!(
            error.contains("Action: Run `nib config validate`"),
            "{error}"
        );
        assert!(error.contains(&format!("Session: {session_id}")), "{error}");
        assert!(!error.contains("unsupported LLM provider"), "{error}");
        assert_eq!(
            store
                .load(&session_id)
                .expect("configuration failure session")
                .messages
                .len(),
            message_count,
            "configuration failures must not become assistant content"
        );
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

    #[test]
    #[serial]
    fn run_rejects_a_credential_derived_session_before_persistence() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        config.llm.providers.insert(
            "inactive-openai".to_string(),
            nib::config::ProviderEntry {
                model: "fixture".to_string(),
                api_key: Some("private-session-key".to_string()),
                ..Default::default()
            },
        );
        config.skills.enabled = false;
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        save_nib_config_full(project.path(), &mut config).expect("mock config");
        let _cwd = CurrentDirGuard::enter(project.path());

        let error = run_agent(&RunArgs {
            goal: "list project files".to_string(),
            session: Some("private-session-key".to_string()),
            max_steps: 1,
            mode: "execute".to_string(),
            provider: Some("mock".to_string()),
            model: Some("mock-model".to_string()),
            yes: true,
        })
        .expect_err("credential-derived session id");

        assert_eq!(
            error,
            "session identifier conflicts with configured sensitive data"
        );
        assert!(!error.contains("private-session-key"));
        assert!(SessionStore::for_project(project.path())
            .expect("session store")
            .list_result()
            .expect("session list")
            .is_empty());
    }

    #[test]
    #[serial]
    fn run_rejects_an_oversized_goal_before_output_or_persistence() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        save_nib_config_full(project.path(), &mut config).expect("mock config");
        let _cwd = CurrentDirGuard::enter(project.path());

        let error = run_agent(&RunArgs {
            goal: "x".repeat(MAX_RUN_GOAL_BYTES + 1),
            session: None,
            max_steps: 1,
            mode: "execute".to_string(),
            provider: Some("mock".to_string()),
            model: Some("mock-model".to_string()),
            yes: true,
        })
        .expect_err("oversized goal");

        assert_eq!(
            error,
            format!("agent goal exceeds the {MAX_RUN_GOAL_BYTES}-byte limit")
        );
        assert!(SessionStore::for_project(project.path())
            .expect("session store")
            .list_result()
            .expect("session list")
            .is_empty());
    }
}
