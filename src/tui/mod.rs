//! Ratatui session browser with live agent lifecycle rendering.

use crate::llm::types::StreamEvent;
use crate::session::SessionStore;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::DefaultTerminal;
use std::io;
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
}

const MAX_LIVE_OUTPUT_BYTES: usize = 1_048_576;
const OMITTED_OUTPUT_MARKER: &str = "[older live output omitted]\n";
const MAX_SESSION_DETAIL_BYTES: usize = 131_072;
const MAX_SESSION_DETAIL_ITEMS: usize = 100;
const MAX_SESSION_DETAIL_ITEM_CHARS: usize = 500;
const SESSION_DETAIL_TRUNCATED_MARKER: &str = "\n[session detail truncated]\n";

impl LiveOutput {
    fn apply(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Content(content) => self.push_raw(&content),
            StreamEvent::ToolCallChunk {
                name: Some(name), ..
            } if !name.is_empty() => {
                self.push_status(format!("[tool call] {name}"));
            }
            StreamEvent::ToolCallChunk { .. } => {}
            StreamEvent::StateTransition { state } => {
                self.push_status(format!("[state] {state}"));
            }
            StreamEvent::PlanGenerated { step_count } => {
                let noun = if step_count == 1 { "step" } else { "steps" };
                self.push_status(format!("[plan] generated {step_count} {noun}"));
            }
            StreamEvent::ApprovalRequired {
                tool_name,
                arguments,
            } => {
                self.push_status(format!(
                    "[approval required] {tool_name} {}",
                    inline_json(&arguments)
                ));
            }
            StreamEvent::QuestionRequired { question, options } => {
                let options = if options.is_empty() {
                    String::new()
                } else {
                    format!(" (options: {})", options.join(" | "))
                };
                self.push_status(format!("[question] {question}{options}"));
            }
            StreamEvent::ToolStarted {
                tool_name,
                arguments,
            } => {
                self.push_status(format!(
                    "[tool started] {tool_name} {}",
                    inline_json(&arguments)
                ));
            }
            StreamEvent::TerminalOutput {
                tool_name,
                stream,
                chunk,
                background_task_id,
            } => {
                let task = background_task_id
                    .as_deref()
                    .map(|id| format!(" task={id}"))
                    .unwrap_or_default();
                self.push_status(format!(
                    "[terminal {stream}] {tool_name}{task}: {}",
                    chunk.trim_end_matches(['\r', '\n'])
                ));
            }
            StreamEvent::ToolCompleted {
                tool_name,
                success,
                output,
                error,
            } => {
                let status = if success { "ok" } else { "failed" };
                let detail = match (output.as_ref(), error.as_deref()) {
                    (Some(output), Some(error)) => {
                        format!("{}; error: {error}", inline_json(output))
                    }
                    (Some(output), None) => inline_json(output),
                    (None, Some(error)) => error.to_string(),
                    (None, None) => "no result".to_string(),
                };
                self.push_status(format!("[tool completed] {tool_name}: {status} - {detail}"));
            }
            StreamEvent::Compression {
                before_tokens,
                after_tokens,
                summarized_through,
            } => {
                self.push_status(format!(
                    "[compression] {before_tokens} -> {after_tokens} tokens; summarized through message {summarized_through}"
                ));
            }
            StreamEvent::Reconciled { outcome } => {
                self.push_status(format!("[reconciled] {outcome}"));
            }
            StreamEvent::End(reason) => {
                self.push_status(format!("[stream ended] {reason}"));
            }
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

fn inline_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn bottom_scroll(text: &str, width: u16, height: u16) -> u16 {
    if text.is_empty() || width == 0 || height == 0 {
        return 0;
    }
    let width = usize::from(width);
    let visual_lines = text
        .split('\n')
        .map(|line| line.chars().count().max(1).div_ceil(width))
        .sum::<usize>();
    visual_lines
        .saturating_sub(usize::from(height))
        .min(usize::from(u16::MAX)) as u16
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionDetail {
    text: String,
    scroll: u16,
    max_scroll: u16,
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
        } else {
            text.push_str("Session is no longer available.\n");
        }

        truncate_session_detail(&mut text);
        let max_scroll = text
            .lines()
            .count()
            .saturating_sub(1)
            .min(usize::from(u16::MAX)) as u16;
        Self {
            text,
            scroll: 0,
            max_scroll,
        }
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

fn load_session_detail(id: &str, store: &SessionStore) -> io::Result<SessionDetail> {
    let session = store.load_result(id).map_err(io::Error::other)?;
    Ok(SessionDetail::new(id, session.as_ref()))
}

fn list_sessions(store: &SessionStore) -> io::Result<Vec<String>> {
    store
        .list_result()
        .map_err(|error| io::Error::other(format!("failed to list sessions: {error}")))
}

fn handle_session_detail_key(detail: &mut Option<SessionDetail>, code: KeyCode) -> bool {
    let Some(active) = detail.as_mut() else {
        return false;
    };
    match code {
        KeyCode::Esc => {
            *detail = None;
        }
        KeyCode::Up => active.scroll = active.scroll.saturating_sub(1),
        KeyCode::Down => active.scroll = active.scroll.saturating_add(1).min(active.max_scroll),
        KeyCode::PageUp => active.scroll = active.scroll.saturating_sub(10),
        KeyCode::PageDown => {
            active.scroll = active.scroll.saturating_add(10).min(active.max_scroll)
        }
        KeyCode::Home => active.scroll = 0,
        KeyCode::End => active.scroll = active.max_scroll,
        _ => return false,
    }
    true
}

fn render_session_detail(frame: &mut ratatui::Frame<'_>, detail: &SessionDetail) {
    let modal_area = centered_rect(85, 80, frame.area());
    let modal = Paragraph::new(detail.text.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Session Detail | Esc close | Up/Down scroll "),
        )
        .scroll((detail.scroll, 0))
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(ratatui::widgets::Clear, modal_area);
    frame.render_widget(modal, modal_area);
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
        KeyCode::Char(character) => {
            question.response.push(character);
            QuestionAction::Pending
        }
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

struct TuiAgentWorker {
    cancellation: CancellationSignal,
    handle: Option<JoinHandle<()>>,
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
    stream_tx: tokio::sync::mpsc::Sender<StreamEvent>,
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
                    let _ = stream_tx.blocking_send(StreamEvent::End(format!(
                        "failed to initialize the async runtime: {error}"
                    )));
                    return;
                }
            };

            runtime.block_on(async move {
                let loop_cfg = crate::agent::AgentLoopConfig {
                    max_steps: 0,
                    approval_handler: Some(std::sync::Arc::new(TuiApprovalHandler {
                        tx: approval_tx,
                    })),
                    question_handler: Some(std::sync::Arc::new(TuiQuestionHandler {
                        tx: question_tx,
                    })),
                    stream_tx: Some(stream_tx.clone()),
                    cancellation: Some(run_cancellation),
                    ..Default::default()
                };

                if let Err(error) =
                    crate::agent::run_agent_loop(project_root, &session_id, &goal, loop_cfg).await
                {
                    let _ = stream_tx
                        .send(StreamEvent::End(format!("agent error: {error}")))
                        .await;
                }
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
    stream_rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>,
    live_output: &mut LiveOutput,
) {
    while let Ok(event) = stream_rx.try_recv() {
        live_output.apply(event);
    }
}

fn shutdown_agent_worker(
    worker: &mut Option<TuiAgentWorker>,
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
    stream_rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>,
    live_output: &mut LiveOutput,
) -> io::Result<()> {
    let Some(active_worker) = worker.as_mut() else {
        cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
        drain_stream_events(stream_rx, live_output);
        return Ok(());
    };
    active_worker.request_cancellation();
    while !active_worker.is_finished() {
        drain_stream_events(stream_rx, live_output);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    active_worker.join()?;
    cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
    drain_stream_events(stream_rx, live_output);
    *worker = None;
    Ok(())
}

fn reap_finished_worker(
    worker: &mut Option<TuiAgentWorker>,
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
    stream_rx: &mut tokio::sync::mpsc::Receiver<StreamEvent>,
    live_output: &mut LiveOutput,
) -> io::Result<()> {
    if worker.as_ref().is_some_and(TuiAgentWorker::is_finished) {
        cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
        if let Some(worker) = worker.as_mut() {
            worker.join()?;
        }
        *worker = None;
        drain_stream_events(stream_rx, live_output);
    }
    Ok(())
}

pub fn run_tui(project_root: &Path, run_goal: Option<String>) -> io::Result<()> {
    let store = SessionStore::for_project(project_root).map_err(io::Error::other)?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let terminal = ratatui::init();
    let result = draw_loop(terminal, project_root, store, run_goal);
    ratatui::restore();
    disable_raw_mode()?;
    execute!(stdout, LeaveAlternateScreen)?;
    result
}

fn draw_loop(
    mut terminal: DefaultTerminal,
    project_root: &Path,
    store: SessionStore,
    run_goal: Option<String>,
) -> io::Result<()> {
    let mut selected = 0usize;
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (question_tx, question_rx) = mpsc::channel::<TuiQuestionRequest>();
    let (stream_tx, mut stream_rx) =
        tokio::sync::mpsc::channel::<crate::llm::types::StreamEvent>(100);
    let mut live_output = LiveOutput::default();
    let mut worker = if let Some(goal) = run_goal {
        let sid = store.try_create_session().map_err(io::Error::other)?.id;
        Some(spawn_tui_agent_worker(
            project_root.to_path_buf(),
            sid,
            goal,
            approval_tx.clone(),
            question_tx.clone(),
            stream_tx,
        )?)
    } else {
        None
    };

    let mut pending_approval: Option<TuiApprovalRequest> = None;
    let mut pending_question: Option<PendingQuestion> = None;
    let mut session_detail: Option<SessionDetail> = None;

    let loop_result = loop {
        drain_stream_events(&mut stream_rx, &mut live_output);
        if let Err(error) = reap_finished_worker(
            &mut worker,
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
            &mut stream_rx,
            &mut live_output,
        ) {
            break Err(error);
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

        let ids = match list_sessions(&store) {
            Ok(ids) => ids,
            Err(error) => break Err(error),
        };
        if let Err(error) = terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(10),
                    Constraint::Length(3),
                ])
                .split(f.area());

            let items: Vec<ListItem> = if ids.is_empty() {
                vec![ListItem::new("(no sessions)")]
            } else {
                ids.iter()
                    .enumerate()
                    .map(|(i, id)| {
                        let mark = if i == selected { "▶ " } else { "  " };
                        ListItem::new(format!("{mark}{id}"))
                    })
                    .collect()
            };

            let list = List::new(items).block(
                Block::default()
                    .title(" nibble sessions ")
                    .borders(Borders::ALL),
            );
            f.render_widget(list, chunks[0]);

            let stream_scroll = bottom_scroll(
                &live_output.text,
                chunks[1].width.saturating_sub(2),
                chunks[1].height.saturating_sub(2),
            );
            let stream_para = Paragraph::new(live_output.text.as_str())
                .block(
                    Block::default()
                        .title(" Live Stream ")
                        .borders(Borders::ALL),
                )
                .scroll((stream_scroll, 0))
                .wrap(ratatui::widgets::Wrap { trim: true });
            f.render_widget(stream_para, chunks[1]);

            let help = Paragraph::new(Line::from(vec![
                Span::styled("↑/↓", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" select  "),
                Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" detail  "),
                Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" quit"),
            ]))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(help, chunks[2]);
            if let Some(req) = &pending_approval {
                let modal_area = centered_rect(60, 20, f.area());
                let text = vec![
                    Line::from(vec![Span::styled(
                        format!("Approval Required: {}", req.call.tool_name),
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(ratatui::style::Color::Yellow),
                    )]),
                    Line::from(format!("Level: {:?}", req.level)),
                    Line::from(""),
                    Line::from("Arguments:"),
                    Line::from(format!("{}", req.call.arguments)),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Approve? [y/n]",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                ];
                let modal = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Action Required "),
                    )
                    .wrap(ratatui::widgets::Wrap { trim: true });

                f.render_widget(ratatui::widgets::Clear, modal_area);
                f.render_widget(modal, modal_area);
            } else if let Some(question) = &pending_question {
                let modal_height = if question.request.options.is_empty() {
                    30
                } else {
                    45
                };
                let modal_area = centered_rect(70, modal_height, f.area());
                let mut text = vec![
                    Line::from(Span::styled(
                        question.request.question.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                ];
                for (index, option) in question.request.options.iter().enumerate() {
                    let marker = if index == question.selected_option {
                        "> "
                    } else {
                        "  "
                    };
                    text.push(Line::from(format!("{marker}{option}")));
                }
                if !question.request.options.is_empty() {
                    text.push(Line::from(""));
                }
                text.push(Line::from(vec![
                    Span::styled("Response: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(question.response.clone()),
                ]));
                text.push(Line::from(""));
                text.push(Line::from("Enter submit  Esc cancel"));

                let modal = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Input Required "),
                    )
                    .wrap(ratatui::widgets::Wrap { trim: true });
                f.render_widget(ratatui::widgets::Clear, modal_area);
                f.render_widget(modal, modal_area);
            } else if let Some(detail) = &session_detail {
                render_session_detail(f, detail);
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
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if handle_pending_interaction_key(
                    &mut pending_approval,
                    &mut pending_question,
                    key.code,
                ) {
                    continue;
                }
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                    break Ok(());
                }
                if session_detail.is_some() {
                    handle_session_detail_key(&mut session_detail, key.code);
                    continue;
                }
                match key.code {
                    KeyCode::Down if !ids.is_empty() => {
                        selected = (selected + 1).min(ids.len() - 1);
                    }
                    KeyCode::Up if selected > 0 => selected -= 1,
                    KeyCode::Enter if !ids.is_empty() => {
                        session_detail = match load_session_detail(&ids[selected], &store) {
                            Ok(detail) => Some(detail),
                            Err(error) => break Err(error),
                        };
                    }
                    _ => {}
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
        &mut live_output,
    );
    loop_result.and(shutdown)
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
                },
            )]),
            ..Default::default()
        }
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
    fn session_detail_loads_scrolls_and_closes_without_leaving_the_tui() {
        let directory = tempdir().expect("tempdir");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        store.create_session_with_id("detail-session");
        store
            .try_append_message("detail-session", "user", "inspect this session")
            .expect("user message");
        store
            .try_append_message("detail-session", "assistant", "inspection complete")
            .expect("assistant message");

        let detail = load_session_detail("detail-session", &store).expect("load detail");
        assert!(detail.text.contains("Session: detail-session"));
        assert!(detail.text.contains("Messages: 2"));
        assert!(detail.text.contains("inspect this session"));
        assert!(detail.text.len() <= MAX_SESSION_DETAIL_BYTES);

        let mut overlay = Some(detail);
        assert!(handle_session_detail_key(&mut overlay, KeyCode::End));
        assert_eq!(
            overlay.as_ref().map(|detail| detail.scroll),
            overlay.as_ref().map(|detail| detail.max_scroll)
        );
        assert!(!handle_session_detail_key(&mut overlay, KeyCode::Char('x')));
        assert!(overlay.is_some());
        assert!(handle_session_detail_key(&mut overlay, KeyCode::Esc));
        assert!(overlay.is_none());
    }

    #[test]
    fn session_listing_surfaces_store_errors() {
        let directory = tempdir().expect("tempdir");
        let sessions = directory.path().join("sessions");
        let store = SessionStore::at_dir(sessions.clone());
        std::fs::write(sessions.join("invalid session id.json"), "{}").expect("invalid session");

        let error = list_sessions(&store).expect_err("invalid listing must fail");
        assert!(error.to_string().contains("failed to list sessions"));
        assert!(error.to_string().contains("invalid session id"));
    }

    #[test]
    fn session_listing_surfaces_valid_named_corrupt_state() {
        let directory = tempdir().expect("tempdir");
        let sessions = directory.path().join("sessions");
        let store = SessionStore::at_dir(sessions.clone());
        std::fs::write(sessions.join("corrupt-session.json"), "not json").expect("corrupt session");

        let error = list_sessions(&store).expect_err("corrupt listing must fail");
        assert!(error.to_string().contains("failed to list sessions"));
        assert!(error.to_string().contains("parse session JSON"));
    }

    #[test]
    fn session_detail_renders_as_a_bounded_overlay() {
        let detail = SessionDetail {
            text: "Session: visible-session\nMessages: 1\n\n#0 [user]\nvisible content\n"
                .to_string(),
            scroll: 0,
            max_scroll: 4,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| render_session_detail(frame, &detail))
            .expect("render detail");

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Session Detail"));
        assert!(rendered.contains("Session: visible-session"));
        assert!(rendered.contains("visible content"));
        assert!(rendered.contains("Esc close"));
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
    fn approval_modal_only_resolves_on_explicit_decision() {
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let mut pending = Some(TuiApprovalRequest {
            call: ToolCall {
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
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<StreamEvent>(100);
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
        let mut live_output = LiveOutput::default();

        shutdown_agent_worker(
            &mut worker,
            &mut pending_approval,
            &mut pending_question,
            &approval_rx,
            &question_rx,
            &mut stream_rx,
            &mut live_output,
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
        let reconciled = live_output
            .text
            .find("[reconciled] cancelled_by_user")
            .expect("reconciled event was drained");
        let ended = live_output
            .text
            .find("[stream ended] cancelled_by_user")
            .expect("end event was drained");
        assert!(reconciled < ended);
    }
}
