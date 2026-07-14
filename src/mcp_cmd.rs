use clap::{Args, Subcommand};
use std::collections::HashMap;
use std::path::Path;

use nib::config::{load_nib_config, save_nib_config_full, McpServerEntry};

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

pub fn run_mcp_cmd(args: &McpArgs, project_root: &Path) {
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

pub fn list_mcp_servers(project_root: &Path) {
    let cfg = load_nib_config(project_root);
    let servers = &cfg.mcp.servers;

    if servers.is_empty() {
        println!("No MCP servers configured.");
        return;
    }

    println!("Configured MCP Servers:");
    for (name, entry) in servers {
        println!("  - {}: {} {}", name, entry.command, entry.args.join(" "));
    }
}

pub fn add_mcp_server(project_root: &Path, name: &str, command: &str, args: &[String]) {
    let mut cfg = load_nib_config(project_root);

    let entry = McpServerEntry {
        command: command.to_string(),
        args: args.to_vec(),
        env: HashMap::new(),
    };

    cfg.mcp.servers.insert(name.to_string(), entry);

    if let Err(e) = save_nib_config_full(project_root, &cfg) {
        eprintln!("Failed to save config: {}", e);
    } else {
        println!("Successfully added MCP server '{}'.", name);
    }
}

pub fn remove_mcp_server(project_root: &Path, name: &str) {
    let mut cfg = load_nib_config(project_root);

    if cfg.mcp.servers.remove(name).is_some() {
        if let Err(e) = save_nib_config_full(project_root, &cfg) {
            eprintln!("Failed to save config: {}", e);
        } else {
            println!("Successfully removed MCP server '{}'.", name);
        }
    } else {
        println!("MCP server '{}' not found.", name);
    }
}
