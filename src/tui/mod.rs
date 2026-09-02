//! Current-session-first Ratatui interface with live agent lifecycle rendering.

use crate::interactive::{
    apply_stream_event, claim_next_queued_follow_up_after_startup,
    display_stream_event_with_sensitive_values, execute_interactive_command_in_state,
    format_interaction_chrome, interactive_completions, interactive_session_candidate,
    interactive_session_selection, path_completions, persist_queued_follow_up,
    project_session_activities, queue_disposition_message, reduce_interaction, resolve_session,
    restore_queued_follow_up_after_start_failure, safe_event_fields, set_active_model,
    unicode_display_width, validate_interactive_session_target, wrapped_display_rows,
    wrapped_line_count, ActivityEntry, ActivityKind, DraftHistory, DraftHistorySearch,
    InteractionConsumer, InteractionDecision, InteractionInput, InteractionReduction,
    InteractionRunState, InteractionState, InteractionTerminalOutcome, InteractiveAgentMode,
    InteractiveCompletion, InteractiveEffect, InteractiveSessionCandidate,
    InteractiveSessionSelection, ModelSelection, SelectorDetailKind, StreamDisplay,
    TranscriptViewport, TranscriptViewportAction, MAX_DRAFT_HISTORY_QUERY_BYTES,
};
use crate::interactive::{bounded_public_text, control_safe_text};
use crate::llm::types::StreamEvent;
use crate::session::SessionStore;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::DefaultTerminal;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::mpsc;
use std::thread::JoinHandle;
use tokio::sync::oneshot;

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
const MAX_SESSION_DETAIL_BYTES: usize = 131_072;
const MAX_SESSION_DETAIL_ITEMS: usize = 100;
const MAX_SESSION_DETAIL_ITEM_CHARS: usize = 500;
const MAX_SESSION_DETAIL_ROWS: usize = 400;
const SESSION_DETAIL_TRUNCATED_MARKER: &str = "\n[session detail truncated]\n";
const MAX_VISIBLE_COMPLETIONS: usize = 10;
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionDetail {
    text: String,
}

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
    persisted: String,
    activities: Vec<ActivityEntry>,
    live: LiveOutput,
    notice: Option<String>,
    sensitive_values: Vec<String>,
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
            persisted: SessionDetail::new(&session.id, Some(session), &sensitive_values).text,
            activities,
            live: LiveOutput::default(),
            notice: None,
            sensitive_values,
        }
    }

    fn push_status(&mut self, status: String) {
        let status =
            bounded_public_text(&status, &self.sensitive_values, MAX_LIVE_OUTPUT_BYTES, true);
        if status.starts_with("Resumed session ") {
            self.notice = Some(status.clone());
        }
        self.live.push_status(status.clone());
        self.activities.push(ActivityEntry {
            kind: ActivityKind::System,
            title: status,
            body: String::new(),
        });
    }

    fn push_steering(&mut self, _text: &str, sequence: usize) {
        self.activities.push(ActivityEntry {
            kind: ActivityKind::User,
            title: format!("steer {sequence}"),
            body: "instruction persisted for the exact active run".to_string(),
        });
        self.live
            .push_status(format!("[steer] accepted for safe boundary #{sequence}"));
    }

    fn apply_event(&mut self, event: StreamEvent) {
        if let StreamEvent::Reconciled { outcome } = &event {
            if let InteractionReduction::Reconciled { terminal, .. } = reduce_interaction(
                &InteractionState::default(),
                InteractionInput::ReconciledOutcome {
                    outcome,
                    failure: false,
                },
            ) {
                self.reconciled_terminal = Some(terminal);
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

    fn bind_run(&mut self, run_id: Option<String>) {
        if run_id.is_some() {
            self.reconciled_terminal = None;
        }
        self.active_run_id = run_id;
    }

    fn rendered_text(&self) -> String {
        let mut text = self
            .activities
            .iter()
            .map(ActivityEntry::render_line)
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.is_empty() {
            text = self.persisted.clone();
        }
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
    pub reply: oneshot::Sender<Result<String, String>>,
}

pub struct TuiQuestionHandler {
    pub tx: mpsc::Sender<TuiQuestionRequest>,
}

#[async_trait::async_trait]
impl crate::agent::QuestionHandler for TuiQuestionHandler {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(TuiQuestionRequest {
                question: question.to_string(),
                options: options.to_vec(),
                reply: reply_tx,
            })
            .map_err(|_| "TUI question channel closed".to_string())?;
        reply_rx
            .await
            .map_err(|_| "TUI question response was dropped".to_string())?
    }
}

struct PendingQuestion {
    request: TuiQuestionRequest,
    response: String,
    selected_option: usize,
    error: Option<String>,
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

    fn handle_key(&mut self, composer: &mut Composer, code: KeyCode) -> bool {
        if !self.is_open() {
            return false;
        }
        match code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.suggestions.len() - 1);
            }
            KeyCode::Tab | KeyCode::Enter => {
                if let Some(completion) = self.suggestions.get(self.selected) {
                    composer.set_text(completion.insertion.clone());
                }
                self.dismissed_for_input = Some(composer.input.clone());
                self.suggestions.clear();
                self.selected = 0;
            }
            KeyCode::Esc => {
                self.dismissed_for_input = Some(composer.input.clone());
                self.suggestions.clear();
                self.selected = 0;
            }
            _ => return false,
        }
        true
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
            response: String::new(),
            selected_option: 0,
            error: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum QuestionAction {
    Pending,
    Submit(String),
    Cancel,
    Error(String),
}

fn question_action_for_key(question: &mut PendingQuestion, code: KeyCode) -> QuestionAction {
    match code {
        KeyCode::Char(character)
            if question.response.len().saturating_add(character.len_utf8())
                <= MAX_COMPOSER_BYTES =>
        {
            question.response.push(character);
            question.error = None;
            QuestionAction::Pending
        }
        KeyCode::Char(_) => QuestionAction::Pending,
        KeyCode::Backspace => {
            question.response.pop();
            question.error = None;
            QuestionAction::Pending
        }
        KeyCode::Up => {
            question.selected_option = question.selected_option.saturating_sub(1);
            QuestionAction::Pending
        }
        KeyCode::Down | KeyCode::Tab => {
            if !question.request.options.is_empty() {
                question.selected_option = (question.selected_option + 1)
                    .min(question.request.options.len().saturating_sub(1));
            }
            QuestionAction::Pending
        }
        KeyCode::Enter => {
            let state = InteractionState {
                question_pending: true,
                ..InteractionState::default()
            };
            match reduce_interaction(
                &state,
                InteractionInput::QuestionAnswer {
                    answer: &question.response,
                    options: &question.request.options,
                    selected_option: (!question.request.options.is_empty())
                        .then_some(question.selected_option),
                },
            ) {
                InteractionReduction::QuestionAnswered(answer) => QuestionAction::Submit(answer),
                InteractionReduction::Error { message, .. } => QuestionAction::Error(message),
                _ => QuestionAction::Error(
                    "question input was rejected by the shared reducer".to_string(),
                ),
            }
        }
        KeyCode::Esc => QuestionAction::Cancel,
        _ => QuestionAction::Pending,
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
            if let Some(pending) = question.take() {
                let _ = pending.request.reply.send(Ok(response));
            }
            true
        }
        QuestionAction::Cancel => {
            if let Some(pending) = question.take() {
                let _ = pending
                    .request
                    .reply
                    .send(Err("question cancelled by user".to_string()));
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

fn approval_decision_for_key(code: KeyCode) -> Option<ApprovalDecision> {
    let answer = match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => "y",
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => "n",
        _ => return None,
    };
    let state = InteractionState {
        approval_pending: true,
        ..InteractionState::default()
    };
    match reduce_interaction(&state, InteractionInput::ApprovalAnswer(answer)) {
        InteractionReduction::ApprovalDecision(InteractionDecision::Accept) => {
            Some(ApprovalDecision::granted_user())
        }
        InteractionReduction::ApprovalDecision(InteractionDecision::Reject) => {
            Some(ApprovalDecision::denied())
        }
        _ => None,
    }
}

fn handle_approval_key(approval: &mut Option<TuiApprovalRequest>, code: KeyCode) -> bool {
    let Some(decision) = approval_decision_for_key(code) else {
        return false;
    };
    if let Some(request) = approval.take() {
        let _ = request.reply.send(decision);
        return true;
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

fn render_model_selection(frame: &mut ratatui::Frame<'_>, pending: &PendingModelSelection) {
    let modal_area = centered_rect(75, 60, frame.area());
    let safe_label = |value: &str| {
        bounded_public_text(
            value,
            &pending.selection.sensitive_values,
            MAX_SESSION_DETAIL_ITEM_CHARS,
            false,
        )
    };
    let mut text = vec![
        Line::from(Span::styled(
            format!(
                "Select model for {}",
                safe_label(&pending.selection.provider)
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if pending.selection.available.is_empty() {
        text.push(Line::from(
            "No configured suggestions; enter an exact model ID.",
        ));
    } else {
        for (index, model) in pending.selection.available.iter().enumerate() {
            let selected = if index == pending.selected_option {
                "> "
            } else {
                "  "
            };
            let current = if model == &pending.selection.current {
                " (current)"
            } else {
                ""
            };
            text.push(Line::from(format!(
                "{selected}{}. {}{current}",
                index + 1,
                safe_label(model),
            )));
        }
    }
    text.push(Line::from(""));
    text.push(Line::from(vec![
        Span::styled("Model: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(safe_label(&pending.response)),
    ]));
    text.push(Line::from("Enter select  Esc cancel"));
    let modal = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Model Selection "),
        )
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(ratatui::widgets::Clear, modal_area);
    frame.render_widget(modal, modal_area);
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
    let mut replacement =
        ActiveTimeline::from_session(&session, store.public_sensitive_values().to_vec());
    replacement.push_status(format!("Resumed session {target} from persisted state."));
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
    switcher: &SessionSwitcher,
    active_session_id: &str,
) {
    let modal_area = centered_rect(92, 84, frame.area());
    frame.render_widget(ratatui::widgets::Clear, modal_area);
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" Session Switcher | Up/Down or type exact ID | Enter preview/resume | Esc close ");
    let inner = outer.inner(modal_area);
    frame.render_widget(outer, modal_area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(inner);
    let visible_rows = usize::from(columns[0].height.saturating_sub(2)).max(1);
    let start = switcher
        .selected
        .saturating_add(1)
        .saturating_sub(visible_rows);
    let end = (start + visible_rows).min(switcher.candidates.len());
    let items = if switcher.candidates.is_empty() {
        vec![ListItem::new("(no sessions)")]
    } else {
        switcher.candidates[start..end]
            .iter()
            .enumerate()
            .map(|(offset, candidate)| {
                let index = start + offset;
                let selected = if index == switcher.selected { ">" } else { " " };
                let active = if candidate.id == active_session_id {
                    "*"
                } else {
                    " "
                };
                ListItem::new(format!("{selected}{active} {}", candidate.id))
            })
            .collect()
    };
    let range = if switcher.candidates.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}-{}/{}", start + 1, end, switcher.candidates.len())
    };
    let list_title = if switcher.omitted == 0 {
        format!(" Sessions {range} (* active) ")
    } else {
        format!(
            " Sessions {range} (* active; {} omitted, type exact ID) ",
            switcher.omitted
        )
    };
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(list_title)),
        columns[0],
    );
    let selected_preview = switcher
        .candidates
        .get(switcher.selected)
        .map(|candidate| candidate.preview.as_str())
        .unwrap_or("No session is available to preview.");
    let error = switcher
        .error
        .as_deref()
        .map(|error| format!("\n[switcher error] {error}\n"))
        .unwrap_or_default();
    let preview = format!(
        "Exact session ID: {}{error}\n{selected_preview}",
        switcher.exact_id
    );
    frame.render_widget(
        Paragraph::new(preview)
            .block(Block::default().borders(Borders::ALL).title(" Preview "))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        columns[1],
    );

    if switcher.confirming {
        let target = switcher
            .candidates
            .get(switcher.selected)
            .map(|candidate| candidate.id.as_str())
            .unwrap_or("(missing)");
        let mut lines = vec![
            Line::from(Span::styled(
                "Confirm session switch",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!("Current: {active_session_id}")),
            Line::from(format!("Target:  {target}")),
        ];
        lines.push(Line::from(""));
        lines.push(Line::from("Enter/Y resume  Esc/N keep current session"));
        let confirmation_area = centered_rect(70, 45, frame.area());
        frame.render_widget(ratatui::widgets::Clear, confirmation_area);
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Resume Confirmation "),
                )
                .wrap(ratatui::widgets::Wrap { trim: false }),
            confirmation_area,
        );
    }
}

fn render_completion(frame: &mut ratatui::Frame<'_>, completion: &CompletionMenu) {
    if !completion.is_open() {
        return;
    }
    let start = completion
        .selected
        .saturating_sub(MAX_VISIBLE_COMPLETIONS.saturating_sub(1));
    let end = (start + MAX_VISIBLE_COMPLETIONS).min(completion.suggestions.len());
    let lines = completion.suggestions[start..end]
        .iter()
        .enumerate()
        .map(|(offset, suggestion)| {
            let marker = if start + offset == completion.selected {
                ">"
            } else {
                " "
            };
            Line::from(format!(
                "{marker} {:<22} | {} | {}",
                suggestion.insertion.trim_end(),
                suggestion.usage,
                suggestion.summary
            ))
        })
        .collect::<Vec<_>>();
    let modal_area = centered_rect(92, 42, frame.area());
    frame.render_widget(ratatui::widgets::Clear, modal_area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Command Completion | Up/Down select | Tab insert | Esc close "),
            )
            .wrap(ratatui::widgets::Wrap { trim: true }),
        modal_area,
    );
}

fn render_history_search(frame: &mut ratatui::Frame<'_>, search: &PendingHistorySearch) {
    let modal_area = centered_rect(88, 62, frame.area());
    frame.render_widget(ratatui::widgets::Clear, modal_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Draft History | type to search | Up/Down select | Enter restore | Esc close ");
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(format!("Query: {}", search.query))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        chunks[0],
    );

    let visible_rows = usize::from(chunks[1].height).max(1);
    let start = search
        .selected
        .saturating_add(1)
        .saturating_sub(visible_rows);
    let end = (start + visible_rows).min(search.search.matches.len());
    let items = if search.search.matches.is_empty() {
        vec![ListItem::new("(no matches)")]
    } else {
        search.search.matches[start..end]
            .iter()
            .enumerate()
            .map(|(offset, result)| {
                let marker = if start + offset == search.selected {
                    ">"
                } else {
                    " "
                };
                ListItem::new(format!("{marker} {}", result.display))
            })
            .collect()
    };
    frame.render_widget(List::new(items), chunks[1]);
    frame.render_widget(
        Paragraph::new(
            search
                .error
                .as_deref()
                .unwrap_or("History is process-local."),
        ),
        chunks[2],
    );
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

fn render_interaction_overlay(
    frame: &mut ratatui::Frame<'_>,
    layer: InteractionLayer,
    pending_model: Option<&PendingModelSelection>,
    pending_switcher: Option<&SessionSwitcher>,
    pending_history_search: Option<&PendingHistorySearch>,
    active_session_id: &str,
    completion: &CompletionMenu,
) {
    match layer {
        InteractionLayer::Model => {
            if let Some(model) = pending_model {
                render_model_selection(frame, model);
            } else {
                render_modal_state_error(
                    frame,
                    "Model selector state is unavailable; press Esc to continue.",
                );
            }
        }
        InteractionLayer::SessionConfirmation | InteractionLayer::SessionSwitcher => {
            if let Some(switcher) = pending_switcher {
                render_session_switcher(frame, switcher, active_session_id);
            } else {
                render_modal_state_error(
                    frame,
                    "Session selector state is unavailable; press Esc to continue.",
                );
            }
        }
        InteractionLayer::HistorySearch => {
            if let Some(search) = pending_history_search {
                render_history_search(frame, search);
            } else {
                render_modal_state_error(
                    frame,
                    "Draft history state is unavailable; press Esc to continue.",
                );
            }
        }
        InteractionLayer::Completion => render_completion(frame, completion),
        InteractionLayer::RecoverableError => render_modal_state_error(
            frame,
            "Interaction state is unavailable; press Esc to continue.",
        ),
        InteractionLayer::Approval | InteractionLayer::Question | InteractionLayer::Composer => {}
    }
}

struct TuiAgentWorker {
    run_id: String,
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
    fn start(mut self, goal: String, mode: InteractiveAgentMode) -> io::Result<TuiAgentWorker> {
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
            .send((goal, mode, run_id.clone(), steering_receiver))
            .map_err(|_| io::Error::other("prepared TUI worker stopped before activation"))?;
        Ok(TuiAgentWorker {
            run_id,
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
            let Ok((goal, mode, run_id, steering)) = start_rx.recv() else {
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
            .send(Err("cancelled_by_user".to_string()));
    }
    while let Ok(request) = approval_rx.try_recv() {
        let _ = request.reply.send(ApprovalDecision::denied());
    }
    while let Ok(request) = question_rx.try_recv() {
        let _ = request.reply.send(Err("cancelled_by_user".to_string()));
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
        let session_notice = resolution.notice();
        draw_loop(
            terminal,
            project_root,
            profile_id,
            store,
            run_goal,
            active_session_id,
            session_notice,
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

fn restore_terminal_to(output: &mut impl io::Write, raw_result: io::Result<()>) -> io::Result<()> {
    // Attempt all three restorations even if an earlier one fails. In particular,
    // bracketed paste must not remain enabled on a raw-mode or alternate-screen error.
    let paste_result = execute!(output, DisableBracketedPaste);
    let alternate_result = execute!(output, LeaveAlternateScreen);
    let mut errors = Vec::new();
    if let Err(error) = raw_result {
        errors.push(format!("failed to disable raw mode: {error}"));
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
    restore_terminal_to(&mut io::stdout(), disable_raw_mode())
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
    let visual = wrapped_line_count(&composer.input, width.max(1)).clamp(2, 6);
    u16::try_from(visual).unwrap_or(6)
}

fn composer_cursor_cell(input: &str, cursor: usize, width: u16) -> (u16, u16) {
    let width = usize::from(width.max(1));
    let cursor = cursor.min(input.len());
    let prefix = &input[..cursor];
    let mut row = 0u16;
    let mut col = 0usize;
    for character in prefix.chars() {
        if character == '\n' {
            row = row.saturating_add(1);
            col = 0;
            continue;
        }
        let glyph = unicode_display_width(&character.to_string()).max(1);
        if col.saturating_add(glyph) > width {
            row = row.saturating_add(1);
            col = glyph;
        } else {
            col += glyph;
        }
    }
    (
        u16::try_from(col.min(width.saturating_sub(1))).unwrap_or(0),
        row,
    )
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
fn render_current_session_view(
    frame: &mut ratatui::Frame<'_>,
    header: &str,
    status: &str,
    timeline_text: &str,
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
) {
    let mut viewport = TranscriptViewport::default();
    render_current_session_view_with_viewport(
        frame,
        header,
        status,
        timeline_text,
        composer,
        pending_approval,
        pending_question,
        &mut viewport,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_current_session_view_with_viewport(
    frame: &mut ratatui::Frame<'_>,
    header: &str,
    status: &str,
    timeline_text: &str,
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
    viewport: &mut TranscriptViewport,
) {
    let composer_h = composer_height(composer, frame.area().width);
    let dock = pending_approval.is_some() || pending_question.is_some();
    let dock_h = if pending_question.is_some_and(|question| question.error.is_some()) {
        9
    } else if pending_question.is_some() {
        8
    } else if dock {
        7
    } else {
        0
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(composer_h),
            Constraint::Length(1),
        ])
        .split(frame.area());
    frame.render_widget(Paragraph::new(header), chunks[0]);
    let body = if dock {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(dock_h)])
            .split(chunks[2])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1)])
            .split(chunks[2])
    };
    let rendered_rows = wrapped_display_rows(timeline_text, body[0].width.max(1));
    viewport.observe_layout(rendered_rows.len(), usize::from(body[0].height.max(1)));
    frame.render_widget(
        Paragraph::new(format!("{status}  {}", viewport.status_label())),
        chunks[1],
    );
    let visible_start = viewport.top_row();
    let visible_end = visible_start
        .saturating_add(usize::from(body[0].height.max(1)))
        .min(rendered_rows.len());
    frame.render_widget(
        Paragraph::new(rendered_rows[visible_start..visible_end].join("\n")),
        body[0],
    );
    if let Some(req) = pending_approval {
        let mut lines = req.context.lines();
        let first = lines.remove(0);
        let mut text = vec![Line::from(Span::styled(
            format!("approval  {first}"),
            approval_dock_style(std::env::var_os("NO_COLOR").is_some()),
        ))];
        text.extend(lines.into_iter().map(Line::from));
        text.push(Line::from("Keys: Y approve once; N/Esc deny"));
        frame.render_widget(
            Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: true }),
            body[1],
        );
    } else if let Some(question) = pending_question {
        let mut text = vec![Line::from(Span::styled(
            format!("question  {}", question.request.question),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        for (index, option) in question.request.options.iter().enumerate() {
            let marker = if index == question.selected_option {
                "> "
            } else {
                "  "
            };
            text.push(Line::from(format!("{marker}{option}")));
        }
        text.push(Line::from(format!("Response: {}", question.response)));
        if let Some(error) = &question.error {
            text.push(Line::from(format!("[question error] {error}")));
        }
        text.push(Line::from("Enter submit  Esc cancel"));
        frame.render_widget(
            Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: true }),
            body[1],
        );
    }
    frame.render_widget(
        Paragraph::new(composer.input.as_str())
            .style(Style::default().add_modifier(Modifier::BOLD))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        chunks[3],
    );
    let composer_area = chunks[3];
    if composer_area.width > 0 && composer_area.height > 0 {
        let (cx, cy) = composer_cursor_cell(&composer.input, composer.cursor, composer_area.width);
        frame.set_cursor_position(Position {
            x: composer_area
                .x
                .saturating_add(cx.min(composer_area.width.saturating_sub(1))),
            y: composer_area
                .y
                .saturating_add(cy.min(composer_area.height.saturating_sub(1))),
        });
    }
    frame.render_widget(
        Paragraph::new(format!(
            "{}  pgup/pgdn scroll  ctrl+end follow  ctrl+r history  enter send/queue",
            viewport.status_label(),
        )),
        chunks[4],
    );
}

fn draw_loop(
    mut terminal: DefaultTerminal,
    project_root: &Path,
    profile_id: String,
    store: SessionStore,
    run_goal: Option<String>,
    mut active_session_id: String,
    session_notice: String,
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
    timeline.push_status(session_notice);
    let mut worker = if let Some(goal) = run_goal {
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
    };
    timeline.bind_run(worker.as_ref().map(|worker| worker.run_id.clone()));

    let mut pending_approval: Option<TuiApprovalRequest> = None;
    let mut pending_question: Option<PendingQuestion> = None;
    let mut pending_model: Option<PendingModelSelection> = None;
    let mut pending_switcher: Option<SessionSwitcher> = None;
    let mut pending_history_search: Option<PendingHistorySearch> = None;
    let mut composer = Composer::default();
    let mut completion = CompletionMenu::default();
    let mut transcript_viewport = TranscriptViewport::default();
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

        let timeline_text = timeline.rendered_text();
        let interaction_layer = active_interaction_layer(
            pending_approval.is_some(),
            pending_question.is_some(),
            pending_model.is_some(),
            pending_switcher.as_ref(),
            pending_history_search.is_some(),
            completion.is_open(),
        );
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
        let (header, mut status_line) = format_interaction_chrome(
            project_root,
            &profile_id,
            session.as_ref(),
            &timeline.session_id,
            worker_status,
            queued,
        )
        .unwrap_or_else(|error| (error.clone(), error));
        if let Some(notice) = &timeline.notice {
            status_line = format!("{status_line}  {notice}");
        }
        if let Err(error) = terminal.draw(|f| {
            render_current_session_view_with_viewport(
                f,
                &header,
                &status_line,
                &timeline_text,
                &composer,
                pending_approval.as_ref(),
                pending_question.as_ref(),
                &mut transcript_viewport,
            );
            render_interaction_overlay(
                f,
                interaction_layer,
                pending_model.as_ref(),
                pending_switcher.as_ref(),
                pending_history_search.as_ref(),
                &active_session_id,
                &completion,
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
            if let Event::Paste(pasted) = input {
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
            if let Event::Key(key) = input {
                if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                    continue;
                }
                let control_c = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                let control_q = matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                    && key.modifiers.contains(KeyModifiers::CONTROL);
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
                } else if control_c || control_q {
                    Some(reduce_interaction(
                        &interaction_state,
                        InteractionInput::Quit,
                    ))
                } else {
                    None
                };
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
                if global_reduction == Some(InteractionReduction::Quit) {
                    exit_requested_with_active_run = worker.is_some();
                    exit_requested = true;
                    break Ok(());
                }
                let semantic_reduction = match key.code {
                    KeyCode::PageUp => Some(reduce_interaction(
                        &interaction_state,
                        InteractionInput::Transcript(TranscriptViewportAction::PageUp),
                    )),
                    KeyCode::PageDown => Some(reduce_interaction(
                        &interaction_state,
                        InteractionInput::Transcript(TranscriptViewportAction::PageDown),
                    )),
                    KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        Some(reduce_interaction(
                            &interaction_state,
                            InteractionInput::Transcript(TranscriptViewportAction::JumpToEnd),
                        ))
                    }
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
                if let Some(InteractionReduction::Transcript(action)) = semantic_reduction {
                    transcript_viewport.apply(action);
                    continue;
                }
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
                                    completion = CompletionMenu::default();
                                    pending_switcher = None;
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
                    if completion.handle_key(&mut composer, key.code) {
                        continue;
                    }
                    match composer_action_for_key(&mut composer, key.code, key.modifiers) {
                        ComposerAction::Pending => {
                            completion.sync_for(&composer.input, Some(project_root));
                        }
                        ComposerAction::Submit(submitted) => {
                            composer.set_text(submitted);
                            timeline.push_status(
                                "[ui error] completion input could not be submitted".to_string(),
                            );
                        }
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
                                        timeline.push_status(output)
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
                                    Ok(InteractiveEffect::RunAgent { goal, mode }) => {
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
                            | InteractionReduction::ApprovalDecision(_)
                            | InteractionReduction::QuestionAnswered(_)
                            | InteractionReduction::ConfirmationDecision(_)
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
            reply,
        }
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
        assert!(completion.handle_key(&mut composer, KeyCode::Tab));
        assert_eq!(composer.input, "/session");
        assert!(!completion.is_open());

        composer.set_text("/".to_string());
        completion.sync(&composer.input);
        completion.selected = completion
            .suggestions
            .iter()
            .position(|item| item.insertion == "/help")
            .expect("help completion");
        assert!(completion.handle_key(&mut composer, KeyCode::Enter));
        assert_eq!(composer.input, "/help");
        assert!(!completion.is_open());

        composer.input.push('x');
        completion.sync(&composer.input);
        assert!(!completion.is_open(), "unknown commands are not guessed");
        composer.set_text("/skills ".to_string());
        completion.sync(&composer.input);
        assert!(completion.is_open());
        let preserved = composer.input.clone();
        assert!(completion.handle_key(&mut composer, KeyCode::Esc));
        assert_eq!(composer.input, preserved);
        assert!(!completion.is_open());
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
                    "you  persisted request\n\nassistant  persisted reply",
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
        assert!(rendered.contains("sess active-one"));
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
        assert!(timeline.persisted.contains("from b"));
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
            .draw(|frame| render_session_switcher(frame, &switcher, "session-000"))
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
        assert_eq!(timeline.session_id, "detail-session");
        assert!(timeline.persisted.contains("Session: detail-session"));
        assert!(timeline.persisted.contains("Messages: 2"));
        assert!(timeline.persisted.contains("inspect this session"));
        assert!(timeline.persisted.len() <= MAX_SESSION_DETAIL_BYTES);
        assert!(timeline.persisted.lines().count() <= MAX_SESSION_DETAIL_ROWS);
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
            .draw(|frame| render_session_switcher(frame, &switcher, "current-session"))
            .expect("render switcher");

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Resume Confirmation"));
        assert!(rendered.contains("Current: current-session"));
        assert!(rendered.contains("Target:  visible-session"));
    }

    #[test]
    fn completion_and_session_overlays_render_on_small_terminals() {
        let mut completion = CompletionMenu::default();
        completion.sync("/");
        let switcher = SessionSwitcher {
            candidates: vec![InteractiveSessionCandidate {
                id: "small-session".to_string(),
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
                    render_current_session_view(
                        frame,
                        "sess small-session",
                        "idle",
                        "Session: small-session",
                        &Composer::default(),
                        None,
                        None,
                    );
                    render_completion(frame, &completion);
                })
                .expect("render completion on small terminal");
            terminal
                .draw(|frame| render_session_switcher(frame, &switcher, "small-session"))
                .expect("render switcher on small terminal");
        }
    }

    #[test]
    fn renders_every_agent_lifecycle_event() {
        let mut output = LiveOutput::default();
        let events = vec![
            StreamEvent::StateTransition {
                state: "planning".to_string(),
            },
            StreamEvent::PlanGenerated { step_count: 2 },
            StreamEvent::Content("Working".to_string()),
            StreamEvent::ToolCallChunk {
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
                tool_name: "read_file".to_string(),
            },
            StreamEvent::TerminalOutput {
                tool_name: "run_terminal".to_string(),
                stream: "stderr".to_string(),
                chunk: "building\n".to_string(),
                background_task_id: None,
            },
            StreamEvent::ToolCompleted {
                tool_name: "read_file".to_string(),
                success: true,
                output: Some(json!({"content": "nib"})),
                error: None,
            },
            StreamEvent::ToolCompleted {
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
            persisted: "Session: tui-failure-session".to_string(),
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

        assert!(!handle_approval_key(&mut pending, KeyCode::Char('x')));
        assert!(pending.is_some());
        assert!(reply_rx.try_recv().is_err());

        assert!(handle_approval_key(&mut pending, KeyCode::Char('Y')));
        let decision = reply_rx.try_recv().unwrap();
        assert!(decision.granted);
        assert_eq!(decision.source, "user");
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
        let decision = reply_rx.try_recv().unwrap();
        assert!(!decision.granted);
        assert_eq!(decision.source, "denied");
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
            KeyCode::Char('y')
        ));
        assert!(approval.is_none());
        assert!(approval_rx.try_recv().expect("approval reply").granted);
        assert!(question.is_some());
        assert!(question_rx.try_recv().is_err());
        assert_eq!(composer.input, "/");
        assert!(completion.is_open());

        // The next key belongs only to the still-higher-priority question.
        assert!(handle_pending_interaction_key(
            &mut approval,
            &mut question,
            KeyCode::Enter
        ));
        assert_eq!(
            question_rx.try_recv().expect("question reply"),
            Ok("yes".to_string())
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

        assert_eq!(reply_rx.try_recv().unwrap(), Ok("main".to_string()));
        assert!(pending.is_none());
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

        assert_eq!(reply_rx.try_recv().unwrap(), Ok("query".to_string()));
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

        assert!(!handle_question_key(&mut pending, KeyCode::Down));
        assert!(handle_question_key(&mut pending, KeyCode::Enter));
        assert_eq!(reply_rx.try_recv().unwrap(), Ok("execute".to_string()));
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
            Err("question cancelled by user".to_string())
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
        request.reply.send(Ok("execute".to_string())).unwrap();

        assert_eq!(handle.join().unwrap(), Ok("execute".to_string()));
    }

    #[test]
    fn tui_shutdown_cancels_and_joins_a_worker_blocked_on_approval() {
        let directory = tempdir().expect("tempdir");
        save_config(directory.path(), &mock_config()).expect("save mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let goal = "wait for plan approval";
        let mut session = store.create_session();
        session.plan = Some(crate::session::Plan::new(
            goal,
            vec![crate::session::PlanStep {
                description: "wait for approval".to_string(),
                status: "Pending".to_string(),
                outcome: None,
                attempts: 0,
                updated_at: None,
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
        let request = approval_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("worker reached plan approval");
        assert_eq!(request.call.tool_name, "approve_plan");
        let mut pending_approval = Some(request);
        let mut pending_question = None;
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
        let reconciled = timeline
            .live
            .text
            .find("[reconciled] cancelled_by_user")
            .expect("reconciled event was drained");
        let ended = timeline
            .live
            .text
            .find("[stream ended] cancelled_by_user")
            .expect("end event was drained");
        assert!(reconciled < ended);
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
        let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
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
            let request = approval_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("worker reached plan approval");
            assert_eq!(request.call.tool_name, "approve_plan");
            request.reply.send(ApprovalDecision::denied()).unwrap();
            while !worker.is_finished() {
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
        assert!(rendered.contains("sess dock-session"));
        assert!(rendered.contains("inspect wrap"));
        assert!(rendered.contains("approval"));
        assert!(rendered.contains("run_terminal"));
        assert!(rendered.contains("Y approve once"));
        assert!(rendered.contains("task test"));
        assert!(!rendered.contains("{\"command\""));
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
            .draw(|frame| render_session_switcher(frame, &switcher, "session-a"))
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
    fn bracketed_paste_sequences_and_restore_guard_are_deterministic() {
        let mut enabled = Vec::new();
        enable_bracketed_paste_to(&mut enabled).expect("enable paste sequence");
        assert_eq!(enabled, b"\x1b[?2004h");

        let mut restored = Vec::new();
        restore_terminal_to(&mut restored, Ok(())).expect("restore sequences");
        let restored = String::from_utf8(restored).expect("terminal control UTF-8");
        let paste = restored.find("\x1b[?2004l").expect("disable paste");
        let alternate = restored
            .find("\x1b[?1049l")
            .expect("leave alternate screen");
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
                    &CompletionMenu::default(),
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
        terminal
            .draw(|frame| {
                render_current_session_view_with_viewport(
                    frame,
                    "header",
                    "idle",
                    &transcript,
                    &composer,
                    None,
                    None,
                    &mut viewport,
                );
                render_interaction_overlay(
                    frame,
                    InteractionLayer::HistorySearch,
                    None,
                    None,
                    Some(&search),
                    "session-a",
                    &CompletionMenu::default(),
                );
            })
            .expect("render history overlay");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Draft History"));
        assert!(rendered.contains("safe draft 🙂"));
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
        assert!(rendered.contains("tail:following"));
        assert!(rendered.contains("row-31"));
    }

    #[test]
    fn composer_height_follows_unicode_wrap_not_newline_count() {
        let composer = Composer::from_text("a".repeat(200));
        assert_eq!(composer_height(&composer, 80), 3);
        assert_eq!(composer_height(&Composer::default(), 80), 2);
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
        assert_eq!(position.x, 2);
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
