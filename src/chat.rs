use clap::Args;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::auth::run_auth_wizard;
use nib::config::{load_config, save_config};
use nib::session::SessionStore;

#[derive(Args, Debug)]
pub struct ChatArgs {
    #[arg(short, long)]
    pub session: Option<String>,

    /// Run the auth wizard before starting chat (same as `nib auth`)
    #[arg(long)]
    pub auth: bool,
}

pub fn run_chat(args: &ChatArgs) {
    let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut cfg = load_config(&project);

    if args.auth || cfg.providers.is_empty() {
        // Run wizard (it may prompt for multiple)
        run_auth_wizard();
        cfg = load_config(&project);
    }

    // If still no active/non-mock, ensure at least mock is usable
    let active = cfg.get_active_provider();
    if active != "mock" && cfg.get_provider(None).is_none() {
        // fallback to mock silently
        cfg.active_provider = Some("mock".to_string());
        let _ = save_config(&project, &cfg);
    }

    let session_store = SessionStore::new(&project);

    // Resolve or create session
    let sid = if let Some(s) = &args.session {
        if session_store.load(s).is_some() {
            println!("[dim]Resumed session {s}[/dim]");
            s.clone()
        } else {
            println!("[yellow]Session {s} not found, creating new.[/yellow]");
            let new_s = session_store.create_session();
            new_s.id
        }
    } else {
        let new_s = session_store.create_session();
        new_s.id
    };

    println!("\n\x1b[1;32mnib chat\x1b[0m  |  session: {sid}  |  provider: {active}");
    println!("[dim]Type message. /model to change (list/select or name). /help for commands. Ctrl+C to exit.\x1b[0m\n");

    // Show recent history (last few)
    if let Some(sess) = session_store.load(&sid) {
        for msg in sess.messages.iter().rev().take(6).rev() {
            let color = if msg.role == "user" {
                "\x1b[36m"
            } else {
                "\x1b[32m"
            };
            let prefix = format!("{}{}\x1b[0m", color, msg.role);
            let short = if msg.content.len() > 200 {
                format!("{}...", &msg.content[..200])
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

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            break;
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        if let Some(cmdline) = input.strip_prefix('/') {
            let parts: Vec<&str> = cmdline.splitn(2, ' ').collect();
            let command = parts[0].to_lowercase();
            let arg = parts.get(1).map(|s| s.trim().to_string());

            match command.as_str() {
                "q" | "quit" | "exit" => {
                    println!("[dim]Goodbye. Session saved to .nib/sessions/{sid}.json[/dim]");
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
                    let new_s = session_store.create_session();
                    // update local sid by re-entering? for simplicity print and user can continue
                    println!("[yellow]Started fresh session {}. (Restart chat or continue here.)[/yellow]", new_s.id);
                    // We keep using the old sid var for this run; user sees note.
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
                            let mut m = String::new();
                            let _ = io::stdin().read_line(&mut m);
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
                            let mut choice = String::new();
                            let _ = io::stdin().read_line(&mut choice);
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

                    cfg.update_model_for_active(new_model.clone());
                    if let Err(e) = save_config(&project, &cfg) {
                        eprintln!("Failed saving model: {}", e);
                    } else {
                        println!(
                            "[green]Switched model to '{}' for provider '{}'.[/green]",
                            new_model, provider_name
                        );
                    }
                    // cfg reloaded implicitly on next use
                    cfg = load_config(&project);
                }
                "skills" => {
                    let sub_args: Vec<&str> =
                        arg.as_deref().unwrap_or("").split_whitespace().collect();
                    if sub_args.is_empty() || sub_args[0] == "list" {
                        crate::skill_cmd::list_skills(&project);
                    } else if sub_args[0] == "install" && sub_args.len() > 1 {
                        crate::skill_cmd::install_skill(sub_args[1]);
                    } else if sub_args[0] == "remove" && sub_args.len() > 1 {
                        crate::skill_cmd::remove_skill(sub_args[1]);
                    } else {
                        println!("[red]Usage: /skills list | /skills install <url_or_path> | /skills remove <name>[/red]");
                    }
                }
                "mcp" => {
                    let sub_args: Vec<&str> =
                        arg.as_deref().unwrap_or("").split_whitespace().collect();
                    if sub_args.is_empty() || sub_args[0] == "list" {
                        crate::mcp_cmd::list_mcp_servers(&project);
                    } else if sub_args[0] == "add" && sub_args.len() >= 3 {
                        let name = sub_args[1];
                        let command = sub_args[2];
                        let args: Vec<String> =
                            sub_args[3..].iter().map(|s| s.to_string()).collect();
                        crate::mcp_cmd::add_mcp_server(&project, name, command, &args);
                    } else if sub_args[0] == "remove" && sub_args.len() > 1 {
                        crate::mcp_cmd::remove_mcp_server(&project, sub_args[1]);
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

        // Normal user message: delegate to Python (the agent loop appends the goal as user message + runs)
        println!("[dim]Thinking... (delegating to Python LLM + tools)[/dim]");

        match execute_agent_step(&project, &sid, input) {
            Ok(()) => {
                // After step, show new assistant messages
                if let Some(sess) = session_store.load(&sid) {
                    // print the last few assistant or tool msgs not previously shown (simple: last message)
                    if let Some(last) = sess.messages.last() {
                        if last.role == "assistant" {
                            println!("\x1b[32mAssistant\x1b[0m: {}", last.content);
                        } else if last.role == "tool" {
                            // Compact note for tool-using turns (full in session file)
                            println!(
                                "[dim](tool results recorded; last: {}...)[/dim]",
                                &last.content[..last.content.len().min(80)]
                            );
                        }
                    }
                }
            }
            Err(e) => {
                println!("[red]Error during step: {}\x1b[0m", e);
                session_store.append_message(&sid, "assistant", &format!("[error] {}", e));
            }
        }
    }
}

fn execute_agent_step(project: &Path, session_id: &str, goal: &str) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let loop_cfg = nib::agent::AgentLoopConfig {
        max_steps: 15,
        mode: "execute".to_string(),
        provider: None,
        auto_approve: false,
        ..Default::default()
    };

    let result = rt.block_on(nib::agent::run_agent_loop(
        project.to_path_buf(),
        session_id,
        goal,
        loop_cfg,
    ));

    match result {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}
