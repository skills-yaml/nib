//! Current-session-first Ratatui interface with live agent lifecycle rendering.

use crate::interactive::{
    apply_stream_event, bottom_scroll_for_wrap, display_stream_event,
    execute_interactive_command_in_state, format_interaction_chrome, interactive_completions,
    interactive_session_candidate, interactive_session_selection, parse_interactive_command,
    parse_queue_line, path_completions, persist_queued_follow_up, project_session_activities,
    queue_disposition_message, resolve_session, set_active_model, steer_unavailable_message,
    take_next_queued_follow_up, unicode_display_width, validate_interactive_session_target,
    wrapped_line_count, ActivityEntry, ActivityKind, InteractiveCompletion, InteractiveEffect,
    InteractiveSessionCandidate, InteractiveSessionSelection, ModelSelection, StreamDisplay,
};
use crate::llm::types::StreamEvent;
use crate::session::SessionStore;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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
use crate::tools::executor::ApprovalHandler;
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

impl LiveOutput {
    fn apply(&mut self, event: StreamEvent) {
        if let StreamEvent::StateTransition { state } = &event {
            self.state = Some(state.clone());
        }
        match display_stream_event(event) {
            Some(StreamDisplay::Content(content)) => self.push_raw(&content),
            Some(StreamDisplay::Status(status)) => self.push_status(status),
            None => {}
        }
    }

    fn push_status(&mut self, status: String) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
        self.text.push_str(&status);
        self.text.push('\n');
        self.enforce_bound();
    }

    fn push_raw(&mut self, content: &str) {
        self.text.push_str(content);
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
    fn new(id: &str, session: Option<&crate::session::Session>) -> Self {
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
                    bounded_preview(&message.content)
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
                    text.push_str(&format!(": {}", bounded_preview(error)));
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
                text.push_str(&format!(
                    "#{} {}: {}\n",
                    event.index,
                    event.kind,
                    bounded_preview(&event.details.to_string())
                ));
            }
        } else {
            text.push_str("Session is no longer available.\n");
        }

        truncate_session_detail(&mut text);
        truncate_session_detail_rows(&mut text);
        Self { text }
    }
}

fn bounded_preview(content: &str) -> String {
    let mut characters = content.chars();
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
    persisted: String,
    activities: Vec<ActivityEntry>,
    live: LiveOutput,
    notice: Option<String>,
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
        Ok(Self::from_session(&session))
    }

    fn from_session(session: &crate::session::Session) -> Self {
        let activities = project_session_activities(session);
        Self {
            session_id: session.id.clone(),
            persisted: SessionDetail::new(&session.id, Some(session)).text,
            activities,
            live: LiveOutput::default(),
            notice: None,
        }
    }

    fn push_status(&mut self, status: String) {
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

    fn apply_event(&mut self, event: StreamEvent) {
        apply_stream_event(&mut self.activities, event.clone(), &mut self.live.state);
        self.live.apply(event);
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
    pub reply: oneshot::Sender<ApprovalDecision>,
}

pub struct TuiApprovalHandler {
    pub tx: mpsc::Sender<TuiApprovalRequest>,
}

#[async_trait::async_trait]
impl ApprovalHandler for TuiApprovalHandler {
    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision {
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = TuiApprovalRequest {
            call: call.clone(),
            level,
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
}

const MAX_COMPOSER_BYTES: usize = 16 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
struct Composer {
    input: String,
    cursor: usize,
}

impl Composer {
    #[cfg(test)]
    fn from_text(input: impl Into<String>) -> Self {
        let input = input.into();
        Self {
            cursor: input.len(),
            input,
        }
    }

    fn set_text(&mut self, input: String) {
        self.cursor = input.len();
        self.input = input;
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

    fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let end = self.cursor;
        self.move_left();
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
        KeyCode::Enter => {
            if composer.input.trim().is_empty() {
                ComposerAction::Pending
            } else {
                let submitted = std::mem::take(&mut composer.input);
                composer.cursor = 0;
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
            KeyCode::Tab => {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractionLayer {
    Approval,
    Question,
    Model,
    SessionConfirmation,
    SessionSwitcher,
    Completion,
    Composer,
}

fn active_interaction_layer(
    approval: bool,
    question: bool,
    model: bool,
    switcher: Option<&SessionSwitcher>,
    completion: bool,
) -> InteractionLayer {
    if approval {
        InteractionLayer::Approval
    } else if question {
        InteractionLayer::Question
    } else if model {
        InteractionLayer::Model
    } else if switcher.is_some_and(|switcher| switcher.confirming) {
        InteractionLayer::SessionConfirmation
    } else if switcher.is_some() {
        InteractionLayer::SessionSwitcher
    } else if completion {
        InteractionLayer::Completion
    } else {
        InteractionLayer::Composer
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
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum QuestionAction {
    Pending,
    Submit(String),
    Cancel,
}

fn question_action_for_key(question: &mut PendingQuestion, code: KeyCode) -> QuestionAction {
    match code {
        KeyCode::Char(character)
            if question.response.len().saturating_add(character.len_utf8())
                <= MAX_COMPOSER_BYTES =>
        {
            question.response.push(character);
            QuestionAction::Pending
        }
        KeyCode::Char(_) => QuestionAction::Pending,
        KeyCode::Backspace => {
            question.response.pop();
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
            let response = question.response.trim();
            if !response.is_empty() {
                QuestionAction::Submit(response.to_string())
            } else if let Some(option) = question.request.options.get(question.selected_option) {
                QuestionAction::Submit(option.clone())
            } else {
                QuestionAction::Pending
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
    }
}

fn approval_decision_for_key(code: KeyCode) -> Option<ApprovalDecision> {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalDecision::granted_user()),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ApprovalDecision::denied()),
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
    let mut text = vec![
        Line::from(Span::styled(
            format!("Select model for {}", pending.selection.provider),
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
                "{selected}{}. {model}{current}",
                index + 1
            )));
        }
    }
    text.push(Line::from(""));
    text.push(Line::from(vec![
        Span::styled("Model: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(pending.response.clone()),
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
        return match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => SwitcherAction::Activate,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
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
    let mut replacement = ActiveTimeline::from_session(&session);
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

struct TuiAgentWorker {
    cancellation: CancellationSignal,
    handle: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct SessionStreamEvent {
    session_id: String,
    event: StreamEvent,
}

impl TuiAgentWorker {
    fn request_cancellation(&self) {
        self.cancellation.cancel();
    }

    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
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

fn spawn_tui_agent_worker(
    project_root: std::path::PathBuf,
    session_id: String,
    goal: String,
    approval_tx: mpsc::Sender<TuiApprovalRequest>,
    question_tx: mpsc::Sender<TuiQuestionRequest>,
    stream_tx: tokio::sync::mpsc::Sender<SessionStreamEvent>,
) -> io::Result<TuiAgentWorker> {
    let cancellation = CancellationSignal::new();
    let run_cancellation = cancellation.clone();
    let handle = std::thread::Builder::new()
        .name("nib-tui-agent".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build();
            let runtime = match runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = stream_tx.blocking_send(SessionStreamEvent {
                        session_id,
                        event: StreamEvent::End(format!(
                            "failed to initialize the async runtime: {error}"
                        )),
                    });
                    return;
                }
            };

            runtime.block_on(async move {
                let (agent_stream_tx, mut agent_stream_rx) =
                    tokio::sync::mpsc::channel::<StreamEvent>(100);
                let forwarding_session_id = session_id.clone();
                let forwarding_tx = stream_tx.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(event) = agent_stream_rx.recv().await {
                        if forwarding_tx
                            .send(SessionStreamEvent {
                                session_id: forwarding_session_id.clone(),
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
                    approval_handler: Some(std::sync::Arc::new(TuiApprovalHandler {
                        tx: approval_tx,
                    })),
                    question_handler: Some(std::sync::Arc::new(TuiQuestionHandler {
                        tx: question_tx,
                    })),
                    stream_tx: Some(agent_stream_tx.clone()),
                    cancellation: Some(run_cancellation),
                    ..Default::default()
                };

                if let Err(error) =
                    crate::agent::run_agent_loop(project_root, &session_id, &goal, loop_cfg).await
                {
                    let _ = agent_stream_tx
                        .send(StreamEvent::End(format!("agent error: {error}")))
                        .await;
                }
                drop(agent_stream_tx);
                let _ = forwarder.await;
            });
        })?;
    Ok(TuiAgentWorker {
        cancellation,
        handle: Some(handle),
    })
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

fn drain_stream_events(
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
) {
    while let Ok(event) = stream_rx.try_recv() {
        if event.session_id == timeline.session_id {
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
    let Some(active_worker) = worker.as_mut() else {
        cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
        drain_stream_events(stream_rx, timeline);
        return Ok(());
    };
    active_worker.request_cancellation();
    while !active_worker.is_finished() {
        drain_stream_events(stream_rx, timeline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    active_worker.join()?;
    cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
    drain_stream_events(stream_rx, timeline);
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
    }
    Ok(())
}

pub fn run_tui(
    project_root: &Path,
    run_goal: Option<String>,
    requested_session: Option<String>,
) -> io::Result<()> {
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

    // Terminal ownership is established before session resolution. A terminal startup
    // failure therefore cannot create a session or submit the optional initial goal.
    let result = (|| {
        let store = SessionStore::for_project(project_root).map_err(io::Error::other)?;
        let resolution =
            resolve_session(&store, requested_session.as_deref()).map_err(io::Error::other)?;
        let active_session_id = resolution.session_id().to_string();
        let session_notice = resolution.notice();
        draw_loop(
            terminal,
            project_root,
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

fn restore_terminal() -> io::Result<()> {
    let raw_result = disable_raw_mode();
    let alternate_result = execute!(io::stdout(), LeaveAlternateScreen);
    match (raw_result, alternate_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(raw), Err(alternate)) => Err(io::Error::other(format!(
            "failed to disable raw mode: {raw}; failed to leave alternate screen: {alternate}"
        ))),
    }
}

struct TerminalRestoreGuard {
    active: bool,
}

impl TerminalRestoreGuard {
    fn active() -> Self {
        Self { active: true }
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        restore_terminal()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = restore_terminal();
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

fn tui_report_cancelled_run(
    store: &SessionStore,
    session_id: &str,
    timeline: &mut ActiveTimeline,
) -> Result<String, String> {
    timeline.push_status("[cancelled] active agent run".to_string());
    let message = queue_disposition_message(store, session_id, "cancelled")?;
    timeline.push_status(message.clone());
    Ok(message)
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

fn render_current_session_view(
    frame: &mut ratatui::Frame<'_>,
    header: &str,
    status: &str,
    timeline_text: &str,
    composer: &Composer,
    pending_approval: Option<&TuiApprovalRequest>,
    pending_question: Option<&PendingQuestion>,
) {
    let composer_h = composer_height(composer, frame.area().width);
    let dock = pending_approval.is_some() || pending_question.is_some();
    let dock_h = if pending_question.is_some() {
        8
    } else if dock {
        6
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
    frame.render_widget(Paragraph::new(status), chunks[1]);
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
    let stream_scroll =
        bottom_scroll_for_wrap(timeline_text, body[0].width.max(1), body[0].height.max(1));
    frame.render_widget(
        Paragraph::new(timeline_text)
            .scroll((stream_scroll, 0))
            .wrap(ratatui::widgets::Wrap { trim: true }),
        body[0],
    );
    if let Some(req) = pending_approval {
        let text = vec![
            Line::from(Span::styled(
                format!("approval  {} {:?}", req.call.tool_name, req.level),
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(ratatui::style::Color::Yellow),
            )),
            Line::from(format!("{}", req.call.arguments)),
            Line::from("Y approve  N/Esc deny  D stays in this dock"),
        ];
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
        Paragraph::new(
            "enter send/queue  ctrl+j newline  ctrl+s steer (unavailable)  ctrl+c cancel  ctrl+q quit",
        ),
        chunks[4],
    );
}

fn draw_loop(
    mut terminal: DefaultTerminal,
    project_root: &Path,
    store: SessionStore,
    run_goal: Option<String>,
    mut active_session_id: String,
    session_notice: String,
) -> io::Result<Option<String>> {
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(100);
    let mut timeline = ActiveTimeline::load(&store, &active_session_id)?;
    timeline.push_status(session_notice);
    let mut worker = if let Some(goal) = run_goal {
        Some(spawn_tui_agent_worker(
            project_root.to_path_buf(),
            active_session_id.clone(),
            goal,
            approval_tx.clone(),
            question_tx.clone(),
            stream_tx.clone(),
        )?)
    } else {
        None
    };

    let mut pending_approval: Option<TuiApprovalRequest> = None;
    let mut pending_question: Option<PendingQuestion> = None;
    let mut pending_model: Option<PendingModelSelection> = None;
    let mut pending_switcher: Option<SessionSwitcher> = None;
    let mut composer = Composer::default();
    let mut completion = CompletionMenu::default();
    let mut exit_notice = None;

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
        if worker_finished && worker.is_none() {
            match take_next_queued_follow_up(&store, &active_session_id) {
                Ok(Some(goal)) => {
                    timeline.push_status(format!("[user] {goal}"));
                    match spawn_tui_agent_worker(
                        project_root.to_path_buf(),
                        active_session_id.clone(),
                        goal,
                        approval_tx.clone(),
                        question_tx.clone(),
                        stream_tx.clone(),
                    ) {
                        Ok(next) => worker = Some(next),
                        Err(error) => break Err(error),
                    }
                }
                Ok(None) => {}
                Err(error) => timeline.push_status(format!("[queue error] {error}")),
            }
        }
        if pending_approval.is_none() {
            if let Ok(request) = approval_rx.try_recv() {
                pending_approval = Some(request);
            }
        }
        if pending_question.is_none() {
            if let Ok(request) = question_rx.try_recv() {
                pending_question = Some(PendingQuestion::new(request));
            }
        }

        let timeline_text = timeline.rendered_text();
        let interaction_layer = active_interaction_layer(
            pending_approval.is_some(),
            pending_question.is_some(),
            pending_model.is_some(),
            pending_switcher.as_ref(),
            completion.is_open(),
        );
        let worker_status = if pending_approval.is_some() || pending_question.is_some() {
            "awaiting you"
        } else if worker.is_some() && timeline.live.state.as_deref() == Some("reconciliation") {
            "reconciling"
        } else if worker.is_some() {
            "running"
        } else {
            "idle"
        };
        let session = store.load_result(&timeline.session_id).ok().flatten();
        let queued = session
            .as_ref()
            .map(|session| session.queued_follow_ups.len())
            .unwrap_or(0);
        let (header, mut status_line) = format_interaction_chrome(
            project_root,
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
            render_current_session_view(
                f,
                &header,
                &status_line,
                &timeline_text,
                &composer,
                pending_approval.as_ref(),
                pending_question.as_ref(),
            );
            if interaction_layer == InteractionLayer::Model {
                let model = pending_model
                    .as_ref()
                    .expect("model layer requires a pending model selection");
                render_model_selection(f, model);
            } else if matches!(
                interaction_layer,
                InteractionLayer::SessionConfirmation | InteractionLayer::SessionSwitcher
            ) {
                let switcher = pending_switcher
                    .as_ref()
                    .expect("session layer requires a pending switcher");
                render_session_switcher(f, switcher, &active_session_id);
            } else if interaction_layer == InteractionLayer::Completion {
                render_completion(f, &completion);
            }
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
            if let Event::Key(key) = input {
                if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                    continue;
                }
                let control_c = matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                let control_q = matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
                    && key.modifiers.contains(KeyModifiers::CONTROL);
                if control_c && worker.is_some() {
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
                if control_c || control_q {
                    exit_notice = Some(
                        tui_exit_disposition(&store, &active_session_id)
                            .unwrap_or_else(|error| error),
                    );
                    break Ok(());
                }
                let interaction_layer = active_interaction_layer(
                    pending_approval.is_some(),
                    pending_question.is_some(),
                    pending_model.is_some(),
                    pending_switcher.as_ref(),
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
                if interaction_layer == InteractionLayer::Model {
                    let model = pending_model
                        .as_mut()
                        .expect("model layer requires a pending model selection");
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
                    let switcher = pending_switcher
                        .as_mut()
                        .expect("session layer requires a pending switcher");
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
                if interaction_layer == InteractionLayer::Completion
                    && completion.handle_key(&mut composer, key.code)
                {
                    continue;
                }
                if matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    timeline.push_status(steer_unavailable_message().to_string());
                    continue;
                }
                match composer_action_for_key(&mut composer, key.code, key.modifiers) {
                    ComposerAction::Pending => {
                        completion.sync_for(&composer.input, Some(project_root))
                    }
                    ComposerAction::Submit(submitted) => {
                        completion = CompletionMenu::default();
                        let normalized = submitted.trim();
                        let parsed = match parse_interactive_command(normalized) {
                            Ok(parsed) => parsed,
                            Err(error) => {
                                composer.set_text(submitted);
                                completion.sync_for(&composer.input, Some(project_root));
                                timeline.push_status(format!("[command error] {error}"));
                                continue;
                            }
                        };
                        if matches!(
                            parsed,
                            Some(crate::interactive::InteractiveCommand::Session)
                                | Some(crate::interactive::InteractiveCommand::Resume)
                        ) {
                            match load_session_switcher(&store, &active_session_id) {
                                Ok(switcher) => pending_switcher = Some(switcher),
                                Err(error) => {
                                    composer.set_text(submitted);
                                    completion.sync_for(&composer.input, Some(project_root));
                                    timeline.push_status(format!(
                                        "[command error] could not open session switcher: {error}"
                                    ));
                                }
                            }
                            continue;
                        }
                        if let Some(queued) = parse_queue_line(normalized) {
                            match persist_queued_follow_up(
                                &store,
                                &active_session_id,
                                queued,
                                "composer",
                            ) {
                                Ok(_) => timeline.push_status(format!(
                                    "queued follow-up retained on session {active_session_id}"
                                )),
                                Err(error) => {
                                    timeline.push_status(format!("[queue error] {error}"))
                                }
                            }
                            continue;
                        }
                        if worker.is_some() {
                            if parsed == Some(crate::interactive::InteractiveCommand::Quit) {
                                exit_notice = Some(
                                    tui_exit_disposition(&store, &active_session_id)
                                        .unwrap_or_else(|error| error),
                                );
                                break Ok(());
                            }
                            if parsed.is_some() {
                                composer.set_text(submitted);
                                completion.sync_for(&composer.input, Some(project_root));
                                timeline.push_status(
                                    "Agent is still running; cancel it or wait before running a command."
                                        .to_string(),
                                );
                                continue;
                            }
                            match persist_queued_follow_up(
                                &store,
                                &active_session_id,
                                normalized,
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
                            continue;
                        }
                        if let Some(command) = parsed {
                            match execute_interactive_command_in_state(
                                command,
                                project_root,
                                &store,
                                &active_session_id,
                                worker_status,
                            ) {
                                Ok(InteractiveEffect::Quit) => {
                                    exit_notice = Some(
                                        tui_exit_disposition(&store, &active_session_id)
                                            .unwrap_or_else(|error| error),
                                    );
                                    break Ok(());
                                }
                                Ok(InteractiveEffect::Output(output)) => {
                                    timeline.push_status(output)
                                }
                                Ok(InteractiveEffect::SessionChanged { session_id, output }) => {
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
                                Ok(InteractiveEffect::SubmitGoal { goal }) => {
                                    timeline.push_status(format!("[user] {goal}"));
                                    worker = Some(spawn_tui_agent_worker(
                                        project_root.to_path_buf(),
                                        active_session_id.clone(),
                                        goal,
                                        approval_tx.clone(),
                                        question_tx.clone(),
                                        stream_tx.clone(),
                                    )?);
                                }
                                Err(error) => {
                                    composer.set_text(submitted);
                                    completion.sync_for(&composer.input, Some(project_root));
                                    timeline.push_status(format!("[command error] {error}"))
                                }
                            }
                        } else {
                            let goal = normalized.to_string();
                            timeline.push_status(format!("[user] {goal}"));
                            worker = Some(spawn_tui_agent_worker(
                                project_root.to_path_buf(),
                                active_session_id.clone(),
                                goal,
                                approval_tx.clone(),
                                question_tx.clone(),
                                stream_tx.clone(),
                            )?);
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
    loop_result.and(shutdown).map(|()| exit_notice)
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
    use crate::interactive::execute_interactive_command;
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::tempdir;

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
    fn completion_navigation_inserts_without_clearing_on_escape() {
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
                arguments: json!({"command": "task test"}),
            },
            StreamEvent::QuestionRequired {
                question: "Choose a mode".to_string(),
                options: vec!["plan".to_string(), "execute".to_string()],
            },
            StreamEvent::ToolStarted {
                tool_name: "read_file".to_string(),
                arguments: json!({"path": "README.md"}),
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
            output.apply(event);
        }

        for expected in [
            "[state] planning",
            "[plan] generated 2 steps",
            "Working\n[tool call] read_file",
            "[approval required] run_terminal {\"command\":\"task test\"}",
            "[question] Choose a mode (options: plan | execute)",
            "[tool started] read_file {\"path\":\"README.md\"}",
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
    fn live_output_retains_a_bounded_utf8_tail() {
        let mut output = LiveOutput::default();
        output.apply(StreamEvent::Content(
            "é".repeat(MAX_LIVE_OUTPUT_BYTES).to_string(),
        ));

        assert!(output.text.len() <= MAX_LIVE_OUTPUT_BYTES);
        assert!(output.text.starts_with(OMITTED_OUTPUT_MARKER));
        assert!(output.text.is_char_boundary(output.text.len()));
        assert!(output.text.ends_with('é'));
    }

    #[test]
    fn late_stream_events_for_another_session_are_ignored() {
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<SessionStreamEvent>(4);
        stream_tx
            .try_send(SessionStreamEvent {
                session_id: "old-session".to_string(),
                event: StreamEvent::Content("must not leak".to_string()),
            })
            .expect("old event");
        stream_tx
            .try_send(SessionStreamEvent {
                session_id: "active-session".to_string(),
                event: StreamEvent::Content("visible active output".to_string()),
            })
            .expect("active event");
        let mut timeline = ActiveTimeline {
            session_id: "active-session".to_string(),
            ..ActiveTimeline::default()
        };

        drain_stream_events(&mut stream_rx, &mut timeline);

        assert_eq!(timeline.live.text, "visible active output");
    }

    #[test]
    fn approval_modal_only_resolves_on_explicit_decision() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(TuiApprovalRequest {
            call: ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task test"}),
                session_id: None,
                project_root: None,
            },
            level: PermissionLevel::Destructive,
            reply: reply_tx,
        });

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
        let mut pending = Some(TuiApprovalRequest {
            call: ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "apply_patch".to_string(),
                arguments: json!({}),
                session_id: None,
                project_root: None,
            },
            level: PermissionLevel::Destructive,
            reply: reply_tx,
        });

        assert!(handle_approval_key(&mut pending, KeyCode::Char('n')));
        let decision = reply_rx.try_recv().unwrap();
        assert!(!decision.granted);
        assert_eq!(decision.source, "denied");
    }

    #[test]
    fn approval_consumes_input_before_question_and_completion_layers() {
        let (approval_tx, mut approval_rx) = oneshot::channel();
        let mut approval = Some(TuiApprovalRequest {
            call: ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({}),
                session_id: None,
                project_root: None,
            },
            level: PermissionLevel::Destructive,
            reply: approval_tx,
        });
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
    fn interaction_layer_precedence_is_total_and_deterministic() {
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
                InteractionLayer::Approval,
            ),
            (
                false,
                true,
                true,
                Some(&confirming),
                true,
                InteractionLayer::Question,
            ),
            (
                false,
                false,
                true,
                Some(&confirming),
                true,
                InteractionLayer::Model,
            ),
            (
                false,
                false,
                false,
                Some(&confirming),
                true,
                InteractionLayer::SessionConfirmation,
            ),
            (
                false,
                false,
                false,
                Some(&browsing),
                true,
                InteractionLayer::SessionSwitcher,
            ),
            (
                false,
                false,
                false,
                None,
                true,
                InteractionLayer::Completion,
            ),
            (false, false, false, None, false, InteractionLayer::Composer),
        ];

        for (approval, question, model, switcher, completion, expected) in cases {
            assert_eq!(
                active_interaction_layer(approval, question, model, switcher, completion),
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
                directory.path().to_path_buf(),
                session.id.clone(),
                goal.to_string(),
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
                directory.path().to_path_buf(),
                session.id.clone(),
                goal.to_string(),
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
        let approval = TuiApprovalRequest {
            call: ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "task test"}),
                session_id: None,
                project_root: None,
            },
            level: PermissionLevel::Destructive,
            reply: approval_tx,
        };
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
        assert!(rendered.contains("Y approve"));
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
        let cancelled =
            tui_report_cancelled_run(&store, "session-a", &mut timeline).expect("cancel");
        assert!(cancelled.contains("cancelled;"));
        assert!(cancelled.contains("retained on session session-a"));
        assert!(timeline.rendered_text().contains(&cancelled));
        assert!(timeline
            .rendered_text()
            .contains("[cancelled] active agent run"));

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
