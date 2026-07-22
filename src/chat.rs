use clap::Args;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use crate::auth::run_auth_wizard;
use crate::console::{ConsoleApprovalHandler, ConsoleInput, ConsoleQuestionHandler};
use nib::config::{load_nib_config_full, update_nib_config};
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
    let mut sid = if let Some(s) = &args.session {
        if session_store
            .load_result(s)
            .map_err(|error| format!("failed to load requested session {s}: {error}"))?
            .is_some()
        {
            println!("[dim]Resumed session {s}[/dim]");
            s.clone()
        } else {
            println!("[yellow]Session {s} not found, creating new.[/yellow]");
            let new_s = session_store
                .try_create_session()
                .map_err(|error| format!("failed to create session: {error}"))?;
            new_s.id
        }
    } else {
        let new_s = session_store
            .try_create_session()
            .map_err(|error| format!("failed to create session: {error}"))?;
        new_s.id
    };

    println!("\n\x1b[1;32mnib chat\x1b[0m  |  session: {sid}  |  provider: {active}");
    println!("[dim]Type message. /model to change (list/select or name). /help for commands. Ctrl+C to exit.\x1b[0m\n");

    // Show recent history (last few)
    if let Some(sess) = session_store
        .load_result(&sid)
        .map_err(|error| format!("failed to load session history: {error}"))?
    {
        for msg in sess.messages.iter().rev().take(6).rev() {
            let color = if msg.role == "user" {
                "\x1b[36m"
            } else {
                "\x1b[32m"
            };
            let prefix = format!("{}{}\x1b[0m", color, msg.role);
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
        print!("\n\x1b[1;36mYou\x1b[0m> ");
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

        if let Some(cmdline) = command_input.strip_prefix('/') {
            let parts: Vec<&str> = cmdline.splitn(2, ' ').collect();
            let command = parts[0].to_lowercase();
            let arg = parts.get(1).map(|s| s.trim().to_string());

            match command.as_str() {
                "q" | "quit" | "exit" => {
                    println!(
                        "[dim]Goodbye. Session saved to {}[/dim]",
                        session_store
                            .sessions_dir()
                            .join(format!("{sid}.json"))
                            .display()
                    );
                    break;
                }
                "help" => {
                    println!(
                        r#"[dim]
Commands (chat):
  /model           List models for current provider and select by number or type exact name
  /model <name>    Directly set model (must be valid for the active provider)
                   Model always belongs to the active provider.
  /providers       List configured providers
  /session         Show current session ID
  /clear           Start fresh session (new history)
  /skills [cmd]    Manage skills (list, install <url>, remove <name>)
  /mcp [cmd]       Manage MCP servers (list, add <name> <cmd> [args], remove <name>)
  /help            This help
  /quit /exit /q   Exit
[/dim]"#
                    );
                }
                "providers" => {
                    let provs = &cfg.providers;
                    println!("[bold]Configured providers:[/bold]");
                    if provs.is_empty() {
                        println!("  (none - using mock)");
                    }
                    for (name, entry) in provs {
                        let active_mark = if name == &cfg.get_active_provider() {
                            " (active)"
                        } else {
                            ""
                        };
                        println!("  - {}: {}{}", name, entry.model, active_mark);
                    }
                }
                "session" => {
                    println!("Current session: \x1b[1m{sid}\x1b[0m");
                }
                "clear" => {
                    let new_s = session_store
                        .try_create_session()
                        .map_err(|error| format!("failed to create session: {error}"))?;
                    sid = new_s.id;
                    println!("[yellow]Started fresh session {sid}.[/yellow]");
                }
                "model" => {
                    let provider_name = cfg.get_active_provider();
                    let available = cfg.get_available_models(None);

                    let new_model = if let Some(a) = arg {
                        // direct name
                        a
                    } else {
                        // list + select
                        if available.is_empty() {
                            println!("[yellow]No predefined list for {}. Type the full model name.[/yellow]", provider_name);
                            print!("Model name: ");
                            io::stdout().flush().unwrap();
                            let m = input.read_line_blocking()?;
                            m.trim().to_string()
                        } else {
                            println!("\n[bold]Available models for {}:[/bold]", provider_name);
                            let current_model = cfg
                                .get_provider(None)
                                .map(|e| e.model.clone())
                                .unwrap_or_default();
                            for (i, m) in available.iter().enumerate() {
                                let mark = if m == &current_model {
                                    " (current)"
                                } else {
                                    ""
                                };
                                println!("  {}. {}{}", i + 1, m, mark);
                            }
                            println!("\nEnter number or exact model name.");
                            print!("Selection: ");
                            io::stdout().flush().unwrap();
                            let choice = input.read_line_blocking()?;
                            let choice = choice.trim();
                            if choice.is_empty() {
                                continue;
                            }
                            if let Ok(num) = choice.parse::<usize>() {
                                if num > 0 && num <= available.len() {
                                    available[num - 1].clone()
                                } else {
                                    println!("[red]Invalid number.[/red]");
                                    continue;
                                }
                            } else {
                                choice.to_string()
                            }
                        }
                    };

                    if new_model.is_empty() {
                        continue;
                    }

                    // Validate against current provider (or allow free for openrouter / mock)
                    let is_valid = available.iter().any(|m| m == &new_model)
                        || provider_name == "openrouter" && new_model.contains('/')
                        || provider_name == "mock"
                        || available.is_empty();

                    if !is_valid {
                        println!(
                            "[red]Model '{}' not valid for provider '{}'.[/red]",
                            new_model, provider_name
                        );
                        if !available.is_empty() {
                            println!("Available: {}", available.join(", "));
                        }
                        continue;
                    }

                    let selected_provider = provider_name.clone();
                    let selected_model = new_model.clone();
                    if let Err(e) = update_nib_config(&project, move |config| {
                        if let Some(provider) = config.llm.providers.get_mut(&selected_provider) {
                            provider.model = selected_model;
                            Ok(())
                        } else if selected_provider == "mock" {
                            config.llm.add_or_update_provider(
                                selected_provider,
                                selected_model,
                                None,
                            );
                            Ok(())
                        } else {
                            Err(format!(
                                "provider '{selected_provider}' is no longer configured"
                            ))
                        }
                    }) {
                        eprintln!("Failed saving model: {}", e);
                    } else {
                        println!(
                            "[green]Switched model to '{}' for provider '{}'.[/green]",
                            new_model, provider_name
                        );
                    }
                    // cfg reloaded implicitly on next use
                    cfg = load_nib_config_full(&project)
                        .map_err(|error| error.to_string())?
                        .llm;
                }
                "skills" => {
                    let sub_args: Vec<&str> =
                        arg.as_deref().unwrap_or("").split_whitespace().collect();
                    if sub_args.is_empty() || sub_args[0] == "list" {
                        if let Err(error) = crate::skill_cmd::list_skills(&project) {
                            eprintln!("Failed to list skills: {error}");
                        }
                    } else if sub_args[0] == "install" && sub_args.len() > 1 {
                        match crate::skill_cmd::install_skill(sub_args[1]) {
                            Ok(path) => println!("Installed skill at {}", path.display()),
                            Err(error) => eprintln!("Failed to install skill: {error}"),
                        }
                    } else if sub_args[0] == "remove" && sub_args.len() > 1 {
                        match crate::skill_cmd::remove_skill(sub_args[1]) {
                            Ok(()) => println!("Removed skill '{}'.", sub_args[1]),
                            Err(error) => eprintln!("Failed to remove skill: {error}"),
                        }
                    } else {
                        println!("[red]Usage: /skills list | /skills install <url_or_path> | /skills remove <name>[/red]");
                    }
                }
                "mcp" => {
                    let sub_args: Vec<&str> =
                        arg.as_deref().unwrap_or("").split_whitespace().collect();
                    if sub_args.is_empty() || sub_args[0] == "list" {
                        if let Err(error) = crate::mcp_cmd::list_mcp_servers(&project) {
                            eprintln!("Failed to list MCP servers: {error}");
                        }
                    } else if sub_args[0] == "add" && sub_args.len() >= 3 {
                        let name = sub_args[1];
                        let command = sub_args[2];
                        let args: Vec<String> =
                            sub_args[3..].iter().map(|s| s.to_string()).collect();
                        if let Err(error) =
                            crate::mcp_cmd::add_mcp_server(&project, name, command, &args)
                        {
                            eprintln!("Failed to add MCP server: {error}");
                        }
                    } else if sub_args[0] == "remove" && sub_args.len() > 1 {
                        if let Err(error) = crate::mcp_cmd::remove_mcp_server(&project, sub_args[1])
                        {
                            eprintln!("Failed to remove MCP server: {error}");
                        }
                    } else {
                        println!("[red]Usage: /mcp list | /mcp add <name> <command> [args...] | /mcp remove <name>[/red]");
                    }
                }
                _ => {
                    println!(
                        "[red]Unknown command: /{}. Only /model for changes. See /help[/red]",
                        command
                    );
                }
            }
            continue;
        }

        println!("[dim]Thinking...[/dim]");

        match execute_agent_step(&project, &sid, command_input, &input) {
            Ok(()) => {
                // After step, show new assistant messages
                if let Some(sess) = session_store
                    .load_result(&sid)
                    .map_err(|error| format!("failed to reload session: {error}"))?
                {
                    // print the last few assistant or tool msgs not previously shown (simple: last message)
                    if let Some(last) = sess.messages.last() {
                        if last.role == "assistant" {
                            println!("\x1b[32mAssistant\x1b[0m: {}", last.content);
                        } else if last.role == "tool" {
                            // Compact note for tool-using turns (full in session file)
                            println!(
                                "[dim](tool results recorded; last: {}...)[/dim]",
                                last.content.chars().take(80).collect::<String>()
                            );
                        }
                    }
                }
            }
            Err(e) => {
                println!("[red]Error during step: {}\x1b[0m", e);
                let should_append = session_store
                    .load_result(&sid)
                    .map_err(|error| error.to_string())?
                    .and_then(|session| session.messages.last().cloned())
                    .is_none_or(|message| message.role != "assistant");
                if should_append {
                    session_store
                        .try_append_message(&sid, "assistant", &format!("[error] {e}"))
                        .map_err(|error| error.to_string())?;
                }
            }
        }
    }

    Ok(())
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

    let loop_cfg = nib::agent::AgentLoopConfig {
        max_steps: 0,
        mode: "execute".to_string(),
        provider: None,
        auto_approve: false,
        approval_handler: Some(Arc::new(ConsoleApprovalHandler::new(input.clone()))),
        question_handler: Some(Arc::new(ConsoleQuestionHandler::new(input.clone()))),
        ..Default::default()
    };

    let result = rt.block_on(nib::agent::run_agent_loop(
        project.to_path_buf(),
        session_id,
        goal,
        loop_cfg,
    ));

    result.and_then(|summary| {
        if summary.outcome == "waiting_for_user_input" {
            Err(format!(
                "question input was unavailable; session {session_id} was reconciled without continuing"
            ))
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
