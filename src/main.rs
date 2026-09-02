use clap::{Parser, Subcommand};
use nib::{mcp_cmd, skill_cmd};
use std::path::PathBuf;
use std::process;

mod auth;
mod chat;
mod config_cmd;
mod console;
mod context_cmd;
mod doctor;
#[cfg(debug_assertions)]
mod mcp_test_fixture;
mod run;
mod task_cmd;
mod updater;
mod version;

#[derive(Parser)]
#[command(name = "nib")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "AI agent for coding and workload management", long_about = None)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(flatten)]
    interactive: chat::ChatArgs,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[cfg(test)]
mod cli_tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn conventional_version_flag_is_available() {
        let error = match Cli::try_parse_from(["nib", "--version"]) {
            Ok(_) => panic!("the version flag must exit through clap's display path"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn update_command_is_public_and_worker_commands_skip_startup_checks() {
        let update = Cli::try_parse_from(["nib", "update"]).expect("update command");
        assert!(matches!(update.command, Some(Commands::Update(_))));
        assert!(!startup_update_check_is_eligible(&update.command));

        let version = Cli::try_parse_from(["nib", "version"]).expect("version command");
        assert!(startup_update_check_is_eligible(&version.command));

        let server = Cli::try_parse_from(["nib", "mcp-server"]).expect("MCP server command");
        assert!(!startup_update_check_is_eligible(&server.command));
    }

    #[test]
    fn update_channel_accepts_canonical_values_and_bounded_aliases() {
        for (value, expected) in [
            ("prod", updater::UpdateChannel::Prod),
            ("production", updater::UpdateChannel::Prod),
            ("development", updater::UpdateChannel::Development),
            ("dev", updater::UpdateChannel::Development),
        ] {
            let parsed = Cli::try_parse_from(["nib", "update", "--channel", value])
                .expect("valid update channel");
            let Some(Commands::Update(args)) = parsed.command else {
                panic!("expected update command");
            };
            assert_eq!(args.channel, Some(expected));
        }

        assert!(Cli::try_parse_from(["nib", "update", "--channel", "nightly"]).is_err());
    }

    #[test]
    fn tui_accepts_chat_equivalent_session_and_auth_entry_options() {
        let parsed = Cli::try_parse_from([
            "nib",
            "tui",
            "--run",
            "inspect",
            "--session",
            "session-1",
            "--auth",
        ])
        .expect("TUI parity options");
        let Some(Commands::Tui(args)) = parsed.command else {
            panic!("expected TUI command");
        };
        assert_eq!(args.run.as_deref(), Some("inspect"));
        assert_eq!(args.session.as_deref(), Some("session-1"));
        assert!(args.auth);
    }

    #[test]
    fn no_subcommand_and_chat_share_the_interactive_argument_contract() {
        let root = Cli::try_parse_from([
            "nib",
            "--plain",
            "--run",
            "inspect",
            "--session",
            "session-1",
            "--auth",
        ])
        .expect("root interactive options");
        assert!(root.command.is_none());
        assert!(root.interactive.plain);
        assert_eq!(root.interactive.run.as_deref(), Some("inspect"));
        assert_eq!(root.interactive.session.as_deref(), Some("session-1"));
        assert!(root.interactive.auth);

        let chat = Cli::try_parse_from([
            "nib",
            "chat",
            "--plain",
            "--run",
            "inspect",
            "--session",
            "session-1",
            "--auth",
        ])
        .expect("chat interactive options");
        let Some(Commands::Chat(chat)) = chat.command else {
            panic!("expected chat command");
        };
        assert_eq!(root.interactive, chat);
    }

    #[test]
    fn interactive_modes_conflict_and_do_not_mix_with_subcommands() {
        assert!(Cli::try_parse_from(["nib", "--plain", "--tui"]).is_err());
        assert!(Cli::try_parse_from(["nib", "chat", "--plain", "--tui"]).is_err());
        assert!(Cli::try_parse_from(["nib", "--plain", "version"]).is_err());
        assert!(Cli::try_parse_from(["nib", "--session", "session-1", "doctor"]).is_err());
    }

    #[test]
    fn doctor_accepts_explicit_fix_mode() {
        let parsed = Cli::try_parse_from(["nib", "doctor", "--fix"]).expect("doctor repair option");
        let Some(Commands::Doctor(args)) = parsed.command else {
            panic!("expected doctor command");
        };
        assert!(args.fix);
        assert!(!args.confirm_no_legacy_processes);

        let parsed =
            Cli::try_parse_from(["nib", "doctor", "--fix", "--confirm-no-legacy-processes"])
                .expect("offline legacy migration confirmation");
        let Some(Commands::Doctor(args)) = parsed.command else {
            panic!("expected doctor command");
        };
        assert!(args.fix);
        assert!(args.confirm_no_legacy_processes);
        assert!(Cli::try_parse_from(["nib", "doctor", "--confirm-no-legacy-processes"]).is_err());
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Show the installed version
    Version,

    /// Update this installed release, optionally switching release channels
    Update(updater::UpdateArgs),

    /// Start the interactive session (TUI when supported, plain mode otherwise)
    Chat(chat::ChatArgs),

    /// Run the agent loop for a specific goal
    Run(run::RunArgs),

    /// Run the provider auth wizard (select provider + API key)
    Auth,

    /// Assemble and display rich project context
    Context(context_cmd::ContextArgs),

    /// Inspect or edit project-local configuration
    Config(config_cmd::ConfigArgs),

    /// Validate config, providers, sandbox, and sessions
    Doctor(doctor::DoctorArgs),

    /// Quick demo of tool executor (dev)
    #[command(name = "demo-tool")]
    DemoTool {
        tool: String,
        #[arg(short, long, default_value = ".")]
        arg: String,
        #[arg(long)]
        yes: bool,
    },

    /// Launch the full-screen TUI (compatibility alias for `nib --tui`)
    Tui(TuiArgs),

    /// Start nib as an MCP Server (JSON-RPC over stdio)
    #[command(name = "mcp-server")]
    McpServer,

    /// Copy MCP response frames to the server's real stdout
    #[command(name = "mcp-stdio-relay", hide = true)]
    McpStdioRelay,

    /// Manage skills (list, install, remove)
    Skill(skill_cmd::SkillArgs),

    /// Manage MCP servers (list, add, remove)
    Mcp(mcp_cmd::McpArgs),

    /// Manage durable background terminal and scheduled jobs
    Task(task_cmd::TaskArgs),

    /// Execute one persisted job in a detached worker process
    #[command(name = "task-worker", hide = true)]
    TaskWorker {
        #[arg(long)]
        daemon_dir: PathBuf,
        #[arg(long)]
        task_id: String,
        #[arg(long, hide = true)]
        lease_token: String,
    },

    /// Supervise one foreground subagent worker process
    #[command(name = "subagent-supervisor", hide = true)]
    SubagentSupervisor {
        #[arg(long)]
        project_root: PathBuf,
        #[arg(long)]
        subagent_id: String,
        #[arg(long)]
        execution_generation: u64,
        #[arg(long, hide = true)]
        owner_lease: String,
        #[arg(long, hide = true)]
        cleanup_lease_id: String,
        #[arg(long, hide = true)]
        supervisor_registration_nonce: String,
        #[arg(long)]
        worktree: PathBuf,
    },

    /// Execute one supervised subagent agent loop
    #[command(name = "subagent-worker", hide = true)]
    SubagentWorker {
        #[arg(long)]
        worktree: PathBuf,
        #[arg(long)]
        subagent_id: String,
    },
}

#[derive(clap::Args)]
pub struct TuiArgs {
    /// Optional goal to run immediately in the background
    #[arg(long)]
    pub run: Option<String>,

    /// Resume an existing session for subsequent TUI turns
    #[arg(short, long)]
    pub session: Option<String>,

    /// Run the auth wizard before starting the TUI
    #[arg(long)]
    pub auth: bool,
}

fn main() {
    #[cfg(windows)]
    if let Some(status) = updater::run_windows_update_worker_if_requested() {
        process::exit(status);
    }

    #[cfg(debug_assertions)]
    if let Some(status) = mcp_test_fixture::run_if_requested() {
        process::exit(status);
    }

    let cli = Cli::parse();
    let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if startup_update_check_is_eligible(&cli.command) {
        updater::maybe_print_startup_notice();
    }

    match &cli.command {
        Some(Commands::Version) => version::show_version(),
        Some(Commands::Update(args)) => match updater::run_update(args) {
            Ok(message) => println!("{message}"),
            Err(error) => {
                eprintln!("Update error: {error}");
                process::exit(1);
            }
        },
        Some(Commands::Chat(args)) => {
            if let Err(error) = chat::run_interactive(args) {
                eprintln!("Interactive error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::Run(args)) => {
            if let Err(error) = run::run_agent(args) {
                eprintln!("{error}");
                process::exit(1);
            }
        }
        Some(Commands::Auth) => {
            if let Err(error) = auth::run_auth_wizard() {
                eprintln!("Auth error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::Context(args)) => {
            if let Err(error) = context_cmd::run_context(args) {
                eprintln!("Context error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::Config(args)) => {
            if let Err(error) = config_cmd::run_config_cmd(args, &project) {
                eprintln!("Config error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::Doctor(args)) => {
            if !doctor::run_doctor_with_args(&project, args) {
                process::exit(1);
            }
        }
        Some(Commands::DemoTool { tool, arg, yes }) => {
            let cfg = match nib::config::load_nib_config_full(&project) {
                Ok(config) => config,
                Err(error) => {
                    eprintln!("Demo tool configuration error: {error}");
                    process::exit(1);
                }
            };
            let profiles = match nib::profile::ProfileRegistry::load(&project, &cfg.profiles) {
                Ok(profiles) => profiles,
                Err(error) => {
                    eprintln!("Demo tool profile error: {error}");
                    process::exit(1);
                }
            };
            let profile = profiles
                .for_workspace(&project)
                .unwrap_or_else(|| profiles.default_profile());
            if let Err(error) = profile.ensure_state_dirs() {
                eprintln!("Demo tool profile error: {error}");
                process::exit(1);
            }
            let session_store =
                nib::session::SessionStore::at_dir(profile.sessions_dir().to_path_buf());
            let session = match session_store.try_create_session() {
                Ok(session) => session,
                Err(error) => {
                    eprintln!("Demo tool session error: {error}");
                    process::exit(1);
                }
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio");
            let mut executor = nib::tools::ToolExecutor::new(
                profile.root_path().to_path_buf(),
                cfg.execution.clone(),
            )
            .with_auto_approve(*yes)
            .with_terminal_config(&cfg.terminal)
            .with_approvals_config(&cfg.approvals)
            .with_session_store(session_store)
            .with_environment(profile.custom_env())
            .with_sensitive_values(cfg.public_session_sensitive_values());
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
                invocation_id: nib::tools::ToolInvocationId::new(),
                tool_name: tool.clone(),
                arguments: args_json,
                session_id: Some(session.id.clone()),
                project_root: Some(profile.root_path().to_path_buf()),
            };
            let result = rt.block_on(executor.execute(call, Some(&session.id)));
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
            process::exit(if result.success { 0 } else { 1 });
        }
        Some(Commands::Tui(args)) => {
            eprintln!("nib tui is a compatibility alias; prefer nib --tui");
            let interactive = chat::ChatArgs {
                run: args.run.clone(),
                session: args.session.clone(),
                auth: args.auth,
                tui: true,
                ..Default::default()
            };
            if let Err(error) = chat::run_interactive(&interactive) {
                eprintln!("Interactive error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::McpServer) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio");
            if let Err(error) = rt.block_on(nib::integrations::mcp_server::run_mcp_server(&project))
            {
                eprintln!("MCP server error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::McpStdioRelay) => {
            if let Err(error) = nib::integrations::mcp_server::run_stdio_relay() {
                eprintln!("{error}");
                process::exit(1);
            }
        }
        Some(Commands::Skill(args)) => {
            if let Err(error) = skill_cmd::run_skill_cmd(args, &project) {
                eprintln!("Skill error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::Mcp(args)) => {
            if let Err(error) = mcp_cmd::run_mcp_cmd(args, &project) {
                eprintln!("MCP config error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::Task(args)) => {
            if let Err(error) = task_cmd::run_task_cmd(args, &project) {
                eprintln!("Task error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::TaskWorker {
            daemon_dir,
            task_id,
            lease_token,
        }) => {
            let rt = match nib::agent::build_agent_runtime(
                "failed to initialize the task worker runtime",
            ) {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("Task worker error: {error}");
                    process::exit(1);
                }
            };
            let worker_daemon_dir = daemon_dir.clone();
            let worker_task_id = task_id.clone();
            let worker_lease_token = lease_token.clone();
            let result = nib::agent::block_on_agent_runtime_worker(
                &rt,
                async move {
                    nib::daemons::workload::run_worker(
                        &worker_daemon_dir,
                        &worker_task_id,
                        &worker_lease_token,
                    )
                    .await
                },
                "task runtime worker",
            )
            .and_then(|result| result);
            if let Err(error) = result {
                eprintln!("Task worker error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::SubagentSupervisor {
            project_root,
            subagent_id,
            execution_generation,
            owner_lease,
            cleanup_lease_id,
            supervisor_registration_nonce,
            worktree,
        }) => {
            if let Err(error) = nib::tools::delegation::run_subagent_supervisor(
                project_root,
                subagent_id,
                *execution_generation,
                owner_lease,
                cleanup_lease_id,
                supervisor_registration_nonce,
                worktree,
            ) {
                eprintln!("Subagent supervisor error: {error}");
                process::exit(1);
            }
        }
        Some(Commands::SubagentWorker {
            worktree,
            subagent_id,
        }) => {
            if let Err(error) = nib::tools::delegation::run_subagent_worker(worktree, subagent_id) {
                eprintln!("Subagent worker error: {error}");
                process::exit(1);
            }
        }
        None => {
            if let Err(error) = chat::run_interactive(&cli.interactive) {
                eprintln!("Interactive error: {error}");
                process::exit(1);
            }
        }
    }
}

fn startup_update_check_is_eligible(command: &Option<Commands>) -> bool {
    !matches!(
        command,
        Some(
            Commands::Update(_)
                | Commands::McpServer
                | Commands::McpStdioRelay
                | Commands::TaskWorker { .. }
                | Commands::SubagentSupervisor { .. }
                | Commands::SubagentWorker { .. }
        )
    )
}
