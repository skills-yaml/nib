use clap::Args;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;

use crate::auth::run_auth_wizard;
use crate::console::{ConsoleApprovalHandler, ConsoleInput, ConsoleQuestionHandler};
use nib::config::load_nib_config_full;
use nib::interactive::{
    display_stream_event, execute_interactive_command_in_state, format_session_status,
    interactive_completions, interactive_session_candidate, parse_interactive_command,
    parse_queue_line, persist_queued_follow_up, resolve_session, set_active_model,
    take_next_queued_follow_up, validate_interactive_session_target, InteractiveEffect,
    InteractiveSessionSelection, ModelSelection, SessionResolution, StreamDisplay,
};
use nib::session::SessionStore;

#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatArgs {
    /// Optional goal to submit as the first interactive turn
    #[arg(long)]
    pub run: Option<String>,

    #[arg(short, long)]
    pub session: Option<String>,

    /// Run the auth wizard before starting the interactive session (same as `nib auth`)
    #[arg(long)]
    pub auth: bool,

    /// Force the line-oriented interactive renderer
    #[arg(long, conflicts_with = "tui")]
    pub plain: bool,

    /// Force the full-screen terminal renderer
    #[arg(long, conflicts_with = "plain")]
    pub tui: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveMode {
    Plain,
    Tui,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalCapabilities {
    input_is_terminal: bool,
    output_is_terminal: bool,
    term: Option<String>,
}

impl TerminalCapabilities {
    fn detect() -> Self {
        Self {
            input_is_terminal: io::stdin().is_terminal(),
            output_is_terminal: io::stdout().is_terminal(),
            term: std::env::var("TERM").ok(),
        }
    }

    fn rejection(&self) -> Option<&'static str> {
        nib::tui::tui_environment_rejection(
            self.input_is_terminal,
            self.output_is_terminal,
            self.term.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModeSelection {
    mode: InteractiveMode,
    auto_fallback_notice: Option<&'static str>,
}

fn resolve_interactive_mode(
    args: &ChatArgs,
    terminal: &TerminalCapabilities,
) -> Result<ModeSelection, String> {
    if args.plain && args.tui {
        return Err("--plain and --tui cannot be used together".to_string());
    }
    if args.plain {
        return Ok(ModeSelection {
            mode: InteractiveMode::Plain,
            auto_fallback_notice: None,
        });
    }
    if args.tui {
        if let Some(reason) = terminal.rejection() {
            return Err(format!("{reason}; use --plain instead"));
        }
        return Ok(ModeSelection {
            mode: InteractiveMode::Tui,
            auto_fallback_notice: None,
        });
    }
    if let Some(reason) = terminal.rejection() {
        let notice = if terminal.input_is_terminal && terminal.output_is_terminal {
            Some(reason)
        } else {
            None
        };
        return Ok(ModeSelection {
            mode: InteractiveMode::Plain,
            auto_fallback_notice: notice,
        });
    }
    Ok(ModeSelection {
        mode: InteractiveMode::Tui,
        auto_fallback_notice: None,
    })
}

pub fn run_interactive(args: &ChatArgs) -> Result<(), String> {
    let selection = resolve_interactive_mode(args, &TerminalCapabilities::detect())?;
    if let Some(reason) = selection.auto_fallback_notice {
        eprintln!("nib: {reason}; starting plain mode");
    }
    let project = std::env::current_dir()
        .map_err(|error| format!("failed to resolve the current project directory: {error}"))?;
    let config = prepare_interactive_config(&project, args.auth)?;
    match selection.mode {
        InteractiveMode::Plain => run_plain_with_input(
            args,
            &project,
            config,
            ConsoleInput::new(io::BufReader::new(io::stdin())),
        ),
        InteractiveMode::Tui => nib::tui::run_tui(&project, args.run.clone(), args.session.clone())
            .map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
fn run_chat_with_input(
    args: &ChatArgs,
    reader: impl io::BufRead + Send + 'static,
) -> Result<(), String> {
    let project = std::env::current_dir()
        .map_err(|error| format!("failed to resolve the current project directory: {error}"))?;
    let config = prepare_interactive_config(&project, args.auth)?;
    run_plain_with_input(args, &project, config, ConsoleInput::new(reader))
}

fn prepare_interactive_config(
    project: &Path,
    authenticate: bool,
) -> Result<nib::config::LlmConfig, String> {
    let mut config = load_nib_config_full(project)
        .map_err(|error| error.to_string())?
        .llm;
    if authenticate || config.providers.is_empty() {
        run_auth_wizard()?;
        config = load_nib_config_full(project)
            .map_err(|error| error.to_string())?
            .llm;
    }
    Ok(config)
}

fn run_plain_with_input(
    args: &ChatArgs,
    project: &Path,
    config: nib::config::LlmConfig,
    input: ConsoleInput,
) -> Result<(), String> {
    let active = config.get_active_provider();
    let session_store = SessionStore::for_project(project)?;

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

    println!("\nnib  |  mode: plain  |  session: {sid}  |  provider: {active}");
    if let Ok(status) = format_session_status(project, &session_store, &sid, "idle") {
        println!("{status}");
    }
    println!("Type message, queue: <text>, or /help. Enter never steers. Ctrl+C to exit.\n");

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

    if let Some(goal) = args.run.as_deref() {
        println!("Thinking...");
        if let Err(error) = execute_agent_step(project, &sid, goal, &input) {
            println!("{error}");
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
        let mut command_input = line.trim().to_string();
        if command_input.is_empty() {
            continue;
        }

        if let Some(queued) = parse_queue_line(&command_input) {
            match persist_queued_follow_up(&session_store, &sid, queued, "composer") {
                Ok(_) => println!("queued follow-up retained on session {sid}"),
                Err(error) => println!("{error}"),
            }
            continue;
        }

        let parsed = match parse_interactive_command(&command_input) {
            Err(error) => {
                println!("{error}");
                let completed = match select_command_completion_from_console(&input, &command_input)
                {
                    Ok(completed) => completed,
                    Err(error) => {
                        println!("Could not complete command: {error}");
                        continue;
                    }
                };
                let Some(completed) = completed else { continue };
                command_input = completed;
                match parse_interactive_command(&command_input) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        println!("{error}");
                        continue;
                    }
                }
            }
            Ok(parsed) => parsed,
        };

        if let Some(command) = parsed {
            match execute_interactive_command_in_state(
                command,
                project,
                &session_store,
                &sid,
                "idle",
            ) {
                Ok(InteractiveEffect::Quit) => {
                    if let Ok(count) =
                        nib::interactive::queued_follow_up_count(&session_store, &sid)
                    {
                        if count > 0 {
                            println!("{count} queued follow-up(s) retained on session {sid}.");
                        }
                    }
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
                Ok(InteractiveEffect::SelectSession(selection)) => {
                    match select_session_from_console(&input, &session_store, &sid, &selection) {
                        Ok(ChatSessionAction::Activated(session_id)) => {
                            sid = session_id;
                            println!("Resumed session {sid} from persisted state.");
                        }
                        Ok(ChatSessionAction::Unchanged) => {
                            println!("Session {sid} is already active.")
                        }
                        Ok(ChatSessionAction::Cancelled) => {
                            println!("Session switch cancelled.")
                        }
                        Err(error) => println!(
                            "Could not switch sessions: {error}. The active session is unchanged."
                        ),
                    }
                }
                Ok(InteractiveEffect::SelectModel(selection)) => {
                    if let Some(model) = select_model_from_console(&input, &selection)? {
                        match set_active_model(project, &model) {
                            Ok(output) => println!("{output}"),
                            Err(error) => eprintln!("{error}"),
                        }
                    }
                }
                Ok(InteractiveEffect::SubmitGoal { goal }) => {
                    println!("Thinking...");
                    if let Err(error) = execute_agent_step(project, &sid, &goal, &input) {
                        println!("{error}");
                    }
                    match take_next_queued_follow_up(&session_store, &sid) {
                        Ok(Some(next)) => {
                            println!("Starting queued follow-up.");
                            if let Err(error) = execute_agent_step(project, &sid, &next, &input) {
                                println!("{error}");
                            }
                        }
                        Ok(None) => {}
                        Err(error) => println!("{error}"),
                    }
                }
                Err(error) => println!("{error}"),
            }
            continue;
        }

        println!("Thinking...");

        match execute_agent_step(project, &sid, &command_input, &input) {
            Ok(()) => {}
            Err(error) => println!("{error}"),
        }
        match take_next_queued_follow_up(&session_store, &sid) {
            Ok(Some(goal)) => {
                println!("Starting queued follow-up.");
                if let Err(error) = execute_agent_step(project, &sid, &goal, &input) {
                    println!("{error}");
                }
            }
            Ok(None) => {}
            Err(error) => println!("{error}"),
        }
    }

    Ok(())
}

fn select_command_completion_from_console(
    input: &ConsoleInput,
    command_input: &str,
) -> Result<Option<String>, String> {
    let completions = interactive_completions(command_input);
    if completions.is_empty() {
        return Ok(None);
    }

    println!("Command completions:");
    for (index, completion) in completions.iter().enumerate() {
        println!(
            "  {}. {} — {} ({})",
            index + 1,
            completion.insertion,
            completion.summary,
            completion.usage
        );
    }
    print!("Completion (number, blank to cancel): ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let choice = input.read_line_blocking()?;
    let choice = choice.trim();
    if choice.is_empty() {
        return Ok(None);
    }
    let index = choice
        .parse::<usize>()
        .map_err(|_| "completion selection must be a displayed number".to_string())?;
    let Some(completion) = index
        .checked_sub(1)
        .and_then(|index| completions.get(index))
    else {
        return Err(format!("completion selection {index} is out of range"));
    };
    let mut completed = completion.insertion.clone();
    if completed.ends_with(' ') {
        print!("Complete command: {completed}");
        io::stdout().flush().map_err(|error| error.to_string())?;
        let arguments = input.read_line_blocking()?;
        let arguments = arguments.trim();
        if arguments.is_empty() {
            return Ok(None);
        }
        completed.push_str(arguments);
    }
    Ok(Some(completed))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatSessionAction {
    Cancelled,
    Unchanged,
    Activated(String),
}

fn select_session_from_console(
    input: &ConsoleInput,
    store: &SessionStore,
    active_session_id: &str,
    selection: &InteractiveSessionSelection,
) -> Result<ChatSessionAction, String> {
    println!("Current session: {active_session_id}");
    println!("Available sessions:");
    for (index, candidate) in selection.candidates.iter().enumerate() {
        let marker = if candidate.is_active { " (active)" } else { "" };
        println!("  {}. {}{}", index + 1, candidate.id, marker);
    }
    if selection.omitted > 0 {
        println!(
            "  ... {} additional sessions omitted; enter an exact ID to select one",
            selection.omitted
        );
    }
    print!("Session to preview (number or exact ID, blank to cancel): ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let choice = input.read_line_blocking()?;
    let choice = choice.trim();
    if choice.is_empty() {
        return Ok(ChatSessionAction::Cancelled);
    }

    let exact_candidate = selection
        .candidates
        .iter()
        .find(|candidate| candidate.id == choice)
        .cloned();
    let candidate = if let Some(candidate) = exact_candidate {
        candidate
    } else if choice.bytes().all(|byte| byte.is_ascii_digit())
        && store
            .load_result(choice)
            .map_err(|error| format!("failed to load session {choice}: {error}"))?
            .is_none()
    {
        let index = choice
            .parse::<usize>()
            .map_err(|_| format!("session selection {choice} is out of range"))?;
        index
            .checked_sub(1)
            .and_then(|index| selection.candidates.get(index))
            .cloned()
            .ok_or_else(|| format!("session selection {index} is out of range"))?
    } else {
        interactive_session_candidate(store, choice, active_session_id)?
    };

    println!("\n{}", candidate.preview);
    if candidate.id == active_session_id {
        return Ok(ChatSessionAction::Unchanged);
    }
    print!(
        "Resume session {} instead of {}? [y/N]: ",
        candidate.id, active_session_id
    );
    io::stdout().flush().map_err(|error| error.to_string())?;
    let confirmation = input.read_line_blocking()?;
    confirm_session_candidate(store, candidate, &confirmation)
}

fn confirm_session_candidate(
    store: &SessionStore,
    candidate: nib::interactive::InteractiveSessionCandidate,
    confirmation: &str,
) -> Result<ChatSessionAction, String> {
    if !confirmation.trim().eq_ignore_ascii_case("y") {
        return Ok(ChatSessionAction::Cancelled);
    }
    validate_interactive_session_target(store, &candidate)?;
    Ok(ChatSessionAction::Activated(candidate.id))
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
        .name("nib-plain-stream".to_string())
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
        .map_err(|error| format!("failed to initialize plain stream rendering: {error}"))?;

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
        .map_err(|_| "plain stream renderer panicked".to_string())?;

    result.and_then(|summary| {
        if summary.outcome == "waiting_for_user_input" {
            Err(format!(
                "question input was unavailable; session {session_id} was reconciled without continuing"
            ))
        } else if summary.is_failure() && summary.failure.is_some() {
            // The agent emits its structured failure on the lifecycle stream after
            // reconciliation, so plain mode has already rendered the single safe report.
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

    fn terminal_capabilities(
        input_is_terminal: bool,
        output_is_terminal: bool,
        term: Option<&str>,
    ) -> TerminalCapabilities {
        TerminalCapabilities {
            input_is_terminal,
            output_is_terminal,
            term: term.map(str::to_string),
        }
    }

    #[test]
    fn interactive_mode_resolution_is_deterministic_and_explicit_modes_win() {
        let capable = terminal_capabilities(true, true, Some("xterm-256color"));
        assert_eq!(
            resolve_interactive_mode(&ChatArgs::default(), &capable).unwrap(),
            ModeSelection {
                mode: InteractiveMode::Tui,
                auto_fallback_notice: None,
            }
        );

        let redirected = terminal_capabilities(true, false, Some("xterm-256color"));
        assert_eq!(
            resolve_interactive_mode(&ChatArgs::default(), &redirected).unwrap(),
            ModeSelection {
                mode: InteractiveMode::Plain,
                auto_fallback_notice: None,
            }
        );

        let dumb = terminal_capabilities(true, true, Some("dumb"));
        let fallback = resolve_interactive_mode(&ChatArgs::default(), &dumb).unwrap();
        assert_eq!(fallback.mode, InteractiveMode::Plain);
        assert_eq!(
            fallback.auto_fallback_notice,
            Some("TERM=dumb does not support the full-screen TUI")
        );

        let plain = ChatArgs {
            plain: true,
            ..Default::default()
        };
        assert_eq!(
            resolve_interactive_mode(&plain, &capable).unwrap().mode,
            InteractiveMode::Plain
        );

        let tui = ChatArgs {
            tui: true,
            ..Default::default()
        };
        assert_eq!(
            resolve_interactive_mode(&tui, &capable).unwrap().mode,
            InteractiveMode::Tui
        );
        assert!(resolve_interactive_mode(&tui, &redirected)
            .unwrap_err()
            .contains("use --plain instead"));

        let conflict = ChatArgs {
            plain: true,
            tui: true,
            ..Default::default()
        };
        assert!(resolve_interactive_mode(&conflict, &capable).is_err());
    }

    #[test]
    #[serial]
    fn plain_initial_goal_is_submitted_exactly_once() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let _cwd = CurrentDirGuard::enter(project.path());
        let goal = "initial interactive goal";

        run_chat_with_input(
            &ChatArgs {
                run: Some(goal.to_string()),
                plain: true,
                ..Default::default()
            },
            Cursor::new(b"y\n/quit\n"),
        )
        .expect("plain initial goal");

        let store = SessionStore::for_project(project.path()).expect("session store");
        let sessions = store.list_result().expect("sessions");
        assert_eq!(sessions.len(), 1);
        let session = store
            .load_result(&sessions[0])
            .expect("load session")
            .expect("session");
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| message.role == "user" && message.content == goal)
                .count(),
            1
        );
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
            "\n",
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
                ..Default::default()
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
    fn chat_completion_prompts_use_the_shared_registry() {
        let command = select_command_completion_from_console(
            &ConsoleInput::new(Cursor::new(b"1\n".to_vec())),
            "/pro",
        )
        .expect("root completion");
        assert_eq!(command.as_deref(), Some("/providers"));

        let command = select_command_completion_from_console(
            &ConsoleInput::new(Cursor::new(b"1\n./reviewed-skill\n".to_vec())),
            "/skills i",
        )
        .expect("fixed-subcommand completion");
        assert_eq!(command.as_deref(), Some("/skills install ./reviewed-skill"));

        let cancelled = select_command_completion_from_console(
            &ConsoleInput::new(Cursor::new(b"\n".to_vec())),
            "/",
        )
        .expect("cancelled completion");
        assert_eq!(cancelled, None);
    }

    #[test]
    fn chat_session_confirmation_cancels_and_rejects_a_stale_preview() {
        let directory = tempdir().expect("session directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let active = store
            .try_create_session_with_id("active-session")
            .expect("active session");
        let target = store
            .try_create_session_with_id("target-session")
            .expect("target session");
        let selection = nib::interactive::interactive_session_selection(&store, &active.id)
            .expect("session selection");
        let candidate = selection
            .candidates
            .iter()
            .find(|candidate| candidate.id == target.id)
            .expect("target candidate")
            .clone();

        assert_eq!(
            confirm_session_candidate(&store, candidate.clone(), "n\n")
                .expect("cancel confirmation"),
            ChatSessionAction::Cancelled
        );
        store
            .try_append_message(&target.id, "user", "changed after preview")
            .expect("change target");
        let error = confirm_session_candidate(&store, candidate, "y\n")
            .expect_err("stale candidate must fail closed");
        assert!(error.contains("changed since it was previewed"));

        let scripted = format!("{}\nn\n", target.id);
        assert_eq!(
            select_session_from_console(
                &ConsoleInput::new(Cursor::new(scripted.into_bytes())),
                &store,
                &active.id,
                &nib::interactive::interactive_session_selection(&store, &active.id)
                    .expect("refreshed selection"),
            )
            .expect("scripted cancellation"),
            ChatSessionAction::Cancelled
        );

        let numeric = store
            .try_create_session_with_id("7")
            .expect("numeric session ID");
        let numeric_selection = nib::interactive::interactive_session_selection(&store, &active.id)
            .expect("numeric selection");
        assert_eq!(
            select_session_from_console(
                &ConsoleInput::new(Cursor::new(b"7\ny\n".to_vec())),
                &store,
                &active.id,
                &numeric_selection,
            )
            .expect("numeric exact ID"),
            ChatSessionAction::Activated(numeric.id)
        );
    }

    #[test]
    fn chat_accepts_an_exact_numeric_session_id_when_it_is_not_a_displayed_number() {
        let directory = tempdir().expect("session directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let active = store
            .try_create_session_with_id("active-session")
            .expect("active session");
        store
            .try_create_session_with_id("200")
            .expect("numeric target session");
        let selection = nib::interactive::interactive_session_selection(&store, &active.id)
            .expect("session selection");

        let action = select_session_from_console(
            &ConsoleInput::new(Cursor::new(b"200\ny\n".to_vec())),
            &store,
            &active.id,
            &selection,
        )
        .expect("exact numeric session selection");

        assert_eq!(action, ChatSessionAction::Activated("200".to_string()));
    }

    #[test]
    #[serial]
    fn chat_confirms_session_switch_and_routes_the_next_turn_to_the_target() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let current = store.try_create_session().expect("current session");
        let target = store.try_create_session().expect("target session");
        store
            .try_append_message(&target.id, "user", "existing target context")
            .expect("target context");
        store
            .try_append_message(&target.id, "assistant", "target ready")
            .expect("target response");
        let _cwd = CurrentDirGuard::enter(project.path());
        let commands = format!(
            "/session\n{}\ny\nparity routing goal\ny\n/quit\n",
            target.id
        );

        run_chat_with_input(
            &ChatArgs {
                session: Some(current.id.clone()),
                auth: false,
                ..Default::default()
            },
            Cursor::new(commands.into_bytes()),
        )
        .expect("session-switching chat");

        let current = store
            .load_result(&current.id)
            .expect("load current")
            .expect("current session");
        let target = store
            .load_result(&target.id)
            .expect("load target")
            .expect("target session");
        assert!(!current
            .messages
            .iter()
            .any(|message| message.content == "parity routing goal"));
        assert!(target
            .messages
            .iter()
            .any(|message| message.role == "user" && message.content == "parity routing goal"));
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
