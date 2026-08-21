//! Shared interactive command grammar and effects for chat and TUI surfaces.

use crate::config::{load_nib_config_full, update_nib_config};
use crate::llm::types::StreamEvent;
use crate::session::{QueuedFollowUp, Session, SessionStore};
use crate::{mcp_cmd, skill_cmd};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::path::Path;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

/// Presentation-neutral metadata for one interactive command.
///
/// Chat help and TUI completion are both derived from this registry. Parser dispatch
/// validates its token through the same registry before interpreting arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractiveCommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub summary: &'static str,
    pub fixed_subcommands: &'static [&'static str],
    pub gated_reason: Option<&'static str>,
}

const fn spec(
    name: &'static str,
    aliases: &'static [&'static str],
    usage: &'static str,
    summary: &'static str,
    fixed_subcommands: &'static [&'static str],
    gated_reason: Option<&'static str>,
) -> InteractiveCommandSpec {
    InteractiveCommandSpec {
        name,
        aliases,
        usage,
        summary,
        fixed_subcommands,
        gated_reason,
    }
}

pub const INTERACTIVE_COMMANDS: &[InteractiveCommandSpec] = &[
    spec(
        "status",
        &[],
        "/status",
        "Show session, model, permissions, plan, and queue",
        &[],
        None,
    ),
    spec(
        "model",
        &[],
        "/model [name]",
        "List models or select an exact model ID",
        &[],
        None,
    ),
    spec(
        "providers",
        &[],
        "/providers",
        "List configured providers",
        &[],
        None,
    ),
    spec(
        "permissions",
        &[],
        "/permissions [manual|smart|policy|off]",
        "Inspect or set the configured approval mode",
        &["manual", "smart", "policy", "off"],
        None,
    ),
    spec(
        "plan",
        &[],
        "/plan [prompt]",
        "Show the current plan or request planning",
        &[],
        None,
    ),
    spec(
        "review",
        &[],
        "/review",
        "Review authoritative workspace changes",
        &[],
        None,
    ),
    spec(
        "diff",
        &[],
        "/diff",
        "Show the session workspace diff",
        &[],
        None,
    ),
    spec(
        "compact",
        &[],
        "/compact",
        "Request bounded context compression",
        &[],
        Some("unavailable: explicit compact waits on T003's user-facing compact request"),
    ),
    spec(
        "session",
        &[],
        "/session",
        "Show or switch the active session",
        &[],
        None,
    ),
    spec(
        "resume",
        &[],
        "/resume",
        "Preview and confirm resuming another session",
        &[],
        None,
    ),
    spec("new", &[], "/new", "Start a fresh session", &[], None),
    spec("clear", &[], "/clear", "Start a fresh session", &[], None),
    spec(
        "fork",
        &[],
        "/fork",
        "Branch a new session from the current transcript",
        &[],
        None,
    ),
    spec(
        "rename",
        &[],
        "/rename <name>",
        "Set the current session display name",
        &[],
        None,
    ),
    spec(
        "copy",
        &[],
        "/copy",
        "Print the latest completed assistant output",
        &[],
        None,
    ),
    spec(
        "ps",
        &[],
        "/ps",
        "List session-owned background work",
        &[],
        Some("unavailable: session-owned process listing waits on FT-017"),
    ),
    spec(
        "stop",
        &[],
        "/stop",
        "Stop session-owned background work",
        &[],
        Some("unavailable: session-owned process stop waits on FT-017"),
    ),
    spec(
        "skills",
        &[],
        "/skills [list|install <url_or_path>|remove <name>]",
        "Manage installed skills",
        &["list", "install", "remove"],
        None,
    ),
    spec(
        "mcp",
        &[],
        "/mcp [list|add <name> <command> [args...]|remove <name>]",
        "Manage MCP servers",
        &["list", "add", "remove"],
        None,
    ),
    spec(
        "help",
        &[],
        "/help",
        "Show interactive command help",
        &[],
        None,
    ),
    spec(
        "quit",
        &["exit", "q"],
        "/quit (aliases: /exit, /q)",
        "Exit the interactive session",
        &[],
        None,
    ),
];

const MAX_QUEUED_FOLLOW_UPS: usize = 16;
const MAX_QUEUE_TEXT_BYTES: usize = 16 * 1024;
const MAX_DISPLAY_NAME_BYTES: usize = 128;
const MAX_ACTIVITY_BODY_BYTES: usize = 8 * 1024;
const MAX_DIFF_BYTES: usize = 32 * 1024;
const STEER_UNAVAILABLE: &str = "Steer is unavailable until the agent loop can bind instructions to the exact active run. Enter queues the next turn.";

const MAX_INTERACTIVE_SESSION_ITEMS: usize = 100;
const MAX_INTERACTIVE_SESSION_PREVIEW_CHARS: usize = 2_000;
const MAX_INTERACTIVE_SESSION_ITEM_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveCompletion {
    /// Text inserted into the composer. A trailing space means a free-form argument
    /// is still required and is intentionally not guessed.
    pub insertion: String,
    pub usage: &'static str,
    pub summary: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveSessionCandidate {
    pub id: String,
    pub preview: String,
    pub is_active: bool,
    pub(crate) snapshot_token: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveSessionSelection {
    pub candidates: Vec<InteractiveSessionCandidate>,
    pub omitted: usize,
}

pub fn interactive_session_selection(
    store: &SessionStore,
    active_session_id: &str,
) -> Result<InteractiveSessionSelection, String> {
    let ids = store
        .list_result()
        .map_err(|error| format!("failed to list sessions: {error}"))?;
    let mut candidates = Vec::with_capacity(ids.len());
    for id in ids {
        let session = load_session_strict(store, &id)?;
        let last_activity = session_last_activity(&session);
        candidates.push((
            last_activity,
            InteractiveSessionCandidate::from_session(&session, active_session_id)?,
        ));
    }
    candidates.sort_by(|(left_activity, left), (right_activity, right)| {
        right_activity
            .cmp(left_activity)
            .then_with(|| right.id.cmp(&left.id))
    });

    let omitted = candidates
        .len()
        .saturating_sub(MAX_INTERACTIVE_SESSION_ITEMS);
    if candidates.len() > MAX_INTERACTIVE_SESSION_ITEMS {
        let retained_active = candidates
            .iter()
            .position(|(_, candidate)| candidate.is_active)
            .filter(|index| *index >= MAX_INTERACTIVE_SESSION_ITEMS)
            .map(|index| candidates[index].clone());
        candidates.truncate(MAX_INTERACTIVE_SESSION_ITEMS);
        if let Some(active) = retained_active {
            if let Some(last) = candidates.last_mut() {
                *last = active;
            }
        }
    }

    Ok(InteractiveSessionSelection {
        candidates: candidates
            .into_iter()
            .map(|(_, candidate)| candidate)
            .collect(),
        omitted,
    })
}

pub fn interactive_session_candidate(
    store: &SessionStore,
    session_id: &str,
    active_session_id: &str,
) -> Result<InteractiveSessionCandidate, String> {
    let session = load_session_strict(store, session_id)?;
    InteractiveSessionCandidate::from_session(&session, active_session_id)
}

pub fn validate_interactive_session_target(
    store: &SessionStore,
    candidate: &InteractiveSessionCandidate,
) -> Result<Session, String> {
    let session = load_session_strict(store, &candidate.id)?;
    if session_snapshot_token(&session)? != candidate.snapshot_token {
        return Err(format!(
            "session {} changed since it was previewed; open /session and preview it again",
            candidate.id
        ));
    }
    Ok(session)
}

fn load_session_strict(store: &SessionStore, session_id: &str) -> Result<Session, String> {
    store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .ok_or_else(|| format!("session {session_id} no longer exists"))
}

fn session_last_activity(session: &Session) -> Option<chrono::DateTime<chrono::Utc>> {
    session
        .messages
        .iter()
        .filter_map(|message| message.timestamp)
        .chain(session.tool_calls.iter().filter_map(|call| call.timestamp))
        .chain(session.events.iter().filter_map(|event| event.timestamp))
        .chain(
            session
                .plan
                .iter()
                .flat_map(|plan| plan.steps.iter().filter_map(|step| step.updated_at)),
        )
        .max()
        .or(session.started_at)
}

impl InteractiveSessionCandidate {
    fn from_session(session: &Session, active_session_id: &str) -> Result<Self, String> {
        let last_activity = session_last_activity(session);
        let latest_user = session
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| bounded_session_preview(&message.content))
            .unwrap_or_else(|| "(no user message)".to_string());
        let plan = session.plan.as_ref().map_or_else(
            || "none".to_string(),
            |plan| {
                format!(
                    "step {}/{}; outcome={}",
                    plan.current_step_index.min(plan.steps.len()),
                    plan.steps.len(),
                    plan.outcome.as_deref().unwrap_or("active")
                )
            },
        );
        let tail_start = session.messages.len().saturating_sub(3);
        let transcript_tail = session.messages[tail_start..]
            .iter()
            .map(|message| {
                format!(
                    "[{}] {}",
                    message.role,
                    bounded_session_preview(&message.content)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let preview = format!(
            "Session: {}\nLast activity: {}\nLatest user message: {}\nPlan: {}\n\n{}",
            session.id,
            last_activity
                .map(|timestamp| timestamp.to_rfc3339())
                .unwrap_or_else(|| "unknown".to_string()),
            latest_user,
            plan,
            transcript_tail
        )
        .chars()
        .take(MAX_INTERACTIVE_SESSION_PREVIEW_CHARS)
        .collect();
        Ok(Self {
            id: session.id.clone(),
            preview,
            is_active: session.id == active_session_id,
            snapshot_token: session_snapshot_token(session)?,
        })
    }
}

fn session_snapshot_token(session: &Session) -> Result<[u8; 32], String> {
    let encoded = serde_json::to_vec(session)
        .map_err(|error| format!("failed to fingerprint session {}: {error}", session.id))?;
    Ok(Sha256::digest(encoded).into())
}

fn bounded_session_preview(content: &str) -> String {
    let mut characters = content.chars();
    let mut preview: String = characters
        .by_ref()
        .take(MAX_INTERACTIVE_SESSION_ITEM_CHARS)
        .collect();
    if characters.next().is_some() {
        preview.push_str("...");
    }
    preview
}

pub fn interactive_help() -> String {
    let mut help = String::from("Commands:");
    for command in INTERACTIVE_COMMANDS {
        help.push_str(&format!("\n  {:<62} {}", command.usage, command.summary));
        if let Some(reason) = command.gated_reason {
            help.push_str(&format!("\n    {reason}"));
        }
    }
    help.push_str(
        "\n  queue: <text>                                                Queue a follow-up for the next turn",
    );
    help.push_str(&format!("\n  {STEER_UNAVAILABLE}"));
    help
}

pub fn interactive_completions(input: &str) -> Vec<InteractiveCompletion> {
    const MAX_COMPLETIONS: usize = 32;

    if !input.starts_with('/') || input.contains(['\n', '\r']) {
        return Vec::new();
    }
    let command_line = &input[1..];
    let mut parts = command_line.split_whitespace();
    let command_prefix = parts.next().unwrap_or_default();
    let has_separator = command_line
        .get(command_prefix.len()..)
        .and_then(|tail| tail.chars().next())
        .is_some_and(char::is_whitespace);
    let remaining: Vec<&str> = parts.collect();

    if !has_separator && remaining.is_empty() {
        let prefix = command_prefix.to_ascii_lowercase();
        let mut completions = Vec::new();
        for spec in INTERACTIVE_COMMANDS {
            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                if name.to_ascii_lowercase().starts_with(&prefix) {
                    completions.push(InteractiveCompletion {
                        insertion: format!("/{name}"),
                        usage: spec.usage,
                        summary: spec.summary,
                    });
                }
            }
        }
        completions.truncate(MAX_COMPLETIONS);
        return completions;
    }

    let Some(spec) = find_command_spec(command_prefix) else {
        return Vec::new();
    };
    if remaining.len() > 1 || spec.fixed_subcommands.is_empty() {
        return Vec::new();
    }
    let subcommand_prefix = remaining.first().copied().unwrap_or_default();
    if !subcommand_prefix.is_empty()
        && command_line.chars().last().is_some_and(char::is_whitespace)
        && spec
            .fixed_subcommands
            .iter()
            .any(|subcommand| subcommand.eq_ignore_ascii_case(subcommand_prefix))
    {
        return Vec::new();
    }
    spec.fixed_subcommands
        .iter()
        .copied()
        .filter(|subcommand| {
            subcommand
                .to_ascii_lowercase()
                .starts_with(&subcommand_prefix.to_ascii_lowercase())
        })
        .take(MAX_COMPLETIONS)
        .map(|subcommand| InteractiveCompletion {
            insertion: completion_insertion(spec.name, subcommand),
            usage: spec.usage,
            summary: spec.summary,
        })
        .collect()
}

fn completion_insertion(command: &str, subcommand: &str) -> String {
    let needs_argument = matches!(
        (command, subcommand),
        ("skills", "install" | "remove") | ("mcp", "add" | "remove")
    );
    format!(
        "/{command} {subcommand}{}",
        if needs_argument { " " } else { "" }
    )
}

fn find_command_spec(token: &str) -> Option<&'static InteractiveCommandSpec> {
    INTERACTIVE_COMMANDS.iter().find(|spec| {
        spec.name.eq_ignore_ascii_case(token)
            || spec
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(token))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillCommand {
    List,
    Install { source: String },
    Remove { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    List,
    Add {
        name: String,
        command: String,
        args: Vec<String>,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveCommand {
    Quit,
    Help,
    Status,
    Providers,
    Permissions { selection: Option<String> },
    Plan { prompt: Option<String> },
    Review,
    Diff,
    Compact,
    Session,
    Resume,
    New,
    Clear,
    Fork,
    Rename { name: String },
    Copy,
    Ps,
    Stop,
    Model { selection: Option<String> },
    Skills(SkillCommand),
    Mcp(McpCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: String,
    pub current: String,
    pub available: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveEffect {
    Quit,
    Output(String),
    SessionChanged { session_id: String, output: String },
    SelectSession(InteractiveSessionSelection),
    SelectModel(ModelSelection),
    SubmitGoal { goal: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerSubmitKind {
    IdleTurn,
    QueueNext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    User,
    Assistant,
    Plan,
    Tool,
    Approval,
    Question,
    Compression,
    Reconcile,
    Cancellation,
    Failure,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub kind: ActivityKind,
    pub title: String,
    pub body: String,
}

impl ActivityKind {
    pub fn role_label(self) -> &'static str {
        match self {
            Self::User => "you",
            Self::Assistant => "assistant",
            Self::Plan => "plan",
            Self::Tool => "tool",
            Self::Approval => "approval",
            Self::Question => "question",
            Self::Compression => "compression",
            Self::Reconcile => "reconcile",
            Self::Cancellation => "cancel",
            Self::Failure => "fail",
            Self::System => "system",
        }
    }
}

impl ActivityEntry {
    pub fn render_line(&self) -> String {
        if self.body.is_empty() {
            format!("{}  {}", self.kind.role_label(), self.title)
        } else {
            format!("{}  {}\n{}", self.kind.role_label(), self.title, self.body)
        }
    }
}

pub fn classify_composer_submit(worker_active: bool) -> ComposerSubmitKind {
    if worker_active {
        ComposerSubmitKind::QueueNext
    } else {
        ComposerSubmitKind::IdleTurn
    }
}

pub fn steer_unavailable_message() -> &'static str {
    STEER_UNAVAILABLE
}

pub fn parse_queue_line(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    trimmed
        .strip_prefix("queue:")
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

pub fn persist_queued_follow_up(
    store: &SessionStore,
    session_id: &str,
    text: &str,
    source: &str,
) -> Result<QueuedFollowUp, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("queued follow-up must not be empty".to_string());
    }
    if text.len() > MAX_QUEUE_TEXT_BYTES {
        return Err(format!(
            "queued follow-up must be at most {MAX_QUEUE_TEXT_BYTES} bytes"
        ));
    }
    store
        .update_session(session_id, |session| {
            if session.queued_follow_ups.len() >= MAX_QUEUED_FOLLOW_UPS {
                return Err(crate::session::SessionError::InvalidMutation(format!(
                    "at most {MAX_QUEUED_FOLLOW_UPS} queued follow-ups are retained"
                )));
            }
            let item = QueuedFollowUp {
                id: Uuid::new_v4().to_string(),
                text: text.to_string(),
                created_at: Utc::now(),
                source: source.to_string(),
            };
            session.queued_follow_ups.push(item.clone());
            Ok(item)
        })
        .map_err(|error| format!("failed to persist queued follow-up: {error}"))
}

pub fn queued_follow_up_count(store: &SessionStore, session_id: &str) -> Result<usize, String> {
    Ok(store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .map(|session| session.queued_follow_ups.len())
        .unwrap_or(0))
}

pub fn queue_disposition_message(
    store: &SessionStore,
    session_id: &str,
    action: &str,
) -> Result<String, String> {
    let count = queued_follow_up_count(store, session_id)?;
    if count == 0 {
        Ok(format!("{action}; no queued follow-ups."))
    } else {
        Ok(format!(
            "{action}; {count} queued follow-up(s) retained on session {session_id}."
        ))
    }
}

pub fn take_next_queued_follow_up(
    store: &SessionStore,
    session_id: &str,
) -> Result<Option<String>, String> {
    store
        .update_session(session_id, |session| {
            if session.queued_follow_ups.is_empty() {
                return Ok(None);
            }
            Ok(Some(session.queued_follow_ups.remove(0).text))
        })
        .map_err(|error| format!("failed to take queued follow-up: {error}"))
}

pub fn unicode_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn wrapped_line_count(text: &str, width: u16) -> usize {
    if width == 0 {
        return 1;
    }
    let width = usize::from(width);
    text.split('\n')
        .map(|line| unicode_display_width(line).max(1).div_ceil(width))
        .sum::<usize>()
        .max(1)
}

pub fn bottom_scroll_for_wrap(text: &str, width: u16, height: u16) -> u16 {
    if text.is_empty() || width == 0 || height == 0 {
        return 0;
    }
    wrapped_line_count(text, width)
        .saturating_sub(usize::from(height))
        .min(usize::from(u16::MAX)) as u16
}

pub fn project_session_activities(session: &Session) -> Vec<ActivityEntry> {
    let mut activities = Vec::new();
    if let Some(name) = session.display_name.as_deref() {
        activities.push(ActivityEntry {
            kind: ActivityKind::System,
            title: format!("session {name}"),
            body: String::new(),
        });
    }
    if let Some(parent) = session.forked_from.as_deref() {
        activities.push(ActivityEntry {
            kind: ActivityKind::System,
            title: format!("forked from {parent}"),
            body: String::new(),
        });
    }
    for message in &session.messages {
        let kind = if message.role.eq_ignore_ascii_case("user") {
            ActivityKind::User
        } else {
            ActivityKind::Assistant
        };
        activities.push(ActivityEntry {
            kind,
            title: String::new(),
            body: bounded_activity_body(&message.content),
        });
    }
    if let Some(plan) = &session.plan {
        activities.push(plan_activity(plan));
    }
    for call in &session.tool_calls {
        let name = call.tool_name.as_deref().unwrap_or("tool");
        let status = if call.error.is_some() { "failed" } else { "ok" };
        let body = call
            .error
            .clone()
            .or_else(|| call.result.as_ref().map(inline_json))
            .unwrap_or_default();
        activities.push(ActivityEntry {
            kind: ActivityKind::Tool,
            title: format!("{name} {status}"),
            body: bounded_activity_body(&body),
        });
    }
    if let Some(summary) = &session.summary {
        activities.push(ActivityEntry {
            kind: ActivityKind::Compression,
            title: format!("summarized through message {}", session.summary_index),
            body: bounded_activity_body(summary),
        });
    }
    activities
}

pub fn apply_stream_event(
    activities: &mut Vec<ActivityEntry>,
    event: StreamEvent,
    live_state: &mut Option<String>,
) {
    match event {
        StreamEvent::StateTransition { state } => {
            *live_state = Some(state.clone());
            activities.push(ActivityEntry {
                kind: ActivityKind::System,
                title: format!("state {state}"),
                body: String::new(),
            });
        }
        StreamEvent::Content(content) => {
            if let Some(last) = activities.last_mut() {
                if last.kind == ActivityKind::Assistant && last.title == "live" {
                    last.body.push_str(&content);
                    last.body = bounded_activity_body(&last.body);
                    return;
                }
            }
            activities.push(ActivityEntry {
                kind: ActivityKind::Assistant,
                title: "live".to_string(),
                body: bounded_activity_body(&content),
            });
        }
        StreamEvent::ToolCallChunk {
            name: Some(name), ..
        } if !name.is_empty() => activities.push(ActivityEntry {
            kind: ActivityKind::Tool,
            title: format!("{name} requested"),
            body: String::new(),
        }),
        StreamEvent::ToolCallChunk { .. } => {}
        StreamEvent::PlanGenerated { step_count } => {
            let noun = if step_count == 1 { "step" } else { "steps" };
            activities.push(ActivityEntry {
                kind: ActivityKind::Plan,
                title: format!("generated {step_count} {noun}"),
                body: String::new(),
            });
        }
        StreamEvent::ApprovalRequired {
            tool_name,
            arguments,
        } => activities.push(ActivityEntry {
            kind: ActivityKind::Approval,
            title: format!("{tool_name} needs approval"),
            body: bounded_activity_body(&inline_json(&arguments)),
        }),
        StreamEvent::QuestionRequired { question, options } => {
            let options = if options.is_empty() {
                String::new()
            } else {
                format!("options: {}", options.join(" | "))
            };
            activities.push(ActivityEntry {
                kind: ActivityKind::Question,
                title: question,
                body: options,
            });
        }
        StreamEvent::ToolStarted {
            tool_name,
            arguments,
        } => activities.push(ActivityEntry {
            kind: ActivityKind::Tool,
            title: format!("{tool_name} running"),
            body: bounded_activity_body(&inline_json(&arguments)),
        }),
        StreamEvent::TerminalOutput {
            tool_name, chunk, ..
        } => {
            if let Some(last) = activities.iter_mut().rev().find(|entry| {
                entry.kind == ActivityKind::Tool && entry.title.starts_with(&tool_name)
            }) {
                if !last.body.is_empty() && !last.body.ends_with('\n') {
                    last.body.push('\n');
                }
                last.body.push_str(chunk.trim_end_matches(['\r', '\n']));
                last.body = bounded_activity_body(&last.body);
            }
        }
        StreamEvent::ToolCompleted {
            tool_name,
            success,
            output,
            error,
        } => {
            let status = if success { "ok" } else { "failed" };
            let detail = match (output.as_ref(), error.as_deref()) {
                (Some(output), Some(error)) => format!("{}; error: {error}", inline_json(output)),
                (Some(output), None) => inline_json(output),
                (None, Some(error)) => error.to_string(),
                (None, None) => "no result".to_string(),
            };
            activities.push(ActivityEntry {
                kind: ActivityKind::Tool,
                title: format!("{tool_name} {status}"),
                body: bounded_activity_body(&detail),
            });
        }
        StreamEvent::Compression {
            before_tokens,
            after_tokens,
            summarized_through,
        } => activities.push(ActivityEntry {
            kind: ActivityKind::Compression,
            title: format!("{before_tokens} -> {after_tokens} tokens"),
            body: format!("summarized through message {summarized_through}"),
        }),
        StreamEvent::Reconciled { outcome } => activities.push(ActivityEntry {
            kind: ActivityKind::Reconcile,
            title: outcome,
            body: String::new(),
        }),
        StreamEvent::Failure {
            failure,
            session_id,
        } => activities.push(ActivityEntry {
            kind: ActivityKind::Failure,
            title: "LLM request failed".to_string(),
            body: bounded_activity_body(&failure.user_report(session_id.as_deref())),
        }),
        StreamEvent::End(reason) => {
            let kind = if reason.to_ascii_lowercase().contains("cancel") {
                ActivityKind::Cancellation
            } else {
                ActivityKind::System
            };
            activities.push(ActivityEntry {
                kind,
                title: reason,
                body: String::new(),
            });
        }
    }
}

fn plan_activity(plan: &crate::session::Plan) -> ActivityEntry {
    let current = plan.current_step_index.min(plan.steps.len());
    let title = plan
        .steps
        .get(plan.current_step_index)
        .map(|step| step.description.as_str())
        .unwrap_or("complete");
    let body = plan
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| format!("{}. [{}] {}", index + 1, step.status, step.description))
        .collect::<Vec<_>>()
        .join("\n");
    ActivityEntry {
        kind: ActivityKind::Plan,
        title: format!("{current}/{} {title}", plan.steps.len()),
        body: bounded_activity_body(&body),
    }
}

fn bounded_activity_body(content: &str) -> String {
    if content.len() <= MAX_ACTIVITY_BODY_BYTES {
        return content.to_string();
    }
    let mut end = MAX_ACTIVITY_BODY_BYTES.saturating_sub(3);
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &content[..end])
}

pub fn format_interaction_chrome(
    project_root: &Path,
    session: Option<&Session>,
    session_id: &str,
    lifecycle: &str,
    queued: usize,
) -> Result<(String, String), String> {
    let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    let provider = config.llm.get_active_provider();
    let model = config
        .llm
        .get_provider(None)
        .map(|entry| entry.model.clone())
        .unwrap_or_else(|| "unconfigured".to_string());
    let reasoning = config
        .llm
        .get_provider(None)
        .and_then(|entry| entry.reasoning_effort)
        .map(|effort| format!(" {effort:?}"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = session
        .and_then(|session| session.display_name.as_deref())
        .unwrap_or("");
    let name = if name.is_empty() {
        String::new()
    } else {
        format!(" \"{name}\"")
    };
    let origin = if session
        .and_then(|session| session.forked_from.as_deref())
        .is_some()
    {
        "forked"
    } else if session
        .and_then(|session| session.messages.first())
        .is_some()
    {
        "resumed"
    } else {
        "local"
    };
    let worktree = session
        .and_then(|session| session.tool_calls.last())
        .and_then(|call| call.worktree_path.as_deref())
        .unwrap_or("-");
    let header = format!(
        "{project}  ·  sess {session_id}{name}  ·  {origin}  ·  {worktree}",
        project = project_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
    );
    let plan = session
        .and_then(|session| session.plan.as_ref())
        .map(|plan| {
            let current = plan.current_step_index.min(plan.steps.len());
            let title = plan
                .steps
                .get(plan.current_step_index)
                .map(|step| step.description.as_str())
                .unwrap_or("complete");
            format!("  ·  plan {current}/{} {title}", plan.steps.len())
        })
        .unwrap_or_default();
    let status = format!(
        "{lifecycle}  ·  {provider}/{model}{reasoning}  ·  {}/{} net {}  ·  queue {queued}{plan}",
        config.approvals.mode, config.execution.provider, config.execution.boundaries.network
    );
    Ok((header, status))
}

pub fn format_session_status(
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    lifecycle: &str,
) -> Result<String, String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?;
    let queued = session
        .as_ref()
        .map(|session| session.queued_follow_ups.len())
        .unwrap_or(0);
    let (header, status) = format_interaction_chrome(
        project_root,
        session.as_ref(),
        session_id,
        lifecycle,
        queued,
    )?;
    Ok(format!("{header}\n{status}"))
}

pub fn path_completions(project_root: &Path, input: &str) -> Vec<InteractiveCompletion> {
    const MAX_PATHS: usize = 32;
    let Some(at) = input.rfind('@') else {
        return Vec::new();
    };
    if input[..at].contains(['\n', '\r']) {
        return Vec::new();
    }
    let prefix = &input[at + 1..];
    if prefix.contains([' ', '\n', '\r']) {
        return Vec::new();
    }
    let mut matches = Vec::new();
    collect_project_paths(project_root, project_root, prefix, 0, &mut matches);
    matches.sort();
    matches.truncate(MAX_PATHS);
    matches
        .into_iter()
        .map(|path| InteractiveCompletion {
            insertion: format!("{}@{path}", &input[..at]),
            usage: "@path",
            summary: "Attach a project path",
        })
        .collect()
}

fn collect_project_paths(
    root: &Path,
    current: &Path,
    prefix: &str,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth > 3 || out.len() >= 64 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.starts_with('.')
            || relative.starts_with("target/")
            || relative == "target"
            || relative.starts_with("node_modules/")
        {
            continue;
        }
        if relative.starts_with(prefix) {
            out.push(relative.clone());
        }
        if path.is_dir() {
            collect_project_paths(root, &path, prefix, depth + 1, out);
        }
    }
}

fn gated_command_output(name: &str) -> Result<InteractiveEffect, String> {
    let spec = find_command_spec(name).expect("gated command is registered");
    let reason = spec.gated_reason.expect("gated command requires a reason");
    Ok(InteractiveEffect::Output(format!("/{name} {reason}")))
}

fn bounded_workspace_diff(project_root: &Path) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(["-C", &project_root.to_string_lossy(), "diff", "--no-color"])
        .output()
        .map_err(|error| format!("failed to read git diff: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git diff failed: {}",
            stderr
                .trim()
                .lines()
                .next()
                .unwrap_or("repository unavailable")
        ));
    }
    let mut diff = String::from_utf8_lossy(&output.stdout).into_owned();
    if diff.trim().is_empty() {
        return Ok("No workspace diff.".to_string());
    }
    if diff.len() > MAX_DIFF_BYTES {
        diff.truncate(MAX_DIFF_BYTES);
        while !diff.is_char_boundary(diff.len()) {
            diff.pop();
        }
        diff.push_str("\n[diff truncated]");
    }
    Ok(diff)
}

fn format_current_plan(session: Option<&Session>) -> String {
    match session.and_then(|session| session.plan.as_ref()) {
        Some(plan) => plan_activity(plan).render_line(),
        None => "No plan in the active session.".to_string(),
    }
}

fn latest_assistant_output(session: Option<&Session>) -> Result<String, String> {
    session
        .and_then(|session| {
            session
                .messages
                .iter()
                .rev()
                .find(|message| message.role.eq_ignore_ascii_case("assistant"))
        })
        .map(|message| message.content.clone())
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "no completed assistant output to copy".to_string())
}

fn fork_session(store: &SessionStore, session_id: &str) -> Result<(String, String), String> {
    let source = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .ok_or_else(|| format!("session {session_id} no longer exists"))?;
    let created = store
        .try_create_session()
        .map_err(|error| format!("failed to fork session: {error}"))?;
    store
        .update_session(&created.id, |session| {
            session.messages = source.messages.clone();
            session.tool_calls = source.tool_calls.clone();
            session.plan = source.plan.clone();
            session.summary = source.summary.clone();
            session.summary_index = source.summary_index;
            session.events = source.events.clone();
            session.active_skills = source.active_skills.clone();
            session.skill_usage = source.skill_usage.clone();
            session.display_name = source.display_name.clone();
            session.forked_from = Some(source.id.clone());
            session.queued_follow_ups.clear();
            Ok(())
        })
        .map_err(|error| format!("failed to copy forked session: {error}"))?;
    Ok((
        created.id.clone(),
        format!("Forked session {} from {session_id}.", created.id),
    ))
}

fn rename_session(store: &SessionStore, session_id: &str, name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("usage: /rename <name>".to_string());
    }
    if name.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(format!(
            "session name must be at most {MAX_DISPLAY_NAME_BYTES} bytes"
        ));
    }
    store
        .update_session(session_id, |session| {
            session.display_name = Some(name.to_string());
            Ok(())
        })
        .map_err(|error| format!("failed to rename session: {error}"))?;
    Ok(format!("Renamed session {session_id} to '{name}'."))
}

fn set_approval_mode(project_root: &Path, mode: &str) -> Result<String, String> {
    let mode = mode.trim().to_ascii_lowercase();
    if !matches!(mode.as_str(), "manual" | "smart" | "policy" | "off") {
        return Err("usage: /permissions [manual|smart|policy|off]".to_string());
    }
    update_nib_config(project_root, {
        let selected = mode.clone();
        move |config| {
            config.approvals.mode = selected;
            Ok(())
        }
    })
    .map_err(|error| format!("failed saving permissions: {error}"))?;
    Ok(format!("Approval mode set to '{mode}'."))
}

fn format_permissions(project_root: &Path) -> Result<String, String> {
    let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    Ok(format!(
        "Effective permissions:\n  approval: {}\n  execution: {}\n  profile: {}\n  network: {}\n  plan_mode: {}\nConfigured UI selection cannot weaken AGENTS.md, skill, worktree, sandbox, or platform limits.",
        config.approvals.mode,
        config.execution.provider,
        config.execution.default_profile,
        config.execution.boundaries.network,
        config.execution.plan_mode
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionResolution {
    Created(String),
    Resumed(String),
    RequestedMissing { requested: String, created: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDisplay {
    Content(String),
    Status(String),
}

pub fn display_stream_event(event: StreamEvent) -> Option<StreamDisplay> {
    let display = match event {
        StreamEvent::Content(content) => StreamDisplay::Content(content),
        StreamEvent::ToolCallChunk {
            name: Some(name), ..
        } if !name.is_empty() => StreamDisplay::Status(format!("[tool call] {name}")),
        StreamEvent::ToolCallChunk { .. } => return None,
        StreamEvent::StateTransition { state } => {
            StreamDisplay::Status(format!("[state] {state}"))
        }
        StreamEvent::PlanGenerated { step_count } => {
            let noun = if step_count == 1 { "step" } else { "steps" };
            StreamDisplay::Status(format!("[plan] generated {step_count} {noun}"))
        }
        StreamEvent::ApprovalRequired {
            tool_name,
            arguments,
        } => StreamDisplay::Status(format!(
            "[approval required] {tool_name} {}",
            inline_json(&arguments)
        )),
        StreamEvent::QuestionRequired { question, options } => {
            let options = if options.is_empty() {
                String::new()
            } else {
                format!(" (options: {})", options.join(" | "))
            };
            StreamDisplay::Status(format!("[question] {question}{options}"))
        }
        StreamEvent::ToolStarted {
            tool_name,
            arguments,
        } => StreamDisplay::Status(format!(
            "[tool started] {tool_name} {}",
            inline_json(&arguments)
        )),
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
            StreamDisplay::Status(format!(
                "[terminal {stream}] {tool_name}{task}: {}",
                chunk.trim_end_matches(['\r', '\n'])
            ))
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
            StreamDisplay::Status(format!(
                "[tool completed] {tool_name}: {status} - {detail}"
            ))
        }
        StreamEvent::Compression {
            before_tokens,
            after_tokens,
            summarized_through,
        } => StreamDisplay::Status(format!(
            "[compression] {before_tokens} -> {after_tokens} tokens; summarized through message {summarized_through}"
        )),
        StreamEvent::Reconciled { outcome } => {
            StreamDisplay::Status(format!("[reconciled] {outcome}"))
        }
        StreamEvent::Failure {
            failure,
            session_id,
        } => StreamDisplay::Status(failure.user_report(session_id.as_deref())),
        StreamEvent::End(reason) => StreamDisplay::Status(format!("[stream ended] {reason}")),
    };
    Some(display)
}

fn inline_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

impl SessionResolution {
    pub fn session_id(&self) -> &str {
        match self {
            Self::Created(id) | Self::Resumed(id) => id,
            Self::RequestedMissing { created, .. } => created,
        }
    }

    pub fn notice(&self) -> String {
        match self {
            Self::Created(id) => format!("Started session {id}."),
            Self::Resumed(id) => format!("Resumed session {id}."),
            Self::RequestedMissing { requested, created } => {
                format!("Session {requested} was not found; started session {created}.")
            }
        }
    }
}

pub fn resolve_session(
    store: &SessionStore,
    requested: Option<&str>,
) -> Result<SessionResolution, String> {
    if let Some(requested) = requested {
        if store
            .load_result(requested)
            .map_err(|error| format!("failed to load requested session {requested}: {error}"))?
            .is_some()
        {
            return Ok(SessionResolution::Resumed(requested.to_string()));
        }
        let created = store
            .try_create_session()
            .map_err(|error| format!("failed to create session: {error}"))?
            .id;
        return Ok(SessionResolution::RequestedMissing {
            requested: requested.to_string(),
            created,
        });
    }

    Ok(SessionResolution::Created(
        store
            .try_create_session()
            .map_err(|error| format!("failed to create session: {error}"))?
            .id,
    ))
}

pub fn parse_interactive_command(input: &str) -> Result<Option<InteractiveCommand>, String> {
    let Some(command_line) = input.trim().strip_prefix('/') else {
        return Ok(None);
    };
    let mut parts = command_line.split_whitespace();
    let command = parts.next().unwrap_or_default().to_ascii_lowercase();
    let arguments: Vec<String> = parts.map(ToString::to_string).collect();

    if command.is_empty() {
        return Err("empty slash command; use /help".to_string());
    }
    let Some(spec) = find_command_spec(&command) else {
        return Err(format!("unknown command: /{command}; use /help"));
    };
    let command = spec.name;

    let parsed = match command {
        "quit" if arguments.is_empty() => InteractiveCommand::Quit,
        "help" if arguments.is_empty() => InteractiveCommand::Help,
        "status" if arguments.is_empty() => InteractiveCommand::Status,
        "providers" if arguments.is_empty() => InteractiveCommand::Providers,
        "permissions" => InteractiveCommand::Permissions {
            selection: if arguments.is_empty() {
                None
            } else {
                Some(arguments.join(" "))
            },
        },
        "plan" => InteractiveCommand::Plan {
            prompt: if arguments.is_empty() {
                None
            } else {
                Some(arguments.join(" "))
            },
        },
        "review" if arguments.is_empty() => InteractiveCommand::Review,
        "diff" if arguments.is_empty() => InteractiveCommand::Diff,
        "compact" if arguments.is_empty() => InteractiveCommand::Compact,
        "session" if arguments.is_empty() => InteractiveCommand::Session,
        "resume" if arguments.is_empty() => InteractiveCommand::Resume,
        "new" if arguments.is_empty() => InteractiveCommand::New,
        "clear" if arguments.is_empty() => InteractiveCommand::Clear,
        "fork" if arguments.is_empty() => InteractiveCommand::Fork,
        "rename" => InteractiveCommand::Rename {
            name: arguments.join(" "),
        },
        "copy" if arguments.is_empty() => InteractiveCommand::Copy,
        "ps" if arguments.is_empty() => InteractiveCommand::Ps,
        "stop" if arguments.is_empty() => InteractiveCommand::Stop,
        "model" => InteractiveCommand::Model {
            selection: if arguments.is_empty() {
                None
            } else {
                Some(arguments.join(" "))
            },
        },
        "skills" => InteractiveCommand::Skills(parse_skill_command(&arguments)?),
        "mcp" => InteractiveCommand::Mcp(parse_mcp_command(&arguments)?),
        _ => return Err(format!("usage: {}", spec.usage)),
    };
    Ok(Some(parsed))
}

fn parse_skill_command(arguments: &[String]) -> Result<SkillCommand, String> {
    if arguments.is_empty() || (arguments.len() == 1 && arguments[0] == "list") {
        return Ok(SkillCommand::List);
    }
    match arguments {
        [command, source] if command == "install" => Ok(SkillCommand::Install {
            source: source.clone(),
        }),
        [command, name] if command == "remove" => Ok(SkillCommand::Remove { name: name.clone() }),
        _ => Err(
            "usage: /skills list | /skills install <url_or_path> | /skills remove <name>"
                .to_string(),
        ),
    }
}

fn parse_mcp_command(arguments: &[String]) -> Result<McpCommand, String> {
    if arguments.is_empty() || (arguments.len() == 1 && arguments[0] == "list") {
        return Ok(McpCommand::List);
    }
    match arguments {
        [command, name, executable, args @ ..] if command == "add" => Ok(McpCommand::Add {
            name: name.clone(),
            command: executable.clone(),
            args: args.to_vec(),
        }),
        [command, name] if command == "remove" => Ok(McpCommand::Remove { name: name.clone() }),
        _ => Err(
            "usage: /mcp list | /mcp add <name> <command> [args...] | /mcp remove <name>"
                .to_string(),
        ),
    }
}

pub fn execute_interactive_command(
    command: InteractiveCommand,
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
) -> Result<InteractiveEffect, String> {
    execute_interactive_command_in_state(command, project_root, store, session_id, "idle")
}

pub fn execute_interactive_command_in_state(
    command: InteractiveCommand,
    project_root: &Path,
    store: &SessionStore,
    session_id: &str,
    lifecycle: &str,
) -> Result<InteractiveEffect, String> {
    match command {
        InteractiveCommand::Quit => Ok(InteractiveEffect::Quit),
        InteractiveCommand::Help => Ok(InteractiveEffect::Output(interactive_help())),
        InteractiveCommand::Status => Ok(InteractiveEffect::Output(format_session_status(
            project_root,
            store,
            session_id,
            lifecycle,
        )?)),
        InteractiveCommand::Permissions { selection: None } => {
            Ok(InteractiveEffect::Output(format_permissions(project_root)?))
        }
        InteractiveCommand::Permissions {
            selection: Some(mode),
        } => Ok(InteractiveEffect::Output(set_approval_mode(
            project_root,
            &mode,
        )?)),
        InteractiveCommand::Plan { prompt: None } => {
            let session = store
                .load_result(session_id)
                .map_err(|error| format!("failed to load session {session_id}: {error}"))?;
            Ok(InteractiveEffect::Output(format_current_plan(
                session.as_ref(),
            )))
        }
        InteractiveCommand::Plan {
            prompt: Some(prompt),
        } => Ok(InteractiveEffect::SubmitGoal { goal: prompt }),
        InteractiveCommand::Review | InteractiveCommand::Diff => Ok(InteractiveEffect::Output(
            bounded_workspace_diff(project_root)?,
        )),
        InteractiveCommand::Compact => gated_command_output("compact"),
        InteractiveCommand::Ps => gated_command_output("ps"),
        InteractiveCommand::Stop => gated_command_output("stop"),
        InteractiveCommand::Resume | InteractiveCommand::Session => Ok(
            InteractiveEffect::SelectSession(interactive_session_selection(store, session_id)?),
        ),
        InteractiveCommand::New | InteractiveCommand::Clear => {
            let new_session = store
                .try_create_session()
                .map_err(|error| format!("failed to create session: {error}"))?;
            Ok(InteractiveEffect::SessionChanged {
                output: format!("Started fresh session {}.", new_session.id),
                session_id: new_session.id,
            })
        }
        InteractiveCommand::Fork => {
            let (session_id, output) = fork_session(store, session_id)?;
            Ok(InteractiveEffect::SessionChanged { session_id, output })
        }
        InteractiveCommand::Rename { name } => Ok(InteractiveEffect::Output(rename_session(
            store, session_id, &name,
        )?)),
        InteractiveCommand::Copy => {
            let session = store
                .load_result(session_id)
                .map_err(|error| format!("failed to load session {session_id}: {error}"))?;
            Ok(InteractiveEffect::Output(latest_assistant_output(
                session.as_ref(),
            )?))
        }
        InteractiveCommand::Providers => {
            let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
            let active = config.llm.get_active_provider();
            let mut output = String::from("Configured providers:");
            if config.llm.providers.is_empty() {
                output.push_str("\n  (none - using mock)");
            }
            for (name, entry) in &config.llm.providers {
                let marker = if name == &active { " (active)" } else { "" };
                output.push_str(&format!("\n  - {name}: {}{marker}", entry.model));
            }
            Ok(InteractiveEffect::Output(output))
        }
        InteractiveCommand::Model { selection: None } => {
            let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
            let provider = config.llm.get_active_provider();
            let current = config
                .llm
                .get_provider(None)
                .map(|entry| entry.model.clone())
                .unwrap_or_default();
            Ok(InteractiveEffect::SelectModel(ModelSelection {
                available: config.llm.get_available_models(None),
                provider,
                current,
            }))
        }
        InteractiveCommand::Model {
            selection: Some(model),
        } => Ok(InteractiveEffect::Output(set_active_model(
            project_root,
            &model,
        )?)),
        InteractiveCommand::Skills(SkillCommand::List) => Ok(InteractiveEffect::Output(
            skill_cmd::format_installed_skills(project_root)?,
        )),
        InteractiveCommand::Skills(SkillCommand::Install { source }) => {
            let path = skill_cmd::install_skill(&source)?;
            Ok(InteractiveEffect::Output(format!(
                "Installed skill at {}",
                path.display()
            )))
        }
        InteractiveCommand::Skills(SkillCommand::Remove { name }) => {
            skill_cmd::remove_skill(&name)?;
            Ok(InteractiveEffect::Output(format!(
                "Removed skill '{name}'."
            )))
        }
        InteractiveCommand::Mcp(McpCommand::List) => Ok(InteractiveEffect::Output(
            mcp_cmd::format_mcp_servers(project_root)?,
        )),
        InteractiveCommand::Mcp(McpCommand::Add {
            name,
            command,
            args,
        }) => {
            mcp_cmd::add_mcp_server_quiet(project_root, &name, &command, &args)?;
            Ok(InteractiveEffect::Output(format!(
                "Successfully added MCP server '{name}'."
            )))
        }
        InteractiveCommand::Mcp(McpCommand::Remove { name }) => {
            mcp_cmd::remove_mcp_server_quiet(project_root, &name)?;
            Ok(InteractiveEffect::Output(format!(
                "Successfully removed MCP server '{name}'."
            )))
        }
    }
}

pub fn set_active_model(project_root: &Path, model: &str) -> Result<String, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("model ID must not be empty".to_string());
    }
    let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    let provider = config.llm.get_active_provider();
    let selected_provider = provider.clone();
    let selected_model = model.to_string();
    update_nib_config(project_root, move |config| {
        if let Some(entry) = config.llm.providers.get_mut(&selected_provider) {
            entry.model = selected_model;
            Ok(())
        } else if selected_provider == "mock" {
            config
                .llm
                .add_or_update_provider(selected_provider, selected_model, None);
            Ok(())
        } else {
            Err(format!(
                "provider '{selected_provider}' is no longer configured"
            ))
        }
    })
    .map_err(|error| format!("failed saving model: {error}"))?;
    Ok(format!(
        "Switched model to '{model}' for provider '{provider}'."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_nib_config_full, save_nib_config_full, NibConfig};
    use std::collections::HashSet;
    use tempfile::tempdir;

    #[test]
    fn shared_parser_defines_the_complete_interactive_command_vocabulary() {
        let mut names = HashSet::new();
        let help = interactive_help();
        for spec in INTERACTIVE_COMMANDS {
            assert!(!spec.name.is_empty());
            assert!(!spec.usage.is_empty());
            assert!(!spec.summary.is_empty());
            assert!(help.contains(spec.usage));
            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                assert!(names.insert(name), "duplicate command token: {name}");
                let command = format!("/{name}");
                assert!(
                    parse_interactive_command(&command).is_ok(),
                    "{command} must be shared by chat and TUI"
                );
            }
        }
        assert_eq!(parse_interactive_command("hello").unwrap(), None);
        assert!(parse_interactive_command("/unknown").is_err());
        assert!(parse_interactive_command("/skills invalid").is_err());
        assert!(parse_interactive_command("/mcp add only-name").is_err());
    }

    #[test]
    fn completion_is_bounded_case_insensitive_and_uses_registry_metadata() {
        let root = interactive_completions("/");
        assert!(root.len() <= 32);
        assert!(root.iter().any(|item| item.insertion == "/session"));
        assert!(root.iter().any(|item| item.insertion == "/q"));
        assert_eq!(
            interactive_completions("/PRO")
                .iter()
                .map(|item| item.insertion.as_str())
                .collect::<Vec<_>>(),
            vec!["/providers"]
        );

        let skills = interactive_completions("/skills ");
        assert_eq!(
            skills
                .iter()
                .map(|item| item.insertion.as_str())
                .collect::<Vec<_>>(),
            vec!["/skills list", "/skills install ", "/skills remove "]
        );
        assert!(parse_interactive_command("/skills list").is_ok());
        for incomplete in ["/skills install ", "/skills remove "] {
            assert!(
                parse_interactive_command(incomplete).is_err(),
                "free-form arguments must not be guessed or executed"
            );
        }
        assert!(interactive_completions("/skills install ").is_empty());
        assert!(interactive_completions("/unknown").is_empty());
        assert!(interactive_completions("not a command").is_empty());
    }

    #[test]
    fn shared_session_selection_is_ordered_bounded_and_read_only() {
        let directory = tempdir().expect("session directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let older = store
            .try_create_session_with_id("older-session")
            .expect("older session");
        let newer = store
            .try_create_session_with_id("newer-session")
            .expect("newer session");
        store
            .try_append_message(&older.id, "user", "older goal")
            .expect("older message");
        store
            .try_append_message(&newer.id, "user", &"newer goal ".repeat(400))
            .expect("newer message");

        let anchor = chrono::Utc::now();
        let mut older_state = store
            .load_result(&older.id)
            .expect("load older for timestamp")
            .expect("older session");
        older_state.messages[0].timestamp = Some(anchor - chrono::Duration::seconds(1));
        store.save(&mut older_state).expect("save older timestamp");
        let mut newer_state = store
            .load_result(&newer.id)
            .expect("load newer for timestamp")
            .expect("newer session");
        newer_state.messages[0].timestamp = Some(anchor);
        store.save(&mut newer_state).expect("save newer timestamp");

        let before_older = store
            .load_result(&older.id)
            .expect("load older")
            .expect("older session");
        let before_newer = store
            .load_result(&newer.id)
            .expect("load newer")
            .expect("newer session");
        let selection = interactive_session_selection(&store, &older.id).expect("selection");

        assert_eq!(selection.omitted, 0);
        assert_eq!(selection.candidates.len(), 2);
        assert_eq!(selection.candidates[0].id, newer.id);
        assert!(selection
            .candidates
            .iter()
            .find(|candidate| candidate.id == older.id)
            .is_some_and(|candidate| candidate.is_active));
        assert!(selection
            .candidates
            .iter()
            .all(|candidate| candidate.preview.chars().count()
                <= MAX_INTERACTIVE_SESSION_PREVIEW_CHARS));
        assert_eq!(
            store.load_result(&older.id).expect("reload older"),
            Some(before_older)
        );
        assert_eq!(
            store.load_result(&newer.id).expect("reload newer"),
            Some(before_newer.clone())
        );
        let previewed_newer = selection
            .candidates
            .iter()
            .find(|candidate| candidate.id == newer.id)
            .expect("previewed newer session")
            .clone();
        assert_eq!(
            validate_interactive_session_target(&store, &previewed_newer)
                .expect("unchanged target"),
            before_newer
        );

        // A valid same-ID replacement with the same revision is still stale relative
        // to what the user previewed and must not be activated.
        let mut replaced = before_newer.clone();
        replaced.messages[0].content = "valid replacement content".to_string();
        std::fs::write(
            store.sessions_dir().join(format!("{}.json", newer.id)),
            serde_json::to_vec(&replaced).expect("serialize replacement"),
        )
        .expect("replace target");
        let stale = validate_interactive_session_target(&store, &previewed_newer)
            .expect_err("same-revision replacement must fail closed");
        assert!(stale.contains("changed since it was previewed"));
        assert!(interactive_session_candidate(&store, "missing", &older.id).is_err());
    }

    #[test]
    fn structured_failure_events_render_once_with_separate_session_context() {
        let event = StreamEvent::Failure {
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
                "private diagnostic",
            ),
            session_id: Some("session-123".to_string()),
        };

        let StreamDisplay::Status(report) = display_stream_event(event).expect("failure display")
        else {
            panic!("failure event must be a status")
        };
        assert_eq!(report.matches("LLM request failed").count(), 1);
        assert!(report.contains("LLM-AUTH"));
        assert!(report.contains("Session: session-123"));
        assert!(!report.contains("private diagnostic"));
        assert!(!report.contains("agent run failed"));
        assert!(!report.contains("[red]"));
        assert!(!report.contains("\u{1b}"));
    }

    #[test]
    fn shared_effects_change_sessions_models_and_mcp_configuration() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        save_nib_config_full(project.path(), &mut config).expect("config");
        let store = SessionStore::for_project(project.path()).expect("store");
        let resolution = resolve_session(&store, None).expect("session");
        let session_id = resolution.session_id().to_string();

        let effect = execute_interactive_command(
            InteractiveCommand::Clear,
            project.path(),
            &store,
            &session_id,
        )
        .expect("clear");
        assert!(matches!(effect, InteractiveEffect::SessionChanged { .. }));

        execute_interactive_command(
            InteractiveCommand::Model {
                selection: Some("custom-mock".to_string()),
            },
            project.path(),
            &store,
            &session_id,
        )
        .expect("model");
        assert_eq!(
            load_nib_config_full(project.path()).unwrap().llm.providers["mock"].model,
            "custom-mock"
        );

        execute_interactive_command(
            InteractiveCommand::Mcp(McpCommand::Add {
                name: "local".to_string(),
                command: "echo".to_string(),
                args: vec!["--stdio".to_string()],
            }),
            project.path(),
            &store,
            &session_id,
        )
        .expect("MCP add");
        assert!(load_nib_config_full(project.path())
            .unwrap()
            .mcp
            .servers
            .contains_key("local"));
    }

    #[test]
    fn live_input_queues_instead_of_steering_and_persists_before_ack() {
        assert_eq!(
            classify_composer_submit(false),
            ComposerSubmitKind::IdleTurn
        );
        assert_eq!(
            classify_composer_submit(true),
            ComposerSubmitKind::QueueNext
        );
        assert!(steer_unavailable_message().contains("Enter queues"));
        assert_eq!(parse_queue_line("queue: next goal"), Some("next goal"));
        assert_eq!(parse_queue_line("hello"), None);

        let directory = tempdir().expect("session directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let session = store.try_create_session().expect("session");
        let queued = persist_queued_follow_up(&store, &session.id, "run the follow-up", "composer")
            .expect("persist");
        assert_eq!(queued.text, "run the follow-up");
        assert_eq!(
            queued_follow_up_count(&store, &session.id).expect("count"),
            1
        );
        let disposition =
            queue_disposition_message(&store, &session.id, "cancelled").expect("disposition");
        assert!(disposition.contains("retained on session"));
        let taken = take_next_queued_follow_up(&store, &session.id)
            .expect("take")
            .expect("queued text");
        assert_eq!(taken, "run the follow-up");
        assert_eq!(
            queued_follow_up_count(&store, &session.id).expect("empty"),
            0
        );
    }

    #[test]
    fn new_commands_are_parsed_and_gated_commands_explain_unavailability() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        save_nib_config_full(project.path(), &mut config).expect("config");
        let store = SessionStore::for_project(project.path()).expect("store");
        let session_id = resolve_session(&store, None)
            .expect("session")
            .session_id()
            .to_string();
        store
            .try_append_message(&session_id, "user", "goal")
            .expect("user");
        store
            .try_append_message(&session_id, "assistant", "done")
            .expect("assistant");

        for command in [
            "/status",
            "/permissions",
            "/plan",
            "/review",
            "/diff",
            "/compact",
            "/new",
            "/resume",
            "/fork",
            "/rename wrap-fix",
            "/copy",
            "/ps",
            "/stop",
        ] {
            parse_interactive_command(command).unwrap_or_else(|error| panic!("{command}: {error}"));
        }

        let InteractiveEffect::Output(status) = execute_interactive_command_in_state(
            InteractiveCommand::Status,
            project.path(),
            &store,
            &session_id,
            "running",
        )
        .expect("status") else {
            panic!("status output");
        };
        assert!(status.contains("running"));
        assert!(status.contains(&session_id));

        let InteractiveEffect::Output(compact) = execute_interactive_command(
            InteractiveCommand::Compact,
            project.path(),
            &store,
            &session_id,
        )
        .expect("compact") else {
            panic!("compact output");
        };
        assert!(compact.contains("unavailable"));
        assert!(compact.contains("T003"));

        let InteractiveEffect::Output(copied) = execute_interactive_command(
            InteractiveCommand::Copy,
            project.path(),
            &store,
            &session_id,
        )
        .expect("copy") else {
            panic!("copy output");
        };
        assert_eq!(copied, "done");

        let InteractiveEffect::SessionChanged {
            session_id: forked, ..
        } = execute_interactive_command(
            InteractiveCommand::Fork,
            project.path(),
            &store,
            &session_id,
        )
        .expect("fork")
        else {
            panic!("fork session");
        };
        let forked_session = store
            .load_result(&forked)
            .expect("load fork")
            .expect("fork exists");
        assert_eq!(
            forked_session.forked_from.as_deref(),
            Some(session_id.as_str())
        );
        let source = store
            .load_result(&session_id)
            .expect("reload source")
            .expect("source");
        assert!(source.forked_from.is_none());

        assert!(
            unicode_display_width("漢字") > unicode_display_width("ab")
                || unicode_display_width("漢字") == 4
        );
        assert_eq!(bottom_scroll_for_wrap("ab", 1, 1), 1);
        assert_eq!(wrapped_line_count(&"a".repeat(200), 80), 3);
    }

    #[test]
    fn typed_activities_keep_local_work_distinct_from_assistant_speech() {
        let directory = tempdir().expect("dir");
        let mut session = SessionStore::at_dir(directory.path().join("s"))
            .try_create_session()
            .expect("session");
        session.messages.push(crate::session::SessionMessage {
            index: 0,
            role: "user".to_string(),
            content: "inspect wrap".to_string(),
            timestamp: None,
        });
        session.plan = Some(crate::session::Plan::new(
            "inspect wrap",
            vec![crate::session::PlanStep {
                description: "write tests".to_string(),
                status: "InProgress".to_string(),
                outcome: None,
                attempts: 1,
                updated_at: None,
            }],
        ));
        let projected = project_session_activities(&session);
        assert!(projected
            .iter()
            .any(|entry| entry.kind == ActivityKind::User));
        assert!(projected
            .iter()
            .any(|entry| entry.kind == ActivityKind::Plan));

        let mut live = Vec::new();
        let mut state = None;
        apply_stream_event(
            &mut live,
            StreamEvent::Content("hello".to_string()),
            &mut state,
        );
        apply_stream_event(
            &mut live,
            StreamEvent::ToolStarted {
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "README.md"}),
            },
            &mut state,
        );
        apply_stream_event(
            &mut live,
            StreamEvent::Reconciled {
                outcome: "completed".to_string(),
            },
            &mut state,
        );
        assert_eq!(live[0].kind, ActivityKind::Assistant);
        assert_eq!(live[1].kind, ActivityKind::Tool);
        assert_eq!(live[2].kind, ActivityKind::Reconcile);
    }

    #[test]
    fn path_completions_stay_inside_the_project_and_ignore_dot_entries() {
        let project = tempdir().expect("project");
        std::fs::create_dir_all(project.path().join("src")).expect("src");
        std::fs::write(project.path().join("src/main.rs"), "fn main() {}").expect("file");
        std::fs::write(project.path().join(".secret"), "nope").expect("dotfile");
        let matches = path_completions(project.path(), "see @src/m");
        assert!(
            matches
                .iter()
                .any(|item| item.insertion.ends_with("@src/main.rs")),
            "{matches:?}"
        );
        assert!(matches
            .iter()
            .all(|item| !item.insertion.contains(".secret")));
    }
}
