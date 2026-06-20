use clap::Args;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::auth::run_auth_wizard;
use crate::config::{load_config, save_config, LLMConfigFile};
use crate::session::SessionStore;

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
    let mut cfg: LLMConfigFile = load_config(&project);

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

/// Delegate a single agent loop step (LLM + tool execution + recording) to the Python core.
/// This keeps Rust as the CLI while the agent/llm/executor stay in Python (litellm etc).
/// Uses uv if present, falls back to python3. PYTHONPATH is set for src/ layout in workspace.
fn execute_agent_step(project: &Path, session_id: &str, goal: &str) -> Result<(), String> {
    // Prepare a small python snippet that reads from env and drives one loop pass
    // We avoid double-appending the user message by letting the loop handle it.
    // (run_agent_loop appends the goal as user internally)

    let py_code = r#"
import os, sys, asyncio, json
from pathlib import Path

# Ensure we can import nib from the workspace layout
src = os.environ.get("NIB_PYTHONPATH") or "src"
if src and src not in sys.path:
    sys.path.insert(0, src)

project = Path(os.environ.get("NIB_PROJECT", ".")).resolve()
sid = os.environ.get("NIB_SESSION_ID", "")
goal = os.environ.get("NIB_GOAL", "")

if not sid or not goal:
    print("MISSING_ENV")
    sys.exit(2)

from nib.config import LLMConfig
from nib.core.workload import SessionStore
from nib.agent.loop import run_agent_loop
from nib.tools.executor import default_executor

ss = SessionStore(project_root=project)
llm_cfg = LLMConfig(project_root=project)
llm = llm_cfg.get_current_client()

executor = default_executor
executor.session_store = ss
executor.project_root = project

summary = asyncio.run(run_agent_loop(
    session_store=ss,
    session_id=sid,
    goal=goal,
    llm=llm,
    executor=executor,
    max_steps=int(os.environ.get("NIB_MAX_STEPS", "6")),
    mode=os.environ.get("NIB_MODE", "execute"),
    project_root=project,
))
print("STEP_DONE")
print(json.dumps(summary)[:500])
"#;

    let mut cmd = if which_uv() {
        let mut c = std::process::Command::new("uv");
        c.arg("run").arg("python").arg("-c").arg(py_code);
        c
    } else {
        let mut c = std::process::Command::new("python3");
        c.arg("-c").arg(py_code);
        c
    };

    // Set envs for the snippet
    let pp = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("src")
        .to_string_lossy()
        .to_string();

    cmd.env("NIB_PROJECT", project)
        .env("NIB_SESSION_ID", session_id)
        .env("NIB_GOAL", goal)
        .env("NIB_MAX_STEPS", "6")
        .env("NIB_MODE", "execute")
        .env("NIB_PYTHONPATH", pp)
        .current_dir(project);

    let output = cmd
        .output()
        .map_err(|e| format!("failed to spawn python: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // If the python side printed useful info, surface last lines
        return Err(format!(
            "agent step failed (code {:?}): {}",
            output.status.code(),
            stderr.lines().last().unwrap_or("")
        ));
    }

    // success
    Ok(())
}

fn which_uv() -> bool {
    std::process::Command::new("uv")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
