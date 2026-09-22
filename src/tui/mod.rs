//! Current-session-first Ratatui interface with live agent lifecycle rendering.

mod markdown;

#[cfg(test)]
use crate::interactive::safe_event_fields;
use crate::interactive::{
    apply_stream_event, claim_next_queued_follow_up_after_startup,
    display_stream_event_with_sensitive_values, execute_interactive_command_in_state,
    format_tui_interaction_chrome, interactive_completions, interactive_session_candidate,
    interactive_session_selection, is_legacy_thought_title, maybe_assign_session_display_name,
    meter_token_label, path_completions, persist_queued_follow_up, project_session_activities,
    queue_disposition_message, reduce_interaction, resolve_session,
    restore_queued_follow_up_after_start_failure, set_active_model, thought_header_title,
    truncate_display_cells, unicode_display_width, validate_interactive_session_target,
    wrapped_display_rows, ActivityEntry, ActivityKind, DraftHistory, DraftHistorySearch,
    InteractionConsumer, InteractionDecision, InteractionInput, InteractionReduction,
    InteractionRunState, InteractionState, InteractionTerminalOutcome, InteractiveAgentMode,
    InteractiveCompletion, InteractiveEffect, InteractiveSessionCandidate,
    InteractiveSessionSelection, ModelSelection, SelectorDetailKind, StreamDisplay,
    TranscriptViewport, TranscriptViewportAction, TuiChrome, MAX_DRAFT_HISTORY_QUERY_BYTES,
};
use crate::interactive::{bounded_public_text, control_safe_text};
use crate::llm::types::StreamEvent;
use crate::session::SessionStore;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::DefaultTerminal;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use unicode_segmentation::UnicodeSegmentation;

use crate::agent::CancellationSignal;
use crate::tools::executor::{ApprovalContext, ApprovalHandler};
use crate::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};

#[derive(Debug, Default)]
struct LiveOutput {
    text: String,
    state: Option<String>,
}

const MAX_LIVE_OUTPUT_BYTES: usize = 1_048_576;
const OMITTED_OUTPUT_MARKER: &str = "[older live output omitted]\n";
#[cfg(test)]
const MAX_SESSION_DETAIL_BYTES: usize = 131_072;
#[cfg(test)]
const MAX_SESSION_DETAIL_ITEMS: usize = 100;
const MAX_SESSION_DETAIL_ITEM_CHARS: usize = 500;
#[cfg(test)]
const MAX_SESSION_DETAIL_ROWS: usize = 400;
#[cfg(test)]
const SESSION_DETAIL_TRUNCATED_MARKER: &str = "\n[session detail truncated]\n";
const MAX_VISIBLE_COMPLETIONS: usize = 7;
const COMPOSER_BORDER_ROWS: u16 = 1;
const COMPOSER_PROMPT_CELLS: u16 = 2;
const CLEAR_DRAFT_CONFIRM: Duration = Duration::from_millis(800);
const QUIT_CONFIRM: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitConfirmAction {
    Arm,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuitArm {
    armed_at: Instant,
    consumer: InteractionLayer,
}

fn quit_confirm_action(
    armed: Option<QuitArm>,
    now: Instant,
    consumer: InteractionLayer,
) -> QuitConfirmAction {
    if armed.is_some_and(|armed| {
        armed.consumer == consumer && now.duration_since(armed.armed_at) <= QUIT_CONFIRM
    }) {
        QuitConfirmAction::Confirm
    } else {
        QuitConfirmAction::Arm
    }
}

fn quit_arm_after_input(armed: Option<QuitArm>, is_control_q: bool) -> Option<QuitArm> {
    is_control_q.then_some(armed).flatten()
}

fn quit_arm_for_consumer(armed: Option<QuitArm>, consumer: InteractionLayer) -> Option<QuitArm> {
    armed.filter(|armed| armed.consumer == consumer)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupWelcome {
    version: String,
    working_directory: String,
    update_notice: Option<String>,
    consent_directory: Option<String>,
    consent_selected: usize,
}

impl StartupWelcome {
    fn new(project_root: &Path, update_notice: Option<String>) -> Self {
        Self {
            version: format!("Nib {}", env!("CARGO_PKG_VERSION")),
            working_directory: crate::interactive::folder_label(project_root),
            update_notice: update_notice
                .map(|notice| notice.strip_prefix("[nib] ").unwrap_or(&notice).to_string()),
            consent_directory: None,
            consent_selected: 1,
        }
    }

    #[cfg(test)]
    fn fixture() -> Self {
        Self {
            version: format!("Nib {}", env!("CARGO_PKG_VERSION")),
            working_directory: "~/project".to_string(),
            update_notice: None,
            consent_directory: None,
            consent_selected: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TuiFocus {
    #[default]
    Composer,
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PointerSelection {
    anchor_row: usize,
    anchor_col: usize,
    focus_row: usize,
    focus_col: usize,
    moved: bool,
}

impl PointerSelection {
    fn at(row: usize, col: usize) -> Self {
        Self {
            anchor_row: row,
            anchor_col: col,
            focus_row: row,
            focus_col: col,
            moved: false,
        }
    }

    fn drag_to(&mut self, row: usize, col: usize) {
        if row != self.anchor_row || col != self.anchor_col {
            self.moved = true;
        }
        self.focus_row = row;
        self.focus_col = col;
    }

    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        let start = (self.anchor_row, self.anchor_col);
        let end = (self.focus_row, self.focus_col);
        if start <= end {
            (start, end)
        } else {
            (end, start)
        }
    }

    fn covers_row(&self, row: usize) -> bool {
        if !self.moved {
            return false;
        }
        let ((start_row, _), (end_row, _)) = self.ordered();
        row >= start_row && row <= end_row
    }

    fn is_active(&self) -> bool {
        self.moved
    }
}

fn pointer_selection_is_active(pointer: Option<PointerSelection>) -> bool {
    pointer.is_some_and(|selection| selection.is_active())
}

#[derive(Debug, Clone, Default)]
struct TranscriptView {
    area: Rect,
    composer_area: Rect,
    top_row: usize,
    plain_rows: Vec<String>,
    owners: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum WaitingKind {
    #[default]
    None,
    Approval,
    Question,
    Workspace,
}

enum InteractionBand<'a> {
    Completion(&'a CompletionMenu),
    Model(&'a PendingModelSelection),
    Sessions {
        switcher: &'a SessionSwitcher,
        active: &'a str,
    },
    History(&'a PendingHistorySearch),
    Approval(&'a TuiApprovalRequest),
    Question(&'a PendingQuestion),
    Workspace {
        directory: &'a str,
        selected: usize,
    },
}

impl InteractionBand<'_> {
    fn row_count(&self) -> usize {
        match self {
            Self::Completion(menu) => menu.suggestions.len(),
            Self::Model(model) => model.selection.available.len().max(1),
            Self::Sessions { switcher, .. } => {
                let rows = switcher.candidates.len().max(1);
                if switcher.confirming {
                    rows.max(2)
                } else if !switcher.exact_id.is_empty() {
                    rows.saturating_add(1)
                } else {
                    rows
                }
            }
            Self::History(search) => search.search.matches.len().max(1).saturating_add(1),
            Self::Approval(_) | Self::Workspace { .. } => 2,
            Self::Question(question) if question.request.options.is_empty() => 1,
            Self::Question(question) => question.request.options.len().saturating_add(1),
        }
    }

    fn footer_hint(&self) -> &'static str {
        match self {
            Self::Completion(_) => "Tab insert · Enter run · Esc close",
            Self::Model(_) => "Enter select · Esc cancel",
            Self::Sessions { switcher, .. } if switcher.confirming => {
                "Y/Enter resume · N/Esc keep current"
            }
            Self::Sessions { .. } => "Enter resume · Esc close",
            Self::History(_) => "Enter restore · Esc close",
            Self::Approval(_) => "Y/Enter approve once · N deny · Esc deny",
            Self::Question(_) => "Enter / 1-9 answer · Esc skip",
            Self::Workspace { .. } => "Y/Enter allow this directory · N decline",
        }
    }
}

#[derive(Debug, Clone)]
struct WaitingMeter {
    job: String,
    step: String,
    elapsed: Duration,
    tokens: String,
    status: String,
    tick: u128,
}

struct SessionLayout {
    header: Rect,
    transcript: Rect,
    meter: Rect,
    composer: Rect,
    completion: Rect,
    footer: Rect,
}

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPINNER_ASCII: &[char] = &['|', '/', '-', '\\'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionKeyResult {
    Ignored,
    Consumed,
    Submit,
}

#[derive(Debug, Clone)]
struct ChromeCache {
    generation: u64,
    session_id: String,
    session_revision: Option<u64>,
    origin: String,
    chrome: TuiChrome,
}

impl ChromeCache {
    fn matches(
        &self,
        generation: u64,
        session_id: &str,
        session_revision: Option<u64>,
        origin: &str,
    ) -> bool {
        self.generation == generation
            && self.session_id == session_id
            && self.session_revision == session_revision
            && self.origin == origin
    }
}
const MAX_SWITCHER_CANDIDATES: usize = 100;
const MAX_SWITCHER_EXACT_ID_BYTES: usize = 256;
const AGENT_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl LiveOutput {
    fn apply(&mut self, event: StreamEvent, sensitive_values: &[String]) {
        if let StreamEvent::StateTransition { state } = &event {
            self.state = Some(state.clone());
        }
        match display_stream_event_with_sensitive_values(event, sensitive_values) {
            Some(StreamDisplay::Content(content)) => self.push_raw(&content),
            Some(StreamDisplay::Status(status)) => self.push_status(status),
            None => {}
        }
    }

    fn push_status(&mut self, status: String) {
        let status = control_safe_text(&status, true);
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self.text.push_str(&status);
        self.text.push('\n');
        self.enforce_bound();
    }

    fn push_raw(&mut self, content: &str) {
        self.text.push_str(&control_safe_text(content, true));
        self.enforce_bound();
    }

    fn enforce_bound(&mut self) {
        if self.text.len() <= MAX_LIVE_OUTPUT_BYTES {
            return;
        }
        let keep = MAX_LIVE_OUTPUT_BYTES.saturating_sub(OMITTED_OUTPUT_MARKER.len());
        let mut start = self.text.len().saturating_sub(keep);
        while start < self.text.len() && !self.text.is_char_boundary(start) {
            start += 1;
        }
        let tail = self.text.split_off(start);
        self.text.clear();
        self.text.push_str(OMITTED_OUTPUT_MARKER);
        self.text.push_str(&tail);
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionDetail {
    text: String,
}

#[cfg(test)]
impl SessionDetail {
    fn new(
        id: &str,
        session: Option<&crate::session::Session>,
        sensitive_values: &[String],
    ) -> Self {
        let mut text = format!("Session: {id}\n");
        if let Some(session) = session {
            text.push_str(&format!("Messages: {}\n", session.messages.len()));
            if let Some(plan) = &session.plan {
                text.push_str(&format!(
                    "Plan: step {}/{}; outcome={}\n",
                    plan.current_step_index.min(plan.steps.len()),
                    plan.steps.len(),
                    plan.outcome.as_deref().unwrap_or("active")
                ));
            }
            text.push('\n');

            let message_start = session
                .messages
                .len()
                .saturating_sub(MAX_SESSION_DETAIL_ITEMS);
            if message_start > 0 {
                text.push_str(&format!("[{message_start} earlier messages omitted]\n\n"));
            }
            for message in &session.messages[message_start..] {
                text.push_str(&format!(
                    "#{} [{}]\n{}\n\n",
                    message.index,
                    message.role,
                    bounded_preview(&message.content, sensitive_values)
                ));
            }

            text.push_str(&format!("Tool calls: {}\n", session.tool_calls.len()));
            let tool_start = session
                .tool_calls
                .len()
                .saturating_sub(MAX_SESSION_DETAIL_ITEMS);
            if tool_start > 0 {
                text.push_str(&format!("[{tool_start} earlier tool calls omitted]\n"));
            }
            for (index, call) in session.tool_calls[tool_start..].iter().enumerate() {
                let status = if call.error.is_some() { "failed" } else { "ok" };
                text.push_str(&format!(
                    "#{} {} ({status})",
                    tool_start + index,
                    call.tool_name.as_deref().unwrap_or("unknown")
                ));
                if let Some(error) = &call.error {
                    text.push_str(&format!(": {}", bounded_preview(error, sensitive_values)));
                }
                text.push('\n');
            }

            text.push_str(&format!("\nLifecycle events: {}\n", session.events.len()));
            let event_start = session
                .events
                .len()
                .saturating_sub(MAX_SESSION_DETAIL_ITEMS);
            if event_start > 0 {
                text.push_str(&format!("[{event_start} earlier events omitted]\n"));
            }
            for event in &session.events[event_start..] {
                let details = if event.kind == "steering_input" {
                    safe_event_fields(&event.details, &["sequence", "source"])
                } else {
                    bounded_preview(&event.details.to_string(), sensitive_values)
                };
                text.push_str(&format!("#{} {}: {}\n", event.index, event.kind, details,));
            }
        } else {
            text.push_str("Session is no longer available.\n");
        }

        text = bounded_public_text(&text, sensitive_values, MAX_SESSION_DETAIL_BYTES, true);
        truncate_session_detail(&mut text);
        truncate_session_detail_rows(&mut text);
        Self { text }
    }
}

#[cfg(test)]
fn bounded_preview(content: &str, sensitive_values: &[String]) -> String {
    let safe = bounded_public_text(
        content,
        sensitive_values,
        MAX_SESSION_DETAIL_ITEM_CHARS.saturating_mul(4),
        true,
    );
    let mut characters = safe.chars();
    let mut preview: String = characters
        .by_ref()
        .take(MAX_SESSION_DETAIL_ITEM_CHARS)
        .collect();
    if characters.next().is_some() {
        preview.push_str("...");
    }
    preview
}

#[cfg(test)]
fn truncate_session_detail(text: &mut String) {
    if text.len() <= MAX_SESSION_DETAIL_BYTES {
        return;
    }
    let mut end = MAX_SESSION_DETAIL_BYTES.saturating_sub(SESSION_DETAIL_TRUNCATED_MARKER.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(SESSION_DETAIL_TRUNCATED_MARKER);
}

#[cfg(test)]
fn truncate_session_detail_rows(text: &mut String) {
    let line_count = text.lines().count();
    if line_count <= MAX_SESSION_DETAIL_ROWS {
        return;
    }
    let keep_from = line_count.saturating_sub(MAX_SESSION_DETAIL_ROWS - 2);
    let tail = text.lines().skip(keep_from).collect::<Vec<_>>().join("\n");
    *text = format!("[{} earlier timeline rows omitted]\n{}\n", keep_from, tail);
    truncate_session_detail(text);
}

#[derive(Debug, Default)]
struct ActiveTimeline {
    session_id: String,
    active_run_id: Option<String>,
    reconciled_terminal: Option<InteractionTerminalOutcome>,
    reconciled_outcome: Option<String>,
    activities: Vec<ActivityEntry>,
    live: LiveOutput,
    sensitive_values: Vec<String>,
    run_started_at: Option<Instant>,
}

impl ActiveTimeline {
    fn load(store: &SessionStore, session_id: &str) -> io::Result<Self> {
        let session = store
            .load_result(session_id)
            .map_err(|error| {
                io::Error::other(format!("failed to load session {session_id}: {error}"))
            })?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("session {session_id} no longer exists"),
                )
            })?;
        Ok(Self::from_session(
            &session,
            store.public_sensitive_values().to_vec(),
        ))
    }

    fn from_session(session: &crate::session::Session, sensitive_values: Vec<String>) -> Self {
        let activities = project_session_activities(session, &sensitive_values);
        Self {
            session_id: session.id.clone(),
            active_run_id: None,
            reconciled_terminal: None,
            reconciled_outcome: None,
            activities,
            live: LiveOutput::default(),
            sensitive_values,
            run_started_at: None,
        }
    }

    fn push_status(&mut self, status: String) {
        let status =
            bounded_public_text(&status, &self.sensitive_values, MAX_LIVE_OUTPUT_BYTES, true);
        self.live.push_status(status.clone());
        self.activities.push(ActivityEntry::new(
            ActivityKind::System,
            status,
            String::new(),
        ));
    }

    fn push_steering(&mut self, _text: &str, sequence: usize) {
        self.activities.push(ActivityEntry::new(
            ActivityKind::User,
            format!("steer {sequence}"),
            "instruction persisted for the exact active run",
        ));
        self.live
            .push_status(format!("[steer] accepted for safe boundary #{sequence}"));
    }

    fn apply_event(&mut self, event: StreamEvent) {
        if let StreamEvent::Reconciled { outcome } = &event {
            if !matches!(outcome.as_str(), "step_completed" | "verification_recovery") {
                if let InteractionReduction::Reconciled { terminal, outcome } = reduce_interaction(
                    &InteractionState::default(),
                    InteractionInput::ReconciledOutcome {
                        outcome,
                        failure: false,
                    },
                ) {
                    self.reconciled_terminal = Some(terminal);
                    self.reconciled_outcome = Some(outcome);
                }
            }
        }
        let duplicate_end = matches!(&event, StreamEvent::End(outcome)
            if self.reconciled_outcome.as_deref() == Some(outcome.as_str()));
        if !duplicate_end {
            if let StreamEvent::End(outcome) = &event {
                if let InteractionReduction::Reconciled { terminal, .. } = reduce_interaction(
                    &InteractionState::default(),
                    InteractionInput::ReconciledOutcome {
                        outcome,
                        failure: false,
                    },
                ) {
                    // Final persistence can fail after a successful reconciliation.
                    // Preserve that failure and prevent queued work from advancing.
                    if terminal != InteractionTerminalOutcome::Completed {
                        self.reconciled_terminal = Some(terminal);
                    }
                }
            }
            apply_stream_event(
                &mut self.activities,
                event.clone(),
                &mut self.live.state,
                &self.sensitive_values,
            );
            self.live.apply(event, &self.sensitive_values);
        }
    }

    fn bind_run(&mut self, run_id: Option<String>) {
        if run_id.is_some() {
            self.reconciled_terminal = None;
            self.reconciled_outcome = None;
            if self.active_run_id != run_id || self.run_started_at.is_none() {
                self.run_started_at = Some(Instant::now());
            }
        } else {
            self.run_started_at = None;
        }
        self.active_run_id = run_id;
    }

    #[cfg(test)]
    fn rendered_text(&self) -> String {
        let text = self
            .activities
            .iter()
            .map(ActivityEntry::render_line)
            .collect::<Vec<_>>()
            .join("\n\n");
        if self.live.text.is_empty() {
            return text;
        }
        let separator = if text.ends_with('\n') { "" } else { "\n" };
        format!("{text}{separator}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionSwitcher {
    candidates: Vec<InteractiveSessionCandidate>,
    selected: usize,
    omitted: usize,
    confirming: bool,
    exact_id: String,
    error: Option<String>,
}

impl SessionSwitcher {
    fn from_selection(selection: InteractiveSessionSelection, active_session_id: &str) -> Self {
        let selected = selection
            .candidates
            .iter()
            .position(|candidate| candidate.id == active_session_id)
            .unwrap_or(0);
        Self {
            candidates: selection.candidates,
            selected,
            omitted: selection.omitted,
            confirming: false,
            exact_id: String::new(),
            error: None,
        }
    }
}

fn load_session_switcher(
    store: &SessionStore,
    active_session_id: &str,
) -> io::Result<SessionSwitcher> {
    let selection =
        interactive_session_selection(store, active_session_id).map_err(io::Error::other)?;
    Ok(SessionSwitcher::from_selection(
        selection,
        active_session_id,
    ))
}

pub struct TuiApprovalRequest {
    pub call: ToolCall,
    pub level: PermissionLevel,
    pub context: ApprovalContext,
    pub selected_option: usize,
    pub typed: String,
    details_open: bool,
    detail_offset: usize,
    error: Option<String>,
    pub reply: oneshot::Sender<ApprovalDecision>,
}

pub struct TuiApprovalHandler {
    pub tx: mpsc::Sender<TuiApprovalRequest>,
}

#[async_trait::async_trait]
impl ApprovalHandler for TuiApprovalHandler {
    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision {
        let context = ApprovalContext::compatibility(call, level);
        self.request(call, level, context).await
    }

    async fn handle_approval_with_context(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        context: &ApprovalContext,
    ) -> ApprovalDecision {
        self.request(call, level, context.clone()).await
    }
}

impl TuiApprovalHandler {
    async fn request(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        context: ApprovalContext,
    ) -> ApprovalDecision {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = TuiApprovalRequest {
            call: call.clone(),
            level,
            context,
            selected_option: 1,
            typed: String::new(),
            details_open: false,
            detail_offset: 0,
            error: None,
            reply: reply_tx,
        };
        let _ = self.tx.send(req);
        reply_rx
            .await
            .unwrap_or_else(|_| ApprovalDecision::denied())
    }
}

pub struct TuiQuestionRequest {
    pub question: String,
    pub options: Vec<String>,
    pub reply: oneshot::Sender<crate::agent::QuestionOutcome>,
}

pub struct TuiQuestionHandler {
    pub tx: mpsc::Sender<TuiQuestionRequest>,
}

#[async_trait::async_trait]
impl crate::agent::QuestionHandler for TuiQuestionHandler {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        match self
            .ask_with_context(crate::agent::QuestionRequestContext {
                invocation_id: crate::tools::ToolInvocationId::new(),
                question,
                options,
            })
            .await
        {
            crate::agent::QuestionOutcome::Answered(answer) => Ok(answer),
            crate::agent::QuestionOutcome::LeftUnanswered => Err("left unanswered".to_string()),
            crate::agent::QuestionOutcome::Cancelled => Err("cancelled".to_string()),
            crate::agent::QuestionOutcome::InputClosed => Err("input closed".to_string()),
            crate::agent::QuestionOutcome::InputUnavailable(error) => Err(error),
        }
    }

    async fn ask_with_context(
        &self,
        context: crate::agent::QuestionRequestContext<'_>,
    ) -> crate::agent::QuestionOutcome {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(TuiQuestionRequest {
                question: context.question.to_string(),
                options: context.options.to_vec(),
                reply: reply_tx,
            })
            .is_err()
        {
            return crate::agent::QuestionOutcome::InputUnavailable(
                "TUI question channel closed".to_string(),
            );
        }
        reply_rx
            .await
            .unwrap_or(crate::agent::QuestionOutcome::InputUnavailable(
                "TUI question response was dropped".to_string(),
            ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuestionFocus {
    Editor,
    Suggestions,
    Actions,
}

struct PendingQuestion {
    request: TuiQuestionRequest,
    recovery: Option<RecoveredQuestionTarget>,
    response: String,
    selected_option: Option<usize>,
    focus: QuestionFocus,
    error: Option<String>,
}

struct RecoveredQuestionTarget {
    store: SessionStore,
    session_id: String,
    invocation_id: String,
}

const MAX_COMPOSER_BYTES: usize = 16 * 1024;
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PasteInsertion {
    inserted_bytes: usize,
    truncated: bool,
    controls_omitted: bool,
}

impl PasteInsertion {
    fn visible_status(self) -> Option<String> {
        match (self.truncated, self.controls_omitted) {
            (true, true) => Some(format!(
                "[composer] paste truncated at {MAX_COMPOSER_BYTES} bytes; control characters omitted"
            )),
            (true, false) => Some(format!(
                "[composer] paste truncated at {MAX_COMPOSER_BYTES} bytes"
            )),
            (false, true) => {
                Some("[composer] unsafe paste control characters omitted".to_string())
            }
            (false, false) => None,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Composer {
    input: String,
    cursor: usize,
    history: DraftHistory,
    history_index: Option<usize>,
    stash: Option<String>,
}

impl Composer {
    #[cfg(test)]
    fn from_text(input: impl Into<String>) -> Self {
        let input = input.into();
        Self {
            cursor: input.len(),
            input,
            history: DraftHistory::default(),
            history_index: None,
            stash: None,
        }
    }

    fn set_text(&mut self, input: String) {
        self.cursor = input.len();
        self.input = input;
    }

    fn remember_submission(&mut self, submitted: &str) {
        if submitted.trim().is_empty() {
            return;
        }
        self.history.remember_submission(submitted);
        self.history_index = None;
        self.stash = None;
    }

    fn recall_older(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                self.stash = Some(self.input.clone());
                self.history.entries().len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_index = Some(next);
        if let Some(entry) = self.history.entry(next) {
            self.set_text(entry.to_string());
        }
    }

    fn recall_newer(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 >= self.history.entries().len() {
            self.history_index = None;
            let restored = self.stash.take().unwrap_or_default();
            self.set_text(restored);
            return;
        }
        self.history_index = Some(index + 1);
        if let Some(entry) = self.history.entry(index + 1) {
            self.set_text(entry.to_string());
        }
    }

    fn select_history_entry(&mut self, index: usize) -> bool {
        let Some(entry) = self.history.entry(index).map(str::to_string) else {
            return false;
        };
        if self.history_index.is_none() {
            self.stash = Some(self.input.clone());
        }
        self.history_index = Some(index);
        self.set_text(entry);
        true
    }

    fn clamp_cursor(&mut self) {
        if self.cursor > self.input.len() {
            self.cursor = self.input.len();
        }
        while self.cursor > 0 && !self.input.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    fn move_left(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.clamp_cursor();
    }

    fn move_right(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.input.len() {
            return;
        }
        self.cursor += 1;
        while self.cursor < self.input.len() && !self.input.is_char_boundary(self.cursor) {
            self.cursor += 1;
        }
    }

    fn insert_str(&mut self, text: &str) {
        self.clamp_cursor();
        if self.input.len().saturating_add(text.len()) > MAX_COMPOSER_BYTES {
            return;
        }
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn insert_paste(&mut self, pasted: &str) -> PasteInsertion {
        self.clamp_cursor();
        let available = MAX_COMPOSER_BYTES.saturating_sub(self.input.len());
        let mut insertion = String::with_capacity(available.min(pasted.len()));
        let mut outcome = PasteInsertion::default();
        let mut characters = pasted.chars().peekable();
        let mut capacity_exhausted = false;

        while let Some(character) = characters.next() {
            let mut encoded = [0u8; 4];
            let normalized = match character {
                '\r' => {
                    if characters.peek() == Some(&'\n') {
                        characters.next();
                    }
                    "\n"
                }
                '\n' => "\n",
                '\t' => "    ",
                character if character.is_control() => {
                    outcome.controls_omitted = true;
                    continue;
                }
                character => character.encode_utf8(&mut encoded),
            };
            if capacity_exhausted || insertion.len().saturating_add(normalized.len()) > available {
                outcome.truncated = true;
                capacity_exhausted = true;
                continue;
            }
            insertion.push_str(normalized);
        }

        outcome.inserted_bytes = insertion.len();
        self.input.insert_str(self.cursor, &insertion);
        self.cursor += insertion.len();
        outcome
    }

    fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let end = self.cursor;
        self.move_left();
        self.input.replace_range(self.cursor..end, "");
    }

    fn delete(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.input.len() {
            return;
        }
        let mut end = self.cursor + 1;
        while end < self.input.len() && !self.input.is_char_boundary(end) {
            end += 1;
        }
        self.input.replace_range(self.cursor..end, "");
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ComposerAction {
    Pending,
    Submit(String),
}

fn composer_action_for_key(
    composer: &mut Composer,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ComposerAction {
    match code {
        KeyCode::Left => {
            composer.move_left();
            ComposerAction::Pending
        }
        KeyCode::Right => {
            composer.move_right();
            ComposerAction::Pending
        }
        KeyCode::Home => {
            composer.cursor = 0;
            ComposerAction::Pending
        }
        KeyCode::End => {
            composer.cursor = composer.input.len();
            ComposerAction::Pending
        }
        KeyCode::Up => {
            composer.recall_older();
            ComposerAction::Pending
        }
        KeyCode::Down => {
            composer.recall_newer();
            ComposerAction::Pending
        }
        KeyCode::Char('j') | KeyCode::Char('J') if modifiers.contains(KeyModifiers::CONTROL) => {
            composer.insert_str("\n");
            ComposerAction::Pending
        }
        KeyCode::Enter
            if modifiers.contains(KeyModifiers::SHIFT) || modifiers.contains(KeyModifiers::ALT) =>
        {
            composer.insert_str("\n");
            ComposerAction::Pending
        }
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::CONTROL) => {
            composer.insert_str(&character.to_string());
            ComposerAction::Pending
        }
        KeyCode::Backspace => {
            composer.backspace();
            ComposerAction::Pending
        }
        KeyCode::Delete => {
            composer.delete();
            ComposerAction::Pending
        }
        KeyCode::Enter => {
            if composer.input.trim().is_empty() {
                ComposerAction::Pending
            } else {
                let submitted = std::mem::take(&mut composer.input);
                composer.cursor = 0;
                composer.remember_submission(&submitted);
                ComposerAction::Submit(submitted)
            }
        }
        KeyCode::Esc => ComposerAction::Pending,
        _ => ComposerAction::Pending,
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CompletionMenu {
    suggestions: Vec<InteractiveCompletion>,
    selected: usize,
    dismissed_for_input: Option<String>,
}

impl CompletionMenu {
    #[cfg(test)]
    fn sync(&mut self, input: &str) {
        self.sync_for(input, None);
    }

    fn sync_for(&mut self, input: &str, project_root: Option<&Path>) {
        if self.dismissed_for_input.as_deref() == Some(input) {
            self.suggestions.clear();
            self.selected = 0;
            return;
        }
        self.dismissed_for_input = None;
        self.suggestions = if input.starts_with('/') {
            interactive_completions(input)
        } else {
            project_root
                .map(|root| path_completions(root, input))
                .unwrap_or_default()
        };
        if self.selected >= self.suggestions.len() {
            self.selected = 0;
        }
    }

    fn is_open(&self) -> bool {
        !self.suggestions.is_empty()
    }

    fn handle_key(&mut self, composer: &mut Composer, code: KeyCode) -> CompletionKeyResult {
        if !self.is_open() {
            return CompletionKeyResult::Ignored;
        }
        match code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                CompletionKeyResult::Consumed
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.suggestions.len() - 1);
                CompletionKeyResult::Consumed
            }
            KeyCode::Tab => {
                if let Some(completion) = self.suggestions.get(self.selected) {
                    composer.set_text(completion.insertion.clone());
                }
                self.dismissed_for_input = Some(composer.input.clone());
                self.suggestions.clear();
                self.selected = 0;
                CompletionKeyResult::Consumed
            }
            KeyCode::Enter => {
                if let Some(completion) = self.suggestions.get(self.selected) {
                    composer.set_text(completion.insertion.clone());
                }
                let runnable = !composer.input.ends_with(' ');
                self.dismissed_for_input = Some(composer.input.clone());
                self.suggestions.clear();
                self.selected = 0;
                if runnable && !composer.input.trim().is_empty() {
                    CompletionKeyResult::Submit
                } else {
                    CompletionKeyResult::Consumed
                }
            }
            KeyCode::Esc => {
                self.dismissed_for_input = Some(composer.input.clone());
                self.suggestions.clear();
                self.selected = 0;
                CompletionKeyResult::Consumed
            }
            _ => CompletionKeyResult::Ignored,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingHistorySearch {
    query: String,
    search: DraftHistorySearch,
    selected: usize,
    error: Option<String>,
}

impl PendingHistorySearch {
    fn new(history: &DraftHistory, query: Option<String>) -> Self {
        let search = history.search(query.as_deref().unwrap_or_default());
        let error = history_search_notice(&search, history.is_empty());
        Self {
            query: search.query.clone(),
            search,
            selected: 0,
            error,
        }
    }

    fn refresh(&mut self, history: &DraftHistory) {
        self.search = history.search(&self.query);
        self.query = self.search.query.clone();
        self.selected = self
            .selected
            .min(self.search.matches.len().saturating_sub(1));
        self.error = history_search_notice(&self.search, history.is_empty());
    }

    fn insert(&mut self, character: char, history: &DraftHistory) {
        if character.is_control() {
            self.error = Some("[history error] control character ignored".to_string());
            return;
        }
        if self.query.len().saturating_add(character.len_utf8()) > MAX_DRAFT_HISTORY_QUERY_BYTES {
            self.error = Some(format!(
                "[history error] query is limited to {MAX_DRAFT_HISTORY_QUERY_BYTES} bytes"
            ));
            return;
        }
        self.query.push(character);
        self.selected = 0;
        self.refresh(history);
    }

    fn backspace(&mut self, history: &DraftHistory) {
        self.query.pop();
        self.selected = 0;
        self.refresh(history);
    }
}

fn history_search_notice(search: &DraftHistorySearch, history_empty: bool) -> Option<String> {
    if search.query_truncated {
        Some(format!(
            "[history error] query truncated at {MAX_DRAFT_HISTORY_QUERY_BYTES} bytes"
        ))
    } else if search.controls_omitted {
        Some("[history error] control characters omitted from query".to_string())
    } else if history_empty {
        Some("[history] no submitted drafts are available".to_string())
    } else if search.matches.is_empty() {
        Some("[history] no matching submitted drafts".to_string())
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistorySearchAction {
    Pending,
    Close,
    Select(usize),
}

fn history_search_action_for_key(
    search: &mut PendingHistorySearch,
    history: &DraftHistory,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> HistorySearchAction {
    match code {
        KeyCode::Up => {
            search.selected = search.selected.saturating_sub(1);
        }
        KeyCode::Down => {
            search.selected = search
                .selected
                .saturating_add(1)
                .min(search.search.matches.len().saturating_sub(1));
        }
        KeyCode::Backspace => search.backspace(history),
        KeyCode::Char(character)
            if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            search.insert(character, history);
        }
        KeyCode::Enter => {
            if let Some(result) = search.search.matches.get(search.selected) {
                return HistorySearchAction::Select(result.entry_index);
            }
            search.error = Some("[history error] select requires a matching draft".to_string());
        }
        KeyCode::Esc => return HistorySearchAction::Close,
        _ => {}
    }
    HistorySearchAction::Pending
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractionLayer {
    Approval,
    Question,
    Model,
    SessionConfirmation,
    SessionSwitcher,
    HistorySearch,
    Completion,
    Composer,
    RecoverableError,
}

fn tui_interaction_state(
    approval: bool,
    question: bool,
    model: bool,
    switcher: Option<&SessionSwitcher>,
    history_search: bool,
    completion: bool,
    run: InteractionRunState,
) -> InteractionState {
    InteractionState {
        approval_pending: approval,
        question_pending: question,
        destructive_confirmation_pending: switcher.is_some_and(|switcher| switcher.confirming),
        selector_or_detail: (model
            || history_search
            || switcher.is_some_and(|switcher| !switcher.confirming))
        .then_some(SelectorDetailKind::Selector),
        completion_pending: completion,
        run,
    }
}

fn tui_run_state(
    worker_active: bool,
    live_state: Option<&str>,
    reconciled_terminal: Option<InteractionTerminalOutcome>,
) -> InteractionRunState {
    if worker_active {
        match live_state {
            Some("planning") => InteractionRunState::Planning,
            Some("reconciliation") => InteractionRunState::Reconciling,
            _ => InteractionRunState::Running,
        }
    } else {
        match reconciled_terminal {
            Some(InteractionTerminalOutcome::Completed) => InteractionRunState::Completed,
            Some(InteractionTerminalOutcome::Cancelled) => InteractionRunState::Cancelled,
            Some(
                InteractionTerminalOutcome::Failed | InteractionTerminalOutcome::WaitingForInput,
            ) => InteractionRunState::Failed,
            None => InteractionRunState::Idle,
        }
    }
}

fn active_interaction_layer(
    approval: bool,
    question: bool,
    model: bool,
    switcher: Option<&SessionSwitcher>,
    history_search: bool,
    completion: bool,
) -> InteractionLayer {
    let state = tui_interaction_state(
        approval,
        question,
        model,
        switcher,
        history_search,
        completion,
        InteractionRunState::Idle,
    );
    let consumer = match reduce_interaction(&state, InteractionInput::UserAction) {
        InteractionReduction::Consumed(consumer) => consumer,
        _ => return InteractionLayer::RecoverableError,
    };
    match consumer {
        InteractionConsumer::Approval => InteractionLayer::Approval,
        InteractionConsumer::Question => InteractionLayer::Question,
        InteractionConsumer::DestructiveConfirmation => InteractionLayer::SessionConfirmation,
        InteractionConsumer::Selector if history_search => InteractionLayer::HistorySearch,
        InteractionConsumer::Selector if model => InteractionLayer::Model,
        InteractionConsumer::Selector => InteractionLayer::SessionSwitcher,
        InteractionConsumer::Completion => InteractionLayer::Completion,
        InteractionConsumer::Composer => InteractionLayer::Composer,
        InteractionConsumer::Detail | InteractionConsumer::Timeline => {
            InteractionLayer::RecoverableError
        }
    }
}

struct PendingModelSelection {
    selection: ModelSelection,
    response: String,
    selected_option: usize,
}

impl PendingModelSelection {
    fn new(selection: ModelSelection) -> Self {
        let selected_option = selection
            .available
            .iter()
            .position(|model| model == &selection.current)
            .unwrap_or(0);
        Self {
            selection,
            response: String::new(),
            selected_option,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ModelAction {
    Pending,
    Submit(String),
    Cancel,
}

fn model_action_for_key(model: &mut PendingModelSelection, code: KeyCode) -> ModelAction {
    match code {
        KeyCode::Char(character)
            if model.response.len().saturating_add(character.len_utf8()) <= MAX_COMPOSER_BYTES =>
        {
            model.response.push(character);
            ModelAction::Pending
        }
        KeyCode::Backspace => {
            model.response.pop();
            ModelAction::Pending
        }
        KeyCode::Up => {
            model.selected_option = model.selected_option.saturating_sub(1);
            ModelAction::Pending
        }
        KeyCode::Down | KeyCode::Tab => {
            if !model.selection.available.is_empty() {
                model.selected_option = (model.selected_option + 1)
                    .min(model.selection.available.len().saturating_sub(1));
            }
            ModelAction::Pending
        }
        KeyCode::Enter => {
            let response = model.response.trim();
            if let Ok(index) = response.parse::<usize>() {
                if index > 0 {
                    if let Some(selected) = model.selection.available.get(index - 1) {
                        return ModelAction::Submit(selected.clone());
                    }
                }
            }
            if !response.is_empty() {
                ModelAction::Submit(response.to_string())
            } else if let Some(selected) = model.selection.available.get(model.selected_option) {
                ModelAction::Submit(selected.clone())
            } else {
                ModelAction::Pending
            }
        }
        KeyCode::Esc => ModelAction::Cancel,
        _ => ModelAction::Pending,
    }
}

impl PendingQuestion {
    fn new(request: TuiQuestionRequest) -> Self {
        Self {
            request,
            recovery: None,
            response: String::new(),
            selected_option: None,
            focus: QuestionFocus::Editor,
            error: None,
        }
    }

    fn recovered(request: TuiQuestionRequest, target: RecoveredQuestionTarget) -> Self {
        let mut pending = Self::new(request);
        pending.recovery = Some(target);
        pending
    }
}

#[derive(Debug, PartialEq, Eq)]
enum QuestionAction {
    Pending,
    Submit(String),
    LeaveUnanswered,
    Error(String),
}

fn submit_question_answer(question: &PendingQuestion, answer: &str) -> QuestionAction {
    let state = InteractionState {
        question_pending: true,
        ..InteractionState::default()
    };
    match reduce_interaction(
        &state,
        InteractionInput::QuestionAnswer {
            answer,
            options: &question.request.options,
            selected_option: question.selected_option,
        },
    ) {
        InteractionReduction::QuestionAnswered(answer) => QuestionAction::Submit(answer),
        InteractionReduction::QuestionLeftUnanswered => QuestionAction::LeaveUnanswered,
        InteractionReduction::Error { message, .. } => QuestionAction::Error(message),
        InteractionReduction::ModalCommand(_) => QuestionAction::Error(
            crate::interactive::modal_command_unsupported_message().to_string(),
        ),
        _ => QuestionAction::Error("question input was rejected by the shared reducer".to_string()),
    }
}

fn question_action_for_key(question: &mut PendingQuestion, code: KeyCode) -> QuestionAction {
    match code {
        KeyCode::Tab => {
            question.focus = match question.focus {
                QuestionFocus::Editor if !question.request.options.is_empty() => {
                    QuestionFocus::Suggestions
                }
                QuestionFocus::Editor | QuestionFocus::Suggestions => QuestionFocus::Actions,
                QuestionFocus::Actions => QuestionFocus::Editor,
            };
            QuestionAction::Pending
        }
        KeyCode::BackTab => {
            question.focus = match question.focus {
                QuestionFocus::Editor => QuestionFocus::Actions,
                QuestionFocus::Suggestions => QuestionFocus::Editor,
                QuestionFocus::Actions if question.request.options.is_empty() => {
                    QuestionFocus::Editor
                }
                QuestionFocus::Actions => QuestionFocus::Suggestions,
            };
            QuestionAction::Pending
        }
        KeyCode::Char(character)
            if question.focus == QuestionFocus::Editor
                && question.response.len().saturating_add(character.len_utf8())
                    <= MAX_COMPOSER_BYTES =>
        {
            question.response.push(character);
            question.error = None;
            QuestionAction::Pending
        }
        KeyCode::Backspace if question.focus == QuestionFocus::Editor => {
            question.response.pop();
            question.error = None;
            QuestionAction::Pending
        }
        KeyCode::Up if question.focus == QuestionFocus::Suggestions => {
            if let Some(selected) = question.selected_option {
                question.selected_option = selected.checked_sub(1);
            }
            QuestionAction::Pending
        }
        KeyCode::Down if question.focus == QuestionFocus::Suggestions => {
            let last = question.request.options.len().saturating_sub(1);
            question.selected_option = Some(match question.selected_option {
                Some(selected) => selected.saturating_add(1).min(last),
                None => 0,
            });
            QuestionAction::Pending
        }
        KeyCode::Enter => match question.focus {
            QuestionFocus::Actions => QuestionAction::LeaveUnanswered,
            QuestionFocus::Suggestions => {
                if let Some(index) = question.selected_option {
                    if let Some(option) = question.request.options.get(index) {
                        return QuestionAction::Submit(option.clone());
                    }
                }
                QuestionAction::Error("select an option or type a custom answer".to_string())
            }
            QuestionFocus::Editor => submit_question_answer(question, &question.response),
        },
        KeyCode::Esc => QuestionAction::LeaveUnanswered,
        _ => QuestionAction::Pending,
    }
}

fn paste_question_answer(question: &mut PendingQuestion, pasted: &str) {
    if question.focus != QuestionFocus::Editor {
        return;
    }
    let input = std::mem::take(&mut question.response);
    let mut editor = Composer {
        cursor: input.len(),
        input,
        ..Composer::default()
    };
    let outcome = editor.insert_paste(pasted);
    question.response = editor.input;
    question.error = outcome.visible_status();
}

fn open_prompt_command_overlay(
    code: KeyCode,
    prompt_pending: bool,
    command_overlay: &mut Option<String>,
) -> bool {
    if matches!(code, KeyCode::F(2)) && prompt_pending && command_overlay.is_none() {
        *command_overlay = Some(String::new());
        true
    } else {
        false
    }
}

fn handle_question_key(question: &mut Option<PendingQuestion>, code: KeyCode) -> bool {
    let Some(pending) = question.as_mut() else {
        return false;
    };
    let action = question_action_for_key(pending, code);
    match action {
        QuestionAction::Pending => false,
        QuestionAction::Submit(response) => {
            if let Some(target) = question
                .as_ref()
                .and_then(|pending| pending.recovery.as_ref())
            {
                match crate::interactive::persist_recovered_question_answer(
                    &target.store,
                    &target.session_id,
                    &target.invocation_id,
                    &response,
                ) {
                    Ok(_) => {
                        question.take();
                        return true;
                    }
                    Err(message) => {
                        if let Some(pending) = question.as_mut() {
                            pending.error = Some(message);
                        }
                        return false;
                    }
                }
            }
            if let Some(pending) = question.take() {
                let _ = pending
                    .request
                    .reply
                    .send(crate::agent::QuestionOutcome::Answered(response));
            }
            true
        }
        QuestionAction::LeaveUnanswered => {
            if let Some(pending) = question.take() {
                let _ = pending
                    .request
                    .reply
                    .send(crate::agent::QuestionOutcome::LeftUnanswered);
            }
            true
        }
        QuestionAction::Error(message) => {
            if let Some(pending) = question.as_mut() {
                pending.error = Some(message);
            }
            false
        }
    }
}

fn approval_decision_from_answer(answer: &str) -> Result<ApprovalDecision, String> {
    let state = InteractionState {
        approval_pending: true,
        ..InteractionState::default()
    };
    match reduce_interaction(&state, InteractionInput::ApprovalAnswer(answer)) {
        InteractionReduction::ApprovalDecision(InteractionDecision::Accept) => {
            Ok(ApprovalDecision::granted_user())
        }
        InteractionReduction::ApprovalDecision(InteractionDecision::Reject) => {
            Ok(ApprovalDecision::denied())
        }
        InteractionReduction::Error { message, .. } => Err(message),
        _ => Err("approval input was rejected".to_string()),
    }
}

fn handle_approval_key(approval: &mut Option<TuiApprovalRequest>, code: KeyCode) -> bool {
    let Some(pending) = approval.as_mut() else {
        return false;
    };
    if pending.details_open {
        match code {
            KeyCode::Esc => pending.details_open = false,
            KeyCode::Up => pending.detail_offset = pending.detail_offset.saturating_sub(1),
            KeyCode::Down => pending.detail_offset = pending.detail_offset.saturating_add(1),
            KeyCode::PageUp => pending.detail_offset = pending.detail_offset.saturating_sub(5),
            KeyCode::PageDown => pending.detail_offset = pending.detail_offset.saturating_add(5),
            _ => {}
        }
        return true;
    }
    match code {
        KeyCode::Esc => {
            if let Some(request) = approval.take() {
                let _ = request.reply.send(ApprovalDecision::denied());
            }
            return true;
        }
        KeyCode::Up => {
            pending.selected_option = pending.selected_option.saturating_sub(1);
            pending.typed.clear();
            pending.error = None;
            return true;
        }
        KeyCode::Down | KeyCode::Tab => {
            pending.selected_option = (pending.selected_option + 1).min(2);
            pending.typed.clear();
            pending.error = None;
            return true;
        }
        KeyCode::Char(character) if !character.is_control() => {
            pending.typed.push(character);
            pending.error = None;
            return true;
        }
        KeyCode::Backspace => {
            pending.typed.pop();
            pending.error = None;
            return true;
        }
        KeyCode::Enter => {
            let typed = pending.typed.trim().to_string();
            if typed.is_empty() && pending.selected_option == 2 {
                pending.details_open = true;
                pending.detail_offset = 0;
                return true;
            }
            let answer = if typed.is_empty() {
                if pending.selected_option == 0 {
                    "y"
                } else {
                    "n"
                }
            } else {
                typed.as_str()
            };
            match approval_decision_from_answer(answer) {
                Ok(decision) => {
                    if let Some(request) = approval.take() {
                        let _ = request.reply.send(decision);
                    }
                }
                Err(message) => {
                    pending.typed.clear();
                    pending.error = Some(message);
                }
            }
            return true;
        }
        _ => {}
    }
    false
}

fn handle_pending_interaction_key(
    approval: &mut Option<TuiApprovalRequest>,
    question: &mut Option<PendingQuestion>,
    code: KeyCode,
) -> bool {
    if approval.is_some() {
        handle_approval_key(approval, code);
        return true;
    }
    if question.is_some() {
        handle_question_key(question, code);
        return true;
    }
    false
}

fn render_model_selection(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    pending: &PendingModelSelection,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let safe_label = |value: &str| {
        bounded_public_text(
            value,
            &pending.selection.sensitive_values,
            MAX_SESSION_DETAIL_ITEM_CHARS,
            false,
        )
    };
    let hint_rows = u16::from(inner.height > 1);
    let capacity = usize::from(inner.height.saturating_sub(hint_rows)).max(1);
    let (start, end) = visible_option_range(
        pending.selected_option,
        pending.selection.available.len(),
        capacity,
    );
    let mut lines = if pending.selection.available.is_empty() {
        vec![list_option_line(
            "No configured suggestions; type an exact model ID.",
            false,
            inner.width,
            no_color,
        )]
    } else {
        pending.selection.available[start..end]
            .iter()
            .enumerate()
            .map(|(offset, model)| {
                let current = if model == &pending.selection.current {
                    "current"
                } else {
                    ""
                };
                two_column_option(
                    &safe_label(model),
                    current,
                    start + offset == pending.selected_option,
                    inner.width,
                    no_color,
                )
            })
            .collect()
    };
    if hint_rows > 0 {
        let typed = safe_label(&pending.response);
        let hint = if typed.is_empty() {
            "Up/Down select · type exact ID · Enter select · Esc cancel".to_string()
        } else {
            format!("ID {typed} · Enter select · Esc cancel")
        };
        lines.push(band_hint_line(&hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SwitcherAction {
    Pending,
    Close,
    PreviewExact(String),
    Activate,
}

fn session_switcher_action_for_key(
    switcher: &mut SessionSwitcher,
    code: KeyCode,
) -> SwitcherAction {
    if switcher.confirming {
        let answer = match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => "y",
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => "n",
            _ => return SwitcherAction::Pending,
        };
        let state = InteractionState {
            destructive_confirmation_pending: true,
            ..InteractionState::default()
        };
        return match reduce_interaction(&state, InteractionInput::ConfirmationAnswer(answer)) {
            InteractionReduction::ConfirmationDecision(InteractionDecision::Accept) => {
                SwitcherAction::Activate
            }
            InteractionReduction::ConfirmationDecision(InteractionDecision::Reject) => {
                switcher.confirming = false;
                SwitcherAction::Pending
            }
            _ => SwitcherAction::Pending,
        };
    }
    match code {
        KeyCode::Esc => SwitcherAction::Close,
        KeyCode::Backspace => {
            switcher.exact_id.pop();
            switcher.error = None;
            SwitcherAction::Pending
        }
        KeyCode::Char(character)
            if switcher.exact_id.len().saturating_add(character.len_utf8())
                <= MAX_SWITCHER_EXACT_ID_BYTES =>
        {
            switcher.exact_id.push(character);
            switcher.error = None;
            SwitcherAction::Pending
        }
        KeyCode::Up => {
            switcher.selected = switcher.selected.saturating_sub(1);
            SwitcherAction::Pending
        }
        KeyCode::Down => {
            if !switcher.candidates.is_empty() {
                switcher.selected =
                    (switcher.selected + 1).min(switcher.candidates.len().saturating_sub(1));
            }
            SwitcherAction::Pending
        }
        KeyCode::Enter if !switcher.exact_id.trim().is_empty() => {
            SwitcherAction::PreviewExact(switcher.exact_id.trim().to_string())
        }
        KeyCode::Enter if !switcher.candidates.is_empty() => {
            switcher.confirming = true;
            SwitcherAction::Pending
        }
        _ => SwitcherAction::Pending,
    }
}

fn preview_exact_session(
    store: &SessionStore,
    switcher: &mut SessionSwitcher,
    active_session_id: &str,
    session_id: &str,
) -> Result<(), String> {
    let candidate = interactive_session_candidate(store, session_id, active_session_id)?;
    if let Some(index) = switcher
        .candidates
        .iter()
        .position(|existing| existing.id == candidate.id)
    {
        switcher.candidates[index] = candidate;
        switcher.selected = index;
    } else {
        if switcher.candidates.len() >= MAX_SWITCHER_CANDIDATES {
            let replace = switcher
                .candidates
                .iter()
                .rposition(|existing| !existing.is_active)
                .ok_or_else(|| "no bounded switcher slot is available".to_string())?;
            switcher.candidates.remove(replace);
        }
        switcher.candidates.push(candidate);
        switcher.selected = switcher.candidates.len() - 1;
    }
    switcher.exact_id.clear();
    switcher.error = None;
    Ok(())
}

fn activate_selected_session(
    store: &SessionStore,
    switcher: &SessionSwitcher,
    worker_active: bool,
    active_session_id: &mut String,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    if worker_active {
        return Err(
            "Agent is still running; cancel it or wait before switching sessions.".to_string(),
        );
    }
    let candidate = switcher
        .candidates
        .get(switcher.selected)
        .ok_or_else(|| "No session is selected.".to_string())?;
    let target = candidate.id.clone();
    // This is deliberately a strict, preview-token-validated read, not
    // resolve_session: a stale target must never create a replacement session or
    // redirect the active workload.
    let session = validate_interactive_session_target(store, candidate).map_err(|error| {
        format!("Could not resume {target}: {error}. The active session is unchanged.")
    })?;
    let replacement =
        ActiveTimeline::from_session(&session, store.public_sensitive_values().to_vec());
    *timeline = replacement;
    *active_session_id = target.clone();
    Ok(target)
}

fn replace_active_session(
    store: &SessionStore,
    session_id: String,
    output: String,
    active_session_id: &mut String,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    let previous = active_session_id.clone();
    let disposition = queue_disposition_message(store, &previous, "switched sessions")?;
    let mut replacement = ActiveTimeline::load(store, &session_id)
        .map_err(|error| format!("created session could not be loaded: {error}"))?;
    replacement.push_status(output);
    replacement.push_status(disposition.clone());
    *timeline = replacement;
    *active_session_id = session_id;
    Ok(disposition)
}

fn render_session_switcher(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    switcher: &SessionSwitcher,
    active_session_id: &str,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let hint_rows = u16::from(inner.height > 1);
    let mut lines = Vec::new();
    if switcher.confirming {
        let label = switcher
            .candidates
            .get(switcher.selected)
            .map(|candidate| candidate.label.as_str())
            .unwrap_or("session");
        lines.push(two_column_option(
            &format!("Resume {label}"),
            "replaces this session",
            true,
            inner.width,
            no_color,
        ));
        if hint_rows > 0 {
            lines.push(band_hint_line(
                "Y / Enter resume · N / Esc keep current",
                inner.width,
                no_color,
            ));
        }
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    if !switcher.exact_id.is_empty() && inner.height > 2 {
        lines.push(band_hint_line(
            &format!("ID {}", switcher.exact_id),
            inner.width,
            no_color,
        ));
    }
    let used = lines.len();
    let capacity = usize::from(inner.height.saturating_sub(hint_rows))
        .saturating_sub(used)
        .max(1);
    let (start, end) = visible_option_range(switcher.selected, switcher.candidates.len(), capacity);
    if switcher.candidates.is_empty() {
        lines.push(list_option_line(
            "(no sessions)",
            false,
            inner.width,
            no_color,
        ));
    } else {
        for (offset, candidate) in switcher.candidates[start..end].iter().enumerate() {
            let description = if candidate.id == active_session_id {
                "active"
            } else {
                candidate.preview.lines().next().unwrap_or("")
            };
            lines.push(two_column_option(
                &candidate.label,
                description,
                start + offset == switcher.selected,
                inner.width,
                no_color,
            ));
        }
    }
    if hint_rows > 0 {
        let error_hint = switcher
            .error
            .as_ref()
            .map(|error| format!("[switcher error] {error}"));
        let hint = if let Some(error) = error_hint.as_deref() {
            error
        } else if switcher.omitted > 0 {
            "Up/Down select · type exact ID · Enter resume · Esc close"
        } else {
            "Up/Down select · Enter resume · Esc close"
        };
        lines.push(band_hint_line(hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn completion_reserved_height(
    area_height: u16,
    row_count: usize,
    composer_height: u16,
    meter_height: u16,
) -> u16 {
    if row_count == 0 {
        return 0;
    }
    let desired =
        u16::try_from(row_count.min(MAX_VISIBLE_COMPLETIONS).saturating_add(1)).unwrap_or(u16::MAX);
    let chrome = 1u16
        .saturating_add(3)
        .saturating_add(composer_height)
        .saturating_add(meter_height)
        .saturating_add(1);
    desired.min(area_height.saturating_sub(chrome))
}

fn split_session_layout(
    area: Rect,
    composer_height: u16,
    meter_height: u16,
    completion_height: u16,
) -> SessionLayout {
    let mut constraints = vec![Constraint::Length(1), Constraint::Min(3)];
    if meter_height > 0 {
        constraints.push(Constraint::Length(meter_height));
    }
    constraints.push(Constraint::Length(composer_height));
    if completion_height > 0 {
        constraints.push(Constraint::Length(completion_height));
    }
    constraints.push(Constraint::Length(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut index = 0;
    let header = chunks[index];
    index += 1;
    let transcript = chunks[index];
    index += 1;
    let meter = if meter_height > 0 {
        let rect = chunks[index];
        index += 1;
        rect
    } else {
        Rect::default()
    };
    let composer = chunks[index];
    index += 1;
    let completion = if completion_height > 0 {
        let rect = chunks[index];
        index += 1;
        rect
    } else {
        Rect::default()
    };
    SessionLayout {
        header,
        transcript,
        meter,
        composer,
        completion,
        footer: chunks[index],
    }
}

fn completion_inner_rect(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let left = COMPOSER_PROMPT_CELLS.min(area.width.saturating_sub(1));
    let width = area.width.saturating_sub(left).clamp(1, 92);
    Rect {
        x: area.x.saturating_add(left),
        y: area.y,
        width,
        height: area.height,
    }
}

#[cfg(test)]
fn completion_rect(area: Rect, row_count: usize, composer_height: u16) -> Rect {
    let height = completion_reserved_height(area.height, row_count, composer_height, 0);
    let layout = split_session_layout(area, composer_height, 0, height);
    completion_inner_rect(layout.completion)
}

fn completion_signature(suggestion: &InteractiveCompletion) -> &str {
    let insertion = suggestion.insertion.trim_end();
    if suggestion.insertion.starts_with('/') && !insertion.contains(char::is_whitespace) {
        suggestion.usage
    } else {
        insertion
    }
}

fn selected_option_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
}

fn list_option_line(text: &str, selected: bool, width: u16, no_color: bool) -> Line<'static> {
    let style = if selected {
        selected_option_style(no_color)
    } else {
        Style::default()
    };
    Line::from(Span::styled(
        truncate_completion_text(text, usize::from(width.max(1))),
        style,
    ))
}

fn truncate_completion_text(value: &str, max_cells: usize) -> String {
    if unicode_display_width(value) <= max_cells {
        return value.to_string();
    }
    if max_cells == 0 {
        return String::new();
    }
    let mut output = String::new();
    for grapheme in value.graphemes(true) {
        let mut candidate = output.clone();
        candidate.push_str(grapheme);
        if unicode_display_width(&candidate) > max_cells.saturating_sub(1) {
            break;
        }
        output.push_str(grapheme);
    }
    output.push('…');
    output
}

fn two_column_text(signature: &str, description: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let signature_column = width.saturating_mul(2).checked_div(5).unwrap_or(0).max(1);
    let signature = truncate_completion_text(signature, signature_column);
    let signature_width = unicode_display_width(&signature);
    let gap = if width > signature_column { 2 } else { 0 };
    let description_width = width.saturating_sub(signature_column.saturating_add(gap));
    let description = truncate_completion_text(description, description_width);
    format!(
        "{signature}{}{description}",
        " ".repeat(
            signature_column
                .saturating_sub(signature_width)
                .saturating_add(gap)
        )
    )
}

fn two_column_option(
    signature: &str,
    description: &str,
    selected: bool,
    width: u16,
    no_color: bool,
) -> Line<'static> {
    list_option_line(
        &two_column_text(signature, description, width),
        selected,
        width,
        no_color,
    )
}

fn completion_line(suggestion: &InteractiveCompletion, width: u16) -> String {
    two_column_text(completion_signature(suggestion), suggestion.summary, width)
}

fn visible_option_range(selected: usize, len: usize, capacity: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let capacity = capacity.max(1);
    let selected = if selected < len { selected } else { 0 };
    let start = selected.saturating_sub(capacity.saturating_sub(1));
    (start, start.saturating_add(capacity).min(len))
}

fn band_hint_line(text: &str, width: u16, no_color: bool) -> Line<'static> {
    Line::from(Span::styled(
        truncate_completion_text(text, usize::from(width.max(1))),
        muted_style(no_color),
    ))
}

fn numbered_choice_line(
    index: usize,
    text: &str,
    shortcut: &str,
    selected: bool,
    width: u16,
    no_color: bool,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let body = format!("{}. {text} ({shortcut})", index + 1);
    let line = truncate_completion_text(&format!("{marker}{body}"), usize::from(width.max(1)));
    let style = if selected {
        selected_option_style(no_color)
    } else if no_color {
        Style::default()
    } else {
        speech_body_style(false)
    };
    Line::from(Span::styled(line, style))
}

fn render_numbered_choice_band(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    choices: &[(String, String)],
    selected: usize,
    hint: &str,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let hint_rows = u16::from(inner.height > 1);
    let capacity = usize::from(inner.height.saturating_sub(hint_rows)).max(1);
    let (start, end) = visible_option_range(selected, choices.len(), capacity);
    let mut lines = choices[start..end]
        .iter()
        .enumerate()
        .map(|(offset, (text, shortcut))| {
            numbered_choice_line(
                start + offset,
                text,
                shortcut,
                start + offset == selected,
                inner.width,
                no_color,
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() || hint_rows > 0 {
        lines.push(band_hint_line(hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

#[allow(dead_code)]
fn render_choice_band(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    options: &[(&str, &str)],
    selected: usize,
    hint: &str,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let hint_rows = u16::from(inner.height > 1);
    let capacity = usize::from(inner.height.saturating_sub(hint_rows)).max(1);
    let (start, end) = visible_option_range(selected, options.len(), capacity);
    let mut lines = options[start..end]
        .iter()
        .enumerate()
        .map(|(offset, (signature, description))| {
            two_column_option(
                signature,
                description,
                start + offset == selected,
                inner.width,
                no_color,
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() || hint_rows > 0 {
        lines.push(band_hint_line(hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_approval_band(frame: &mut ratatui::Frame<'_>, area: Rect, req: &TuiApprovalRequest) {
    if req.details_open {
        render_choice_band(
            frame,
            area,
            &[("Approval details", "Esc returns to the decision")],
            0,
            "Up/Down scroll · Esc back",
        );
        return;
    }
    render_numbered_choice_band(
        frame,
        area,
        &[
            ("Approve once".to_string(), "y".to_string()),
            ("Deny".to_string(), "esc".to_string()),
            ("View details".to_string(), "enter".to_string()),
        ],
        req.selected_option.min(2),
        "Up/Down select · Enter choose · Esc deny",
    );
}

fn render_question_band(frame: &mut ratatui::Frame<'_>, area: Rect, question: &PendingQuestion) {
    let mut choices: Vec<(String, String)> = question
        .request
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| (option.clone(), (index + 1).to_string()))
        .collect();
    choices.push(("Leave unanswered".to_string(), "esc".to_string()));
    let selected = match question.focus {
        QuestionFocus::Suggestions => question.selected_option.unwrap_or(0),
        QuestionFocus::Actions => choices.len().saturating_sub(1),
        QuestionFocus::Editor => choices.len(),
    };
    let hint = if question.focus == QuestionFocus::Editor {
        "Type an answer · Tab suggestions · Enter submit · Esc leave unanswered"
    } else {
        "Up/Down select · Tab editor/actions · Enter submit · Esc leave unanswered"
    };
    render_numbered_choice_band(frame, area, &choices, selected, hint);
}

fn render_workspace_band(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    _directory: &str,
    selected: usize,
) {
    render_numbered_choice_band(
        frame,
        area,
        &[
            ("Allow".to_string(), "y".to_string()),
            ("Decline and quit".to_string(), "esc".to_string()),
        ],
        selected.min(1),
        "Up/Down select · Enter choose · Esc decline",
    );
}

fn render_completion(frame: &mut ratatui::Frame<'_>, area: Rect, completion: &CompletionMenu) {
    if !completion.is_open() || area.width == 0 || area.height == 0 {
        return;
    }
    let modal_area = completion_inner_rect(area);
    if modal_area.width == 0 || modal_area.height == 0 {
        return;
    }
    let hint_rows = u16::from(modal_area.height > 1);
    let visible_capacity = usize::from(modal_area.height.saturating_sub(hint_rows)).max(1);
    let start = completion
        .selected
        .saturating_sub(visible_capacity.saturating_sub(1));
    let end = (start + visible_capacity).min(completion.suggestions.len());
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let inner_width = modal_area.width;
    let mut lines = completion.suggestions[start..end]
        .iter()
        .enumerate()
        .map(|(offset, suggestion)| {
            let selected = start + offset == completion.selected;
            let style = if selected {
                selected_option_style(no_color)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                completion_line(suggestion, inner_width),
                style,
            ))
        })
        .collect::<Vec<_>>();
    if hint_rows > 0 {
        lines.push(Line::from(Span::styled(
            truncate_completion_text(
                "Up/Down select · Tab insert · Enter run · Esc close",
                usize::from(inner_width),
            ),
            muted_style(no_color),
        )));
    }
    frame.render_widget(Paragraph::new(lines), modal_area);
}

fn render_history_search(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    search: &PendingHistorySearch,
) {
    let inner = completion_inner_rect(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let hint_rows = u16::from(inner.height > 1);
    let mut lines = Vec::new();
    if inner.height > 2 {
        let query = if search.query.is_empty() {
            "type to search drafts"
        } else {
            search.query.as_str()
        };
        lines.push(band_hint_line(query, inner.width, no_color));
    }
    let used = lines.len();
    let capacity = usize::from(inner.height.saturating_sub(hint_rows))
        .saturating_sub(used)
        .max(1);
    let (start, end) = visible_option_range(search.selected, search.search.matches.len(), capacity);
    if search.search.matches.is_empty() {
        lines.push(list_option_line(
            "(no matches)",
            false,
            inner.width,
            no_color,
        ));
    } else {
        for (offset, result) in search.search.matches[start..end].iter().enumerate() {
            lines.push(list_option_line(
                &result.display,
                start + offset == search.selected,
                inner.width,
                no_color,
            ));
        }
    }
    if hint_rows > 0 {
        let hint = search
            .error
            .as_deref()
            .unwrap_or("Up/Down select · Enter restore · Esc close");
        lines.push(band_hint_line(hint, inner.width, no_color));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_modal_state_error(frame: &mut ratatui::Frame<'_>, message: &'static str) {
    let modal_area = centered_rect(70, 30, frame.area());
    frame.render_widget(ratatui::widgets::Clear, modal_area);
    frame.render_widget(
        Paragraph::new(message)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Recoverable UI Error "),
            )
            .wrap(ratatui::widgets::Wrap { trim: true }),
        modal_area,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_interaction_overlay(
    frame: &mut ratatui::Frame<'_>,
    layer: InteractionLayer,
    pending_model: Option<&PendingModelSelection>,
    pending_switcher: Option<&SessionSwitcher>,
    pending_history_search: Option<&PendingHistorySearch>,
    _active_session_id: &str,
) {
    match layer {
        InteractionLayer::Model if pending_model.is_none() => render_modal_state_error(
            frame,
            "Model selector state is unavailable; press Esc to continue.",
        ),
        InteractionLayer::SessionConfirmation | InteractionLayer::SessionSwitcher
            if pending_switcher.is_none() =>
        {
            render_modal_state_error(
                frame,
                "Session selector state is unavailable; press Esc to continue.",
            );
        }
        InteractionLayer::HistorySearch if pending_history_search.is_none() => {
            render_modal_state_error(
                frame,
                "Draft history state is unavailable; press Esc to continue.",
            );
        }
        InteractionLayer::RecoverableError => render_modal_state_error(
            frame,
            "Interaction state is unavailable; press Esc to continue.",
        ),
        InteractionLayer::Model
        | InteractionLayer::SessionConfirmation
        | InteractionLayer::SessionSwitcher
        | InteractionLayer::HistorySearch
        | InteractionLayer::Approval
        | InteractionLayer::Question
        | InteractionLayer::Composer
        | InteractionLayer::Completion => {}
    }
}

struct TuiAgentWorker {
    run_id: String,
    mode: InteractiveAgentMode,
    cancellation: CancellationSignal,
    steering: Option<crate::agent::ExactRunSteeringHandle>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct TuiAgentProfileScope {
    project_root: std::path::PathBuf,
    profile_id: String,
    sessions_dir: std::path::PathBuf,
}

type TuiAgentStart = (
    String,
    InteractiveAgentMode,
    String,
    Option<crate::agent::ExactRunSteeringReceiver>,
    Option<String>,
);

struct PreparedTuiAgentWorker {
    cancellation: Option<CancellationSignal>,
    start_tx: Option<mpsc::SyncSender<TuiAgentStart>>,
    session_store: SessionStore,
    session_id: String,
    handle: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct SessionStreamEvent {
    session_id: String,
    run_id: String,
    event: StreamEvent,
}

impl TuiAgentWorker {
    fn request_cancellation(&self) {
        self.cancellation.cancel();
    }

    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn submit_steering(&self, text: &str) -> Result<usize, String> {
        self.steering
            .as_ref()
            .ok_or_else(|| "this active operation does not accept exact-run steering".to_string())?
            .submit(text)
    }

    fn fail_unaccounted_steering(&self, reason: &str) -> Result<(), String> {
        self.steering
            .as_ref()
            .map_or(Ok(()), |steering| steering.fail_unaccounted(reason))
    }

    fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| io::Error::other("TUI agent worker panicked"))
    }
}

fn submit_tui_steering_draft(
    worker: Option<&TuiAgentWorker>,
    composer: &mut Composer,
) -> Result<(String, usize), String> {
    let reduction = reduce_interaction(
        &InteractionState {
            run: tui_run_state(worker.is_some(), None, None),
            ..InteractionState::default()
        },
        InteractionInput::SteerCurrent(&composer.input),
    );
    let text = match reduction {
        InteractionReduction::SteerCurrent(text) => text,
        InteractionReduction::Error { message, .. } => return Err(message),
        _ => return Err("steering input had no valid consumer".to_string()),
    };
    let sequence = worker
        .ok_or_else(|| "exact active run is unavailable".to_string())?
        .submit_steering(&text)?;
    composer.remember_submission(&text);
    composer.set_text(String::new());
    Ok((text, sequence))
}

impl PreparedTuiAgentWorker {
    fn start(self, goal: String, mode: InteractiveAgentMode) -> io::Result<TuiAgentWorker> {
        self.start_with_continuation(goal, mode, None)
    }

    fn start_with_continuation(
        mut self,
        goal: String,
        mode: InteractiveAgentMode,
        continuation_plan_id: Option<String>,
    ) -> io::Result<TuiAgentWorker> {
        let start_tx = self
            .start_tx
            .take()
            .ok_or_else(|| io::Error::other("prepared TUI worker has no start channel"))?;
        let run_id = uuid::Uuid::new_v4().simple().to_string();
        let (steering, steering_receiver) = if mode == InteractiveAgentMode::Compact {
            (None, None)
        } else {
            let (steering, receiver) = crate::agent::exact_run_steering_channel(
                self.session_store.clone(),
                self.session_id.clone(),
                run_id.clone(),
                "tui",
            )
            .map_err(io::Error::other)?;
            (Some(steering), Some(receiver))
        };
        start_tx
            .send((
                goal,
                mode,
                run_id.clone(),
                steering_receiver,
                continuation_plan_id,
            ))
            .map_err(|_| io::Error::other("prepared TUI worker stopped before activation"))?;
        Ok(TuiAgentWorker {
            run_id,
            mode,
            cancellation: self.cancellation.take().ok_or_else(|| {
                io::Error::other("prepared TUI worker has no cancellation signal")
            })?,
            steering,
            handle: self.handle.take(),
        })
    }
}

impl Drop for PreparedTuiAgentWorker {
    fn drop(&mut self) {
        self.start_tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn prepare_tui_agent_worker(
    profile_scope: TuiAgentProfileScope,
    session_id: String,
    approval_tx: mpsc::Sender<TuiApprovalRequest>,
    question_tx: mpsc::Sender<TuiQuestionRequest>,
    stream_tx: tokio::sync::mpsc::Sender<SessionStreamEvent>,
) -> io::Result<PreparedTuiAgentWorker> {
    let TuiAgentProfileScope {
        project_root,
        profile_id,
        sessions_dir,
    } = profile_scope;
    let cancellation = CancellationSignal::new();
    let run_cancellation = cancellation.clone();
    let session_store = SessionStore::at_dir(sessions_dir.clone());
    let prepared_session_id = session_id.clone();
    let (start_tx, start_rx) = mpsc::sync_channel::<TuiAgentStart>(0);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(0);
    let handle = std::thread::Builder::new()
        .name("nib-tui-agent".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!(
                        "failed to initialize the async runtime: {error}"
                    )));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            let Ok((goal, mode, run_id, steering, continuation_plan_id)) = start_rx.recv() else {
                return;
            };

            runtime.block_on(async move {
                let (agent_stream_tx, mut agent_stream_rx) =
                    tokio::sync::mpsc::channel::<StreamEvent>(100);
                let forwarding_session_id = session_id.clone();
                let forwarding_run_id = run_id.clone();
                let forwarding_tx = stream_tx.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(event) = agent_stream_rx.recv().await {
                        if forwarding_tx
                            .send(SessionStreamEvent {
                                session_id: forwarding_session_id.clone(),
                                run_id: forwarding_run_id.clone(),
                                event,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                let loop_cfg = crate::agent::AgentLoopConfig {
                    max_steps: 0,
                    mode: mode.as_str().to_string(),
                    interactive_request: mode == InteractiveAgentMode::Execute,
                    approval_handler: Some(std::sync::Arc::new(TuiApprovalHandler {
                        tx: approval_tx,
                    })),
                    question_handler: Some(std::sync::Arc::new(TuiQuestionHandler {
                        tx: question_tx,
                    })),
                    stream_tx: Some(agent_stream_tx.clone()),
                    cancellation: Some(run_cancellation),
                    run_id: Some(run_id),
                    steering,
                    continuation_plan_id,
                    ..Default::default()
                };

                if let Err(error) = crate::agent::run_agent_loop_for_profile(
                    project_root,
                    &profile_id,
                    &sessions_dir,
                    &session_id,
                    &goal,
                    loop_cfg,
                )
                .await
                {
                    let _ = agent_stream_tx
                        .send(safe_agent_error_stream_event(&error))
                        .await;
                }
                drop(agent_stream_tx);
                let _ = forwarder.await;
            });
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PreparedTuiAgentWorker {
            cancellation: Some(cancellation),
            start_tx: Some(start_tx),
            session_store,
            session_id: prepared_session_id,
            handle: Some(handle),
        }),
        Ok(Err(error)) => {
            let _ = handle.join();
            Err(io::Error::other(error))
        }
        Err(_) => {
            let _ = handle.join();
            Err(io::Error::other(
                "TUI agent worker stopped before reporting startup readiness",
            ))
        }
    }
}

fn safe_agent_error_stream_event(_error: &str) -> StreamEvent {
    StreamEvent::End("local_error".to_string())
}

fn assign_session_title_from_goal(
    store: &SessionStore,
    session_id: &str,
    goal: &str,
    chrome_generation: &mut u64,
) {
    if let Ok(Some(_)) = maybe_assign_session_display_name(store, session_id, goal) {
        *chrome_generation = chrome_generation.saturating_add(1);
    }
}

fn spawn_tui_agent_worker(
    profile_scope: TuiAgentProfileScope,
    session_id: String,
    goal: String,
    mode: InteractiveAgentMode,
    approval_tx: mpsc::Sender<TuiApprovalRequest>,
    question_tx: mpsc::Sender<TuiQuestionRequest>,
    stream_tx: tokio::sync::mpsc::Sender<SessionStreamEvent>,
) -> io::Result<TuiAgentWorker> {
    prepare_tui_agent_worker(
        profile_scope,
        session_id,
        approval_tx,
        question_tx,
        stream_tx,
    )?
    .start(goal, mode)
}

fn cancel_pending_interactions(
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
) {
    if let Some(request) = pending_approval.take() {
        let _ = request.reply.send(ApprovalDecision::denied());
    }
    if let Some(question) = pending_question.take() {
        let _ = question
            .request
            .reply
            .send(crate::agent::QuestionOutcome::Cancelled);
    }
    while let Ok(request) = approval_rx.try_recv() {
        let _ = request.reply.send(ApprovalDecision::denied());
    }
    while let Ok(request) = question_rx.try_recv() {
        let _ = request.reply.send(crate::agent::QuestionOutcome::Cancelled);
    }
}

fn refresh_pending_interactions(
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
) {
    if pending_approval.is_none() {
        if let Ok(request) = approval_rx.try_recv() {
            *pending_approval = Some(request);
        }
    }
    if pending_question.is_none() {
        if let Ok(request) = question_rx.try_recv() {
            *pending_question = Some(PendingQuestion::new(request));
        }
    }
}

fn drain_stream_events(
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
) {
    while let Ok(event) = stream_rx.try_recv() {
        let reduction = reduce_interaction(
            &InteractionState::default(),
            InteractionInput::SessionRunEvent {
                active_session_id: &timeline.session_id,
                active_run_id: timeline.active_run_id.as_deref(),
                event_session_id: &event.session_id,
                event_run_id: &event.run_id,
            },
        );
        if reduction == InteractionReduction::Consumed(InteractionConsumer::Timeline) {
            timeline.apply_event(event.event);
        }
    }
}

fn shutdown_agent_worker(
    worker: &mut Option<TuiAgentWorker>,
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
) -> io::Result<()> {
    shutdown_agent_worker_with_timeout(
        worker,
        pending_approval,
        pending_question,
        approval_rx,
        question_rx,
        stream_rx,
        timeline,
        AGENT_SHUTDOWN_TIMEOUT,
    )
}

#[allow(clippy::too_many_arguments)]
fn shutdown_agent_worker_with_timeout(
    worker: &mut Option<TuiAgentWorker>,
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
    shutdown_timeout: std::time::Duration,
) -> io::Result<()> {
    let Some(active_worker) = worker.as_ref() else {
        cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
        timeline.active_run_id = None;
        drain_stream_events(stream_rx, timeline);
        return Ok(());
    };
    active_worker.request_cancellation();
    // Approval and question handlers are explicit worker dependencies. Resolve them
    // before waiting so cancellation cannot deadlock behind a modal response.
    cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
    let shutdown_started = std::time::Instant::now();
    while worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
        drain_stream_events(stream_rx, timeline);
        if shutdown_started.elapsed() >= shutdown_timeout {
            // Rust threads cannot be killed safely. Drop the join handle and fail the
            // TUI closed so its outer restoration guard can restore the terminal and
            // process shutdown can terminate the unresponsive worker.
            let compensation = worker
                .as_ref()
                .expect("active worker exists while shutting down")
                .fail_unaccounted_steering("unresponsive_worker_shutdown");
            *worker = None;
            timeline.active_run_id = None;
            drain_stream_events(stream_rx, timeline);
            if let Err(error) = compensation {
                return Err(io::Error::other(format!(
                    "TUI agent worker did not stop within the cancellation deadline; failed to reconcile accepted steering: {error}"
                )));
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TUI agent worker did not stop within the cancellation deadline",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if let Some(active_worker) = worker.as_mut() {
        active_worker.join()?;
    }
    // The worker can publish a modal request after the early cleanup while
    // cancellation is settling. Joining closes that producer before the final drain.
    cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
    drain_stream_events(stream_rx, timeline);
    timeline.active_run_id = None;
    *worker = None;
    Ok(())
}

fn reap_finished_worker(
    worker: &mut Option<TuiAgentWorker>,
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
) -> io::Result<()> {
    if worker.as_ref().is_some_and(TuiAgentWorker::is_finished) {
        cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
        if let Some(worker) = worker.as_mut() {
            worker.join()?;
        }
        *worker = None;
        drain_stream_events(stream_rx, timeline);
        timeline.active_run_id = None;
    }
    Ok(())
}

pub fn run_tui(
    project_root: &Path,
    run_goal: Option<String>,
    requested_session: Option<String>,
    update_notice: Option<String>,
) -> io::Result<()> {
    if let Some(session_id) = requested_session.as_deref() {
        crate::config::load_nib_config_full(project_root)
            .map_err(io::Error::other)?
            .validate_public_session_id(session_id)
            .map_err(io::Error::other)?;
    }
    preflight_tui()?;
    let terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = restore_terminal();
            return Err(io::Error::new(
                error.kind(),
                format!("failed to initialize the full-screen TUI: {error}; use --plain instead"),
            ));
        }
    };
    let mut restore_guard = TerminalRestoreGuard::active();
    if let Err(error) = enable_bracketed_paste() {
        let restoration = restore_guard.restore();
        return Err(match restoration {
            Ok(()) => io::Error::new(
                error.kind(),
                format!("failed to enable bracketed paste: {error}; use --plain instead"),
            ),
            Err(restoration) => io::Error::other(format!(
                "failed to enable bracketed paste: {error}; terminal restoration also failed: {restoration}"
            )),
        });
    }
    let _ = enable_mouse_capture();

    // Terminal ownership is established before session resolution. A terminal startup
    // failure therefore cannot create a session or submit the optional initial goal.
    let result = (|| {
        let profile_scope = crate::interactive::resolve_interactive_profile_scope(project_root)
            .map_err(io::Error::other)?;
        let profile_id = profile_scope.profile_id().to_string();
        let store = profile_scope.into_session_store();
        let resolution =
            resolve_session(&store, requested_session.as_deref()).map_err(io::Error::other)?;
        let active_session_id = resolution.session_id().to_string();
        let session_origin = resolution.tui_origin().to_string();
        let session_notice = resolution.tui_notice();
        let welcome = StartupWelcome::new(project_root, update_notice);
        draw_loop(
            terminal,
            project_root,
            profile_id,
            store,
            run_goal,
            active_session_id,
            session_origin,
            session_notice,
            welcome,
        )
    })();
    let restoration = restore_guard.restore();
    match (result, restoration) {
        (Ok(notice), Ok(())) => {
            if let Some(notice) = notice {
                eprintln!("{notice}");
            }
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(notice), Err(error)) => {
            if let Some(notice) = notice {
                eprintln!("{notice}");
            }
            Err(error)
        }
        (Err(error), Err(restoration)) => Err(io::Error::other(format!(
            "{error}; terminal restoration also failed: {restoration}"
        ))),
    }
}

/// Validate the process-level terminal capabilities required by the full-screen UI.
/// This check is read-only and must run before authentication or session mutation.
pub fn preflight_tui() -> io::Result<()> {
    let term = std::env::var("TERM").ok();
    if let Some(reason) = tui_environment_rejection(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        term.as_deref(),
    ) {
        return Err(io::Error::other(format!("{reason}; use --plain instead")));
    }
    Ok(())
}

/// Return a stable rejection reason for terminal capabilities that cannot host the TUI.
pub fn tui_environment_rejection(
    input_is_terminal: bool,
    output_is_terminal: bool,
    term: Option<&str>,
) -> Option<&'static str> {
    if !input_is_terminal || !output_is_terminal {
        return Some("the full-screen TUI requires terminal input and output");
    }
    if term.is_some_and(|term| term.eq_ignore_ascii_case("dumb")) {
        return Some("TERM=dumb does not support the full-screen TUI");
    }
    None
}

fn enable_bracketed_paste_to(output: &mut impl io::Write) -> io::Result<()> {
    execute!(output, EnableBracketedPaste)
}

fn enable_bracketed_paste() -> io::Result<()> {
    enable_bracketed_paste_to(&mut io::stdout())
}

fn enable_mouse_capture_to(output: &mut impl io::Write) -> io::Result<()> {
    execute!(output, EnableMouseCapture)
}

fn enable_mouse_capture() -> io::Result<()> {
    enable_mouse_capture_to(&mut io::stdout())
}

fn transcript_action_for_key(
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<TranscriptViewportAction> {
    match code {
        KeyCode::PageUp => Some(TranscriptViewportAction::PageUp),
        KeyCode::PageDown => Some(TranscriptViewportAction::PageDown),
        KeyCode::Home if modifiers.contains(KeyModifiers::CONTROL) => {
            Some(TranscriptViewportAction::JumpToStart)
        }
        KeyCode::End if modifiers.contains(KeyModifiers::CONTROL) => {
            Some(TranscriptViewportAction::JumpToEnd)
        }
        KeyCode::Up
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(TranscriptViewportAction::Lines(-1))
        }
        KeyCode::Down
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(TranscriptViewportAction::Lines(1))
        }
        _ => None,
    }
}

fn transcript_action_for_mouse(kind: MouseEventKind) -> Option<TranscriptViewportAction> {
    match kind {
        MouseEventKind::ScrollUp => Some(TranscriptViewportAction::Lines(-3)),
        MouseEventKind::ScrollDown => Some(TranscriptViewportAction::Lines(3)),
        _ => None,
    }
}

fn point_in_rect(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && y >= area.y
        && x < area.x.saturating_add(area.width)
        && y < area.y.saturating_add(area.height)
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn slice_display_cells(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for grapheme in text.graphemes(true) {
        let width = unicode_display_width(grapheme);
        if col >= end {
            break;
        }
        if col >= start {
            out.push_str(grapheme);
        }
        col = col.saturating_add(width);
    }
    out
}

fn extract_pointer_text(rows: &[String], selection: PointerSelection) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let ((start_row, start_col), (end_row, end_col)) = selection.ordered();
    let start_row = start_row.min(rows.len().saturating_sub(1));
    let end_row = end_row.min(rows.len().saturating_sub(1));
    if start_row == end_row {
        let end = end_col.max(start_col.saturating_add(1));
        return slice_display_cells(&rows[start_row], start_col, end);
    }
    let mut parts = Vec::new();
    parts.push(slice_display_cells(&rows[start_row], start_col, usize::MAX));
    parts.extend(
        rows.iter()
            .take(end_row)
            .skip(start_row.saturating_add(1))
            .cloned(),
    );
    parts.push(slice_display_cells(&rows[end_row], 0, end_col.max(1)));
    parts.join("\n")
}

fn last_assistant_copy(activities: &[ActivityEntry]) -> Option<String> {
    activities.iter().rev().find_map(|entry| {
        if entry.kind == ActivityKind::Assistant {
            let text = entry.copy_text();
            (!text.trim().is_empty()).then_some(text)
        } else {
            None
        }
    })
}

fn copy_chat_content(
    pointer: Option<PointerSelection>,
    view: &TranscriptView,
    selected: Option<usize>,
    activities: &[ActivityEntry],
) -> Option<(String, &'static str)> {
    if let Some(selection) = pointer.filter(|selection| selection.moved) {
        let text = extract_pointer_text(&view.plain_rows, selection);
        if !text.trim().is_empty() {
            return Some((text, "Copied selection."));
        }
    }
    if let Some(index) = selected {
        if let Some(entry) = activities.get(index) {
            let text = entry.copy_text();
            if !text.is_empty() {
                return Some((text, "Copied."));
            }
        }
    }
    if let Some(text) = last_assistant_copy(activities) {
        return Some((text, "Copied last reply."));
    }
    let joined = view
        .plain_rows
        .iter()
        .filter(|row| !row.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if joined.trim().is_empty() {
        None
    } else {
        Some((joined, "Copied chat."))
    }
}

fn transcript_hit(view: &TranscriptView, mouse: MouseEvent) -> Option<(usize, usize)> {
    if view.area.width == 0 || view.area.height == 0 || view.plain_rows.is_empty() {
        return None;
    }
    if !point_in_rect(view.area, mouse.column, mouse.row) {
        return None;
    }
    let row = view
        .top_row
        .saturating_add(usize::from(mouse.row.saturating_sub(view.area.y)));
    if row >= view.plain_rows.len() {
        return None;
    }
    let col = usize::from(mouse.column.saturating_sub(view.area.x))
        .min(unicode_display_width(&view.plain_rows[row]));
    Some((row, col))
}

fn restore_terminal_to(
    output: &mut impl io::Write,
    restore_raw_mode: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // Attempt all restorations even if an earlier one fails. In particular,
    // mouse capture and bracketed paste must not remain enabled on a raw-mode
    // or alternate-screen error.
    let mouse_result = execute!(output, DisableMouseCapture);
    let paste_result = execute!(output, DisableBracketedPaste);
    let alternate_result = execute!(output, LeaveAlternateScreen);
    // Windows mouse cleanup restores the input mode it saved after raw mode
    // was enabled. Disable raw mode last so that snapshot cannot re-enable it.
    let raw_result = restore_raw_mode();
    let mut errors = Vec::new();
    if let Err(error) = raw_result {
        errors.push(format!("failed to disable raw mode: {error}"));
    }
    if let Err(error) = mouse_result {
        errors.push(format!("failed to disable mouse capture: {error}"));
    }
    if let Err(error) = paste_result {
        errors.push(format!("failed to disable bracketed paste: {error}"));
    }
    if let Err(error) = alternate_result {
        errors.push(format!("failed to leave alternate screen: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("; ")))
    }
}

fn restore_terminal() -> io::Result<()> {
    restore_terminal_to(&mut io::stdout(), disable_raw_mode)
}

type RestoreTerminalFn = fn() -> io::Result<()>;

struct TerminalRestoreGuard {
    active: bool,
    restore_terminal: RestoreTerminalFn,
}

impl TerminalRestoreGuard {
    fn active() -> Self {
        Self {
            active: true,
            restore_terminal,
        }
    }

    #[cfg(test)]
    fn with_restore(restore_terminal: RestoreTerminalFn) -> Self {
        Self {
            active: true,
            restore_terminal,
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        (self.restore_terminal)()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = (self.restore_terminal)();
            self.active = false;
        }
    }
}

fn composer_height(composer: &Composer, width: u16) -> u16 {
    let visual = composer_visual_rows(composer, width).len().clamp(2, 6);
    u16::try_from(visual).unwrap_or(6)
}

fn composer_cursor_cell(input: &str, cursor: usize, width: u16) -> (u16, u16) {
    let width = usize::from(width.max(1));
    let cursor = cursor.min(input.len());
    let prefix = format!("> {}", &input[..cursor]);
    let rows = wrapped_display_rows(&prefix, width as u16);
    let last_width = rows.last().map_or(0, |row| unicode_display_width(row));
    if last_width >= width {
        return (0, u16::try_from(rows.len()).unwrap_or(u16::MAX));
    }
    (
        u16::try_from(last_width).unwrap_or(width as u16 - 1),
        u16::try_from(rows.len().saturating_sub(1)).unwrap_or(u16::MAX),
    )
}

fn composer_visual_rows(composer: &Composer, width: u16) -> Vec<String> {
    let mut rows = wrapped_display_rows(&format!("> {}", composer.input), width.max(1));
    let (_, cursor_row) = composer_cursor_cell(&composer.input, composer.cursor, width);
    let required = usize::from(cursor_row).saturating_add(1);
    if rows.len() < required {
        rows.resize(required, String::new());
    }
    rows
}

fn overlay_visual_rows(first: &str, extra: &[String], width: u16) -> Vec<String> {
    const MAX_ROWS: usize = 6;
    let width = width.max(1);
    let mut rows = wrapped_display_rows(&format!("> {first}"), width);
    let inner = width.saturating_sub(COMPOSER_PROMPT_CELLS).max(1);
    let extras: Vec<&str> = extra
        .iter()
        .map(String::as_str)
        .filter(|line| !line.is_empty())
        .collect();
    let mut used = 0usize;
    while used < extras.len() {
        let remaining = extras.len() - used;
        let item_rows: Vec<String> = wrapped_display_rows(extras[used], inner)
            .into_iter()
            .map(|wrapped| format!("  {wrapped}"))
            .collect();
        let limit = if remaining > 1 {
            MAX_ROWS.saturating_sub(1)
        } else {
            MAX_ROWS
        };
        if rows.len().saturating_add(item_rows.len()) > limit {
            if used == 0 {
                let room = limit.saturating_sub(rows.len());
                rows.extend(item_rows.into_iter().take(room));
                if remaining > 1 && rows.len() < MAX_ROWS {
                    rows.push(format!("  … {} more", remaining.saturating_sub(1)));
                }
            } else {
                rows.push(format!("  … {remaining} more"));
            }
            break;
        }
        rows.extend(item_rows);
        used += 1;
    }
    if rows.len() < 2 {
        rows.resize(2, String::new());
    }
    rows.truncate(MAX_ROWS);
    rows
}

fn approval_detail_view_rows(details: &[String], width: u16, offset: usize) -> Vec<String> {
    const MAX_ROWS: usize = 6;
    let width = width.max(1);
    let inner = width.saturating_sub(COMPOSER_PROMPT_CELLS).max(1);
    let mut detail_rows = details
        .iter()
        .flat_map(|detail| detail.lines())
        .flat_map(|line| wrapped_display_rows(line, inner))
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>();
    if detail_rows.is_empty() {
        detail_rows.push("  No additional details are available.".to_string());
    }
    let offset = offset.min(detail_rows.len().saturating_sub(1));
    let mut rows = vec!["> Approval details".to_string()];
    let capacity = MAX_ROWS.saturating_sub(rows.len());
    rows.extend(detail_rows.iter().skip(offset).take(capacity).cloned());
    let remaining = detail_rows.len().saturating_sub(offset + capacity);
    if remaining > 0 {
        if let Some(last) = rows.last_mut() {
            *last = format!("  … {remaining} more rows · Down to inspect");
        }
    }
    if rows.len() < 2 {
        rows.resize(2, String::new());
    }
    rows.truncate(MAX_ROWS);
    rows
}

fn waiting_composer_rows(
    waiting: WaitingKind,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
    consent_directory: Option<&str>,
    width: u16,
) -> Option<(Vec<String>, bool)> {
    match waiting {
        WaitingKind::Approval => {
            let req = pending_approval?;
            let prompt = approval_prompt(&req.call, &req.context);
            if req.details_open {
                return Some((
                    approval_detail_view_rows(&req.context.details, width, req.detail_offset),
                    false,
                ));
            }
            let mut extra = if req.call.tool_name == "approve_plan" {
                req.context.details.clone()
            } else {
                Vec::new()
            };
            if extra.is_empty() && !prompt.subject.is_empty() {
                extra.push(prompt.subject);
            }
            if req.call.tool_name != "approve_plan" {
                if let Some(location) =
                    usable_approval_location(prompt.location.clone(), &req.context.target_scope)
                {
                    extra.push(format!("in {location}"));
                }
                let risk = compact_approval_risk(&req.context.permission_and_risk);
                if !risk.is_empty() {
                    extra.push(format!("Risk: {risk}"));
                }
            }
            if let Some(error) = &req.error {
                extra.push(format!("Input error: {error}"));
            }
            Some((overlay_visual_rows(&prompt.statement, &extra, width), false))
        }
        WaitingKind::Question => {
            let question = pending_question?;
            if question.request.options.is_empty() {
                let first = if question.response.is_empty() {
                    question.request.question.as_str()
                } else {
                    question.response.as_str()
                };
                Some((overlay_visual_rows(first, &[], width), true))
            } else {
                Some((
                    overlay_visual_rows(&question.request.question, &[], width),
                    false,
                ))
            }
        }
        WaitingKind::Workspace => {
            let directory = consent_directory?;
            Some((
                overlay_visual_rows("Work in this directory", &[directory.to_string()], width),
                false,
            ))
        }
        WaitingKind::None => None,
    }
}

const CHANNEL_DOT: &str = "● ";
const RESULT_DOT: &str = "· ";

#[derive(Clone, Copy)]
enum ChannelInk {
    User,
    Assistant,
    Thought,
    ToolCall,
    ToolResult,
    System,
    Failure,
}

fn channel_ink(kind: ActivityKind) -> ChannelInk {
    match kind {
        ActivityKind::User => ChannelInk::User,
        ActivityKind::Assistant => ChannelInk::Assistant,
        ActivityKind::Thinking | ActivityKind::Plan => ChannelInk::Thought,
        ActivityKind::Tool | ActivityKind::Approval | ActivityKind::Question => {
            ChannelInk::ToolCall
        }
        ActivityKind::Failure | ActivityKind::Cancellation => ChannelInk::Failure,
        ActivityKind::Compression | ActivityKind::Reconcile | ActivityKind::System => {
            ChannelInk::System
        }
    }
}

fn ink_color(ink: ChannelInk) -> Color {
    match ink {
        ChannelInk::User => Color::Rgb(122, 158, 168),
        ChannelInk::Assistant => Color::Rgb(148, 156, 142),
        ChannelInk::Thought => Color::Rgb(108, 108, 112),
        ChannelInk::ToolCall => Color::Rgb(168, 148, 112),
        ChannelInk::ToolResult => Color::Rgb(118, 122, 130),
        ChannelInk::System => Color::Rgb(128, 132, 128),
        ChannelInk::Failure => Color::Rgb(158, 118, 118),
    }
}

fn ink_style(ink: ChannelInk, no_color: bool) -> Style {
    if no_color {
        match ink {
            ChannelInk::Thought => Style::default().add_modifier(Modifier::ITALIC),
            ChannelInk::Failure => Style::default().add_modifier(Modifier::BOLD),
            _ => Style::default(),
        }
    } else {
        let style = Style::default().fg(ink_color(ink));
        match ink {
            ChannelInk::Thought => style.add_modifier(Modifier::ITALIC),
            _ => style,
        }
    }
}

fn muted_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::Rgb(108, 108, 112))
    }
}

fn role_style(kind: ActivityKind, no_color: bool) -> Style {
    ink_style(channel_ink(kind), no_color)
}

fn speech_body_style(no_color: bool) -> Style {
    if no_color {
        Style::default()
    } else {
        Style::default().fg(Color::Rgb(168, 168, 164))
    }
}

fn thought_body_style(no_color: bool) -> Style {
    ink_style(ChannelInk::Thought, no_color)
}

fn fold_prefix(entry: &ActivityEntry) -> &'static str {
    if entry.kind == ActivityKind::Thinking {
        return "";
    }
    if entry.folded && !entry.body.is_empty() && !tool_is_live(entry) {
        "› "
    } else {
        ""
    }
}

#[derive(Debug, Clone)]
struct TranscriptLive {
    elapsed: Duration,
    tokens: Option<String>,
    tick: u128,
}

fn tool_phase(title: &str) -> &str {
    title
        .split_once(' ')
        .map(|(_, rest)| {
            rest.split_once(" · ")
                .map(|(phase, _)| phase)
                .unwrap_or(rest)
        })
        .unwrap_or("")
}

fn tool_is_live(entry: &ActivityEntry) -> bool {
    entry.kind == ActivityKind::Tool && matches!(tool_phase(&entry.title), "running" | "requested")
}

fn quiet_tool_title(title: &str) -> String {
    let Some((name, rest)) = title.split_once(' ') else {
        return title.to_string();
    };
    let (phase, remainder) = rest.split_once(" · ").unwrap_or((rest, ""));
    match phase {
        "running" | "requested" | "ok" => {
            if remainder.is_empty() {
                name.to_string()
            } else {
                format!("{name}  {remainder}")
            }
        }
        _ => title.to_string(),
    }
}

fn thought_row_title(entry: &ActivityEntry, live: Option<&TranscriptLive>) -> String {
    if let Some(started) = entry.live_since {
        thought_header_title(
            started.elapsed(),
            live.and_then(|value| value.tokens.as_deref()),
        )
    } else if entry.title.starts_with("Thought") {
        entry.title.clone()
    } else if entry.title.is_empty() || is_legacy_thought_title(&entry.title) {
        live.map(|value| thought_header_title(value.elapsed, value.tokens.as_deref()))
            .unwrap_or_else(|| "Thought".to_string())
    } else {
        entry.title.clone()
    }
}

fn header_body_style(kind: ActivityKind, rest: &str, no_color: bool) -> Style {
    if matches!(kind, ActivityKind::Thinking | ActivityKind::Plan) {
        thought_body_style(no_color)
    } else if matches!(kind, ActivityKind::Assistant | ActivityKind::User) {
        speech_body_style(no_color)
    } else if kind == ActivityKind::Tool {
        tool_status_style(kind, rest, no_color)
    } else {
        ink_style(channel_ink(kind), no_color)
    }
}

fn dotted_header_lines(
    kind: ActivityKind,
    rest: &str,
    fold: &str,
    width: u16,
    no_color: bool,
) -> Vec<Line<'static>> {
    let fold_width = unicode_display_width(fold);
    let dot_width = unicode_display_width(CHANNEL_DOT);
    let inner = usize::from(width.max(1))
        .saturating_sub(fold_width)
        .saturating_sub(dot_width)
        .max(1);
    if rest.is_empty() {
        let mut spans = Vec::new();
        if !fold.is_empty() {
            spans.push(Span::styled(fold.to_string(), muted_style(no_color)));
        }
        spans.push(Span::styled(
            CHANNEL_DOT.to_string(),
            ink_style(channel_ink(kind), no_color),
        ));
        return vec![Line::from(spans)];
    }
    wrapped_display_rows(rest, u16::try_from(inner).unwrap_or(u16::MAX))
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            if index == 0 {
                let mut spans = Vec::new();
                if !fold.is_empty() {
                    spans.push(Span::styled(fold.to_string(), muted_style(no_color)));
                }
                spans.push(Span::styled(
                    CHANNEL_DOT.to_string(),
                    ink_style(channel_ink(kind), no_color),
                ));
                spans.push(Span::styled(row, header_body_style(kind, rest, no_color)));
                Line::from(spans)
            } else {
                Line::from(Span::styled(
                    format!("{}{}{row}", fold, " ".repeat(dot_width)),
                    header_body_style(kind, rest, no_color),
                ))
            }
        })
        .collect()
}

fn dotted_result_lines(
    body: &str,
    width: u16,
    ink: ChannelInk,
    no_color: bool,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(1);
    let style = ink_style(ink, no_color);
    body.lines()
        .flat_map(|line| {
            wrapped_display_rows(line, inner)
                .into_iter()
                .map(|row| {
                    Line::from(vec![
                        Span::styled(RESULT_DOT.to_string(), style),
                        Span::styled(row, style),
                    ])
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn visual_channel(kind: ActivityKind) -> u8 {
    match kind {
        ActivityKind::User => 0,
        ActivityKind::Assistant => 1,
        ActivityKind::Thinking | ActivityKind::Plan => 2,
        ActivityKind::Tool | ActivityKind::Approval => 3,
        _ => 4,
    }
}

fn tool_status_style(kind: ActivityKind, rest: &str, no_color: bool) -> Style {
    if kind != ActivityKind::Tool {
        return muted_style(no_color);
    }
    if rest.contains(" failed") {
        ink_style(ChannelInk::Failure, no_color)
    } else if rest.contains(" running") || rest.contains(" requested") {
        ink_style(ChannelInk::ToolCall, no_color)
    } else if rest.contains(" ok") {
        ink_style(ChannelInk::Assistant, no_color)
    } else {
        ink_style(ChannelInk::ToolCall, no_color)
    }
}

fn branch_style(branch: &str, no_color: bool) -> Style {
    if no_color {
        return Style::default().add_modifier(Modifier::BOLD);
    }
    let detached = branch == "-"
        || branch == "HEAD"
        || (branch.len() >= 7 && branch.chars().all(|ch| ch.is_ascii_hexdigit()));
    if detached {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    }
}

fn model_style(no_color: bool) -> Style {
    if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }
}

fn context_style(context: &str, no_color: bool) -> Style {
    if no_color {
        return Style::default();
    }
    let percent = context
        .trim_end_matches('%')
        .trim_start_matches('<')
        .parse::<u16>()
        .unwrap_or(0);
    if percent >= 90 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if percent >= 70 {
        Style::default().fg(Color::Yellow)
    } else {
        muted_style(false)
    }
}

fn header_line(chrome: &TuiChrome, width: u16, no_color: bool) -> Line<'static> {
    let width = usize::from(width.max(1));
    let mut folder = chrome.folder.clone();
    let mut branch = chrome.branch.clone();
    let mut model = chrome.model.clone();
    let context = chrome.context.clone();
    let mut left_width = unicode_display_width(&folder)
        .saturating_add(2)
        .saturating_add(unicode_display_width(&branch));
    let mut right_width = unicode_display_width(&model)
        .saturating_add(2)
        .saturating_add(unicode_display_width(&context));
    if left_width.saturating_add(1).saturating_add(right_width) > width {
        folder = truncate_display_cells(
            &folder,
            22.min(width.saturating_sub(right_width).saturating_sub(3)),
        );
        left_width = unicode_display_width(&folder)
            .saturating_add(2)
            .saturating_add(unicode_display_width(&branch));
    }
    if left_width.saturating_add(1).saturating_add(right_width) > width {
        branch = truncate_display_cells(
            &branch,
            16.min(
                width
                    .saturating_sub(right_width)
                    .saturating_sub(unicode_display_width(&folder).saturating_add(3)),
            ),
        );
        left_width = unicode_display_width(&folder)
            .saturating_add(2)
            .saturating_add(unicode_display_width(&branch));
    }
    if left_width.saturating_add(1).saturating_add(right_width) > width {
        model = truncate_display_cells(
            &model,
            width
                .saturating_sub(left_width)
                .saturating_sub(unicode_display_width(&context).saturating_add(3))
                .max(4),
        );
        right_width = unicode_display_width(&model)
            .saturating_add(2)
            .saturating_add(unicode_display_width(&context));
    }
    let used = left_width.saturating_add(right_width);
    let pad = width.saturating_sub(used);
    let mut spans = vec![
        Span::styled(folder, muted_style(no_color)),
        Span::raw("  "),
        Span::styled(branch, branch_style(&chrome.branch, no_color)),
    ];
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    } else if width > left_width {
        spans.push(Span::raw(" "));
    }
    if width > left_width {
        spans.push(Span::styled(model, model_style(no_color)));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            context,
            context_style(&chrome.context, no_color),
        ));
    }
    Line::from(spans)
}

fn waiting_keys(waiting: WaitingKind) -> &'static str {
    match waiting {
        WaitingKind::Approval => "Enter deny · select Approve once then Enter · Esc deny",
        WaitingKind::Workspace => "Enter decline · select Allow then Enter · Esc decline",
        WaitingKind::Question => "Type answer · Enter submit · Esc leave unanswered",
        WaitingKind::None => "",
    }
}

fn agent_mode_label(
    waiting: WaitingKind,
    worker_mode: Option<InteractiveAgentMode>,
) -> &'static str {
    match waiting {
        WaitingKind::Approval => "WAITING APPROVAL",
        WaitingKind::Question => "WAITING QUESTION",
        WaitingKind::Workspace => "WAITING PERMISSION",
        WaitingKind::None => worker_mode
            .map(InteractiveAgentMode::as_str)
            .unwrap_or("idle"),
    }
}

fn empty_state_lines(welcome: &StartupWelcome, no_color: bool) -> Vec<Line<'static>> {
    let muted = muted_style(no_color);
    let title = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(Span::styled(welcome.version.clone(), title)),
        Line::from(""),
        Line::from(Span::styled("Working directory", muted)),
        Line::from(welcome.working_directory.clone()),
    ];
    if let Some(notice) = &welcome.update_notice {
        let update_style = if no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(notice.clone(), update_style)));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled("Session", title)),
        Line::from(Span::styled(
            "/new            new session and worktree",
            muted,
        )),
        Line::from(Span::styled(
            "/session        switch or resume a session",
            muted,
        )),
        Line::from(""),
        Line::from(Span::styled("Keys", title)),
        Line::from(Span::styled(
            "Enter           send · Shift+Enter newline",
            muted,
        )),
        Line::from(Span::styled(
            "Ctrl+C          stop · clear draft · never copies or quits",
            muted,
        )),
        Line::from(Span::styled(
            "Ctrl+Q          twice to quit · /q also quits",
            muted,
        )),
        Line::from(Span::styled(
            "/               commands · @ files · Tab transcript",
            muted,
        )),
        Line::from(Span::styled("drag            copy chat on release", muted)),
        Line::from(Span::styled("Y / N           approve or deny", muted)),
    ]);
    lines
}

fn footer_line(
    chrome: &TuiChrome,
    viewport: &TranscriptViewport,
    queued: usize,
    waiting: WaitingKind,
    band_hint: Option<&str>,
    selecting: bool,
) -> String {
    let mut hint = format!("approval {} · {}", chrome.approval, chrome.agent_mode);
    if waiting != WaitingKind::None {
        hint = format!("{hint} · {}", waiting_keys(waiting));
    } else if selecting {
        hint = format!("{hint} · Ctrl+Y copy · Esc clear");
    } else if let Some(band) = band_hint {
        hint = format!("{hint} · {band}");
    }
    if queued > 0 {
        hint = format!("queue {queued} · {hint}");
    }
    if !viewport.is_pinned_to_tail() {
        hint.push_str(" · Ctrl+End follow");
    }
    hint
}

fn spinner_glyph(tick: u128, no_color: bool) -> char {
    if no_color {
        SPINNER_ASCII[tick as usize % SPINNER_ASCII.len()]
    } else {
        SPINNER_FRAMES[tick as usize % SPINNER_FRAMES.len()]
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let remain = secs % 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{remain:02}s")
    } else {
        format!("{remain}s")
    }
}

fn live_job_label(activities: &[ActivityEntry], live_state: Option<&str>) -> String {
    for entry in activities.iter().rev() {
        if entry.kind != ActivityKind::Tool {
            continue;
        }
        let (name_phase, hint) = entry
            .title
            .split_once(" · ")
            .unwrap_or((entry.title.as_str(), ""));
        let Some(name) = name_phase
            .strip_suffix(" running")
            .or_else(|| name_phase.strip_suffix(" requested"))
        else {
            continue;
        };
        if hint.is_empty() {
            return name.to_string();
        }
        return format!("{name} · {hint}");
    }
    match live_state {
        Some(state) if !state.is_empty() => state.to_string(),
        _ => "working".to_string(),
    }
}

fn plan_step_label(session: Option<&crate::session::Session>) -> String {
    session
        .and_then(|session| session.plan.as_ref())
        .map(|plan| {
            format!(
                "{}/{}",
                plan.current_step_index.min(plan.steps.len()),
                plan.steps.len()
            )
        })
        .unwrap_or_else(|| "-".to_string())
}

fn approximate_visible_tokens(session: Option<&crate::session::Session>) -> String {
    let Some(session) = session else {
        return "tok -".to_string();
    };
    let bytes = session
        .messages
        .iter()
        .map(|message| message.content.len())
        .sum::<usize>()
        .saturating_add(session.summary.as_deref().map(str::len).unwrap_or(0));
    let tokens = bytes / 4;
    if tokens >= 1000 {
        format!("tok {}k", tokens / 1000)
    } else {
        format!("tok {tokens}")
    }
}

fn meter_status_label(waiting: WaitingKind, lifecycle: &str) -> String {
    match waiting {
        WaitingKind::Approval => "WAITING APPROVAL".to_string(),
        WaitingKind::Question => "WAITING QUESTION".to_string(),
        WaitingKind::Workspace => "WAITING PERMISSION".to_string(),
        WaitingKind::None => lifecycle.to_string(),
    }
}

fn render_waiting_meter(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    meter: &WaitingMeter,
    no_color: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = waiting_meter_text(meter, usize::from(area.width), no_color);
    let style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
}

fn waiting_meter_text(meter: &WaitingMeter, width: usize, no_color: bool) -> String {
    let spin = spinner_glyph(meter.tick, no_color);
    let job = control_safe_text(&meter.job, false);
    let step = truncate_display_cells(&control_safe_text(&meter.step, false), 10);
    let elapsed = truncate_display_cells(&format_elapsed(meter.elapsed), 8);
    let tokens = meter.tokens.strip_prefix("tok ").unwrap_or(&meter.tokens);
    let tokens = truncate_display_cells(&control_safe_text(tokens, false), 8);
    let token_label = format!("tok {tokens}");
    let status = truncate_display_cells(&control_safe_text(&meter.status, false), 16);
    let full = format!(
        "{spin}  {job}  ·  step {step}  ·  {elapsed}  ·  {}  ·  {status}",
        token_label
    );
    if unicode_display_width(&full) <= width {
        return full;
    }

    // Keep every operational field visible on constrained terminals. The job is
    // the only elastic field; labels become compact before any suffix is dropped.
    let suffix = format!("s:{step} {elapsed} t:{tokens} {status}");
    let fixed_width = unicode_display_width(&format!("{spin}  {suffix}"));
    let job_budget = width.saturating_sub(fixed_width.saturating_add(1)).max(1);
    let job = truncate_display_cells(&job, job_budget);
    truncate_display_cells(&format!("{spin} {job} {suffix}"), width)
}

fn copy_text_osc52(text: &str) -> bool {
    let mut out = io::stdout();
    write!(out, "{}", osc52_sequence(text)).is_ok() && out.flush().is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardDelivery {
    Native,
    Osc52Unconfirmed,
    Unavailable,
    Failed,
}

impl ClipboardDelivery {
    fn status(self) -> &'static str {
        match self {
            Self::Native => "Copied",
            Self::Osc52Unconfirmed => "Copy requested via OSC52 (unconfirmed)",
            Self::Unavailable => {
                "Copy unavailable: output is not a terminal; text remains available for manual selection"
            }
            Self::Failed => "Copy failed; text remains available for manual selection",
        }
    }

    fn may_clear_selection(self) -> bool {
        matches!(self, Self::Native | Self::Osc52Unconfirmed)
    }
}

fn deliver_text_to_clipboard(text: &str) -> ClipboardDelivery {
    deliver_text_to_clipboard_with(
        text,
        io::stdout().is_terminal(),
        copy_text_system_clipboard,
        copy_text_osc52,
    )
}

fn deliver_text_to_clipboard_with<Native, Osc52>(
    text: &str,
    is_terminal: bool,
    mut native: Native,
    mut osc52: Osc52,
) -> ClipboardDelivery
where
    Native: FnMut(&str) -> bool,
    Osc52: FnMut(&str) -> bool,
{
    if !is_terminal {
        ClipboardDelivery::Unavailable
    } else if native(text) {
        ClipboardDelivery::Native
    } else if osc52(text) {
        ClipboardDelivery::Osc52Unconfirmed
    } else {
        ClipboardDelivery::Failed
    }
}

pub fn copy_text_to_clipboard(text: &str) -> &'static str {
    deliver_text_to_clipboard(text).status()
}

fn copy_text_system_clipboard(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        return pipe_stdin_to_command(text, "pbcopy", &[]);
    }
    #[cfg(windows)]
    {
        return pipe_stdin_to_command(text, "clip", &[]);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        pipe_stdin_to_command(text, "wl-copy", &[])
            || pipe_stdin_to_command(text, "xclip", &["-selection", "clipboard"])
            || pipe_stdin_to_command(text, "xsel", &["--clipboard", "--input"])
    }
}

fn pipe_stdin_to_command(text: &str, program: &str, args: &[&str]) -> bool {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    matches!(child.wait(), Ok(status) if status.success()) && wrote
}

fn publish_copied_chat(
    pointer: Option<PointerSelection>,
    view: &TranscriptView,
    selected: Option<usize>,
    activities: &[ActivityEntry],
) -> Option<&'static str> {
    let (text, _status) = copy_chat_content(pointer, view, selected, activities)?;
    Some(deliver_text_to_clipboard(&text).status())
}

fn copy_pointer_selection_and_clear(
    pointer_selection: &mut Option<PointerSelection>,
    tui_focus: &mut TuiFocus,
    view: &TranscriptView,
    selected: Option<usize>,
    activities: &[ActivityEntry],
) -> Option<&'static str> {
    copy_pointer_selection_and_clear_with(
        pointer_selection,
        tui_focus,
        view,
        selected,
        activities,
        deliver_text_to_clipboard,
    )
}

fn copy_pointer_selection_and_clear_with<Deliver>(
    pointer_selection: &mut Option<PointerSelection>,
    tui_focus: &mut TuiFocus,
    view: &TranscriptView,
    selected: Option<usize>,
    activities: &[ActivityEntry],
    deliver: Deliver,
) -> Option<&'static str>
where
    Deliver: FnOnce(&str) -> ClipboardDelivery,
{
    let (text, _status) = copy_chat_content(*pointer_selection, view, selected, activities)?;
    let delivery = deliver(&text);
    if delivery.may_clear_selection() {
        *pointer_selection = None;
        *tui_focus = TuiFocus::Composer;
    }
    Some(delivery.status())
}

fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", encode_base64(text.as_bytes()))
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let remaining = bytes.len() - index;
        let b0 = bytes[index];
        let b1 = if remaining > 1 { bytes[index + 1] } else { 0 };
        let b2 = if remaining > 2 { bytes[index + 2] } else { 0 };
        output.push(TABLE[(b0 >> 2) as usize] as char);
        output.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if remaining == 1 {
            output.push('=');
            output.push('=');
        } else {
            output.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
            if remaining == 2 {
                output.push('=');
            } else {
                output.push(TABLE[(b2 & 0x3f) as usize] as char);
            }
        }
        index += 3;
    }
    output
}

struct ApprovalPrompt {
    statement: String,
    subject: String,
    location: Option<String>,
}

fn safe_approval_text(value: &str) -> String {
    control_safe_text(&crate::tools::executor::redact_text(value), true)
}

fn compact_approval_risk(value: &str) -> String {
    value
        .split(" / ")
        .next()
        .unwrap_or(value)
        .trim()
        .to_string()
}

fn approval_prompt(call: &ToolCall, context: &ApprovalContext) -> ApprovalPrompt {
    let statement = match call.tool_name.as_str() {
        "run_terminal" => "Run this command".to_string(),
        "read_file" => "Read this file".to_string(),
        "list_directory" => "List this directory".to_string(),
        "search_replace" => "Edit this file".to_string(),
        "apply_patch" => "Apply a patch".to_string(),
        "grep" => "Search files".to_string(),
        "approve_plan" => "Approve this plan".to_string(),
        "merge_subagent_worktree" => "Merge a subagent worktree".to_string(),
        other => format!("Use {other}"),
    };
    ApprovalPrompt {
        statement,
        subject: context.display_subject.clone(),
        location: context.display_location.clone(),
    }
}

fn usable_approval_location(value: Option<String>, scope: &str) -> Option<String> {
    let skip = |value: &str| {
        value.is_empty()
            || value == "."
            || value.contains("not available")
            || value.contains("not classified")
    };
    if let Some(location) = value.filter(|location| !skip(location)) {
        return Some(location);
    }
    if skip(scope) {
        None
    } else if scope.starts_with('/') || scope.starts_with('.') {
        Some(safe_approval_text(scope))
    } else {
        None
    }
}

fn approval_dock_style(no_color: bool) -> Style {
    let style = Style::default().add_modifier(Modifier::BOLD);
    if no_color {
        style
    } else {
        style.fg(ratatui::style::Color::Yellow)
    }
}

fn tui_report_cancelled_run(
    store: &SessionStore,
    session_id: &str,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    tui_report_reconciled_shutdown(store, session_id, timeline, false)
}

fn tui_report_quit_run(
    store: &SessionStore,
    session_id: &str,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    tui_report_reconciled_shutdown(store, session_id, timeline, true)
}

fn tui_report_reconciled_shutdown(
    store: &SessionStore,
    session_id: &str,
    timeline: &mut ActiveTimeline,
    quitting: bool,
) -> Result<String, String> {
    let (status, action) = match (quitting, timeline.reconciled_terminal) {
        (true, Some(InteractionTerminalOutcome::Cancelled)) => (
            "[cancelled] active agent run reconciled before quit",
            "quit after cancellation",
        ),
        (true, Some(InteractionTerminalOutcome::Completed)) => (
            "[completed] active run completed before quit",
            "quit after completion",
        ),
        (true, Some(InteractionTerminalOutcome::WaitingForInput)) => (
            "[failed] active run still required user input before quit",
            "quit with question input unavailable",
        ),
        (true, Some(InteractionTerminalOutcome::Failed)) => (
            "[failed] active run reconciled with failure before quit",
            "quit after failure",
        ),
        (true, None) => (
            "[failed] active run ended without reconciliation evidence before quit",
            "quit with reconciliation unavailable",
        ),
        (false, Some(InteractionTerminalOutcome::Cancelled)) => {
            ("[cancelled] active agent run", "cancelled")
        }
        (false, Some(InteractionTerminalOutcome::Completed)) => (
            "[completed] active run completed before cancellation",
            "completed before cancellation",
        ),
        (false, Some(InteractionTerminalOutcome::WaitingForInput)) => (
            "[failed] active run still required user input after shutdown",
            "question input unavailable",
        ),
        (false, Some(InteractionTerminalOutcome::Failed)) => {
            ("[failed] active run reconciled with failure", "failed")
        }
        (false, None) => (
            "[failed] active run ended without reconciliation evidence",
            "reconciliation unavailable",
        ),
    };
    let queue = queue_disposition_message(store, session_id, action)?;
    let report = format!("{status}; {queue}");
    timeline.push_status(report.clone());
    Ok(report)
}

fn tui_exit_disposition(store: &SessionStore, session_id: &str) -> Result<String, String> {
    queue_disposition_message(store, session_id, "exited")
}

fn tui_complete_session_switch(
    store: &SessionStore,
    switcher: &SessionSwitcher,
    worker_active: bool,
    active_session_id: &mut String,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    let previous = active_session_id.clone();
    let disposition = queue_disposition_message(store, &previous, "switched sessions")?;
    activate_selected_session(store, switcher, worker_active, active_session_id, timeline)?;
    timeline.push_status(disposition.clone());
    Ok(disposition)
}

#[cfg(test)]
fn parse_role_line(line: &str) -> Option<(ActivityKind, &str)> {
    for kind in [
        ActivityKind::User,
        ActivityKind::Assistant,
        ActivityKind::Thinking,
        ActivityKind::Plan,
        ActivityKind::Tool,
        ActivityKind::Approval,
        ActivityKind::Question,
        ActivityKind::Compression,
        ActivityKind::Reconcile,
        ActivityKind::Cancellation,
        ActivityKind::Failure,
        ActivityKind::System,
    ] {
        let label = if matches!(kind, ActivityKind::Thinking | ActivityKind::Plan) {
            "thought"
        } else {
            kind.role_label()
        };
        if line == label || line == kind.role_label() {
            return Some((kind, ""));
        }
        for prefix in [format!("{label}  "), format!("{}  ", kind.role_label())] {
            if let Some(rest) = line.strip_prefix(&prefix) {
                return Some((kind, rest));
            }
        }
    }
    None
}

#[cfg(test)]
fn activities_from_timeline_text(text: &str) -> Vec<ActivityEntry> {
    let mut entries = Vec::new();
    let mut current: Option<ActivityEntry> = None;
    for line in text.lines() {
        if let Some((kind, rest)) = parse_role_line(line) {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(ActivityEntry::new(kind, rest.to_string(), String::new()));
        } else if line.is_empty() {
            continue;
        } else if let Some(entry) = current.as_mut() {
            if !entry.body.is_empty() {
                entry.body.push('\n');
            }
            entry.body.push_str(line);
        } else {
            entries.push(ActivityEntry::new(
                ActivityKind::System,
                line.to_string(),
                String::new(),
            ));
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

fn apply_selection(mut line: Line<'static>, selected: bool, no_color: bool) -> Line<'static> {
    if selected {
        if no_color {
            line.spans
                .iter_mut()
                .for_each(|span| span.style = span.style.add_modifier(Modifier::REVERSED));
        } else {
            line.spans.iter_mut().for_each(|span| {
                span.style = span.style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
            });
        }
    }
    line
}

fn speech_source(entry: &ActivityEntry) -> String {
    if entry.title.is_empty() || entry.title == "live" {
        entry.body.clone()
    } else if entry.body.is_empty() || entry.folded {
        entry.title.clone()
    } else {
        format!("{}\n{}", entry.title, entry.body)
    }
}

fn prefix_speech_dot(
    mut lines: Vec<Line<'static>>,
    kind: ActivityKind,
    no_color: bool,
) -> Vec<Line<'static>> {
    let dot = Span::styled(
        CHANNEL_DOT.to_string(),
        ink_style(channel_ink(kind), no_color),
    );
    if lines.is_empty() {
        return vec![Line::from(dot)];
    }
    let indent = markdown::speech_indent();
    let first = &mut lines[0];
    if let Some(span) = first.spans.first_mut() {
        if span.content.as_ref() == indent {
            first.spans.remove(0);
        } else if let Some(rest) = span.content.strip_prefix(indent) {
            span.content = rest.to_string().into();
        }
    }
    let rest = std::mem::take(&mut first.spans);
    first.spans.push(dot);
    first.spans.extend(rest);
    lines
}

fn speech_lines(entry: &ActivityEntry, width: u16, no_color: bool) -> Vec<Line<'static>> {
    let source = speech_source(entry);
    if source.is_empty() {
        return dotted_header_lines(entry.kind, "", "", width, no_color);
    }
    let mut lines = prefix_speech_dot(
        markdown::render_markdown(&source, width, markdown::speech_indent(), no_color),
        entry.kind,
        no_color,
    );
    if no_color {
        let label = entry.kind.role_label();
        if let Some(first) = lines.first_mut() {
            let mut spans = vec![Span::raw(format!("{label}  "))];
            spans.append(&mut first.spans);
            *first = Line::from(spans);
        }
    }
    lines
}

fn plan_todo_lines(entry: &ActivityEntry, width: u16, no_color: bool) -> Vec<Line<'static>> {
    let mut lines = dotted_header_lines(entry.kind, &entry.title, "", width, no_color);
    let indent = "  ";
    let inner = width.saturating_sub(2).max(1);
    for line in entry.body.lines() {
        let style = if line.starts_with('◐') {
            ink_style(ChannelInk::ToolCall, no_color)
        } else if line.starts_with('✓') {
            muted_style(no_color)
        } else {
            thought_body_style(no_color)
        };
        for wrapped in wrapped_display_rows(line, inner) {
            lines.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(wrapped, style),
            ]));
        }
    }
    lines
}

fn thought_lines(
    entry: &ActivityEntry,
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
) -> Vec<Line<'static>> {
    let marker = if entry.folded { "▸ " } else { "▾ " };
    let heading = thought_row_title(entry, live);
    let style = thought_body_style(no_color);
    let inner = usize::from(width.max(1))
        .saturating_sub(unicode_display_width(marker))
        .max(1);
    let mut lines: Vec<Line<'static>> =
        wrapped_display_rows(&heading, u16::try_from(inner).unwrap_or(u16::MAX))
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                if index == 0 {
                    Line::from(vec![
                        Span::styled(marker.to_string(), muted_style(no_color)),
                        Span::styled(row, style),
                    ])
                } else {
                    Line::from(Span::styled(
                        format!("{}{row}", " ".repeat(unicode_display_width(marker))),
                        style,
                    ))
                }
            })
            .collect();
    if !entry.folded && !entry.body.is_empty() {
        lines.extend(dotted_result_lines(
            &entry.body,
            width,
            ChannelInk::Thought,
            no_color,
        ));
    }
    lines
}

fn running_tool_verb(title: &str) -> &'static str {
    if title.starts_with("run_terminal") {
        "Running command…"
    } else if title.starts_with("read_file")
        || title.starts_with("list_directory")
        || title.starts_with("grep")
    {
        "Reading…"
    } else {
        "Working…"
    }
}

fn tool_lines(
    entry: &ActivityEntry,
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
) -> Vec<Line<'static>> {
    let rest = quiet_tool_title(&entry.title);
    let mut lines = dotted_header_lines(entry.kind, &rest, fold_prefix(entry), width, no_color);
    if tool_is_live(entry) {
        let spin = spinner_glyph(live.map(|value| value.tick).unwrap_or(0), no_color);
        let verb = running_tool_verb(&entry.title);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{spin}  {verb}"),
                ink_style(ChannelInk::ToolCall, no_color),
            ),
        ]));
    }
    if !entry.folded && !entry.body.is_empty() {
        lines.extend(dotted_result_lines(
            &entry.body,
            width,
            ChannelInk::ToolResult,
            no_color,
        ));
    }
    lines
}

fn log_lines(entry: &ActivityEntry, width: u16, no_color: bool) -> Vec<Line<'static>> {
    let rest = if entry.title.is_empty() {
        entry.body.as_str()
    } else {
        entry.title.as_str()
    };
    let mut lines = dotted_header_lines(entry.kind, rest, fold_prefix(entry), width, no_color);
    if !entry.folded && !entry.title.is_empty() && !entry.body.is_empty() {
        lines.extend(dotted_result_lines(
            &entry.body,
            width,
            channel_ink(entry.kind),
            no_color,
        ));
    }
    lines
}

fn activity_lines(
    entry: &ActivityEntry,
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
) -> Vec<Line<'static>> {
    match entry.kind {
        ActivityKind::User | ActivityKind::Assistant => speech_lines(entry, width, no_color),
        ActivityKind::Thinking => thought_lines(entry, width, no_color, live),
        ActivityKind::Plan => plan_todo_lines(entry, width, no_color),
        ActivityKind::Tool => tool_lines(entry, width, no_color, live),
        _ => log_lines(entry, width, no_color),
    }
}

fn flatten_activity_lines(
    activities: &[ActivityEntry],
    width: u16,
    no_color: bool,
    live: Option<&TranscriptLive>,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut owners = Vec::new();
    let mut previous_channel = None;
    for (index, entry) in activities.iter().enumerate() {
        let wrapped = activity_lines(entry, width, no_color, live);
        if wrapped.is_empty() {
            continue;
        }
        let channel = visual_channel(entry.kind);
        if previous_channel.is_some_and(|previous| previous != channel) {
            rows.push(Line::from(""));
            owners.push(index);
        }
        previous_channel = Some(channel);
        for row in wrapped {
            rows.push(row);
            owners.push(index);
        }
    }
    (rows, owners)
}

fn ensure_selected_visible(viewport: &mut TranscriptViewport, owners: &[usize], selected: usize) {
    let Some(first) = owners.iter().position(|owner| *owner == selected) else {
        return;
    };
    let last = owners
        .iter()
        .rposition(|owner| *owner == selected)
        .unwrap_or(first);
    let page = viewport.page_rows().max(1);
    let top = viewport.top_row();
    let bottom = top.saturating_add(page.saturating_sub(1));
    if first < top {
        viewport.reveal_row(first);
    } else if last > bottom {
        viewport.reveal_row(last.saturating_sub(page.saturating_sub(1)));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceConsentAction {
    Unhandled,
    SelectionChanged,
    Allow,
    Decline,
}

fn workspace_consent_action_for_key(
    selected: &mut usize,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> WorkspaceConsentAction {
    match code {
        KeyCode::Up => {
            *selected = 0;
            return WorkspaceConsentAction::SelectionChanged;
        }
        KeyCode::Down | KeyCode::Tab => {
            *selected = 1;
            return WorkspaceConsentAction::SelectionChanged;
        }
        _ => {}
    }
    let control_quit = matches!(code, KeyCode::Char('c' | 'C' | 'q' | 'Q'))
        && modifiers.contains(KeyModifiers::CONTROL);
    let allow = (matches!(code, KeyCode::Char('y' | 'Y' | '1'))
        || (code == KeyCode::Enter && *selected == 0))
        && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT);
    let decline = control_quit
        || matches!(code, KeyCode::Esc | KeyCode::Char('n' | 'N' | '2'))
        || (code == KeyCode::Enter && *selected != 0);
    if allow {
        WorkspaceConsentAction::Allow
    } else if decline {
        WorkspaceConsentAction::Decline
    } else {
        WorkspaceConsentAction::Unhandled
    }
}

fn pending_workspace_consent(project_root: &Path) -> Option<String> {
    (!crate::config::workspace_access_is_granted(project_root).unwrap_or(false))
        .then(|| crate::interactive::folder_label(project_root))
}

fn grant_workspace_access_and_release_goal(
    project_root: &Path,
    consent_directory: &mut Option<String>,
    pending_goal: &mut Option<String>,
) -> Result<Option<String>, crate::config::ConfigError> {
    crate::config::grant_workspace_access(project_root)?;
    *consent_directory = None;
    Ok(pending_goal.take())
}

#[cfg(test)]
fn render_current_session_view(
    frame: &mut ratatui::Frame<'_>,
    _header: &str,
    _status: &str,
    timeline_text: &str,
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
) {
    let mut viewport = TranscriptViewport::default();
    render_current_session_view_with_viewport(
        frame,
        _header,
        _status,
        timeline_text,
        composer,
        pending_approval,
        pending_question,
        false,
        &mut viewport,
    );
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render_current_session_view_with_viewport(
    frame: &mut ratatui::Frame<'_>,
    _header: &str,
    _status: &str,
    timeline_text: &str,
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
    run_active: bool,
    viewport: &mut TranscriptViewport,
) {
    let activities = activities_from_timeline_text(timeline_text);
    let welcome = StartupWelcome::fixture();
    render_session_activities(
        frame,
        &TuiChrome::fixture(),
        &activities,
        composer,
        pending_approval,
        pending_question,
        run_active,
        viewport,
        TuiFocus::Composer,
        None,
        0,
        None,
        None,
        &welcome,
        None,
        None,
        None,
    );
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render_session_view_with_completion(
    frame: &mut ratatui::Frame<'_>,
    _header: &str,
    _status: &str,
    timeline_text: &str,
    composer: &Composer,
    completion: Option<&CompletionMenu>,
    meter: Option<&WaitingMeter>,
    run_active: bool,
) {
    let mut viewport = TranscriptViewport::default();
    let activities = activities_from_timeline_text(timeline_text);
    let welcome = StartupWelcome::fixture();
    render_session_activities(
        frame,
        &TuiChrome::fixture(),
        &activities,
        composer,
        None,
        None,
        run_active,
        &mut viewport,
        TuiFocus::Composer,
        None,
        0,
        completion,
        meter,
        &welcome,
        None,
        None,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn render_session_activities(
    frame: &mut ratatui::Frame<'_>,
    chrome: &TuiChrome,
    activities: &[ActivityEntry],
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
    _run_active: bool,
    viewport: &mut TranscriptViewport,
    focus: TuiFocus,
    selected: Option<usize>,
    queued: usize,
    completion: Option<&CompletionMenu>,
    meter: Option<&WaitingMeter>,
    welcome: &StartupWelcome,
    band: Option<&InteractionBand<'_>>,
    pointer: Option<PointerSelection>,
    view: Option<&mut TranscriptView>,
) {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let waiting = if pending_approval.is_some() {
        WaitingKind::Approval
    } else if pending_question.is_some() {
        WaitingKind::Question
    } else if welcome.consent_directory.is_some() {
        WaitingKind::Workspace
    } else {
        WaitingKind::None
    };
    let overlay = waiting_composer_rows(
        waiting,
        pending_approval,
        pending_question,
        welcome.consent_directory.as_deref(),
        frame.area().width,
    );
    let composer_h = overlay
        .as_ref()
        .map(|(rows, _)| u16::try_from(rows.len().clamp(2, 6)).unwrap_or(6))
        .unwrap_or_else(|| composer_height(composer, frame.area().width))
        .saturating_add(COMPOSER_BORDER_ROWS);
    let meter_h = u16::from(meter.is_some());
    let completion = completion.filter(|menu| menu.is_open());
    let completion_band = completion.map(InteractionBand::Completion);
    let waiting_band = pending_approval
        .map(InteractionBand::Approval)
        .or_else(|| pending_question.map(InteractionBand::Question))
        .or_else(|| {
            welcome
                .consent_directory
                .as_deref()
                .map(|directory| InteractionBand::Workspace {
                    directory,
                    selected: welcome.consent_selected,
                })
        });
    let band = waiting_band.as_ref().or(band).or(completion_band.as_ref());
    let completion_h = band
        .map(|band| {
            completion_reserved_height(frame.area().height, band.row_count(), composer_h, meter_h)
        })
        .unwrap_or(0);
    let layout = split_session_layout(frame.area(), composer_h, meter_h, completion_h);
    let mut chrome = chrome.clone();
    if waiting != WaitingKind::None {
        chrome.agent_mode = agent_mode_label(waiting, None).to_string();
    }
    frame.render_widget(
        Paragraph::new(header_line(&chrome, layout.header.width, no_color)),
        layout.header,
    );
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1)])
        .split(layout.transcript);
    let empty = activities.is_empty();
    let (rendered_rows, owners) = if empty {
        (
            vec![Line::from(""); empty_state_lines(welcome, no_color).len()],
            Vec::new(),
        )
    } else {
        let live = meter.map(|meter| TranscriptLive {
            elapsed: meter.elapsed,
            tokens: meter_token_label(&meter.tokens),
            tick: meter.tick,
        });
        flatten_activity_lines(activities, body[0].width.max(1), no_color, live.as_ref())
    };
    viewport.observe_layout(rendered_rows.len(), usize::from(body[0].height.max(1)));
    if let Some(selected) = selected {
        ensure_selected_visible(viewport, &owners, selected);
    }
    if let Some(view) = view {
        view.area = body[0];
        view.composer_area = layout.composer;
        view.top_row = viewport.top_row();
        view.plain_rows = rendered_rows.iter().map(line_plain_text).collect();
        view.owners = owners.clone();
    }
    if empty {
        let welcome_lines = empty_state_lines(welcome, no_color);
        let welcome_height = u16::try_from(welcome_lines.len()).unwrap_or(1);
        let welcome_area = Rect {
            x: body[0].x.saturating_add(1),
            y: body[0].y,
            width: body[0].width.saturating_sub(1),
            height: welcome_height.min(body[0].height),
        };
        frame.render_widget(
            Paragraph::new(welcome_lines).alignment(Alignment::Left),
            welcome_area,
        );
    } else {
        let visible_start = viewport.top_row();
        let visible_end = visible_start
            .saturating_add(usize::from(body[0].height.max(1)))
            .min(rendered_rows.len());
        let lines = rendered_rows[visible_start..visible_end]
            .iter()
            .enumerate()
            .map(|(offset, row)| {
                let row_index = visible_start + offset;
                let activity_index = owners.get(row_index).copied();
                let pointer_selected =
                    pointer.is_some_and(|selection| selection.covers_row(row_index));
                let activity_selected = selected.is_some()
                    && activity_index == selected
                    && focus == TuiFocus::Transcript
                    && !pointer.is_some_and(|selection| selection.moved);
                apply_selection(row.clone(), pointer_selected || activity_selected, no_color)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), body[0]);
    }
    if let Some(meter) = meter {
        render_waiting_meter(frame, layout.meter, meter, no_color);
    }
    let composer_area = layout.composer;
    if composer_area.width > 0 && composer_area.height > 0 {
        let border_style = composer_border_style(focus, no_color);
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(border_style);
        let inner = block.inner(composer_area);
        frame.render_widget(block, composer_area);
        if inner.width > 0 && inner.height > 0 {
            let (rows, show_cursor) = overlay
                .as_ref()
                .map(|(rows, editable)| (rows.clone(), *editable))
                .unwrap_or_else(|| (composer_visual_rows(composer, inner.width), true));
            let (cx, cursor_row) = if overlay.is_some() {
                if let Some(question) = pending_question.filter(|question| {
                    waiting == WaitingKind::Question && question.request.options.is_empty()
                }) {
                    composer_cursor_cell(&question.response, question.response.len(), inner.width)
                } else {
                    (COMPOSER_PROMPT_CELLS, 0)
                }
            } else {
                composer_cursor_cell(&composer.input, composer.cursor, inner.width)
            };
            let visible_height = usize::from(inner.height);
            let first_visible = usize::from(cursor_row)
                .saturating_sub(visible_height.saturating_sub(1))
                .min(rows.len().saturating_sub(visible_height));
            let placeholder = overlay.is_none() && composer.input.is_empty();
            let muted_first = overlay.as_ref().is_some_and(|(_, editable)| {
                *editable && pending_question.is_some_and(|question| question.response.is_empty())
            });
            let lines = rows
                .iter()
                .skip(first_visible)
                .take(visible_height)
                .enumerate()
                .map(|(offset, row)| {
                    if first_visible == 0 && offset == 0 {
                        let input = row.strip_prefix("> ").unwrap_or(row);
                        let mut spans =
                            vec![Span::styled("> ", role_style(ActivityKind::User, no_color))];
                        if placeholder {
                            spans.push(Span::styled("Ask nib anything…", muted_style(no_color)));
                        } else if muted_first {
                            spans.push(Span::styled(input.to_string(), muted_style(no_color)));
                        } else if waiting == WaitingKind::Approval
                            || waiting == WaitingKind::Workspace
                        {
                            spans.push(Span::styled(
                                input.to_string(),
                                approval_dock_style(no_color),
                            ));
                        } else {
                            spans.push(Span::styled(
                                input.to_string(),
                                Style::default().add_modifier(Modifier::BOLD),
                            ));
                        }
                        Line::from(spans)
                    } else {
                        Line::from(Span::styled(
                            row.clone(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ))
                    }
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), inner);
            if focus == TuiFocus::Composer && show_cursor {
                let cy = usize::from(cursor_row).saturating_sub(first_visible);
                frame.set_cursor_position(Position {
                    x: inner
                        .x
                        .saturating_add(cx.min(inner.width.saturating_sub(1))),
                    y: inner.y.saturating_add(
                        u16::try_from(cy)
                            .unwrap_or(u16::MAX)
                            .min(inner.height.saturating_sub(1)),
                    ),
                });
            }
        }
    }
    match band {
        Some(InteractionBand::Completion(menu)) => {
            render_completion(frame, layout.completion, menu);
        }
        Some(InteractionBand::Model(model)) => {
            render_model_selection(frame, layout.completion, model);
        }
        Some(InteractionBand::Sessions { switcher, active }) => {
            render_session_switcher(frame, layout.completion, switcher, active);
        }
        Some(InteractionBand::History(search)) => {
            render_history_search(frame, layout.completion, search);
        }
        Some(InteractionBand::Approval(req)) => {
            render_approval_band(frame, layout.completion, req);
        }
        Some(InteractionBand::Question(question)) => {
            render_question_band(frame, layout.completion, question);
        }
        Some(InteractionBand::Workspace {
            directory,
            selected,
        }) => {
            render_workspace_band(frame, layout.completion, directory, *selected);
        }
        None => {}
    }
    let footer = footer_line(
        &chrome,
        viewport,
        queued,
        waiting,
        band.map(InteractionBand::footer_hint),
        pointer_selection_is_active(pointer),
    );
    frame.render_widget(
        Paragraph::new(footer).style(muted_style(no_color)),
        layout.footer,
    );
}

fn composer_border_style(focus: TuiFocus, no_color: bool) -> Style {
    if focus == TuiFocus::Composer {
        if no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            role_style(ActivityKind::User, false)
        }
    } else {
        muted_style(no_color)
    }
}

#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn draw_loop(
    mut terminal: DefaultTerminal,
    project_root: &Path,
    profile_id: String,
    store: SessionStore,
    run_goal: Option<String>,
    mut active_session_id: String,
    mut session_origin: String,
    session_notice: Option<String>,
    mut welcome: StartupWelcome,
) -> io::Result<Option<String>> {
    let agent_profile_scope = TuiAgentProfileScope {
        project_root: project_root.to_path_buf(),
        profile_id: profile_id.clone(),
        sessions_dir: store.sessions_dir().to_path_buf(),
    };
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
    let mut timeline = ActiveTimeline::load(&store, &active_session_id)?;
    if let Some(notice) = session_notice {
        timeline.push_status(notice);
    }
    welcome.consent_directory = pending_workspace_consent(project_root);
    let mut pending_goal = run_goal;
    if let Some(goal) = pending_goal.as_deref() {
        let _ = maybe_assign_session_display_name(&store, &active_session_id, goal);
    }
    let mut worker = if welcome.consent_directory.is_none() {
        if let Some(goal) = pending_goal.take() {
            Some(spawn_tui_agent_worker(
                agent_profile_scope.clone(),
                active_session_id.clone(),
                goal,
                InteractiveAgentMode::Execute,
                approval_tx.clone(),
                question_tx.clone(),
                stream_tx.clone(),
            )?)
        } else {
            None
        }
    } else {
        None
    };
    timeline.bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));

    let mut pending_approval: Option<TuiApprovalRequest> = None;
    let mut pending_question: Option<PendingQuestion> = None;
    let mut command_overlay: Option<String> = None;
    let mut pending_model: Option<PendingModelSelection> = None;
    let mut pending_switcher: Option<SessionSwitcher> = None;
    let mut pending_history_search: Option<PendingHistorySearch> = None;
    let mut composer = Composer::default();
    let mut completion = CompletionMenu::default();
    let mut transcript_viewport = TranscriptViewport::default();
    let mut tui_focus = TuiFocus::Composer;
    let mut selected_activity: Option<usize> = None;
    let mut pointer_selection: Option<PointerSelection> = None;
    let mut transcript_view = TranscriptView::default();
    let mut clear_armed_at: Option<Instant> = None;
    let mut quit_arm: Option<QuitArm> = None;
    let spinner_origin = Instant::now();
    let mut chrome_generation: u64 = 0;
    let mut chrome_cache: Option<ChromeCache> = None;
    let mut exit_requested = false;
    let mut exit_requested_with_active_run = false;

    let loop_result = loop {
        drain_stream_events(&mut stream_rx, &mut timeline);
        let worker_finished = worker.as_ref().is_some_and(TuiAgentWorker::is_finished);
        if let Err(error) = reap_finished_worker(
            &mut worker,
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
            &mut stream_rx,
            &mut timeline,
        ) {
            break Err(error);
        }
        if worker_finished
            && worker.is_none()
            && timeline.reconciled_terminal == Some(InteractionTerminalOutcome::Completed)
        {
            match claim_next_queued_follow_up_after_startup(&store, &active_session_id, |_| {
                prepare_tui_agent_worker(
                    agent_profile_scope.clone(),
                    active_session_id.clone(),
                    approval_tx.clone(),
                    question_tx.clone(),
                    stream_tx.clone(),
                )
                .map_err(|error| error.to_string())
            }) {
                Ok(Some((queued, prepared))) => {
                    match prepared.start(queued.text.clone(), InteractiveAgentMode::Execute) {
                        Ok(next) => {
                            timeline.push_status(format!("[user] {}", queued.text));
                            worker = Some(next);
                            timeline.bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));
                        }
                        Err(error) => {
                            let queue_id = queued.id.clone();
                            let recovery = restore_queued_follow_up_after_start_failure(
                                &store,
                                &active_session_id,
                                queued,
                            );
                            match recovery {
                            Ok(()) => timeline.push_status(format!(
                                "[queue error] queued follow-up {queue_id} could not activate and remains queued: {error}"
                            )),
                            Err(recovery_error) => timeline.push_status(format!(
                                "[queue error] queued follow-up {queue_id} could not activate: {error}; {recovery_error}"
                            )),
                        }
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => timeline.push_status(format!("[queue error] {error}")),
            }
        }
        refresh_pending_interactions(
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
        );

        let interaction_layer = active_interaction_layer(
            pending_approval.is_some(),
            pending_question.is_some(),
            pending_model.is_some(),
            pending_switcher.as_ref(),
            pending_history_search.is_some(),
            completion.is_open(),
        );
        quit_arm = quit_arm_for_consumer(quit_arm, interaction_layer);
        let worker_status = tui_interaction_state(
            pending_approval.is_some(),
            pending_question.is_some(),
            pending_model.is_some(),
            pending_switcher.as_ref(),
            pending_history_search.is_some(),
            completion.is_open(),
            tui_run_state(
                worker.is_some(),
                timeline.live.state.as_deref(),
                timeline.reconciled_terminal,
            ),
        )
        .lifecycle()
        .status_label();
        let session = store.load_result(&timeline.session_id).ok().flatten();
        let queued = session
            .as_ref()
            .map(|session| session.queued_follow_ups.len())
            .unwrap_or(0);
        if let Err(error) = terminal.size() {
            break Err(error);
        }
        let session_revision = session.as_ref().map(|session| session.revision);
        let mut chrome = if chrome_cache.as_ref().is_some_and(|cache| {
            cache.matches(
                chrome_generation,
                &timeline.session_id,
                session_revision,
                &session_origin,
            )
        }) {
            chrome_cache.as_ref().expect("checked").chrome.clone()
        } else {
            let chrome =
                format_tui_interaction_chrome(project_root, session.as_ref(), &timeline.session_id)
                    .unwrap_or_else(TuiChrome::error);
            chrome_cache = Some(ChromeCache {
                generation: chrome_generation,
                session_id: timeline.session_id.clone(),
                session_revision,
                origin: session_origin.clone(),
                chrome: chrome.clone(),
            });
            chrome
        };
        chrome.agent_mode = agent_mode_label(
            if pending_approval.is_some() {
                WaitingKind::Approval
            } else if pending_question.is_some() {
                WaitingKind::Question
            } else if welcome.consent_directory.is_some() {
                WaitingKind::Workspace
            } else {
                WaitingKind::None
            },
            worker.as_ref().map(|worker| worker.mode),
        )
        .to_string();
        if selected_activity.is_some_and(|index| index >= timeline.activities.len()) {
            selected_activity = timeline.activities.len().checked_sub(1);
        }
        let waiting = if pending_approval.is_some() {
            WaitingKind::Approval
        } else if pending_question.is_some() {
            WaitingKind::Question
        } else if welcome.consent_directory.is_some() {
            WaitingKind::Workspace
        } else {
            WaitingKind::None
        };
        let meter = if worker.is_some() || waiting != WaitingKind::None {
            Some(WaitingMeter {
                job: live_job_label(&timeline.activities, timeline.live.state.as_deref()),
                step: plan_step_label(session.as_ref()),
                elapsed: timeline
                    .run_started_at
                    .map(|started| started.elapsed())
                    .unwrap_or_default(),
                tokens: approximate_visible_tokens(session.as_ref()),
                status: meter_status_label(waiting, worker_status),
                tick: spinner_origin.elapsed().as_millis(),
            })
        } else {
            None
        };
        let interaction_band = pending_history_search
            .as_ref()
            .map(InteractionBand::History)
            .or_else(|| pending_model.as_ref().map(InteractionBand::Model))
            .or_else(|| {
                pending_switcher
                    .as_ref()
                    .map(|switcher| InteractionBand::Sessions {
                        switcher,
                        active: &active_session_id,
                    })
            });
        if let Err(error) = terminal.draw(|f| {
            render_session_activities(
                f,
                &chrome,
                &timeline.activities,
                &composer,
                pending_approval.as_ref(),
                pending_question.as_ref(),
                worker.is_some(),
                &mut transcript_viewport,
                tui_focus,
                selected_activity,
                queued,
                Some(&completion).filter(|menu| menu.is_open()),
                meter.as_ref(),
                &welcome,
                interaction_band.as_ref(),
                pointer_selection,
                Some(&mut transcript_view),
            );
            render_interaction_overlay(
                f,
                interaction_layer,
                pending_model.as_ref(),
                pending_switcher.as_ref(),
                pending_history_search.as_ref(),
                &active_session_id,
            );
        }) {
            break Err(error);
        }

        let has_event = match event::poll(std::time::Duration::from_millis(200)) {
            Ok(has_event) => has_event,
            Err(error) => break Err(error),
        };
        if has_event {
            let input = match event::read() {
                Ok(input) => input,
                Err(error) => break Err(error),
            };
            let is_control_q = matches!(
                &input,
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
            );
            refresh_pending_interactions(
                &mut pending_approval,
                &mut pending_question,
                &approval_rx,
                &question_rx,
            );
            let interaction_layer = active_interaction_layer(
                pending_approval.is_some(),
                pending_question.is_some(),
                pending_model.is_some(),
                pending_switcher.as_ref(),
                pending_history_search.is_some(),
                completion.is_open(),
            );
            quit_arm = quit_arm_for_consumer(quit_arm, interaction_layer);
            quit_arm = quit_arm_after_input(quit_arm, is_control_q);
            if let Event::Paste(pasted) = input {
                if welcome.consent_directory.is_some() {
                    continue;
                }
                if let Some(question) = pending_question.as_mut() {
                    paste_question_answer(question, &pasted);
                    continue;
                }
                if matches!(
                    interaction_layer,
                    InteractionLayer::Composer | InteractionLayer::Completion
                ) {
                    let outcome = composer.insert_paste(&pasted);
                    completion.sync_for(&composer.input, Some(project_root));
                    if let Some(status) = outcome.visible_status() {
                        timeline.push_status(status);
                    }
                } else {
                    timeline.push_status(
                        "[composer] paste ignored while a modal response is required".to_string(),
                    );
                }
                continue;
            }
            if let Event::Mouse(mouse) = input {
                if let Some(action) = transcript_action_for_mouse(mouse.kind) {
                    transcript_viewport.apply(action);
                    continue;
                }
                if let Some((row, col)) = transcript_hit(&transcript_view, mouse) {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            tui_focus = TuiFocus::Transcript;
                            pointer_selection = Some(PointerSelection::at(row, col));
                            selected_activity = transcript_view.owners.get(row).copied();
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            tui_focus = TuiFocus::Transcript;
                            if let Some(selection) = pointer_selection.as_mut() {
                                selection.drag_to(row, col);
                            } else {
                                pointer_selection = Some(PointerSelection::at(row, col));
                            }
                            selected_activity = transcript_view.owners.get(row).copied();
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            if let Some(selection) = pointer_selection.as_mut() {
                                selection.drag_to(row, col);
                            }
                            if pointer_selection_is_active(pointer_selection) {
                                if let Some(status) = copy_pointer_selection_and_clear(
                                    &mut pointer_selection,
                                    &mut tui_focus,
                                    &transcript_view,
                                    selected_activity,
                                    &timeline.activities,
                                ) {
                                    timeline.push_status(status.to_string());
                                }
                            } else {
                                tui_focus = TuiFocus::Transcript;
                                pointer_selection = None;
                                selected_activity = transcript_view.owners.get(row).copied();
                            }
                        }
                        _ => {}
                    }
                } else if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    tui_focus = TuiFocus::Composer;
                    pointer_selection = None;
                } else if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left))
                    && pointer_selection_is_active(pointer_selection)
                {
                    if let Some(status) = copy_pointer_selection_and_clear(
                        &mut pointer_selection,
                        &mut tui_focus,
                        &transcript_view,
                        selected_activity,
                        &timeline.activities,
                    ) {
                        timeline.push_status(status.to_string());
                    }
                }
                continue;
            }
            if let Event::Key(key) = input {
                if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                    continue;
                }
                let control_c = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SHIFT);
                let control_shift_c = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT);
                let control_q = matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                if welcome.consent_directory.is_some() {
                    if let Some(action) = transcript_action_for_key(key.code, key.modifiers) {
                        transcript_viewport.apply(action);
                        continue;
                    }
                    match workspace_consent_action_for_key(
                        &mut welcome.consent_selected,
                        key.code,
                        key.modifiers,
                    ) {
                        WorkspaceConsentAction::SelectionChanged => continue,
                        WorkspaceConsentAction::Allow => {
                            match grant_workspace_access_and_release_goal(
                                project_root,
                                &mut welcome.consent_directory,
                                &mut pending_goal,
                            ) {
                                Ok(released_goal) => {
                                    timeline
                                        .push_status("Allowed work in this directory.".to_string());
                                    if let Some(goal) = released_goal {
                                        match spawn_tui_agent_worker(
                                            agent_profile_scope.clone(),
                                            active_session_id.clone(),
                                            goal,
                                            InteractiveAgentMode::Execute,
                                            approval_tx.clone(),
                                            question_tx.clone(),
                                            stream_tx.clone(),
                                        ) {
                                            Ok(next) => {
                                                timeline.bind_run(Some(next.run_id.clone()));
                                                worker = Some(next);
                                            }
                                            Err(error) => break Err(error),
                                        }
                                    }
                                }
                                Err(error) => {
                                    timeline.push_status(format!("[workspace error] {error}"))
                                }
                            }
                            continue;
                        }
                        WorkspaceConsentAction::Decline => {
                            exit_requested = true;
                            break Ok(());
                        }
                        WorkspaceConsentAction::Unhandled => continue,
                    }
                }
                let interaction_state = tui_interaction_state(
                    pending_approval.is_some(),
                    pending_question.is_some(),
                    pending_model.is_some(),
                    pending_switcher.as_ref(),
                    pending_history_search.is_some(),
                    completion.is_open(),
                    tui_run_state(
                        worker.is_some(),
                        timeline.live.state.as_deref(),
                        timeline.reconciled_terminal,
                    ),
                );
                let global_reduction = if control_c && worker.is_some() {
                    Some(reduce_interaction(
                        &interaction_state,
                        InteractionInput::CancelRun,
                    ))
                } else {
                    None
                };
                let copy_requested = (matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
                    && key.modifiers.contains(KeyModifiers::CONTROL))
                    || control_shift_c;
                if copy_requested {
                    if let Some(status) = publish_copied_chat(
                        pointer_selection,
                        &transcript_view,
                        selected_activity,
                        &timeline.activities,
                    ) {
                        timeline.push_status(status.to_string());
                    }
                    continue;
                }
                if global_reduction == Some(InteractionReduction::CancelRun) {
                    if let Err(error) = shutdown_agent_worker(
                        &mut worker,
                        &mut pending_approval,
                        &mut pending_question,
                        &approval_rx,
                        &question_rx,
                        &mut stream_rx,
                        &mut timeline,
                    ) {
                        break Err(error);
                    }
                    match tui_report_cancelled_run(&store, &active_session_id, &mut timeline) {
                        Ok(_) => {}
                        Err(error) => timeline.push_status(format!("[queue error] {error}")),
                    }
                    continue;
                }
                if control_c {
                    pointer_selection = None;
                    composer.set_text(String::new());
                    completion.sync_for(&composer.input, Some(project_root));
                    quit_arm = None;
                    if let Some(question) = pending_question.as_mut() {
                        question.response.clear();
                        question.error = None;
                    }
                    timeline.push_status("Draft cleared.".to_string());
                    continue;
                }
                if control_q {
                    let now = Instant::now();
                    match quit_confirm_action(quit_arm, now, interaction_layer) {
                        QuitConfirmAction::Confirm => {
                            exit_requested_with_active_run = worker.is_some();
                            exit_requested = true;
                            break Ok(());
                        }
                        QuitConfirmAction::Arm => {
                            quit_arm = Some(QuitArm {
                                armed_at: now,
                                consumer: interaction_layer,
                            });
                            timeline.push_status("Press Ctrl+Q again to quit.".to_string());
                        }
                    }
                    continue;
                }
                if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && !transcript_view.plain_rows.is_empty()
                {
                    let last = transcript_view.plain_rows.len().saturating_sub(1);
                    pointer_selection = Some(PointerSelection {
                        anchor_row: 0,
                        anchor_col: 0,
                        focus_row: last,
                        focus_col: unicode_display_width(&transcript_view.plain_rows[last]),
                        moved: true,
                    });
                    tui_focus = TuiFocus::Transcript;
                    timeline.push_status("Chat selected.".to_string());
                    continue;
                }
                if let Some(action) = transcript_action_for_key(key.code, key.modifiers) {
                    transcript_viewport.apply(action);
                    continue;
                }
                let semantic_reduction = match key.code {
                    KeyCode::Char('r') | KeyCode::Char('R')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        Some(reduce_interaction(
                            &interaction_state,
                            InteractionInput::OpenHistorySearch,
                        ))
                    }
                    _ => None,
                };
                if let Some(InteractionReduction::OpenHistorySearch { query }) = semantic_reduction
                {
                    pending_history_search =
                        Some(PendingHistorySearch::new(&composer.history, query));
                    continue;
                }
                let interaction_layer = active_interaction_layer(
                    pending_approval.is_some(),
                    pending_question.is_some(),
                    pending_model.is_some(),
                    pending_switcher.as_ref(),
                    pending_history_search.is_some(),
                    completion.is_open(),
                );
                if open_prompt_command_overlay(
                    key.code,
                    pending_approval.is_some() || pending_question.is_some(),
                    &mut command_overlay,
                ) {
                    timeline
                        .push_status("Command (F2): type a slash command · Esc cancel".to_string());
                    continue;
                }
                if let Some(buffer) = command_overlay.as_mut() {
                    match key.code {
                        KeyCode::Esc => {
                            command_overlay = None;
                            timeline.push_status("Command entry cancelled.".to_string());
                        }
                        KeyCode::Backspace => {
                            buffer.pop();
                        }
                        KeyCode::Enter => {
                            let raw = command_overlay.take().unwrap_or_default();
                            let command_line = if raw.trim().starts_with('/') {
                                raw
                            } else {
                                format!("/{}", raw.trim())
                            };
                            match crate::interactive::parse_interactive_command(&command_line) {
                                Ok(Some(command))
                                    if matches!(
                                        crate::interactive::command_effect_class(&command),
                                        crate::interactive::CommandEffectClass::ReadOnlyInspection
                                            | crate::interactive::CommandEffectClass::LiveControl
                                    ) =>
                                {
                                    let copy_command = matches!(
                                        &command,
                                        crate::interactive::InteractiveCommand::Copy
                                    );
                                    match execute_interactive_command_in_state(
                                        command,
                                        project_root,
                                        &profile_id,
                                        &store,
                                        &active_session_id,
                                        worker_status,
                                    ) {
                                        Ok(InteractiveEffect::Output(output)) => timeline
                                            .push_status(if copy_command {
                                                copy_text_to_clipboard(&output).to_string()
                                            } else {
                                                output
                                            }),
                                        Ok(_) => timeline.push_status(
                                            "command completed without changing the pending prompt"
                                                .to_string(),
                                        ),
                                        Err(error) => {
                                            timeline.push_status(format!("[command error] {error}"))
                                        }
                                    }
                                }
                                Ok(Some(crate::interactive::InteractiveCommand::Stop {
                                    task_id: None,
                                })) => timeline.push_status(
                                    crate::interactive::live_stop_requires_id_message().to_string(),
                                ),
                                Ok(Some(command)) => timeline.push_status(format!(
                                    "command /{} is unavailable while a prompt is pending",
                                    command.spec().name
                                )),
                                Ok(None) => {
                                    timeline.push_status("F2 requires a slash command".to_string())
                                }
                                Err(error) => {
                                    timeline.push_status(format!("[command error] {error}"))
                                }
                            }
                        }
                        KeyCode::Char(character) if !character.is_control() => {
                            buffer.push(character);
                        }
                        _ => {}
                    }
                    continue;
                }
                if matches!(
                    interaction_layer,
                    InteractionLayer::Approval | InteractionLayer::Question
                ) {
                    handle_pending_interaction_key(
                        &mut pending_approval,
                        &mut pending_question,
                        key.code,
                    );
                    continue;
                }
                if interaction_layer == InteractionLayer::HistorySearch {
                    let Some(search) = pending_history_search.as_mut() else {
                        timeline.push_status(
                            "[ui error] draft history state was unavailable; returned to composer"
                                .to_string(),
                        );
                        continue;
                    };
                    match history_search_action_for_key(
                        search,
                        &composer.history,
                        key.code,
                        key.modifiers,
                    ) {
                        HistorySearchAction::Pending => {}
                        HistorySearchAction::Close => pending_history_search = None,
                        HistorySearchAction::Select(index) => {
                            pending_history_search = None;
                            if composer.select_history_entry(index) {
                                completion.sync_for(&composer.input, Some(project_root));
                            } else {
                                timeline.push_status(
                                    "[history error] selected draft is no longer available"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    continue;
                }
                if interaction_layer == InteractionLayer::Model {
                    let Some(model) = pending_model.as_mut() else {
                        timeline.push_status(
                            "[ui error] model selector state was unavailable; returned to composer"
                                .to_string(),
                        );
                        continue;
                    };
                    match model_action_for_key(model, key.code) {
                        ModelAction::Pending => {}
                        ModelAction::Cancel => {
                            pending_model = None;
                            timeline.push_status("Model selection cancelled.".to_string());
                        }
                        ModelAction::Submit(selected) => {
                            pending_model = None;
                            match set_active_model(project_root, &selected) {
                                Ok(output) => timeline.push_status(output),
                                Err(error) => {
                                    timeline.push_status(format!("[command error] {error}"))
                                }
                            }
                        }
                    }
                    continue;
                }
                if matches!(
                    interaction_layer,
                    InteractionLayer::SessionConfirmation | InteractionLayer::SessionSwitcher
                ) {
                    let Some(switcher) = pending_switcher.as_mut() else {
                        timeline.push_status(
                            "[ui error] session selector state was unavailable; returned to composer"
                                .to_string(),
                        );
                        continue;
                    };
                    match session_switcher_action_for_key(switcher, key.code) {
                        SwitcherAction::Pending => {}
                        SwitcherAction::Close => {
                            pending_switcher = None;
                            timeline.push_status("Session switch cancelled.".to_string());
                        }
                        SwitcherAction::PreviewExact(session_id) => {
                            if let Err(error) = preview_exact_session(
                                &store,
                                switcher,
                                &active_session_id,
                                &session_id,
                            ) {
                                switcher.error = Some(error);
                            }
                        }
                        SwitcherAction::Activate => {
                            match tui_complete_session_switch(
                                &store,
                                switcher,
                                worker.is_some(),
                                &mut active_session_id,
                                &mut timeline,
                            ) {
                                Ok(_) => {
                                    session_origin = "resumed".to_string();
                                    completion = CompletionMenu::default();
                                    pending_switcher = None;
                                    chrome_generation = chrome_generation.saturating_add(1);
                                    transcript_viewport.pin_to_tail();
                                }
                                Err(error) => {
                                    switcher.confirming = false;
                                    switcher.error = Some(error);
                                }
                            }
                        }
                    }
                    continue;
                }
                if interaction_layer == InteractionLayer::RecoverableError {
                    timeline.push_status(
                        "[ui error] interaction state was unavailable; input ignored".to_string(),
                    );
                    continue;
                }
                if interaction_layer == InteractionLayer::Completion {
                    match completion.handle_key(&mut composer, key.code) {
                        CompletionKeyResult::Ignored => {
                            match composer_action_for_key(&mut composer, key.code, key.modifiers) {
                                ComposerAction::Pending => {
                                    completion.sync_for(&composer.input, Some(project_root));
                                }
                                ComposerAction::Submit(submitted) => {
                                    composer.set_text(submitted);
                                    timeline.push_status(
                                        "[ui error] completion input could not be submitted"
                                            .to_string(),
                                    );
                                }
                            }
                            continue;
                        }
                        CompletionKeyResult::Consumed => continue,
                        CompletionKeyResult::Submit => {
                            tui_focus = TuiFocus::Composer;
                        }
                    }
                }
                if key.code == KeyCode::Tab
                    && !key.modifiers.contains(KeyModifiers::SHIFT)
                    && matches!(
                        interaction_layer,
                        InteractionLayer::Composer | InteractionLayer::Completion
                    )
                    && !completion.is_open()
                {
                    match tui_focus {
                        TuiFocus::Composer => {
                            tui_focus = TuiFocus::Transcript;
                            selected_activity = timeline.activities.len().checked_sub(1);
                        }
                        TuiFocus::Transcript => {
                            tui_focus = TuiFocus::Composer;
                            pointer_selection = None;
                        }
                    }
                    continue;
                }
                if tui_focus == TuiFocus::Transcript
                    && matches!(
                        interaction_layer,
                        InteractionLayer::Composer | InteractionLayer::Completion
                    )
                {
                    match key.code {
                        KeyCode::Up => {
                            let len = timeline.activities.len();
                            if len > 0 {
                                selected_activity =
                                    Some(selected_activity.unwrap_or(len - 1).saturating_sub(1));
                            }
                            continue;
                        }
                        KeyCode::Down => {
                            let len = timeline.activities.len();
                            if len > 0 {
                                let current = selected_activity.unwrap_or(len - 1);
                                selected_activity = Some((current + 1).min(len - 1));
                            }
                            continue;
                        }
                        KeyCode::Left | KeyCode::Right => {
                            if let Some(index) = selected_activity {
                                if let Some(entry) = timeline.activities.get_mut(index) {
                                    if !entry.body.is_empty() {
                                        entry.folded = !entry.folded;
                                    }
                                }
                            }
                            continue;
                        }
                        KeyCode::Enter | KeyCode::Esc => {
                            tui_focus = TuiFocus::Composer;
                            pointer_selection = None;
                            continue;
                        }
                        KeyCode::Char(character)
                            if !key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            tui_focus = TuiFocus::Composer;
                            pointer_selection = None;
                            composer.insert_str(&character.to_string());
                            completion.sync_for(&composer.input, Some(project_root));
                            continue;
                        }
                        _ => {}
                    }
                }
                if key.code == KeyCode::Esc
                    && pointer_selection_is_active(pointer_selection)
                    && matches!(interaction_layer, InteractionLayer::Composer)
                {
                    pointer_selection = None;
                    tui_focus = TuiFocus::Composer;
                    continue;
                }
                if key.code == KeyCode::Esc
                    && tui_focus == TuiFocus::Composer
                    && matches!(interaction_layer, InteractionLayer::Composer)
                {
                    if composer.input.is_empty() {
                        continue;
                    }
                    let now = Instant::now();
                    if clear_armed_at
                        .is_some_and(|armed| now.duration_since(armed) <= CLEAR_DRAFT_CONFIRM)
                    {
                        composer.set_text(String::new());
                        completion.sync_for(&composer.input, Some(project_root));
                        clear_armed_at = None;
                        timeline.push_status("Draft cleared.".to_string());
                    } else {
                        clear_armed_at = Some(now);
                        timeline.push_status("Press Esc again to clear.".to_string());
                    }
                    continue;
                }
                if matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    match submit_tui_steering_draft(worker.as_ref(), &mut composer) {
                        Ok((text, sequence)) => {
                            completion = CompletionMenu::default();
                            transcript_viewport.on_submission();
                            timeline.push_steering(&text, sequence);
                        }
                        Err(error) => {
                            timeline.push_status(format!("[steer error] {error}"));
                        }
                    }
                    continue;
                }
                match composer_action_for_key(&mut composer, key.code, key.modifiers) {
                    ComposerAction::Pending => {
                        completion.sync_for(&composer.input, Some(project_root))
                    }
                    ComposerAction::Submit(submitted) => {
                        transcript_viewport.on_submission();
                        completion = CompletionMenu::default();
                        let state = tui_interaction_state(
                            false,
                            false,
                            false,
                            None,
                            false,
                            false,
                            tui_run_state(
                                worker.is_some(),
                                timeline.live.state.as_deref(),
                                timeline.reconciled_terminal,
                            ),
                        );
                        match reduce_interaction(
                            &state,
                            InteractionInput::ComposerSubmit(&submitted),
                        ) {
                            InteractionReduction::NoOp(_) => {}
                            InteractionReduction::Error { message, .. } => {
                                composer.set_text(submitted);
                                completion.sync_for(&composer.input, Some(project_root));
                                timeline.push_status(format!("[command error] {message}"));
                            }
                            InteractionReduction::SteerCurrent(text) => {
                                composer.set_text(submitted);
                                completion.sync_for(&composer.input, Some(project_root));
                                timeline.push_status(format!(
                                    "[ui error] Enter cannot steer an active run: {text}"
                                ));
                            }
                            InteractionReduction::QueueNext(queued) => {
                                match persist_queued_follow_up(
                                    &store,
                                    &active_session_id,
                                    &queued,
                                    "composer",
                                ) {
                                    Ok(_) => timeline.push_status(format!(
                                        "queued follow-up retained on session {active_session_id}"
                                    )),
                                    Err(error) => {
                                        composer.set_text(submitted);
                                        completion.sync_for(&composer.input, Some(project_root));
                                        timeline.push_status(format!("[queue error] {error}"));
                                    }
                                }
                            }
                            InteractionReduction::OpenHistorySearch { query } => {
                                composer.history.discard_latest_if(&submitted);
                                pending_history_search =
                                    Some(PendingHistorySearch::new(&composer.history, query));
                            }
                            InteractionReduction::Command(command) => {
                                let copy_command = matches!(
                                    &command,
                                    crate::interactive::InteractiveCommand::Copy
                                );
                                let changed_origin = match &command {
                                    crate::interactive::InteractiveCommand::New
                                    | crate::interactive::InteractiveCommand::Clear => Some("new"),
                                    crate::interactive::InteractiveCommand::Fork => Some("fork"),
                                    _ => None,
                                };
                                if matches!(
                                    command,
                                    crate::interactive::InteractiveCommand::Session
                                        | crate::interactive::InteractiveCommand::Resume
                                ) {
                                    match load_session_switcher(&store, &active_session_id) {
                                        Ok(switcher) => pending_switcher = Some(switcher),
                                        Err(error) => {
                                            composer.set_text(submitted);
                                            completion
                                                .sync_for(&composer.input, Some(project_root));
                                            timeline.push_status(format!(
                                                "[command error] could not open session switcher: {error}"
                                            ));
                                        }
                                    }
                                    continue;
                                }
                                match execute_interactive_command_in_state(
                                    command,
                                    project_root,
                                    &profile_id,
                                    &store,
                                    &active_session_id,
                                    worker_status,
                                ) {
                                    Ok(InteractiveEffect::Quit) => {
                                        exit_requested_with_active_run = worker.is_some();
                                        exit_requested = true;
                                        break Ok(());
                                    }
                                    Ok(InteractiveEffect::Output(output)) => {
                                        chrome_generation = chrome_generation.saturating_add(1);
                                        timeline.push_status(if copy_command {
                                            copy_text_to_clipboard(&output).to_string()
                                        } else {
                                            output
                                        })
                                    }
                                    Ok(InteractiveEffect::SessionChanged {
                                        session_id,
                                        output,
                                    }) => {
                                        if let Err(error) = replace_active_session(
                                            &store,
                                            session_id,
                                            output,
                                            &mut active_session_id,
                                            &mut timeline,
                                        ) {
                                            timeline.push_status(format!(
                                            "[command error] {error}; the active session is unchanged"
                                            ));
                                        } else {
                                            if let Some(origin) = changed_origin {
                                                session_origin = origin.to_string();
                                            }
                                            chrome_generation = chrome_generation.saturating_add(1);
                                            transcript_viewport.pin_to_tail();
                                        }
                                    }
                                    Ok(InteractiveEffect::SelectSession(selection)) => {
                                        pending_switcher = Some(SessionSwitcher::from_selection(
                                            selection,
                                            &active_session_id,
                                        ));
                                    }
                                    Ok(InteractiveEffect::SelectModel(selection)) => {
                                        pending_model = Some(PendingModelSelection::new(selection));
                                    }
                                    Ok(InteractiveEffect::Compact) => {
                                        timeline.push_status("[compact] requested".to_string());
                                        worker = Some(spawn_tui_agent_worker(
                                            agent_profile_scope.clone(),
                                            active_session_id.clone(),
                                            String::new(),
                                            InteractiveAgentMode::Compact,
                                            approval_tx.clone(),
                                            question_tx.clone(),
                                            stream_tx.clone(),
                                        )?);
                                        timeline.bind_run(
                                            worker.as_ref().map(|worker| worker.run_id.clone()),
                                        );
                                    }
                                    Ok(InteractiveEffect::ContinuePlan { plan_id, goal }) => {
                                        timeline.push_status(format!("Continue plan {plan_id}"));
                                        worker = Some(
                                            prepare_tui_agent_worker(
                                                agent_profile_scope.clone(),
                                                active_session_id.clone(),
                                                approval_tx.clone(),
                                                question_tx.clone(),
                                                stream_tx.clone(),
                                            )?
                                            .start_with_continuation(
                                                goal,
                                                InteractiveAgentMode::Execute,
                                                Some(plan_id),
                                            )?,
                                        );
                                        timeline.bind_run(
                                            worker.as_ref().map(|worker| worker.run_id.clone()),
                                        );
                                    }
                                    Ok(InteractiveEffect::OpenQuestion {
                                        invocation_id,
                                        question,
                                        options,
                                    }) => {
                                        let (reply_tx, reply_rx) = oneshot::channel();
                                        drop(reply_rx);
                                        pending_question = Some(PendingQuestion::recovered(
                                            TuiQuestionRequest {
                                                question,
                                                options,
                                                reply: reply_tx,
                                            },
                                            RecoveredQuestionTarget {
                                                store: store.clone(),
                                                session_id: active_session_id.clone(),
                                                invocation_id: invocation_id.clone(),
                                            },
                                        ));
                                        timeline.push_status(format!(
                                            "Answer recovered question {invocation_id}"
                                        ));
                                    }
                                    Ok(InteractiveEffect::RunAgent { goal, mode }) => {
                                        if mode != InteractiveAgentMode::Compact {
                                            assign_session_title_from_goal(
                                                &store,
                                                &active_session_id,
                                                &goal,
                                                &mut chrome_generation,
                                            );
                                        }
                                        timeline.push_status(format!("[user] {goal}"));
                                        worker = Some(spawn_tui_agent_worker(
                                            agent_profile_scope.clone(),
                                            active_session_id.clone(),
                                            goal,
                                            mode,
                                            approval_tx.clone(),
                                            question_tx.clone(),
                                            stream_tx.clone(),
                                        )?);
                                        timeline.bind_run(
                                            worker.as_ref().map(|worker| worker.run_id.clone()),
                                        );
                                    }
                                    Err(error) => {
                                        composer.set_text(submitted);
                                        completion.sync_for(&composer.input, Some(project_root));
                                        timeline.push_status(format!("[command error] {error}"))
                                    }
                                }
                            }
                            InteractionReduction::IdleTurn(goal) => {
                                assign_session_title_from_goal(
                                    &store,
                                    &active_session_id,
                                    &goal,
                                    &mut chrome_generation,
                                );
                                timeline.push_status(format!("[user] {goal}"));
                                worker = Some(spawn_tui_agent_worker(
                                    agent_profile_scope.clone(),
                                    active_session_id.clone(),
                                    goal,
                                    InteractiveAgentMode::Execute,
                                    approval_tx.clone(),
                                    question_tx.clone(),
                                    stream_tx.clone(),
                                )?);
                                timeline
                                    .bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));
                            }
                            InteractionReduction::Consumed(_)
                            | InteractionReduction::ModalCommand(_)
                            | InteractionReduction::ApprovalDecision(_)
                            | InteractionReduction::ApprovalInputClosed
                            | InteractionReduction::QuestionAnswered(_)
                            | InteractionReduction::QuestionLeftUnanswered
                            | InteractionReduction::QuestionInputClosed
                            | InteractionReduction::ConfirmationDecision(_)
                            | InteractionReduction::ConfirmationInputClosed
                            | InteractionReduction::Reconciled { .. }
                            | InteractionReduction::Transcript(_)
                            | InteractionReduction::CancelRun
                            | InteractionReduction::Quit
                            | InteractionReduction::StaleEvent => {
                                timeline.push_status(
                                    "[ui error] submitted input had no valid consumer".to_string(),
                                );
                            }
                        }
                    }
                }
            }
        }
    };
    let shutdown = shutdown_agent_worker(
        &mut worker,
        &mut pending_approval,
        &mut pending_question,
        &approval_rx,
        &question_rx,
        &mut stream_rx,
        &mut timeline,
    );
    loop_result.and(shutdown).map(|()| {
        if !exit_requested {
            return None;
        }
        let notice = if exit_requested_with_active_run {
            tui_report_quit_run(&store, &active_session_id, &mut timeline)
                .unwrap_or_else(|error| error)
        } else {
            tui_exit_disposition(&store, &active_session_id).unwrap_or_else(|error| error)
        };
        Some(notice)
    })
}

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    r: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        save_config, save_nib_config_full, LlmConfig, NibConfig, ProfileConfig, ProfilesConfig,
        ProviderEntry,
    };
    use crate::interactive::{
        bottom_scroll_for_wrap, execute_interactive_command, MAX_DRAFT_HISTORY,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    static TEST_TERMINAL_RESTORE_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn record_test_terminal_restore() -> io::Result<()> {
        TEST_TERMINAL_RESTORE_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn buffer_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::new();
                for x in 0..buffer.area.width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                row
            })
            .collect()
    }

    fn row_index_containing(rows: &[String], needle: &str) -> Option<usize> {
        rows.iter().position(|row| row.contains(needle))
    }

    fn approval_request(
        call: ToolCall,
        level: PermissionLevel,
        reply: oneshot::Sender<ApprovalDecision>,
    ) -> TuiApprovalRequest {
        let context = ApprovalContext::compatibility(&call, level);
        TuiApprovalRequest {
            call,
            level,
            context,
            selected_option: 1,
            typed: String::new(),
            details_open: false,
            detail_offset: 0,
            error: None,
            reply,
        }
    }

    fn recoverable_question_session() -> (
        tempfile::TempDir,
        SessionStore,
        String,
        crate::tools::ToolInvocationId,
    ) {
        let directory = tempdir().expect("session directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let mut session = store.try_create_session().expect("session");
        let plan = crate::session::Plan::new(
            "resume after answering",
            vec![crate::session::PlanStep {
                description: "use answer".to_string(),
                status: "Blocked".to_string(),
                outcome: None,
                attempts: 1,
                updated_at: None,
                verification_obligations: Vec::new(),
                content_generation: 0,
            }],
        );
        let plan_id = plan.id.clone();
        let invocation_id = crate::tools::ToolInvocationId::new();
        session.plan = Some(plan);
        session.events.push(crate::session::SessionEvent {
            index: 0,
            kind: "question_required".to_string(),
            details: json!({
                "invocation_id": invocation_id,
                "question": "Which target?",
                "options": ["alpha", "beta"],
            }),
            timestamp: Some(chrono::Utc::now()),
        });
        session
            .clarifications
            .push(crate::session::ClarificationRecord {
                invocation_id,
                plan_id: Some(plan_id),
                question: "Which target?".to_string(),
                options: vec!["alpha".to_string(), "beta".to_string()],
                dependent_paths: Vec::new(),
                status: crate::session::ClarificationStatus::Unresolved,
                answer: None,
                question_event_index: 0,
                answer_message_index: None,
                answer_event_index: None,
                reason: Some("left unanswered".to_string()),
                outcome: Some("left_unanswered".to_string()),
            });
        store.save(&mut session).expect("recoverable question");
        (directory, store, session.id, invocation_id)
    }

    fn mock_config() -> LlmConfig {
        LlmConfig {
            active_provider: Some("mock".to_string()),
            providers: HashMap::from([(
                "mock".to_string(),
                ProviderEntry {
                    model: "mock-model".to_string(),
                    api_key: None,
                    api_keys: Vec::new(),
                    base_url: None,
                    ..ProviderEntry::default()
                },
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn tui_environment_preflight_is_bounded_and_platform_neutral() {
        assert_eq!(
            tui_environment_rejection(true, true, Some("xterm-256color")),
            None
        );
        assert_eq!(tui_environment_rejection(true, true, None), None);
        assert_eq!(
            tui_environment_rejection(false, true, Some("xterm")),
            Some("the full-screen TUI requires terminal input and output")
        );
        assert_eq!(
            tui_environment_rejection(true, false, Some("xterm")),
            Some("the full-screen TUI requires terminal input and output")
        );
        assert_eq!(
            tui_environment_rejection(true, true, Some("DUMB")),
            Some("TERM=dumb does not support the full-screen TUI")
        );
    }

    #[test]
    fn approval_dock_no_color_style_preserves_non_color_signaling() {
        assert_eq!(
            approval_dock_style(true),
            Style::default().add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            composer_border_style(TuiFocus::Composer, true),
            Style::default().add_modifier(Modifier::BOLD)
        );
        assert_ne!(
            composer_border_style(TuiFocus::Composer, true),
            composer_border_style(TuiFocus::Transcript, true)
        );
        assert_eq!(
            approval_dock_style(false),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(ratatui::style::Color::Yellow)
        );
    }

    #[test]
    fn composer_accepts_chat_text_commands_and_enforces_its_bound() {
        let mut composer = Composer::default();
        for character in "/providers".chars() {
            assert_eq!(
                composer_action_for_key(
                    &mut composer,
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                ),
                ComposerAction::Pending
            );
        }
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Enter, KeyModifiers::NONE),
            ComposerAction::Submit("/providers".to_string())
        );
        assert!(composer.input.is_empty());
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Tab, KeyModifiers::NONE),
            ComposerAction::Pending
        );

        composer.set_text("x".repeat(MAX_COMPOSER_BYTES));
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Char('y'), KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input.len(), MAX_COMPOSER_BYTES);

        let mut slash = Composer::from_text("/skills install ");
        assert_eq!(
            composer_action_for_key(&mut slash, KeyCode::Enter, KeyModifiers::NONE),
            ComposerAction::Submit("/skills install ".to_string())
        );
        assert!(slash.input.is_empty());
    }

    #[test]
    fn shared_interaction_reducer_completion_owns_enter_tab_and_escape() {
        let mut composer = Composer::from_text("/");
        let mut completion = CompletionMenu::default();
        completion.sync(&composer.input);
        assert!(completion.is_open());
        assert!(completion.suggestions.len() <= 32);

        completion.selected = completion
            .suggestions
            .iter()
            .position(|item| item.insertion == "/session")
            .expect("session completion");
        assert_eq!(
            completion.handle_key(&mut composer, KeyCode::Tab),
            CompletionKeyResult::Consumed
        );
        assert_eq!(composer.input, "/session");
        assert!(!completion.is_open());

        composer.set_text("/".to_string());
        completion.sync(&composer.input);
        completion.selected = completion
            .suggestions
            .iter()
            .position(|item| item.insertion == "/help")
            .expect("help completion");
        assert_eq!(
            completion.handle_key(&mut composer, KeyCode::Enter),
            CompletionKeyResult::Submit
        );
        assert_eq!(composer.input, "/help");
        assert!(!completion.is_open());

        composer.input.push('x');
        completion.sync(&composer.input);
        assert!(!completion.is_open(), "unknown commands are not guessed");
        composer.set_text("/skills ".to_string());
        completion.sync(&composer.input);
        assert!(completion.is_open());
        let preserved = composer.input.clone();
        assert_eq!(
            completion.handle_key(&mut composer, KeyCode::Esc),
            CompletionKeyResult::Consumed
        );
        assert_eq!(composer.input, preserved);
        assert!(!completion.is_open());

        let mut skills = CompletionMenu::default();
        composer.set_text("/skills ".to_string());
        skills.sync(&composer.input);
        skills.selected = skills
            .suggestions
            .iter()
            .position(|item| item.insertion.ends_with(' '))
            .expect("free-form skill argument completion");
        assert_eq!(
            skills.handle_key(&mut composer, KeyCode::Enter),
            CompletionKeyResult::Consumed
        );
        assert!(composer.input.ends_with(' '));
    }

    #[test]
    fn shift_enter_inserts_a_newline_instead_of_submitting() {
        let mut composer = Composer::from_text("hello");
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Enter, KeyModifiers::SHIFT),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "hello\n");
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Enter, KeyModifiers::ALT),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "hello\n\n");
    }

    #[test]
    fn chrome_cache_skips_identical_idle_frames() {
        let cache = ChromeCache {
            generation: 1,
            session_id: "abc".to_string(),
            session_revision: Some(4),
            origin: "new".to_string(),
            chrome: TuiChrome::fixture(),
        };
        assert!(cache.matches(1, "abc", Some(4), "new"));
        assert!(!cache.matches(1, "abc", Some(5), "new"));
        assert!(!cache.matches(1, "other", Some(4), "new"));
        assert!(!cache.matches(2, "abc", Some(4), "new"));
        assert!(!cache.matches(1, "abc", Some(4), "resumed"));
    }

    #[test]
    fn selected_tool_block_can_expand_and_collapse() {
        let mut entry = ActivityEntry::new(
            ActivityKind::Tool,
            "list_directory ok · 1 entries",
            "{\"path\":\"README.md\"}",
        )
        .folded();
        assert!(entry.render_line().starts_with('›'));
        assert!(!entry.render_line().contains("README.md"));
        entry.folded = false;
        assert!(entry.render_line().contains("README.md"));
    }

    #[test]
    fn osc52_copy_sequence_is_exact_and_utf8_safe() {
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(osc52_sequence("🙂"), "\x1b]52;c;8J+Zgg==\x07");
    }

    #[test]
    fn composer_border_is_visible_and_focus_changes_its_style() {
        fn render_border_style(focus: TuiFocus) -> Style {
            let backend = TestBackend::new(72, 18);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut viewport = TranscriptViewport::default();
            terminal
                .draw(|frame| {
                    render_session_activities(
                        frame,
                        &TuiChrome::fixture(),
                        &[ActivityEntry::new(ActivityKind::Assistant, "", "ready")],
                        &Composer::default(),
                        None,
                        None,
                        false,
                        &mut viewport,
                        focus,
                        None,
                        0,
                        None,
                        None,
                        &StartupWelcome::fixture(),
                        None,
                        None,
                        None,
                    )
                })
                .expect("render");
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .find(|cell| cell.symbol() == "─")
                .expect("composer top border")
                .style()
        }

        let focused = render_border_style(TuiFocus::Composer);
        let transcript = render_border_style(TuiFocus::Transcript);
        assert_ne!(focused, transcript);
    }

    #[test]
    fn selected_transcript_block_is_kept_inside_the_row_viewport() {
        let owners = (0..12).flat_map(|owner| [owner, owner]).collect::<Vec<_>>();
        let mut viewport = TranscriptViewport::default();
        viewport.observe_layout(owners.len(), 5);
        assert_eq!(viewport.top_row(), 19);

        ensure_selected_visible(&mut viewport, &owners, 2);
        assert_eq!(viewport.top_row(), 4);
        ensure_selected_visible(&mut viewport, &owners, 11);
        assert_eq!(viewport.top_row(), 19);
    }

    #[test]
    fn empty_session_invites_a_conversation_and_keeps_the_prompt_visible() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "nib  ·  project  ·  session abc12345  ·  new",
                    "idle  ·  mock/mock-model  ·  approval manual  ·  sandboxed",
                    "",
                    &Composer::default(),
                    None,
                    None,
                )
            })
            .expect("render empty session");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("Your AI agent for this project."));
        assert!(rendered.contains(&format!("Nib {}", env!("CARGO_PKG_VERSION"))));
        assert!(rendered.contains("Working directory"));
        assert!(rendered.contains("~/project"));
        assert!(rendered.contains("/new"));
        assert!(rendered.contains("new session and worktree"));
        assert!(rendered.contains("/session"));
        assert!(rendered.contains("Ctrl+C"));
        assert!(rendered.contains("never copies or quits"));
        assert!(rendered.contains("> Ask nib anything…"));
        assert!(rendered.contains("approval manual"));
        assert!(rendered.contains("idle"));
        assert_eq!(terminal.get_cursor_position().expect("cursor").x, 2);
    }

    #[test]
    fn startup_asks_permission_to_work_in_the_working_directory() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut viewport = TranscriptViewport::default();
        let welcome = StartupWelcome {
            version: format!("Nib {}", env!("CARGO_PKG_VERSION")),
            working_directory: "~/work/nib".to_string(),
            update_notice: None,
            consent_directory: Some("~/work/nib".to_string()),
            consent_selected: 0,
        };
        terminal
            .draw(|frame| {
                render_session_activities(
                    frame,
                    &TuiChrome::fixture(),
                    &[],
                    &Composer::default(),
                    None,
                    None,
                    false,
                    &mut viewport,
                    TuiFocus::Composer,
                    None,
                    0,
                    None,
                    None,
                    &welcome,
                    None,
                    None,
                    None,
                )
            })
            .expect("render workspace consent");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Work in this directory"), "{rendered}");
        assert!(rendered.contains("~/work/nib"), "{rendered}");
        assert!(rendered.contains("WAITING PERMISSION"), "{rendered}");
        assert!(rendered.contains("Allow"), "{rendered}");
        assert!(rendered.contains("Decline"), "{rendered}");
        assert!(!rendered.contains("Permission required"), "{rendered}");
        assert!(
            rendered.contains("Enter decline · select Allow then Enter"),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("Nib {}", env!("CARGO_PKG_VERSION"))),
            "{rendered}"
        );
    }

    #[test]
    fn workspace_consent_key_reducer_persists_the_gate_before_a_goal_can_start() {
        let project = tempdir().expect("project");
        let mut consent_directory = pending_workspace_consent(project.path());
        let mut pending_goal = Some("inspect the project".to_string());
        assert!(consent_directory.is_some());

        let mut selected = 0;
        assert_eq!(
            workspace_consent_action_for_key(&mut selected, KeyCode::Down, KeyModifiers::NONE),
            WorkspaceConsentAction::SelectionChanged
        );
        assert_eq!(selected, 1);
        assert_eq!(
            workspace_consent_action_for_key(&mut selected, KeyCode::Enter, KeyModifiers::NONE),
            WorkspaceConsentAction::Decline
        );
        assert!(pending_workspace_consent(project.path()).is_some());
        assert_eq!(pending_goal.as_deref(), Some("inspect the project"));

        selected = 0;
        assert_eq!(
            workspace_consent_action_for_key(&mut selected, KeyCode::Char('y'), KeyModifiers::NONE),
            WorkspaceConsentAction::Allow
        );
        let released = grant_workspace_access_and_release_goal(
            project.path(),
            &mut consent_directory,
            &mut pending_goal,
        )
        .expect("persist workspace grant before releasing goal");
        assert_eq!(released.as_deref(), Some("inspect the project"));
        assert!(pending_goal.is_none());
        assert!(consent_directory.is_none());
        assert!(pending_workspace_consent(project.path()).is_none());
        assert!(crate::config::workspace_access_is_granted(project.path()).expect("read grant"));
        assert!(grant_workspace_access_and_release_goal(
            project.path(),
            &mut consent_directory,
            &mut pending_goal,
        )
        .expect("reload persisted workspace grant")
        .is_none());
    }

    #[test]
    fn empty_state_includes_update_notice_and_install_instruction() {
        let welcome = StartupWelcome {
            version: "Nib 0.1.0".to_string(),
            working_directory: "~/work/nib".to_string(),
            update_notice: Some(
                "Channel update available: 0.1.1 (development, abcdef0). Run `nib update`."
                    .to_string(),
            ),
            consent_directory: None,
            consent_selected: 0,
        };
        let rendered = empty_state_lines(&welcome, true)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Nib 0.1.0"), "{rendered}");
        assert!(rendered.contains("~/work/nib"), "{rendered}");
        assert!(
            rendered.contains("Channel update available: 0.1.1"),
            "{rendered}"
        );
        assert!(rendered.contains("Run `nib update`"), "{rendered}");
        assert!(rendered.contains("/new"), "{rendered}");
        assert!(rendered.contains("Ctrl+C"), "{rendered}");
    }

    #[test]
    fn ctrl_q_requires_two_uninterrupted_presses() {
        let now = Instant::now();
        let arm = QuitArm {
            armed_at: now,
            consumer: InteractionLayer::Composer,
        };
        assert_eq!(
            quit_confirm_action(
                Some(arm),
                now + Duration::from_millis(400),
                InteractionLayer::Composer,
            ),
            QuitConfirmAction::Confirm
        );
        assert_eq!(
            quit_confirm_action(None, now, InteractionLayer::Composer),
            QuitConfirmAction::Arm
        );
        assert_eq!(quit_arm_after_input(Some(arm), false), None);
        assert_eq!(quit_arm_after_input(Some(arm), true), Some(arm));
        assert_eq!(
            quit_confirm_action(
                quit_arm_after_input(Some(arm), false),
                now + Duration::from_millis(400),
                InteractionLayer::Composer,
            ),
            QuitConfirmAction::Arm
        );
        assert_eq!(
            quit_confirm_action(
                Some(arm),
                now + QUIT_CONFIRM + Duration::from_millis(1),
                InteractionLayer::Composer,
            ),
            QuitConfirmAction::Arm,
            "the confirmation window must expire"
        );
        assert_eq!(
            quit_arm_for_consumer(Some(arm), InteractionLayer::Approval),
            None,
            "a new modal consumer must disarm quit confirmation"
        );
    }

    #[test]
    fn startup_welcome_strips_stderr_prefix_from_update_notice() {
        let welcome = StartupWelcome::new(
            Path::new("/tmp/project"),
            Some("[nib] Channel update available: 0.1.1. Run `nib update`.".to_string()),
        );
        assert_eq!(
            welcome.version,
            format!("Nib {}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            welcome.update_notice.as_deref(),
            Some("Channel update available: 0.1.1. Run `nib update`.")
        );
    }

    #[test]
    fn transcript_separates_thought_tools_and_user_facing_speech() {
        let activities = vec![
            ActivityEntry::new(ActivityKind::User, "", "inspect wrap"),
            ActivityEntry::new(ActivityKind::Thinking, "Thought for 14s", String::new()).folded(),
            ActivityEntry::new(
                ActivityKind::Tool,
                "read_file running · src/lib.rs",
                "line one",
            )
            .folded(),
            ActivityEntry::new(ActivityKind::Assistant, "", "Here is the answer"),
        ];
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut viewport = TranscriptViewport::default();
        let welcome = StartupWelcome::fixture();
        terminal
            .draw(|frame| {
                render_session_activities(
                    frame,
                    &TuiChrome::fixture(),
                    &activities,
                    &Composer::default(),
                    None,
                    None,
                    false,
                    &mut viewport,
                    TuiFocus::Composer,
                    None,
                    0,
                    None,
                    None,
                    &welcome,
                    None,
                    None,
                    None,
                )
            })
            .expect("render channels");
        let rows = buffer_rows(&terminal);
        let joined = rows.concat();
        assert!(joined.contains("inspect wrap"), "{joined}");
        assert!(joined.contains("Thought for 14s"), "{joined}");
        assert!(joined.contains("●"), "{joined}");
        assert!(joined.contains("read_file"), "{joined}");
        assert!(joined.contains("Reading…"), "{joined}");
        assert!(joined.contains("Here is the answer"), "{joined}");
        assert!(!joined.contains("planning"), "{joined}");
        assert!(!joined.contains(" running"), "{joined}");
        assert!(!joined.contains("line one"), "{joined}");
        assert!(!joined.contains("● you"), "no you role label: {joined}");
        assert!(!joined.contains("● nib"), "no nib role label: {joined}");
        assert!(!joined.contains("thought"), "{joined}");
        assert!(
            !rows.iter().any(|row| row.contains("● tool")),
            "tool rows use the tool name, not a tool role label: {rows:?}"
        );
        let you = row_index_containing(&rows, "inspect wrap").expect("user");
        let thought = row_index_containing(&rows, "Thought for 14s").expect("thought");
        let tool = row_index_containing(&rows, "read_file").expect("tool");
        let speech = rows
            .iter()
            .position(|row| row.contains("Here is the answer"))
            .expect("speech");
        assert!(you < thought, "{rows:?}");
        assert!(thought < tool, "{rows:?}");
        assert!(tool < speech, "{rows:?}");
        assert!(
            rows[you].contains('●'),
            "user input is marked with a colored dot: {}",
            rows[you]
        );
        assert!(
            rows[thought].contains('▸'),
            "thought is a folded chevron header: {}",
            rows[thought]
        );
        assert!(
            !rows[thought].contains('●'),
            "thought does not use the tool dot: {}",
            rows[thought]
        );
        assert!(
            rows[tool].contains('●'),
            "tool calls are marked with a colored dot: {}",
            rows[tool]
        );
        assert!(
            rows[speech].contains('●'),
            "assistant speech starts with a colored dot: {}",
            rows[speech]
        );
        assert!(
            rows[speech].contains("nib") || rows[speech].contains('●'),
            "assistant speech has a role or channel marker: {}",
            rows[speech]
        );
    }

    #[test]
    fn quiet_tool_title_hides_running_and_ok() {
        assert_eq!(
            quiet_tool_title("read_file running · src/lib.rs"),
            "read_file  src/lib.rs"
        );
        assert_eq!(
            quiet_tool_title("read_file ok · src/lib.rs · 3 lines"),
            "read_file  src/lib.rs · 3 lines"
        );
        assert_eq!(
            quiet_tool_title("run_terminal failed · task backup · exit 1"),
            "run_terminal failed · task backup · exit 1"
        );
        assert_eq!(
            thought_header_title(Duration::from_secs(14), Some("267")),
            "Thought for 14s, 267 tokens"
        );
        assert_eq!(
            thought_header_title(Duration::from_secs(14), None),
            "Thought for 14s"
        );
        assert_eq!(meter_token_label("tok -"), None);
        assert_eq!(meter_token_label("tok 8k").as_deref(), Some("8k"));
    }

    #[test]
    fn header_shows_folder_and_branch_left_and_model_right() {
        let chrome = TuiChrome {
            folder: "~/work/nib".to_string(),
            branch: "feat/t039".to_string(),
            model: "mock-model".to_string(),
            context: "12%".to_string(),
            approval: "manual".to_string(),
            agent_mode: "idle".to_string(),
        };
        let line = header_line(&chrome, 80, true);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.starts_with("~/work/nib"), "{text}");
        assert!(text.contains("feat/t039"), "{text}");
        assert!(
            text.find("~/work/nib").expect("folder") < text.find("mock-model").expect("model"),
            "{text}"
        );
        assert!(text.trim_end().ends_with("12%"), "{text}");
        let folder_end = text.find("feat/t039").expect("branch") + "feat/t039".len();
        let model_start = text.find("mock-model").expect("model");
        assert!(
            text[folder_end..model_start].chars().all(|ch| ch == ' '),
            "model must sit on the right of the header: {text:?}"
        );
    }

    #[test]
    fn speech_renders_markdown_headings_lists_and_fenced_code() {
        let activities = vec![ActivityEntry::new(
            ActivityKind::Assistant,
            "",
            "# Fix\n\nUse `task check` then:\n\n```rust\nfn main() {}\n```\n\n- one\n- two\n",
        )];
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut viewport = TranscriptViewport::default();
        let welcome = StartupWelcome::fixture();
        terminal
            .draw(|frame| {
                render_session_activities(
                    frame,
                    &TuiChrome::fixture(),
                    &activities,
                    &Composer::default(),
                    None,
                    None,
                    false,
                    &mut viewport,
                    TuiFocus::Composer,
                    None,
                    0,
                    None,
                    None,
                    &welcome,
                    None,
                    None,
                    None,
                )
            })
            .expect("render markdown speech");
        let rows = buffer_rows(&terminal);
        let joined = rows.concat();
        assert!(joined.contains("# Fix"), "{joined}");
        assert!(joined.contains("task check"), "{joined}");
        assert!(joined.contains("fn main()"), "{joined}");
        assert!(joined.contains("• one"), "{joined}");
        assert!(joined.contains("• two"), "{joined}");
        assert!(
            !joined.contains("```"),
            "fenced markers should be rendered away: {joined}"
        );
    }

    #[test]
    fn activity_styles_keep_text_labels_without_color() {
        let line = dotted_header_lines(ActivityKind::Assistant, "", "", 40, true)
            .into_iter()
            .next()
            .expect("header");
        assert_eq!(line.spans[0].content, "● ");
        assert_eq!(line.spans[0].style.fg, None);
        assert!(line.spans.get(1).is_none_or(|span| span.content != "nib"));
        let tool = dotted_header_lines(
            ActivityKind::Tool,
            "read_file ok · src/lib.rs",
            "",
            40,
            true,
        )
        .into_iter()
        .next()
        .expect("tool header");
        assert_eq!(tool.spans[0].content, "● ");
        assert_eq!(tool.spans[1].content, "read_file ok · src/lib.rs");
        let result = dotted_result_lines("line one", 40, ChannelInk::ToolResult, true);
        assert_eq!(result[0].spans[0].content, "· ");
        assert_eq!(result[0].spans[1].content, "line one");
        assert_eq!(result[0].spans[0].style.fg, None);
    }

    #[test]
    fn footer_and_completion_follow_the_current_interaction() {
        let mut viewport = TranscriptViewport::default();
        let chrome = TuiChrome::fixture();
        assert_eq!(
            footer_line(&chrome, &viewport, 0, WaitingKind::None, None, false),
            "approval manual · idle"
        );
        let mut running = chrome.clone();
        running.agent_mode = "execute".to_string();
        assert_eq!(
            footer_line(&running, &viewport, 0, WaitingKind::None, None, false),
            "approval manual · execute"
        );
        assert_eq!(
            footer_line(&running, &viewport, 2, WaitingKind::None, None, false),
            "queue 2 · approval manual · execute"
        );
        let mut waiting = chrome.clone();
        waiting.agent_mode = "WAITING APPROVAL".to_string();
        assert_eq!(
            footer_line(&waiting, &viewport, 0, WaitingKind::Approval, None, false),
            "approval manual · WAITING APPROVAL · Enter deny · select Approve once then Enter · Esc deny"
        );
        let mut question = chrome.clone();
        question.agent_mode = "WAITING QUESTION".to_string();
        assert_eq!(
            footer_line(&question, &viewport, 0, WaitingKind::Question, None, false),
            "approval manual · WAITING QUESTION · Type answer · Enter submit · Esc leave unanswered"
        );
        let mut workspace = chrome.clone();
        workspace.agent_mode = "WAITING PERMISSION".to_string();
        assert_eq!(
            footer_line(
                &workspace,
                &viewport,
                0,
                WaitingKind::Workspace,
                None,
                false
            ),
            "approval manual · WAITING PERMISSION · Enter decline · select Allow then Enter · Esc decline"
        );
        assert!(footer_line(
            &chrome,
            &viewport,
            0,
            WaitingKind::None,
            Some("Tab insert · Enter run · Esc close"),
            false,
        )
        .contains("Tab insert"));
        assert!(
            footer_line(&chrome, &viewport, 0, WaitingKind::None, None, true)
                .contains("Ctrl+Y copy · Esc clear")
        );
        viewport.observe_layout(100, 10);
        viewport.apply(TranscriptViewportAction::PageUp);
        assert!(
            footer_line(&running, &viewport, 0, WaitingKind::None, None, false)
                .contains("Ctrl+End follow")
        );

        let area = Rect::new(0, 0, 100, 30);
        let composer_height = 2;
        let completion = completion_rect(area, MAX_VISIBLE_COMPLETIONS, composer_height);
        assert_eq!(completion.width, 92);
        let composer_bottom = completion.y;
        assert!(completion.height >= 2, "{completion:?}");
        assert_eq!(
            completion.y + completion.height,
            area.height.saturating_sub(1),
            "{completion:?}"
        );
        assert!(
            composer_bottom >= composer_height,
            "completion {completion:?} must sit under the composer"
        );

        let multiline = completion_rect(Rect::new(0, 0, 80, 24), MAX_VISIBLE_COMPLETIONS, 6);
        let composer_bottom = 24u16.saturating_sub(1).saturating_sub(multiline.height);
        assert_eq!(multiline.y, composer_bottom, "{multiline:?}");
        assert!(
            multiline.y >= 6,
            "completion must start at or below the composer bottom: {multiline:?}"
        );
        assert_eq!(multiline.y + multiline.height, 23, "{multiline:?}");

        let permissions = interactive_completions("/permissions ");
        let rows = permissions
            .iter()
            .map(|suggestion| completion_line(suggestion, 76))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(rows.len(), 4, "fixed subcommands must have distinct rows");
        assert!(rows.iter().any(|row| row.contains("/permissions manual")));
        assert!(rows
            .iter()
            .all(|row| !row.starts_with('>') && !row.starts_with("  /")));
        assert!(rows.iter().all(|row| unicode_display_width(row) <= 76));
    }

    #[test]
    fn completion_truncation_preserves_complete_graphemes() {
        for grapheme in ["☺\u{fe0f}", "👩\u{200d}💻"] {
            let text = format!("a{grapheme}xy");
            assert_eq!(truncate_completion_text(&text, 3), "a…");
            assert_eq!(truncate_completion_text(&text, 4), format!("a{grapheme}…"));
        }
        assert_eq!(truncate_completion_text("e\u{301}xyz", 2), "e\u{301}…");
    }

    #[test]
    fn slash_completion_renders_under_the_composer_without_covering_conversation() {
        let mut completion = CompletionMenu::default();
        completion.sync("/");
        assert!(completion.is_open());
        let composer = Composer::from_text("/");
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_session_view_with_completion(
                    frame,
                    "nib · project · session abc",
                    "idle · mock/mock-model",
                    "you  hello conversation\n\nnib  keep this visible",
                    &composer,
                    Some(&completion),
                    None,
                    false,
                )
            })
            .expect("render slash completion");
        let rows = buffer_rows(&terminal);
        let conversation =
            row_index_containing(&rows, "hello conversation").expect("conversation row");
        let reply = row_index_containing(&rows, "keep this visible").expect("reply row");
        let prompt = row_index_containing(&rows, "> /").expect("composer row");
        let suggestion = row_index_containing(&rows, "/status").expect("completion row");
        assert!(
            conversation < prompt,
            "conversation must stay above the input: {rows:?}"
        );
        assert!(
            reply < prompt,
            "assistant text must stay above the input: {rows:?}"
        );
        assert!(
            prompt < suggestion,
            "slash options must render under the text input: {rows:?}"
        );
        let prompt_slash = rows[prompt].find('/').expect("composer slash");
        let option_slash = rows[suggestion].find('/').expect("option slash");
        assert_eq!(
            prompt_slash, option_slash,
            "options must align to the composer /: prompt={:?} option={:?}",
            rows[prompt], rows[suggestion]
        );
        assert!(
            !rows[suggestion].contains("> /") && !rows[suggestion].contains("> /status"),
            "completion list must not use a caret: {}",
            rows[suggestion]
        );
    }

    #[test]
    fn waiting_meter_shows_job_step_time_tokens_and_status() {
        let meter = WaitingMeter {
            job: "read_file · src/lib.rs".to_string(),
            step: "2/5".to_string(),
            elapsed: Duration::from_secs(12),
            tokens: "tok 8k".to_string(),
            status: "running".to_string(),
            tick: 0,
        };
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_session_view_with_completion(
                    frame,
                    "nib · project · session abc",
                    "running · mock/mock-model",
                    "you  inspect wrap",
                    &Composer::default(),
                    None,
                    Some(&meter),
                    true,
                )
            })
            .expect("render waiting meter");
        let rows = buffer_rows(&terminal);
        let joined = rows.concat();
        assert!(joined.contains("read_file · src/lib.rs"), "{joined}");
        assert!(joined.contains("step 2/5"), "{joined}");
        assert!(joined.contains("12s"), "{joined}");
        assert!(joined.contains("tok 8k"), "{joined}");
        assert!(joined.contains("running"), "{joined}");
        let meter_row = row_index_containing(&rows, "step 2/5").expect("meter row");
        let prompt = rows
            .iter()
            .position(|row| row.contains("> Ask nib anything"))
            .expect("composer row");
        assert!(
            meter_row < prompt,
            "waiting meter must sit above the text input: {rows:?}"
        );
        assert_eq!(
            live_job_label(
                &[ActivityEntry::new(
                    ActivityKind::Tool,
                    "read_file running · src/lib.rs",
                    String::new(),
                )],
                Some("planning"),
            ),
            "read_file · src/lib.rs"
        );
        assert_eq!(format_elapsed(Duration::from_secs(75)), "1m15s");

        let compact = waiting_meter_text(&meter, 40, true);
        assert!(unicode_display_width(&compact) <= 40, "{compact}");
        assert!(compact.contains("s:2/5"), "{compact}");
        assert!(compact.contains("12s"), "{compact}");
        assert!(compact.contains("t:8k"), "{compact}");
        assert!(compact.contains("running"), "{compact}");
    }

    #[test]
    fn current_session_view_is_primary_and_has_no_permanent_browser() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let composer = Composer::default();
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace  ·  sess active-one  ·  local  ·  -",
                    "idle  ·  mock/mock-model  ·  manual/hybrid net restricted  ·  queue 0",
                    "you  persisted request\n\nnib  persisted reply",
                    &composer,
                    None,
                    None,
                )
            })
            .expect("render current session view");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("workspace"));
        assert!(rendered.contains("main"));
        assert!(rendered.contains("mock-model"));
        assert!(rendered.contains("idle"));
        assert!(rendered.contains("persisted request"));
        assert!(rendered.contains("persisted reply"));
        assert!(!rendered.contains("nibble sessions"));
    }

    #[test]
    fn session_switch_dispatch_previews_confirms_and_strictly_reloads() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        for (id, message) in [("session-a", "from a"), ("session-b", "from b")] {
            store.create_session_with_id(id);
            store
                .try_append_message(id, "user", message)
                .expect("append persisted message");
        }
        let before_a = store.load("session-a").expect("session a");
        let before_b = store.load("session-b").expect("session b");
        let mut active_id = "session-a".to_string();
        let mut timeline = ActiveTimeline::load(&store, &active_id).expect("active timeline");
        timeline.push_status("live output owned by a".to_string());
        let composer = Composer::from_text("draft survives browsing");
        let mut switcher = load_session_switcher(&store, &active_id).expect("switcher");
        switcher.selected = switcher
            .candidates
            .iter()
            .position(|candidate| candidate.id == "session-b")
            .expect("target session");

        assert_eq!(
            session_switcher_action_for_key(&mut switcher, KeyCode::Enter),
            SwitcherAction::Pending
        );
        assert!(switcher.confirming);
        assert_eq!(active_id, "session-a");
        assert_eq!(composer.input, "draft survives browsing");
        assert_eq!(
            session_switcher_action_for_key(&mut switcher, KeyCode::Esc),
            SwitcherAction::Pending
        );
        assert!(!switcher.confirming);
        assert_eq!(active_id, "session-a");
        assert_eq!(composer.input, "draft survives browsing");

        assert!(
            activate_selected_session(&store, &switcher, true, &mut active_id, &mut timeline,)
                .is_err()
        );
        assert_eq!(active_id, "session-a");
        assert!(timeline.rendered_text().contains("live output owned by a"));
        assert_eq!(composer.input, "draft survives browsing");

        activate_selected_session(&store, &switcher, false, &mut active_id, &mut timeline)
            .expect("confirmed switch");
        assert_eq!(active_id, "session-b");
        assert_eq!(timeline.session_id, "session-b");
        assert!(timeline.rendered_text().contains("from b"));
        assert!(!timeline.rendered_text().contains("live output owned by a"));
        assert_eq!(composer.input, "draft survives browsing");
        assert_eq!(
            store.load("session-a").expect("session a unchanged"),
            before_a
        );
        assert_eq!(
            store.load("session-b").expect("session b unchanged"),
            before_b
        );
    }

    #[test]
    fn stale_switch_target_fails_without_changing_session_or_draft() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        store.create_session_with_id("session-a");
        store.create_session_with_id("session-b");
        let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
        switcher.selected = switcher
            .candidates
            .iter()
            .position(|candidate| candidate.id == "session-b")
            .expect("target");
        std::fs::write(store.sessions_dir().join("session-b.json"), "not json")
            .expect("corrupt stale target");

        let mut active_id = "session-a".to_string();
        let mut timeline = ActiveTimeline::load(&store, &active_id).expect("timeline");
        let original = timeline.rendered_text();
        let composer = Composer::from_text("keep this draft");
        let error =
            activate_selected_session(&store, &switcher, false, &mut active_id, &mut timeline)
                .expect_err("corrupt target must fail closed");
        assert!(error.contains("active session is unchanged"));
        assert_eq!(active_id, "session-a");
        assert_eq!(timeline.rendered_text(), original);
        assert_eq!(composer.input, "keep this draft");
    }

    #[test]
    fn changed_switch_target_requires_a_fresh_preview() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        store.create_session_with_id("session-a");
        store.create_session_with_id("session-b");
        let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
        switcher.selected = switcher
            .candidates
            .iter()
            .position(|candidate| candidate.id == "session-b")
            .expect("target");
        store
            .try_append_message("session-b", "user", "changed after preview")
            .expect("change target");

        let mut active_id = "session-a".to_string();
        let mut timeline = ActiveTimeline::load(&store, &active_id).expect("timeline");
        let original = timeline.rendered_text();
        let error =
            activate_selected_session(&store, &switcher, false, &mut active_id, &mut timeline)
                .expect_err("changed target must fail closed");

        assert!(error.contains("changed since it was previewed"));
        assert_eq!(active_id, "session-a");
        assert_eq!(timeline.rendered_text(), original);
    }

    #[test]
    fn bounded_switcher_retains_and_marks_an_old_active_session() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        for index in 0..=100 {
            store.create_session_with_id(format!("session-{index:03}"));
        }

        let mut switcher = load_session_switcher(&store, "session-000").expect("switcher");
        assert_eq!(switcher.candidates.len(), 100);
        assert_eq!(switcher.omitted, 1);
        assert_eq!(switcher.candidates[switcher.selected].id, "session-000");

        let omitted_id = (0..=100)
            .map(|index| format!("session-{index:03}"))
            .find(|id| {
                !switcher
                    .candidates
                    .iter()
                    .any(|candidate| &candidate.id == id)
            })
            .expect("one omitted session");
        for character in omitted_id.chars() {
            assert_eq!(
                session_switcher_action_for_key(&mut switcher, KeyCode::Char(character)),
                SwitcherAction::Pending
            );
        }
        let SwitcherAction::PreviewExact(requested) =
            session_switcher_action_for_key(&mut switcher, KeyCode::Enter)
        else {
            panic!("exact ID entry must request a preview")
        };
        preview_exact_session(&store, &mut switcher, "session-000", &requested)
            .expect("preview omitted target");
        assert_eq!(switcher.candidates.len(), MAX_SWITCHER_CANDIDATES);
        assert!(switcher
            .candidates
            .iter()
            .any(|candidate| candidate.id == "session-000" && candidate.is_active));
        assert_eq!(switcher.candidates[switcher.selected].id, omitted_id);
        assert_eq!(
            session_switcher_action_for_key(&mut switcher, KeyCode::Enter),
            SwitcherAction::Pending
        );
        assert!(switcher.confirming);

        switcher.confirming = false;
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("short switcher terminal");
        terminal
            .draw(|frame| render_session_switcher(frame, frame.area(), &switcher, "session-000"))
            .expect("render selected tail candidate");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains(&omitted_id));
    }

    #[test]
    fn clear_effect_replaces_the_entire_visible_session_projection() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        store.create_session_with_id("old-session");
        store
            .try_append_message("old-session", "user", "old persisted text")
            .expect("old message");
        let mut active_id = "old-session".to_string();
        let mut timeline = ActiveTimeline::load(&store, &active_id).expect("timeline");
        timeline.push_status("old live text".to_string());

        let InteractiveEffect::SessionChanged { session_id, output } = execute_interactive_command(
            crate::interactive::InteractiveCommand::Clear,
            directory.path(),
            &store,
            &active_id,
        )
        .expect("clear effect") else {
            panic!("clear must create a session")
        };
        replace_active_session(
            &store,
            session_id.clone(),
            output,
            &mut active_id,
            &mut timeline,
        )
        .expect("replace timeline");

        assert_eq!(active_id, session_id);
        assert_eq!(timeline.session_id, session_id);
        assert!(!timeline.rendered_text().contains("old persisted text"));
        assert!(!timeline.rendered_text().contains("old live text"));
        assert!(timeline.rendered_text().contains("Started fresh session"));
    }

    #[test]
    fn model_picker_accepts_selection_exact_ids_and_cancel() {
        let selection = ModelSelection {
            provider: "mock".to_string(),
            current: "mock-a".to_string(),
            available: vec!["mock-a".to_string(), "mock-b".to_string()],
            sensitive_values: Vec::new(),
        };
        let mut picker = PendingModelSelection::new(selection.clone());
        assert_eq!(picker.selected_option, 0);
        assert_eq!(
            model_action_for_key(&mut picker, KeyCode::Down),
            ModelAction::Pending
        );
        assert_eq!(
            model_action_for_key(&mut picker, KeyCode::Enter),
            ModelAction::Submit("mock-b".to_string())
        );

        let mut exact = PendingModelSelection::new(selection);
        for character in "gateway/custom".chars() {
            assert_eq!(
                model_action_for_key(&mut exact, KeyCode::Char(character)),
                ModelAction::Pending
            );
        }
        assert_eq!(
            model_action_for_key(&mut exact, KeyCode::Enter),
            ModelAction::Submit("gateway/custom".to_string())
        );
        assert_eq!(
            model_action_for_key(&mut exact, KeyCode::Esc),
            ModelAction::Cancel
        );
    }

    #[test]
    fn tui_resolves_the_selected_profile_session_store() {
        let directory = tempdir().expect("tempdir");
        let mut config = NibConfig {
            profiles: ProfilesConfig {
                default: "workspace".to_string(),
                active: vec![ProfileConfig {
                    id: "workspace".to_string(),
                    root: PathBuf::from("."),
                    ..ProfileConfig::default()
                }],
            },
            ..NibConfig::default()
        };
        save_nib_config_full(directory.path(), &mut config).expect("save config");

        let store = SessionStore::for_project(directory.path()).expect("profile store");
        let expected = directory.path().join(".nib/profiles/workspace/sessions");
        let actual_directory = crate::daemons::state::StableDirectory::open(store.sessions_dir())
            .expect("opened profile session store");
        let expected_directory = crate::daemons::state::StableDirectory::open(&expected)
            .expect("opened expected session store");
        assert!(
            actual_directory.same_identity(&expected_directory),
            "selected profile session store resolved to another directory"
        );
        assert!(!directory.path().join(".nib/sessions").exists());
    }

    #[test]
    fn active_timeline_hydrates_persisted_history_with_explicit_bounds() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        store.create_session_with_id("detail-session");
        store
            .try_append_message("detail-session", "user", "inspect this session")
            .expect("user message");
        store
            .try_append_message("detail-session", "assistant", "inspection complete")
            .expect("assistant message");

        let timeline = ActiveTimeline::load(&store, "detail-session").expect("load timeline");
        let persisted = store.load("detail-session").expect("persisted session");
        let detail = SessionDetail::new("detail-session", Some(&persisted), &[]);
        assert_eq!(timeline.session_id, "detail-session");
        assert!(detail.text.contains("Session: detail-session"));
        assert!(detail.text.contains("Messages: 2"));
        assert!(detail.text.contains("inspect this session"));
        assert!(detail.text.len() <= MAX_SESSION_DETAIL_BYTES);
        assert!(detail.text.lines().count() <= MAX_SESSION_DETAIL_ROWS);
        assert!(timeline
            .activities
            .iter()
            .any(|entry| entry.kind == ActivityKind::User
                && entry.body.contains("inspect this session")));
    }

    #[test]
    fn session_listing_surfaces_store_errors() {
        let directory = tempdir().expect("tempdir");
        let sessions = directory.path().join("sessions");
        let store = SessionStore::at_dir(sessions.clone());
        std::fs::write(sessions.join("invalid session id.json"), "{}").expect("invalid session");

        let error =
            interactive_session_selection(&store, "active").expect_err("invalid listing must fail");
        assert!(error.contains("failed to list sessions"));
        assert!(error.contains("invalid session id"));
    }

    #[test]
    fn session_listing_surfaces_valid_named_corrupt_state() {
        let directory = tempdir().expect("tempdir");
        let sessions = directory.path().join("sessions");
        let store = SessionStore::at_dir(sessions.clone());
        std::fs::write(sessions.join("corrupt-session.json"), "not json").expect("corrupt session");

        let error =
            interactive_session_selection(&store, "active").expect_err("corrupt listing must fail");
        assert!(error.contains("failed to list sessions"));
        assert!(error.contains("parse session JSON"));
    }

    #[test]
    fn session_switcher_renders_preview_and_confirmation_as_bounded_overlays() {
        let switcher = SessionSwitcher {
            candidates: vec![InteractiveSessionCandidate {
                id: "visible-session".to_string(),
                label: "visible-session".to_string(),
                preview: "Latest user message: visible content".to_string(),
                is_active: false,
                snapshot_token: [0; 32],
            }],
            selected: 0,
            omitted: 0,
            confirming: true,
            exact_id: String::new(),
            error: None,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                render_session_switcher(frame, frame.area(), &switcher, "current-session")
            })
            .expect("render switcher");

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Resume visible-session"), "{rendered}");
        assert!(rendered.contains("Y / Enter resume"), "{rendered}");
        assert!(!rendered.contains("Resume Confirmation"), "{rendered}");
    }

    #[test]
    fn session_list_matches_slash_option_style_without_a_caret() {
        let switcher = SessionSwitcher {
            candidates: vec![
                InteractiveSessionCandidate {
                    id: "current-session".to_string(),
                    label: "wrap-fix".to_string(),
                    preview: "Latest user message: wrap".to_string(),
                    is_active: true,
                    snapshot_token: [0; 32],
                },
                InteractiveSessionCandidate {
                    id: "other-session".to_string(),
                    label: "inspect tests".to_string(),
                    preview: "Latest user message: inspect".to_string(),
                    is_active: false,
                    snapshot_token: [1; 32],
                },
            ],
            selected: 1,
            omitted: 0,
            confirming: false,
            exact_id: String::new(),
            error: None,
        };
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_session_switcher(frame, frame.area(), &switcher, "current-session")
            })
            .expect("render session list");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("wrap-fix"), "{rendered}");
        assert!(rendered.contains("inspect tests"), "{rendered}");
        assert!(rendered.contains("active"), "{rendered}");
        assert!(!rendered.contains(">wrap-fix"), "{rendered}");
        assert!(!rendered.contains("> inspect"), "{rendered}");
        assert!(!rendered.contains("* wrap-fix"), "{rendered}");
    }

    #[test]
    fn session_switcher_stays_under_the_composer_like_slash_options() {
        let switcher = SessionSwitcher {
            candidates: vec![InteractiveSessionCandidate {
                id: "visible-session".to_string(),
                label: "visible-session".to_string(),
                preview: "Latest user message: visible content".to_string(),
                is_active: false,
                snapshot_token: [0; 32],
            }],
            selected: 0,
            omitted: 0,
            confirming: false,
            exact_id: String::new(),
            error: None,
        };
        let band = InteractionBand::Sessions {
            switcher: &switcher,
            active: "current-session",
        };
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut viewport = TranscriptViewport::default();
        let welcome = StartupWelcome::fixture();
        let composer = Composer::from_text("/session");
        terminal
            .draw(|frame| {
                render_session_activities(
                    frame,
                    &TuiChrome::fixture(),
                    &[ActivityEntry::new(
                        ActivityKind::User,
                        "",
                        "hello conversation",
                    )],
                    &composer,
                    None,
                    None,
                    false,
                    &mut viewport,
                    TuiFocus::Composer,
                    None,
                    0,
                    None,
                    None,
                    &welcome,
                    Some(&band),
                    None,
                    None,
                )
            })
            .expect("render under-composer sessions");
        let rows = buffer_rows(&terminal);
        let conversation = row_index_containing(&rows, "hello conversation").expect("conversation");
        let prompt = row_index_containing(&rows, "> /session").expect("composer");
        let option = row_index_containing(&rows, "visible-session").expect("session option");
        assert!(conversation < prompt, "{rows:?}");
        assert!(prompt < option, "{rows:?}");
        let slash = rows[prompt].find('/').expect("composer slash");
        let option_start = rows[option].find('v').expect("option text");
        assert_eq!(
            slash, option_start,
            "prompt={:?} option={:?}",
            rows[prompt], rows[option]
        );
    }

    #[test]
    fn completion_and_session_overlays_render_on_small_terminals() {
        let mut completion = CompletionMenu::default();
        completion.sync("/");
        let switcher = SessionSwitcher {
            candidates: vec![InteractiveSessionCandidate {
                id: "small-session".to_string(),
                label: "small-session".to_string(),
                preview: "bounded preview".to_string(),
                is_active: true,
                snapshot_token: [0; 32],
            }],
            selected: 0,
            omitted: 0,
            confirming: true,
            exact_id: String::new(),
            error: None,
        };

        for (width, height) in [(20, 6), (40, 10)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("small test terminal");
            terminal
                .draw(|frame| {
                    render_session_view_with_completion(
                        frame,
                        "sess small-session",
                        "idle",
                        "Session: small-session",
                        &Composer::default(),
                        Some(&completion),
                        None,
                        false,
                    );
                })
                .expect("render completion on small terminal");
            terminal
                .draw(|frame| {
                    render_session_switcher(frame, frame.area(), &switcher, "small-session")
                })
                .expect("render switcher on small terminal");
        }
    }

    #[test]
    fn renders_every_agent_lifecycle_event() {
        let mut output = LiveOutput::default();
        let read_invocation = crate::tools::ToolInvocationId::new();
        let terminal_invocation = crate::tools::ToolInvocationId::new();
        let events = vec![
            StreamEvent::StateTransition {
                state: "planning".to_string(),
            },
            StreamEvent::PlanGenerated {
                step_count: 2,
                steps: vec!["inspect wrap".to_string(), "write tests".to_string()],
            },
            StreamEvent::Content("Working".to_string()),
            StreamEvent::ToolCallChunk {
                invocation_id: read_invocation,
                index: 0,
                name: Some("read_file".to_string()),
                arguments: Some("{}".to_string()),
            },
            StreamEvent::ApprovalRequired {
                tool_name: "run_terminal".to_string(),
            },
            StreamEvent::QuestionRequired {
                question: "Choose a mode".to_string(),
                options: vec!["plan".to_string(), "execute".to_string()],
            },
            StreamEvent::ToolStarted {
                invocation_id: read_invocation,
                tool_name: "read_file".to_string(),
            },
            StreamEvent::TerminalOutput {
                invocation_id: terminal_invocation,
                tool_name: "run_terminal".to_string(),
                stream: "stderr".to_string(),
                chunk: "building\n".to_string(),
                background_task_id: None,
            },
            StreamEvent::ToolCompleted {
                invocation_id: read_invocation,
                tool_name: "read_file".to_string(),
                success: true,
                output: Some(json!({"content": "nib"})),
                error: None,
            },
            StreamEvent::ToolCompleted {
                invocation_id: terminal_invocation,
                tool_name: "run_terminal".to_string(),
                success: false,
                output: None,
                error: Some("exit 1".to_string()),
            },
            StreamEvent::Compression {
                before_tokens: 1_000,
                after_tokens: 250,
                summarized_through: 4,
            },
            StreamEvent::Reconciled {
                outcome: "completed".to_string(),
            },
            StreamEvent::Failure {
                failure: crate::llm::LlmError::new(
                    crate::llm::LlmErrorClass::Authentication,
                    crate::llm::LlmErrorPhase::HttpResponse,
                    crate::llm::RetryDisposition::NotRetryable,
                    crate::llm::LlmErrorMetadata::new(
                        "openai",
                        "responses",
                        Some("gpt-test"),
                        Some(401),
                        &[],
                    ),
                    "credential rejected",
                ),
                session_id: Some("failure-session".to_string()),
            },
            StreamEvent::End("stop".to_string()),
        ];

        for event in events {
            output.apply(event, &[]);
        }

        for expected in [
            "[state] planning",
            "[plan] generated 2 steps",
            "Working\n[tool call] read_file",
            "[approval required] run_terminal",
            "[question] Choose a mode (options: plan | execute)",
            "[tool started] read_file",
            "[terminal stderr] run_terminal: building",
            "[tool completed] read_file: ok - {\"content\":\"nib\"}",
            "[tool completed] run_terminal: failed - exit 1",
            "[compression] 1000 -> 250 tokens; summarized through message 4",
            "[reconciled] completed",
            "LLM request failed [LLM-AUTH]",
            "Provider: openai (responses), model: gpt-test",
            "Action: Refresh this provider's credential with `nib auth`, then retry.",
            "Session: failure-session",
            "[stream ended] stop",
        ] {
            assert!(output.text.contains(expected), "missing: {expected}");
        }
    }

    #[test]
    fn timeline_ignores_intermediate_plan_reconciliation_and_deduplicates_end() {
        let mut timeline = ActiveTimeline::default();
        for outcome in ["step_completed", "verification_recovery"] {
            timeline.apply_event(StreamEvent::Reconciled {
                outcome: outcome.to_string(),
            });
        }
        assert!(timeline.reconciled_terminal.is_none());
        assert!(timeline.activities.is_empty());

        timeline.apply_event(StreamEvent::Reconciled {
            outcome: "completed".to_string(),
        });
        timeline.apply_event(StreamEvent::End("completed".to_string()));
        assert_eq!(
            timeline.reconciled_terminal,
            Some(InteractionTerminalOutcome::Completed)
        );
        assert_eq!(timeline.activities.len(), 1);
        assert_eq!(timeline.activities[0].kind, ActivityKind::Reconcile);

        timeline.bind_run(Some("next-run".to_string()));
        timeline.apply_event(StreamEvent::End("local_error".to_string()));
        assert_eq!(timeline.activities.len(), 2);
        assert_eq!(timeline.activities[1].kind, ActivityKind::System);
        assert_eq!(timeline.activities[1].title, "local_error");
        assert_eq!(
            timeline.reconciled_terminal,
            Some(InteractionTerminalOutcome::Failed)
        );
    }

    #[test]
    fn timeline_preserves_a_final_error_after_successful_reconciliation() {
        let mut timeline = ActiveTimeline::default();
        timeline.apply_event(StreamEvent::Reconciled {
            outcome: "completed".to_string(),
        });
        timeline.apply_event(StreamEvent::End("local_error".to_string()));

        assert_eq!(timeline.activities.len(), 2);
        assert_eq!(timeline.activities[1].title, "local_error");
        assert!(timeline.live.text.contains("[stream ended] local_error"));
        assert_eq!(
            timeline.reconciled_terminal,
            Some(InteractionTerminalOutcome::Failed)
        );
        assert_eq!(
            tui_run_state(
                false,
                timeline.live.state.as_deref(),
                timeline.reconciled_terminal
            ),
            InteractionRunState::Failed
        );
    }

    #[test]
    fn test_backend_renders_one_bounded_control_free_failure_detail() {
        const SECRET: &str = "tui/observer+secret";
        let failure = crate::llm::LlmError::new(
            crate::llm::LlmErrorClass::Authentication,
            crate::llm::LlmErrorPhase::HttpResponse,
            crate::llm::RetryDisposition::NotRetryable,
            crate::llm::LlmErrorMetadata::new(
                "openai",
                "responses",
                Some("fixture-model"),
                Some(401),
                &[SECRET.to_string()],
            ),
            format!(
                "{SECRET} <red>[bold] REMOTE_TUI_SENTINEL \u{1b}[31m {}",
                "x".repeat(MAX_LIVE_OUTPUT_BYTES)
            ),
        );
        let mut timeline = ActiveTimeline {
            session_id: "tui-failure-session".to_string(),
            ..ActiveTimeline::default()
        };
        timeline.apply_event(StreamEvent::Failure {
            failure,
            session_id: Some("tui-failure-session".to_string()),
        });
        let timeline_text = timeline.rendered_text();

        assert!(timeline_text.len() <= 1_024, "{}", timeline_text.len());
        assert_eq!(timeline_text.matches("LLM request failed").count(), 1);
        assert!(timeline_text.contains("LLM request failed [LLM-AUTH]"));
        assert!(timeline_text.contains("Session: tui-failure-session"));
        for forbidden in [SECRET, "REMOTE_TUI_SENTINEL", "[red]", "[bold]", "\u{1b}"] {
            assert!(!timeline_text.contains(forbidden), "{timeline_text}");
        }
        assert!(timeline_text.chars().all(|character| {
            !character.is_control() || matches!(character, '\n' | '\r' | '\t')
        }));

        let backend = TestBackend::new(48, 12);
        let mut terminal = Terminal::new(backend).expect("failure detail terminal");
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace  ·  sess tui-failure-session",
                    "idle  ·  openai/fixture-model",
                    &timeline_text,
                    &Composer::default(),
                    None,
                    None,
                )
            })
            .expect("render failure detail");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.content.len(), 48 * 12);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        // The deliberately short viewport is bottom-aligned, so the heading may
        // scroll out while the actionable tail remains visible. The full bounded
        // timeline above owns the exactly-once heading assertion.
        assert!(
            rendered.contains("Provider: openai (responses)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("HTTP: 401; retry: not retryable"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Session: tui-failure-session"),
            "{rendered}"
        );
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains("REMOTE_TUI_SENTINEL"));
    }

    #[test]
    fn live_output_retains_a_bounded_utf8_tail() {
        let mut output = LiveOutput::default();
        for _ in 0..(MAX_LIVE_OUTPUT_BYTES / 8_192 + 2) {
            output.apply(StreamEvent::Content("é".repeat(4_096)), &[]);
        }

        assert!(output.text.len() <= MAX_LIVE_OUTPUT_BYTES);
        assert!(output.text.starts_with(OMITTED_OUTPUT_MARKER));
        assert!(output.text.is_char_boundary(output.text.len()));
        assert!(output.text.ends_with('é'));
    }

    #[test]
    fn live_output_replaces_terminal_active_and_bidi_controls() {
        let mut output = LiveOutput::default();
        output.apply(
            StreamEvent::Content("safe\u{1b}[2J\rreplace\u{202e}tail".to_string()),
            &[],
        );

        assert!(!output.text.contains('\u{1b}'));
        assert!(!output.text.contains('\r'));
        assert!(!output.text.contains('\u{202e}'));
        assert!(output.text.contains("safe"));
        assert!(output.text.contains("tail"));
    }

    #[test]
    fn exact_run_identity_ignores_prior_and_idle_run_events() {
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(4);
        stream_tx
            .try_send(SessionStreamEvent {
                session_id: "active-session".to_string(),
                run_id: "old-run".to_string(),
                event: StreamEvent::Content("must not leak".to_string()),
            })
            .expect("old event");
        stream_tx
            .try_send(SessionStreamEvent {
                session_id: "active-session".to_string(),
                run_id: "active-run".to_string(),
                event: StreamEvent::Content("visible active output".to_string()),
            })
            .expect("active event");
        let mut timeline = ActiveTimeline {
            session_id: "active-session".to_string(),
            active_run_id: Some("active-run".to_string()),
            ..ActiveTimeline::default()
        };

        drain_stream_events(&mut stream_rx, &mut timeline);

        assert_eq!(timeline.live.text, "visible active output");

        timeline.active_run_id = None;
        stream_tx
            .try_send(SessionStreamEvent {
                session_id: "active-session".to_string(),
                run_id: "active-run".to_string(),
                event: StreamEvent::Content("must not mutate idle timeline".to_string()),
            })
            .expect("idle late event");
        drain_stream_events(&mut stream_rx, &mut timeline);
        assert_eq!(timeline.live.text, "visible active output");
    }

    #[test]
    fn exact_run_identity_worker_error_renders_only_safe_terminal_outcome() {
        let event = safe_agent_error_stream_event(
            "PRIVATE_LOCAL_ERROR_SENTINEL\u{1b}[2J\nprovider payload",
        );
        assert_eq!(event, StreamEvent::End("local_error".to_string()));
    }

    #[test]
    fn approval_modal_only_resolves_on_explicit_decision() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task test"}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            reply_tx,
        ));

        assert!(handle_approval_key(&mut pending, KeyCode::Char('x')));
        assert!(pending.is_some());
        assert!(reply_rx.try_recv().is_err());

        assert!(handle_approval_key(&mut pending, KeyCode::Backspace));
        assert!(handle_approval_key(&mut pending, KeyCode::Char('Y')));
        assert!(handle_approval_key(&mut pending, KeyCode::Enter));
        let decision = reply_rx.try_recv().unwrap();
        assert!(decision.granted);
        assert_eq!(decision.source, "user");
    }

    #[test]
    fn approval_enter_and_number_keys_are_explicit_choices() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task test"}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            reply_tx,
        ));
        assert!(handle_approval_key(&mut pending, KeyCode::Enter));
        assert!(!reply_rx.try_recv().unwrap().granted);

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task test"}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            reply_tx,
        ));
        assert!(handle_approval_key(&mut pending, KeyCode::Up));
        assert!(handle_approval_key(&mut pending, KeyCode::Enter));
        assert!(reply_rx.try_recv().unwrap().granted);

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task test"}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            reply_tx,
        ));
        assert!(handle_approval_key(&mut pending, KeyCode::Enter));
        assert!(!reply_rx.try_recv().unwrap().granted);

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task test"}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            reply_tx,
        ));
        assert!(handle_approval_key(&mut pending, KeyCode::Down));
        assert!(pending.as_ref().is_some_and(|req| req.selected_option == 2));
        assert!(handle_approval_key(&mut pending, KeyCode::Enter));
        assert!(pending.as_ref().is_some_and(|req| req.details_open));
        assert!(reply_rx.try_recv().is_err());
        assert!(handle_approval_key(&mut pending, KeyCode::Esc));
        assert!(pending.as_ref().is_some_and(|req| !req.details_open));
        assert!(handle_approval_key(&mut pending, KeyCode::Up));
        assert!(handle_approval_key(&mut pending, KeyCode::Enter));
        assert!(!reply_rx.try_recv().unwrap().granted);
    }

    #[test]
    fn approval_modal_sends_denial() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "apply_patch".to_string(),
                arguments: json!({}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            reply_tx,
        ));

        assert!(handle_approval_key(&mut pending, KeyCode::Char('n')));
        assert!(handle_approval_key(&mut pending, KeyCode::Enter));
        let decision = reply_rx.try_recv().unwrap();
        assert!(!decision.granted);
        assert_eq!(decision.source, "denied");
    }

    #[test]
    fn approval_details_are_labeled_scrollable_and_non_authorizing() {
        let details = vec![format!("Command: {} END", "界".repeat(80))];
        let first = approval_detail_view_rows(&details, 20, 0);
        let later = approval_detail_view_rows(&details, 20, 4);
        assert!(first.iter().any(|row| row.contains("Approval details")));
        assert!(first.iter().any(|row| row.contains("more rows")));
        assert_ne!(first, later);
        let end = approval_detail_view_rows(&details, 20, usize::MAX);
        assert!(end.iter().any(|row| row.contains("END")), "{end:?}");

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task verify"}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            reply_tx,
        ));
        pending.as_mut().expect("approval").selected_option = 2;
        assert!(handle_approval_key(&mut pending, KeyCode::Enter));
        assert!(pending.as_ref().is_some_and(|req| req.details_open));
        assert!(reply_rx.try_recv().is_err());
        assert!(handle_approval_key(&mut pending, KeyCode::Esc));
        assert!(pending.as_ref().is_some_and(|req| !req.details_open));
        assert!(reply_rx.try_recv().is_err());
    }

    #[test]
    fn approval_consumes_input_before_question_and_completion_layers() {
        let (approval_tx, mut approval_rx) = oneshot::channel();
        let mut approval = Some(approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            approval_tx,
        ));
        let (question_tx, mut question_rx) = oneshot::channel();
        let mut question = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Still here?".to_string(),
            options: vec!["yes".to_string()],
            reply: question_tx,
        }));
        let composer = Composer::from_text("/");
        let mut completion = CompletionMenu::default();
        completion.sync(&composer.input);

        assert!(handle_pending_interaction_key(
            &mut approval,
            &mut question,
            KeyCode::Char('s')
        ));
        assert!(approval.is_some());
        assert!(approval_rx.try_recv().is_err());
        assert_eq!(composer.input, "/");

        assert!(handle_pending_interaction_key(
            &mut approval,
            &mut question,
            KeyCode::Esc
        ));
        assert!(approval.is_none());
        assert!(!approval_rx.try_recv().expect("approval reply").granted);
        assert!(question.is_some());
        assert!(question_rx.try_recv().is_err());
        assert_eq!(composer.input, "/");
        assert!(completion.is_open());

        assert!(handle_pending_interaction_key(
            &mut approval,
            &mut question,
            KeyCode::Char('y')
        ));
        assert!(handle_pending_interaction_key(
            &mut approval,
            &mut question,
            KeyCode::Enter
        ));
        assert_eq!(
            question_rx.try_recv().expect("question reply"),
            crate::agent::QuestionOutcome::Answered("y".to_string())
        );
        assert_eq!(composer.input, "/");
    }

    #[test]
    fn modal_arriving_after_poll_refreshes_before_the_key_is_dispatched() {
        let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
        let (_question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        approval_tx
            .send(approval_request(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "run_terminal".to_string(),
                    arguments: json!({}),
                    session_id: None,
                    project_root: None,
                },
                PermissionLevel::Destructive,
                reply_tx,
            ))
            .expect("approval arrives while terminal poll is blocked");
        let mut pending_approval = None;
        let mut pending_question = None;

        refresh_pending_interactions(
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
        );
        let state = tui_interaction_state(
            pending_approval.is_some(),
            pending_question.is_some(),
            false,
            None,
            false,
            false,
            InteractionRunState::Running,
        );
        assert_eq!(
            reduce_interaction(&state, InteractionInput::SteerCurrent("private steering")),
            InteractionReduction::Consumed(InteractionConsumer::Approval)
        );
        assert!(handle_pending_interaction_key(
            &mut pending_approval,
            &mut pending_question,
            KeyCode::Char('s'),
        ));
        assert!(pending_approval.is_some());
        assert!(reply_rx.try_recv().is_err());
    }

    #[test]
    fn fixed_status_projects_authoritative_terminal_state_after_worker_join() {
        for (terminal, expected) in [
            (
                InteractionTerminalOutcome::Completed,
                InteractionRunState::Completed,
            ),
            (
                InteractionTerminalOutcome::Cancelled,
                InteractionRunState::Cancelled,
            ),
            (
                InteractionTerminalOutcome::Failed,
                InteractionRunState::Failed,
            ),
            (
                InteractionTerminalOutcome::WaitingForInput,
                InteractionRunState::Failed,
            ),
        ] {
            assert_eq!(tui_run_state(false, None, Some(terminal)), expected);
        }
        assert_eq!(tui_run_state(false, None, None), InteractionRunState::Idle);
        assert_eq!(
            tui_run_state(
                true,
                Some("reconciliation"),
                Some(InteractionTerminalOutcome::Completed),
            ),
            InteractionRunState::Reconciling,
            "a bound worker remains authoritative until it is joined"
        );
    }

    #[test]
    fn shared_interaction_reducer_maps_to_tui_renderer_layers() {
        let browsing = SessionSwitcher {
            candidates: Vec::new(),
            selected: 0,
            omitted: 0,
            confirming: false,
            exact_id: String::new(),
            error: None,
        };
        let confirming = SessionSwitcher {
            confirming: true,
            ..browsing.clone()
        };
        let cases = [
            (
                true,
                true,
                true,
                Some(&confirming),
                true,
                true,
                InteractionLayer::Approval,
            ),
            (
                false,
                true,
                true,
                Some(&confirming),
                true,
                true,
                InteractionLayer::Question,
            ),
            (
                false,
                false,
                true,
                Some(&confirming),
                true,
                true,
                InteractionLayer::SessionConfirmation,
            ),
            (
                false,
                false,
                false,
                Some(&confirming),
                true,
                true,
                InteractionLayer::SessionConfirmation,
            ),
            (
                false,
                false,
                false,
                Some(&browsing),
                false,
                true,
                InteractionLayer::SessionSwitcher,
            ),
            (
                false,
                false,
                false,
                None,
                true,
                true,
                InteractionLayer::HistorySearch,
            ),
            (
                false,
                false,
                false,
                None,
                false,
                true,
                InteractionLayer::Completion,
            ),
            (
                false,
                false,
                false,
                None,
                false,
                false,
                InteractionLayer::Composer,
            ),
        ];

        for (approval, question, model, switcher, history, completion, expected) in cases {
            assert_eq!(
                active_interaction_layer(approval, question, model, switcher, history, completion,),
                expected
            );
        }
    }

    #[test]
    fn question_modal_submits_typed_response() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Branch name?".to_string(),
            options: vec![],
            reply: reply_tx,
        }));

        assert!(!handle_question_key(&mut pending, KeyCode::Char('m')));
        assert!(!handle_question_key(&mut pending, KeyCode::Char('a')));
        assert!(!handle_question_key(&mut pending, KeyCode::Char('x')));
        assert!(!handle_question_key(&mut pending, KeyCode::Backspace));
        assert!(!handle_question_key(&mut pending, KeyCode::Char('i')));
        assert!(!handle_question_key(&mut pending, KeyCode::Char('n')));
        assert!(handle_question_key(&mut pending, KeyCode::Enter));

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            crate::agent::QuestionOutcome::Answered("main".to_string())
        );
        assert!(pending.is_none());
    }

    #[test]
    fn question_modal_paste_preserves_unicode_multiline_and_filters_controls() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut question = PendingQuestion::new(TuiQuestionRequest {
            question: "Describe the result".to_string(),
            options: vec!["short".to_string()],
            reply: reply_tx,
        });

        paste_question_answer(&mut question, "first🙂\r\nsecond\tvalue\u{1b}");
        assert_eq!(question.response, "first🙂\nsecond    value");
        assert!(question
            .error
            .as_deref()
            .is_some_and(|status| status.contains("unsafe paste control")));
        let mut pending = Some(question);
        assert!(handle_question_key(&mut pending, KeyCode::Enter));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            crate::agent::QuestionOutcome::Answered("first🙂\nsecond    value".to_string())
        );
    }

    #[test]
    fn f2_command_overlay_requires_a_prompt_and_preserves_question_draft() {
        let (reply_tx, _reply_rx) = oneshot::channel();
        let question = PendingQuestion::new(TuiQuestionRequest {
            question: "Which target?".to_string(),
            options: vec!["alpha".to_string()],
            reply: reply_tx,
        });
        let mut overlay = None;
        assert!(!open_prompt_command_overlay(
            KeyCode::F(2),
            false,
            &mut overlay
        ));
        assert!(overlay.is_none(), "idle F2 must be a no-op");

        let before = question.response.clone();
        assert!(open_prompt_command_overlay(
            KeyCode::F(2),
            true,
            &mut overlay
        ));
        assert_eq!(overlay.take().as_deref(), Some(""));
        assert_eq!(question.response, before);
        assert_eq!(
            question.response, before,
            "Escape/close preserves the draft"
        );
    }

    #[test]
    fn recovered_question_modal_persists_before_it_closes() {
        let (_directory, store, session_id, invocation_id) = recoverable_question_session();
        let (reply_tx, reply_rx) = oneshot::channel();
        drop(reply_rx);
        let mut pending = Some(PendingQuestion::recovered(
            TuiQuestionRequest {
                question: "Which target?".to_string(),
                options: vec!["alpha".to_string(), "beta".to_string()],
                reply: reply_tx,
            },
            RecoveredQuestionTarget {
                store: store.clone(),
                session_id: session_id.clone(),
                invocation_id: invocation_id.to_string(),
            },
        ));

        assert!(!handle_question_key(&mut pending, KeyCode::Char('2')));
        assert!(handle_question_key(&mut pending, KeyCode::Enter));
        assert!(pending.is_none());
        let persisted = store.load(&session_id).expect("answered session");
        assert_eq!(
            persisted.clarifications[0].status,
            crate::session::ClarificationStatus::Answered
        );
        assert_eq!(persisted.clarifications[0].answer.as_deref(), Some("beta"));
    }

    #[test]
    fn question_modal_accepts_q_in_free_form_response() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Search term?".to_string(),
            options: vec![],
            reply: reply_tx,
        }));
        let mut approval = None;

        for character in "query".chars() {
            assert!(handle_pending_interaction_key(
                &mut approval,
                &mut pending,
                KeyCode::Char(character)
            ));
        }
        assert!(handle_question_key(&mut pending, KeyCode::Enter));

        assert_eq!(
            reply_rx.try_recv().unwrap(),
            crate::agent::QuestionOutcome::Answered("query".to_string())
        );
        assert!(pending.is_none());
    }

    #[test]
    fn question_modal_submits_selected_option() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Mode?".to_string(),
            options: vec!["plan".to_string(), "execute".to_string()],
            reply: reply_tx,
        }));

        assert!(!handle_question_key(&mut pending, KeyCode::Tab));
        assert!(!handle_question_key(&mut pending, KeyCode::Down));
        assert!(!handle_question_key(&mut pending, KeyCode::Down));
        assert!(handle_question_key(&mut pending, KeyCode::Enter));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            crate::agent::QuestionOutcome::Answered("execute".to_string())
        );
    }

    #[test]
    fn question_number_keys_choose_a_labeled_option() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Mode?".to_string(),
            options: vec!["plan".to_string(), "execute".to_string()],
            reply: reply_tx,
        }));
        assert!(!handle_question_key(&mut pending, KeyCode::Char('1')));
        assert!(handle_question_key(&mut pending, KeyCode::Enter));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            crate::agent::QuestionOutcome::Answered("plan".to_string())
        );
        assert!(pending.is_none());

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Mode?".to_string(),
            options: vec!["plan".to_string(), "execute".to_string()],
            reply: reply_tx,
        }));
        assert!(!handle_question_key(&mut pending, KeyCode::Char('y')));
        assert!(handle_question_key(&mut pending, KeyCode::Enter));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            crate::agent::QuestionOutcome::Answered("y".to_string())
        );

        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Mode?".to_string(),
            options: vec!["plan".to_string(), "execute".to_string()],
            reply: reply_tx,
        }));
        handle_question_key(&mut pending, KeyCode::Tab);
        handle_question_key(&mut pending, KeyCode::Tab);
        assert!(handle_question_key(&mut pending, KeyCode::Enter));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            crate::agent::QuestionOutcome::LeftUnanswered
        );
    }

    #[test]
    fn question_modal_reports_cancellation() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Continue?".to_string(),
            options: vec!["yes".to_string(), "no".to_string()],
            reply: reply_tx,
        }));

        assert!(handle_question_key(&mut pending, KeyCode::Esc));
        assert_eq!(
            reply_rx.try_recv().unwrap(),
            crate::agent::QuestionOutcome::LeftUnanswered
        );
    }

    #[test]
    fn question_handler_round_trips_ui_response() {
        let (request_tx, request_rx) = mpsc::channel();
        let handler = TuiQuestionHandler { tx: request_tx };
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(crate::agent::QuestionHandler::ask(
                &handler,
                "Mode?",
                &["plan".to_string(), "execute".to_string()],
            ))
        });

        let request = request_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(request.question, "Mode?");
        assert_eq!(request.options, ["plan", "execute"]);
        request
            .reply
            .send(crate::agent::QuestionOutcome::Answered(
                "execute".to_string(),
            ))
            .unwrap();

        assert_eq!(handle.join().unwrap(), Ok("execute".to_string()));
    }

    #[test]
    fn tui_shutdown_cancels_and_joins_a_worker_blocked_on_approval() {
        let directory = tempdir().expect("tempdir");
        save_config(directory.path(), &mock_config()).expect("save mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let goal = "ask a question";
        let mut session = store.create_session();
        session.plan = Some(crate::session::Plan::new(
            goal,
            vec![crate::session::PlanStep {
                description: "ask a question".to_string(),
                status: "Pending".to_string(),
                outcome: None,
                attempts: 0,
                updated_at: None,
                verification_obligations: Vec::new(),
                content_generation: 0,
            }],
        ));
        store.save(&mut session).expect("save pending plan");

        let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
        let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
        let mut worker = Some(
            spawn_tui_agent_worker(
                TuiAgentProfileScope {
                    project_root: directory.path().to_path_buf(),
                    profile_id: "default".to_string(),
                    sessions_dir: store.sessions_dir().to_path_buf(),
                },
                session.id.clone(),
                goal.to_string(),
                InteractiveAgentMode::Execute,
                approval_tx,
                question_tx,
                stream_tx,
            )
            .expect("spawn TUI worker"),
        );
        let question = question_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("worker reached a blocking question");
        let mut pending_approval = None;
        let mut pending_question = Some(PendingQuestion::new(question));
        let mut timeline = ActiveTimeline::load(&store, &session.id).expect("active timeline");
        timeline.active_run_id = worker.as_ref().map(|worker| worker.run_id.clone());

        shutdown_agent_worker(
            &mut worker,
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
            &mut stream_rx,
            &mut timeline,
        )
        .expect("cancel and join worker");

        assert!(worker.is_none(), "worker handle must be joined and cleared");
        assert!(timeline.active_run_id.is_none());
        assert_eq!(
            timeline.reconciled_terminal,
            Some(InteractionTerminalOutcome::Cancelled)
        );
        assert!(pending_approval.is_none());
        assert!(pending_question.is_none());
        let persisted = store.load(&session.id).expect("cancelled session");
        let plan = persisted.plan.expect("generated plan");
        assert_eq!(plan.outcome.as_deref(), Some("cancelled_by_user"));
        assert_eq!(plan.steps[plan.current_step_index].status, "Cancelled");
        assert_eq!(
            plan.steps[plan.current_step_index].outcome.as_deref(),
            Some("cancelled_by_user")
        );
        assert!(timeline
            .live
            .text
            .contains("[reconciled] cancelled_by_user"));
        assert!(!timeline.live.text.contains("[stream ended]"));
        assert_eq!(
            timeline
                .activities
                .iter()
                .filter(|entry| entry.kind == ActivityKind::Reconcile)
                .count(),
            1
        );
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    fn tui_shutdown_rejects_modal_requests_published_after_initial_cleanup() {
        let session_id = "late-modal-session";
        let run_id = "0123456789abcdef0123456789abcdef";
        let cancellation = CancellationSignal::new();
        let worker_cancellation = cancellation.clone();
        let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
        let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
        let (initial_reply_tx, initial_reply_rx) = oneshot::channel();
        let (approval_reply_tx, mut approval_reply_rx) = oneshot::channel();
        let (question_reply_tx, mut question_reply_rx) = oneshot::channel();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(1);
        stream_tx
            .try_send(SessionStreamEvent {
                session_id: session_id.to_string(),
                run_id: run_id.to_string(),
                event: StreamEvent::StateTransition {
                    state: "planning".to_string(),
                },
            })
            .expect("fill the stream before shutdown");

        let handle = std::thread::spawn(move || {
            assert_eq!(
                initial_reply_rx
                    .blocking_recv()
                    .expect("early question rejection"),
                crate::agent::QuestionOutcome::Cancelled
            );
            assert!(worker_cancellation.is_cancelled());
            // This send cannot finish until shutdown drains the full stream. That
            // drain follows early modal cleanup, so both requests below are late
            // by construction, without a timing sleep or production test hook.
            stream_tx
                .blocking_send(SessionStreamEvent {
                    session_id: session_id.to_string(),
                    run_id: run_id.to_string(),
                    event: StreamEvent::Reconciled {
                        outcome: "cancelled_by_user".to_string(),
                    },
                })
                .expect("shutdown reached its post-cleanup stream drain");
            approval_tx
                .send(approval_request(
                    ToolCall {
                        invocation_id: crate::tools::ToolInvocationId::new(),
                        tool_name: "run_terminal".to_string(),
                        arguments: json!({}),
                        session_id: Some(session_id.to_string()),
                        project_root: None,
                    },
                    PermissionLevel::Destructive,
                    approval_reply_tx,
                ))
                .expect("publish late approval");
            question_tx
                .send(TuiQuestionRequest {
                    question: "This cancelled question must not reopen".to_string(),
                    options: Vec::new(),
                    reply: question_reply_tx,
                })
                .expect("publish late question");
        });
        let mut worker = Some(TuiAgentWorker {
            run_id: run_id.to_string(),
            mode: InteractiveAgentMode::Execute,
            cancellation,
            steering: None,
            handle: Some(handle),
        });
        let mut pending_approval = None;
        let mut pending_question = Some(PendingQuestion::new(TuiQuestionRequest {
            question: "Initial question".to_string(),
            options: Vec::new(),
            reply: initial_reply_tx,
        }));
        let mut timeline = ActiveTimeline {
            session_id: session_id.to_string(),
            active_run_id: Some(run_id.to_string()),
            ..ActiveTimeline::default()
        };

        shutdown_agent_worker(
            &mut worker,
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
            &mut stream_rx,
            &mut timeline,
        )
        .expect("cancel and join the late producer");
        assert!(worker.is_none());
        assert!(timeline.active_run_id.is_none());
        assert_eq!(
            timeline.reconciled_terminal,
            Some(InteractionTerminalOutcome::Cancelled)
        );

        refresh_pending_interactions(
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
        );
        assert!(
            pending_approval.is_none(),
            "cancelled approval must not reopen"
        );
        assert!(
            pending_question.is_none(),
            "cancelled question must not reopen"
        );
        assert!(
            !approval_reply_rx
                .try_recv()
                .expect("late approval rejected")
                .granted
        );
        assert_eq!(
            question_reply_rx
                .try_recv()
                .expect("late question rejected"),
            crate::agent::QuestionOutcome::Cancelled
        );
        assert_eq!(
            active_interaction_layer(
                pending_approval.is_some(),
                pending_question.is_some(),
                false,
                None,
                false,
                false,
            ),
            InteractionLayer::Composer
        );
    }

    #[test]
    fn tui_shutdown_times_out_an_unresponsive_worker_without_blocking_restoration() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let session = store.create_session();
        let cancellation = CancellationSignal::new();
        let run_id = "0123456789abcdef0123456789abcdef";
        store
            .record_event(&session.id, "run_started", json!({"run_id": run_id}))
            .expect("run start");
        let (steering, steering_receiver) = crate::agent::exact_run_steering_channel(
            store.clone(),
            session.id.clone(),
            run_id,
            "tui",
        )
        .expect("steering channel");
        crate::agent::r#loop::bind_exact_run_steering_receiver(
            &store,
            &session.id,
            run_id,
            &steering_receiver,
        )
        .expect("install exact receiver");
        steering
            .submit("pending credential sk-private-timeout-sentinel")
            .expect("accepted pending steering");
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let steering_receiver = steering_receiver;
            let _ = release_rx.recv();
            drop(steering_receiver);
            let _ = done_tx.send(());
        });
        let mut worker = Some(TuiAgentWorker {
            run_id: run_id.to_string(),
            mode: InteractiveAgentMode::Execute,
            cancellation,
            steering: Some(steering),
            handle: Some(handle),
        });
        let (_approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
        let (_question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
        let (_stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(1);
        let mut pending_approval = None;
        let mut pending_question = None;
        let mut timeline = ActiveTimeline::load(&store, &session.id).expect("timeline");
        timeline.active_run_id = Some(run_id.to_string());

        let started = std::time::Instant::now();
        let error = shutdown_agent_worker_with_timeout(
            &mut worker,
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
            &mut stream_rx,
            &mut timeline,
            std::time::Duration::from_millis(25),
        )
        .expect_err("unresponsive worker must time out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(worker.is_none());
        assert!(timeline.active_run_id.is_none());
        release_tx.send(()).expect("release detached test worker");
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("detached test worker exits");
        let persisted = store.load(&session.id).expect("shutdown evidence");
        assert!(persisted.events.iter().any(|event| {
            event.kind == "steering_delivery_failed"
                && event.details["sequence"] == 1
                && event.details["reason"] == "unresponsive_worker_shutdown"
        }));
    }

    #[test]
    fn tui_steering_clears_the_draft_only_after_exact_run_persistence() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let session = store.create_session();
        let run_id = "0123456789abcdef0123456789abcdef";
        let mut rejected = Composer::from_text("retain this draft");
        assert!(submit_tui_steering_draft(None, &mut rejected).is_err());
        assert_eq!(rejected.input, "retain this draft");

        store
            .record_event(&session.id, "run_started", json!({"run_id": run_id}))
            .expect("run admission");
        let (steering, receiver) = crate::agent::exact_run_steering_channel(
            store.clone(),
            session.id.clone(),
            run_id,
            "tui",
        )
        .expect("steering channel");
        crate::agent::r#loop::bind_exact_run_steering_receiver(
            &store,
            &session.id,
            run_id,
            &receiver,
        )
        .expect("install exact receiver");
        let worker = TuiAgentWorker {
            run_id: run_id.to_string(),
            mode: InteractiveAgentMode::Execute,
            cancellation: CancellationSignal::new(),
            steering: Some(steering),
            handle: None,
        };
        let mut accepted = Composer::from_text("change the verification approach");
        let (text, sequence) = submit_tui_steering_draft(Some(&worker), &mut accepted)
            .expect("durable exact-run steering");

        assert_eq!(text, "change the verification approach");
        assert_eq!(sequence, 1);
        assert!(accepted.input.is_empty());
        assert_eq!(
            accepted.history.entries().last().map(String::as_str),
            Some("change the verification approach")
        );
        let persisted = store.load(&session.id).expect("persisted steering");
        assert!(persisted.events.iter().any(|event| {
            event.kind == "steering_input"
                && event.details["run_id"] == run_id
                && event.details["source"] == "tui"
                && event.details["text"] == "change the verification approach"
        }));
        let detail = SessionDetail::new(&session.id, Some(&persisted), &[]);
        assert!(!detail.text.contains("change the verification approach"));
        let mut timeline = ActiveTimeline::load(&store, &session.id).expect("safe timeline");
        timeline.push_steering("sk-private-live-steering-sentinel", 2);
        let rendered = timeline.rendered_text();
        assert!(!rendered.contains("change the verification approach"));
        assert!(!rendered.contains("sk-private-live-steering-sentinel"));
        assert!(rendered.contains("instruction persisted for the exact active run"));
    }

    #[test]
    fn tui_history_status_and_detail_redact_configured_encoded_secrets() {
        let directory = tempdir().expect("tempdir");
        let secret = "tui/history-secret";
        let encoded = "dHVpL2hpc3Rvcnktc2VjcmV0";
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        config.llm.providers.insert(
            "inactive-openai".to_string(),
            ProviderEntry {
                model: "safe-model".to_string(),
                api_key: Some(secret.to_string()),
                ..ProviderEntry::default()
            },
        );
        save_nib_config_full(directory.path(), &mut config).expect("sensitive config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let session = store.create_session();
        let unsafe_text =
            format!("raw={secret} json=tui\\/history-secret b64={encoded} \u{1b}[2J\u{202e}");
        store
            .try_append_message(&session.id, "user", &unsafe_text)
            .expect("legacy unsafe history");

        let persisted = store.load(&session.id).expect("persisted session");
        let detail = SessionDetail::new(
            &session.id,
            Some(&persisted),
            store.public_sensitive_values(),
        );
        let mut timeline = ActiveTimeline::load(&store, &session.id).expect("timeline");
        timeline.push_status(unsafe_text);
        let public = format!("{}\n{}", detail.text, timeline.rendered_text());
        for forbidden in [
            secret,
            r"tui\/history-secret",
            encoded,
            "\u{1b}",
            "\u{202e}",
        ] {
            assert!(!public.contains(forbidden), "TUI surface: {public:?}");
        }
        assert!(public.contains("[REDACTED]"));
    }

    #[test]
    fn tui_session_detail_redacts_before_per_item_preview_truncation() {
        let directory = tempdir().expect("tempdir");
        let mut session = SessionStore::at_dir(directory.path().join("sessions"))
            .try_create_session()
            .expect("session");
        let secret = format!("detail/boundary/{}", "s".repeat(256));
        let content = format!(
            "{}{}-safe-tail",
            "p".repeat(MAX_SESSION_DETAIL_ITEM_CHARS - secret.len() / 2),
            secret
        );
        session.messages.push(crate::session::SessionMessage {
            index: 0,
            role: "user".to_string(),
            content,
            timestamp: None,
            attachments: Vec::new(),
        });

        let detail = SessionDetail::new(&session.id, Some(&session), std::slice::from_ref(&secret));
        assert!(detail.text.contains("[REDACTED]"), "{:?}", detail.text);
        assert!(
            !detail.text.contains(&secret[..secret.len() / 2 - 8]),
            "credential prefix survived preview truncation: {:?}",
            detail.text
        );
        assert!(detail.text.len() <= MAX_SESSION_DETAIL_BYTES);
    }

    #[test]
    fn tui_plan_mode_worker_persists_an_unapproved_plan_without_interaction() {
        let directory = tempdir().expect("tempdir");
        save_config(directory.path(), &mock_config()).expect("save mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let session = store.create_session();
        let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
        let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
        let (stream_tx, _stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
        let mut worker = spawn_tui_agent_worker(
            TuiAgentProfileScope {
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: store.sessions_dir().to_path_buf(),
            },
            session.id.clone(),
            "plan the requested work".to_string(),
            InteractiveAgentMode::Plan,
            approval_tx,
            question_tx,
            stream_tx,
        )
        .expect("spawn plan worker");

        while !worker.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        worker.join().expect("join plan worker");

        assert!(approval_rx.try_recv().is_err());
        assert!(question_rx.try_recv().is_err());
        let persisted = store.load(&session.id).expect("planned session");
        let plan = persisted.plan.as_ref().expect("structured plan");
        assert!(plan.is_structured());
        assert!(!plan.approved);
        assert!(!persisted
            .events
            .iter()
            .any(|event| { matches!(event.kind.as_str(), "tool_started" | "tool_completed") }));
        assert!(!persisted
            .events
            .iter()
            .any(|event| event.kind == "approval_required"));
        assert!(persisted.events.iter().any(|event| {
            event.kind == "reconciliation" && event.details["outcome"] == "plan_ready"
        }));
    }

    #[test]
    fn tui_explicit_compaction_is_typed_activity_without_a_synthetic_user_row() {
        let directory = tempdir().expect("tempdir");
        save_config(directory.path(), &mock_config()).expect("save mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let session = store.create_session();
        store
            .try_append_message(&session.id, "user", "retain this context")
            .expect("user message");
        store
            .try_append_message(&session.id, "assistant", "retain this answer")
            .expect("assistant message");
        let before = store.load(&session.id).expect("before compact").messages;
        let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
        let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
        let mut timeline = ActiveTimeline::load(&store, &session.id).expect("timeline");
        timeline.push_status("[compact] requested".to_string());
        let mut worker = spawn_tui_agent_worker(
            TuiAgentProfileScope {
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: store.sessions_dir().to_path_buf(),
            },
            session.id.clone(),
            String::new(),
            InteractiveAgentMode::Compact,
            approval_tx,
            question_tx,
            stream_tx,
        )
        .expect("spawn compact worker");
        timeline.active_run_id = Some(worker.run_id.clone());
        let mut rejected_steering = Composer::from_text("do not steer maintenance");
        assert!(submit_tui_steering_draft(Some(&worker), &mut rejected_steering).is_err());
        assert_eq!(rejected_steering.input, "do not steer maintenance");
        while !worker.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        worker.join().expect("join compact worker");
        drain_stream_events(&mut stream_rx, &mut timeline);

        assert!(approval_rx.try_recv().is_err());
        assert!(question_rx.try_recv().is_err());
        let persisted = store.load(&session.id).expect("compacted session");
        assert_eq!(persisted.messages, before);
        assert!(!persisted.events.iter().any(|event| {
            matches!(
                event.kind.as_str(),
                "steering_channel_bound" | "steering_admission" | "steering_input"
            )
        }));
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| event.kind == "compression")
                .count(),
            1
        );

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let composer = Composer::default();
        let transcript = timeline.rendered_text();
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace · sess compact",
                    "idle · mock/mock-model",
                    &transcript,
                    &composer,
                    None,
                    None,
                )
            })
            .expect("render compact activity");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("compact"));
        assert!(rendered.contains("context_compacted"));
        assert!(!rendered.contains("[user]"));
        assert!(!rendered.contains("explicit context compression"));
    }

    #[test]
    fn tui_background_commands_render_the_same_bounded_safe_projection() {
        let directory = tempdir().expect("tempdir");
        save_config(directory.path(), &mock_config()).expect("save mock config");
        let sessions = SessionStore::for_project(directory.path()).expect("session store");
        let session = sessions.create_session_with_id("tui-background-owner");
        let tasks = crate::daemons::workload::DurableTaskStore::for_project(directory.path())
            .expect("task store");
        for index in 0..=crate::interactive::MAX_INTERACTIVE_BACKGROUND_TASKS {
            tasks
                .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                    id: format!("tui-bg-{index:03}"),
                    command: format!("private-tui-command-{index}"),
                    cwd: directory.path().to_path_buf(),
                    project_root: directory.path().to_path_buf(),
                    profile_id: "default".to_string(),
                    sessions_dir: sessions.sessions_dir().to_path_buf(),
                    session_id: session.id.clone(),
                    execution: crate::config::ExecutionConfig::default(),
                    timeout_secs: 10,
                    max_output_bytes: 1_024,
                })
                .expect("prepare background task");
        }

        for (command, expected_tail) in [
            (
                crate::interactive::InteractiveCommand::Ps,
                "additional tasks omitted",
            ),
            (
                crate::interactive::InteractiveCommand::Stop { task_id: None },
                "/stop <task-id>",
            ),
        ] {
            let InteractiveEffect::Output(output) =
                execute_interactive_command(command, directory.path(), &sessions, &session.id)
                    .expect("background command")
            else {
                panic!("background command must be a local output effect");
            };
            assert_eq!(
                output
                    .lines()
                    .filter(|line| line.trim_start().starts_with("- tui-bg-"))
                    .count(),
                crate::interactive::MAX_INTERACTIVE_BACKGROUND_TASKS
            );
            assert!(output.contains("1 additional tasks omitted"));
            assert!(!output.contains("private-tui-command"));

            let backend = TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let composer = Composer::default();
            terminal
                .draw(|frame| {
                    render_current_session_view(
                        frame,
                        "workspace · sess tui-background-owner",
                        "idle · mock/mock-model",
                        &output,
                        &composer,
                        None,
                        None,
                    )
                })
                .expect("render background command");
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains(expected_tail), "{rendered}");
            assert!(!rendered.contains("private-tui-command"));
            assert!(!rendered.contains("worker_pid"));
        }
    }

    #[test]
    fn repeated_tui_workers_reuse_the_same_active_session() {
        let directory = tempdir().expect("tempdir");
        save_config(directory.path(), &mock_config()).expect("save mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let session = store.create_session();
        let (approval_tx, _approval_rx) = mpsc::channel::<TuiApprovalRequest>();
        let (question_tx, _question_rx) = mpsc::channel::<TuiQuestionRequest>();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);

        for goal in ["first TUI turn", "second TUI turn"] {
            let mut worker = spawn_tui_agent_worker(
                TuiAgentProfileScope {
                    project_root: directory.path().to_path_buf(),
                    profile_id: "default".to_string(),
                    sessions_dir: store.sessions_dir().to_path_buf(),
                },
                session.id.clone(),
                goal.to_string(),
                InteractiveAgentMode::Execute,
                approval_tx.clone(),
                question_tx.clone(),
                stream_tx.clone(),
            )
            .expect("spawn TUI worker");
            let started = std::time::Instant::now();
            while !worker.is_finished() && started.elapsed() < std::time::Duration::from_secs(10) {
                while stream_rx.try_recv().is_ok() {}
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            worker.join().expect("join TUI worker");
            while stream_rx.try_recv().is_ok() {}
        }

        let persisted = store.load(&session.id).expect("reused session");
        for goal in ["first TUI turn", "second TUI turn"] {
            assert!(persisted
                .messages
                .iter()
                .any(|message| message.role == "user" && message.content == goal));
        }
    }

    #[test]
    fn approval_prompt_states_the_command_without_a_metadata_dump() {
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({
                "command": "git status --short --branch && task check",
                "cwd": "/home/e/work/projects/nib",
                "background": false
            }),
            session_id: None,
            project_root: None,
        };
        let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);
        let prompt = approval_prompt(&call, &context);
        assert_eq!(prompt.statement, "Run this command");
        assert_eq!(prompt.subject, "git status --short --branch && task check");
        assert_eq!(
            prompt.location.as_deref(),
            Some("/home/e/work/projects/nib")
        );
        assert!(!prompt.subject.contains("command="));
        assert_eq!(
            compact_approval_risk("destructive / requires_approval"),
            "destructive"
        );

        let raw_secret = "raw-command-secret";
        let secret_call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": format!("deploy --token={raw_secret}")}),
            session_id: None,
            project_root: None,
        };
        let mut safe_context =
            ApprovalContext::compatibility(&secret_call, PermissionLevel::Destructive);
        safe_context.display_subject = "deploy --token=[REDACTED]".to_string();
        let safe_prompt = approval_prompt(&secret_call, &safe_context);
        assert_eq!(safe_prompt.subject, "deploy --token=[REDACTED]");
        assert!(!safe_prompt.subject.contains(raw_secret));
    }

    #[test]
    fn approval_prompt_discloses_omitted_patch_targets() {
        let patch = ["e.rs", "b.rs", "a.rs", "d.rs", "c.rs"]
            .into_iter()
            .map(|path| format!("*** Update File: {path}\n"))
            .collect::<String>();
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "apply_patch".to_string(),
            arguments: json!({"dry_run": false, "patch": patch}),
            session_id: None,
            project_root: None,
        };
        let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);

        let prompt = approval_prompt(&call, &context);

        assert_eq!(prompt.statement, "Apply a patch");
        assert_eq!(prompt.subject, "5 files (+1 more): a.rs,b.rs,c.rs,d.rs");
        assert!(!prompt.subject.contains("e.rs"));
    }

    #[test]
    fn plan_approval_card_lists_numbered_steps() {
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "approve_plan".to_string(),
            arguments: json!({
                "plan_id": "plan-123",
                "goal": "inspect wrap",
                "steps": ["inspect files", "change parser", "run tests"],
            }),
            session_id: None,
            project_root: None,
        };
        let context = ApprovalContext::compatibility(&call, PermissionLevel::Plan);
        let prompt = approval_prompt(&call, &context);
        assert_eq!(prompt.statement, "Approve this plan");
        assert_eq!(
            context.details,
            vec![
                "1. inspect files".to_string(),
                "2. change parser".to_string(),
                "3. run tests".to_string(),
            ]
        );
        assert!(!prompt.subject.contains("plan_id="));

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let composer = Composer::default();
        let (approval_tx, _approval_rx) = oneshot::channel();
        let approval = approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "approve_plan".to_string(),
                arguments: json!({
                    "plan_id": "plan-123",
                    "goal": "inspect wrap",
                    "steps": ["inspect files", "change parser", "run tests"],
                }),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Plan,
            approval_tx,
        );
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace  ·  sess dock-session  ·  local  ·  -",
                    "awaiting you  ·  mock/mock-model  ·  queue 0",
                    "you  inspect wrap\n\nthought  generated 3 steps\n1. inspect files\n2. change parser\n3. run tests",
                    &composer,
                    Some(&approval),
                    None,
                )
            })
            .expect("render plan approval");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Approve this plan"), "{rendered}");
        assert!(rendered.contains("1. inspect files"), "{rendered}");
        assert!(rendered.contains("2. change parser"), "{rendered}");
        assert!(rendered.contains("3. run tests"), "{rendered}");
        assert!(rendered.contains("generated 3 steps"), "{rendered}");
        assert!(rendered.contains("Approve once"), "{rendered}");
        assert!(rendered.contains("Deny"), "{rendered}");
        assert!(!rendered.contains("plan_id="), "{rendered}");
        assert!(!rendered.contains("command="), "{rendered}");
    }

    #[test]
    fn plan_approval_card_marks_omitted_steps() {
        let rows = overlay_visual_rows(
            "Approve this plan",
            &(1..=8)
                .map(|index| format!("{index}. step {index}"))
                .collect::<Vec<_>>(),
            40,
        );
        assert!(rows.iter().any(|row| row.contains("Approve this plan")));
        assert!(rows.iter().any(|row| row.contains("1. step 1")));
        assert!(rows.iter().any(|row| row.contains("more")), "{rows:?}");
        assert_eq!(rows.len(), 6);
        assert!(!rows.iter().any(|row| row.contains("8. step 8")));
    }

    #[test]
    fn ledger_keeps_transcript_visible_under_approval_and_question_docks() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let composer = Composer::default();
        let (approval_tx, _approval_rx) = oneshot::channel();
        let approval = approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task test"}),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            approval_tx,
        );
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace  ·  sess dock-session  ·  local  ·  -",
                    "awaiting you  ·  mock/mock-model  ·  queue 0",
                    "you  inspect wrap\n\nassistant  the tests fail because width is wrong",
                    &composer,
                    Some(&approval),
                    None,
                )
            })
            .expect("render dock");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("workspace"));
        assert!(rendered.contains("inspect wrap"));
        assert!(rendered.contains("WAITING APPROVAL"));
        assert!(rendered.contains("mock-model"));
        assert!(rendered.contains("Run this command"));
        assert!(rendered.contains("task test"));
        assert!(rendered.contains("Approve once"));
        assert!(rendered.contains("Deny"));
        assert!(!rendered.contains("Approval required"));
        assert!(!rendered.contains("command="));
        assert!(!rendered.contains("{\"command\""));
    }

    #[test]
    fn question_card_states_the_ask_and_numbered_choices() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let (reply_tx, _reply_rx) = oneshot::channel();
        let question = PendingQuestion::new(TuiQuestionRequest {
            question: "Choose a mode".to_string(),
            options: vec!["plan".to_string(), "execute".to_string()],
            reply: reply_tx,
        });
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace  ·  sess dock-session  ·  local  ·  -",
                    "awaiting you  ·  mock/mock-model  ·  queue 0",
                    "you  inspect wrap\n\nnib  keep this visible",
                    &Composer::default(),
                    None,
                    Some(&question),
                )
            })
            .expect("render question card");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Choose a mode"), "{rendered}");
        assert!(rendered.contains("1. plan (1)"), "{rendered}");
        assert!(rendered.contains("2. execute (2)"), "{rendered}");
        assert!(rendered.contains("Leave unanswered"), "{rendered}");
        assert!(rendered.contains("WAITING QUESTION"), "{rendered}");
        assert!(rendered.contains("Enter"), "{rendered}");
        assert!(rendered.contains("Esc"), "{rendered}");
        assert!(rendered.contains("inspect wrap"), "{rendered}");
        assert!(rendered.contains("keep this visible"), "{rendered}");
        assert!(!rendered.contains("nib is asking"), "{rendered}");
        assert!(!rendered.contains("question  Choose a mode"), "{rendered}");
    }

    #[test]
    fn approval_card_states_the_command_and_keeps_choices_on_a_narrow_terminal() {
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let composer = Composer::default();
        let (approval_tx, _approval_rx) = oneshot::channel();
        let approval = approval_request(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({
                    "command": "git status --short --branch && task check",
                    "cwd": "/home/e/work/projects/nib",
                    "background": false
                }),
                session_id: None,
                project_root: None,
            },
            PermissionLevel::Destructive,
            approval_tx,
        );
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "workspace  ·  sess dock-session  ·  local  ·  -",
                    "awaiting you  ·  mock/mock-model  ·  queue 0",
                    "you  inspect wrap\n\nnib  keep this visible",
                    &composer,
                    Some(&approval),
                    None,
                )
            })
            .expect("render narrow approval");
        let rows = buffer_rows(&terminal);
        let joined = rows.concat();
        assert!(joined.contains("Run this command"), "{joined}");
        assert!(joined.contains("git status --short --branch"), "{joined}");
        assert!(rows.iter().any(|row| row.contains("task")), "{rows:?}");
        assert!(joined.contains("Approve once"), "{joined}");
        assert!(joined.contains("Deny"), "{joined}");
        assert!(joined.contains("keep this visible"), "{joined}");
        assert!(!joined.contains("command="), "{joined}");
        assert!(!joined.contains("background=false"), "{joined}");
        let command = row_index_containing(&rows, "git status").expect("command row");
        let approve = row_index_containing(&rows, "Approve once").expect("approve row");
        let deny = row_index_containing(&rows, "Deny").expect("deny row");
        assert!(
            command < approve,
            "command must sit above the choices: {rows:?}"
        );
        assert!(approve < deny, "approve must sit above deny: {rows:?}");
        let prompt = rows
            .iter()
            .position(|row| row.contains("> Run this command") || row.contains("Run this command"))
            .expect("composer prompt");
        assert!(
            prompt < approve,
            "approval choices must sit under the composer: {rows:?}"
        );
    }

    #[test]
    fn exact_id_preview_refreshes_listed_candidate_snapshot() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        store.create_session_with_id("session-a");
        store
            .try_append_message("session-a", "user", "original")
            .expect("message");
        let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
        let original_token = switcher.candidates[0].snapshot_token;
        store
            .try_append_message("session-a", "assistant", "changed")
            .expect("mutate");
        preview_exact_session(&store, &mut switcher, "session-a", "session-a")
            .expect("refresh listed");
        assert_ne!(switcher.candidates[0].snapshot_token, original_token);
        assert!(switcher.candidates[0].preview.contains("changed"));
    }

    #[test]
    fn switcher_activate_failure_stays_on_the_overlay() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        store.create_session_with_id("session-a");
        let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
        let mut active_id = "session-a".to_string();
        let mut timeline = ActiveTimeline::load(&store, &active_id).expect("timeline");
        let error =
            activate_selected_session(&store, &switcher, true, &mut active_id, &mut timeline)
                .expect_err("busy worker");
        switcher.error = Some(error);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_session_switcher(frame, frame.area(), &switcher, "session-a"))
            .expect("render");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("switcher error"));
        assert!(rendered.contains("still running"));
    }

    #[test]
    fn unicode_width_follow_tail_counts_wide_glyphs() {
        assert!(bottom_scroll_for_wrap("漢字漢字", 2, 1) >= 3);
        let mut composer = Composer::default();
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Char('j'), KeyModifiers::CONTROL),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "\n");
        assert_eq!(composer.cursor, 1);
    }

    #[test]
    fn composer_moves_the_caret_and_inserts_in_the_middle() {
        let mut composer = Composer::default();
        for character in "abc".chars() {
            assert_eq!(
                composer_action_for_key(
                    &mut composer,
                    KeyCode::Char(character),
                    KeyModifiers::NONE
                ),
                ComposerAction::Pending
            );
        }
        assert_eq!(composer.cursor, 3);
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Left, KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Char('X'), KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "abXc");
        assert_eq!(composer.cursor, 3);
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Backspace, KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "abc");
    }

    #[test]
    fn composer_delete_removes_one_unicode_scalar_at_the_caret() {
        let mut composer = Composer::from_text("a🙂漢b");
        composer.cursor = 1;
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Delete, KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "a漢b");
        assert_eq!(composer.cursor, 1);
        assert!(composer.input.is_char_boundary(composer.cursor));

        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Delete, KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "ab");
        assert_eq!(composer.cursor, 1);
    }

    #[test]
    fn composer_paste_normalizes_lines_and_omits_unsafe_controls() {
        let mut composer = Composer::from_text("leftright");
        composer.cursor = "left".len();
        let outcome = composer.insert_paste("🙂\r\nline\rnext\t\0\u{1b}end");

        assert_eq!(composer.input, "left🙂\nline\nnext    endright");
        assert_eq!(composer.cursor, "left🙂\nline\nnext    end".len());
        assert!(!outcome.truncated);
        assert!(outcome.controls_omitted);
        assert_eq!(
            outcome.visible_status().as_deref(),
            Some("[composer] unsafe paste control characters omitted")
        );
        assert!(!composer.input.contains('\0'));
        assert!(!composer.input.contains('\u{1b}'));
    }

    #[test]
    fn composer_paste_truncation_preserves_utf8_prefix_and_reports_status() {
        let mut composer = Composer::from_text("x".repeat(MAX_COMPOSER_BYTES - 5));
        let outcome = composer.insert_paste("🙂étail");

        assert_eq!(outcome.inserted_bytes, "🙂".len());
        assert!(outcome.truncated);
        assert!(composer.input.ends_with('🙂'));
        assert_eq!(composer.input.len(), MAX_COMPOSER_BYTES - 1);
        assert!(std::str::from_utf8(composer.input.as_bytes()).is_ok());
        assert_eq!(
            outcome.visible_status().as_deref(),
            Some("[composer] paste truncated at 16384 bytes")
        );
    }

    #[test]
    fn transcript_scroll_keys_and_wheel_move_content() {
        assert_eq!(
            transcript_action_for_key(KeyCode::PageUp, KeyModifiers::NONE),
            Some(TranscriptViewportAction::PageUp)
        );
        assert_eq!(
            transcript_action_for_key(KeyCode::Up, KeyModifiers::SHIFT),
            Some(TranscriptViewportAction::Lines(-1))
        );
        assert_eq!(
            transcript_action_for_key(KeyCode::Down, KeyModifiers::CONTROL),
            Some(TranscriptViewportAction::Lines(1))
        );
        assert_eq!(
            transcript_action_for_key(KeyCode::Up, KeyModifiers::NONE),
            None,
            "unmodified Up remains draft history"
        );
        assert_eq!(
            transcript_action_for_mouse(MouseEventKind::ScrollUp),
            Some(TranscriptViewportAction::Lines(-3))
        );
        assert_eq!(
            transcript_action_for_mouse(MouseEventKind::ScrollDown),
            Some(TranscriptViewportAction::Lines(3))
        );

        let mut viewport = TranscriptViewport::default();
        viewport.observe_layout(40, 5);
        assert_eq!(viewport.top_row(), 35);
        viewport.apply(transcript_action_for_mouse(MouseEventKind::ScrollUp).expect("wheel"));
        assert!(!viewport.is_pinned_to_tail());
        assert_eq!(viewport.top_row(), 32);
        viewport.apply(TranscriptViewportAction::PageUp);
        assert_eq!(viewport.top_row(), 27);
        assert_eq!(
            transcript_action_for_key(KeyCode::Home, KeyModifiers::CONTROL),
            Some(TranscriptViewportAction::JumpToStart)
        );
    }

    #[test]
    fn pointer_selection_copies_chat_rows_and_falls_back_to_the_last_reply() {
        let rows = vec![
            "● inspect wrap".to_string(),
            "● read_file running · src/lib.rs".to_string(),
            "● Here is the answer".to_string(),
        ];
        let mut selection = PointerSelection::at(0, 2);
        selection.drag_to(0, 14);
        assert_eq!(extract_pointer_text(&rows, selection), "inspect wrap");
        selection = PointerSelection::at(0, 0);
        selection.drag_to(2, 20);
        let copied = extract_pointer_text(&rows, selection);
        assert!(copied.contains("inspect wrap"), "{copied}");
        assert!(copied.contains("Here is the answer"), "{copied}");

        let view = TranscriptView {
            plain_rows: rows,
            ..TranscriptView::default()
        };
        let activities = vec![
            ActivityEntry::new(ActivityKind::User, "", "inspect wrap"),
            ActivityEntry::new(ActivityKind::Assistant, "", "Here is the answer"),
        ];
        let (text, status) =
            copy_chat_content(None, &view, None, &activities).expect("copy fallback");
        assert_eq!(text, "Here is the answer");
        assert_eq!(status, "Copied last reply.");
        let (block, block_status) =
            copy_chat_content(None, &view, Some(0), &activities).expect("copy block");
        assert_eq!(block, "inspect wrap");
        assert_eq!(block_status, "Copied.");

        let mut pointer = PointerSelection::at(0, 2);
        assert!(!pointer_selection_is_active(Some(pointer)));
        pointer.drag_to(0, 14);
        assert!(pointer_selection_is_active(Some(pointer)));
        let (selected, selected_status) =
            copy_chat_content(Some(pointer), &view, Some(0), &activities).expect("copy drag");
        assert_eq!(selected, "inspect wrap");
        assert_eq!(selected_status, "Copied selection.");

        let mut live = Some(pointer);
        let mut focus = TuiFocus::Transcript;
        let failed = copy_pointer_selection_and_clear_with(
            &mut live,
            &mut focus,
            &view,
            Some(0),
            &activities,
            |text| {
                assert_eq!(text, "inspect wrap");
                ClipboardDelivery::Failed
            },
        );
        assert_eq!(
            failed,
            Some("Copy failed; text remains available for manual selection")
        );
        assert_eq!(live, Some(pointer));
        assert_eq!(focus, TuiFocus::Transcript);

        let copied = copy_pointer_selection_and_clear_with(
            &mut live,
            &mut focus,
            &view,
            Some(0),
            &activities,
            |_| ClipboardDelivery::Native,
        );
        assert_eq!(copied, Some("Copied"));
        assert!(live.is_none());
        assert_eq!(focus, TuiFocus::Composer);
        assert!(ClipboardDelivery::Native.may_clear_selection());
        assert!(ClipboardDelivery::Osc52Unconfirmed.may_clear_selection());
        assert!(!ClipboardDelivery::Unavailable.may_clear_selection());
        assert!(!ClipboardDelivery::Failed.may_clear_selection());
    }

    #[test]
    fn clipboard_delivery_reports_every_backend_outcome_without_fallthrough() {
        use std::cell::Cell;

        let native_calls = Cell::new(0);
        let osc52_calls = Cell::new(0);
        let delivery = deliver_text_to_clipboard_with(
            "copy me",
            false,
            |_| {
                native_calls.set(native_calls.get() + 1);
                true
            },
            |_| {
                osc52_calls.set(osc52_calls.get() + 1);
                true
            },
        );
        assert_eq!(delivery, ClipboardDelivery::Unavailable);
        assert_eq!((native_calls.get(), osc52_calls.get()), (0, 0));

        let delivery = deliver_text_to_clipboard_with(
            "copy me",
            true,
            |_| true,
            |_| panic!("OSC52 must not run after native success"),
        );
        assert_eq!(delivery, ClipboardDelivery::Native);
        assert_eq!(delivery.status(), "Copied");

        let delivery = deliver_text_to_clipboard_with("copy me", true, |_| false, |_| true);
        assert_eq!(delivery, ClipboardDelivery::Osc52Unconfirmed);
        assert_eq!(delivery.status(), "Copy requested via OSC52 (unconfirmed)");

        let delivery = deliver_text_to_clipboard_with("copy me", true, |_| false, |_| false);
        assert_eq!(delivery, ClipboardDelivery::Failed);
        assert_eq!(
            delivery.status(),
            "Copy failed; text remains available for manual selection"
        );
    }

    #[test]
    fn transcript_hit_maps_mouse_cells_to_plain_rows() {
        let view = TranscriptView {
            area: Rect::new(0, 1, 40, 10),
            top_row: 0,
            plain_rows: vec!["● inspect wrap".to_string(), "● reply".to_string()],
            owners: vec![0, 1],
            ..TranscriptView::default()
        };
        let hit = transcript_hit(
            &view,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(hit, Some((0, 2)));
        assert_eq!(
            transcript_hit(
                &view,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 2,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
            ),
            None
        );
    }

    #[test]
    fn bracketed_paste_sequences_and_restore_guard_are_deterministic() {
        let mut enabled = Vec::new();
        enable_bracketed_paste_to(&mut enabled).expect("enable paste sequence");
        assert_eq!(enabled, b"\x1b[?2004h");

        #[cfg(not(windows))]
        let restored = {
            let mut restored = Vec::new();
            restore_terminal_to(&mut restored, || Ok(())).expect("restore sequences");
            String::from_utf8(restored).expect("terminal control UTF-8")
        };
        #[cfg(windows)]
        let restored = {
            // `execute!` invokes Win32 console APIs even for an in-memory writer.
            // Check encoding here; native ConPTY smokes verify real restoration.
            let mut restored = String::new();
            crossterm::Command::write_ansi(&DisableMouseCapture, &mut restored)
                .expect("disable mouse sequence");
            crossterm::Command::write_ansi(&DisableBracketedPaste, &mut restored)
                .expect("disable paste sequence");
            crossterm::Command::write_ansi(&LeaveAlternateScreen, &mut restored)
                .expect("leave alternate screen sequence");
            restored
        };
        let mouse = restored.find("\x1b[?1000l").expect("disable mouse capture");
        let paste = restored.find("\x1b[?2004l").expect("disable paste");
        let alternate = restored
            .find("\x1b[?1049l")
            .expect("leave alternate screen");
        assert!(mouse < paste);
        assert!(paste < alternate);

        TEST_TERMINAL_RESTORE_CALLS.store(0, Ordering::SeqCst);
        {
            let _guard = TerminalRestoreGuard::with_restore(record_test_terminal_restore);
        }
        assert_eq!(TEST_TERMINAL_RESTORE_CALLS.load(Ordering::SeqCst), 1);

        TEST_TERMINAL_RESTORE_CALLS.store(0, Ordering::SeqCst);
        {
            let mut guard = TerminalRestoreGuard::with_restore(record_test_terminal_restore);
            guard.restore().expect("explicit restoration");
        }
        assert_eq!(TEST_TERMINAL_RESTORE_CALLS.load(Ordering::SeqCst), 1);
    }

    #[cfg(not(windows))]
    #[test]
    fn raw_mode_restoration_runs_last_even_after_an_output_failure() {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        struct Output {
            bytes: Rc<RefCell<Vec<u8>>>,
            fail_first_flush: bool,
            flushes: usize,
        }

        impl io::Write for Output {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.borrow_mut().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                if self.fail_first_flush && self.flushes == 1 {
                    Err(io::Error::other("mouse output failed"))
                } else {
                    Ok(())
                }
            }
        }

        for fail in [false, true] {
            let bytes = Rc::new(RefCell::new(Vec::new()));
            let mut output = Output {
                bytes: Rc::clone(&bytes),
                fail_first_flush: fail,
                flushes: 0,
            };
            let raw_called = Cell::new(false);
            let result = restore_terminal_to(&mut output, || {
                let captured = String::from_utf8(bytes.borrow().clone()).expect("ANSI output");
                for sequence in ["\x1b[?1000l", "\x1b[?2004l", "\x1b[?1049l"] {
                    assert!(
                        captured.contains(sequence),
                        "raw cleanup ran before {sequence:?}"
                    );
                }
                raw_called.set(true);
                if fail {
                    Err(io::Error::other("raw cleanup failed"))
                } else {
                    Ok(())
                }
            });
            assert!(raw_called.get());
            assert_eq!(output.flushes, 3);
            if fail {
                let error = result
                    .expect_err("retain both cleanup failures")
                    .to_string();
                assert!(error.contains("mouse output failed"));
                assert!(error.contains("raw cleanup failed"));
            } else {
                result.expect("successful restoration");
            }
        }
    }

    #[test]
    fn missing_modal_state_renders_a_recoverable_error() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_interaction_overlay(
                    frame,
                    InteractionLayer::Model,
                    None,
                    None,
                    None,
                    "session-a",
                )
            })
            .expect("recoverable modal render");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Recoverable UI Error"));
        assert!(rendered.contains("state is unavailable"));
    }

    #[test]
    fn composer_restores_bounded_draft_history_with_up_and_down() {
        let mut composer = Composer::default();
        for draft in ["first goal", "second goal"] {
            for character in draft.chars() {
                composer_action_for_key(
                    &mut composer,
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                );
            }
            assert_eq!(
                composer_action_for_key(&mut composer, KeyCode::Enter, KeyModifiers::NONE),
                ComposerAction::Submit(draft.to_string())
            );
        }
        composer.set_text("scratch".to_string());
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Up, KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "second goal");
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Up, KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "first goal");
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Down, KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "second goal");
        assert_eq!(
            composer_action_for_key(&mut composer, KeyCode::Down, KeyModifiers::NONE),
            ComposerAction::Pending
        );
        assert_eq!(composer.input, "scratch");
    }

    #[test]
    fn composer_draft_history_drops_oldest_entries_beyond_the_bound() {
        let mut composer = Composer::default();
        for index in 0..=MAX_DRAFT_HISTORY {
            composer.remember_submission(&format!("goal-{index}"));
        }
        assert_eq!(composer.history.entries().len(), MAX_DRAFT_HISTORY);
        assert_eq!(composer.history.entries()[0], "goal-1");
        assert_eq!(
            composer.history.entries().last().map(String::as_str),
            Some("goal-50")
        );
    }

    #[test]
    fn draft_history_search_restores_unicode_entry_and_preserves_current_draft() {
        let mut composer = Composer::from_text("current draft");
        composer.remember_submission("first");
        composer.remember_submission("fix 🙂 unicode");
        let mut search = PendingHistorySearch::new(&composer.history, Some("🙂".to_string()));
        assert_eq!(search.search.matches.len(), 1);
        let HistorySearchAction::Select(index) = history_search_action_for_key(
            &mut search,
            &composer.history,
            KeyCode::Enter,
            KeyModifiers::NONE,
        ) else {
            panic!("matching history entry must be selectable");
        };
        assert!(composer.select_history_entry(index));
        assert_eq!(composer.input, "fix 🙂 unicode");
        composer.recall_newer();
        assert_eq!(composer.input, "current draft");
    }

    #[test]
    fn draft_history_search_empty_cancel_and_control_input_recover_in_overlay() {
        let history = DraftHistory::default();
        let mut search = PendingHistorySearch::new(&history, None);
        assert_eq!(
            search.error.as_deref(),
            Some("[history] no submitted drafts are available")
        );
        assert_eq!(
            history_search_action_for_key(
                &mut search,
                &history,
                KeyCode::Enter,
                KeyModifiers::NONE,
            ),
            HistorySearchAction::Pending
        );
        assert_eq!(
            search.error.as_deref(),
            Some("[history error] select requires a matching draft")
        );
        search.insert('\0', &history);
        assert_eq!(search.query, "");
        assert_eq!(
            search.error.as_deref(),
            Some("[history error] control character ignored")
        );
        assert_eq!(
            history_search_action_for_key(&mut search, &history, KeyCode::Esc, KeyModifiers::NONE,),
            HistorySearchAction::Close
        );
    }

    #[test]
    fn draft_history_overlay_is_control_safe_and_transcript_viewport_is_visible() {
        let mut history = DraftHistory::default();
        history.remember_submission("safe\0\u{1b} draft 🙂");
        let search = PendingHistorySearch::new(&history, None);
        let backend = TestBackend::new(84, 22);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut viewport = TranscriptViewport::default();
        let transcript = (0..30)
            .map(|index| format!("row-{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let composer = Composer::default();
        let welcome = StartupWelcome::fixture();
        let activities = activities_from_timeline_text(&transcript);
        let band = InteractionBand::History(&search);
        terminal
            .draw(|frame| {
                render_session_activities(
                    frame,
                    &TuiChrome::fixture(),
                    &activities,
                    &composer,
                    None,
                    None,
                    false,
                    &mut viewport,
                    TuiFocus::Composer,
                    None,
                    0,
                    None,
                    None,
                    &welcome,
                    Some(&band),
                    None,
                    None,
                );
            })
            .expect("render history list");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("safe draft 🙂"), "{rendered}");
        assert!(rendered.contains("row-29"), "{rendered}");
        assert!(!rendered.contains("Draft History"), "{rendered}");
        assert!(!rendered.contains('\0'));
        assert!(!rendered.contains('\u{1b}'));
        assert!(viewport.is_pinned_to_tail());
    }

    #[test]
    fn transcript_viewport_keeps_manual_row_on_append_and_submit_repins() {
        let backend = TestBackend::new(48, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let composer = Composer::default();
        let mut viewport = TranscriptViewport::default();
        let first = (0..30)
            .map(|index| format!("row-{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        terminal
            .draw(|frame| {
                render_current_session_view_with_viewport(
                    frame,
                    "header",
                    "running",
                    &first,
                    &composer,
                    None,
                    None,
                    true,
                    &mut viewport,
                )
            })
            .expect("tail render");
        viewport.apply(TranscriptViewportAction::PageUp);
        let manual_top = viewport.top_row();
        assert!(!viewport.is_pinned_to_tail());

        let appended = format!("{first}\nrow-30\nrow-31");
        terminal
            .draw(|frame| {
                render_current_session_view_with_viewport(
                    frame,
                    "header",
                    "running",
                    &appended,
                    &composer,
                    None,
                    None,
                    true,
                    &mut viewport,
                )
            })
            .expect("unpinned append render");
        assert_eq!(viewport.top_row(), manual_top);

        viewport.on_submission();
        assert!(viewport.is_pinned_to_tail());
        terminal
            .draw(|frame| {
                render_current_session_view_with_viewport(
                    frame,
                    "header",
                    "idle",
                    &appended,
                    &composer,
                    None,
                    None,
                    false,
                    &mut viewport,
                )
            })
            .expect("repinned render");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("approval manual"));
        assert!(rendered.contains("row-31"));
    }

    #[test]
    fn composer_height_follows_unicode_wrap_not_newline_count() {
        let composer = Composer::from_text("a".repeat(200));
        assert_eq!(composer_height(&composer, 80), 3);
        assert_eq!(composer_height(&Composer::default(), 80), 2);

        let combining = "e\u{301}";
        assert_eq!(composer_cursor_cell(combining, combining.len(), 4), (3, 0));
        assert_eq!(composer_cursor_cell("ab", 2, 4), (0, 1));
        assert_eq!(composer_cursor_cell("a\n漢", "a\n漢".len(), 4), (2, 1));

        let joined = "👩\u{200d}💻";
        assert_eq!(unicode_display_width(joined), 2);
        assert_eq!(composer_cursor_cell(joined, joined.len(), 4), (0, 1));

        let exact = Composer::from_text("ab");
        assert_eq!(composer_visual_rows(&exact, 4), ["> ab", ""]);
    }

    #[test]
    fn composer_wraps_and_renders_complete_emoji_at_the_caret() {
        for grapheme in ["☺\u{fe0f}", "1\u{fe0f}\u{20e3}", "👩\u{200d}💻"] {
            let input = format!("a{grapheme}");
            let composer = Composer::from_text(input.as_str());
            assert_eq!(composer_cursor_cell(&input, input.len(), 4), (2, 1));
            assert_eq!(
                composer_visual_rows(&composer, 4),
                vec!["> a".to_string(), grapheme.to_string()]
            );
            let mut terminal = Terminal::new(TestBackend::new(4, 12)).expect("test terminal");
            terminal
                .draw(|frame| {
                    render_current_session_view(frame, "nib", "idle", "", &composer, None, None)
                })
                .expect("render grapheme boundary");
            let cursor = terminal.get_cursor_position().expect("cursor");
            assert_eq!(cursor.x, 2);
            assert_eq!(
                terminal.backend().buffer()[(0, cursor.y)].symbol(),
                grapheme
            );
        }
    }

    #[test]
    fn ledger_renders_exact_width_and_newline_carets_in_the_composer_viewport() {
        for (input, width, expected_x) in [("abcdef", 8, 0), ("a\n漢", 8, 2)] {
            let backend = TestBackend::new(width, 12);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let composer = Composer::from_text(input);
            terminal
                .draw(|frame| {
                    render_current_session_view(
                        frame,
                        "header",
                        "idle",
                        "you  hello",
                        &composer,
                        None,
                        None,
                    )
                })
                .expect("render composer boundary");
            let position = terminal.get_cursor_position().expect("cursor");
            assert_eq!(position.x, expected_x, "input={input:?}");
            assert!(position.y < 11, "input={input:?}, position={position:?}");
        }
    }

    #[test]
    fn ledger_places_the_caret_inside_the_composer_rect() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let composer = Composer::from_text("hi");
        terminal
            .draw(|frame| {
                render_current_session_view(
                    frame,
                    "header",
                    "idle",
                    "you  hello",
                    &composer,
                    None,
                    None,
                )
            })
            .expect("render");
        let position = terminal.get_cursor_position().expect("cursor");
        assert!(
            position.y >= 21 && position.y <= 22,
            "caret y {} should be in the composer rows",
            position.y
        );
        assert_eq!(position.x, 4);
    }

    #[test]
    fn tui_cancel_quit_and_switch_report_queue_disposition() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        store.create_session_with_id("session-a");
        store.create_session_with_id("session-b");
        persist_queued_follow_up(&store, "session-a", "next turn", "composer").expect("queue");

        let mut timeline = ActiveTimeline::load(&store, "session-a").expect("timeline");
        timeline.reconciled_terminal = Some(InteractionTerminalOutcome::Cancelled);
        let cancelled =
            tui_report_cancelled_run(&store, "session-a", &mut timeline).expect("cancel");
        assert!(cancelled.contains("cancelled;"));
        assert!(cancelled.contains("retained on session session-a"));
        assert!(timeline.rendered_text().contains(&cancelled));
        assert!(timeline
            .rendered_text()
            .contains("[cancelled] active agent run"));

        timeline.reconciled_terminal = Some(InteractionTerminalOutcome::Completed);
        let quit = tui_report_quit_run(&store, "session-a", &mut timeline).expect("quit run");
        assert!(quit.contains("[completed] active run completed before quit"));
        assert!(quit.contains("quit after completion;"));
        assert!(quit.contains("retained on session session-a"));
        assert!(timeline.rendered_text().contains(&quit));

        let exited = tui_exit_disposition(&store, "session-a").expect("exit");
        assert!(exited.contains("exited;"));
        assert!(exited.contains("retained on session session-a"));

        let mut switcher = load_session_switcher(&store, "session-a").expect("switcher");
        switcher.selected = switcher
            .candidates
            .iter()
            .position(|candidate| candidate.id == "session-b")
            .expect("target");
        let mut active_id = "session-a".to_string();
        let mut timeline = ActiveTimeline::load(&store, &active_id).expect("switch timeline");
        let switched =
            tui_complete_session_switch(&store, &switcher, false, &mut active_id, &mut timeline)
                .expect("switch");
        assert_eq!(active_id, "session-b");
        assert!(switched.contains("switched sessions;"));
        assert!(switched.contains("retained on session session-a"));
        assert!(timeline.rendered_text().contains(&switched));
    }
}
