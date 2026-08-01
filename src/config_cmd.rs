use clap::{Args, Subcommand};
use std::path::Path;
use std::process::Command;

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommands,
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Open the project-local .nib/config.toml in $VISUAL or $EDITOR
    Edit,
    /// Print the resolved project-local config with credentials redacted
    Show {
        /// Include plaintext API credentials in the output
        #[arg(long)]
        show_secrets: bool,
    },
    /// Validate the project-local config without changing it
    Validate,
}

pub fn run_config_cmd(args: &ConfigArgs, project_root: &Path) -> Result<(), String> {
    match &args.command {
        ConfigCommands::Edit => edit_config(project_root),
        ConfigCommands::Show { show_secrets } => {
            let (config, source) = nib::config::load_nib_config_full_with_source(project_root)
                .map_err(|error| error.to_string())?;
            let value = render_config_with_source(config, *show_secrets, Some(source.as_str()))?;
            print!("{value}");
            Ok(())
        }
        ConfigCommands::Validate => {
            let config = nib::config::load_nib_config_full(project_root)
                .map_err(|error| error.to_string())?;
            config.validate().map_err(|error| error.to_string())?;
            nib::llm::factory::validate_provider_endpoints(&config.llm)?;
            println!(
                "{} is valid",
                nib::config::config_paths(project_root).toml.display()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
fn render_config(config: nib::config::NibConfig, show_secrets: bool) -> Result<String, String> {
    render_config_with_source(config, show_secrets, None)
}

fn render_config_with_source(
    mut config: nib::config::NibConfig,
    show_secrets: bool,
    source: Option<&str>,
) -> Result<String, String> {
    nib::llm::factory::validate_provider_endpoints(&config.llm)?;
    let diagnostics = nib::llm::factory::provider_diagnostics(&config.llm, None)?;
    let mut sensitive_values = config.sensitive_values();
    sensitive_values.extend(nib::llm::factory::provider_environment_credentials());
    if !show_secrets {
        redact_credentials(&mut config);
    }
    let mut serialized_config =
        toml::Value::try_from(&config).map_err(|error| error.to_string())?;
    if !show_secrets {
        redact_toml_string_values(&mut serialized_config, &sensitive_values, &mut Vec::new());
    }
    let serialized =
        toml::to_string_pretty(&serialized_config).map_err(|error| error.to_string())?;
    let mut output = String::from("# Effective LLM configuration\n");
    if let Some(source) = source {
        output.push_str("# Configuration source: ");
        output.push_str(source);
        output.push('\n');
    }
    for line in diagnostics.redacted_lines(&sensitive_values) {
        output.push_str("# ");
        output.push_str(&line);
        output.push('\n');
    }
    output.push_str(&serialized);
    Ok(output)
}

fn redact_toml_string_values(
    value: &mut toml::Value,
    sensitive_values: &[String],
    path: &mut Vec<String>,
) {
    match value {
        toml::Value::String(text) if !is_provider_enum_path(path) => {
            *text = nib::llm::factory::redact_sensitive_value(text, sensitive_values);
        }
        toml::Value::Array(values) => {
            for value in values {
                redact_toml_string_values(value, sensitive_values, path);
            }
        }
        toml::Value::Table(table) => {
            for (field, value) in table {
                path.push(field.clone());
                redact_toml_string_values(value, sensitive_values, path);
                path.pop();
            }
        }
        _ => {}
    }
}

fn is_provider_enum_path(path: &[String]) -> bool {
    path.len() == 4
        && path[0] == "llm"
        && path[1] == "providers"
        && matches!(path[3].as_str(), "api" | "reasoning_effort")
}

fn redact_credentials(config: &mut nib::config::NibConfig) {
    let sensitive_values = config
        .sensitive_values()
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    for provider in config.llm.providers.values_mut() {
        if provider.api_key.is_some() {
            provider.api_key = Some("<redacted>".to_string());
        }
        for key in &mut provider.api_keys {
            *key = "<redacted>".to_string();
        }
    }
    for server in config.mcp.servers.values_mut() {
        for value in server.env.values_mut() {
            if sensitive_values.contains(value) {
                *value = "<redacted>".to_string();
            }
        }
    }
}

fn edit_config(project_root: &Path) -> Result<(), String> {
    let paths = nib::config::config_paths(project_root);
    nib::config::edit_nib_config(project_root, |config_path| {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .map_err(|_| format!("set VISUAL or EDITOR, then edit {}", paths.toml.display()))?;
        let mut parts = editor.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| "VISUAL/EDITOR cannot be empty".to_string())?;
        let status = Command::new(program)
            .args(parts)
            .arg(config_path)
            .status()
            .map_err(|error| format!("failed to start editor: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("editor exited with {status}"))
        }
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib::config::{LlmApiMode, McpServerEntry, ProviderEntry, ReasoningEffort};
    use serial_test::serial;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn config_display_redacts_primary_and_rotating_keys() {
        let mut config = nib::config::NibConfig::default();
        config.llm.providers = HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                model: "model".to_string(),
                api_key: Some("primary-secret".to_string()),
                api_keys: vec!["backup-secret".to_string()],
                ..ProviderEntry::default()
            },
        )]);
        config.mcp.servers = HashMap::from([(
            "fixture".to_string(),
            McpServerEntry {
                command: "fixture".to_string(),
                env: HashMap::from([
                    ("SERVICE_TOKEN".to_string(), "mcp-secret".to_string()),
                    ("PUBLIC_VALUE".to_string(), "visible".to_string()),
                ]),
                ..McpServerEntry::default()
            },
        )]);

        redact_credentials(&mut config);

        let provider = &config.llm.providers["openai"];
        assert_eq!(provider.api_key.as_deref(), Some("<redacted>"));
        assert_eq!(provider.api_keys, ["<redacted>"]);
        assert_eq!(
            config.mcp.servers["fixture"].env["SERVICE_TOKEN"],
            "<redacted>"
        );
        assert_eq!(config.mcp.servers["fixture"].env["PUBLIC_VALUE"], "visible");
    }

    #[test]
    fn config_display_reports_effective_transport_and_remains_redacted_toml() {
        let mut config = nib::config::NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "gpt-5.6-luna".to_string(),
                api_key: Some("display-secret".to_string()),
                base_url: Some("https://api.openai.com/v1".to_string()),
                api: Some(LlmApiMode::Responses),
                reasoning_effort: Some(ReasoningEffort::High),
                ..ProviderEntry::default()
            },
        );

        let rendered = render_config(config, false).expect("redacted config display");
        assert!(rendered.starts_with("# Effective LLM configuration\n"));
        assert!(rendered.contains("# Provider: openai"));
        assert!(rendered.contains("# Model: gpt-5.6-luna"));
        assert!(rendered.contains("# API mode: responses"));
        assert!(rendered.contains("# Endpoint path: /v1/responses"));
        assert!(rendered.contains("# Reasoning effort: high"));
        assert!(rendered.contains("api_key = \"<redacted>\""));
        assert!(!rendered.contains("display-secret"));
        toml::from_str::<nib::config::NibConfig>(&rendered)
            .expect("diagnostic comments preserve valid TOML");
    }

    #[test]
    fn config_display_rejects_unsafe_inactive_provider_endpoint_without_leaking_it() {
        let mut config = nib::config::NibConfig::default();
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "model".to_string(),
                base_url: Some("https://user:inactive-secret@example.test/v1".to_string()),
                ..ProviderEntry::default()
            },
        );

        let error = render_config(config, false).expect_err("unsafe provider URL");
        assert!(error.contains("embedded credentials"), "{error}");
        assert!(!error.contains("inactive-secret"), "{error}");
    }

    #[test]
    fn config_display_redacts_credentials_reused_in_model_and_endpoint_path() {
        let mut config = nib::config::NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "model-path-secret".to_string(),
                api_key: Some("path-secret".to_string()),
                base_url: Some("https://gateway.test/path-secret/v1".to_string()),
                api: Some(LlmApiMode::Responses),
                ..ProviderEntry::default()
            },
        );

        let rendered = render_config(config, false).expect("redacted config display");
        assert!(!rendered.contains("path-secret"));
        assert!(rendered.contains("# Model: <redacted>"));
        assert!(rendered.contains("# Endpoint path: <redacted>"));
        assert!(rendered.contains("model = \"<redacted>\""));
        assert!(rendered.contains("base_url = \"<redacted>\""));
        toml::from_str::<nib::config::NibConfig>(&rendered).expect("redacted output is valid TOML");
    }

    #[test]
    fn config_display_short_credentials_do_not_rewrite_toml_keys() {
        let mut config = nib::config::NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "monkey".to_string(),
                api_key: Some("key".to_string()),
                base_url: Some("https://gateway.test/key/v1".to_string()),
                api: Some(LlmApiMode::Responses),
                ..ProviderEntry::default()
            },
        );

        let rendered = render_config(config, false).expect("redacted config display");
        assert!(rendered.contains("api_key = \"<redacted>\""));
        assert!(rendered.contains("api = \"responses\""));
        assert!(rendered.contains("base_url = \"<redacted>\""));
        assert!(!rendered.contains("api_<redacted>"));
        assert!(!rendered.contains("monkey"));
        toml::from_str::<nib::config::NibConfig>(&rendered).expect("field names remain valid TOML");
    }

    #[test]
    #[serial]
    fn config_display_redacts_environment_credentials_and_encoded_reuse() {
        let previous = std::env::var_os("OPENAI_API_KEY");
        std::env::set_var("OPENAI_API_KEY", "env/only");

        let mut config = nib::config::NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "model-env/only".to_string(),
                base_url: Some("https://gateway.test/env%2Fonly/v1".to_string()),
                api: Some(LlmApiMode::Responses),
                ..ProviderEntry::default()
            },
        );

        let rendered = render_config(config, false);
        restore_env("OPENAI_API_KEY", previous);
        let rendered = rendered.expect("redacted config display");
        assert!(!rendered.contains("env/only"));
        assert!(!rendered.contains("env%2Fonly"));
        assert!(rendered.contains("# Model: <redacted>"));
        assert!(rendered.contains("# Endpoint path: <redacted>"));
        assert!(rendered.contains("model = \"<redacted>\""));
        assert!(rendered.contains("base_url = \"<redacted>\""));
        toml::from_str::<nib::config::NibConfig>(&rendered).expect("redacted output is valid TOML");
    }

    #[test]
    #[serial]
    fn config_display_does_not_exempt_unrelated_enum_named_keys() {
        let previous = std::env::var_os("OPENAI_API_KEY");
        std::env::set_var("OPENAI_API_KEY", "env-schema-secret");

        let mut config = nib::config::NibConfig::default();
        config.mcp.servers.insert(
            "fixture".to_string(),
            McpServerEntry {
                command: "fixture".to_string(),
                env: HashMap::from([
                    ("api".to_string(), "env-schema-secret".to_string()),
                    (
                        "reasoning_effort".to_string(),
                        "env-schema-secret".to_string(),
                    ),
                ]),
                ..McpServerEntry::default()
            },
        );

        let rendered = render_config(config, false);
        restore_env("OPENAI_API_KEY", previous);
        let rendered = rendered.expect("redacted config display");
        assert!(!rendered.contains("env-schema-secret"));
        assert!(rendered.contains("api = \"<redacted>\""));
        assert!(rendered.contains("reasoning_effort = \"<redacted>\""));
    }

    #[test]
    fn command_dispatch_shows_and_validates_config_and_rejects_corruption() {
        let project = tempdir().expect("project");
        let mut config = nib::config::NibConfig::default();
        nib::config::save_nib_config_full(project.path(), &mut config).expect("default config");

        for show_secrets in [false, true] {
            run_config_cmd(
                &ConfigArgs {
                    command: ConfigCommands::Show { show_secrets },
                },
                project.path(),
            )
            .expect("show config");
        }
        run_config_cmd(
            &ConfigArgs {
                command: ConfigCommands::Validate,
            },
            project.path(),
        )
        .expect("validate config");

        let config_path = nib::config::config_paths(project.path()).toml;
        std::fs::write(&config_path, "not = [valid").expect("corrupt config");
        assert!(run_config_cmd(
            &ConfigArgs {
                command: ConfigCommands::Show {
                    show_secrets: false,
                },
            },
            project.path(),
        )
        .is_err());
        assert!(run_config_cmd(
            &ConfigArgs {
                command: ConfigCommands::Validate,
            },
            project.path(),
        )
        .is_err());
    }

    #[test]
    #[serial]
    fn edit_command_honors_editor_precedence_and_reports_editor_failures() {
        let project = tempdir().expect("project");
        let previous_visual = std::env::var_os("VISUAL");
        let previous_editor = std::env::var_os("EDITOR");
        std::env::remove_var("VISUAL");
        std::env::set_var("EDITOR", "true");

        run_config_cmd(
            &ConfigArgs {
                command: ConfigCommands::Edit,
            },
            project.path(),
        )
        .expect("successful editor");
        assert!(nib::config::config_paths(project.path()).toml.is_file());

        std::env::set_var("VISUAL", "false ignored-argument");
        let error = run_config_cmd(
            &ConfigArgs {
                command: ConfigCommands::Edit,
            },
            project.path(),
        )
        .expect_err("VISUAL failure");
        assert!(error.contains("editor exited"));

        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");
        let error = run_config_cmd(
            &ConfigArgs {
                command: ConfigCommands::Edit,
            },
            project.path(),
        )
        .expect_err("missing editor");
        assert!(error.contains("set VISUAL or EDITOR"));

        restore_env("VISUAL", previous_visual);
        restore_env("EDITOR", previous_editor);
    }
}
