use clap::Args;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use crate::auth::run_auth_wizard;
use crate::console::{ConsoleApprovalHandler, ConsoleInput, ConsoleQuestionHandler};
use nib::config::load_nib_config_full;
use nib::interactive::{
    display_stream_event, execute_interactive_command, parse_interactive_command, resolve_session,
    set_active_model, InteractiveEffect, ModelSelection, SessionResolution, StreamDisplay,
};
use nib::session::SessionStore;

#[derive(Args, Debug)]
pub struct ChatArgs {
    #[arg(short, long)]
    pub session: Option<String>,

    /// Run the auth wizard before starting chat (same as `nib auth`)
    #[arg(long)]
    pub auth: bool,
}

pub fn run_chat(args: &ChatArgs) -> Result<(), String> {
    run_chat_with_input(args, io::BufReader::new(io::stdin()))
}

fn run_chat_with_input(
    args: &ChatArgs,
    reader: impl BufRead + Send + 'static,
) -> Result<(), String> {
    let input = ConsoleInput::new(reader);
    let project = std::env::current_dir()
        .map_err(|error| format!("failed to resolve the current project directory: {error}"))?;
    let mut cfg = load_nib_config_full(&project)
        .map_err(|error| error.to_string())?
        .llm;

    if args.auth || cfg.providers.is_empty() {
        // Run wizard (it may prompt for multiple)
        run_auth_wizard()?;
        cfg = load_nib_config_full(&project)
            .map_err(|error| error.to_string())?
            .llm;
    }

    let active = cfg.get_active_provider();
    let session_store = SessionStore::for_project(&project)?;

    // Resolve or create session
    let resolution = resolve_session(&session_store, args.session.as_deref())?;
    match &resolution {
        SessionResolution::Created(_) => {}
        SessionResolution::Resumed(id) => println!("Resumed session {id}."),
        SessionResolution::RequestedMissing { requested, .. } => {
            println!("Session {requested} not found; creating a new session.")
        }
    }
    let mut sid = resolution.session_id().to_string();

    println!("\nnib chat  |  session: {sid}  |  provider: {active}");
    println!("Type message. /model to change (list/select or name). /help for commands. Ctrl+C to exit.\n");

    // Show recent history (last few)
    if let Some(sess) = session_store
        .load_result(&sid)
        .map_err(|error| format!("failed to load session history: {error}"))?
    {
        for msg in sess.messages.iter().rev().take(6).rev() {
            let prefix = &msg.role;
            let short = if msg.content.chars().count() > 200 {
                format!("{}...", msg.content.chars().take(200).collect::<String>())
            } else {
                msg.content.clone()
            };
            println!("{prefix}: {short}");
        }
    }

    // Main REPL
    loop {
        print!("\nYou> ");
        io::stdout().flush().unwrap();

        let line = match input.read_line_blocking() {
            Ok(line) => line,
            Err(error) if error.contains("input closed") => break,
            Err(error) => return Err(error),
        };
        let command_input = line.trim();
        if command_input.is_empty() {
            continue;
        }

        match parse_interactive_command(command_input) {
            Ok(Some(command)) => {
                match execute_interactive_command(command, &project, &session_store, &sid) {
                    Ok(InteractiveEffect::Quit) => {
                        println!(
                            "Goodbye. Session saved to {}",
                            session_store
                                .sessions_dir()
                                .join(format!("{sid}.json"))
                                .display()
                        );
                        break;
                    }
                    Ok(InteractiveEffect::Output(output)) => println!("{output}"),
                    Ok(InteractiveEffect::SessionChanged { session_id, output }) => {
                        sid = session_id;
                        println!("{output}");
                    }
                    Ok(InteractiveEffect::SelectModel(selection)) => {
                        if let Some(model) = select_model_from_console(&input, &selection)? {
                            match set_active_model(&project, &model) {
                                Ok(output) => println!("{output}"),
                                Err(error) => eprintln!("{error}"),
                            }
                        }
                    }
                    Err(error) => println!("{error}"),
                }
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                println!("{error}");
                continue;
            }
        }

        println!("Thinking...");

        match execute_agent_step(&project, &sid, command_input, &input) {
            Ok(()) => {}
            Err(error) => println!("{error}"),
        }
    }

    Ok(())
}

fn select_model_from_console(
    input: &ConsoleInput,
    selection: &ModelSelection,
) -> Result<Option<String>, String> {
    if selection.available.is_empty() {
        println!(
            "No predefined list for {}. Type the full model name.",
            selection.provider
        );
        print!("Model name: ");
    } else {
        println!("\nAvailable models for {}:", selection.provider);
        for (index, model) in selection.available.iter().enumerate() {
            let marker = if model == &selection.current {
                " (current)"
            } else {
                ""
            };
            println!("  {}. {}{}", index + 1, model, marker);
        }
        print!("Selection (number or exact model): ");
    }
    io::stdout().flush().map_err(|error| error.to_string())?;
    let choice = input.read_line_blocking()?;
    let choice = choice.trim();
    if choice.is_empty() {
        return Ok(None);
    }
    if let Ok(index) = choice.parse::<usize>() {
        if index > 0 {
            if let Some(model) = selection.available.get(index - 1) {
                return Ok(Some(model.clone()));
            }
        }
        println!("Invalid model number.");
        return Ok(None);
    }
    Ok(Some(choice.to_string()))
}

fn execute_agent_step(
    project: &Path,
    session_id: &str,
    goal: &str,
    input: &ConsoleInput,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to initialize the async runtime: {error}"))?;

    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(100);
    let renderer = std::thread::Builder::new()
        .name("nib-chat-stream".to_string())
        .spawn(move || {
            while let Some(event) = stream_rx.blocking_recv() {
                let Some(display) = display_stream_event(event) else {
                    continue;
                };
                match display {
                    StreamDisplay::Content(content) => {
                        print!("{content}");
                        let _ = io::stdout().flush();
                    }
                    StreamDisplay::Status(status) => println!("\n{status}"),
                }
            }
        })
        .map_err(|error| format!("failed to initialize chat stream rendering: {error}"))?;

    let loop_cfg = nib::agent::AgentLoopConfig {
        max_steps: 0,
        mode: "execute".to_string(),
        provider: None,
        auto_approve: false,
        approval_handler: Some(Arc::new(ConsoleApprovalHandler::new(input.clone()))),
        question_handler: Some(Arc::new(ConsoleQuestionHandler::new(input.clone()))),
        stream_tx: Some(stream_tx),
        ..Default::default()
    };

    let result = rt.block_on(nib::agent::run_agent_loop(
        project.to_path_buf(),
        session_id,
        goal,
        loop_cfg,
    ));

    renderer
        .join()
        .map_err(|_| "chat stream renderer panicked".to_string())?;

    result.and_then(|summary| {
        if summary.outcome == "waiting_for_user_input" {
            Err(format!(
                "question input was unavailable; session {session_id} was reconciled without continuing"
            ))
        } else if summary.is_failure() && summary.failure.is_some() {
            // The agent emits its structured failure on the lifecycle stream after
            // reconciliation, so chat has already rendered the single safe report.
            Ok(())
        } else if summary.is_failure() {
            Err(summary.user_failure_report().unwrap_or_else(|| {
                format!("Agent run failed: {}\nSession: {session_id}", summary.outcome)
            }))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib::config::{load_nib_config_full, save_nib_config_full, NibConfig};
    use serial_test::serial;
    use std::ffi::OsString;
    use std::io::Cursor;
    use std::path::PathBuf;
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

    fn restore_env(name: &str, value: Option<OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    fn save_mock_config(project: &Path) {
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        config.skills.enabled = false;
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        save_nib_config_full(project, &mut config).expect("mock config");
    }

    #[test]
    #[serial]
    fn chat_routes_commands_and_persists_model_and_mcp_changes() {
        let project = tempdir().expect("project");
        let global_skills = tempdir().expect("global skills");
        save_mock_config(project.path());
        let previous_skills_dir = std::env::var_os("NIB_SKILLS_DIR");
        std::env::set_var("NIB_SKILLS_DIR", global_skills.path());
        let _cwd = CurrentDirGuard::enter(project.path());

        let commands = concat!(
            "\n",
            "/help\n",
            "/providers\n",
            "/session\n",
            "/clear\n",
            "/model\n",
            "2\n",
            "/model\n",
            "1\n",
            "/model custom-mock\n",
            "/skills list\n",
            "/skills invalid\n",
            "/mcp list\n",
            "/mcp add local echo --stdio\n",
            "/mcp list\n",
            "/mcp remove local\n",
            "/mcp invalid\n",
            "/unknown\n",
            "/quit\n"
        );
        run_chat_with_input(
            &ChatArgs {
                session: Some("missing-session".to_string()),
                auth: false,
            },
            Cursor::new(commands.as_bytes()),
        )
        .expect("scripted chat");

        let config = load_nib_config_full(project.path()).expect("updated config");
        assert_eq!(config.llm.providers["mock"].model, "custom-mock");
        assert!(config.mcp.servers.is_empty());
        restore_env("NIB_SKILLS_DIR", previous_skills_dir);
    }

    #[test]
    #[serial]
    fn chat_accepts_exact_model_outside_provider_suggestions_without_mutating_override() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("openai".to_string(), "gpt-5.6-sol".to_string(), None);
        config.llm.providers.get_mut("openai").unwrap().models =
            Some(vec!["gateway/reviewed".to_string()]);
        config.skills.enabled = false;
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        save_nib_config_full(project.path(), &mut config).expect("OpenAI config");
        let _cwd = CurrentDirGuard::enter(project.path());

        run_chat_with_input(
            &ChatArgs {
                session: None,
                auth: false,
            },
            Cursor::new(b"/model gateway/future-model\n/quit\n"),
        )
        .expect("scripted exact model selection");

        let config = load_nib_config_full(project.path()).expect("updated config");
        assert_eq!(config.llm.providers["openai"].model, "gateway/future-model");
        assert_eq!(
            config.llm.providers["openai"].models.as_deref(),
            Some(["gateway/reviewed".to_string()].as_slice())
        );
    }

    #[test]
    #[serial]
    fn chat_resumes_existing_session_and_renders_bounded_history() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        store
            .try_append_message(&session.id, "user", &"x".repeat(240))
            .expect("long history message");
        store
            .try_append_message(&session.id, "assistant", "ready")
            .expect("assistant history message");
        let _cwd = CurrentDirGuard::enter(project.path());
        run_chat_with_input(
            &ChatArgs {
                session: Some(session.id),
                auth: false,
            },
            Cursor::new(b"/quit\n"),
        )
        .expect("resume chat");
    }

    #[test]
    #[serial]
    fn chat_shares_approval_and_question_input_without_deadlocking() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let _cwd = CurrentDirGuard::enter(project.path());

        run_chat_with_input(
            &ChatArgs {
                session: None,
                auth: false,
            },
            Cursor::new(b"ask a question before continuing\ny\n2\n/quit\n".to_vec()),
        )
        .expect("question chat");

        let store = SessionStore::for_project(project.path()).expect("session store");
        let session_id = store
            .list_result()
            .expect("sessions")
            .into_iter()
            .next()
            .expect("chat session");
        let session = store
            .load_result(&session_id)
            .expect("load session")
            .expect("chat session state");
        assert!(session.messages.iter().any(|message| {
            message.role == "tool" && message.content.contains("\"answer\":\"full\"")
        }));
    }

    #[test]
    #[serial]
    fn chat_reconciles_closed_question_input_without_a_role_violation() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let _cwd = CurrentDirGuard::enter(project.path());

        run_chat_with_input(
            &ChatArgs {
                session: None,
                auth: false,
            },
            Cursor::new(b"ask a question before continuing\ny\n".to_vec()),
        )
        .expect("closed chat input exits after reconciliation");

        let store = SessionStore::for_project(project.path()).expect("session store");
        let session_id = store
            .list_result()
            .expect("sessions")
            .into_iter()
            .next()
            .expect("chat session");
        let session = store
            .load_result(&session_id)
            .expect("load session")
            .expect("chat session");
        session
            .validate_message_sequence()
            .expect("role-safe reconciled transcript");
        assert!(session.events.iter().any(|event| {
            event.kind == "reconciliation" && event.details["outcome"] == "waiting_for_user_input"
        }));
        let question = session
            .tool_calls
            .iter()
            .find(|record| record.tool_name.as_deref() == Some("ask_question"))
            .expect("question audit");
        assert!(question
            .error
            .as_deref()
            .is_some_and(|error| error.contains("console input closed")));
    }
}
