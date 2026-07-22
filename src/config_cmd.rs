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
            let mut config = nib::config::load_nib_config_full(project_root)
                .map_err(|error| error.to_string())?;
            if !show_secrets {
                redact_credentials(&mut config);
            }
            let value = toml::to_string_pretty(&config).map_err(|error| error.to_string())?;
            print!("{value}");
            Ok(())
        }
        ConfigCommands::Validate => {
            let config = nib::config::load_nib_config_full(project_root)
                .map_err(|error| error.to_string())?;
            config.validate().map_err(|error| error.to_string())?;
            println!(
                "{} is valid",
                nib::config::config_paths(project_root).toml.display()
            );
            Ok(())
        }
    }
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
    use nib::config::{McpServerEntry, ProviderEntry};
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
                base_url: None,
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
