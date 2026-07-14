use std::path::{Path, PathBuf};
use std::process::Command;

use nib::config::{config_paths, load_config_with_source, load_nib_config, ConfigSource};
use nib::llm::factory::provider_ready;
use nib::sandbox;

pub fn run_doctor(project: &Path) -> bool {
    println!("nib doctor");
    println!("==========");

    let mut all_passed = true;

    // 1. Config Validation
    print!("Checking config... ");
    match load_config_with_source(project) {
        Ok((llm, source)) => {
            let label = match source {
                ConfigSource::Toml => "config.toml",
                ConfigSource::MigratedFromJson => "migrated from config.json",
                ConfigSource::Default => "defaults (no config file)",
            };
            println!("OK ({})", label);
            println!("  Active provider: {}", llm.get_active_provider());
            for (name, _) in nib::config::SUPPORTED_PROVIDERS {
                let ready = provider_ready(llm.get_provider(Some(name)), name);
                println!(
                    "  Provider {name}: {}",
                    if ready { "ready" } else { "missing key" }
                );
            }
        }
        Err(e) => {
            println!("FAILED ({})", e);
            all_passed = false;
        }
    }

    let paths = config_paths(project);
    println!(
        "  Config path: {} (exists: {})",
        paths.toml.display(),
        paths.toml.exists()
    );

    // 2. Git / Worktree availability
    print!("Checking git/worktree... ");
    let git_status = Command::new("git")
        .current_dir(project)
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .output();
    match git_status {
        Ok(out) if out.status.success() => println!("OK (inside git worktree)"),
        _ => {
            println!("WARNING (not in a git worktree or git missing)");
        }
    }

    // 3. MCP servers
    print!("Checking MCP configs... ");
    let nib_cfg = load_nib_config(project);
    if nib_cfg.mcp.servers.is_empty() {
        println!("None configured");
    } else {
        println!("{} servers configured", nib_cfg.mcp.servers.len());
        for (name, server) in &nib_cfg.mcp.servers {
            let status = Command::new("which").arg(&server.command).output();
            let found = status.map(|o| o.status.success()).unwrap_or(false);
            if found {
                println!("  Server '{}' command '{}' found", name, server.command);
            } else {
                println!(
                    "  Server '{}' command '{}' NOT FOUND in $PATH",
                    name, server.command
                );
                all_passed = false;
            }
        }
    }

    // 4. Skills Discoverability
    print!("Checking Skills discoverability... ");
    let skills_dir = project.join(".nib").join("skills");
    let global_skills_dir = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config").join("nib").join("skills"));

    let mut skills_found = 0;
    if skills_dir.exists() {
        skills_found += 1;
    }
    if let Some(ref g) = global_skills_dir {
        if g.exists() {
            skills_found += 1;
        }
    }
    if skills_found > 0 {
        println!("OK (skills directory found)");
    } else {
        println!("WARNING (no skills directories found)");
    }

    // 5. Workload DB / File permissions
    print!("Checking persistence layer... ");
    let session_store = nib::session::SessionStore::new(project);
    let test_file = session_store.sessions_dir().join(".doctor_write_test");
    if std::fs::create_dir_all(session_store.sessions_dir()).is_ok()
        && std::fs::write(&test_file, "ok").is_ok()
    {
        let _ = std::fs::remove_file(&test_file);
        println!("OK (writable)");
    } else {
        println!("FAILED (cannot write to sessions directory)");
        all_passed = false;
    }
    println!(
        "  Sessions: {} in {}",
        session_store.list().len(),
        session_store.sessions_dir().display()
    );

    // 6. Sandbox
    print!("Checking sandbox... ");
    let report = sandbox::doctor_report();
    if report.contains("FAILED") || report.contains("NOT FOUND") {
        if nib_cfg.execution.provider == "bwrap" {
            println!("FAILED");
            println!("  {}", report);
            all_passed = false;
        } else {
            println!("WARNING");
            println!("  {}", report);
        }
    } else {
        println!("OK");
        println!("  {}", report);
    }

    println!(
        "Execution provider: {} (profile: {})",
        nib_cfg.execution.provider, nib_cfg.execution.default_profile
    );

    println!("==========");
    if all_passed {
        println!("Doctor summary: Everything looks good!");
    } else {
        println!("Doctor summary: Some checks FAILED.");
    }

    all_passed
}
