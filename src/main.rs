use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process;

mod auth;
mod chat;
mod context_cmd;
mod doctor;
mod run;
mod updater;
mod version;

#[derive(Parser)]
#[command(name = "nib")]
#[command(about = "AI agent for coding and workload management", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show the installed version
    Version,

    /// Start an interactive chat session
    Chat(chat::ChatArgs),

    /// Run the agent loop for a specific goal
    Run(run::RunArgs),

    /// Run the provider auth wizard (select provider + API key)
    Auth,

    /// Assemble and display rich project context
    Context(context_cmd::ContextArgs),

    /// Validate config, providers, sandbox, and sessions
    Doctor,

    /// Quick demo of tool executor (dev)
    #[command(name = "demo-tool")]
    DemoTool {
        tool: String,
        #[arg(short, long, default_value = ".")]
        arg: String,
        #[arg(long)]
        yes: bool,
    },

    /// Launch session browser TUI (ratatui)
    Tui(TuiArgs),
}

#[derive(clap::Args)]
pub struct TuiArgs {
    /// Optional goal to run immediately in the background
    #[arg(long)]
    pub run: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    match &cli.command {
        Some(Commands::Version) => version::show_version(),
        Some(Commands::Chat(args)) => chat::run_chat(args),
        Some(Commands::Run(args)) => run::run_agent(args),
        Some(Commands::Auth) => auth::run_auth_wizard(),
        Some(Commands::Context(args)) => context_cmd::run_context(args),
        Some(Commands::Doctor) => doctor::run_doctor(&project),
        Some(Commands::DemoTool { tool, arg, yes }) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio");
            let cfg = nib::config::load_nib_config(&project);
            let mut executor = nib::tools::ToolExecutor::new(project.clone(), cfg.execution)
                .with_auto_approve(*yes);
            let args_json = if tool == "run_terminal" {
                serde_json::json!({"command": arg})
            } else if tool == "grep" {
                serde_json::json!({"pattern": arg, "path": "."})
            } else if tool == "apply_patch" {
                serde_json::json!({"patch": arg, "dry_run": true})
            } else {
                serde_json::json!({"path": arg})
            };
            let call = nib::tools::ToolCall {
                tool_name: tool.clone(),
                arguments: args_json,
                session_id: None,
                project_root: Some(project),
            };
            let result = rt.block_on(executor.execute(call, None));
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
            process::exit(if result.success { 0 } else { 1 });
        }
        Some(Commands::Tui(args)) => {
            if let Err(e) = nib::tui::run_tui(&project, args.run.clone()) {
                eprintln!("TUI error: {e}");
                process::exit(1);
            }
        }
        None => {
            println!("nib — AI agent for coding and workload management");
            println!(
                "Use `nib chat`, `nib run \"goal\"`, `nib auth`, `nib doctor`, or `nib --help`"
            );
        }
    }
}
