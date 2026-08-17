//! Shared interactive command grammar and effects for chat and TUI surfaces.

use crate::config::{load_nib_config_full, update_nib_config};
use crate::llm::types::StreamEvent;
use crate::session::SessionStore;
use crate::{mcp_cmd, skill_cmd};
use std::path::Path;

pub const INTERACTIVE_HELP: &str = "Commands:\n  /model           List models for the active provider and select one\n  /model <name>    Set an exact model ID for the active provider\n  /providers       List configured providers\n  /session         Show the active session ID\n  /clear           Start a fresh session\n  /skills [cmd]    Manage skills (list, install <url_or_path>, remove <name>)\n  /mcp [cmd]       Manage MCP servers (list, add <name> <cmd> [args], remove <name>)\n  /help            Show this help\n  /quit /exit /q   Exit";

pub const INTERACTIVE_COMMAND_NAMES: &[&str] = &[
    "model",
    "providers",
    "session",
    "clear",
    "skills",
    "mcp",
    "help",
    "quit",
    "exit",
    "q",
];

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

    let parsed = match command.as_str() {
        "q" | "quit" | "exit" if arguments.is_empty() => InteractiveCommand::Quit,
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
        "" => return Err("empty slash command; use /help".to_string()),
        _ => return Err(format!("unknown command: /{command}; use /help")),
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
        InteractiveCommand::Help => Ok(InteractiveEffect::Output(INTERACTIVE_HELP.to_string())),
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
        InteractiveCommand::Session => Ok(InteractiveEffect::Output(format!(
            "Current session: {session_id}"
        ))),
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
    use tempfile::tempdir;

    #[test]
    fn shared_parser_defines_the_complete_interactive_command_vocabulary() {
        for name in INTERACTIVE_COMMAND_NAMES {
            let command = format!("/{name}");
            assert!(
                parse_interactive_command(&command).is_ok(),
                "{command} must be shared by chat and TUI"
            );
        }
        assert_eq!(parse_interactive_command("hello").unwrap(), None);
        assert!(parse_interactive_command("/unknown").is_err());
        assert!(parse_interactive_command("/skills invalid").is_err());
        assert!(parse_interactive_command("/mcp add only-name").is_err());
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
