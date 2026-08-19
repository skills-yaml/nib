//! Shared interactive command grammar and effects for chat and TUI surfaces.

use crate::config::{load_nib_config_full, update_nib_config};
use crate::llm::types::StreamEvent;
use crate::session::{Session, SessionStore};
use crate::{mcp_cmd, skill_cmd};
use sha2::{Digest, Sha256};
use std::path::Path;

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
}

pub const INTERACTIVE_COMMANDS: &[InteractiveCommandSpec] = &[
    InteractiveCommandSpec {
        name: "model",
        aliases: &[],
        usage: "/model [name]",
        summary: "List models or select an exact model ID",
        fixed_subcommands: &[],
    },
    InteractiveCommandSpec {
        name: "providers",
        aliases: &[],
        usage: "/providers",
        summary: "List configured providers",
        fixed_subcommands: &[],
    },
    InteractiveCommandSpec {
        name: "session",
        aliases: &[],
        usage: "/session",
        summary: "Show or switch the active session",
        fixed_subcommands: &[],
    },
    InteractiveCommandSpec {
        name: "clear",
        aliases: &[],
        usage: "/clear",
        summary: "Start a fresh session",
        fixed_subcommands: &[],
    },
    InteractiveCommandSpec {
        name: "skills",
        aliases: &[],
        usage: "/skills [list|install <url_or_path>|remove <name>]",
        summary: "Manage installed skills",
        fixed_subcommands: &["list", "install", "remove"],
    },
    InteractiveCommandSpec {
        name: "mcp",
        aliases: &[],
        usage: "/mcp [list|add <name> <command> [args...]|remove <name>]",
        summary: "Manage MCP servers",
        fixed_subcommands: &["list", "add", "remove"],
    },
    InteractiveCommandSpec {
        name: "help",
        aliases: &[],
        usage: "/help",
        summary: "Show interactive command help",
        fixed_subcommands: &[],
    },
    InteractiveCommandSpec {
        name: "quit",
        aliases: &["exit", "q"],
        usage: "/quit (aliases: /exit, /q)",
        summary: "Exit the interactive session",
        fixed_subcommands: &[],
    },
];

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
    }
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
    Providers,
    Session,
    Clear,
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
        "providers" if arguments.is_empty() => InteractiveCommand::Providers,
        "session" if arguments.is_empty() => InteractiveCommand::Session,
        "clear" if arguments.is_empty() => InteractiveCommand::Clear,
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
    match command {
        InteractiveCommand::Quit => Ok(InteractiveEffect::Quit),
        InteractiveCommand::Help => Ok(InteractiveEffect::Output(interactive_help())),
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
        InteractiveCommand::Session => Ok(InteractiveEffect::SelectSession(
            interactive_session_selection(store, session_id)?,
        )),
        InteractiveCommand::Clear => {
            let new_session = store
                .try_create_session()
                .map_err(|error| format!("failed to create session: {error}"))?;
            Ok(InteractiveEffect::SessionChanged {
                output: format!("Started fresh session {}.", new_session.id),
                session_id: new_session.id,
            })
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
}
