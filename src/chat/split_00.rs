//! T043 split.

use super::*;

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
pub(crate) enum InteractiveMode {
    Plain,
    Tui,
}

#[derive(Default)]
pub(crate) struct PlainSignalRegistrationState {
    pub(crate) next_generation: u64,
    pub(crate) active: Option<(u64, nib::agent::CancellationSignal)>,
}

#[derive(Clone)]
pub(crate) struct PlainSignalOwner {
    pub(crate) state: Arc<Mutex<PlainSignalRegistrationState>>,
}

pub(crate) struct PlainSignalRegistration {
    pub(crate) state: Arc<Mutex<PlainSignalRegistrationState>>,
    pub(crate) generation: u64,
}

pub(crate) struct PlainAgentScope<'a> {
    pub(crate) project: &'a Path,
    pub(crate) profile_id: &'a str,
    pub(crate) session_store: &'a SessionStore,
    pub(crate) signal_owner: &'a PlainSignalOwner,
}

pub(crate) struct PlainApprovalPrompt {
    pub(crate) context: nib::tools::executor::ApprovalContext,
    pub(crate) reply: tokio::sync::oneshot::Sender<nib::tools::models::ApprovalDecision>,
}

pub(crate) const PLAIN_MODAL_IDLE: u8 = 0;
pub(crate) const PLAIN_MODAL_APPROVAL: u8 = 1;
pub(crate) const PLAIN_MODAL_QUESTION: u8 = 2;

#[derive(Clone, Default)]
pub(crate) struct PlainModalState {
    pub(crate) kind: Arc<AtomicU8>,
}

impl PlainModalState {
    pub(crate) fn claim(&self, kind: u8) -> bool {
        self.kind
            .compare_exchange(PLAIN_MODAL_IDLE, kind, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub(crate) fn clear(&self) {
        self.kind.store(PLAIN_MODAL_IDLE, Ordering::SeqCst);
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.kind.load(Ordering::SeqCst) != PLAIN_MODAL_IDLE
    }

    #[cfg(test)]
    pub(crate) fn current(&self) -> u8 {
        self.kind.load(Ordering::SeqCst)
    }
}

pub(crate) struct PlainQuestionPrompt {
    pub(crate) form: crate::console::LineQuestionForm,
    pub(crate) sensitive_values: Vec<String>,
    pub(crate) reply: tokio::sync::oneshot::Sender<nib::interactive::QuestionFormOutcome>,
}

pub(crate) enum PendingPlainModalResponse {
    Approval {
        decision: nib::tools::models::ApprovalDecision,
        reply: tokio::sync::oneshot::Sender<nib::tools::models::ApprovalDecision>,
    },
    Question {
        outcome: nib::interactive::QuestionFormOutcome,
        reply: tokio::sync::oneshot::Sender<nib::interactive::QuestionFormOutcome>,
    },
}

impl PendingPlainModalResponse {
    pub(crate) fn deliver(self) {
        match self {
            Self::Approval { decision, reply } => {
                let _ = reply.send(decision);
            }
            Self::Question { outcome, reply } => {
                let _ = reply.send(outcome);
            }
        }
    }

    pub(crate) fn fail_closed(self) {
        match self {
            Self::Approval { reply, .. } => {
                let _ = reply.send(nib::tools::models::ApprovalDecision::denied());
            }
            Self::Question { reply, .. } => {
                let _ = reply.send(nib::interactive::QuestionFormOutcome::InputClosed);
            }
        }
    }
}

pub(crate) struct BrokeredPlainApprovalHandler {
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<PlainApprovalPrompt>,
    pub(crate) modal_state: PlainModalState,
}

pub(crate) struct BrokeredPlainQuestionHandler {
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<PlainQuestionPrompt>,
    pub(crate) modal_state: PlainModalState,
    pub(crate) sensitive_values: Vec<String>,
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
        match self
            .ask_with_context(nib::agent::QuestionRequestContext {
                invocation_id: nib::tools::ToolInvocationId::new(),
                question,
                proposed_answer: None,
                options,
            })
            .await
        {
            nib::agent::QuestionOutcome::Answered(answer) => Ok(answer),
            nib::agent::QuestionOutcome::ApprovedProposal(answer) => Ok(answer),
            nib::agent::QuestionOutcome::LeftUnanswered => Err("left unanswered".to_string()),
            nib::agent::QuestionOutcome::Cancelled => Err("cancelled".to_string()),
            nib::agent::QuestionOutcome::InputClosed => Err("input closed".to_string()),
            nib::agent::QuestionOutcome::InputUnavailable(error) => Err(error),
        }
    }

    async fn ask_with_context(
        &self,
        context: nib::agent::QuestionRequestContext<'_>,
    ) -> nib::agent::QuestionOutcome {
        let form = crate::console::legacy_question_form(&context);
        nib::agent::QuestionOutcome::from_form(self.ask_form(nib::agent::QuestionFormRequestContext {
            invocation_id: context.invocation_id, form: &form, initial_answers: &[],
        }).await)
    }

    async fn ask_form(
        &self,
        context: nib::agent::QuestionFormRequestContext<'_>,
    ) -> nib::interactive::QuestionFormOutcome {
        use nib::interactive::QuestionFormOutcome;
        if context.form.questions.is_empty() {
            return QuestionFormOutcome::InputUnavailable("question form is empty".into());
        }
        let displayed = match nib::interactive::public_question_form(context.form, &self.sensitive_values) {
            Ok(form) => form,
            Err(error) => return QuestionFormOutcome::InputUnavailable(error),
        };
        if !self.modal_state.claim(PLAIN_MODAL_QUESTION) {
            return QuestionFormOutcome::InputUnavailable(
                "another interactive prompt already owns plain input".to_string(),
            );
        }
        let (reply, response) = tokio::sync::oneshot::channel();
        if self.tx.send(PlainQuestionPrompt {
            form: crate::console::LineQuestionForm::new(displayed, context.initial_answers.to_vec()),
            sensitive_values: self.sensitive_values.clone(), reply,
        }).is_err() {
            self.modal_state.clear();
            return QuestionFormOutcome::InputUnavailable("plain question input router stopped".into());
        }
        let outcome = response.await.unwrap_or_else(|_| {
            QuestionFormOutcome::InputUnavailable("plain question input router stopped".into())
        });
        self.modal_state.clear();
        outcome
    }

}

pub(crate) static PLAIN_SIGNAL_STATE: OnceLock<
    Result<Arc<Mutex<PlainSignalRegistrationState>>, String>,
> = OnceLock::new();

impl PlainSignalOwner {
    pub(crate) fn install() -> Result<Self, String> {
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

    pub(crate) fn register(
        &self,
        cancellation: nib::agent::CancellationSignal,
    ) -> PlainSignalRegistration {
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
    pub(crate) fn detached() -> Self {
        Self {
            state: Arc::new(Mutex::new(PlainSignalRegistrationState::default())),
        }
    }

    #[cfg(test)]
    pub(crate) fn dispatch_for_test(&self) -> bool {
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
pub(crate) struct TerminalCapabilities {
    pub(crate) input_is_terminal: bool,
    pub(crate) output_is_terminal: bool,
    pub(crate) term: Option<String>,
}

impl TerminalCapabilities {
    pub(crate) fn detect() -> Self {
        Self {
            input_is_terminal: io::stdin().is_terminal(),
            output_is_terminal: io::stdout().is_terminal(),
            term: std::env::var("TERM").ok(),
        }
    }

    pub(crate) fn rejection(&self) -> Option<&'static str> {
        nib::tui::tui_environment_rejection(
            self.input_is_terminal,
            self.output_is_terminal,
            self.term.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModeSelection {
    pub(crate) mode: InteractiveMode,
    pub(crate) auto_fallback_notice: Option<&'static str>,
}

pub(crate) fn resolve_interactive_mode(
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
    if selection.mode == InteractiveMode::Plain {
        confirm_workspace_access(&project)?;
    }
    match selection.mode {
        InteractiveMode::Plain => run_plain_with_input(
            args,
            &project,
            config,
            ConsoleInput::new(io::BufReader::new(io::stdin())),
        ),
        InteractiveMode::Tui => nib::tui::run_tui(
            &project,
            args.run.clone(),
            args.session.clone(),
            crate::updater::startup_update_notice(),
        )
        .map_err(|error| error.to_string()),
    }
}

#[cfg(test)]
pub(crate) fn run_chat_with_input(
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
pub(crate) fn run_chat_with_modal_state_input(
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

pub(crate) fn confirm_workspace_access(project: &Path) -> Result<(), String> {
    if nib::config::workspace_access_is_granted(project).map_err(|error| error.to_string())? {
        return Ok(());
    }
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        return Ok(());
    }
    println!("Allow nib to work in this directory?");
    println!("  {}", project.display());
    println!("Y Allow  ·  N Decline");
    print!("> ");
    io::stdout()
        .flush()
        .map_err(|error| format!("failed to prompt for workspace access: {error}"))?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("failed to read workspace access answer: {error}"))?;
    let answer = line.trim();
    if matches!(answer, "y" | "Y" | "yes" | "Yes") {
        nib::config::grant_workspace_access(project).map_err(|error| error.to_string())?;
        return Ok(());
    }
    Err("workspace access declined".to_string())
}

pub(crate) fn prepare_interactive_config(
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

pub(crate) fn plain_goodbye(
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

pub(crate) fn run_plain_with_input(
    args: &ChatArgs,
    project: &Path,
    config: nib::config::NibConfig,
    input: ConsoleInput,
) -> Result<(), String> {
    run_plain_with_input_and_modal_state(args, project, config, input, PlainModalState::default())
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn run_plain_with_input_and_modal_state(
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
        let conversation = project_session_conversation(&sess, &sensitive_values);
        for activity in conversation.iter().rev().take(6).rev() {
            let prefix = match activity.kind {
                ActivityKind::User => "user",
                ActivityKind::Assistant => "assistant",
                _ => "system",
            };
            println!("{prefix}: {}", activity.copy_text());
        }
    }

    if let Some(goal) = args.run.as_deref() {
        let result = match nib::interactive::recover_question_conversation(&session_store, &sid, goal) {
            Ok(Some(effect)) => execute_plain_question_recovery(&agent_scope, &sid, effect, &input, modal_state.clone()),
            Ok(None) => {
                println!("Thinking...");
                execute_plain_turn_and_queued_follow_ups(&agent_scope, &sid, goal, InteractiveAgentMode::Execute, &input, modal_state.clone())
            }
            Err(error) => Err(error),
        };
        match result {
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
            let copy_command = matches!(command, InteractiveCommand::Copy);
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
                Ok(InteractiveEffect::Output(output)) => {
                    if copy_command {
                        println!("{}", nib::tui::copy_text_to_clipboard(&output));
                    } else {
                        println!("{}", public_output(&output));
                    }
                }
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
                Ok(InteractiveEffect::ContinuePlan { plan_id, goal }) => {
                    println!("Continuing plan {plan_id}...");
                    match execute_plain_continuation(
                        &agent_scope,
                        &sid,
                        &goal,
                        &plan_id,
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
                Ok(InteractiveEffect::OpenQuestion { invocation_id, question, proposed_answer, options }) => {
                    let form = nib::interactive::QuestionForm { header: None, questions: vec![nib::interactive::FormQuestion {
                        title: None, question, proposed_answer,
                        options: options.into_iter().map(|label| nib::interactive::QuestionOption { label, description: None }).collect(),
                    }] };
                    let effect = nib::interactive::QuestionRecoveryEffect::OpenForm(nib::session::PersistedQuestionForm {
                        invocation_id, run_id: None, plan_id: None, form, initial_answers: Vec::new(),
                    });
                    match execute_plain_question_recovery(&agent_scope, &sid, effect, &input, modal_state.clone()) {
                        Ok(PlainAgentDisposition::QuitRequested(terminal)) => {
                            println!("{}", plain_quit_disposition(&session_store, &sid, terminal));
                            break 'repl;
                        }
                        Err(error) => println!("{error}"),
                        _ => {}
                    }
                }
                Ok(InteractiveEffect::RunAgent { goal, mode }) => {
                    if mode != InteractiveAgentMode::Compact {
                        let _ = maybe_assign_session_display_name(&session_store, &sid, &goal);
                    }
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

        match nib::interactive::recover_question_conversation(&session_store, &sid, &goal) {
            Ok(Some(effect)) => {
                match execute_plain_question_recovery(&agent_scope, &sid, effect, &input, modal_state.clone()) {
                    Ok(PlainAgentDisposition::QuitRequested(terminal)) => {
                        println!("{}", plain_quit_disposition(&session_store, &sid, terminal));
                        break 'repl;
                    }
                    Err(error) => println!("{error}"),
                    _ => {}
                }
                continue;
            }
            Ok(None) => {}
            Err(error) => { println!("{error}"); continue; }
        }
        let _ = maybe_assign_session_display_name(&session_store, &sid, &goal);
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

pub(crate) fn require_plain_consumer(
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

pub(crate) fn select_draft_history_from_console(
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

pub(crate) fn select_command_completion_from_console(
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
pub(crate) enum ChatSessionAction {
    Cancelled,
    Unchanged,
    Activated(String),
}

pub(crate) fn select_session_from_console(
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

pub(crate) fn confirm_session_candidate(
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

pub(crate) fn select_model_from_console(
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

pub(crate) fn chat_queue_disposition(
    store: &SessionStore,
    session_id: &str,
    action: &str,
) -> String {
    queue_disposition_message(store, session_id, action).unwrap_or_else(|error| error)
}

pub(crate) fn plain_quit_disposition(
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
pub(crate) enum PlainAgentDisposition {
    Completed,
    Cancelled,
    Failed,
    QuitRequested(InteractionTerminalOutcome),
}

pub(crate) struct PreparedPlainAgentStep {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) renderer: Option<std::thread::JoinHandle<()>>,
    pub(crate) cancellation: nib::agent::CancellationSignal,
    pub(crate) steering: Option<nib::agent::ExactRunSteeringHandle>,
    pub(crate) approval_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PlainApprovalPrompt>>,
    pub(crate) question_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PlainQuestionPrompt>>,
    pub(crate) loop_cfg: Option<nib::agent::AgentLoopConfig>,
}

impl PreparedPlainAgentStep {
    pub(crate) fn prepare(
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
        let question_sensitive_values = sensitive_values.clone();
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
            interactive_request: mode == InteractiveAgentMode::Execute,
            provider: None,
            auto_approve: false,
            approval_handler: Some(Arc::new(BrokeredPlainApprovalHandler {
                tx: approval_tx,
                modal_state: modal_state.clone(),
            })),
            question_handler: Some(Arc::new(BrokeredPlainQuestionHandler {
                tx: question_tx,
                modal_state: modal_state.clone(),
                sensitive_values: question_sensitive_values,
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

pub(crate) fn parse_plain_question_answer(line: &str, options: &[String]) -> InteractionReduction {
    let state = InteractionState {
        question_pending: true,
        ..InteractionState::default()
    };
    reduce_interaction(
        &state,
        InteractionInput::QuestionAnswer {
            answer: line,
            options,
            selected_option: None,
        },
    )
}

pub(crate) fn plain_approval_decision(line: &str) -> InteractionReduction {
    let state = InteractionState {
        approval_pending: true,
        ..InteractionState::default()
    };
    reduce_interaction(&state, InteractionInput::ApprovalAnswer(line))
}

pub(crate) fn complete_command_approval_line(
    line: &str,
    context: &nib::tools::executor::ApprovalContext,
) -> Result<Option<nib::tools::models::ApprovalDecision>, String> {
    let Some(command) = context.shown_command.as_deref() else {
        return Ok(Some(
            nib::tools::models::ApprovalDecision::denied_unshowable(),
        ));
    };
    let environment = if context.command_environment.is_empty() {
        "local"
    } else {
        context.command_environment.as_str()
    };
    let card = nib::interaction_card::command_approval_card(
        environment,
        &context.reason,
        command,
        &context.command_extras,
        context.remember_exact.as_deref(),
        0,
        false,
    );
    match nib::interaction_card::plain_command_line(&card.rows, line) {
        nib::interaction_card::PlainCommandLine::Retry(message) => Err(message),
        nib::interaction_card::PlainCommandLine::GrantOnce => {
            Ok(Some(nib::tools::models::ApprovalDecision::granted_user()))
        }
        nib::interaction_card::PlainCommandLine::Remember => {
            let Some(exact) = context.remember_exact.clone() else {
                return Err("this command cannot be remembered".to_string());
            };
            Ok(Some(
                nib::tools::models::ApprovalDecision::granted_remembered(exact),
            ))
        }
        nib::interaction_card::PlainCommandLine::Deny => {
            Ok(Some(nib::tools::models::ApprovalDecision::denied()))
        }
        nib::interaction_card::PlainCommandLine::NeedReason => Err("REASON".to_string()),
    }
}

pub(crate) fn complete_plain_approval_line(
    line: &str,
    context: &nib::tools::executor::ApprovalContext,
) -> Result<Option<nib::tools::models::ApprovalDecision>, String> {
    if context.shown_command.is_some() {
        return complete_command_approval_line(line, context);
    }
    if line.trim().eq_ignore_ascii_case("details") {
        eprintln!("{}", context.render());
        return Ok(None);
    }
    match plain_approval_decision(line) {
        InteractionReduction::ApprovalDecision(InteractionDecision::Accept) => {
            Ok(Some(nib::tools::models::ApprovalDecision::granted_user()))
        }
        InteractionReduction::ApprovalDecision(InteractionDecision::Reject) => {
            Ok(Some(nib::tools::models::ApprovalDecision::denied()))
        }
        InteractionReduction::ApprovalInputClosed => Ok(Some(
            nib::tools::models::ApprovalDecision::denied_input_closed(),
        )),
        InteractionReduction::Error { message, .. } => Err(message),
        _ => Ok(Some(nib::tools::models::ApprovalDecision::denied())),
    }
}

pub(crate) fn request_plain_modal_frame_delimiter() {
    println!("[modal] response recorded; press Enter on an empty line to return input ownership");
}

pub(crate) fn submit_plain_steering(
    steering: Option<&nib::agent::ExactRunSteeringHandle>,
    text: &str,
) -> Result<usize, String> {
    steering
        .ok_or_else(|| "this active operation does not accept exact-run steering".to_string())?
        .submit(text)
}

#[cfg(test)]
pub(crate) fn execute_agent_step(
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

pub(crate) fn execute_agent_step_with_modal_state(
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
