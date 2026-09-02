//! Shared interactive command grammar and effects for chat and TUI surfaces.

use crate::config::{load_nib_config_full, update_nib_config};
use crate::context::bounded_session_context;
use crate::llm::factory::provider_diagnostics;
use crate::llm::types::StreamEvent;
use crate::session::{PathAttachment, QueuedFollowUp, Session, SessionEvent, SessionStore};
use crate::tools::executor::{
    EffectiveExecutionPosture, InstructionExecutionPosture, ToolExecutor,
};
use crate::{mcp_cmd, skill_cmd};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::path::Path;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

const MAX_STATUS_VALUE_BYTES: usize = 160;
const MAX_PUBLIC_PRESENTATION_BYTES: usize = 8 * 1024;

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
    pub arguments: InteractiveArgumentSchema,
    pub mutability: InteractiveMutability,
    pub availability: InteractiveAvailability,
    pub worker_policy: InteractiveWorkerPolicy,
    pub completion: InteractiveCompletionSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveArgumentSchema {
    None,
    OptionalText,
    RequiredText,
    OptionalSingle,
    Permissions,
    Skills,
    Mcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveMutability {
    ReadOnly,
    Runtime,
    Session,
    Configuration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveAvailability {
    Available,
    Unavailable(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveWorkerPolicy {
    Allowed,
    RequiresIdle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractiveCompletionSpec {
    pub candidates: &'static [&'static str],
    pub argument_after: &'static [&'static str],
}

const NO_COMPLETION: InteractiveCompletionSpec = InteractiveCompletionSpec {
    candidates: &[],
    argument_after: &[],
};

// Registry rows intentionally spell out every public behavior dimension together;
// grouping these fields behind positional defaults would make parser/help drift easier.
#[allow(clippy::too_many_arguments)]
const fn spec(
    name: &'static str,
    aliases: &'static [&'static str],
    usage: &'static str,
    summary: &'static str,
    arguments: InteractiveArgumentSchema,
    mutability: InteractiveMutability,
    worker_policy: InteractiveWorkerPolicy,
    completion: InteractiveCompletionSpec,
) -> InteractiveCommandSpec {
    InteractiveCommandSpec {
        name,
        aliases,
        usage,
        summary,
        arguments,
        mutability,
        availability: InteractiveAvailability::Available,
        worker_policy,
        completion,
    }
}

pub const INTERACTIVE_COMMANDS: &[InteractiveCommandSpec] = &[
    spec(
        "status",
        &[],
        "/status",
        "Show session, model, permissions, plan, and queue",
        InteractiveArgumentSchema::None,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "model",
        &[],
        "/model [name]",
        "List models or select an exact model ID",
        InteractiveArgumentSchema::OptionalSingle,
        InteractiveMutability::Configuration,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "providers",
        &[],
        "/providers",
        "List configured providers",
        InteractiveArgumentSchema::None,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "permissions",
        &[],
        "/permissions [manual|smart|policy|off]",
        "Inspect or set the configured approval mode",
        InteractiveArgumentSchema::Permissions,
        InteractiveMutability::Configuration,
        InteractiveWorkerPolicy::RequiresIdle,
        InteractiveCompletionSpec {
            candidates: &["manual", "smart", "policy", "off"],
            argument_after: &[],
        },
    ),
    spec(
        "plan",
        &[],
        "/plan [prompt]",
        "Show the current plan or request planning",
        InteractiveArgumentSchema::OptionalText,
        InteractiveMutability::Runtime,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "review",
        &[],
        "/review",
        "Review authoritative workspace changes",
        InteractiveArgumentSchema::None,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "diff",
        &[],
        "/diff",
        "Show the session workspace diff",
        InteractiveArgumentSchema::None,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "compact",
        &[],
        "/compact",
        "Request bounded context compression",
        InteractiveArgumentSchema::None,
        InteractiveMutability::Session,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "session",
        &[],
        "/session",
        "Show or switch the active session",
        InteractiveArgumentSchema::None,
        InteractiveMutability::Session,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "resume",
        &[],
        "/resume",
        "Preview and confirm resuming another session",
        InteractiveArgumentSchema::None,
        InteractiveMutability::Session,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "new",
        &[],
        "/new",
        "Start a fresh session",
        InteractiveArgumentSchema::None,
        InteractiveMutability::Session,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "clear",
        &[],
        "/clear",
        "Start a fresh session",
        InteractiveArgumentSchema::None,
        InteractiveMutability::Session,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "fork",
        &[],
        "/fork",
        "Branch a new session from the current transcript",
        InteractiveArgumentSchema::None,
        InteractiveMutability::Session,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "rename",
        &[],
        "/rename <name>",
        "Set the current session display name",
        InteractiveArgumentSchema::RequiredText,
        InteractiveMutability::Session,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "copy",
        &[],
        "/copy",
        "Print the latest completed assistant output",
        InteractiveArgumentSchema::None,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "history",
        &[],
        "/history [query]",
        "Search process-local submitted draft history",
        InteractiveArgumentSchema::OptionalText,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "ps",
        &[],
        "/ps",
        "List session-owned background work",
        InteractiveArgumentSchema::None,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "stop",
        &[],
        "/stop [task-id]",
        "Stop exact session-owned background work",
        InteractiveArgumentSchema::OptionalSingle,
        InteractiveMutability::Runtime,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "skills",
        &[],
        "/skills [list|install <url_or_path>|remove <name>]",
        "Manage installed skills",
        InteractiveArgumentSchema::Skills,
        InteractiveMutability::Configuration,
        InteractiveWorkerPolicy::RequiresIdle,
        InteractiveCompletionSpec {
            candidates: &["list", "install", "remove"],
            argument_after: &["install", "remove"],
        },
    ),
    spec(
        "mcp",
        &[],
        "/mcp [list|add <name> <command> [args...]|remove <name>]",
        "Manage MCP servers",
        InteractiveArgumentSchema::Mcp,
        InteractiveMutability::Configuration,
        InteractiveWorkerPolicy::RequiresIdle,
        InteractiveCompletionSpec {
            candidates: &["list", "add", "remove"],
            argument_after: &["add", "remove"],
        },
    ),
    spec(
        "help",
        &[],
        "/help",
        "Show interactive command help",
        InteractiveArgumentSchema::None,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::RequiresIdle,
        NO_COMPLETION,
    ),
    spec(
        "quit",
        &["exit", "q"],
        "/quit (aliases: /exit, /q)",
        "Exit the interactive session",
        InteractiveArgumentSchema::None,
        InteractiveMutability::Runtime,
        InteractiveWorkerPolicy::Allowed,
        NO_COMPLETION,
    ),
];

const MAX_QUEUED_FOLLOW_UPS: usize = 16;
const MAX_QUEUE_TEXT_BYTES: usize = 16 * 1024;
const MAX_DISPLAY_NAME_BYTES: usize = 128;
const MAX_ACTIVITY_BODY_BYTES: usize = 8 * 1024;
const MAX_PROJECTED_SESSION_EVENTS: usize = 200;
const MAX_ACTIVITY_LABEL_BYTES: usize = 96;
const MAX_DIFF_BYTES: usize = 32 * 1024;
const MAX_DIFF_REDACTION_INPUT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_INTERACTIVE_BACKGROUND_TASKS: usize = 100;
const STEER_HINT: &str =
    "Ctrl+S steers the exact active run. Enter queues the next turn and never steers.";

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
            InteractiveSessionCandidate::from_session(
                &session,
                active_session_id,
                store.public_sensitive_values(),
            )?,
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
    InteractiveSessionCandidate::from_session(
        &session,
        active_session_id,
        store.public_sensitive_values(),
    )
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
        .map_err(|error| {
            if matches!(error, crate::session::SessionError::SensitiveSessionId) {
                error.to_string()
            } else {
                format!("failed to load session {session_id}: {error}")
            }
        })?
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
    fn from_session(
        session: &Session,
        active_session_id: &str,
        sensitive_values: &[String],
    ) -> Result<Self, String> {
        let last_activity = session_last_activity(session);
        let latest_user = session
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| bounded_session_preview(&message.content, sensitive_values))
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
                    bounded_session_preview(&message.content, sensitive_values)
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

fn bounded_session_preview(content: &str, sensitive_values: &[String]) -> String {
    let safe = bounded_public_text(
        content,
        sensitive_values,
        MAX_INTERACTIVE_SESSION_ITEM_CHARS.saturating_mul(4),
        false,
    );
    let mut characters = safe.chars();
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
        if let InteractiveAvailability::Unavailable(reason) = command.availability {
            help.push_str(&format!("\n    {reason}"));
        }
    }
    help.push_str(
        "\n  queue: <text>                                                Queue a follow-up for the next turn",
    );
    help.push_str(&format!("\n  {STEER_HINT}"));
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
    if remaining.len() > 1 || spec.completion.candidates.is_empty() {
        return Vec::new();
    }
    let subcommand_prefix = remaining.first().copied().unwrap_or_default();
    if !subcommand_prefix.is_empty()
        && command_line.chars().last().is_some_and(char::is_whitespace)
        && spec
            .completion
            .candidates
            .iter()
            .any(|subcommand| subcommand.eq_ignore_ascii_case(subcommand_prefix))
    {
        return Vec::new();
    }
    spec.completion
        .candidates
        .iter()
        .copied()
        .filter(|subcommand| {
            subcommand
                .to_ascii_lowercase()
                .starts_with(&subcommand_prefix.to_ascii_lowercase())
        })
        .take(MAX_COMPLETIONS)
        .map(|subcommand| InteractiveCompletion {
            insertion: completion_insertion(spec, subcommand),
            usage: spec.usage,
            summary: spec.summary,
        })
        .collect()
}

fn completion_insertion(spec: &InteractiveCommandSpec, subcommand: &str) -> String {
    let needs_argument = spec
        .completion
        .argument_after
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(subcommand));
    format!(
        "/{} {subcommand}{}",
        spec.name,
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
    History { query: Option<String> },
    Ps,
    Stop { task_id: Option<String> },
    Model { selection: Option<String> },
    Skills(SkillCommand),
    Mcp(McpCommand),
}

impl InteractiveCommand {
    pub fn spec(&self) -> &'static InteractiveCommandSpec {
        let name = match self {
            Self::Quit => "quit",
            Self::Help => "help",
            Self::Status => "status",
            Self::Providers => "providers",
            Self::Permissions { .. } => "permissions",
            Self::Plan { .. } => "plan",
            Self::Review => "review",
            Self::Diff => "diff",
            Self::Compact => "compact",
            Self::Session => "session",
            Self::Resume => "resume",
            Self::New => "new",
            Self::Clear => "clear",
            Self::Fork => "fork",
            Self::Rename { .. } => "rename",
            Self::Copy => "copy",
            Self::History { .. } => "history",
            Self::Ps => "ps",
            Self::Stop { .. } => "stop",
            Self::Model { .. } => "model",
            Self::Skills(_) => "skills",
            Self::Mcp(_) => "mcp",
        };
        find_command_spec(name).expect("every typed interactive command has registry metadata")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: String,
    pub current: String,
    pub available: Vec<String>,
    pub sensitive_values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveEffect {
    Quit,
    Output(String),
    SessionChanged {
        session_id: String,
        output: String,
    },
    SelectSession(InteractiveSessionSelection),
    SelectModel(ModelSelection),
    /// Run explicit context compression without manufacturing a chat turn.
    Compact,
    RunAgent {
        goal: String,
        mode: InteractiveAgentMode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveAgentMode {
    Execute,
    Plan,
    Compact,
}

#[derive(Clone)]
pub struct InteractiveProfileScope {
    profile_id: String,
    session_store: SessionStore,
}

impl InteractiveProfileScope {
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub fn session_store(&self) -> &SessionStore {
        &self.session_store
    }

    pub fn into_session_store(self) -> SessionStore {
        self.session_store
    }
}

/// Resolve the interactive profile once so later commands and agent turns cannot
/// drift when project configuration changes while the UI remains open.
pub fn resolve_interactive_profile_scope(
    project_root: &Path,
) -> Result<InteractiveProfileScope, String> {
    let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    let profiles = crate::profile::ProfileRegistry::load(project_root, &config.profiles)
        .map_err(|error| error.to_string())?;
    let profile = profiles
        .for_workspace(project_root)
        .unwrap_or_else(|| profiles.default_profile());
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;
    Ok(InteractiveProfileScope {
        profile_id: profile.id().to_string(),
        session_store: SessionStore::at_dir(profile.sessions_dir().to_path_buf())
            .with_sensitive_values(config.public_session_sensitive_values()),
    })
}

impl InteractiveAgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Plan => "plan",
            Self::Compact => "compact",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerSubmitKind {
    IdleTurn,
    QueueNext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorDetailKind {
    Selector,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptViewportAction {
    PageUp,
    PageDown,
    JumpToEnd,
}

/// Presentation-only transcript viewport measured in the same rendered rows used
/// by the TUI. It never owns or mutates session content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptViewport {
    top_row: usize,
    total_rows: usize,
    page_rows: usize,
    pinned_to_tail: bool,
}

impl Default for TranscriptViewport {
    fn default() -> Self {
        Self {
            top_row: 0,
            total_rows: 1,
            page_rows: 1,
            pinned_to_tail: true,
        }
    }
}

impl TranscriptViewport {
    pub fn observe_layout(&mut self, total_rows: usize, page_rows: usize) {
        self.total_rows = total_rows.max(1);
        self.page_rows = page_rows.max(1);
        let bottom = self.bottom_row();
        if self.pinned_to_tail {
            self.top_row = bottom;
        } else {
            self.top_row = self.top_row.min(bottom);
        }
    }

    pub fn apply(&mut self, action: TranscriptViewportAction) {
        match action {
            TranscriptViewportAction::PageUp => {
                let current = if self.pinned_to_tail {
                    self.bottom_row()
                } else {
                    self.top_row
                };
                let next = current.saturating_sub(self.page_rows);
                if next < current {
                    self.top_row = next;
                    self.pinned_to_tail = false;
                }
            }
            TranscriptViewportAction::PageDown => {
                if !self.pinned_to_tail {
                    self.top_row = self
                        .top_row
                        .saturating_add(self.page_rows)
                        .min(self.bottom_row());
                }
            }
            TranscriptViewportAction::JumpToEnd => self.pin_to_tail(),
        }
    }

    pub fn pin_to_tail(&mut self) {
        self.pinned_to_tail = true;
        self.top_row = self.bottom_row();
    }

    pub fn on_submission(&mut self) {
        self.pin_to_tail();
    }

    pub fn top_row(&self) -> usize {
        if self.pinned_to_tail {
            self.bottom_row()
        } else {
            self.top_row.min(self.bottom_row())
        }
    }

    pub fn is_pinned_to_tail(&self) -> bool {
        self.pinned_to_tail
    }

    pub fn status_label(&self) -> String {
        if self.pinned_to_tail {
            "tail:following".to_string()
        } else {
            format!("tail:paused row {}/{}", self.top_row() + 1, self.total_rows)
        }
    }

    fn bottom_row(&self) -> usize {
        self.total_rows.saturating_sub(self.page_rows)
    }
}

pub const MAX_DRAFT_HISTORY: usize = 50;
pub const MAX_DRAFT_HISTORY_QUERY_BYTES: usize = 256;
pub const MAX_DRAFT_HISTORY_RESULTS: usize = 20;
const MAX_DRAFT_HISTORY_ENTRY_BYTES: usize = 16 * 1024;
const MAX_DRAFT_HISTORY_DISPLAY_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftHistoryMatch {
    pub entry_index: usize,
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftHistorySearch {
    pub query: String,
    pub matches: Vec<DraftHistoryMatch>,
    pub query_truncated: bool,
    pub controls_omitted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftHistory {
    entries: Vec<String>,
}

impl DraftHistory {
    pub fn remember_submission(&mut self, submitted: &str) {
        let submitted = submitted.trim();
        if submitted.is_empty() {
            return;
        }
        let submitted = unicode_prefix(submitted, MAX_DRAFT_HISTORY_ENTRY_BYTES).to_string();
        if self.entries.last().map(String::as_str) == Some(submitted.as_str()) {
            return;
        }
        self.entries.push(submitted);
        if self.entries.len() > MAX_DRAFT_HISTORY {
            let extra = self.entries.len() - MAX_DRAFT_HISTORY;
            self.entries.drain(0..extra);
        }
    }

    pub fn search(&self, query: &str) -> DraftHistorySearch {
        let (query, query_truncated, controls_omitted) = normalize_history_query(query);
        let folded_query = query.to_lowercase();
        let matches = self
            .entries
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(entry_index, entry)| {
                let display = safe_history_display(entry);
                (folded_query.is_empty() || display.to_lowercase().contains(&folded_query))
                    .then_some(DraftHistoryMatch {
                        entry_index,
                        display,
                    })
            })
            .take(MAX_DRAFT_HISTORY_RESULTS)
            .collect();
        DraftHistorySearch {
            query,
            matches,
            query_truncated,
            controls_omitted,
        }
    }

    /// A history-search invocation is presentation control, not a restorable draft.
    /// Remove it only when it is the exact most-recent normalized submission.
    pub fn discard_latest_if(&mut self, submitted: &str) {
        let submitted = unicode_prefix(submitted.trim(), MAX_DRAFT_HISTORY_ENTRY_BYTES);
        if self.entries.last().map(String::as_str) == Some(submitted) {
            self.entries.pop();
        }
    }

    pub fn entry(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn unicode_prefix(value: &str, maximum_bytes: usize) -> &str {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut end = maximum_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn normalize_history_query(query: &str) -> (String, bool, bool) {
    let mut safe = String::new();
    let mut controls_omitted = false;
    for character in query.trim().chars() {
        if character.is_control() {
            controls_omitted = true;
        } else {
            safe.push(character);
        }
    }
    let truncated = safe.len() > MAX_DRAFT_HISTORY_QUERY_BYTES;
    if truncated {
        safe.truncate(unicode_prefix(&safe, MAX_DRAFT_HISTORY_QUERY_BYTES).len());
    }
    (safe, truncated, controls_omitted)
}

fn safe_history_display(entry: &str) -> String {
    let mut safe = String::new();
    let mut truncated = false;
    for character in entry.chars() {
        match character {
            '\n' | '\r' => safe.push_str(" ↵ "),
            '\t' => safe.push_str("    "),
            character if character.is_control() => {}
            character => safe.push(character),
        }
        if safe.len() > MAX_DRAFT_HISTORY_DISPLAY_BYTES {
            truncated = true;
            break;
        }
    }
    if truncated || safe.len() > MAX_DRAFT_HISTORY_DISPLAY_BYTES {
        let prefix = unicode_prefix(
            &safe,
            MAX_DRAFT_HISTORY_DISPLAY_BYTES.saturating_sub('…'.len_utf8()),
        );
        format!("{}…", prefix.trim_end())
    } else {
        safe
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionConsumer {
    Approval,
    Question,
    DestructiveConfirmation,
    Selector,
    Detail,
    Completion,
    Composer,
    Timeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InteractionRunState {
    #[default]
    Idle,
    Planning,
    Running,
    Reconciling,
    Completed,
    Cancelled,
    Failed,
}

impl InteractionRunState {
    pub fn worker_active(self) -> bool {
        matches!(self, Self::Planning | Self::Running | Self::Reconciling)
    }

    pub fn accepts_live_input(self) -> bool {
        matches!(self, Self::Planning | Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionLifecycle {
    Idle,
    Planning,
    Running,
    WaitingApproval,
    WaitingQuestion,
    Reconciling,
    Completed,
    Cancelled,
    Failed,
}

impl InteractionLifecycle {
    pub fn status_label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Planning => "planning",
            Self::Running => "running",
            Self::WaitingApproval | Self::WaitingQuestion => "awaiting you",
            Self::Reconciling => "reconciling",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InteractionState {
    pub approval_pending: bool,
    pub question_pending: bool,
    pub destructive_confirmation_pending: bool,
    pub selector_or_detail: Option<SelectorDetailKind>,
    pub completion_pending: bool,
    pub run: InteractionRunState,
}

impl InteractionState {
    pub fn lifecycle(&self) -> InteractionLifecycle {
        if self.approval_pending {
            InteractionLifecycle::WaitingApproval
        } else if self.question_pending {
            InteractionLifecycle::WaitingQuestion
        } else {
            match self.run {
                InteractionRunState::Idle => InteractionLifecycle::Idle,
                InteractionRunState::Planning => InteractionLifecycle::Planning,
                InteractionRunState::Running => InteractionLifecycle::Running,
                InteractionRunState::Reconciling => InteractionLifecycle::Reconciling,
                InteractionRunState::Completed => InteractionLifecycle::Completed,
                InteractionRunState::Cancelled => InteractionLifecycle::Cancelled,
                InteractionRunState::Failed => InteractionLifecycle::Failed,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionInput<'a> {
    UserAction,
    SubmittedLine(&'a str),
    ComposerSubmit(&'a str),
    ApprovalAnswer(&'a str),
    QuestionAnswer {
        answer: &'a str,
        options: &'a [String],
        selected_option: Option<usize>,
    },
    ConfirmationAnswer(&'a str),
    ReconciledOutcome {
        outcome: &'a str,
        failure: bool,
    },
    SteerCurrent(&'a str),
    OpenHistorySearch,
    Transcript(TranscriptViewportAction),
    CancelRun,
    Quit,
    SessionRunEvent {
        active_session_id: &'a str,
        active_run_id: Option<&'a str>,
        event_session_id: &'a str,
        event_run_id: &'a str,
    },
    InvalidAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionReduction {
    Consumed(InteractionConsumer),
    Command(InteractiveCommand),
    ApprovalDecision(InteractionDecision),
    QuestionAnswered(String),
    ConfirmationDecision(InteractionDecision),
    Reconciled {
        outcome: String,
        terminal: InteractionTerminalOutcome,
    },
    QueueNext(String),
    SteerCurrent(String),
    IdleTurn(String),
    OpenHistorySearch {
        query: Option<String>,
    },
    Transcript(TranscriptViewportAction),
    CancelRun,
    Quit,
    StaleEvent,
    NoOp(InteractionConsumer),
    Error {
        consumer: InteractionConsumer,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionDecision {
    Accept,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionTerminalOutcome {
    Completed,
    Cancelled,
    WaitingForInput,
    Failed,
}

pub fn active_interaction_consumer(state: &InteractionState) -> InteractionConsumer {
    if state.approval_pending {
        InteractionConsumer::Approval
    } else if state.question_pending {
        InteractionConsumer::Question
    } else if state.destructive_confirmation_pending {
        InteractionConsumer::DestructiveConfirmation
    } else if let Some(selector_or_detail) = state.selector_or_detail {
        match selector_or_detail {
            SelectorDetailKind::Selector => InteractionConsumer::Selector,
            SelectorDetailKind::Detail => InteractionConsumer::Detail,
        }
    } else if state.completion_pending {
        InteractionConsumer::Completion
    } else {
        InteractionConsumer::Composer
    }
}

pub fn reduce_interaction(
    state: &InteractionState,
    input: InteractionInput<'_>,
) -> InteractionReduction {
    let consumer = active_interaction_consumer(state);
    match input {
        InteractionInput::CancelRun if state.run.accepts_live_input() => {
            InteractionReduction::CancelRun
        }
        InteractionInput::CancelRun => InteractionReduction::NoOp(consumer),
        InteractionInput::Quit => InteractionReduction::Quit,
        InteractionInput::SessionRunEvent {
            active_session_id,
            active_run_id,
            event_session_id,
            event_run_id,
        } => {
            if active_session_id == event_session_id && active_run_id == Some(event_run_id) {
                InteractionReduction::Consumed(InteractionConsumer::Timeline)
            } else {
                InteractionReduction::StaleEvent
            }
        }
        InteractionInput::ReconciledOutcome { outcome, failure } => {
            let outcome = bounded_status_value(outcome);
            let terminal = if outcome == "cancelled_by_user" {
                InteractionTerminalOutcome::Cancelled
            } else if outcome == "waiting_for_user_input" {
                InteractionTerminalOutcome::WaitingForInput
            } else if failure
                || !matches!(
                    outcome.as_str(),
                    "completed" | "plan_ready" | "context_compacted" | "context_unchanged"
                )
            {
                InteractionTerminalOutcome::Failed
            } else {
                InteractionTerminalOutcome::Completed
            };
            InteractionReduction::Reconciled { outcome, terminal }
        }
        InteractionInput::InvalidAction => InteractionReduction::Error {
            consumer,
            message: "invalid interaction action; input was not applied".to_string(),
        },
        InteractionInput::OpenHistorySearch if consumer == InteractionConsumer::Composer => {
            InteractionReduction::OpenHistorySearch { query: None }
        }
        InteractionInput::Transcript(action) if consumer == InteractionConsumer::Composer => {
            InteractionReduction::Transcript(action)
        }
        InteractionInput::OpenHistorySearch | InteractionInput::Transcript(_) => {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::UserAction => InteractionReduction::Consumed(consumer),
        InteractionInput::ApprovalAnswer(_) if consumer != InteractionConsumer::Approval => {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::ApprovalAnswer(answer) => {
            let decision = if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                InteractionDecision::Accept
            } else {
                InteractionDecision::Reject
            };
            InteractionReduction::ApprovalDecision(decision)
        }
        InteractionInput::QuestionAnswer { .. } if consumer != InteractionConsumer::Question => {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::QuestionAnswer {
            answer,
            options,
            selected_option,
        } => reduce_question_answer(answer, options, selected_option),
        InteractionInput::ConfirmationAnswer(_)
            if consumer != InteractionConsumer::DestructiveConfirmation =>
        {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::ConfirmationAnswer(answer) => {
            let decision = if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                InteractionDecision::Accept
            } else {
                InteractionDecision::Reject
            };
            InteractionReduction::ConfirmationDecision(decision)
        }
        InteractionInput::SteerCurrent(_) if consumer != InteractionConsumer::Composer => {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::SteerCurrent(line) => {
            let line = line.trim();
            if !state.run.accepts_live_input() {
                InteractionReduction::Error {
                    consumer,
                    message: "No active run is available to steer.".to_string(),
                }
            } else if line.is_empty() {
                InteractionReduction::Error {
                    consumer,
                    message: "Steering input cannot be empty.".to_string(),
                }
            } else {
                InteractionReduction::SteerCurrent(line.to_string())
            }
        }
        InteractionInput::SubmittedLine(_) if consumer != InteractionConsumer::Composer => {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::ComposerSubmit(_) if consumer != InteractionConsumer::Composer => {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::ComposerSubmit(line) => reduce_composer_submission(state, line),
        InteractionInput::SubmittedLine(line) => reduce_composer_submission(state, line),
    }
}

fn reduce_question_answer(
    answer: &str,
    options: &[String],
    selected_option: Option<usize>,
) -> InteractionReduction {
    let answer = answer.trim();
    if !answer.is_empty() {
        if let Ok(index) = answer.parse::<usize>() {
            return options
                .get(index.saturating_sub(1))
                .filter(|_| index > 0)
                .cloned()
                .map(InteractionReduction::QuestionAnswered)
                .unwrap_or_else(|| InteractionReduction::Error {
                    consumer: InteractionConsumer::Question,
                    message: format!("question option {index} is out of range"),
                });
        }
        return InteractionReduction::QuestionAnswered(answer.to_string());
    }
    if let Some(answer) = selected_option
        .and_then(|index| options.get(index))
        .cloned()
    {
        return InteractionReduction::QuestionAnswered(answer);
    }
    InteractionReduction::Error {
        consumer: InteractionConsumer::Question,
        message: "question response cannot be empty".to_string(),
    }
}

fn reduce_composer_submission(state: &InteractionState, line: &str) -> InteractionReduction {
    let normalized = line.trim();
    if normalized.is_empty() {
        return InteractionReduction::NoOp(InteractionConsumer::Composer);
    }
    if normalized.starts_with("queue:") {
        return parse_queue_line(normalized).map_or_else(
            || InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                message: "queued follow-up cannot be empty".to_string(),
            },
            |queued| InteractionReduction::QueueNext(queued.to_string()),
        );
    }
    if normalized.starts_with("steer:") {
        return parse_steer_line(normalized).map_or_else(
            || InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                message: "steering instruction cannot be empty".to_string(),
            },
            |steering| {
                if state.run.accepts_live_input() {
                    InteractionReduction::SteerCurrent(steering.to_string())
                } else {
                    InteractionReduction::Error {
                        consumer: InteractionConsumer::Composer,
                        message: "No active run is available to steer.".to_string(),
                    }
                }
            },
        );
    }
    let parsed = match parse_interactive_command(normalized) {
        Ok(parsed) => parsed,
        Err(message) => {
            return InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                message: bounded_status_value(&message),
            }
        }
    };
    if state.run.worker_active() {
        return match parsed {
            Some(command)
                if command.spec().worker_policy == InteractiveWorkerPolicy::RequiresIdle =>
            {
                InteractionReduction::Error {
                    consumer: InteractionConsumer::Composer,
                    message:
                        "Agent is still running; cancel it or wait before running this command."
                            .to_string(),
                }
            }
            Some(InteractiveCommand::History { query }) => {
                InteractionReduction::OpenHistorySearch { query }
            }
            Some(command) => InteractionReduction::Command(command),
            None => InteractionReduction::QueueNext(normalized.to_string()),
        };
    }
    match parsed {
        Some(InteractiveCommand::History { query }) => {
            InteractionReduction::OpenHistorySearch { query }
        }
        Some(command) => InteractionReduction::Command(command),
        None => InteractionReduction::IdleTurn(normalized.to_string()),
    }
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

pub fn steer_hint_message() -> &'static str {
    STEER_HINT
}

pub fn parse_queue_line(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    trimmed
        .strip_prefix("queue:")
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

pub fn parse_steer_line(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    trimmed
        .strip_prefix("steer:")
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

fn append_queue_start_event(
    session: &mut Session,
    kind: &str,
    item: &QueuedFollowUp,
    phase: &str,
    disposition: &str,
) {
    session.events.push(SessionEvent {
        index: session.events.len(),
        kind: kind.to_string(),
        details: serde_json::json!({
            "queue_id": item.id,
            "source": item.source,
            "phase": phase,
            "disposition": disposition,
        }),
        timestamp: Some(Utc::now()),
    });
}

/// Prepare the next FIFO item without releasing it to execution, then durably
/// claim it only after worker startup succeeds.
///
/// `startup` must return a prepared execution handle that cannot process the
/// item until the caller explicitly activates it. If preparation fails, the
/// queue remains unchanged and the retained disposition is recorded in the
/// session audit.
pub fn claim_next_queued_follow_up_after_startup<T>(
    store: &SessionStore,
    session_id: &str,
    startup: impl FnOnce(&QueuedFollowUp) -> Result<T, String>,
) -> Result<Option<(QueuedFollowUp, T)>, String> {
    let Some(item) = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load queued follow-up: {error}"))?
        .and_then(|session| session.queued_follow_ups.first().cloned())
    else {
        return Ok(None);
    };

    let prepared = match startup(&item) {
        Ok(prepared) => prepared,
        Err(error) => {
            let audit = store.update_session(session_id, |session| {
                let retained = session
                    .queued_follow_ups
                    .iter()
                    .any(|queued| queued.id == item.id);
                append_queue_start_event(
                    session,
                    "queued_follow_up_start_failed",
                    &item,
                    "worker_startup",
                    if retained { "retained" } else { "not_retained" },
                );
                Ok(retained)
            });
            return match audit {
                Ok(true) => Err(format!(
                    "queued follow-up {} could not start and remains queued: {error}",
                    item.id
                )),
                Ok(false) => Err(format!(
                    "queued follow-up {} could not start but was no longer queued: {error}",
                    item.id
                )),
                Err(audit_error) => Err(format!(
                    "queued follow-up {} could not start: {error}; failed to record its retained disposition: {audit_error}",
                    item.id
                )),
            };
        }
    };

    let claimed = store
        .update_session(session_id, |session| {
            let Some(head) = session.queued_follow_ups.first() else {
                return Err(crate::session::SessionError::InvalidMutation(format!(
                    "queued follow-up {} disappeared before startup commit",
                    item.id
                )));
            };
            if head.id != item.id {
                return Err(crate::session::SessionError::InvalidMutation(format!(
                    "queued follow-up order changed before startup commit: expected {}, found {}",
                    item.id, head.id
                )));
            }
            let claimed = session.queued_follow_ups.remove(0);
            append_queue_start_event(
                session,
                "queued_follow_up_start_committed",
                &claimed,
                "worker_startup",
                "claimed",
            );
            Ok(claimed)
        })
        .map_err(|error| format!("failed to claim queued follow-up after startup: {error}"))?;
    Ok(Some((claimed, prepared)))
}

/// Restore an item whose prepared worker could not be activated after the
/// durable claim. Reinsertion is idempotent and returns the item to the FIFO
/// head before recording the recoverable failure.
pub fn restore_queued_follow_up_after_start_failure(
    store: &SessionStore,
    session_id: &str,
    item: QueuedFollowUp,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if !session
                .queued_follow_ups
                .iter()
                .any(|queued| queued.id == item.id)
            {
                session.queued_follow_ups.insert(0, item.clone());
            }
            append_queue_start_event(
                session,
                "queued_follow_up_start_failed",
                &item,
                "worker_activation",
                "retained",
            );
            Ok(())
        })
        .map_err(|error| format!("failed to restore queued follow-up {}: {error}", item.id))
}

pub fn unicode_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Wrap plain presentation text into the exact visual rows consumed by the TUI.
/// The renderer displays these rows without applying a second wrapping policy.
pub fn wrapped_display_rows(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    for logical_line in text.split('\n') {
        let mut row = String::new();
        let mut row_width = 0usize;
        for character in logical_line.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if !row.is_empty() && row_width.saturating_add(character_width) > width {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            row.push(character);
            row_width = row_width.saturating_add(character_width);
        }
        if !row.is_empty() || logical_line.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

pub fn wrapped_line_count(text: &str, width: u16) -> usize {
    wrapped_display_rows(text, width).len()
}

pub fn bottom_scroll_for_wrap(text: &str, width: u16, height: u16) -> u16 {
    if text.is_empty() || width == 0 || height == 0 {
        return 0;
    }
    wrapped_line_count(text, width)
        .saturating_sub(usize::from(height))
        .min(usize::from(u16::MAX)) as u16
}

#[derive(Debug)]
struct PersistedActivity {
    timestamp: Option<chrono::DateTime<Utc>>,
    source_rank: u8,
    source_index: usize,
    activity: ActivityEntry,
}

fn bounded_activity_label(value: &str) -> String {
    let mut label = String::new();
    let mut truncated = false;
    let maximum = MAX_ACTIVITY_LABEL_BYTES.saturating_sub(3);
    for character in value.chars() {
        if label.len() >= maximum {
            truncated = true;
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ' ') {
            label.push(character);
        } else {
            label.push('?');
        }
    }
    if truncated {
        label.push_str("...");
    }
    if label.trim().is_empty() {
        "unknown".to_string()
    } else {
        label
    }
}

fn safe_event_atom(details: &serde_json::Value, field: &str) -> Option<String> {
    let value = details.get(field)?.as_str()?;
    if value.is_empty()
        || value.len() > MAX_ACTIVITY_LABEL_BYTES
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return None;
    }
    Some(value.to_string())
}

fn safe_event_number(details: &serde_json::Value, field: &str) -> Option<String> {
    details
        .get(field)
        .and_then(|value| value.as_u64().map(|value| value.to_string()))
}

pub(crate) fn safe_event_fields(details: &serde_json::Value, fields: &[&str]) -> String {
    fields
        .iter()
        .filter_map(|field| {
            safe_event_atom(details, field)
                .or_else(|| safe_event_number(details, field))
                .map(|value| format!("{field}={value}"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn safe_failure_fields(details: &serde_json::Value) -> String {
    let failure = details.get("failure").filter(|value| value.is_object());
    let evidence = failure.unwrap_or(details);
    safe_event_fields(
        evidence,
        &["class", "phase", "retry", "incident_code", "code"],
    )
}

fn is_failure_outcome(outcome: &str) -> bool {
    outcome.contains("failed")
        || outcome.contains("failure")
        || outcome.contains("refusal")
        || outcome.contains("interrupted")
        || outcome == "invalid_plan"
}

fn project_session_event(
    event: &SessionEvent,
    sensitive_values: &[String],
) -> Option<ActivityEntry> {
    let kind = event.kind.as_str();
    if matches!(kind, "tool_started" | "tool_completed") {
        // ToolCallRecord is the authoritative completed audit projection. Rendering
        // lifecycle events as well would duplicate the same persisted operation.
        return None;
    }

    let (activity_kind, title, body) = match kind {
        "run_started" => (
            ActivityKind::System,
            "run started".to_string(),
            String::new(),
        ),
        "steering_input" => (
            ActivityKind::User,
            "steer".to_string(),
            safe_event_fields(&event.details, &["sequence", "source"]),
        ),
        "steering_intake" => (
            ActivityKind::System,
            "steering accepted at safe boundary".to_string(),
            safe_event_fields(&event.details, &["sequence"]),
        ),
        "steering_delivery_failed" => (
            ActivityKind::Failure,
            "steering delivery failed".to_string(),
            safe_event_fields(&event.details, &["sequence", "reason"]),
        ),
        "provider_continuation_abandoned_by_steering" => (
            ActivityKind::System,
            "provider response superseded by steering".to_string(),
            String::new(),
        ),
        "tool_proposal_superseded_by_steering" => (
            ActivityKind::System,
            "tool proposal superseded by steering".to_string(),
            safe_event_fields(&event.details, &["first_sequence", "last_sequence"]),
        ),
        "run_terminal" => {
            let outcome =
                safe_event_atom(&event.details, "outcome").unwrap_or_else(|| "unknown".to_string());
            let activity_kind = if outcome.contains("cancel") {
                ActivityKind::Cancellation
            } else if outcome == "local_error" || is_failure_outcome(&outcome) {
                ActivityKind::Failure
            } else {
                ActivityKind::Reconcile
            };
            (
                activity_kind,
                format!("run terminal: {outcome}"),
                String::new(),
            )
        }
        "approval_required" => {
            let subject = safe_event_atom(&event.details, "tool_name")
                .or_else(|| safe_event_atom(&event.details, "kind"))
                .unwrap_or_else(|| "action".to_string());
            (
                ActivityKind::Approval,
                format!("{subject} approval required"),
                String::new(),
            )
        }
        "question_required" => {
            let option_count = event
                .details
                .get("options")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            (
                ActivityKind::Question,
                "agent question required".to_string(),
                format!("options={option_count}"),
            )
        }
        "compression" => (
            ActivityKind::Compression,
            "context compressed".to_string(),
            safe_event_fields(
                &event.details,
                &["before_tokens", "after_tokens", "summarized_through"],
            ),
        ),
        "cancel_requested" | "timer_cancelled" | "mcp_request_cancelled" => (
            ActivityKind::Cancellation,
            "cancellation requested".to_string(),
            safe_event_fields(&event.details, &["reason", "state"]),
        ),
        "reconciliation" => {
            let outcome =
                safe_event_atom(&event.details, "outcome").unwrap_or_else(|| "unknown".to_string());
            if outcome.contains("cancel") {
                (
                    ActivityKind::Cancellation,
                    format!("run cancelled: {outcome}"),
                    String::new(),
                )
            } else if event
                .details
                .get("failure")
                .is_some_and(|value| !value.is_null())
                || is_failure_outcome(&outcome)
            {
                (
                    ActivityKind::Failure,
                    format!("run failed: {outcome}"),
                    safe_failure_fields(&event.details),
                )
            } else {
                (
                    ActivityKind::Reconcile,
                    format!("run reconciled: {outcome}"),
                    String::new(),
                )
            }
        }
        "state_transition" => (
            ActivityKind::System,
            "run state transition".to_string(),
            safe_event_fields(&event.details, &["from", "to"]),
        ),
        kind @ ("plan_generation_conflict"
        | "plan_generated"
        | "stale_plan_approval_ignored"
        | "plan_approved"
        | "plan_invalidated"
        | "plan_binding_conflict"
        | "plan_superseded_by_steering"
        | "scheduled_plan_archived") => (
            ActivityKind::Plan,
            bounded_activity_label(&kind.replace('_', " ")),
            safe_event_fields(&event.details, &["reason", "stage"]),
        ),
        kind @ ("prepared_task_start_failed"
        | "queued_follow_up_start_failed"
        | "timer_failed"
        | "timer_schedule_failed"
        | "background_task_failed"
        | "delivery_failed"
        | "scheduled_agent_run_failed"
        | "tool_batch_rejected"
        | "role_violation") => (
            ActivityKind::Failure,
            bounded_activity_label(&kind.replace('_', " ")),
            safe_failure_fields(&event.details),
        ),
        "provider_continuation_interrupted" => (
            ActivityKind::Failure,
            "provider continuation interrupted".to_string(),
            String::new(),
        ),
        "scheduled_agent_run_completed" => (
            ActivityKind::Reconcile,
            "scheduled agent run completed".to_string(),
            safe_event_fields(&event.details, &["occurrence", "repeat_count"]),
        ),
        "background_task_completed" => (
            ActivityKind::Tool,
            "background task completed".to_string(),
            String::new(),
        ),
        "context_bounded" | "queued_follow_up_start_committed" | "timer_fired" => (
            ActivityKind::System,
            bounded_activity_label(&kind.replace('_', " ")),
            String::new(),
        ),
        _ => (
            ActivityKind::System,
            "unclassified session event".to_string(),
            String::new(),
        ),
    };
    Some(ActivityEntry {
        kind: activity_kind,
        title,
        body: bounded_activity_body(&body, sensitive_values),
    })
}

fn project_session_message(
    message: &crate::session::SessionMessage,
    sensitive_values: &[String],
) -> ActivityEntry {
    let normalized_role = message.role.to_ascii_lowercase();
    let (kind, title, body) = match normalized_role.as_str() {
        "user" => (
            ActivityKind::User,
            String::new(),
            bounded_activity_body(&message.content, sensitive_values),
        ),
        "assistant" => (
            ActivityKind::Assistant,
            String::new(),
            bounded_activity_body(&message.content, sensitive_values),
        ),
        "tool" => {
            let observation_count = serde_json::from_str::<serde_json::Value>(&message.content)
                .ok()
                .and_then(|value| {
                    value
                        .get("observations")
                        .and_then(serde_json::Value::as_array)
                        .map(Vec::len)
                });
            let body = observation_count
                .map(|count| format!("{count} persisted observation(s)"))
                .unwrap_or_else(|| "persisted tool result context".to_string());
            (ActivityKind::Tool, "tool result context".to_string(), body)
        }
        "system" => (
            ActivityKind::System,
            String::new(),
            bounded_activity_body(&message.content, sensitive_values),
        ),
        _ => (
            ActivityKind::System,
            "unsupported legacy message role".to_string(),
            "legacy message content omitted".to_string(),
        ),
    };
    ActivityEntry { kind, title, body }
}

pub fn project_session_activities(
    session: &Session,
    sensitive_values: &[String],
) -> Vec<ActivityEntry> {
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
    let mut persisted = Vec::new();
    for message in &session.messages {
        persisted.push(PersistedActivity {
            timestamp: message.timestamp,
            source_rank: 0,
            source_index: message.index,
            activity: project_session_message(message, sensitive_values),
        });
    }
    let event_start = session
        .events
        .len()
        .saturating_sub(MAX_PROJECTED_SESSION_EVENTS);
    if event_start > 0 {
        persisted.push(PersistedActivity {
            timestamp: None,
            source_rank: 1,
            source_index: 0,
            activity: ActivityEntry {
                kind: ActivityKind::System,
                title: format!("{event_start} earlier session event(s) omitted"),
                body: String::new(),
            },
        });
    }
    for event in &session.events[event_start..] {
        if let Some(activity) = project_session_event(event, sensitive_values) {
            persisted.push(PersistedActivity {
                timestamp: event.timestamp,
                source_rank: 1,
                source_index: event.index.saturating_add(1),
                activity,
            });
        }
    }
    for (index, call) in session.tool_calls.iter().enumerate() {
        let name = call
            .tool_name
            .as_deref()
            .map(bounded_activity_label)
            .unwrap_or_else(|| "tool".to_string());
        let status = if call.error.is_some() { "failed" } else { "ok" };
        let body = if call.error.is_some() {
            "recorded tool error; inspect the bounded session audit for details"
        } else if call.result.is_some() {
            "recorded tool result; inspect the bounded session audit for details"
        } else {
            "recorded tool call"
        };
        persisted.push(PersistedActivity {
            timestamp: call.timestamp,
            source_rank: 2,
            source_index: index,
            activity: ActivityEntry {
                kind: ActivityKind::Tool,
                title: format!("{name} {status}"),
                body: body.to_string(),
            },
        });
    }
    persisted.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.source_rank.cmp(&right.source_rank))
            .then_with(|| left.source_index.cmp(&right.source_index))
    });
    activities.extend(persisted.into_iter().map(|entry| entry.activity));
    if let Some(plan) = &session.plan {
        activities.push(plan_activity(plan, sensitive_values));
    }
    if let Some(summary) = &session.summary {
        activities.push(ActivityEntry {
            kind: ActivityKind::Compression,
            title: format!("summarized through message {}", session.summary_index),
            body: bounded_activity_body(summary, sensitive_values),
        });
    }
    for activity in &mut activities {
        sanitize_activity(activity, sensitive_values);
    }
    activities
}

fn sanitize_activity(activity: &mut ActivityEntry, sensitive_values: &[String]) {
    activity.title = bounded_public_text(
        &activity.title,
        sensitive_values,
        MAX_ACTIVITY_LABEL_BYTES,
        false,
    );
    activity.body = bounded_public_text(
        &activity.body,
        sensitive_values,
        MAX_ACTIVITY_BODY_BYTES,
        true,
    );
}

pub fn apply_stream_event(
    activities: &mut Vec<ActivityEntry>,
    event: StreamEvent,
    live_state: &mut Option<String>,
    sensitive_values: &[String],
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
                    last.body = bounded_activity_body(&last.body, sensitive_values);
                    sanitize_activity(last, sensitive_values);
                    return;
                }
            }
            activities.push(ActivityEntry {
                kind: ActivityKind::Assistant,
                title: "live".to_string(),
                body: bounded_activity_body(&content, sensitive_values),
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
        StreamEvent::ApprovalRequired { tool_name } => {
            let tool_name = bounded_status_value(&crate::tools::executor::redact_text(&tool_name));
            activities.push(ActivityEntry {
                kind: ActivityKind::Approval,
                title: format!("{tool_name} needs approval"),
                body: "Open the approval dock for bounded action context.".to_string(),
            });
        }
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
        StreamEvent::ToolStarted { tool_name } => {
            let tool_name = bounded_status_value(&crate::tools::executor::redact_text(&tool_name));
            activities.push(ActivityEntry {
                kind: ActivityKind::Tool,
                title: format!("{tool_name} running"),
                body: "started; bounded result follows authoritative tool audit".to_string(),
            });
        }
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
                last.body = bounded_activity_body(&last.body, sensitive_values);
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
                body: bounded_activity_body(&detail, sensitive_values),
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
        StreamEvent::Reconciled { outcome } => {
            let reduction = reduce_interaction(
                &InteractionState::default(),
                InteractionInput::ReconciledOutcome {
                    outcome: &outcome,
                    failure: false,
                },
            );
            let title = match reduction {
                InteractionReduction::Reconciled { outcome, .. } => outcome,
                _ => unreachable!("reconciliation input always has a terminal reduction"),
            };
            activities.push(ActivityEntry {
                kind: ActivityKind::Reconcile,
                title,
                body: String::new(),
            });
        }
        StreamEvent::Failure {
            failure,
            session_id,
        } => {
            let report = failure.user_report(session_id.as_deref());
            let (title, body) = report
                .split_once('\n')
                .map_or((report.as_str(), ""), |(title, body)| (title, body));
            activities.push(ActivityEntry {
                kind: ActivityKind::Failure,
                title: title.to_string(),
                body: bounded_activity_body(body, sensitive_values),
            });
        }
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
    if let Some(last) = activities.last_mut() {
        sanitize_activity(last, sensitive_values);
    }
}

fn plan_activity(plan: &crate::session::Plan, sensitive_values: &[String]) -> ActivityEntry {
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
        body: bounded_activity_body(&body, sensitive_values),
    }
}

fn bounded_activity_body(content: &str, sensitive_values: &[String]) -> String {
    bounded_public_text(content, sensitive_values, MAX_ACTIVITY_BODY_BYTES, true)
}

/// Redacts configured secret spellings, removes terminal-active controls, and returns a bounded
/// UTF-8 presentation value. `preserve_layout` permits only newline and tab layout controls; it
/// never permits carriage return, escape, bidi controls, or other terminal-active bytes.
pub fn bounded_public_text(
    value: &str,
    sensitive_values: &[String],
    max_bytes: usize,
    preserve_layout: bool,
) -> String {
    let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
        value,
        sensitive_values.iter().cloned(),
    );
    let sanitized = control_safe_text(&redacted, preserve_layout);
    if sanitized.len() <= max_bytes {
        return sanitized;
    }
    let mut end = max_bytes.saturating_sub(3);
    while end > 0 && !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &sanitized[..end])
}

pub(crate) fn control_safe_text(value: &str, preserve_layout: bool) -> String {
    value
        .chars()
        .map(|character| {
            let bidi_control = matches!(
                character,
                '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            );
            if (character.is_control() || bidi_control)
                && !(preserve_layout && matches!(character, '\n' | '\t'))
            {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

fn bounded_status_value(value: &str) -> String {
    bounded_public_text(value, &[], MAX_STATUS_VALUE_BYTES, false)
}

fn bounded_sensitive_status_value(value: &str, sensitive_values: &[String]) -> String {
    bounded_public_text(value, sensitive_values, MAX_STATUS_VALUE_BYTES, false)
}

fn sandbox_posture_label(posture: &EffectiveExecutionPosture) -> &'static str {
    match &posture.sandbox_route {
        crate::sandbox::SandboxExecutionRoute::Bwrap => "bwrap available",
        crate::sandbox::SandboxExecutionRoute::Direct => "direct fallback (no platform sandbox)",
        crate::sandbox::SandboxExecutionRoute::FailClosed(_) => "FAIL CLOSED (sandbox unavailable)",
    }
}

fn instruction_posture_label(posture: &EffectiveExecutionPosture) -> &'static str {
    match posture.instruction_posture {
        InstructionExecutionPosture::Configured => "configured",
        InstructionExecutionPosture::Tightened => "tightened by project instructions",
        InstructionExecutionPosture::InvalidFailClosed => {
            "INVALID project directive; execution FAILS CLOSED"
        }
    }
}

fn format_effective_execution_posture(posture: &EffectiveExecutionPosture) -> String {
    let warning = if posture.broad_or_off {
        "\nWARNING: BROADER/OFF posture selected or direct execution is effective; stronger per-action controls still apply."
    } else {
        ""
    };
    format!(
        "Configured approval preset: {}\nEffective execution posture:\n  approval behavior: {}\n  provider: {}\n  profile: {}\n  network: {}\n  mutation plan gate: {}\n  mutation managed-owned-worktree gate: {}\n  platform sandbox: {}\n  instruction boundary: {}\nPer-action AGENTS.md, skill, tool policy, worktree, and platform controls may only tighten this posture; the configured preset never overrides them.{warning}",
        bounded_status_value(&posture.configured_approval_preset),
        posture.effective_approval_mode,
        bounded_status_value(&posture.provider),
        bounded_status_value(&posture.profile),
        bounded_status_value(&posture.network),
        if posture.mutation_plan_gate { "required" } else { "not configured" },
        if posture.mutation_owned_worktree_gate { "required for tool-declared mutations" } else { "not required" },
        sandbox_posture_label(posture),
        instruction_posture_label(posture),
    )
}

fn persisted_context_usage(session: Option<&Session>, context_limit: usize) -> usize {
    let Some(session) = session else {
        return 0;
    };
    bounded_session_context(session, context_limit).approximate_tokens
}

pub fn format_interaction_chrome(
    project_root: &Path,
    runtime_profile_id: &str,
    session: Option<&Session>,
    session_id: &str,
    lifecycle: &str,
    queued: usize,
) -> Result<(String, String), String> {
    let config = load_nib_config_full(project_root)
        .map_err(|error| bounded_status_value(&error.to_string()))?;
    let sensitive_values = config.public_session_sensitive_values();
    let diagnostics = provider_diagnostics(&config.llm, None).map_err(|error| {
        format!(
            "failed resolving LLM transport: {}",
            bounded_sensitive_status_value(&error, &sensitive_values)
        )
    })?;
    let posture = ToolExecutor::effective_execution_posture(
        project_root,
        config.execution.clone(),
        &config.approvals,
    );
    let provider = bounded_sensitive_status_value(&diagnostics.provider, &sensitive_values);
    let model = bounded_sensitive_status_value(&diagnostics.model, &sensitive_values);
    let transport = diagnostics.transport;
    let reasoning =
        bounded_sensitive_status_value(&diagnostics.reasoning_effort, &sensitive_values);
    let name = session
        .and_then(|session| session.display_name.as_deref())
        .unwrap_or("");
    let name = if name.is_empty() {
        String::new()
    } else {
        format!(
            " \"{}\"",
            bounded_sensitive_status_value(name, &sensitive_values)
        )
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
    let worktree = crate::integrations::worktree::with_validated_session_worktree(
        project_root,
        session_id,
        |path| Ok(path.display().to_string()),
    )?
    .unwrap_or_else(|| "-".to_string());
    let worktree = bounded_sensitive_status_value(&worktree, &sensitive_values);
    let project = bounded_sensitive_status_value(
        project_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project"),
        &sensitive_values,
    );
    let session_id = bounded_sensitive_status_value(session_id, &sensitive_values);
    let runtime_profile_id = bounded_sensitive_status_value(runtime_profile_id, &sensitive_values);
    let header = format!(
        "{project}  ·  profile {runtime_profile_id}  ·  sess {session_id}{name}  ·  {origin}  ·  worktree {worktree}"
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
            format!(
                "  ·  plan {current}/{} {}",
                plan.steps.len(),
                bounded_sensitive_status_value(title, &sensitive_values)
            )
        })
        .unwrap_or_default();
    let context_used = persisted_context_usage(session, config.llm.context_length);
    let status = format!(
        "{}  ·  {provider}/{model} transport {transport} reasoning {reasoning}  ·  effective {}/{}:{} net {} sandbox {}  ·  context ~{context_used}/{}  ·  queue {queued}{plan}",
        bounded_sensitive_status_value(lifecycle, &sensitive_values),
        posture.effective_approval_mode,
        bounded_sensitive_status_value(&posture.provider, &sensitive_values),
        bounded_sensitive_status_value(&posture.profile, &sensitive_values),
        bounded_sensitive_status_value(&posture.network, &sensitive_values),
        sandbox_posture_label(&posture),
        config.llm.context_length,
    );
    Ok((header, status))
}

pub fn format_session_status(
    project_root: &Path,
    runtime_profile_id: &str,
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
        runtime_profile_id,
        session.as_ref(),
        session_id,
        lifecycle,
        queued,
    )?;
    let config = load_nib_config_full(project_root)
        .map_err(|error| bounded_status_value(&error.to_string()))?;
    let posture = ToolExecutor::effective_execution_posture(
        project_root,
        config.execution,
        &config.approvals,
    );
    Ok(format!(
        "{header}\n{status}\n{}",
        format_effective_execution_posture(&posture)
    ))
}

fn session_background_tasks(
    session_store: &SessionStore,
    session_id: &str,
) -> Result<Vec<crate::daemons::workload::SessionOwnedDurableTask>, String> {
    let store =
        crate::daemons::workload::DurableTaskStore::from_sessions_dir(session_store.sessions_dir())
            .map_err(|error| bounded_status_value(&error))?;
    let mut tasks = store
        .list_for_session(session_id)
        .map_err(|error| bounded_status_value(&error))?;
    tasks.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(tasks)
}

fn format_session_background_tasks(
    session_store: &SessionStore,
    session_id: &str,
    stoppable_only: bool,
) -> Result<String, String> {
    let tasks = session_background_tasks(session_store, session_id)?;
    let total = tasks
        .iter()
        .filter(|task| {
            !stoppable_only || !matches!(task.status.as_str(), "completed" | "failed" | "cancelled")
        })
        .count();
    let mut shown = 0usize;
    let mut output = if stoppable_only {
        "Session-owned running background work:".to_string()
    } else {
        "Session-owned background work:".to_string()
    };
    for task in tasks.iter().filter(|task| {
        !stoppable_only || !matches!(task.status.as_str(), "completed" | "failed" | "cancelled")
    }) {
        if shown >= MAX_INTERACTIVE_BACKGROUND_TASKS {
            break;
        }
        output.push_str(&format!(
            "\n  - {} | {} | {} | updated {}",
            bounded_status_value(&task.id),
            bounded_status_value(&task.kind),
            bounded_status_value(&task.status),
            task.updated_at.to_rfc3339(),
        ));
        shown += 1;
    }
    if shown == 0 {
        output.push_str("\n  (none)");
    }
    if total > shown {
        output.push_str(&format!(
            "\n  ... {} additional tasks omitted",
            total - shown
        ));
    }
    if stoppable_only {
        output.push_str("\nUse /stop <task-id> to request cancellation of exactly one task.");
    }
    Ok(output)
}

fn stop_session_background_task(
    session_store: &SessionStore,
    session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    let store =
        crate::daemons::workload::DurableTaskStore::from_sessions_dir(session_store.sessions_dir())
            .map_err(|error| bounded_status_value(&error))?;
    let task = store
        .cancel_for_session(task_id, session_id)
        .map_err(|error| bounded_status_value(&error))?;
    Ok(format!(
        "Background task {} cancellation reconciled with status {}.",
        bounded_status_value(&task.id),
        bounded_status_value(&task.status),
    ))
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

const MAX_PATH_ATTACHMENTS: usize = 8;

/// Resolve `@path` mentions into structured project attachments.
///
/// The returned text keeps the original mentions and does not expand file
/// contents into the prompt. Unsafe or escaped paths fail closed.
pub fn resolve_path_attachments(
    project_root: &Path,
    input: &str,
) -> Result<(String, Vec<PathAttachment>), String> {
    let Ok(root) = project_root.canonicalize() else {
        return Ok((input.to_string(), Vec::new()));
    };
    let mut attachments = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for token in path_mention_tokens(input) {
        if token.contains('\0') || token.starts_with('/') || token.contains("..") {
            return Err(format!(
                "path attachment '{token}' is outside the readable project root"
            ));
        }
        if token
            .split('/')
            .any(|segment| segment.starts_with('.') || segment.is_empty())
        {
            return Err(format!(
                "path attachment '{token}' is outside the readable project root"
            ));
        }
        let candidate = root.join(token);
        let metadata = match candidate.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                return Err(format!(
                    "path attachment '{token}' was not found in the project"
                ))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "path attachment '{token}' is not a readable project file"
            ));
        }
        let canonical = candidate.canonicalize().map_err(|_| {
            format!("path attachment '{token}' is outside the readable project root")
        })?;
        if !canonical.starts_with(&root) {
            return Err(format!(
                "path attachment '{token}' is outside the readable project root"
            ));
        }
        if !seen.insert(token.to_string()) {
            continue;
        }
        if attachments.len() >= MAX_PATH_ATTACHMENTS {
            return Err(format!(
                "at most {MAX_PATH_ATTACHMENTS} project path attachments are allowed"
            ));
        }
        attachments.push(PathAttachment {
            path: token.to_string(),
        });
    }
    Ok((input.to_string(), attachments))
}

fn path_mention_tokens(input: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'@' && (index == 0 || bytes[index - 1].is_ascii_whitespace()) {
            let start = index + 1;
            let mut end = start;
            while end < bytes.len()
                && matches!(
                    bytes[end],
                    b'A'..=b'Z'
                        | b'a'..=b'z'
                        | b'0'..=b'9'
                        | b'_'
                        | b'.'
                        | b'/'
                        | b'-'
                )
            {
                end += 1;
            }
            if end > start {
                if let Ok(token) = std::str::from_utf8(&bytes[start..end]) {
                    tokens.push(token);
                }
            }
            index = end;
            continue;
        }
        index += 1;
    }
    tokens
}

fn bounded_workspace_diff(
    project_root: &Path,
    sensitive_values: &[String],
) -> Result<String, String> {
    let output =
        crate::sandbox::worktree::run_git_bounded_sync(project_root, ["diff", "--no-color"])
            .map_err(|error| {
                let diagnostic = bounded_public_text(
                    &error.to_string(),
                    sensitive_values,
                    MAX_ACTIVITY_LABEL_BYTES,
                    false,
                );
                format!("failed to read bounded git diff: {diagnostic}")
            })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let diagnostic = bounded_public_text(
            stderr
                .trim()
                .lines()
                .next()
                .unwrap_or("repository unavailable"),
            sensitive_values,
            MAX_ACTIVITY_LABEL_BYTES,
            false,
        );
        return Err(format!("git diff failed: {diagnostic}"));
    }
    let raw_diff = String::from_utf8_lossy(&output.stdout);
    if raw_diff.len() > MAX_DIFF_REDACTION_INPUT_BYTES && !sensitive_values.is_empty() {
        return Ok("[REDACTED]".to_string());
    }
    let mut redaction_end = raw_diff.len().min(MAX_DIFF_REDACTION_INPUT_BYTES);
    while redaction_end > 0 && !raw_diff.is_char_boundary(redaction_end) {
        redaction_end -= 1;
    }
    let mut diff = bounded_public_text(
        &raw_diff[..redaction_end],
        sensitive_values,
        MAX_DIFF_REDACTION_INPUT_BYTES,
        true,
    );
    if diff.trim().is_empty() {
        return Ok("No workspace diff.".to_string());
    }
    if diff.len() > MAX_DIFF_BYTES {
        let mut end = MAX_DIFF_BYTES;
        while end > 0 && !diff.is_char_boundary(end) {
            end -= 1;
        }
        diff.truncate(end);
        diff.push_str("\n[diff truncated]");
    }
    Ok(diff)
}

fn bounded_session_workspace_diff(
    project_root: &Path,
    session_id: &str,
    sensitive_values: &[String],
) -> Result<String, String> {
    if let Some(diff) = crate::integrations::worktree::with_validated_session_worktree(
        project_root,
        session_id,
        |worktree| bounded_workspace_diff(worktree, sensitive_values),
    )? {
        return Ok(diff);
    }
    bounded_workspace_diff(project_root, sensitive_values)
}

fn format_current_plan(session: Option<&Session>, sensitive_values: &[String]) -> String {
    match session.and_then(|session| session.plan.as_ref()) {
        Some(plan) => plan_activity(plan, sensitive_values).render_line(),
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
    let name = bounded_public_text(
        name,
        store.public_sensitive_values(),
        MAX_DISPLAY_NAME_BYTES,
        false,
    );
    store
        .update_session(session_id, |session| {
            session.display_name = Some(name.clone());
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
    Ok(format!(
        "Configured approval preset set to '{}'. Stronger controls remain in force.\n{}",
        bounded_status_value(&mode),
        format_permissions(project_root)?
    ))
}

fn format_permissions(project_root: &Path) -> Result<String, String> {
    let config = load_nib_config_full(project_root)
        .map_err(|error| bounded_status_value(&error.to_string()))?;
    let posture = ToolExecutor::effective_execution_posture(
        project_root,
        config.execution,
        &config.approvals,
    );
    Ok(format_effective_execution_posture(&posture))
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

#[cfg(test)]
fn display_stream_event(event: StreamEvent) -> Option<StreamDisplay> {
    display_stream_event_with_sensitive_values(event, &[])
}

pub fn display_stream_event_with_sensitive_values(
    event: StreamEvent,
    sensitive_values: &[String],
) -> Option<StreamDisplay> {
    let display = display_stream_event_unchecked(event)?;
    Some(match display {
        StreamDisplay::Content(content) => StreamDisplay::Content(bounded_public_text(
            &content,
            sensitive_values,
            MAX_PUBLIC_PRESENTATION_BYTES,
            true,
        )),
        StreamDisplay::Status(status) => StreamDisplay::Status(bounded_public_text(
            &status,
            sensitive_values,
            MAX_PUBLIC_PRESENTATION_BYTES,
            true,
        )),
    })
}

fn display_stream_event_unchecked(event: StreamEvent) -> Option<StreamDisplay> {
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
        StreamEvent::ApprovalRequired { tool_name } => {
            let tool_name = bounded_status_value(&crate::tools::executor::redact_text(&tool_name));
            StreamDisplay::Status(format!("[approval required] {tool_name}"))
        }
        StreamEvent::QuestionRequired { question, options } => {
            let options = if options.is_empty() {
                String::new()
            } else {
                format!(" (options: {})", options.join(" | "))
            };
            StreamDisplay::Status(format!("[question] {question}{options}"))
        }
        StreamEvent::ToolStarted { tool_name } => {
            let tool_name = bounded_status_value(&crate::tools::executor::redact_text(&tool_name));
            StreamDisplay::Status(format!("[tool started] {tool_name}"))
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
            let reduction = reduce_interaction(
                &InteractionState::default(),
                InteractionInput::ReconciledOutcome {
                    outcome: &outcome,
                    failure: false,
                },
            );
            let outcome = match reduction {
                InteractionReduction::Reconciled { outcome, .. } => outcome,
                _ => unreachable!("reconciliation input always has a terminal reduction"),
            };
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
    if let InteractiveAvailability::Unavailable(reason) = spec.availability {
        return Err(format!("command /{} is unavailable: {reason}", spec.name));
    }
    validate_interactive_arguments(spec, &arguments)?;
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
        "history" => InteractiveCommand::History {
            query: if arguments.is_empty() {
                None
            } else {
                Some(arguments.join(" "))
            },
        },
        "ps" if arguments.is_empty() => InteractiveCommand::Ps,
        "stop" if arguments.len() <= 1 => InteractiveCommand::Stop {
            task_id: arguments.first().cloned(),
        },
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

fn validate_interactive_arguments(
    spec: &InteractiveCommandSpec,
    arguments: &[String],
) -> Result<(), String> {
    let valid = match spec.arguments {
        InteractiveArgumentSchema::None => arguments.is_empty(),
        InteractiveArgumentSchema::OptionalText => true,
        InteractiveArgumentSchema::RequiredText => !arguments.is_empty(),
        InteractiveArgumentSchema::OptionalSingle => arguments.len() <= 1,
        InteractiveArgumentSchema::Permissions => {
            arguments.len() <= 1
                && arguments.first().is_none_or(|argument| {
                    spec.completion
                        .candidates
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(argument))
                })
        }
        InteractiveArgumentSchema::Skills | InteractiveArgumentSchema::Mcp => true,
    };
    if valid {
        Ok(())
    } else {
        Err(format!("usage: {}", spec.usage))
    }
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
    execute_interactive_command_in_state(
        command,
        project_root,
        "default",
        store,
        session_id,
        "idle",
    )
}

pub fn execute_interactive_command_in_state(
    command: InteractiveCommand,
    project_root: &Path,
    runtime_profile_id: &str,
    store: &SessionStore,
    session_id: &str,
    lifecycle: &str,
) -> Result<InteractiveEffect, String> {
    match command {
        InteractiveCommand::Quit => Ok(InteractiveEffect::Quit),
        InteractiveCommand::Help => Ok(InteractiveEffect::Output(interactive_help())),
        InteractiveCommand::Status => Ok(InteractiveEffect::Output(format_session_status(
            project_root,
            runtime_profile_id,
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
                store.public_sensitive_values(),
            )))
        }
        InteractiveCommand::Plan {
            prompt: Some(prompt),
        } => Ok(InteractiveEffect::RunAgent {
            goal: prompt,
            mode: InteractiveAgentMode::Plan,
        }),
        InteractiveCommand::Review | InteractiveCommand::Diff => {
            Ok(InteractiveEffect::Output(bounded_session_workspace_diff(
                project_root,
                session_id,
                store.public_sensitive_values(),
            )?))
        }
        InteractiveCommand::Compact => Ok(InteractiveEffect::Compact),
        InteractiveCommand::Ps => Ok(InteractiveEffect::Output(format_session_background_tasks(
            store, session_id, false,
        )?)),
        InteractiveCommand::Stop { task_id: None } => Ok(InteractiveEffect::Output(
            format_session_background_tasks(store, session_id, true)?,
        )),
        InteractiveCommand::Stop {
            task_id: Some(task_id),
        } => Ok(InteractiveEffect::Output(stop_session_background_task(
            store, session_id, &task_id,
        )?)),
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
        InteractiveCommand::History { .. } => {
            Err("draft history search requires an interactive composer".to_string())
        }
        InteractiveCommand::Providers => {
            let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
            let sensitive_values = config.public_session_sensitive_values();
            let active = config.llm.get_active_provider();
            let mut output = String::from("Configured providers:");
            if config.llm.providers.is_empty() {
                output.push_str("\n  (none - using mock)");
            }
            for (name, entry) in &config.llm.providers {
                let marker = if name == &active { " (active)" } else { "" };
                let name =
                    bounded_public_text(name, &sensitive_values, MAX_STATUS_VALUE_BYTES, false);
                let model = bounded_public_text(
                    &entry.model,
                    &sensitive_values,
                    MAX_STATUS_VALUE_BYTES,
                    false,
                );
                output.push_str(&format!("\n  - {name}: {model}{marker}"));
            }
            Ok(InteractiveEffect::Output(output))
        }
        InteractiveCommand::Model { selection: None } => {
            let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
            let sensitive_values = config.public_session_sensitive_values();
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
                sensitive_values,
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
    let sensitive_values = config.public_session_sensitive_values();
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
    let model = bounded_public_text(model, &sensitive_values, MAX_STATUS_VALUE_BYTES, false);
    let provider = bounded_public_text(&provider, &sensitive_values, MAX_STATUS_VALUE_BYTES, false);
    Ok(format!(
        "Switched model to '{model}' for provider '{provider}'."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_nib_config_full, save_nib_config_full, NibConfig, ProviderEntry};
    use std::collections::HashSet;
    use std::process::Command;
    use tempfile::tempdir;

    fn git_repository() -> tempfile::TempDir {
        let repository = tempdir().expect("repository");
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(repository.path())
                .args(args)
                .output()
                .expect("git fixture command");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        std::fs::write(repository.path().join("tracked.txt"), "original\n")
            .expect("tracked fixture");
        run(&["add", "tracked.txt"]);
        run(&[
            "-c",
            "user.name=nib tests",
            "-c",
            "user.email=nib@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        repository
    }

    #[test]
    fn shared_interaction_reducer_precedence_is_total_and_table_driven() {
        let cases = [
            (
                InteractionState {
                    approval_pending: true,
                    question_pending: true,
                    destructive_confirmation_pending: true,
                    selector_or_detail: Some(SelectorDetailKind::Selector),
                    completion_pending: true,
                    run: InteractionRunState::Running,
                },
                InteractionConsumer::Approval,
            ),
            (
                InteractionState {
                    question_pending: true,
                    destructive_confirmation_pending: true,
                    selector_or_detail: Some(SelectorDetailKind::Selector),
                    completion_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::Question,
            ),
            (
                InteractionState {
                    destructive_confirmation_pending: true,
                    selector_or_detail: Some(SelectorDetailKind::Selector),
                    completion_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::DestructiveConfirmation,
            ),
            (
                InteractionState {
                    selector_or_detail: Some(SelectorDetailKind::Selector),
                    completion_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::Selector,
            ),
            (
                InteractionState {
                    selector_or_detail: Some(SelectorDetailKind::Detail),
                    completion_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::Detail,
            ),
            (
                InteractionState {
                    completion_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::Completion,
            ),
            (InteractionState::default(), InteractionConsumer::Composer),
        ];

        for (state, expected) in cases {
            assert_eq!(active_interaction_consumer(&state), expected);
            assert_eq!(
                reduce_interaction(&state, InteractionInput::UserAction),
                InteractionReduction::Consumed(expected)
            );
        }
    }

    #[test]
    fn interaction_lifecycle_is_derived_from_authoritative_run_and_modal_state() {
        for (state, expected) in [
            (InteractionState::default(), InteractionLifecycle::Idle),
            (
                InteractionState {
                    run: InteractionRunState::Planning,
                    ..InteractionState::default()
                },
                InteractionLifecycle::Planning,
            ),
            (
                InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionLifecycle::Running,
            ),
            (
                InteractionState {
                    run: InteractionRunState::Reconciling,
                    ..InteractionState::default()
                },
                InteractionLifecycle::Reconciling,
            ),
            (
                InteractionState {
                    approval_pending: true,
                    question_pending: true,
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionLifecycle::WaitingApproval,
            ),
            (
                InteractionState {
                    question_pending: true,
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionLifecycle::WaitingQuestion,
            ),
            (
                InteractionState {
                    run: InteractionRunState::Completed,
                    ..InteractionState::default()
                },
                InteractionLifecycle::Completed,
            ),
            (
                InteractionState {
                    run: InteractionRunState::Cancelled,
                    ..InteractionState::default()
                },
                InteractionLifecycle::Cancelled,
            ),
            (
                InteractionState {
                    run: InteractionRunState::Failed,
                    ..InteractionState::default()
                },
                InteractionLifecycle::Failed,
            ),
        ] {
            assert_eq!(state.lifecycle(), expected);
        }
    }

    #[test]
    fn shared_interaction_reducer_yields_one_effect_without_fallthrough() {
        let approval_state = InteractionState {
            approval_pending: true,
            question_pending: true,
            completion_pending: true,
            ..InteractionState::default()
        };
        assert_eq!(
            reduce_interaction(
                &approval_state,
                InteractionInput::SubmittedLine("ordinary goal must not run"),
            ),
            InteractionReduction::Consumed(InteractionConsumer::Approval)
        );

        let command_error = reduce_interaction(
            &InteractionState::default(),
            InteractionInput::SubmittedLine("/unknown private-goal-sentinel"),
        );
        assert!(matches!(
            &command_error,
            InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                ..
            }
        ));
        assert!(!matches!(&command_error, InteractionReduction::IdleTurn(_)));
        let InteractionReduction::Error { message, .. } = &command_error else {
            unreachable!();
        };
        assert!(!message.contains("private-goal-sentinel"));
        assert!(message.len() <= MAX_STATUS_VALUE_BYTES);
        assert!(!message.chars().any(char::is_control));

        assert_eq!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::SubmittedLine("next ordinary turn"),
            ),
            InteractionReduction::QueueNext("next ordinary turn".to_string())
        );
        assert!(matches!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::SubmittedLine("/status"),
            ),
            InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                ..
            }
        ));
        assert_eq!(
            reduce_interaction(
                &InteractionState::default(),
                InteractionInput::SubmittedLine("queue: durable follow-up"),
            ),
            InteractionReduction::QueueNext("durable follow-up".to_string())
        );
        assert_eq!(
            reduce_interaction(
                &InteractionState::default(),
                InteractionInput::SubmittedLine("ordinary idle turn"),
            ),
            InteractionReduction::IdleTurn("ordinary idle turn".to_string())
        );
        assert!(matches!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::SubmittedLine("/history unicode"),
            ),
            InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                ..
            }
        ));
        for (state, submitted) in [
            (InteractionState::default(), "queue:"),
            (InteractionState::default(), "queue:   "),
            (InteractionState::default(), "steer:"),
            (
                InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                "steer:   ",
            ),
        ] {
            let reduction = reduce_interaction(&state, InteractionInput::SubmittedLine(submitted));
            assert!(matches!(
                reduction,
                InteractionReduction::Error {
                    consumer: InteractionConsumer::Composer,
                    ..
                }
            ));
        }
    }

    #[test]
    fn shared_interaction_reducer_rejects_stale_events_and_recovers_invalid_actions() {
        let state = InteractionState {
            question_pending: true,
            ..InteractionState::default()
        };
        assert_eq!(
            reduce_interaction(
                &state,
                InteractionInput::SessionRunEvent {
                    active_session_id: "session-a",
                    active_run_id: Some("run-current"),
                    event_session_id: "session-a",
                    event_run_id: "run-current",
                },
            ),
            InteractionReduction::Consumed(InteractionConsumer::Timeline)
        );
        for (event_session_id, event_run_id, active_run_id) in [
            ("session-b", "run-current", Some("run-current")),
            ("session-a", "run-old", Some("run-current")),
            ("session-a", "run-current", None),
        ] {
            assert_eq!(
                reduce_interaction(
                    &state,
                    InteractionInput::SessionRunEvent {
                        active_session_id: "session-a",
                        active_run_id,
                        event_session_id,
                        event_run_id,
                    },
                ),
                InteractionReduction::StaleEvent
            );
        }

        let invalid = reduce_interaction(&state, InteractionInput::InvalidAction);
        let InteractionReduction::Error { consumer, message } = invalid else {
            panic!("invalid action must become a typed bounded error");
        };
        assert_eq!(consumer, InteractionConsumer::Question);
        assert_eq!(message, "invalid interaction action; input was not applied");
        assert!(message.len() < 128);
        assert!(!message.chars().any(char::is_control));
    }

    #[test]
    fn shared_interaction_reducer_owns_modal_answers_and_terminal_outcomes() {
        let options = vec!["plan".to_string(), "execute".to_string()];
        let cases = [
            (
                InteractionState {
                    approval_pending: true,
                    ..InteractionState::default()
                },
                InteractionInput::ApprovalAnswer("yes"),
                InteractionReduction::ApprovalDecision(InteractionDecision::Accept),
            ),
            (
                InteractionState {
                    approval_pending: true,
                    ..InteractionState::default()
                },
                InteractionInput::ApprovalAnswer("no"),
                InteractionReduction::ApprovalDecision(InteractionDecision::Reject),
            ),
            (
                InteractionState {
                    question_pending: true,
                    ..InteractionState::default()
                },
                InteractionInput::QuestionAnswer {
                    answer: "2",
                    options: &options,
                    selected_option: None,
                },
                InteractionReduction::QuestionAnswered("execute".to_string()),
            ),
            (
                InteractionState {
                    question_pending: true,
                    ..InteractionState::default()
                },
                InteractionInput::QuestionAnswer {
                    answer: "",
                    options: &options,
                    selected_option: Some(0),
                },
                InteractionReduction::QuestionAnswered("plan".to_string()),
            ),
            (
                InteractionState {
                    destructive_confirmation_pending: true,
                    ..InteractionState::default()
                },
                InteractionInput::ConfirmationAnswer("y"),
                InteractionReduction::ConfirmationDecision(InteractionDecision::Accept),
            ),
        ];
        for (state, input, expected) in cases {
            assert_eq!(reduce_interaction(&state, input), expected);
        }

        assert!(matches!(
            reduce_interaction(
                &InteractionState {
                    question_pending: true,
                    ..InteractionState::default()
                },
                InteractionInput::QuestionAnswer {
                    answer: "3",
                    options: &options,
                    selected_option: None,
                },
            ),
            InteractionReduction::Error {
                consumer: InteractionConsumer::Question,
                ..
            }
        ));
        for (outcome, failure, expected) in [
            ("completed", false, InteractionTerminalOutcome::Completed),
            (
                "cancelled_by_user",
                false,
                InteractionTerminalOutcome::Cancelled,
            ),
            (
                "waiting_for_user_input",
                false,
                InteractionTerminalOutcome::WaitingForInput,
            ),
            ("provider_error", true, InteractionTerminalOutcome::Failed),
            (
                "unknown_terminal",
                false,
                InteractionTerminalOutcome::Failed,
            ),
        ] {
            assert_eq!(
                reduce_interaction(
                    &InteractionState::default(),
                    InteractionInput::ReconciledOutcome { outcome, failure },
                ),
                InteractionReduction::Reconciled {
                    outcome: outcome.to_string(),
                    terminal: expected,
                }
            );
        }
    }

    #[test]
    fn bounded_draft_history_preserves_policy_and_searches_unicode_safely() {
        let mut history = DraftHistory::default();
        assert!(history.search("").matches.is_empty());

        history.remember_submission("alpha 🙂");
        history.remember_submission("alpha 🙂");
        history.remember_submission("beta 界\nnext\0private");
        history.remember_submission("alpha 🙂");
        assert_eq!(
            history.entries().len(),
            3,
            "only consecutive duplicates collapse"
        );
        history.remember_submission("/history alpha");
        history.discard_latest_if("/history alpha");
        assert_eq!(
            history.entries().len(),
            3,
            "search control is not draft history"
        );

        let unicode = history.search("界");
        assert_eq!(unicode.matches.len(), 1);
        assert_eq!(unicode.matches[0].entry_index, 1);
        assert!(unicode.matches[0].display.contains("界"));
        assert!(unicode.matches[0].display.contains('↵'));
        assert!(!unicode.matches[0].display.contains('\0'));

        let query = format!("{}\0", "🙂".repeat(MAX_DRAFT_HISTORY_QUERY_BYTES));
        let bounded = history.search(&query);
        assert!(bounded.query_truncated);
        assert!(bounded.controls_omitted);
        assert!(bounded.query.len() <= MAX_DRAFT_HISTORY_QUERY_BYTES);
        assert!(bounded.query.is_char_boundary(bounded.query.len()));
        assert!(bounded.matches.iter().all(|result| result.display.len()
            <= MAX_DRAFT_HISTORY_DISPLAY_BYTES
            && !result.display.chars().any(char::is_control)));
        assert!(history.search("no-such-draft").matches.is_empty());
    }

    #[test]
    fn bounded_draft_history_evicts_oldest_entries_at_fifty() {
        let mut history = DraftHistory::default();
        for index in 0..=MAX_DRAFT_HISTORY {
            history.remember_submission(&format!("goal-{index}"));
        }
        assert_eq!(history.entries().len(), MAX_DRAFT_HISTORY);
        assert_eq!(
            history.entries().first().map(String::as_str),
            Some("goal-1")
        );
        assert_eq!(
            history.entries().last().map(String::as_str),
            Some("goal-50")
        );
        assert_eq!(history.search("").matches.len(), MAX_DRAFT_HISTORY_RESULTS);
    }

    #[test]
    fn transcript_viewport_clamps_resizes_and_preserves_unpinned_rows_on_append() {
        let mut viewport = TranscriptViewport::default();
        viewport.observe_layout(30, 5);
        assert!(viewport.is_pinned_to_tail());
        assert_eq!(viewport.top_row(), 25);

        viewport.apply(TranscriptViewportAction::PageUp);
        assert!(!viewport.is_pinned_to_tail());
        assert_eq!(viewport.top_row(), 20);
        viewport.observe_layout(40, 5);
        assert_eq!(
            viewport.top_row(),
            20,
            "append must preserve an unpinned viewport"
        );

        viewport.observe_layout(40, 25);
        assert_eq!(
            viewport.top_row(),
            15,
            "resize clamps to the last valid top row"
        );
        viewport.apply(TranscriptViewportAction::PageDown);
        assert_eq!(viewport.top_row(), 15);
        assert!(
            !viewport.is_pinned_to_tail(),
            "down does not implicitly repin"
        );
        viewport.apply(TranscriptViewportAction::JumpToEnd);
        assert!(viewport.is_pinned_to_tail());
        assert_eq!(viewport.top_row(), 15);

        viewport.observe_layout(3, 20);
        assert_eq!(viewport.top_row(), 0);
        viewport.apply(TranscriptViewportAction::PageUp);
        assert!(
            viewport.is_pinned_to_tail(),
            "no upward movement leaves pin state intact"
        );

        viewport.observe_layout(100_000, 7);
        assert_eq!(
            viewport.top_row(),
            99_993,
            "large row counts do not truncate to u16"
        );
    }

    #[test]
    fn transcript_wrapping_and_scroll_reducer_are_unicode_and_narrow_width_safe() {
        assert_eq!(wrapped_display_rows("漢字a", 2), ["漢", "字", "a"]);
        assert_eq!(wrapped_line_count("e\u{301}", 1), 1);
        assert_eq!(bottom_scroll_for_wrap("漢字a", 2, 1), 2);
        assert_eq!(wrapped_display_rows("", 0), [""]);

        let state = InteractionState::default();
        assert_eq!(
            reduce_interaction(
                &state,
                InteractionInput::Transcript(TranscriptViewportAction::PageUp),
            ),
            InteractionReduction::Transcript(TranscriptViewportAction::PageUp)
        );
        assert_eq!(
            reduce_interaction(&state, InteractionInput::OpenHistorySearch),
            InteractionReduction::OpenHistorySearch { query: None }
        );
        assert_eq!(
            reduce_interaction(
                &InteractionState {
                    completion_pending: true,
                    ..InteractionState::default()
                },
                InteractionInput::OpenHistorySearch,
            ),
            InteractionReduction::Consumed(InteractionConsumer::Completion)
        );
    }

    #[test]
    fn shared_parser_defines_the_complete_interactive_command_vocabulary() {
        let mut names = HashSet::new();
        let help = interactive_help();
        for spec in INTERACTIVE_COMMANDS {
            assert!(!spec.name.is_empty());
            assert!(!spec.usage.is_empty());
            assert!(!spec.summary.is_empty());
            assert!(help.contains(spec.usage));
            assert_eq!(spec.availability, InteractiveAvailability::Available);
            assert!(matches!(
                spec.mutability,
                InteractiveMutability::ReadOnly
                    | InteractiveMutability::Runtime
                    | InteractiveMutability::Session
                    | InteractiveMutability::Configuration
            ));
            for candidate in spec.completion.candidates {
                assert!(spec.usage.contains(candidate));
            }
            for candidate in spec.completion.argument_after {
                assert!(spec.completion.candidates.contains(candidate));
            }
            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                assert!(names.insert(name), "duplicate command token: {name}");
                let command = if spec.arguments == InteractiveArgumentSchema::RequiredText {
                    format!("/{name} example")
                } else {
                    format!("/{name}")
                };
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
        for invalid in [
            "/status extra",
            "/rename",
            "/permissions unsupported",
            "/model one two",
            "/stop one two",
        ] {
            assert!(
                parse_interactive_command(invalid).is_err(),
                "registry argument schema must reject {invalid}"
            );
        }
        assert_eq!(
            parse_interactive_command("/q")
                .unwrap()
                .unwrap()
                .spec()
                .name,
            "quit"
        );
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
    fn legacy_sensitive_session_ids_are_rejected_before_selector_preview() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        save_nib_config_full(project.path(), &mut config).expect("initial config");
        let unprotected = SessionStore::for_project(project.path()).expect("initial store");
        unprotected
            .try_create_session_with_id("legacy-private-session")
            .expect("legacy session");

        let mut config = load_nib_config_full(project.path()).expect("reload config");
        config.llm.providers.insert(
            "inactive-openai".to_string(),
            crate::config::ProviderEntry {
                model: "fixture".to_string(),
                api_key: Some("legacy-private-session".to_string()),
                ..Default::default()
            },
        );
        save_nib_config_full(project.path(), &mut config).expect("sensitive config");
        let scope = resolve_interactive_profile_scope(project.path()).expect("profile scope");
        let store = scope.session_store();
        let safe_active = store.try_create_session().expect("safe active session");

        let listing_error = interactive_session_selection(store, &safe_active.id)
            .expect_err("sensitive legacy listing");
        assert_eq!(
            listing_error,
            "failed to list sessions: session identifier conflicts with configured sensitive data"
        );
        assert!(!listing_error.contains("legacy-private-session"));
        let preview_error =
            interactive_session_candidate(store, "legacy-private-session", &safe_active.id)
                .expect_err("sensitive legacy preview");
        assert_eq!(
            preview_error,
            "session identifier conflicts with configured sensitive data"
        );
        assert!(!preview_error.contains("legacy-private-session"));
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

        let StreamDisplay::Status(report) =
            display_stream_event(event.clone()).expect("failure display")
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

        let mut activities = Vec::new();
        let mut live_state = None;
        apply_stream_event(&mut activities, event, &mut live_state, &[]);
        let projected = activities[0].render_line();
        assert_eq!(projected.matches("LLM request failed").count(), 1);
        assert!(projected.contains("LLM request failed [LLM-AUTH]"));
        assert!(!projected.contains("private diagnostic"));
    }

    #[test]
    fn public_stream_presentation_redacts_encoded_secrets_and_terminal_controls() {
        let secret = "plain/output-secret".to_string();
        let StreamDisplay::Content(content) = display_stream_event_with_sensitive_values(
            StreamEvent::Content(format!(
                "raw={secret} json=plain\\/output-secret b64=cGxhaW4vb3V0cHV0LXNlY3JldA== \u{1b}[2J\u{202e}tail"
            )),
            std::slice::from_ref(&secret),
        )
        .expect("content display")
        else {
            panic!("content event must remain content")
        };
        assert!(!content.contains(&secret));
        assert!(!content.contains(r"plain\/output-secret"));
        assert!(!content.contains("cGxhaW4vb3V0cHV0LXNlY3JldA"));
        assert!(!content.contains('\u{1b}'));
        assert!(!content.contains('\u{202e}'));
        assert!(content.contains("[REDACTED]"));
        assert!(content.len() <= MAX_PUBLIC_PRESENTATION_BYTES);
    }

    #[test]
    fn approval_stream_presentations_never_render_raw_arguments() {
        let event = StreamEvent::ApprovalRequired {
            tool_name: "run_terminal".to_string(),
        };
        let StreamDisplay::Status(display) =
            display_stream_event(event.clone()).expect("approval display")
        else {
            panic!("approval is a status")
        };
        assert_eq!(display, "[approval required] run_terminal");
        let mut activities = Vec::new();
        let mut live_state = None;
        apply_stream_event(&mut activities, event, &mut live_state, &[]);
        let rendered = activities[0].render_line();
        assert!(rendered.contains("run_terminal"));
        assert!(!rendered.contains("RAW_APPROVAL_SENTINEL"));
        assert!(!rendered.contains("sk-privateapproval"));
        assert!(!rendered.contains('\u{1b}'));

        let StreamDisplay::Status(unsafe_name) =
            display_stream_event(StreamEvent::ApprovalRequired {
                tool_name: "run_terminal\u{1b}[2J\nsk-privateapproval123456".to_string(),
            })
            .expect("unsafe approval display")
        else {
            panic!("approval is a status")
        };
        assert!(!unsafe_name.contains('\u{1b}'));
        assert!(!unsafe_name.contains('\n'));
        assert!(!unsafe_name.contains("sk-privateapproval"));
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
    fn live_input_distinguishes_queue_and_exact_run_steering() {
        assert_eq!(
            classify_composer_submit(false),
            ComposerSubmitKind::IdleTurn
        );
        assert_eq!(
            classify_composer_submit(true),
            ComposerSubmitKind::QueueNext
        );
        assert!(steer_hint_message().contains("Enter queues"));
        assert_eq!(parse_queue_line("queue: next goal"), Some("next goal"));
        assert_eq!(parse_steer_line("steer: adjust now"), Some("adjust now"));
        assert_eq!(parse_queue_line("hello"), None);
        assert_eq!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::SubmittedLine("steer: adjust now"),
            ),
            InteractionReduction::SteerCurrent("adjust now".to_string())
        );
        assert_eq!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::ComposerSubmit("steer: adjust now"),
            ),
            InteractionReduction::SteerCurrent("adjust now".to_string())
        );
        assert_eq!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::ComposerSubmit("/quit"),
            ),
            InteractionReduction::Command(InteractiveCommand::Quit)
        );
        assert!(matches!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::ComposerSubmit("/unknown must-never-be-a-goal"),
            ),
            InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                ..
            }
        ));
        assert_eq!(
            reduce_interaction(
                &InteractionState {
                    run: InteractionRunState::Running,
                    ..InteractionState::default()
                },
                InteractionInput::ComposerSubmit("ordinary queued goal"),
            ),
            InteractionReduction::QueueNext("ordinary queued goal".to_string())
        );

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

        let private_run_id = "0123456789abcdef0123456789abcdef";
        store
            .record_event(
                &session.id,
                "steering_input",
                serde_json::json!({
                    "run_id": private_run_id,
                    "sequence": 1,
                    "source": "plain",
                    "text": "credential sk-private-activity-sentinel\u{1b}[2J",
                }),
            )
            .expect("legacy steering evidence");
        let persisted = store.load(&session.id).expect("steering session");
        let steering = project_session_activities(&persisted, &[])
            .into_iter()
            .find(|activity| activity.title == "steer")
            .expect("typed steering activity");
        assert_eq!(steering.kind, ActivityKind::User);
        assert!(steering.body.contains("sequence=1"));
        assert!(steering.body.contains("source=plain"));
        assert!(!steering.body.contains("sk-private-activity-sentinel"));
        assert!(!steering.body.contains('\u{1b}'));
        assert!(!steering.render_line().contains(private_run_id));
    }

    #[test]
    fn plan_prompt_returns_a_typed_plan_mode_effect() {
        let directory = tempdir().expect("project");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let session = store.try_create_session().expect("session");

        let effect = execute_interactive_command(
            InteractiveCommand::Plan {
                prompt: Some("inspect without mutation".to_string()),
            },
            directory.path(),
            &store,
            &session.id,
        )
        .expect("plan effect");

        assert_eq!(
            effect,
            InteractiveEffect::RunAgent {
                goal: "inspect without mutation".to_string(),
                mode: InteractiveAgentMode::Plan,
            }
        );
        assert_eq!(InteractiveAgentMode::Execute.as_str(), "execute");
        assert_eq!(InteractiveAgentMode::Plan.as_str(), "plan");
        assert_eq!(InteractiveAgentMode::Compact.as_str(), "compact");
    }

    #[test]
    fn diff_uses_project_fallback_and_truncates_on_a_utf8_boundary() {
        let repository = git_repository();
        let store = SessionStore::for_project(repository.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        std::fs::write(
            repository.path().join("tracked.txt"),
            format!("project fallback\n{}\n", "界".repeat(MAX_DIFF_BYTES)),
        )
        .expect("project change");

        let InteractiveEffect::Output(diff) = execute_interactive_command(
            InteractiveCommand::Diff,
            repository.path(),
            &store,
            &session.id,
        )
        .expect("project diff") else {
            panic!("diff output");
        };

        assert!(diff.contains("project fallback"));
        assert!(diff.ends_with("[diff truncated]"));
        assert!(diff.len() <= MAX_DIFF_BYTES + "\n[diff truncated]".len());
        assert!(std::str::from_utf8(diff.as_bytes()).is_ok());
    }

    #[test]
    fn diff_redacts_credentials_before_the_raw_output_bound() {
        let repository = git_repository();
        let secret = format!("diff/boundary/{}", "s".repeat(512));
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        config.llm.providers.insert(
            "inactive-openai".to_string(),
            ProviderEntry {
                model: "safe-model".to_string(),
                api_key: Some(secret.clone()),
                ..ProviderEntry::default()
            },
        );
        save_nib_config_full(repository.path(), &mut config).expect("sensitive config");
        let store = SessionStore::for_project(repository.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        std::fs::write(
            repository.path().join("tracked.txt"),
            format!("{}{}-safe-tail\n", "p".repeat(MAX_DIFF_BYTES - 256), secret),
        )
        .expect("boundary diff");

        let InteractiveEffect::Output(diff) = execute_interactive_command(
            InteractiveCommand::Diff,
            repository.path(),
            &store,
            &session.id,
        )
        .expect("safe diff") else {
            panic!("diff output")
        };

        assert!(diff.contains("[REDACTED]"), "{diff:?}");
        assert!(
            !diff.contains(&secret[..128]),
            "credential prefix survived diff truncation: {diff:?}"
        );
        assert!(diff.len() <= MAX_DIFF_BYTES + "\n[diff truncated]".len());
    }

    #[test]
    fn oversized_diff_fails_closed_for_long_percent_encoded_credentials() {
        let repository = git_repository();
        let secret = "A ".repeat(10_000);
        let percent_secret = "A%20".repeat(10_000);
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        config.llm.providers.insert(
            "inactive-openai".to_string(),
            ProviderEntry {
                model: "safe-model".to_string(),
                api_key: Some(secret),
                ..ProviderEntry::default()
            },
        );
        save_nib_config_full(repository.path(), &mut config).expect("sensitive config");
        let store = SessionStore::for_project(repository.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        std::fs::write(
            repository.path().join("tracked.txt"),
            format!("{}{}\n", "p".repeat(30 * 1024), percent_secret),
        )
        .expect("oversized encoded diff");

        let InteractiveEffect::Output(diff) = execute_interactive_command(
            InteractiveCommand::Diff,
            repository.path(),
            &store,
            &session.id,
        )
        .expect("safe diff") else {
            panic!("diff output")
        };

        assert_eq!(diff, "[REDACTED]");
        assert!(!diff.contains("A%20"));
    }

    #[test]
    fn review_selects_the_owned_session_worktree() {
        let repository = git_repository();
        let store = SessionStore::for_project(repository.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        let mut manager =
            crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf());
        let worktree = manager
            .create_for_session(&session.id)
            .expect("owned session worktree");
        std::fs::write(
            repository.path().join("tracked.txt"),
            "project root change\n",
        )
        .expect("project change");
        std::fs::write(worktree.join("tracked.txt"), "owned worktree change\n")
            .expect("worktree change");

        let InteractiveEffect::Output(review) = execute_interactive_command(
            InteractiveCommand::Review,
            repository.path(),
            &store,
            &session.id,
        )
        .expect("owned review") else {
            panic!("review output");
        };

        assert!(review.contains("owned worktree change"));
        assert!(!review.contains("project root change"));
    }

    #[test]
    fn diff_rejects_a_replaced_owned_session_worktree() {
        let repository = git_repository();
        let store = SessionStore::for_project(repository.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        let mut manager =
            crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf());
        let worktree = manager
            .create_for_session(&session.id)
            .expect("owned session worktree");
        let displaced = worktree.with_extension("owned-away");
        std::fs::rename(&worktree, &displaced).expect("displace owned worktree");
        std::fs::create_dir(&worktree).expect("replacement worktree");
        std::fs::write(worktree.join("sentinel"), "replacement").expect("replacement sentinel");

        let error = execute_interactive_command(
            InteractiveCommand::Diff,
            repository.path(),
            &store,
            &session.id,
        )
        .expect_err("replaced worktree must fail closed");

        assert!(
            error.contains("ownership") || error.contains("identity"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("sentinel")).expect("replacement retained"),
            "replacement"
        );
    }

    #[test]
    fn review_rejects_an_ownership_record_that_escapes_managed_state() {
        let repository = git_repository();
        let store = SessionStore::for_project(repository.path()).expect("session store");
        let session = store.try_create_session().expect("session");
        let mut manager =
            crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf());
        manager
            .create_for_session(&session.id)
            .expect("owned session worktree");
        let ownership_directory = repository.path().join(".nib/worktree-ownership");
        let ownership_path = std::fs::read_dir(&ownership_directory)
            .expect("ownership directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .expect("ownership record");
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ownership_path).expect("read ownership record"))
                .expect("ownership JSON");
        record["worktree_path"] = serde_json::Value::String("/tmp/nib-escape".to_string());
        std::fs::write(
            &ownership_path,
            serde_json::to_vec(&record).expect("encode ownership record"),
        )
        .expect("replace ownership contents");

        let error = execute_interactive_command(
            InteractiveCommand::Review,
            repository.path(),
            &store,
            &session.id,
        )
        .expect_err("escaped durable ownership must fail closed");

        assert!(error.contains("escapes managed state"), "{error}");
    }

    #[test]
    fn queued_startup_failure_retains_fifo_and_records_audit() {
        let directory = tempdir().expect("session directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let session = store.try_create_session().expect("session");
        let first = persist_queued_follow_up(&store, &session.id, "first", "composer")
            .expect("first queued item");
        let second = persist_queued_follow_up(&store, &session.id, "second", "composer")
            .expect("second queued item");

        let error = claim_next_queued_follow_up_after_startup::<()>(&store, &session.id, |_| {
            Err("deterministic worker startup failure".to_string())
        })
        .expect_err("startup must fail");
        assert!(error.contains("remains queued"));

        let persisted = store
            .load_result(&session.id)
            .expect("load session")
            .expect("session");
        assert_eq!(persisted.queued_follow_ups, vec![first.clone(), second]);
        let audit = persisted.events.last().expect("startup failure audit");
        assert_eq!(audit.kind, "queued_follow_up_start_failed");
        assert_eq!(audit.details["queue_id"], first.id);
        assert_eq!(audit.details["phase"], "worker_startup");
        assert_eq!(audit.details["disposition"], "retained");
        assert!(!audit.details.to_string().contains("deterministic worker"));
    }

    #[test]
    fn queued_claim_is_fifo_and_activation_failure_restores_once() {
        let directory = tempdir().expect("session directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let session = store.try_create_session().expect("session");
        let first = persist_queued_follow_up(&store, &session.id, "first", "composer")
            .expect("first queued item");
        let second = persist_queued_follow_up(&store, &session.id, "second", "composer")
            .expect("second queued item");

        let (claimed, prepared) =
            claim_next_queued_follow_up_after_startup(&store, &session.id, |_| {
                Ok("prepared worker")
            })
            .expect("claim after startup")
            .expect("queued item");
        assert_eq!(prepared, "prepared worker");
        assert_eq!(claimed, first);
        assert_eq!(
            store
                .load_result(&session.id)
                .expect("load claimed session")
                .expect("claimed session")
                .queued_follow_ups,
            vec![second.clone()]
        );

        restore_queued_follow_up_after_start_failure(&store, &session.id, claimed.clone())
            .expect("restore failed activation");
        restore_queued_follow_up_after_start_failure(&store, &session.id, claimed.clone())
            .expect("idempotent restore");
        let restored = store
            .load_result(&session.id)
            .expect("load restored session")
            .expect("restored session");
        assert_eq!(restored.queued_follow_ups, vec![claimed, second]);
        assert_eq!(
            restored
                .events
                .iter()
                .filter(|event| event.kind == "queued_follow_up_start_committed")
                .count(),
            1
        );
        assert_eq!(
            restored
                .events
                .iter()
                .filter(|event| event.kind == "queued_follow_up_start_failed")
                .count(),
            2
        );
    }

    #[test]
    fn new_commands_are_parsed_and_runtime_commands_have_typed_effects() {
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
            "/stop exact-task",
        ] {
            parse_interactive_command(command).unwrap_or_else(|error| panic!("{command}: {error}"));
        }

        let InteractiveEffect::Output(status) = execute_interactive_command_in_state(
            InteractiveCommand::Status,
            project.path(),
            "focused",
            &store,
            &session_id,
            "running",
        )
        .expect("status") else {
            panic!("status output");
        };
        assert!(status.contains("running"));
        assert!(status.contains(&session_id));
        assert!(status.contains("profile focused"));

        let effect = execute_interactive_command(
            InteractiveCommand::Compact,
            project.path(),
            &store,
            &session_id,
        )
        .expect("compact");
        assert_eq!(effect, InteractiveEffect::Compact);

        let InteractiveEffect::Output(tasks) = execute_interactive_command(
            InteractiveCommand::Ps,
            project.path(),
            &store,
            &session_id,
        )
        .expect("ps") else {
            panic!("ps output");
        };
        assert!(tasks.contains("Session-owned background work"));
        assert!(tasks.contains("(none)"));

        let InteractiveEffect::Output(stop) = execute_interactive_command(
            InteractiveCommand::Stop { task_id: None },
            project.path(),
            &store,
            &session_id,
        )
        .expect("stop") else {
            panic!("stop output");
        };
        assert!(stop.contains("running background work"));
        assert!(stop.contains("(none)"));
        assert!(stop.contains("/stop <task-id>"));

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
    fn background_projection_is_session_scoped_bounded_and_command_free() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        save_nib_config_full(project.path(), &mut config).expect("config");
        let sessions = SessionStore::for_project(project.path()).expect("sessions");
        let session = sessions.create_session_with_id("owned-session");
        sessions.create_session_with_id("foreign-session");
        let tasks = crate::daemons::workload::DurableTaskStore::for_project(project.path())
            .expect("task store");
        for index in 0..=MAX_INTERACTIVE_BACKGROUND_TASKS {
            let id = format!("bg-{index:03}");
            tasks
                .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                    id,
                    command: format!("private-command-{index}"),
                    cwd: project.path().to_path_buf(),
                    project_root: project.path().to_path_buf(),
                    profile_id: "default".to_string(),
                    sessions_dir: sessions.sessions_dir().to_path_buf(),
                    session_id: session.id.clone(),
                    execution: crate::config::ExecutionConfig::default(),
                    timeout_secs: 10,
                    max_output_bytes: 1_024,
                })
                .expect("prepare owned task");
        }
        tasks
            .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: "foreign-background".to_string(),
                command: "foreign-private-command".to_string(),
                cwd: project.path().to_path_buf(),
                project_root: project.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: sessions.sessions_dir().to_path_buf(),
                session_id: "foreign-session".to_string(),
                execution: crate::config::ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1_024,
            })
            .expect("prepare foreign task");

        let output = format_session_background_tasks(&sessions, &session.id, false)
            .expect("background projection");
        assert_eq!(
            output
                .lines()
                .filter(|line| line.trim_start().starts_with("- bg-"))
                .count(),
            MAX_INTERACTIVE_BACKGROUND_TASKS
        );
        assert!(output.contains("1 additional tasks omitted"));
        assert!(!output.contains("foreign-background"));
        assert!(!output.contains("private-command"));
        assert!(!output.contains("worker_pid"));
    }

    #[test]
    fn background_commands_remain_bound_to_the_profile_captured_at_startup() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config.profiles.default = "profile-a".to_string();
        config.profiles.active = vec![
            crate::config::ProfileConfig {
                id: "profile-a".to_string(),
                root: Path::new(".").to_path_buf(),
                state_dir: Some(Path::new(".nib/profiles/profile-a").to_path_buf()),
                ..Default::default()
            },
            crate::config::ProfileConfig {
                id: "profile-b".to_string(),
                root: Path::new(".").to_path_buf(),
                state_dir: Some(Path::new(".nib/profiles/profile-b").to_path_buf()),
                ..Default::default()
            },
        ];
        save_nib_config_full(project.path(), &mut config).expect("initial profiles");
        let captured = resolve_interactive_profile_scope(project.path()).expect("captured profile");
        assert_eq!(captured.profile_id(), "profile-a");
        let store_a = captured.into_session_store();
        let session_id = "coincident-session";
        store_a.create_session_with_id(session_id);

        config.profiles.default = "profile-b".to_string();
        save_nib_config_full(project.path(), &mut config).expect("changed default profile");
        let store_b = SessionStore::for_project(project.path()).expect("new default profile");
        store_b.create_session_with_id(session_id);

        for (store, profile_id, task_id, command) in [
            (&store_a, "profile-a", "profile-a-task", "private-a-command"),
            (&store_b, "profile-b", "profile-b-task", "private-b-command"),
        ] {
            crate::daemons::workload::DurableTaskStore::from_sessions_dir(store.sessions_dir())
                .expect("profile task store")
                .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                    id: task_id.to_string(),
                    command: command.to_string(),
                    cwd: project.path().to_path_buf(),
                    project_root: project.path().to_path_buf(),
                    profile_id: profile_id.to_string(),
                    sessions_dir: store.sessions_dir().to_path_buf(),
                    session_id: session_id.to_string(),
                    execution: crate::config::ExecutionConfig::default(),
                    timeout_secs: 10,
                    max_output_bytes: 1_024,
                })
                .expect("prepare profile task");
        }

        let InteractiveEffect::Output(output) = execute_interactive_command(
            InteractiveCommand::Ps,
            project.path(),
            &store_a,
            session_id,
        )
        .expect("profile-bound task listing") else {
            panic!("background output");
        };
        assert!(output.contains("profile-a-task"));
        assert!(!output.contains("profile-b-task"));
        assert!(!output.contains("private-a-command"));
        assert!(!output.contains("private-b-command"));
    }

    #[test]
    fn permissions_use_instruction_tightening_and_invalid_directives_fail_closed() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        save_nib_config_full(project.path(), &mut config).expect("config");
        std::fs::write(
            project.path().join("AGENTS.md"),
            "nib-boundary: disable-network\n",
        )
        .expect("instruction boundary");

        let tightened = format_permissions(project.path()).expect("tightened posture");
        assert!(tightened.contains("Configured approval preset: manual"));
        assert!(tightened.contains("provider: bwrap"));
        assert!(tightened.contains("network: disabled"));
        assert!(tightened.contains("tightened by project instructions"));
        assert!(tightened.contains("managed-owned-worktree gate: required"));

        std::fs::write(project.path().join("AGENTS.md"), "nib-boundary: profile\n")
            .expect("invalid instruction boundary");
        let failed_closed = format_permissions(project.path()).expect("fail-closed posture");
        assert!(failed_closed.contains("provider: bwrap"));
        assert!(failed_closed.contains("network: disabled"));
        assert!(failed_closed.contains("INVALID project directive"));
        assert!(failed_closed.contains("FAILS CLOSED"));
    }

    #[test]
    fn permission_selection_recomputes_posture_and_warns_in_text_when_off() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
        save_nib_config_full(project.path(), &mut config).expect("config");

        let output = set_approval_mode(project.path(), "off").expect("select off");
        assert!(output.contains("Configured approval preset set to 'off'"));
        assert!(output.contains("Configured approval preset: off"));
        assert!(output.contains("approval behavior: off"));
        assert!(output.contains("WARNING: BROADER/OFF"));
        assert!(output.contains("never overrides them"));
    }

    #[test]
    fn status_reports_resolved_transport_bounded_context_and_control_safe_values() {
        let project = tempdir().expect("project");
        let mut config = NibConfig::default();
        config.llm.context_length = 32;
        let secret = "status/credential";
        config.llm.add_or_update_provider(
            "mock".to_string(),
            "mock\u{1b}[31m\nINJECTED-prefix-status\\/credential-c3RhdHVzL2NyZWRlbnRpYWw="
                .to_string(),
            None,
        );
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "inactive-model".to_string(),
                api_key: Some(secret.to_string()),
                ..ProviderEntry::default()
            },
        );
        save_nib_config_full(project.path(), &mut config).expect("config");
        let store = SessionStore::for_project(project.path()).expect("store");
        let session = store.try_create_session().expect("session");
        store
            .try_append_message(&session.id, "user", &"context ".repeat(200))
            .expect("persist context");
        store
            .update_session(&session.id, |session| {
                session.display_name = Some("name\u{1b}[2J\nINJECTED_NAME".to_string());
                session.tool_calls.push(crate::session::ToolCallRecord {
                    worktree_path: Some("/tmp/stale-historical-worktree".to_string()),
                    ..Default::default()
                });
                Ok(())
            })
            .expect("unsafe legacy name fixture");

        let output = format_session_status(
            project.path(),
            "status-profile",
            &store,
            &session.id,
            "idle",
        )
        .expect("status");
        assert!(output.contains("profile status-profile"), "{output}");
        assert!(output.contains("worktree -"), "{output}");
        assert!(!output.contains("stale-historical-worktree"), "{output}");
        assert!(output.contains("transport local"), "{output}");
        assert!(output.contains("context ~"), "{output}");
        assert!(output.contains("/32"), "{output}");
        assert!(output.contains("Configured approval preset: manual"));
        assert!(output.contains("platform sandbox:"));
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains("\nINJECTED"));
        assert!(!output.contains("\nINJECTED_NAME"));
        assert!(!output.contains(secret));
        assert!(!output.contains(r"status\/credential"));
        assert!(!output.contains("c3RhdHVzL2NyZWRlbnRpYWw"));
        assert!(output.contains("[REDACTED]"));
        assert!(output.len() < 4_096, "status output must remain bounded");
    }

    #[test]
    fn model_commands_rename_and_session_history_share_sensitive_projection() {
        let project = tempdir().expect("project");
        let secret = "surface/credential";
        let encoded = "c3VyZmFjZS9jcmVkZW50aWFs";
        let mut config = NibConfig::default();
        config.llm.add_or_update_provider(
            "mock".to_string(),
            format!("prefix {secret} surface\\/credential {encoded} \u{1b}[2J"),
            None,
        );
        config.llm.providers.insert(
            "inactive-openai".to_string(),
            ProviderEntry {
                model: "safe-inactive-model".to_string(),
                api_key: Some(secret.to_string()),
                ..ProviderEntry::default()
            },
        );
        save_nib_config_full(project.path(), &mut config).expect("sensitive config");
        let store = SessionStore::for_project(project.path()).expect("session store");
        let session = store.create_session();
        let unsafe_history =
            format!("raw={secret} json=surface\\/credential b64={encoded} \u{1b}[31m\u{202e}");
        store
            .try_append_message(&session.id, "user", &unsafe_history)
            .expect("legacy unsafe message");

        let InteractiveEffect::Output(providers) = execute_interactive_command(
            InteractiveCommand::Providers,
            project.path(),
            &store,
            &session.id,
        )
        .expect("provider output") else {
            panic!("provider output effect")
        };
        for forbidden in [
            secret,
            r"surface\/credential",
            encoded,
            "\u{1b}",
            "\u{202e}",
        ] {
            assert!(
                !providers.contains(forbidden),
                "provider output: {providers:?}"
            );
        }

        let confirmation =
            set_active_model(project.path(), r"surface\/credential").expect("model confirmation");
        assert!(!confirmation.contains(secret));
        assert!(!confirmation.contains(r"surface\/credential"));

        let InteractiveEffect::Output(rename) = execute_interactive_command(
            InteractiveCommand::Rename {
                name: format!("{secret} {encoded} \u{1b}[2J"),
            },
            project.path(),
            &store,
            &session.id,
        )
        .expect("rename output") else {
            panic!("rename output effect")
        };
        assert!(!rename.contains(secret));
        assert!(!rename.contains(encoded));
        assert!(!rename.contains('\u{1b}'));

        let candidate =
            interactive_session_candidate(&store, &session.id, &session.id).expect("safe preview");
        let persisted = store.load(&session.id).expect("session");
        let activities = project_session_activities(&persisted, store.public_sensitive_values());
        let public_history = format!("{}\n{activities:?}", candidate.preview);
        for forbidden in [
            secret,
            r"surface\/credential",
            encoded,
            "\u{1b}",
            "\u{202e}",
        ] {
            assert!(
                !public_history.contains(forbidden),
                "public history: {public_history:?}"
            );
        }
    }

    #[test]
    fn activity_projection_redacts_before_the_first_body_bound() {
        let directory = tempdir().expect("session directory");
        let mut session = SessionStore::at_dir(directory.path().join("sessions"))
            .try_create_session()
            .expect("session");
        let secret = format!("boundary/activity/{}", "s".repeat(256));
        let json_encoded = secret.replace('/', r"\/");
        let straddles_body_bound = |value: &str| {
            format!(
                "{}{}-safe-tail",
                "p".repeat(MAX_ACTIVITY_BODY_BYTES - value.len() / 2),
                value
            )
        };

        session.messages.push(crate::session::SessionMessage {
            index: 0,
            role: "user".to_string(),
            content: straddles_body_bound(&secret),
            timestamp: None,
            attachments: Vec::new(),
        });
        session.summary = Some(straddles_body_bound(&json_encoded));
        session.summary_index = 1;
        session.plan = Some(crate::session::Plan::new(
            "boundary projection",
            vec![crate::session::PlanStep {
                description: straddles_body_bound(&secret),
                status: "InProgress".to_string(),
                outcome: None,
                attempts: 1,
                updated_at: None,
            }],
        ));

        let sensitive_values = vec![secret.clone()];
        let projected = project_session_activities(&session, &sensitive_values);
        let mut live = Vec::new();
        let mut live_state = None;
        apply_stream_event(
            &mut live,
            StreamEvent::Content(straddles_body_bound(&json_encoded)),
            &mut live_state,
            &sensitive_values,
        );
        let public = format!("{projected:?}\n{live:?}");
        for forbidden in [
            &secret[..secret.len() / 2 - 8],
            &json_encoded[..json_encoded.len() / 2 - 8],
        ] {
            assert!(
                !public.contains(forbidden),
                "credential prefix survived redaction-before-bounding: {forbidden:?}"
            );
        }
        assert!(public.matches("[REDACTED]").count() >= 4, "{public:?}");
        assert!(projected
            .iter()
            .chain(live.iter())
            .all(|activity| activity.body.len() <= MAX_ACTIVITY_BODY_BYTES));
    }

    #[test]
    fn status_context_excludes_the_raw_summarized_prefix_and_never_exceeds_limit() {
        let project = tempdir().expect("project");
        let store = SessionStore::for_project(project.path()).expect("store");
        let session = store.try_create_session().expect("session");
        store
            .try_append_message(&session.id, "user", &"old raw prefix ".repeat(500))
            .expect("prefix");
        store
            .try_append_message(&session.id, "assistant", "current tail")
            .expect("tail");
        store
            .update_session(&session.id, |session| {
                session.summary = Some("bounded summary".to_string());
                session.summary_index = 1;
                Ok(())
            })
            .expect("summary");
        let mut persisted = store
            .load_result(&session.id)
            .expect("load")
            .expect("session");
        let before = persisted_context_usage(Some(&persisted), 32);
        persisted.messages[0].content = "different ignored prefix ".repeat(5_000);
        let after = persisted_context_usage(Some(&persisted), 32);
        assert_eq!(before, after);
        assert!(after <= 32, "{after}");
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
            attachments: Vec::new(),
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
        let projected = project_session_activities(&session, &[]);
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
            &[],
        );
        apply_stream_event(
            &mut live,
            StreamEvent::ToolStarted {
                tool_name: "read_file".to_string(),
            },
            &mut state,
            &[],
        );
        apply_stream_event(
            &mut live,
            StreamEvent::Reconciled {
                outcome: "completed".to_string(),
            },
            &mut state,
            &[],
        );
        assert_eq!(live[0].kind, ActivityKind::Assistant);
        assert_eq!(live[1].kind, ActivityKind::Tool);
        assert_eq!(live[2].kind, ActivityKind::Reconcile);
    }

    #[test]
    fn exact_run_identity_lifecycle_projects_as_typed_bounded_local_activity() {
        let run_id = "0123456789abcdef0123456789abcdef";
        let started = project_session_event(
            &SessionEvent {
                index: 0,
                kind: "run_started".to_string(),
                details: serde_json::json!({"run_id": run_id, "private": "DO_NOT_RENDER"}),
                timestamp: None,
            },
            &[],
        )
        .expect("started activity");
        assert_eq!(started.kind, ActivityKind::System);
        assert_eq!(started.title, "run started");
        assert!(started.body.is_empty());

        let cancelled = project_session_event(
            &SessionEvent {
                index: 1,
                kind: "run_terminal".to_string(),
                details: serde_json::json!({
                    "run_id": run_id,
                    "outcome": "cancelled_by_user",
                    "error": "DO_NOT_RENDER"
                }),
                timestamp: None,
            },
            &[],
        )
        .expect("terminal activity");
        assert_eq!(cancelled.kind, ActivityKind::Cancellation);
        assert_eq!(cancelled.title, "run terminal: cancelled_by_user");
        assert!(cancelled.body.is_empty());
        assert!(!started.render_line().contains(run_id));
        assert!(!cancelled.render_line().contains(run_id));
        assert!(!started.render_line().contains("DO_NOT_RENDER"));
        assert!(!cancelled.render_line().contains("DO_NOT_RENDER"));
        assert_ne!(started.title, "unclassified session event");
        assert_ne!(cancelled.title, "unclassified session event");
    }

    #[test]
    fn persisted_message_roles_are_explicit_and_legacy_safe() {
        let directory = tempdir().expect("dir");
        let mut session = SessionStore::at_dir(directory.path().join("s"))
            .try_create_session()
            .expect("session");
        for (index, (role, content)) in [
            ("user", "request"),
            ("assistant", "answer"),
            ("tool", r#"{"observations":[{"secret":"do-not-render"}]}"#),
            ("system", "local notice"),
            (
                &format!("future\n{}", "x".repeat(256)),
                "PRIVATE_LEGACY_SENTINEL",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            session.messages.push(crate::session::SessionMessage {
                index,
                role: role.to_string(),
                content: content.to_string(),
                timestamp: None,
                attachments: Vec::new(),
            });
        }

        let projected = project_session_activities(&session, &[]);
        assert_eq!(
            projected.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
            vec![
                ActivityKind::User,
                ActivityKind::Assistant,
                ActivityKind::Tool,
                ActivityKind::System,
                ActivityKind::System,
            ]
        );
        assert_eq!(projected[2].body, "1 persisted observation(s)");
        assert!(!projected[2].render_line().contains("do-not-render"));
        assert_eq!(projected[4].title, "unsupported legacy message role");
        assert_eq!(projected[4].body, "legacy message content omitted");
        assert!(!projected[4]
            .render_line()
            .contains("PRIVATE_LEGACY_SENTINEL"));
        assert!(!projected[4].title.contains('\n'));
    }

    #[test]
    fn authoritative_events_are_ordered_typed_redacted_and_tool_deduplicated() {
        let timestamp = |second: u32| {
            chrono::DateTime::parse_from_rfc3339(&format!("2026-08-23T00:00:{second:02}Z"))
                .expect("timestamp")
                .with_timezone(&Utc)
        };
        let directory = tempdir().expect("dir");
        let mut session = SessionStore::at_dir(directory.path().join("s"))
            .try_create_session()
            .expect("session");
        session.messages.push(crate::session::SessionMessage {
            index: 0,
            role: "user".to_string(),
            content: "inspect".to_string(),
            timestamp: Some(timestamp(1)),
            attachments: Vec::new(),
        });
        session.events = vec![
            SessionEvent {
                index: 0,
                kind: "compression".to_string(),
                details: serde_json::json!({
                    "before_tokens": 800,
                    "after_tokens": 300,
                    "summarized_through": 4,
                    "raw_summary": "do-not-render",
                }),
                timestamp: Some(timestamp(2)),
            },
            SessionEvent {
                index: 1,
                kind: "tool_completed".to_string(),
                details: serde_json::json!({
                    "tool_name": "read_file",
                    "success": false,
                    "error": "do-not-render",
                }),
                timestamp: Some(timestamp(3)),
            },
            SessionEvent {
                index: 2,
                kind: "reconciliation".to_string(),
                details: serde_json::json!({
                    "outcome": "llm_stream_failed",
                    "failure": {
                        "class": "transport",
                        "phase": "stream",
                        "retry": "retryable",
                        "incident_code": "LLM-NETWORK",
                        "message": "do-not-render",
                    },
                }),
                timestamp: Some(timestamp(4)),
            },
            SessionEvent {
                index: 3,
                kind: "cancel_requested".to_string(),
                details: serde_json::json!({
                    "reason": "cancelled_by_user",
                    "state": "Running",
                    "raw": "do-not-render",
                }),
                timestamp: Some(timestamp(5)),
            },
        ];
        session.tool_calls.push(crate::session::ToolCallRecord {
            tool_name: Some("read_file".to_string()),
            result: Some(serde_json::json!({"content": "do-not-render"})),
            timestamp: Some(timestamp(3)),
            ..crate::session::ToolCallRecord::default()
        });

        let projected = project_session_activities(&session, &[]);
        assert_eq!(
            projected.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
            vec![
                ActivityKind::User,
                ActivityKind::Compression,
                ActivityKind::Tool,
                ActivityKind::Failure,
                ActivityKind::Cancellation,
            ]
        );
        assert_eq!(
            projected
                .iter()
                .filter(|entry| entry.kind == ActivityKind::Tool)
                .count(),
            1
        );
        let failure = projected
            .iter()
            .find(|entry| entry.kind == ActivityKind::Failure)
            .expect("failure evidence");
        assert_eq!(
            failure.body,
            "class=transport · phase=stream · retry=retryable · incident_code=LLM-NETWORK"
        );
        assert!(projected
            .iter()
            .all(|entry| !entry.render_line().contains("do-not-render")));
    }

    #[test]
    fn legacy_role_projection_is_stable_across_json_roundtrip() {
        let legacy = r#"{
  "id": "legacy-ledger",
  "messages": [
    {"index": 0, "role": "system", "content": "legacy notice"},
    {"index": 1, "role": "future_agent_role", "content": "PRIVATE_LEGACY_SENTINEL"}
  ],
  "events": [
    {"index": 0, "kind": "legacy_event", "details": {"raw": "not projected"}}
  ]
}"#;
        let first: Session = serde_json::from_str(legacy).expect("legacy session");
        let encoded = serde_json::to_string(&first).expect("roundtrip encoding");
        let second: Session = serde_json::from_str(&encoded).expect("roundtrip session");

        let first_projection = project_session_activities(&first, &[]);
        let second_projection = project_session_activities(&second, &[]);
        assert_eq!(first_projection, second_projection);
        assert_eq!(first_projection[0].kind, ActivityKind::System);
        assert_eq!(first_projection[1].kind, ActivityKind::System);
        assert_eq!(first_projection[1].title, "unsupported legacy message role");
        assert_eq!(first_projection[1].body, "legacy message content omitted");
        assert!(!first_projection[1]
            .render_line()
            .contains("PRIVATE_LEGACY_SENTINEL"));
        assert_eq!(first_projection[2].title, "unclassified session event");
        assert!(!first_projection[2].render_line().contains("not projected"));
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

    #[test]
    fn path_attachments_are_structured_and_stay_inside_the_project() {
        let project = tempdir().expect("project");
        std::fs::create_dir_all(project.path().join("src")).expect("src");
        std::fs::write(project.path().join("src/main.rs"), "fn secret_body() {}").expect("file");
        std::fs::write(project.path().join(".secret"), "nope").expect("dotfile");
        let (text, attachments) =
            resolve_path_attachments(project.path(), "inspect @src/main.rs please")
                .expect("valid attachment");
        assert_eq!(text, "inspect @src/main.rs please");
        assert!(!text.contains("secret_body"));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, "src/main.rs");
        assert!(
            resolve_path_attachments(project.path(), "see @../etc/passwd")
                .expect_err("escape")
                .contains("outside")
        );
        assert!(resolve_path_attachments(project.path(), "see @.secret")
            .expect_err("dotfile")
            .contains("outside"));
    }
}
