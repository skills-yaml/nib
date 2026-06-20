use clap::{Parser, Subcommand};
use std::process;

mod auth;
mod chat;
mod config;
mod context_cmd;
mod run;
mod session;
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

    /// Quick demo of tool executor (dev)
    #[command(name = "demo-tool")]
    DemoTool {
        tool: String,
        #[arg(short, long, default_value = "README.md")]
        arg: String,
    },

    /// Launch the TUI (placeholder)
    Tui,
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Some(Commands::Version) => version::show_version(),
        Some(Commands::Chat(args)) => chat::run_chat(args),
        Some(Commands::Run(args)) => run::run_agent(args),
        Some(Commands::Auth) => auth::run_auth_wizard(),
        Some(Commands::Context(args)) => context_cmd::run_context(args),
        Some(Commands::DemoTool { tool, arg }) => {
            println!("demo-tool {} --arg {} (Rust CLI stub)", tool, arg);
            process::exit(0);
        }
        Some(Commands::Tui) => {
            println!("TUI coming soon (see Python implementation for now)");
        }
        None => {
            println!("nib — AI agent for coding and workload management");
            println!("Use `nib chat` for interactive, `nib run \"goal\"` for one-shot, `nib auth` for providers, or `nib --help`");
        }
    }
}
