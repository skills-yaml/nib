use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::Utc;
use nib::config::{config_paths, load_nib_config_full, NibConfig};
use nib::context::select_profile_skills;
use nib::daemons::cron::Cron;
use nib::daemons::curator::{Curator, CuratorPolicy};
use nib::integrations::mcp::McpManager;
use nib::llm::factory::provider_ready;
use nib::profile::ProfileRegistry;
use nib::sandbox;
use nib::tools::executor::ApprovalHandler;
use nib::tools::models::ApprovalDecision;
use nib::tools::{PermissionLevel, ToolCall, ToolExecutor};
use serde_json::json;

struct DoctorDenyApproval {
    called: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl ApprovalHandler for DoctorDenyApproval {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        self.called.store(true, Ordering::SeqCst);
        ApprovalDecision::denied()
    }
}

pub fn run_doctor(project: &Path) -> bool {
    println!("nib doctor");
    println!("==========");

    let mut all_passed = true;

    let paths = config_paths(project);
    let source_label = if paths.toml.exists() {
        "config.toml"
    } else if paths.json.exists() {
        "config.json (migration required)"
    } else {
        "defaults (no config file)"
    };

    // 1. Full Config Validation
    print!("Checking config... ");
    let nib_cfg = match load_nib_config_full(project) {
        Ok(config) => {
            println!("OK ({source_label})");
            let active_provider = config.llm.get_active_provider();
            println!("  Active provider: {active_provider}");
            for (name, _) in nib::config::SUPPORTED_PROVIDERS {
                let ready = provider_ready(config.llm.get_provider(Some(name)), name);
                println!(
                    "  Provider {name}: {}",
                    if ready { "ready" } else { "missing key" }
                );
            }
            if !provider_ready(
                config.llm.get_provider(Some(&active_provider)),
                &active_provider,
            ) {
                println!("  FAILED: active provider '{active_provider}' has no credentials");
                all_passed = false;
            }
            println!(
                "  Agent max turns: {} (tool enforcement: {})",
                config.agent.max_turns, config.agent.tool_use_enforcement
            );
            println!(
                "  Terminal: {} (timeout: {}s)",
                config.terminal.backend, config.terminal.timeout
            );
            println!("  Approvals: {}", config.approvals.mode);
            config
        }
        Err(e) => {
            println!("FAILED ({})", e);
            println!("  Config path: {}", paths.toml.display());
            println!("==========");
            println!("Doctor summary: Some checks FAILED.");
            return false;
        }
    };

    println!(
        "  Config path: {} (exists: {})",
        paths.toml.display(),
        paths.toml.exists()
    );

    // 2. Git / Worktree availability
    print!("Checking git/worktree... ");
    let git_available = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    let git_status = Command::new("git")
        .current_dir(project)
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .output();
    match (git_available, git_status) {
        (true, Ok(out)) if out.status.success() => println!("OK (inside git worktree)"),
        (false, _) => {
            println!("FAILED (git executable is unavailable)");
            all_passed = false;
        }
        _ => {
            println!("FAILED (project is not inside a git worktree)");
            all_passed = false;
        }
    }

    // 3. MCP servers
    print!("Checking MCP configs... ");
    if !nib_cfg.mcp.client_enabled {
        println!(
            "OK (client disabled; server exposure: {})",
            nib_cfg.mcp.server_enabled
        );
    } else if nib_cfg.mcp.servers.is_empty() {
        println!(
            "None configured (server exposure: {})",
            nib_cfg.mcp.server_enabled
        );
    } else {
        println!(
            "{} servers configured (server exposure: {})",
            nib_cfg.mcp.servers.len(),
            nib_cfg.mcp.server_enabled
        );
        let mut commands_available = true;
        for (name, server) in &nib_cfg.mcp.servers {
            match mcp_command_available(project, server) {
                Ok(()) => {
                    println!("  Server '{}' command '{}' found", name, server.command);
                }
                Err(error) => {
                    println!("  Server '{name}' FAILED: {error}");
                    all_passed = false;
                    commands_available = false;
                }
            }
        }
        if commands_available {
            match check_mcp_reachability(project, &nib_cfg.mcp.servers, &nib_cfg.sensitive_values())
            {
                Ok(tool_count) => println!("  Protocol initialize/list OK ({tool_count} tools)"),
                Err(error) => {
                    println!("  MCP protocol FAILED: {error}");
                    all_passed = false;
                }
            }
        }
    }

    // 4. Profiles and isolated stores
    print!("Checking profiles... ");
    let profile_registry = match ProfileRegistry::load(project, &nib_cfg.profiles) {
        Ok(registry) => {
            let mut profile_error = None;
            for profile in registry.all() {
                if let Err(error) = profile.ensure_state_dirs() {
                    profile_error = Some(error.to_string());
                    break;
                }
                if let Err(error) = profile.memory_store().load_result() {
                    profile_error = Some(format!(
                        "profile {} memory is invalid: {error}",
                        profile.id()
                    ));
                    break;
                }
            }
            if let Some(error) = profile_error {
                println!("FAILED ({error})");
                println!("==========");
                println!("Doctor summary: Some checks FAILED.");
                return false;
            } else {
                println!(
                    "OK ({} profiles; default: {})",
                    registry.all().count(),
                    registry.default_profile().id()
                );
                registry
            }
        }
        Err(error) => {
            println!("FAILED ({error})");
            println!("==========");
            println!("Doctor summary: Some checks FAILED.");
            return false;
        }
    };

    // 5. Skills Discoverability
    print!("Checking Skills discoverability... ");
    let mut shared_skill_paths = BTreeSet::new();
    for configured in &nib_cfg.skills.paths {
        shared_skill_paths.insert(resolve_path(project, configured));
    }
    if let Some(home) = std::env::var_os("HOME") {
        shared_skill_paths.insert(
            PathBuf::from(home)
                .join(".config")
                .join("nib")
                .join("skills"),
        );
    }
    let mut skill_paths = shared_skill_paths.clone();
    for profile in profile_registry.all() {
        skill_paths.extend(profile.skill_paths().iter().cloned());
        skill_paths.insert(profile.managed_skills_dir().to_path_buf());
    }
    let skills_found = skill_paths.iter().filter(|path| path.is_dir()).count();
    let mut missing_active_skills = Vec::new();
    let mut invalid_skills = Vec::new();
    for profile in profile_registry.all() {
        match select_profile_skills(profile.root_path(), &nib_cfg, profile, "doctor validation") {
            Ok(selected) => {
                let selected_names = selected
                    .iter()
                    .map(|skill| skill.frontmatter.name.to_ascii_lowercase())
                    .collect::<BTreeSet<_>>();
                for skill in profile.active_skills() {
                    if !selected_names.contains(&skill.to_ascii_lowercase()) {
                        missing_active_skills.push(format!("{}:{skill}", profile.id()));
                    }
                }
            }
            Err(error) => invalid_skills.push(format!("{}: {error}", profile.id())),
        }
    }
    if skills_found > 0 {
        println!("OK ({skills_found} skill directories found)");
    } else if nib_cfg.skills.enabled {
        println!("WARNING (no configured skills directories found)");
    } else {
        println!("OK (skills disabled)");
    }
    if !missing_active_skills.is_empty() {
        println!(
            "  FAILED: active skills not found: {}",
            missing_active_skills.join(", ")
        );
        all_passed = false;
    }
    if !invalid_skills.is_empty() {
        println!("  FAILED: {}", invalid_skills.join("; "));
        all_passed = false;
    }

    // 6. Session and profile-store permissions
    print!("Checking persistence layer... ");
    match nib::session::SessionStore::for_project(project) {
        Ok(session_store) => {
            let test_file = session_store.sessions_dir().join(".doctor_write_test");
            let writable = if std::fs::write(&test_file, "ok").is_ok() {
                let _ = std::fs::remove_file(&test_file);
                true
            } else {
                false
            };
            match (writable, session_store.list_result()) {
                (true, Ok(session_ids)) => {
                    println!("OK (writable)");
                    println!(
                        "  Sessions: {} in {}",
                        session_ids.len(),
                        session_store.sessions_dir().display()
                    );
                }
                (writable, listing) => {
                    println!("FAILED");
                    if !writable {
                        println!("  Cannot write to selected profile sessions directory");
                    }
                    if let Err(error) = listing {
                        println!("  Cannot enumerate selected profile sessions: {error}");
                    }
                    all_passed = false;
                }
            }
        }
        Err(error) => {
            println!("FAILED ({error})");
            all_passed = false;
        }
    }

    for profile in profile_registry.all() {
        let test_file = profile.state_dir().join(".doctor_write_test");
        if std::fs::write(&test_file, "ok").is_ok() {
            let _ = std::fs::remove_file(&test_file);
        } else {
            println!("  FAILED: profile {} state is not writable", profile.id());
            all_passed = false;
        }
    }

    // 7. Permission layer functional smoke (read-only passes, destructive asks and denies)
    print!("Checking permission layer... ");
    match check_permission_layer(&nib_cfg, &profile_registry, project) {
        Ok(()) => println!("OK (read-only allowed; destructive denial enforced)"),
        Err(error) => {
            println!("FAILED ({error})");
            all_passed = false;
        }
    }

    // 8. Maintenance daemon readiness (no cleanup is executed by doctor)
    print!("Checking maintenance daemons... ");
    match validate_daemons(&nib_cfg, &profile_registry) {
        Ok(summary) => println!("OK ({summary})"),
        Err(error) => {
            println!("FAILED ({error})");
            all_passed = false;
        }
    }

    // 9. Command shell and sandbox
    print!("Checking command shell... ");
    match sandbox::check_command_shell() {
        Ok(shell) => println!("OK ({})", shell.display()),
        Err(error) => {
            println!("FAILED ({error})");
            all_passed = false;
        }
    }

    print!("Checking sandbox... ");
    let capabilities = sandbox::detect_capabilities();
    let report = sandbox::doctor_report();
    if !capabilities.bwrap_available {
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

fn resolve_path(project: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        return configured.to_path_buf();
    }
    if let Ok(relative) = configured.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(relative);
        }
    }
    project.join(configured)
}

fn mcp_command_available(
    project: &Path,
    server: &nib::config::McpServerEntry,
) -> Result<(), String> {
    let cwd = server
        .cwd
        .as_deref()
        .map(|path| resolve_path(project, path))
        .unwrap_or_else(|| project.to_path_buf());
    if !cwd.is_dir() {
        return Err(format!(
            "configured cwd is not a directory: {}",
            cwd.display()
        ));
    }

    let command_path = Path::new(&server.command);
    if command_path.is_absolute() || command_path.components().count() > 1 {
        let command_path = if command_path.is_absolute() {
            command_path.to_path_buf()
        } else {
            cwd.join(command_path)
        };
        if command_path.is_file() {
            return Ok(());
        }
        return Err(format!(
            "command path not found: {}",
            command_path.display()
        ));
    }

    let locator = if cfg!(windows) { "where.exe" } else { "which" };
    let found = Command::new(locator)
        .current_dir(cwd)
        .envs(&server.env)
        .arg(&server.command)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if found {
        Ok(())
    } else {
        Err(format!("command '{}' not found in PATH", server.command))
    }
}

fn check_mcp_reachability(
    project: &Path,
    configured: &std::collections::HashMap<String, nib::config::McpServerEntry>,
    sensitive_values: &[String],
) -> Result<usize, String> {
    let mut servers = configured.clone();
    for server in servers.values_mut() {
        if let Some(cwd) = server.cwd.as_mut() {
            if !cwd.is_absolute() {
                *cwd = project.join(&*cwd);
            }
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to initialize async runtime: {error}"))?;
    let sensitive_values = sensitive_values.to_vec();
    runtime.block_on(async move {
        let manager = McpManager::new(&servers, &sensitive_values)
            .await
            .map_err(|error| error.to_string())?;
        manager
            .list_tools()
            .await
            .map(|tools| tools.len())
            .map_err(|error| error.to_string())
    })
}

fn check_permission_layer(
    config: &NibConfig,
    profiles: &ProfileRegistry,
    project: &Path,
) -> Result<(), String> {
    let profile = profiles
        .for_workspace(project)
        .unwrap_or_else(|| profiles.default_profile());
    let called = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(DoctorDenyApproval {
        called: called.clone(),
    });
    let mut execution = config.execution.clone();
    execution.plan_mode = false;
    let mut executor = ToolExecutor::new(profile.root_path().to_path_buf(), execution)
        .with_terminal_config(&config.terminal)
        .with_approval_handler(handler)
        .with_environment(profile.custom_env())
        .with_sensitive_values(config.sensitive_values());
    let root = profile.root_path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to initialize async runtime: {error}"))?;
    runtime.block_on(async move {
        let read = executor
            .execute(
                ToolCall {
                    tool_name: "list_directory".to_string(),
                    arguments: json!({"path": "."}),
                    session_id: None,
                    project_root: Some(root.clone()),
                },
                None,
            )
            .await;
        if !read.success {
            return Err(format!(
                "read-only permission smoke failed: {}",
                read.error.as_deref().unwrap_or("unknown error")
            ));
        }

        let destructive = executor
            .execute(
                ToolCall {
                    tool_name: "apply_patch".to_string(),
                    arguments: json!({"patch": "invalid", "dry_run": true}),
                    session_id: None,
                    project_root: Some(root),
                },
                None,
            )
            .await;
        if destructive.success
            || destructive.approval_source.as_deref() != Some("denied")
            || !called.load(Ordering::SeqCst)
        {
            return Err("destructive approval denial was not enforced".to_string());
        }
        Ok(())
    })
}

fn validate_daemons(config: &NibConfig, profiles: &ProfileRegistry) -> Result<String, String> {
    if !config.daemons.cron_enabled && !config.daemons.curator_enabled {
        return Ok("disabled".to_string());
    }

    if config.daemons.cron_enabled {
        let mut cron = Cron::new();
        cron.schedule_every("curator", config.daemons.interval_seconds, Utc::now())?;
        for profile in profiles.all() {
            Cron::at_dir(profile.daemon_dir())
                .map_err(|error| format!("profile {}: {error}", profile.id()))?;
        }
    }

    if config.daemons.curator_enabled {
        let policy = CuratorPolicy {
            allow_destructive_cleanup: config.daemons.allow_destructive_cleanup,
        };
        for profile in profiles.all() {
            Curator::at_profile_paths(
                profile.sessions_dir().to_path_buf(),
                profile.memory_path().to_path_buf(),
                profile.managed_skills_dir().to_path_buf(),
                profile.daemon_dir().to_path_buf(),
                config.daemons.retention_days,
                policy,
            )
            .validate_state()
            .map_err(|error| format!("profile {}: {error}", profile.id()))?;
        }
    }

    Ok(format!(
        "cron={}, curator={}, cleanup_authorized={}",
        config.daemons.cron_enabled,
        config.daemons.curator_enabled,
        config.daemons.allow_destructive_cleanup
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib::config::{config_paths, save_nib_config_full, NibConfig};
    use tempfile::tempdir;

    fn initialize_git(path: &Path) {
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .status()
            .expect("git init");
        assert!(status.success());
    }

    #[test]
    fn doctor_accepts_healthy_default_runtime() {
        let dir = tempdir().expect("tempdir");
        initialize_git(dir.path());
        let mut config = NibConfig::default();
        config.skills.enabled = false;
        save_nib_config_full(dir.path(), &mut config).expect("save config");

        assert!(run_doctor(dir.path()));
        assert!(dir.path().join(".nib/profiles/default/sessions").is_dir());
        assert!(!dir.path().join(".nib/sessions").exists());
        let store = nib::session::SessionStore::for_project(dir.path()).expect("session store");
        let session_ids = store.list_result().expect("doctor audit sessions");
        assert_eq!(session_ids.len(), 1);
        let session = store
            .load_result(&session_ids[0])
            .expect("load doctor audit")
            .expect("doctor audit session");
        assert_eq!(session.tool_calls.len(), 2);
        assert_eq!(
            session.tool_calls[0].tool_name.as_deref(),
            Some("list_directory")
        );
        assert_eq!(
            session.tool_calls[1].tool_name.as_deref(),
            Some("apply_patch")
        );
        assert_eq!(
            session.tool_calls[1].result.as_ref().unwrap()["approval"]["source"],
            "denied"
        );
    }

    #[test]
    fn doctor_permission_smoke_is_independent_of_configured_approval_mode() {
        for mode in ["policy", "off"] {
            let dir = tempdir().expect("tempdir");
            initialize_git(dir.path());
            let mut config = NibConfig::default();
            config.skills.enabled = false;
            config.approvals.mode = mode.to_string();
            save_nib_config_full(dir.path(), &mut config).expect("save config");

            assert!(run_doctor(dir.path()), "approval mode {mode}");
        }
    }

    #[test]
    fn doctor_rejects_missing_active_provider_credentials() {
        let dir = tempdir().expect("tempdir");
        initialize_git(dir.path());
        let mut config = NibConfig::default();
        config.skills.enabled = false;
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            nib::config::ProviderEntry {
                model: "gpt-4o".to_string(),
                api_key: None,
                api_keys: Vec::new(),
                base_url: None,
            },
        );
        save_nib_config_full(dir.path(), &mut config).expect("save config");

        if std::env::var_os("OPENAI_API_KEY").is_none() {
            assert!(!run_doctor(dir.path()));
        }
    }

    #[test]
    fn doctor_rejects_malformed_discovered_skill() {
        let dir = tempdir().expect("tempdir");
        initialize_git(dir.path());
        let mut config = NibConfig::default();
        save_nib_config_full(dir.path(), &mut config).expect("save config");
        let skill = dir.path().join(".nib/skills/broken/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).expect("skill dir");
        std::fs::write(skill, "not frontmatter").expect("bad skill");

        assert!(!run_doctor(dir.path()));
    }

    #[test]
    fn doctor_rejects_invalid_full_config() {
        let dir = tempdir().expect("tempdir");
        let paths = config_paths(dir.path());
        std::fs::create_dir_all(&paths.nib_dir).expect("config dir");
        std::fs::write(&paths.toml, "[agent]\nmax_turns = 0\n").expect("invalid config");

        assert!(!run_doctor(dir.path()));
    }

    #[test]
    fn doctor_rejects_corrupt_daemon_state() {
        let dir = tempdir().expect("tempdir");
        let mut config = NibConfig::default();
        config.skills.enabled = false;
        save_nib_config_full(dir.path(), &mut config).expect("save config");
        let pins = dir.path().join(".nib/profiles/default/daemons/pins.json");
        std::fs::create_dir_all(pins.parent().unwrap()).expect("daemon dir");
        std::fs::write(pins, "not json").expect("corrupt pins");

        assert!(!run_doctor(dir.path()));
    }

    #[test]
    fn doctor_rejects_corrupt_persisted_cron_schedule() {
        let dir = tempdir().expect("tempdir");
        let mut config = NibConfig::default();
        config.skills.enabled = false;
        save_nib_config_full(dir.path(), &mut config).expect("save config");
        let cron = dir.path().join(".nib/profiles/default/daemons/cron.json");
        std::fs::create_dir_all(cron.parent().unwrap()).expect("daemon dir");
        std::fs::write(cron, "not json").expect("corrupt cron");

        assert!(!run_doctor(dir.path()));
    }

    #[test]
    fn doctor_rejects_unreadable_session_listing() {
        let dir = tempdir().expect("tempdir");
        initialize_git(dir.path());
        let mut config = NibConfig::default();
        config.skills.enabled = false;
        save_nib_config_full(dir.path(), &mut config).expect("save config");
        let sessions = dir.path().join(".nib/profiles/default/sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        std::fs::write(sessions.join("invalid session id.json"), "{}").expect("invalid session");

        assert!(!run_doctor(dir.path()));
    }

    #[test]
    fn doctor_rejects_valid_named_corrupt_session() {
        let dir = tempdir().expect("tempdir");
        initialize_git(dir.path());
        let mut config = NibConfig::default();
        config.skills.enabled = false;
        save_nib_config_full(dir.path(), &mut config).expect("save config");
        let sessions = dir.path().join(".nib/profiles/default/sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        std::fs::write(sessions.join("corrupt-session.json"), "not json").expect("corrupt session");

        assert!(!run_doctor(dir.path()));
    }
}
