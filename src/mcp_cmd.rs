use clap::{Args, Subcommand};
use std::collections::HashMap;
use std::path::Path;

use nib::config::{load_nib_config_full, update_nib_config, McpServerEntry};

#[derive(Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub command: McpCommands,
}

#[derive(Subcommand)]
pub enum McpCommands {
    /// List configured MCP servers
    List,
    /// Add a new MCP server
    Add {
        /// Name of the MCP server
        name: String,
        /// Command to execute (e.g., npx)
        command: String,
        /// Arguments for the command
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        // Note: setting env vars from CLI could be added later with --env, but keeping it simple for now
    },
    /// Remove an MCP server
    Remove {
        /// Name of the MCP server to remove
        name: String,
    },
}

pub fn run_mcp_cmd(args: &McpArgs, project_root: &Path) -> Result<(), String> {
    match &args.command {
        McpCommands::List => list_mcp_servers(project_root),
        McpCommands::Add {
            name,
            command,
            args,
        } => add_mcp_server(project_root, name, command, args),
        McpCommands::Remove { name } => remove_mcp_server(project_root, name),
    }
}

pub fn list_mcp_servers(project_root: &Path) -> Result<(), String> {
    println!("{}", format_mcp_servers(project_root)?);
    Ok(())
}

pub fn format_mcp_servers(project_root: &Path) -> Result<String, String> {
    let cfg = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    let servers = &cfg.mcp.servers;

    if servers.is_empty() {
        return Ok("No MCP servers configured.".to_string());
    }

    let mut output = String::from("Configured MCP Servers:");
    for (name, entry) in servers {
        output.push_str(&format!(
            "\n  - {}: {} {}",
            name,
            entry.command,
            entry.args.join(" ")
        ));
    }
    Ok(output)
}

pub fn add_mcp_server(
    project_root: &Path,
    name: &str,
    command: &str,
    args: &[String],
) -> Result<(), String> {
    add_mcp_server_quiet(project_root, name, command, args)?;
    println!("Successfully added MCP server '{}'.", name);
    Ok(())
}

pub fn add_mcp_server_quiet(
    project_root: &Path,
    name: &str,
    command: &str,
    args: &[String],
) -> Result<(), String> {
    if name.trim().is_empty() || command.trim().is_empty() {
        return Err("MCP server name and command must not be empty".to_string());
    }
    let entry = McpServerEntry {
        command: command.to_string(),
        args: args.to_vec(),
        env: HashMap::new(),
        ..McpServerEntry::default()
    };
    update_nib_config(project_root, |config| {
        config.mcp.servers.insert(name.to_string(), entry);
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn remove_mcp_server(project_root: &Path, name: &str) -> Result<(), String> {
    remove_mcp_server_quiet(project_root, name)?;
    println!("Successfully removed MCP server '{}'.", name);
    Ok(())
}

pub fn remove_mcp_server_quiet(project_root: &Path, name: &str) -> Result<(), String> {
    update_nib_config(project_root, |config| {
        config
            .mcp
            .servers
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| format!("MCP server '{name}' not found"))
    })
    .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn mcp_config_mutations_are_strict_and_persistent() {
        let directory = tempdir().expect("tempdir");
        let mut config = nib::config::NibConfig::default();
        nib::config::save_nib_config_full(directory.path(), &mut config).expect("default config");

        add_mcp_server(
            directory.path(),
            "fixture",
            "fixture-command",
            &["--stdio".to_string()],
        )
        .expect("add server");
        let stored = load_nib_config_full(directory.path()).expect("stored config");
        assert_eq!(stored.mcp.servers["fixture"].args, ["--stdio"]);

        remove_mcp_server(directory.path(), "fixture").expect("remove server");
        assert!(load_nib_config_full(directory.path())
            .unwrap()
            .mcp
            .servers
            .is_empty());
        assert!(remove_mcp_server(directory.path(), "missing").is_err());
    }

    #[test]
    fn corrupt_config_is_never_replaced_with_defaults() {
        let directory = tempdir().expect("tempdir");
        let paths = nib::config::config_paths(directory.path());
        std::fs::create_dir_all(&paths.nib_dir).expect("config directory");
        std::fs::write(&paths.toml, "not = [valid").expect("corrupt config");

        assert!(list_mcp_servers(directory.path()).is_err());
        assert!(add_mcp_server(directory.path(), "fixture", "command", &[]).is_err());
        assert!(remove_mcp_server(directory.path(), "fixture").is_err());
        assert_eq!(std::fs::read_to_string(paths.toml).unwrap(), "not = [valid");
    }

    #[test]
    fn command_dispatch_covers_empty_and_populated_server_lists() {
        let directory = tempdir().expect("tempdir");
        run_mcp_cmd(
            &McpArgs {
                command: McpCommands::List,
            },
            directory.path(),
        )
        .expect("list empty config");
        run_mcp_cmd(
            &McpArgs {
                command: McpCommands::Add {
                    name: "local".to_string(),
                    command: "npx".to_string(),
                    args: vec!["-y".to_string(), "fixture-server".to_string()],
                },
            },
            directory.path(),
        )
        .expect("add through dispatcher");
        run_mcp_cmd(
            &McpArgs {
                command: McpCommands::List,
            },
            directory.path(),
        )
        .expect("list populated config");
        run_mcp_cmd(
            &McpArgs {
                command: McpCommands::Remove {
                    name: "local".to_string(),
                },
            },
            directory.path(),
        )
        .expect("remove through dispatcher");
        assert!(load_nib_config_full(directory.path())
            .unwrap()
            .mcp
            .servers
            .is_empty());
    }

    #[test]
    fn add_rejects_empty_and_schema_invalid_server_fields() {
        let directory = tempdir().expect("tempdir");
        assert!(add_mcp_server(directory.path(), "", "command", &[])
            .expect_err("empty name")
            .contains("must not be empty"));
        assert!(add_mcp_server(directory.path(), "name", " ", &[])
            .expect_err("empty command")
            .contains("must not be empty"));
        assert!(add_mcp_server(directory.path(), "bad/name", "command", &[])
            .expect_err("invalid name")
            .contains("contain only ASCII"));
    }
}
