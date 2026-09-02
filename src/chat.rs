use clap::Args;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::auth::run_auth_wizard;
use crate::console::ConsoleInput;
use nib::config::load_nib_config_full;
use nib::interactive::{
    claim_next_queued_follow_up_after_startup, execute_interactive_command_in_state,
    format_session_status, interactive_completions, interactive_session_candidate,
    persist_queued_follow_up, queue_disposition_message, reduce_interaction,
    resolve_interactive_profile_scope, resolve_session, set_active_model,
    validate_interactive_session_target, DraftHistory, InteractionConsumer, InteractionDecision,
    InteractionInput, InteractionReduction, InteractionRunState, InteractionState,
    InteractionTerminalOutcome, InteractiveAgentMode, InteractiveEffect,
    InteractiveSessionSelection, ModelSelection, SelectorDetailKind, SessionResolution,
    StreamDisplay,
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

#[derive(Default)]
struct PlainSignalRegistrationState {
    next_generation: u64,
    active: Option<(u64, nib::agent::CancellationSignal)>,
}

#[derive(Clone)]
struct PlainSignalOwner {
    state: Arc<Mutex<PlainSignalRegistrationState>>,
}

struct PlainSignalRegistration {
    state: Arc<Mutex<PlainSignalRegistrationState>>,
    generation: u64,
}

struct PlainAgentScope<'a> {
    project: &'a Path,
    profile_id: &'a str,
    session_store: &'a SessionStore,
    signal_owner: &'a PlainSignalOwner,
}

struct PlainApprovalPrompt {
    context: nib::tools::executor::ApprovalContext,
    reply: tokio::sync::oneshot::Sender<nib::tools::models::ApprovalDecision>,
}

const PLAIN_MODAL_IDLE: u8 = 0;
const PLAIN_MODAL_APPROVAL: u8 = 1;
const PLAIN_MODAL_QUESTION: u8 = 2;

#[derive(Clone, Default)]
struct PlainModalState {
    kind: Arc<AtomicU8>,
}

impl PlainModalState {
    fn claim(&self, kind: u8) -> bool {
        self.kind
            .compare_exchange(PLAIN_MODAL_IDLE, kind, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn clear(&self) {
        self.kind.store(PLAIN_MODAL_IDLE, Ordering::SeqCst);
    }

    fn is_pending(&self) -> bool {
        self.kind.load(Ordering::SeqCst) != PLAIN_MODAL_IDLE
    }

    #[cfg(test)]
    fn current(&self) -> u8 {
        self.kind.load(Ordering::SeqCst)
    }
}

struct PlainQuestionPrompt {
    question: String,
    options: Vec<String>,
    reply: tokio::sync::oneshot::Sender<Result<String, String>>,
}

enum PendingPlainModalResponse {
    Approval {
        decision: nib::tools::models::ApprovalDecision,
        reply: tokio::sync::oneshot::Sender<nib::tools::models::ApprovalDecision>,
    },
    Question {
        answer: Result<String, String>,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
}

impl PendingPlainModalResponse {
    fn deliver(self) {
        match self {
            Self::Approval { decision, reply } => {
                let _ = reply.send(decision);
            }
            Self::Question { answer, reply } => {
                let _ = reply.send(answer);
            }
        }
    }

    fn fail_closed(self) {
        match self {
            Self::Approval { reply, .. } => {
                let _ = reply.send(nib::tools::models::ApprovalDecision::denied());
            }
            Self::Question { reply, .. } => {
                let _ = reply.send(Err(
                    "console input closed before the modal frame delimiter".to_string()
                ));
            }
        }
    }
}

struct BrokeredPlainApprovalHandler {
    tx: tokio::sync::mpsc::UnboundedSender<PlainApprovalPrompt>,
    modal_state: PlainModalState,
}

struct BrokeredPlainQuestionHandler {
    tx: tokio::sync::mpsc::UnboundedSender<PlainQuestionPrompt>,
    modal_state: PlainModalState,
}

#[async_trait::async_trait]
impl nib::tools::executor::ApprovalHandler for BrokeredPlainApprovalHandler {
    async fn handle_approval(
        &self,
        call: &nib::tools::models::ToolCall,
        level: nib::tools::models::PermissionLevel,
    ) -> nib::tools::models::ApprovalDecision {
        self.handle_approval_with_context(
            call,
            level,
            &nib::tools::executor::ApprovalContext::compatibility(call, level),
        )
        .await
    }

    async fn handle_approval_with_context(
        &self,
        _call: &nib::tools::models::ToolCall,
        _level: nib::tools::models::PermissionLevel,
        context: &nib::tools::executor::ApprovalContext,
    ) -> nib::tools::models::ApprovalDecision {
        if !self.modal_state.claim(PLAIN_MODAL_APPROVAL) {
            return nib::tools::models::ApprovalDecision::denied();
        }
        let (reply, response) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(PlainApprovalPrompt {
                context: context.clone(),
                reply,
            })
            .is_err()
        {
            self.modal_state.clear();
            return nib::tools::models::ApprovalDecision::denied();
        }
        let decision = response
            .await
            .unwrap_or_else(|_| nib::tools::models::ApprovalDecision::denied());
        self.modal_state.clear();
        decision
    }
}

#[async_trait::async_trait]
impl nib::agent::QuestionHandler for BrokeredPlainQuestionHandler {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        if !self.modal_state.claim(PLAIN_MODAL_QUESTION) {
            return Err("another interactive prompt already owns plain input".to_string());
        }
        let (reply, response) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(PlainQuestionPrompt {
                question: question.to_string(),
                options: options.to_vec(),
                reply,
            })
            .is_err()
        {
            self.modal_state.clear();
            return Err("plain question input router stopped".to_string());
        }
        let answer = match response.await {
            Ok(answer) => answer,
            Err(_) => {
                self.modal_state.clear();
                return Err("plain question input router stopped".to_string());
            }
        };
        self.modal_state.clear();
        answer
    }
}

static PLAIN_SIGNAL_STATE: OnceLock<Result<Arc<Mutex<PlainSignalRegistrationState>>, String>> =
    OnceLock::new();

impl PlainSignalOwner {
    fn install() -> Result<Self, String> {
        let state = PLAIN_SIGNAL_STATE.get_or_init(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("failed to initialize Ctrl+C handling: {error}"))?;
            let state = Arc::new(Mutex::new(PlainSignalRegistrationState::default()));
            let signal_state = state.clone();
            std::thread::Builder::new()
                .name("nib-plain-signal".to_string())
                .spawn(move || {
                    runtime.block_on(async move {
                        while tokio::signal::ctrl_c().await.is_ok() {
                            let cancellation = signal_state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .active
                                .as_ref()
                                .map(|(_, cancellation)| cancellation.clone());
                            if let Some(cancellation) = cancellation {
                                cancellation.cancel();
                            } else {
                                std::process::exit(130);
                            }
                        }
                    });
                })
                .map_err(|error| format!("failed to start Ctrl+C handling: {error}"))?;
            Ok(state)
        });
        state
            .as_ref()
            .map(|state| Self {
                state: state.clone(),
            })
            .map_err(Clone::clone)
    }

    fn register(&self, cancellation: nib::agent::CancellationSignal) -> PlainSignalRegistration {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        let generation = state.next_generation;
        state.active = Some((generation, cancellation));
        PlainSignalRegistration {
            state: self.state.clone(),
            generation,
        }
    }

    #[cfg(test)]
    fn detached() -> Self {
        Self {
            state: Arc::new(Mutex::new(PlainSignalRegistrationState::default())),
        }
    }

    #[cfg(test)]
    fn dispatch_for_test(&self) -> bool {
        let cancellation = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .as_ref()
            .map(|(_, cancellation)| cancellation.clone());
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            false
        } else {
            true
        }
    }
}

impl Drop for PlainSignalRegistration {
    fn drop(&mut self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .active
            .as_ref()
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            state.active = None;
        }
    }
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
    if let Some(session_id) = args.session.as_deref() {
        config.validate_public_session_id(session_id)?;
    }
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
    if let Some(session_id) = args.session.as_deref() {
        config.validate_public_session_id(session_id)?;
    }
    run_plain_with_input(args, &project, config, ConsoleInput::new(reader))
}

#[cfg(test)]
fn run_chat_with_modal_state_input(
    args: &ChatArgs,
    reader: impl io::BufRead + Send + 'static,
    modal_state: PlainModalState,
) -> Result<(), String> {
    let project = std::env::current_dir()
        .map_err(|error| format!("failed to resolve the current project directory: {error}"))?;
    let config = prepare_interactive_config(&project, args.auth)?;
    if let Some(session_id) = args.session.as_deref() {
        config.validate_public_session_id(session_id)?;
    }
    run_plain_with_input_and_modal_state(
        args,
        &project,
        config,
        ConsoleInput::new(reader),
        modal_state,
    )
}

fn prepare_interactive_config(
    project: &Path,
    authenticate: bool,
) -> Result<nib::config::NibConfig, String> {
    let mut config = load_nib_config_full(project).map_err(|error| error.to_string())?;
    if authenticate || config.llm.providers.is_empty() {
        run_auth_wizard()?;
        config = load_nib_config_full(project).map_err(|error| error.to_string())?;
    }
    Ok(config)
}

fn plain_goodbye(
    session_store: &SessionStore,
    session_id: &str,
    sensitive_values: &[String],
) -> String {
    let message = format!(
        "Goodbye. Session saved to {}",
        session_store
            .sessions_dir()
            .join(format!("{session_id}.json"))
            .display()
    );
    nib::interactive::bounded_public_text(&message, sensitive_values, 64 * 1024, true)
}

fn run_plain_with_input(
    args: &ChatArgs,
    project: &Path,
    config: nib::config::NibConfig,
    input: ConsoleInput,
) -> Result<(), String> {
    run_plain_with_input_and_modal_state(args, project, config, input, PlainModalState::default())
}

fn run_plain_with_input_and_modal_state(
    args: &ChatArgs,
    project: &Path,
    config: nib::config::NibConfig,
    input: ConsoleInput,
    modal_state: PlainModalState,
) -> Result<(), String> {
    let active = config.llm.get_active_provider();
    let sensitive_values = config.public_session_sensitive_values();
    let public_output = |value: &str| {
        nib::interactive::bounded_public_text(value, &sensitive_values, 64 * 1024, true)
    };
    let profile_scope = resolve_interactive_profile_scope(project)?;
    let profile_id = profile_scope.profile_id().to_string();
    let session_store = profile_scope.into_session_store();
    let signal_owner = PlainSignalOwner::install()?;
    let agent_scope = PlainAgentScope {
        project,
        profile_id: &profile_id,
        session_store: &session_store,
        signal_owner: &signal_owner,
    };

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

    let active = nib::interactive::bounded_public_text(&active, &sensitive_values, 512, false);
    println!("\nnib  |  mode: plain  |  session: {sid}  |  provider: {active}");
    if let Ok(status) = format_session_status(project, &profile_id, &session_store, &sid, "idle") {
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
            let short =
                nib::interactive::bounded_public_text(&msg.content, &sensitive_values, 512, true);
            println!("{prefix}: {short}");
        }
    }

    if let Some(goal) = args.run.as_deref() {
        println!("Thinking...");
        match execute_plain_turn_and_queued_follow_ups(
            &agent_scope,
            &sid,
            goal,
            InteractiveAgentMode::Execute,
            &input,
            modal_state.clone(),
        ) {
            Ok(PlainAgentDisposition::Completed) => {}
            Ok(PlainAgentDisposition::Cancelled) => println!(
                "{}",
                chat_queue_disposition(&session_store, &sid, "cancelled active run")
            ),
            Ok(PlainAgentDisposition::Failed) => println!(
                "{}",
                chat_queue_disposition(&session_store, &sid, "failed active run")
            ),
            Ok(PlainAgentDisposition::QuitRequested(terminal)) => {
                println!("{}", plain_quit_disposition(&session_store, &sid, terminal));
                return Ok(());
            }
            Err(error) => println!("{error}"),
        }
    }

    let mut draft_history = DraftHistory::default();

    // Main REPL
    'repl: loop {
        print!("\nYou> ");
        io::stdout().flush().unwrap();

        let line = match input.read_line_blocking() {
            Ok(line) => line,
            Err(error) if error.contains("input closed") => break,
            Err(error) => return Err(error),
        };
        let mut submitted = line.trim().to_string();
        draft_history.remember_submission(&submitted);
        let state = InteractionState::default();
        let mut reduction = reduce_interaction(&state, InteractionInput::SubmittedLine(&submitted));
        let mut completion_offered = false;
        loop {
            match &reduction {
                InteractionReduction::Error { message, .. } if !completion_offered => {
                    println!("{message}");
                    completion_offered = true;
                    let completed = match select_command_completion_from_console(&input, &submitted)
                    {
                        Ok(completed) => completed,
                        Err(error) => {
                            println!("Could not complete command: {error}");
                            continue 'repl;
                        }
                    };
                    let Some(completed) = completed else {
                        continue 'repl;
                    };
                    submitted = completed;
                    draft_history.remember_submission(&submitted);
                    reduction =
                        reduce_interaction(&state, InteractionInput::SubmittedLine(&submitted));
                }
                InteractionReduction::Error { message, .. } => {
                    println!("{message}");
                    continue 'repl;
                }
                InteractionReduction::OpenHistorySearch { query } => {
                    draft_history.discard_latest_if(&submitted);
                    let restored = match select_draft_history_from_console(
                        &input,
                        &draft_history,
                        query.as_deref(),
                    ) {
                        Ok(restored) => restored,
                        Err(error) => {
                            println!("Could not search draft history: {error}");
                            continue 'repl;
                        }
                    };
                    let Some(restored) = restored else {
                        continue 'repl;
                    };
                    submitted = restored;
                    draft_history.remember_submission(&submitted);
                    completion_offered = false;
                    reduction =
                        reduce_interaction(&state, InteractionInput::SubmittedLine(&submitted));
                }
                _ => break,
            }
        }
        if let InteractionReduction::NoOp(_) = &reduction {
            continue;
        }
        if let InteractionReduction::QueueNext(queued) = &reduction {
            match persist_queued_follow_up(&session_store, &sid, queued, "composer") {
                Ok(_) => println!("queued follow-up retained on session {sid}"),
                Err(error) => println!("{error}"),
            }
            continue;
        }

        if let InteractionReduction::Command(command) = &reduction {
            match execute_interactive_command_in_state(
                command.clone(),
                project,
                &profile_id,
                &session_store,
                &sid,
                "idle",
            ) {
                Ok(InteractiveEffect::Quit) => {
                    println!("{}", chat_queue_disposition(&session_store, &sid, "exited"));
                    println!("{}", plain_goodbye(&session_store, &sid, &sensitive_values));
                    break;
                }
                Ok(InteractiveEffect::Output(output)) => println!("{}", public_output(&output)),
                Ok(InteractiveEffect::SessionChanged { session_id, output }) => {
                    let disposition =
                        chat_queue_disposition(&session_store, &sid, "switched sessions");
                    sid = session_id;
                    println!("{}", public_output(&output));
                    println!("{}", public_output(&disposition));
                }
                Ok(InteractiveEffect::SelectSession(selection)) => {
                    match select_session_from_console(&input, &session_store, &sid, &selection) {
                        Ok(ChatSessionAction::Activated(session_id)) => {
                            let disposition =
                                chat_queue_disposition(&session_store, &sid, "switched sessions");
                            sid = session_id;
                            println!("Resumed session {sid} from persisted state.");
                            println!("{}", public_output(&disposition));
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
                            Ok(output) => println!("{}", public_output(&output)),
                            Err(error) => eprintln!("{}", public_output(&error)),
                        }
                    }
                }
                Ok(InteractiveEffect::Compact) => {
                    println!("Compacting context...");
                    match execute_plain_turn_and_queued_follow_ups(
                        &agent_scope,
                        &sid,
                        "",
                        InteractiveAgentMode::Compact,
                        &input,
                        modal_state.clone(),
                    ) {
                        Ok(PlainAgentDisposition::Completed) => {}
                        Ok(PlainAgentDisposition::Cancelled) => println!(
                            "{}",
                            chat_queue_disposition(&session_store, &sid, "cancelled active run")
                        ),
                        Ok(PlainAgentDisposition::Failed) => println!(
                            "{}",
                            chat_queue_disposition(&session_store, &sid, "failed active run")
                        ),
                        Ok(PlainAgentDisposition::QuitRequested(terminal)) => {
                            println!("{}", plain_quit_disposition(&session_store, &sid, terminal));
                            break 'repl;
                        }
                        Err(error) => println!("{error}"),
                    }
                }
                Ok(InteractiveEffect::RunAgent { goal, mode }) => {
                    println!("Thinking...");
                    match execute_plain_turn_and_queued_follow_ups(
                        &agent_scope,
                        &sid,
                        &goal,
                        mode,
                        &input,
                        modal_state.clone(),
                    ) {
                        Ok(PlainAgentDisposition::Completed) => {}
                        Ok(PlainAgentDisposition::Cancelled) => println!(
                            "{}",
                            chat_queue_disposition(&session_store, &sid, "cancelled active run")
                        ),
                        Ok(PlainAgentDisposition::Failed) => println!(
                            "{}",
                            chat_queue_disposition(&session_store, &sid, "failed active run")
                        ),
                        Ok(PlainAgentDisposition::QuitRequested(terminal)) => {
                            println!("{}", plain_quit_disposition(&session_store, &sid, terminal));
                            break 'repl;
                        }
                        Err(error) => println!("{error}"),
                    }
                }
                Err(error) => println!("{error}"),
            }
            continue;
        }

        let InteractionReduction::IdleTurn(goal) = reduction else {
            println!("input was not applicable to the plain composer");
            continue;
        };

        println!("Thinking...");

        match execute_plain_turn_and_queued_follow_ups(
            &agent_scope,
            &sid,
            &goal,
            InteractiveAgentMode::Execute,
            &input,
            modal_state.clone(),
        ) {
            Ok(PlainAgentDisposition::Completed) => {}
            Ok(PlainAgentDisposition::Cancelled) => println!(
                "{}",
                chat_queue_disposition(&session_store, &sid, "cancelled active run")
            ),
            Ok(PlainAgentDisposition::Failed) => println!(
                "{}",
                chat_queue_disposition(&session_store, &sid, "failed active run")
            ),
            Ok(PlainAgentDisposition::QuitRequested(terminal)) => {
                println!("{}", plain_quit_disposition(&session_store, &sid, terminal));
                break 'repl;
            }
            Err(error) => println!("{error}"),
        }
    }

    Ok(())
}

fn require_plain_consumer(
    state: &InteractionState,
    line: &str,
    expected: InteractionConsumer,
) -> Result<(), String> {
    match reduce_interaction(state, InteractionInput::SubmittedLine(line)) {
        InteractionReduction::Consumed(consumer) if consumer == expected => Ok(()),
        InteractionReduction::Error { message, .. } => Err(message),
        _ => Err("plain interaction input was rejected by the shared reducer".to_string()),
    }
}

fn select_draft_history_from_console(
    input: &ConsoleInput,
    history: &DraftHistory,
    initial_query: Option<&str>,
) -> Result<Option<String>, String> {
    if history.is_empty() {
        println!("No submitted drafts are available in this process.");
        return Ok(None);
    }
    let query = if let Some(query) = initial_query {
        query.to_string()
    } else {
        print!("History query (blank lists recent drafts): ");
        io::stdout().flush().map_err(|error| error.to_string())?;
        let query = input.read_line_blocking()?;
        require_plain_consumer(
            &InteractionState {
                selector_or_detail: Some(SelectorDetailKind::Selector),
                ..InteractionState::default()
            },
            &query,
            InteractionConsumer::Selector,
        )?;
        query
    };
    let search = history.search(&query);
    if search.query_truncated {
        println!("History query was truncated to its bounded UTF-8 prefix.");
    }
    if search.controls_omitted {
        println!("Control characters were omitted from the history query.");
    }
    if search.matches.is_empty() {
        println!("No submitted drafts match the bounded query.");
        return Ok(None);
    }
    println!(
        "Draft history matches for {}:",
        if search.query.is_empty() {
            "(all)"
        } else {
            search.query.as_str()
        }
    );
    for (index, result) in search.matches.iter().enumerate() {
        println!("  {}. {}", index + 1, result.display);
    }
    print!("Draft to restore (number, blank to cancel): ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let choice = input.read_line_blocking()?;
    let selector_state = InteractionState {
        selector_or_detail: Some(SelectorDetailKind::Selector),
        ..InteractionState::default()
    };
    require_plain_consumer(&selector_state, &choice, InteractionConsumer::Selector)?;
    let choice = choice.trim();
    if choice.is_empty() {
        return Ok(None);
    }
    let selected = choice
        .parse::<usize>()
        .map_err(|_| "history selection must be a displayed number".to_string())?;
    if selected == 0 {
        return Err("history selection 0 is out of range".to_string());
    }
    let result = search
        .matches
        .get(selected - 1)
        .ok_or_else(|| format!("history selection {selected} is out of range"))?;
    let restored = history
        .entry(result.entry_index)
        .ok_or_else(|| "selected draft is no longer available".to_string())?;
    println!("Selected draft: {}", result.display);
    print!("Submit this restored draft now? [y/N]: ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let confirmation = input.read_line_blocking()?;
    require_plain_consumer(
        &selector_state,
        &confirmation,
        InteractionConsumer::Selector,
    )?;
    if confirmation.trim().eq_ignore_ascii_case("y") {
        Ok(Some(restored.to_string()))
    } else {
        Ok(None)
    }
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
    require_plain_consumer(
        &InteractionState {
            completion_pending: true,
            ..InteractionState::default()
        },
        &choice,
        InteractionConsumer::Completion,
    )?;
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
        require_plain_consumer(
            &InteractionState {
                completion_pending: true,
                ..InteractionState::default()
            },
            &arguments,
            InteractionConsumer::Completion,
        )?;
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
    require_plain_consumer(
        &InteractionState {
            selector_or_detail: Some(SelectorDetailKind::Selector),
            ..InteractionState::default()
        },
        &choice,
        InteractionConsumer::Selector,
    )?;
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
    require_plain_consumer(
        &InteractionState {
            destructive_confirmation_pending: true,
            ..InteractionState::default()
        },
        &confirmation,
        InteractionConsumer::DestructiveConfirmation,
    )?;
    confirm_session_candidate(store, candidate, &confirmation)
}

fn confirm_session_candidate(
    store: &SessionStore,
    candidate: nib::interactive::InteractiveSessionCandidate,
    confirmation: &str,
) -> Result<ChatSessionAction, String> {
    let state = InteractionState {
        destructive_confirmation_pending: true,
        ..InteractionState::default()
    };
    if reduce_interaction(&state, InteractionInput::ConfirmationAnswer(confirmation))
        != InteractionReduction::ConfirmationDecision(InteractionDecision::Accept)
    {
        return Ok(ChatSessionAction::Cancelled);
    }
    validate_interactive_session_target(store, &candidate)?;
    Ok(ChatSessionAction::Activated(candidate.id))
}

fn select_model_from_console(
    input: &ConsoleInput,
    selection: &ModelSelection,
) -> Result<Option<String>, String> {
    let safe_label = |value: &str| {
        nib::interactive::bounded_public_text(value, &selection.sensitive_values, 512, false)
    };
    let provider = safe_label(&selection.provider);
    if selection.available.is_empty() {
        println!(
            "No predefined list for {}. Type the full model name.",
            provider
        );
        print!("Model name: ");
    } else {
        println!("\nAvailable models for {provider}:");
        for (index, model) in selection.available.iter().enumerate() {
            let marker = if model == &selection.current {
                " (current)"
            } else {
                ""
            };
            println!("  {}. {}{}", index + 1, safe_label(model), marker);
        }
        print!("Selection (number or exact model): ");
    }
    io::stdout().flush().map_err(|error| error.to_string())?;
    let choice = input.read_line_blocking()?;
    require_plain_consumer(
        &InteractionState {
            selector_or_detail: Some(SelectorDetailKind::Selector),
            ..InteractionState::default()
        },
        &choice,
        InteractionConsumer::Selector,
    )?;
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

fn chat_queue_disposition(store: &SessionStore, session_id: &str, action: &str) -> String {
    queue_disposition_message(store, session_id, action).unwrap_or_else(|error| error)
}

fn plain_quit_disposition(
    store: &SessionStore,
    session_id: &str,
    terminal: InteractionTerminalOutcome,
) -> String {
    let (status, action) = match terminal {
        InteractionTerminalOutcome::Completed => (
            "[completed] active run completed before quit",
            "quit after completion",
        ),
        InteractionTerminalOutcome::Cancelled => (
            "[cancelled] active run reconciled before quit",
            "quit after cancellation",
        ),
        InteractionTerminalOutcome::WaitingForInput => (
            "[failed] active run still required input before quit",
            "quit with input unavailable",
        ),
        InteractionTerminalOutcome::Failed => (
            "[failed] active run reconciled with failure before quit",
            "quit after failure",
        ),
    };
    format!(
        "{status}; {}",
        chat_queue_disposition(store, session_id, action)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlainAgentDisposition {
    Completed,
    Cancelled,
    Failed,
    QuitRequested(InteractionTerminalOutcome),
}

struct PreparedPlainAgentStep {
    runtime: tokio::runtime::Runtime,
    renderer: Option<std::thread::JoinHandle<()>>,
    cancellation: nib::agent::CancellationSignal,
    steering: Option<nib::agent::ExactRunSteeringHandle>,
    approval_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PlainApprovalPrompt>>,
    question_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PlainQuestionPrompt>>,
    loop_cfg: Option<nib::agent::AgentLoopConfig>,
}

impl PreparedPlainAgentStep {
    fn prepare(
        scope: &PlainAgentScope<'_>,
        session_id: &str,
        mode: InteractiveAgentMode,
        modal_state: PlainModalState,
    ) -> Result<Self, String> {
        let sensitive_values = nib::config::load_nib_config_full(scope.project)
            .map_err(|error| error.to_string())?
            .public_session_sensitive_values();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("failed to initialize the async runtime: {error}"))?;
        let cancellation = nib::agent::CancellationSignal::new();
        let run_id = uuid::Uuid::new_v4().simple().to_string();
        let (steering, steering_receiver) = if mode == InteractiveAgentMode::Compact {
            (None, None)
        } else {
            let (steering, receiver) = nib::agent::exact_run_steering_channel(
                scope.session_store.clone(),
                session_id.to_string(),
                run_id.clone(),
                "plain",
            )?;
            (Some(steering), Some(receiver))
        };
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(100);
        let renderer = std::thread::Builder::new()
            .name("nib-plain-stream".to_string())
            .spawn(move || {
                while let Some(event) = stream_rx.blocking_recv() {
                    let Some(display) =
                        nib::interactive::display_stream_event_with_sensitive_values(
                            event,
                            &sensitive_values,
                        )
                    else {
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
        let (approval_tx, approval_rx) = tokio::sync::mpsc::unbounded_channel();
        let (question_tx, question_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_cfg = nib::agent::AgentLoopConfig {
            max_steps: 0,
            mode: mode.as_str().to_string(),
            provider: None,
            auto_approve: false,
            approval_handler: Some(Arc::new(BrokeredPlainApprovalHandler {
                tx: approval_tx,
                modal_state: modal_state.clone(),
            })),
            question_handler: Some(Arc::new(BrokeredPlainQuestionHandler {
                tx: question_tx,
                modal_state: modal_state.clone(),
            })),
            stream_tx: Some(stream_tx),
            cancellation: Some(cancellation.clone()),
            run_id: Some(run_id),
            steering: steering_receiver,
            ..Default::default()
        };
        Ok(Self {
            runtime,
            renderer: Some(renderer),
            cancellation,
            steering,
            approval_rx: Some(approval_rx),
            question_rx: Some(question_rx),
            loop_cfg: Some(loop_cfg),
        })
    }
}

impl Drop for PreparedPlainAgentStep {
    fn drop(&mut self) {
        self.loop_cfg.take();
        if let Some(renderer) = self.renderer.take() {
            let _ = renderer.join();
        }
    }
}

fn parse_plain_question_answer(line: &str, options: &[String]) -> Result<String, String> {
    let state = InteractionState {
        question_pending: true,
        ..InteractionState::default()
    };
    match reduce_interaction(
        &state,
        InteractionInput::QuestionAnswer {
            answer: line,
            options,
            selected_option: None,
        },
    ) {
        InteractionReduction::QuestionAnswered(answer) => Ok(answer),
        InteractionReduction::Error { message, .. } => Err(message),
        _ => Err("plain question input was rejected by the shared reducer".to_string()),
    }
}

fn plain_approval_decision(line: &str) -> nib::tools::models::ApprovalDecision {
    let state = InteractionState {
        approval_pending: true,
        ..InteractionState::default()
    };
    match reduce_interaction(&state, InteractionInput::ApprovalAnswer(line)) {
        InteractionReduction::ApprovalDecision(InteractionDecision::Accept) => {
            nib::tools::models::ApprovalDecision::granted_user()
        }
        _ => nib::tools::models::ApprovalDecision::denied(),
    }
}

fn request_plain_modal_frame_delimiter() {
    println!("[modal] response recorded; press Enter on an empty line to return input ownership");
}

fn submit_plain_steering(
    steering: Option<&nib::agent::ExactRunSteeringHandle>,
    text: &str,
) -> Result<usize, String> {
    steering
        .ok_or_else(|| "this active operation does not accept exact-run steering".to_string())?
        .submit(text)
}

#[cfg(test)]
fn execute_agent_step(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    goal: &str,
    mode: InteractiveAgentMode,
    input: &ConsoleInput,
) -> Result<PlainAgentDisposition, String> {
    execute_agent_step_with_modal_state(
        scope,
        session_id,
        goal,
        mode,
        input,
        PlainModalState::default(),
    )
}

fn execute_agent_step_with_modal_state(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    goal: &str,
    mode: InteractiveAgentMode,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    let prepared = PreparedPlainAgentStep::prepare(scope, session_id, mode, modal_state.clone())?;
    execute_prepared_agent_step(prepared, scope, session_id, goal, input, modal_state)
}

fn execute_prepared_agent_step(
    mut prepared: PreparedPlainAgentStep,
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    goal: &str,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    let cancellation = prepared.cancellation.clone();
    let steering = prepared.steering.take();
    let mut approval_rx = prepared
        .approval_rx
        .take()
        .ok_or_else(|| "prepared plain agent has no approval channel".to_string())?;
    let mut question_rx = prepared
        .question_rx
        .take()
        .ok_or_else(|| "prepared plain agent has no question channel".to_string())?;
    let loop_cfg = prepared
        .loop_cfg
        .take()
        .ok_or_else(|| "prepared plain agent has no runtime configuration".to_string())?;

    let _signal_registration = scope.signal_owner.register(cancellation.clone());
    if steering.is_some() {
        println!("Run active: Enter queues; use steer: <text> for the exact active run.");
    } else {
        println!("Maintenance active: exact-run steering is unavailable; Enter queues.");
    }
    let (result, quit_requested) = prepared.runtime.block_on(async {
        let mut agent = Box::pin(nib::agent::run_agent_loop_for_profile(
            scope.project.to_path_buf(),
            scope.profile_id,
            scope.session_store.sessions_dir(),
            session_id,
            goal,
            loop_cfg,
        ));
        let mut pending_approval: Option<PlainApprovalPrompt> = None;
        let mut pending_question: Option<PlainQuestionPrompt> = None;
        let mut pending_modal_response: Option<PendingPlainModalResponse> = None;
        let mut buffered_modal_line: Option<String> = None;
        let mut input_open = true;
        let mut quit_requested = false;

        loop {
            tokio::select! {
                biased;
                result = &mut agent => {
                    if let Some(prompt) = pending_approval.take() {
                        let _ = prompt.reply.send(nib::tools::models::ApprovalDecision::denied());
                    }
                    if let Some(prompt) = pending_question.take() {
                        let _ = prompt.reply.send(Err("agent run ended before question input".to_string()));
                    }
                    if let Some(response) = pending_modal_response.take() {
                        response.fail_closed();
                    }
                    modal_state.clear();
                    break (result, quit_requested);
                }
                Some(prompt) = approval_rx.recv(), if pending_approval.is_none() => {
                    if input_open {
                        eprintln!("\nApproval required\n{}", prompt.context.render());
                        eprint!("Approve? [y/N]: ");
                        let _ = io::stderr().flush();
                        pending_approval = Some(prompt);
                        if let Some(line) = buffered_modal_line.take() {
                            let prompt = pending_approval.take().expect("approval prompt was installed");
                            let decision = plain_approval_decision(&line);
                            pending_modal_response = Some(PendingPlainModalResponse::Approval {
                                decision,
                                reply: prompt.reply,
                            });
                            request_plain_modal_frame_delimiter();
                        }
                    } else {
                        modal_state.clear();
                        let _ = prompt.reply.send(nib::tools::models::ApprovalDecision::denied());
                    }
                }
                Some(prompt) = question_rx.recv(), if pending_question.is_none() => {
                    if input_open {
                        println!("\nQuestion: {}", prompt.question);
                        for (index, option) in prompt.options.iter().enumerate() {
                            println!("  {}. {}", index + 1, option);
                        }
                        if prompt.options.is_empty() {
                            print!("Answer: ");
                        } else {
                            print!("Answer (number or text): ");
                        }
                        let _ = io::stdout().flush();
                        pending_question = Some(prompt);
                        if let Some(line) = buffered_modal_line.take() {
                            let prompt = pending_question.take().expect("question prompt was installed");
                            let answer = parse_plain_question_answer(&line, &prompt.options);
                            pending_modal_response = Some(PendingPlainModalResponse::Question {
                                answer,
                                reply: prompt.reply,
                            });
                            request_plain_modal_frame_delimiter();
                        }
                    } else {
                        modal_state.clear();
                        let _ = prompt.reply.send(Err("console input closed before a response was received".to_string()));
                    }
                }
                line = input.read_line_async(), if input_open && buffered_modal_line.is_none() => {
                    let line = match line {
                        Ok(line) => line,
                        Err(_) => {
                            input_open = false;
                            if let Some(response) = pending_modal_response.take() {
                                response.fail_closed();
                                modal_state.clear();
                            }
                            if let Some(prompt) = pending_approval.take() {
                                let _ = prompt.reply.send(nib::tools::models::ApprovalDecision::denied());
                            }
                            if let Some(prompt) = pending_question.take() {
                                let _ = prompt.reply.send(Err("console input closed before a response was received".to_string()));
                            }
                            continue;
                        }
                    };
                    if pending_modal_response.is_some() {
                        if line.trim().is_empty() {
                            let response = pending_modal_response
                                .take()
                                .expect("checked pending modal response");
                            modal_state.clear();
                            response.deliver();
                        } else {
                            println!(
                                "[input rejected] surplus modal line was not applied; press Enter on an empty line to return input ownership"
                            );
                        }
                        continue;
                    }
                    if let Some(prompt) = pending_approval.take() {
                        let decision = plain_approval_decision(&line);
                        pending_modal_response = Some(PendingPlainModalResponse::Approval {
                            decision,
                            reply: prompt.reply,
                        });
                        request_plain_modal_frame_delimiter();
                        continue;
                    }
                    if let Some(prompt) = pending_question.take() {
                        let answer = parse_plain_question_answer(&line, &prompt.options);
                        pending_modal_response = Some(PendingPlainModalResponse::Question {
                            answer,
                            reply: prompt.reply,
                        });
                        request_plain_modal_frame_delimiter();
                        continue;
                    }
                    if modal_state.is_pending() {
                        buffered_modal_line = Some(line);
                        continue;
                    }

                    let state = InteractionState {
                        run: InteractionRunState::Running,
                        ..InteractionState::default()
                    };
                    match reduce_interaction(&state, InteractionInput::SubmittedLine(&line)) {
                        InteractionReduction::SteerCurrent(text) => match submit_plain_steering(
                            steering.as_ref(),
                            &text,
                        ) {
                            Ok(sequence) => println!(
                                "steering accepted for the next safe boundary (#{sequence})"
                            ),
                            Err(error) => println!("steering was not accepted: {error}"),
                        },
                        InteractionReduction::QueueNext(text) => {
                            match persist_queued_follow_up(
                                scope.session_store,
                                session_id,
                                &text,
                                "plain_active_run",
                            ) {
                                Ok(_) => println!("queued follow-up retained on session {session_id}"),
                                Err(error) => println!("{error}"),
                            }
                        }
                        InteractionReduction::Command(nib::interactive::InteractiveCommand::Quit) => {
                            quit_requested = true;
                            cancellation.cancel();
                            println!("quit requested; reconciling the active run first");
                        }
                        InteractionReduction::Error { message, .. } => println!("{message}"),
                        InteractionReduction::NoOp(_) => {}
                        _ => println!("input was not applicable while the run was active"),
                    }
                }
            }
        }
    });

    prepared
        .renderer
        .take()
        .ok_or_else(|| "prepared plain agent has no stream renderer".to_string())?
        .join()
        .map_err(|_| "plain stream renderer panicked".to_string())?;

    result.and_then(|summary| {
        let terminal = match reduce_interaction(
            &InteractionState::default(),
            InteractionInput::ReconciledOutcome {
                outcome: &summary.outcome,
                failure: summary.is_failure(),
            },
        ) {
            InteractionReduction::Reconciled { terminal, .. } => terminal,
            _ => unreachable!("reconciliation input always has a terminal reduction"),
        };
        if quit_requested {
            Ok(PlainAgentDisposition::QuitRequested(terminal))
        } else if terminal == InteractionTerminalOutcome::WaitingForInput {
            Err(format!(
                "question input was unavailable; session {session_id} was reconciled without continuing"
            ))
        } else if terminal == InteractionTerminalOutcome::Cancelled {
            Ok(PlainAgentDisposition::Cancelled)
        } else if terminal == InteractionTerminalOutcome::Failed && summary.failure.is_some() {
            // The agent emits its structured failure on the lifecycle stream after
            // reconciliation, so plain mode has already rendered the single safe report.
            Ok(PlainAgentDisposition::Failed)
        } else if terminal == InteractionTerminalOutcome::Failed {
            Err(summary.user_failure_report().unwrap_or_else(|| {
                format!("Agent run failed: {}\nSession: {session_id}", summary.outcome)
            }))
        } else {
            Ok(PlainAgentDisposition::Completed)
        }
    })
}

fn execute_plain_turn_and_queued_follow_ups(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    goal: &str,
    mode: InteractiveAgentMode,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    match execute_agent_step_with_modal_state(
        scope,
        session_id,
        goal,
        mode,
        input,
        modal_state.clone(),
    )? {
        PlainAgentDisposition::Completed => {
            drain_plain_queued_follow_ups(scope, session_id, input, modal_state)
        }
        disposition => Ok(disposition),
    }
}

fn drain_plain_queued_follow_ups(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    loop {
        let Some((queued, prepared)) =
            claim_next_queued_follow_up_after_startup(scope.session_store, session_id, |_| {
                PreparedPlainAgentStep::prepare(
                    scope,
                    session_id,
                    InteractiveAgentMode::Execute,
                    modal_state.clone(),
                )
            })?
        else {
            return Ok(PlainAgentDisposition::Completed);
        };
        println!("Starting queued follow-up.");
        match execute_prepared_agent_step(
            prepared,
            scope,
            session_id,
            &queued.text,
            input,
            modal_state.clone(),
        )? {
            PlainAgentDisposition::Completed => {}
            disposition => return Ok(disposition),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib::config::{load_nib_config_full, save_nib_config_full, NibConfig};
    use serial_test::serial;
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::io::{BufRead, Cursor, Read};
    use std::path::PathBuf;
    use tempfile::tempdir;

    struct ScriptedLineReader {
        lines: VecDeque<(std::time::Duration, String)>,
        current: Vec<u8>,
        offset: usize,
    }

    #[derive(Clone, Copy)]
    enum ModalInputStep {
        Immediate(&'static str),
        Delayed(u64, &'static str),
        AfterModal(u8, &'static str),
        AfterModalCycle(u8, &'static str),
    }

    struct ModalSynchronizedReader {
        steps: VecDeque<ModalInputStep>,
        modal_state: PlainModalState,
        current: Vec<u8>,
        offset: usize,
    }

    impl ModalSynchronizedReader {
        fn new(
            modal_state: PlainModalState,
            steps: impl IntoIterator<Item = ModalInputStep>,
        ) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                modal_state,
                current: Vec::new(),
                offset: 0,
            }
        }
    }

    impl Read for ModalSynchronizedReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let count = available.len().min(output.len());
            output[..count].copy_from_slice(&available[..count]);
            self.consume(count);
            Ok(count)
        }
    }

    impl BufRead for ModalSynchronizedReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.offset >= self.current.len() {
                let Some(step) = self.steps.pop_front() else {
                    return Ok(&[]);
                };
                let line = match step {
                    ModalInputStep::Immediate(line) => line,
                    ModalInputStep::Delayed(delay_ms, line) => {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                        line
                    }
                    ModalInputStep::AfterModal(expected, line)
                    | ModalInputStep::AfterModalCycle(expected, line) => {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(20);
                        if matches!(step, ModalInputStep::AfterModalCycle(_, _)) {
                            while self.modal_state.current() != PLAIN_MODAL_IDLE {
                                if std::time::Instant::now() >= deadline {
                                    return Err(io::Error::new(
                                        io::ErrorKind::TimedOut,
                                        "plain modal did not release input before the test deadline",
                                    ));
                                }
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                        }
                        while self.modal_state.current() != expected {
                            if std::time::Instant::now() >= deadline {
                                return Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "plain modal did not claim input before the test deadline",
                                ));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        line
                    }
                };
                self.current = line.as_bytes().to_vec();
                self.offset = 0;
            }
            Ok(&self.current[self.offset..])
        }

        fn consume(&mut self, amount: usize) {
            self.offset = self.offset.saturating_add(amount).min(self.current.len());
        }
    }

    impl ScriptedLineReader {
        fn new(lines: impl IntoIterator<Item = (u64, &'static str)>) -> Self {
            Self {
                lines: lines
                    .into_iter()
                    .map(|(delay_ms, line)| {
                        (std::time::Duration::from_millis(delay_ms), line.to_string())
                    })
                    .collect(),
                current: Vec::new(),
                offset: 0,
            }
        }
    }

    impl Read for ScriptedLineReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let count = available.len().min(output.len());
            output[..count].copy_from_slice(&available[..count]);
            self.consume(count);
            Ok(count)
        }
    }

    impl BufRead for ScriptedLineReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.offset >= self.current.len() {
                let Some((delay, line)) = self.lines.pop_front() else {
                    return Ok(&[]);
                };
                std::thread::sleep(delay);
                self.current = line.into_bytes();
                self.offset = 0;
            }
            Ok(&self.current[self.offset..])
        }

        fn consume(&mut self, amount: usize) {
            self.offset = self.offset.saturating_add(amount).min(self.current.len());
        }
    }

    #[test]
    fn plain_signal_owner_exits_when_idle_and_cancels_only_the_registered_turn() {
        let owner = PlainSignalOwner::detached();
        assert!(owner.dispatch_for_test(), "idle Ctrl+C owns process exit");

        let cancellation = nib::agent::CancellationSignal::new();
        let registration = owner.register(cancellation.clone());
        assert!(!owner.dispatch_for_test(), "active Ctrl+C cancels the turn");
        assert!(cancellation.is_cancelled());

        drop(registration);
        assert!(
            owner.dispatch_for_test(),
            "turn completion restores idle exit"
        );
    }

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

    struct EnvironmentGuard {
        name: &'static str,
        previous: Option<OsString>,
    }

    impl EnvironmentGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            restore_env(self.name, self.previous.take());
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
    fn shared_interaction_reducer_routes_plain_numbered_layers() {
        for (state, expected) in [
            (
                InteractionState {
                    destructive_confirmation_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::DestructiveConfirmation,
            ),
            (
                InteractionState {
                    selector_or_detail: Some(SelectorDetailKind::Selector),
                    ..InteractionState::default()
                },
                InteractionConsumer::Selector,
            ),
            (
                InteractionState {
                    completion_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::Completion,
            ),
        ] {
            require_plain_consumer(&state, "1\n", expected).expect("owned numbered input");
        }
    }

    #[test]
    fn plain_draft_history_search_uses_bounded_numbered_selection_and_confirmation() {
        let mut history = DraftHistory::default();
        history.remember_submission("older draft");
        history.remember_submission("fix 🙂 unicode");
        let input = ConsoleInput::new(std::io::Cursor::new(b"1\ny\n"));

        let selected = select_draft_history_from_console(&input, &history, Some("🙂"))
            .expect("history search")
            .expect("selected draft");
        assert_eq!(selected, "fix 🙂 unicode");

        let no_input = ConsoleInput::new(std::io::Cursor::new(Vec::<u8>::new()));
        assert_eq!(
            select_draft_history_from_console(&no_input, &history, Some("no match"))
                .expect("bounded no-match search"),
            None
        );

        let invalid = ConsoleInput::new(std::io::Cursor::new(b"0\n"));
        assert!(
            select_draft_history_from_console(&invalid, &history, Some("draft"))
                .expect_err("zero is never a valid numbered selection")
                .contains("out of range")
        );
    }

    #[test]
    fn plain_draft_history_empty_search_requires_no_input() {
        let input = ConsoleInput::new(std::io::Cursor::new(Vec::<u8>::new()));
        assert_eq!(
            select_draft_history_from_console(&input, &DraftHistory::default(), None)
                .expect("empty history"),
            None
        );
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
            Cursor::new(b"y\n\n/quit\n"),
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
    fn plain_plan_mode_persists_an_unapproved_plan_without_execution() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
        let signal_owner = PlainSignalOwner::detached();
        let scope = PlainAgentScope {
            project: project.path(),
            profile_id: "default",
            session_store: &store,
            signal_owner: &signal_owner,
        };

        execute_agent_step(
            &scope,
            &session.id,
            "plan a safe inspection",
            InteractiveAgentMode::Plan,
            &input,
        )
        .expect("plan-only run");

        let persisted = store.load(&session.id).expect("planned session");
        let plan = persisted.plan.expect("structured plan");
        assert!(plan.is_structured());
        assert!(!plan.approved);
        assert!(persisted.tool_calls.is_empty());
        assert!(!persisted
            .events
            .iter()
            .any(|event| event.kind == "approval_required"));
        assert!(persisted.events.iter().any(|event| {
            event.kind == "reconciliation" && event.details["outcome"] == "plan_ready"
        }));
    }

    #[test]
    fn plain_compaction_rejects_steering_without_persisting_an_input_channel() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        store
            .try_append_message(&session.id, "user", "compact this retained context")
            .expect("user context");
        store
            .try_append_message(&session.id, "assistant", "retained answer")
            .expect("assistant context");
        let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
        let signal_owner = PlainSignalOwner::detached();
        let scope = PlainAgentScope {
            project: project.path(),
            profile_id: "default",
            session_store: &store,
            signal_owner: &signal_owner,
        };

        assert_eq!(
            execute_agent_step(
                &scope,
                &session.id,
                "",
                InteractiveAgentMode::Compact,
                &input,
            )
            .expect("plain compaction"),
            PlainAgentDisposition::Completed
        );
        assert!(submit_plain_steering(None, "must not steer maintenance")
            .expect_err("compact steering is unavailable")
            .contains("does not accept"));
        let persisted = store.load(&session.id).expect("compacted session");
        assert!(!persisted.events.iter().any(|event| {
            matches!(
                event.kind.as_str(),
                "steering_channel_bound" | "steering_admission" | "steering_input"
            )
        }));
    }

    #[test]
    #[serial]
    fn plain_active_router_distinguishes_exact_steering_from_durable_queue_input() {
        let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let goal = "exact run steering response smoke";
        let mut session = store.create_session();
        let mut plan = nib::session::Plan::new(
            goal,
            vec![nib::session::PlanStep {
                description: "wait for exact steering".to_string(),
                status: "Pending".to_string(),
                outcome: None,
                attempts: 0,
                updated_at: None,
            }],
        );
        plan.approve();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved plan");
        let input = ConsoleInput::new(ScriptedLineReader::new([
            (
                200,
                "steer: replacement steering marker; answer without tools\n",
            ),
            (100, "queue: verify the persisted result next\n"),
        ]));
        let signal_owner = PlainSignalOwner::detached();
        let scope = PlainAgentScope {
            project: project.path(),
            profile_id: "default",
            session_store: &store,
            signal_owner: &signal_owner,
        };

        assert_eq!(
            execute_agent_step(
                &scope,
                &session.id,
                goal,
                InteractiveAgentMode::Execute,
                &input,
            )
            .expect("plain steered run"),
            PlainAgentDisposition::Completed
        );
        let persisted = store.load(&session.id).expect("plain steering state");
        assert!(persisted.events.iter().any(|event| {
            event.kind == "steering_input"
                && event.details["source"] == "plain"
                && event.details["text"] == "replacement steering marker; answer without tools"
        }));
        assert!(persisted
            .events
            .iter()
            .any(|event| event.kind == "steering_intake"));
        assert_eq!(persisted.queued_follow_ups.len(), 1);
        assert_eq!(
            persisted.queued_follow_ups[0].text,
            "verify the persisted result next"
        );
        assert!(persisted
            .messages
            .iter()
            .all(|message| { message.content != "verify the persisted result next" }));
    }

    #[test]
    fn plain_queue_startup_failure_retains_fifo_and_records_disposition() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        persist_queued_follow_up(&store, &session.id, "first retained", "plain")
            .expect("first queue item");
        persist_queued_follow_up(&store, &session.id, "second retained", "plain")
            .expect("second queue item");
        std::fs::write(project.path().join(".nib/config.toml"), "invalid = [")
            .expect("corrupt config fixture");
        let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
        let signal_owner = PlainSignalOwner::detached();
        let scope = PlainAgentScope {
            project: project.path(),
            profile_id: "default",
            session_store: &store,
            signal_owner: &signal_owner,
        };

        let error =
            drain_plain_queued_follow_ups(&scope, &session.id, &input, PlainModalState::default())
                .expect_err("invalid config prevents prepared startup");
        assert!(error.contains("remains queued"), "{error}");
        let persisted = store.load(&session.id).expect("retained queue");
        assert_eq!(
            persisted
                .queued_follow_ups
                .iter()
                .map(|item| item.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first retained", "second retained"]
        );
        assert!(persisted.events.iter().any(|event| {
            event.kind == "queued_follow_up_start_failed"
                && event.details["phase"] == "worker_startup"
                && event.details["disposition"] == "retained"
        }));
    }

    #[test]
    #[serial]
    fn plain_execution_failure_retains_queued_work_instead_of_launching_it() {
        let _interactive_smoke = EnvironmentGuard::set("NIB_ENABLE_INTERACTIVE_SMOKE", "1");
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        persist_queued_follow_up(
            &store,
            &session.id,
            "must remain after execution failure",
            "plain",
        )
        .expect("queued follow-up");
        let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
        let signal_owner = PlainSignalOwner::detached();
        let scope = PlainAgentScope {
            project: project.path(),
            profile_id: "default",
            session_store: &store,
            signal_owner: &signal_owner,
        };

        assert_eq!(
            execute_plain_turn_and_queued_follow_ups(
                &scope,
                &session.id,
                "interactive provider failure smoke",
                InteractiveAgentMode::Execute,
                &input,
                PlainModalState::default(),
            )
            .expect("typed failure reconciles"),
            PlainAgentDisposition::Failed
        );
        let persisted = store.load(&session.id).expect("failed session");
        assert_eq!(persisted.queued_follow_ups.len(), 1);
        assert_eq!(
            persisted.queued_follow_ups[0].text,
            "must remain after execution failure"
        );
        assert!(persisted
            .messages
            .iter()
            .all(|message| message.content != "must remain after execution failure"));
        assert!(persisted.events.iter().any(|event| {
            event.kind == "reconciliation" && event.details["outcome"] == "planning_failed"
        }));
    }

    #[test]
    fn plain_modal_typeahead_is_rejected_instead_of_becoming_a_queued_goal() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        let modal_state = PlainModalState::default();
        let input = ConsoleInput::new(ModalSynchronizedReader::new(
            modal_state.clone(),
            [
                ModalInputStep::AfterModal(PLAIN_MODAL_APPROVAL, "y\n"),
                ModalInputStep::Delayed(25, "surplus modal paste must not execute\n"),
                ModalInputStep::Delayed(25, "\n"),
            ],
        ));
        let signal_owner = PlainSignalOwner::detached();
        let scope = PlainAgentScope {
            project: project.path(),
            profile_id: "default",
            session_store: &store,
            signal_owner: &signal_owner,
        };

        assert_eq!(
            execute_agent_step_with_modal_state(
                &scope,
                &session.id,
                "list workspace after provider recovery",
                InteractiveAgentMode::Execute,
                &input,
                modal_state,
            )
            .expect("approved run completes"),
            PlainAgentDisposition::Completed
        );
        let persisted = store.load(&session.id).expect("completed session");
        assert!(persisted.queued_follow_ups.is_empty());
        assert!(persisted.messages.iter().all(|message| {
            !message
                .content
                .contains("surplus modal paste must not execute")
        }));
    }

    #[test]
    #[serial]
    fn plain_cancellation_retains_queued_work_instead_of_launching_it() {
        let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        persist_queued_follow_up(
            &store,
            &session.id,
            "must remain after cancellation",
            "plain",
        )
        .expect("queued follow-up");
        let input = ConsoleInput::new(Cursor::new(Vec::<u8>::new()));
        let signal_owner = PlainSignalOwner::detached();
        let cancelling_owner = signal_owner.clone();
        let canceller = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if !cancelling_owner.dispatch_for_test() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("plain run did not register cancellation before the deadline");
        });
        let scope = PlainAgentScope {
            project: project.path(),
            profile_id: "default",
            session_store: &store,
            signal_owner: &signal_owner,
        };

        let disposition = execute_plain_turn_and_queued_follow_ups(
            &scope,
            &session.id,
            "exact run steering response smoke",
            InteractiveAgentMode::Execute,
            &input,
            PlainModalState::default(),
        )
        .expect("cancelled run reconciles");
        canceller.join().expect("canceller");
        assert_eq!(disposition, PlainAgentDisposition::Cancelled);
        let persisted = store.load(&session.id).expect("cancelled session");
        assert_eq!(persisted.queued_follow_ups.len(), 1);
        assert_eq!(
            persisted.queued_follow_ups[0].text,
            "must remain after cancellation"
        );
        assert!(persisted
            .messages
            .iter()
            .all(|message| { message.content != "must remain after cancellation" }));
    }

    #[test]
    fn plain_successful_queued_turns_chain_in_fifo_order() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        persist_queued_follow_up(&store, &session.id, "second fifo turn", "plain")
            .expect("second turn");
        persist_queued_follow_up(&store, &session.id, "third fifo turn", "plain")
            .expect("third turn");
        let modal_state = PlainModalState::default();
        let input = ConsoleInput::new(ModalSynchronizedReader::new(
            modal_state.clone(),
            [
                ModalInputStep::AfterModal(PLAIN_MODAL_APPROVAL, "y\n\n"),
                ModalInputStep::AfterModalCycle(PLAIN_MODAL_APPROVAL, "y\n\n"),
                ModalInputStep::AfterModalCycle(PLAIN_MODAL_APPROVAL, "y\n\n"),
            ],
        ));
        let signal_owner = PlainSignalOwner::detached();
        let scope = PlainAgentScope {
            project: project.path(),
            profile_id: "default",
            session_store: &store,
            signal_owner: &signal_owner,
        };

        assert_eq!(
            execute_plain_turn_and_queued_follow_ups(
                &scope,
                &session.id,
                "first fifo turn",
                InteractiveAgentMode::Execute,
                &input,
                modal_state,
            )
            .expect("FIFO turns complete"),
            PlainAgentDisposition::Completed
        );
        let persisted = store.load(&session.id).expect("completed FIFO session");
        assert!(persisted.queued_follow_ups.is_empty());
        assert_eq!(
            persisted
                .messages
                .iter()
                .filter(|message| {
                    message.role == "user" && message.content.ends_with("fifo turn")
                })
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["first fifo turn", "second fifo turn", "third fifo turn"]
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
    fn chat_rejects_a_credential_derived_session_before_resolution() {
        let project = tempdir().expect("project");
        save_mock_config(project.path());
        let mut config = load_nib_config_full(project.path()).expect("config");
        config.llm.providers.insert(
            "inactive-openai".to_string(),
            nib::config::ProviderEntry {
                model: "fixture".to_string(),
                api_key: Some("private-chat-session".to_string()),
                ..Default::default()
            },
        );
        save_nib_config_full(project.path(), &mut config).expect("credential config");
        let _cwd = CurrentDirGuard::enter(project.path());

        let error = run_chat_with_input(
            &ChatArgs {
                session: Some("private-chat-session".to_string()),
                plain: true,
                ..Default::default()
            },
            Cursor::new(b"/quit\n".to_vec()),
        )
        .expect_err("credential-derived session id");

        assert_eq!(
            error,
            "session identifier conflicts with configured sensitive data"
        );
        assert!(!error.contains("private-chat-session"));
        assert!(SessionStore::for_project(project.path())
            .expect("session store")
            .list_result()
            .expect("session list")
            .is_empty());
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
            "/session\n{}\ny\nparity routing goal\ny\n\n/quit\n",
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

        let modal_state = PlainModalState::default();
        run_chat_with_modal_state_input(
            &ChatArgs {
                session: None,
                auth: false,
                ..Default::default()
            },
            ModalSynchronizedReader::new(
                modal_state.clone(),
                [
                    ModalInputStep::Immediate("ask a question before continuing\n"),
                    ModalInputStep::AfterModal(PLAIN_MODAL_APPROVAL, "y\n\n"),
                    ModalInputStep::AfterModal(PLAIN_MODAL_QUESTION, "2\n\n"),
                ],
            ),
            modal_state,
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

        let modal_state = PlainModalState::default();
        run_chat_with_modal_state_input(
            &ChatArgs {
                session: None,
                auth: false,
                ..Default::default()
            },
            ModalSynchronizedReader::new(
                modal_state.clone(),
                [
                    ModalInputStep::Immediate("ask a question before continuing\n"),
                    ModalInputStep::AfterModal(PLAIN_MODAL_APPROVAL, "y\n\n"),
                ],
            ),
            modal_state,
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

    #[test]
    fn chat_quit_and_session_switch_report_queue_disposition() {
        let directory = tempdir().expect("sessions");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let current = store.try_create_session().expect("current");
        persist_queued_follow_up(&store, &current.id, "follow up later", "composer")
            .expect("queue");
        let exited = chat_queue_disposition(&store, &current.id, "exited");
        assert!(exited.contains("exited;"));
        assert!(exited.contains(&format!("retained on session {}", current.id)));

        let target = store.try_create_session().expect("target");
        let switched = chat_queue_disposition(&store, &current.id, "switched sessions");
        assert!(switched.contains("switched sessions;"));
        assert!(switched.contains(&current.id));
        assert!(!switched.contains(&target.id));
    }

    #[test]
    fn plain_goodbye_projects_the_session_path_before_output() {
        let directory = tempdir().expect("project parent");
        let secret = "credential-project-basename".to_string();
        let store =
            SessionStore::at_dir(directory.path().join(&secret).join(".nib").join("sessions"));

        let goodbye = plain_goodbye(&store, "safe-session", std::slice::from_ref(&secret));
        assert!(goodbye.starts_with("Goodbye. Session saved to "));
        assert!(goodbye.contains("[REDACTED]"), "{goodbye}");
        assert!(!goodbye.contains(&secret), "{goodbye}");
        assert!(!goodbye
            .chars()
            .any(|character| { character.is_control() && !matches!(character, '\n' | '\t') }));
    }
}
