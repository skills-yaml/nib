//! T043 split.

use super::*;

pub(crate) const MAX_STATUS_VALUE_BYTES: usize = 160;
pub(crate) const MAX_PUBLIC_PRESENTATION_BYTES: usize = 8 * 1024;

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
    ReadOnly,
    LiveControl,
    RequiresIdle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandEffectClass {
    Always,
    ReadOnlyInspection,
    LiveControl,
    RequiresIdle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractiveCompletionSpec {
    pub candidates: &'static [&'static str],
    pub argument_after: &'static [&'static str],
}

pub(crate) const NO_COMPLETION: InteractiveCompletionSpec = InteractiveCompletionSpec {
    candidates: &[],
    argument_after: &[],
};

// Registry rows intentionally spell out every public behavior dimension together;
// grouping these fields behind positional defaults would make parser/help drift easier.
#[allow(clippy::too_many_arguments)]
pub(crate) const fn spec(
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
        InteractiveWorkerPolicy::ReadOnly,
        NO_COMPLETION,
    ),
    spec(
        "context",
        &[],
        "/context [details]",
        "Show compact context usage or its bounded breakdown",
        InteractiveArgumentSchema::OptionalSingle,
        InteractiveMutability::ReadOnly,
        InteractiveWorkerPolicy::ReadOnly,
        InteractiveCompletionSpec {
            candidates: &["details"],
            argument_after: &[],
        },
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
        "review",
        &[],
        "/review",
        "Show changes (diff)",
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
        "Copy the latest completed assistant output",
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
        InteractiveWorkerPolicy::ReadOnly,
        NO_COMPLETION,
    ),
    spec(
        "stop",
        &[],
        "/stop [task-id]",
        "Stop exact session-owned background work",
        InteractiveArgumentSchema::OptionalSingle,
        InteractiveMutability::Runtime,
        InteractiveWorkerPolicy::LiveControl,
        NO_COMPLETION,
    ),
    spec(
        "skills",
        &[],
        "/skills [list|use <name>|clear|enable <name>|disable <name>|install <url_or_path>|remove <name>]",
        "Manage installed skills",
        InteractiveArgumentSchema::Skills,
        InteractiveMutability::Configuration,
        InteractiveWorkerPolicy::RequiresIdle,
        InteractiveCompletionSpec {
            candidates: &["list", "install", "remove", "use", "clear", "enable", "disable"],
            argument_after: &["install", "remove", "use", "enable", "disable"],
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
        InteractiveWorkerPolicy::ReadOnly,
        NO_COMPLETION,
    ),
    spec(
        "continue",
        &[],
        "/continue <plan-id>",
        "Continue an exact existing plan without retyping its goal",
        InteractiveArgumentSchema::RequiredText,
        InteractiveMutability::Runtime,
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

pub(crate) const MAX_QUEUED_FOLLOW_UPS: usize = 16;
pub(crate) const MAX_QUEUE_TEXT_BYTES: usize = 16 * 1024;
pub(crate) const MAX_DISPLAY_NAME_BYTES: usize = 128;
pub(crate) const MAX_ACTIVITY_BODY_BYTES: usize = 8 * 1024;
pub(crate) const MAX_PROJECTED_SESSION_EVENTS: usize = 200;
pub(crate) const MAX_ACTIVITY_LABEL_BYTES: usize = 192;
pub(crate) const MAX_DIFF_BYTES: usize = 32 * 1024;
pub(crate) const MAX_DIFF_REDACTION_INPUT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_INTERACTIVE_BACKGROUND_TASKS: usize = 100;
pub(crate) const STEER_HINT: &str =
    "Ctrl+S steers the exact active run. Enter queues the next turn and never steers.";

pub(crate) const MAX_INTERACTIVE_SESSION_ITEMS: usize = 100;
pub(crate) const MAX_INTERACTIVE_SESSION_PREVIEW_CHARS: usize = 2_000;
pub(crate) const MAX_INTERACTIVE_SESSION_ITEM_CHARS: usize = 500;

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
    pub label: String,
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

pub(crate) fn load_session_strict(
    store: &SessionStore,
    session_id: &str,
) -> Result<Session, String> {
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

pub(crate) fn session_last_activity(session: &Session) -> Option<chrono::DateTime<chrono::Utc>> {
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
    pub(crate) fn from_session(
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
        let label = session
            .display_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(|name| bounded_sensitive_status_value(name, sensitive_values))
            .unwrap_or_else(|| abbreviated_session_id(&session.id));
        Ok(Self {
            id: session.id.clone(),
            label,
            preview,
            is_active: session.id == active_session_id,
            snapshot_token: session_snapshot_token(session)?,
        })
    }
}

pub(crate) fn session_snapshot_token(session: &Session) -> Result<[u8; 32], String> {
    let encoded = serde_json::to_vec(session)
        .map_err(|error| format!("failed to fingerprint session {}: {error}", session.id))?;
    Ok(Sha256::digest(encoded).into())
}

pub(crate) fn bounded_session_preview(content: &str, sensitive_values: &[String]) -> String {
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

pub(crate) fn completion_insertion(spec: &InteractiveCommandSpec, subcommand: &str) -> String {
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

pub(crate) fn find_command_spec(token: &str) -> Option<&'static InteractiveCommandSpec> {
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
    Manage { action: String, name: String },
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
    Context { details: bool },
    Providers,
    Permissions { selection: Option<String> },
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
    Continue { plan_id: String },
}

impl InteractiveCommand {
    pub fn spec(&self) -> &'static InteractiveCommandSpec {
        let name = match self {
            Self::Quit => "quit",
            Self::Help => "help",
            Self::Status => "status",
            Self::Context { .. } => "context",
            Self::Providers => "providers",
            Self::Permissions { .. } => "permissions",
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
            Self::Continue { .. } => "continue",
        };
        find_command_spec(name).expect("every typed interactive command has registry metadata")
    }
}

pub fn command_effect_class(command: &InteractiveCommand) -> CommandEffectClass {
    match command {
        InteractiveCommand::Quit => CommandEffectClass::Always,
        InteractiveCommand::Help
        | InteractiveCommand::Status
        | InteractiveCommand::Context { .. }
        | InteractiveCommand::Ps => CommandEffectClass::ReadOnlyInspection,
        InteractiveCommand::Stop { task_id: Some(_) } => CommandEffectClass::LiveControl,
        _ => CommandEffectClass::RequiresIdle,
    }
}

pub fn live_stop_requires_id_message() -> &'static str {
    "usage: /stop <task-id> — live stop needs an exact session-owned task id"
}

pub fn modal_command_unsupported_message() -> &'static str {
    "typed :command is unsupported here; use text: to answer literally"
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
    ContinuePlan {
        plan_id: String,
        goal: String,
    },
    OpenQuestion {
        invocation_id: String,
        question: String,
        proposed_answer: Option<String>,
        options: Vec<String>,
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
    pub(crate) profile_id: String,
    pub(crate) session_store: SessionStore,
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
    JumpToStart,
    JumpToEnd,
    Lines(i32),
}

/// Presentation-only transcript viewport measured in the same rendered rows used
/// by the TUI. It never owns or mutates session content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptViewport {
    pub(crate) top_row: usize,
    pub(crate) total_rows: usize,
    pub(crate) page_rows: usize,
    pub(crate) pinned_to_tail: bool,
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
                self.scroll_lines(-i32::try_from(self.page_rows).unwrap_or(i32::MAX));
            }
            TranscriptViewportAction::PageDown => {
                self.scroll_lines(i32::try_from(self.page_rows).unwrap_or(i32::MAX));
            }
            TranscriptViewportAction::JumpToStart => {
                self.pinned_to_tail = false;
                self.top_row = 0;
            }
            TranscriptViewportAction::JumpToEnd => self.pin_to_tail(),
            TranscriptViewportAction::Lines(delta) => self.scroll_lines(delta),
        }
    }

    pub(crate) fn scroll_lines(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        if delta < 0 {
            let current = if self.pinned_to_tail {
                self.bottom_row()
            } else {
                self.top_row
            };
            let step = usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX);
            let next = current.saturating_sub(step);
            if next < current {
                self.top_row = next;
                self.pinned_to_tail = false;
            }
            return;
        }
        if self.pinned_to_tail {
            return;
        }
        let step = usize::try_from(delta).unwrap_or(usize::MAX);
        self.top_row = self.top_row.saturating_add(step).min(self.bottom_row());
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

    pub fn page_rows(&self) -> usize {
        self.page_rows
    }

    pub fn reveal_row(&mut self, row: usize) {
        self.pinned_to_tail = false;
        self.top_row = row.min(self.bottom_row());
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

    pub(crate) fn bottom_row(&self) -> usize {
        self.total_rows.saturating_sub(self.page_rows)
    }
}

pub const MAX_DRAFT_HISTORY: usize = 50;
pub const MAX_DRAFT_HISTORY_QUERY_BYTES: usize = 256;
pub const MAX_DRAFT_HISTORY_RESULTS: usize = 20;
pub(crate) const MAX_DRAFT_HISTORY_ENTRY_BYTES: usize = 16 * 1024;
pub(crate) const MAX_DRAFT_HISTORY_DISPLAY_BYTES: usize = 512;

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
    pub(crate) entries: Vec<String>,
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

pub(crate) fn unicode_prefix(value: &str, maximum_bytes: usize) -> &str {
    if value.len() <= maximum_bytes {
        return value;
    }
    let mut end = maximum_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub(crate) fn normalize_history_query(query: &str) -> (String, bool, bool) {
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

pub(crate) fn safe_history_display(entry: &str) -> String {
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
    ApprovalInputClosed,
    QuestionAnswer {
        answer: &'a str,
        options: &'a [String],
        selected_option: Option<usize>,
    },
    QuestionLeaveUnanswered,
    QuestionInputClosed,
    ConfirmationAnswer(&'a str),
    ConfirmationInputClosed,
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
    ModalCommand(InteractiveCommand),
    ApprovalDecision(InteractionDecision),
    ApprovalInputClosed,
    QuestionAnswered(String),
    QuestionLeftUnanswered,
    QuestionInputClosed,
    ConfirmationDecision(InteractionDecision),
    ConfirmationInputClosed,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposedQuestionInput {
    Approve,
    Reject,
    InstructOtherwise,
    Answer(String),
    Retry(String),
}

pub fn parse_proposed_question_input(input: &str) -> ProposedQuestionInput {
    let trimmed = input.trim();
    if let Some(answer) = trimmed.strip_prefix("otherwise: ") {
        return if answer.trim().is_empty() {
            ProposedQuestionInput::Retry("alternative answer cannot be empty".to_string())
        } else {
            ProposedQuestionInput::Answer(answer.to_string())
        };
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "1" | "approve" | "yes" => ProposedQuestionInput::Approve,
        "2" | "reject" | "no" => ProposedQuestionInput::Reject,
        "3" | "instruct otherwise" | "otherwise" => ProposedQuestionInput::InstructOtherwise,
        "" => ProposedQuestionInput::Retry(
            "choose Approve, Reject, or Instruct otherwise".to_string(),
        ),
        _ => match parse_prompt_prefix(input) {
            Ok(Some(PromptPrefix::Text(answer))) => ProposedQuestionInput::Answer(answer),
            Ok(Some(PromptPrefix::Command(_))) => ProposedQuestionInput::Retry(
                "use the prompt-local command entry or answer with text: <answer>".to_string(),
            ),
            Err(message) => ProposedQuestionInput::Retry(message),
            Ok(None) => ProposedQuestionInput::Answer(input.to_string()),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionTerminalOutcome {
    Completed,
    Cancelled,
    WaitingForInput,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalOutcomeMessage {
    pub title: &'static str,
    pub detail: &'static str,
}

pub fn terminal_outcome_message(outcome: &str) -> TerminalOutcomeMessage {
    let (title, detail) = match outcome {
        "completed" => ("Run completed", "The result is ready. You can send another request."),
        "plan_ready" => ("Plan ready", "The plan was recorded; no plan actions were run."),
        "step_completed" => ("Plan step completed", "The next plan step is starting."),
        "context_compacted" => ("Context compacted", "Older context was summarized; raw session history remains saved."),
        "context_unchanged" => ("Context unchanged", "No compression was needed or available. The session remains ready."),
        "cancelled_by_user" => ("Run cancelled", "Active work stopped after reconciliation. Check /status before continuing."),
        "waiting_for_user_input" | "unresolved_clarification" => ("Waiting for your answer", "Dependent work is paused. Answer here or say resume to reopen the saved form."),
        "model_refusal" => ("Model declined the request", "No further work ran. Rephrase the request or select a different model."),
        "empty_model_response" => ("Model returned no answer", "No result was produced. Retry the request or inspect /status."),
        "tool_execution_failed" => ("Tool failed", "The current work is incomplete. Inspect the tool result and give corrected instructions."),
        "repeated_tool_failure" => ("Repeated tool failure", "The run stopped after unchanged failures. Inspect the last tool result before retrying."),
        "blocked_step_unresolved" => ("Plan step blocked", "The step could not be verified as complete. Inspect /status and resolve its blocker."),
        "required_verification_unresolved" => ("Verification incomplete", "Required evidence is missing, failed, or stale. Inspect /status and run or repair the exact check."),
        "turn_limit_reached" | "transition_limit_reached" => ("Run limit reached", "Work may be incomplete. Inspect /status before requesting more work."),
        "instruction_context_missing" => ("Project instructions unavailable", "Required instructions could not be loaded. Restore them, then retry the same plan."),
        "tool_scope_required" => ("Terminal scope required", "Declare a non-empty affected_paths array of worktree-relative paths for this terminal command, then retry the same plan."),
        "tool_scope_outside_worktree" => ("Tool path outside project", "A proposed tool targeted a path outside the active worktree. Choose a project path and retry."),
        "planning_required_active_plan" => ("Existing plan is still open", "This request needs planning. Finish or resolve the current plan, or start a new session."),
        "planning_required_active_run" => ("Run is still active", "Wait for reconciliation or cancel the active run before starting another request."),
        "plan_binding_changed" => ("Plan changed during the run", "No further work was admitted. Inspect /status before continuing."),
        "plan_approval_denied" => ("Plan approval declined", "No plan actions were run. Revise the request or start a new plan."),
        "local_error" => (
            "Run stopped",
            "The session was saved. Inspect /status, then retry or run nib doctor.",
        ),
        "worktree_preparation_failed" => (
            "Worktree preparation failed",
            "No proposed tools ran. Inspect Git worktree health and run nib doctor before retrying.",
        ),
        "unresponsive_worker_shutdown" => (
            "Run did not stop in time",
            "The session was saved. Inspect /status before sending more work.",
        ),
        _ if outcome.starts_with("planning_failed")
            || outcome.starts_with("llm_stream_failed")
            || outcome.starts_with("answer_only_failed")
            || outcome.starts_with("configuration_failed")
            || outcome.starts_with("invalid_tool_stream")
            || outcome.starts_with("provider_continuation_failed") => (
                "Model request failed",
                "The session was saved. Follow the incident report above or run nib doctor.",
            ),
        _ => ("Run stopped", "Inspect /status and the saved session before trying again."),
    };
    TerminalOutcomeMessage { title, detail }
}

pub fn user_visible_stop_report(outcome: &str, session_id: &str) -> String {
    let message = terminal_outcome_message(outcome);
    format!(
        "{}. {}\nSession: {session_id}",
        message.title, message.detail
    )
}

pub fn stream_end_status_line(outcome: &str) -> String {
    let message = terminal_outcome_message(outcome);
    format!("[stream ended] {}. {}", message.title, message.detail)
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
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
        InteractionInput::Transcript(action) => InteractionReduction::Transcript(action),
        InteractionInput::OpenHistorySearch => InteractionReduction::Consumed(consumer),
        InteractionInput::UserAction => InteractionReduction::Consumed(consumer),
        InteractionInput::ApprovalAnswer(_) | InteractionInput::ApprovalInputClosed
            if consumer != InteractionConsumer::Approval =>
        {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::ApprovalAnswer(answer) => reduce_choice_answer(
            answer,
            InteractionConsumer::Approval,
            InteractionReduction::ApprovalDecision,
        ),
        InteractionInput::ApprovalInputClosed => InteractionReduction::ApprovalInputClosed,
        InteractionInput::QuestionAnswer { .. }
        | InteractionInput::QuestionLeaveUnanswered
        | InteractionInput::QuestionInputClosed
            if consumer != InteractionConsumer::Question =>
        {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::QuestionLeaveUnanswered => InteractionReduction::QuestionLeftUnanswered,
        InteractionInput::QuestionInputClosed => InteractionReduction::QuestionInputClosed,
        InteractionInput::QuestionAnswer {
            answer,
            options,
            selected_option,
        } => reduce_question_answer(answer, options, selected_option),
        InteractionInput::ConfirmationAnswer(_) | InteractionInput::ConfirmationInputClosed
            if consumer != InteractionConsumer::DestructiveConfirmation =>
        {
            InteractionReduction::Consumed(consumer)
        }
        InteractionInput::ConfirmationAnswer(answer) => reduce_choice_answer(
            answer,
            InteractionConsumer::DestructiveConfirmation,
            InteractionReduction::ConfirmationDecision,
        ),
        InteractionInput::ConfirmationInputClosed => InteractionReduction::ConfirmationInputClosed,
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

pub(crate) fn reduce_choice_answer(
    answer: &str,
    consumer: InteractionConsumer,
    decided: fn(InteractionDecision) -> InteractionReduction,
) -> InteractionReduction {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "n" | "no" => decided(InteractionDecision::Reject),
        "y" | "yes" => decided(InteractionDecision::Accept),
        _ => InteractionReduction::Error {
            consumer,
            message: "enter y/yes to confirm or n/no to deny".to_string(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptPrefix {
    Text(String),
    Command(String),
}

pub(crate) fn parse_prompt_prefix(input: &str) -> Result<Option<PromptPrefix>, String> {
    let trimmed = input.trim_start_matches([' ', '\t']);
    if let Some(rest) = trimmed.strip_prefix("text:") {
        return parse_prefixed_payload("text:", rest)
            .map(|payload| Some(PromptPrefix::Text(payload)));
    }
    if let Some(rest) = trimmed.strip_prefix(":command") {
        return parse_prefixed_payload(":command", rest)
            .map(|payload| Some(PromptPrefix::Command(payload)));
    }
    Ok(None)
}

pub(crate) fn parse_prefixed_payload(token: &str, rest: &str) -> Result<String, String> {
    if rest.is_empty() || !rest.starts_with(' ') {
        return Err(format!(
            "{token} requires an ASCII space and a non-empty payload"
        ));
    }
    let payload = &rest[1..];
    if payload.trim().is_empty() {
        return Err(format!("{token} payload cannot be empty"));
    }
    Ok(payload.to_string())
}

pub(crate) fn reduce_question_answer(
    answer: &str,
    options: &[String],
    selected_option: Option<usize>,
) -> InteractionReduction {
    match parse_prompt_prefix(answer) {
        Err(message) => InteractionReduction::Error {
            consumer: InteractionConsumer::Question,
            message,
        },
        Ok(Some(PromptPrefix::Text(value))) => {
            reduce_literal_question_answer(&value, options, selected_option)
        }
        Ok(Some(PromptPrefix::Command(payload))) => reduce_modal_question_command(&payload),
        Ok(None) => reduce_question_body(answer.trim(), options, selected_option),
    }
}

pub(crate) fn reduce_literal_question_answer(
    value: &str,
    options: &[String],
    selected_option: Option<usize>,
) -> InteractionReduction {
    if value.trim().is_empty() {
        reduce_empty_question_answer(options, selected_option)
    } else {
        InteractionReduction::QuestionAnswered(value.to_string())
    }
}

pub(crate) fn reduce_question_body(
    answer: &str,
    options: &[String],
    selected_option: Option<usize>,
) -> InteractionReduction {
    if answer.is_empty() {
        return reduce_empty_question_answer(options, selected_option);
    }
    if options.is_empty() {
        return InteractionReduction::QuestionAnswered(answer.to_string());
    }
    if !answer.is_empty() && answer.bytes().all(|byte| byte.is_ascii_digit()) {
        return match answer.parse::<usize>() {
            Ok(index) => options
                .get(index.saturating_sub(1))
                .filter(|_| index > 0)
                .cloned()
                .map(InteractionReduction::QuestionAnswered)
                .unwrap_or_else(|| InteractionReduction::Error {
                    consumer: InteractionConsumer::Question,
                    message: format!("question option {index} is out of range"),
                }),
            Err(_) => InteractionReduction::Error {
                consumer: InteractionConsumer::Question,
                message: format!("question option {answer} is out of range"),
            },
        };
    }
    InteractionReduction::QuestionAnswered(answer.to_string())
}

pub(crate) fn reduce_empty_question_answer(
    options: &[String],
    selected_option: Option<usize>,
) -> InteractionReduction {
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

pub(crate) fn reduce_modal_question_command(payload: &str) -> InteractionReduction {
    match parse_interactive_command(payload.trim()) {
        Err(message) => InteractionReduction::Error {
            consumer: InteractionConsumer::Question,
            message,
        },
        Ok(None) => InteractionReduction::Error {
            consumer: InteractionConsumer::Question,
            message: "typed :command requires a slash command".to_string(),
        },
        Ok(Some(InteractiveCommand::Stop { task_id: None })) => InteractionReduction::Error {
            consumer: InteractionConsumer::Question,
            message: live_stop_requires_id_message().to_string(),
        },
        Ok(Some(command)) => match command_effect_class(&command) {
            CommandEffectClass::ReadOnlyInspection | CommandEffectClass::LiveControl => {
                InteractionReduction::ModalCommand(command)
            }
            _ => InteractionReduction::Error {
                consumer: InteractionConsumer::Question,
                message: format!(
                    "command /{} is unavailable while a question is pending",
                    command.spec().name
                ),
            },
        },
    }
}

pub(crate) fn reduce_composer_submission(
    state: &InteractionState,
    line: &str,
) -> InteractionReduction {
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
            Some(InteractiveCommand::Stop { task_id: None }) => InteractionReduction::Error {
                consumer: InteractionConsumer::Composer,
                message: live_stop_requires_id_message().to_string(),
            },
            Some(command) if command_effect_class(&command) == CommandEffectClass::RequiresIdle => {
                InteractionReduction::Error {
                    consumer: InteractionConsumer::Composer,
                    message: format!(
                        "Command /{} is unavailable while the agent is running; cancel it or wait.",
                        command.spec().name
                    ),
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
