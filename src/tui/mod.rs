//! Minimal ratatui session browser.

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
use tokio::sync::oneshot;

use crate::tools::executor::ApprovalHandler;
use crate::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};

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

pub fn run_tui(project_root: &Path, run_goal: Option<String>) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let terminal = ratatui::init();
    let result = draw_loop(terminal, project_root, run_goal);
    ratatui::restore();
    disable_raw_mode()?;
    execute!(stdout, LeaveAlternateScreen)?;
    result
}

fn draw_loop(
    mut terminal: DefaultTerminal,
    project_root: &Path,
    run_goal: Option<String>,
) -> io::Result<()> {
    let store = SessionStore::new(project_root);
    let mut selected = 0usize;
    let (approval_tx, approval_rx) = mpsc::channel::<TuiApprovalRequest>();
    let (stream_tx, mut stream_rx) =
        tokio::sync::mpsc::channel::<crate::llm::types::StreamEvent>(100);
    let mut streaming_content = String::new();

    if let Some(goal) = run_goal {
        let pr = project_root.to_path_buf();
        let sid = store.create_session().id;
        let tx = approval_tx.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                let loop_cfg = crate::agent::AgentLoopConfig {
                    max_steps: 15,
                    approval_handler: Some(std::sync::Arc::new(TuiApprovalHandler { tx })),
                    stream_tx: Some(stream_tx),
                    ..Default::default()
                };

                let _ = crate::agent::run_agent_loop(pr, &sid, &goal, loop_cfg).await;
            });
        });
    }

    let mut pending_approval: Option<TuiApprovalRequest> = None;

    loop {
        let ids = store.list();
        terminal.draw(|f| {
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

            let stream_para = Paragraph::new(streaming_content.as_str())
                .block(
                    Block::default()
                        .title(" Live Stream ")
                        .borders(Borders::ALL),
                )
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
                        "Approve? [y/N]",
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
            }
        })?;

        while let Ok(event) = stream_rx.try_recv() {
            match event {
                crate::llm::types::StreamEvent::Content(c) => streaming_content.push_str(&c),
                crate::llm::types::StreamEvent::ToolCallChunk { name, .. } => {
                    if let Some(n) = name {
                        streaming_content.push_str(&format!("\n[Tool: {}]\n", n));
                    }
                }
                crate::llm::types::StreamEvent::End(_) => {}
            }
        }

        if let Ok(req) = approval_rx.try_recv() {
            pending_approval = Some(req);
        }

        if event::poll(std::time::Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => {
                        if pending_approval.is_none() {
                            break;
                        }
                    }
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if let Some(req) = pending_approval.take() {
                            let _ = req.reply.send(ApprovalDecision::granted_user());
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        if let Some(req) = pending_approval.take() {
                            let _ = req.reply.send(ApprovalDecision::denied());
                        }
                    }
                    KeyCode::Down if !ids.is_empty() && pending_approval.is_none() => {
                        selected = (selected + 1).min(ids.len() - 1);
                    }
                    KeyCode::Up if selected > 0 && pending_approval.is_none() => selected -= 1,
                    KeyCode::Enter if !ids.is_empty() && pending_approval.is_none() => {
                        show_session_detail(&ids[selected], &store)?;
                    }
                    _ => {
                        if pending_approval.is_some() {
                            // any other key denies
                            if let Some(req) = pending_approval.take() {
                                let _ = req.reply.send(ApprovalDecision::denied());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn show_session_detail(id: &str, store: &SessionStore) -> io::Result<()> {
    let session = store.load(id);
    eprintln!("\n--- Session {id} ---");
    if let Some(s) = session {
        eprintln!("Messages: {}", s.messages.len());
        for m in &s.messages {
            eprintln!("  [{}] {}", m.role, &m.content[..m.content.len().min(120)]);
        }
        eprintln!("Tool calls: {}", s.tool_calls.len());
    }
    Ok(())
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
