use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::Utc;
use nib::config::{
    config_paths, load_nib_config_full_with_source, update_nib_config_conditionally,
    ConfigMutation, LlmApiMode, NibConfig, ProviderEntry, ReasoningEffort,
};
use nib::context::select_profile_skills;
use nib::daemons::cron::Cron;
use nib::daemons::curator::{Curator, CuratorPolicy};
use nib::integrations::mcp::McpManager;
use nib::llm::factory::{provider_diagnostics, provider_ready, validate_provider_endpoints};
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

#[derive(clap::Args, Debug, Default)]
pub struct DoctorArgs {
    /// Apply narrowly scoped, deterministic configuration repairs before validation
    #[arg(long)]
    pub fix: bool,

    /// Attest that every prior nib binary is stopped and disabled, then migrate legacy subagent locks
    #[arg(long, requires = "fix")]
    pub confirm_no_legacy_processes: bool,
}

#[cfg(test)]
pub fn run_doctor(project: &Path) -> bool {
    run_doctor_inner(project, false, false)
}

pub fn run_doctor_with_args(project: &Path, args: &DoctorArgs) -> bool {
    run_doctor_inner(project, args.fix, args.confirm_no_legacy_processes)
}

fn run_doctor_inner(project: &Path, fix: bool, confirm_no_legacy_processes: bool) -> bool {
    println!("nib doctor");
    println!("==========");
    println!("Build: {}", crate::version::version_display());
    println!("LLM runtime: native Rust");

    if fix {
        print!("Applying requested fixes... ");
        match repair_openai_transport(project) {
            Ok(true) => println!("FIXED (OpenAI now uses Responses)"),
            Ok(false) => println!("OK (no eligible fixes needed)"),
            Err(error) => {
                println!("FAILED ({error})");
                println!("==========");
                println!("Doctor summary: Some checks FAILED.");
                return false;
            }
        }
    }

    if confirm_no_legacy_processes {
        print!("Migrating offline legacy subagent locks... ");
        match nib::tools::delegation::confirm_no_legacy_subagent_processes(project) {
            Ok(artifacts) => println!(
                "FIXED ({artifacts} legacy artifacts reconciled; fixed-stripe locking active)"
            ),
            Err(error) => {
                println!("FAILED ({error})");
                println!("==========");
                println!("Doctor summary: Some checks FAILED.");
                return false;
            }
        }
    }

    let mut all_passed = true;

    let paths = config_paths(project);
    // 1. Full Config Validation
    print!("Checking config... ");
    let nib_cfg = match load_nib_config_full_with_source(project) {
        Ok((config, source)) => {
            println!("OK ({})", source.as_str());
            let active_provider = config.llm.get_active_provider();
            match active_provider_diagnostic_lines(&config) {
                Ok(lines) => {
                    println!("  Effective LLM configuration:");
                    for line in lines {
                        println!("    {line}");
                    }
                }
                Err(error) => {
                    println!("  FAILED: {error}");
                    all_passed = false;
                }
            }
            if openai_transport_repair_needed(&config) {
                println!(
                    "  FAILED: OpenAI agent transport is not ready: canonical Chat Completions with provider-default or enabled reasoning can reject nib's required function tools"
                );
                println!(
                    "  Action: Run `nib doctor --fix` to switch this provider to the Responses API, then retry in a new agent turn"
                );
                all_passed = false;
            }
            for provider in nib::llm::registry::PROVIDERS {
                let name = provider.id;
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
            match check_mcp_reachability(
                project,
                &nib_cfg.mcp.servers,
                &nib_cfg.public_session_sensitive_values(),
            ) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanonicalOpenAiBase {
    RegisteredDefault,
    Root,
    ChatCompletions,
}

fn canonical_openai_base(entry: &ProviderEntry) -> Option<CanonicalOpenAiBase> {
    let Some(configured) = entry.base_url.as_deref() else {
        return Some(CanonicalOpenAiBase::RegisteredDefault);
    };
    let parsed = reqwest::Url::parse(configured.trim()).ok()?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("api.openai.com")
        || parsed.port_or_known_default() != Some(443)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    match parsed.path().trim_end_matches('/') {
        "/v1" => Some(CanonicalOpenAiBase::Root),
        "/v1/chat/completions" => Some(CanonicalOpenAiBase::ChatCompletions),
        _ => None,
    }
}

fn openai_transport_repair_needed(config: &NibConfig) -> bool {
    if config.llm.get_active_provider() != "openai" {
        return false;
    }
    let Some(entry) = config.llm.providers.get("openai") else {
        return false;
    };
    entry.resolved_api_mode() == LlmApiMode::ChatCompletions
        && entry.reasoning_effort != Some(ReasoningEffort::None)
        && canonical_openai_base(entry).is_some()
}

fn repair_openai_transport(project: &Path) -> Result<bool, String> {
    update_nib_config_conditionally(project, |config| {
        if !openai_transport_repair_needed(config) {
            return Ok(ConfigMutation::Unchanged(false));
        }
        let entry = config
            .llm
            .providers
            .get_mut("openai")
            .ok_or_else(|| "active OpenAI provider disappeared during repair".to_string())?;
        let base = canonical_openai_base(entry)
            .ok_or_else(|| "OpenAI endpoint changed during repair".to_string())?;
        entry.api = Some(LlmApiMode::Responses);
        if base == CanonicalOpenAiBase::ChatCompletions {
            entry.base_url = Some("https://api.openai.com/v1".to_string());
        }
        Ok(ConfigMutation::Changed(true))
    })
    .map_err(|error| error.to_string())
}

fn active_provider_diagnostic_lines(config: &NibConfig) -> Result<Vec<String>, String> {
    validate_provider_endpoints(&config.llm)?;
    provider_diagnostics(&config.llm, None)
        .map(|diagnostics| diagnostics.redacted_lines(&config.sensitive_values()))
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
        .with_sensitive_values(config.public_session_sensitive_values());
    let root = profile.root_path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("failed to initialize async runtime: {error}"))?;
    runtime.block_on(async move {
        let read = executor
            .execute(
                ToolCall {
                    invocation_id: nib::tools::ToolInvocationId::new(),
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
                    invocation_id: nib::tools::ToolInvocationId::new(),
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
    use nib::config::{
        config_paths, save_nib_config_full, LlmApiMode, NibConfig, ProviderEntry, ReasoningEffort,
    };
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
                models: None,
                api_key: None,
                api_keys: Vec::new(),
                base_url: None,
                api: None,
                reasoning_effort: None,
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

    #[test]
    fn doctor_diagnostics_report_effective_openai_transport_without_credentials() {
        let mut config = NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "gpt-5.6-luna".to_string(),
                api: Some(LlmApiMode::Responses),
                reasoning_effort: Some(ReasoningEffort::Medium),
                ..ProviderEntry::default()
            },
        );

        let lines = active_provider_diagnostic_lines(&config)
            .expect("effective provider diagnostics")
            .join("\n");
        assert!(lines.contains("Provider: openai"));
        assert!(lines.contains("Model: gpt-5.6-luna"));
        assert!(lines.contains("Implementation: openai"));
        assert!(lines.contains("Transport: responses"));
        assert!(lines.contains(
            "Adapter capabilities: complete=true, stream=true, tools=true, tool_continuation=true, parallel_tools=true, reasoning=configurable_effort, endpoint_shape=api_root_or_transport_endpoint, terminal_form=responses_status, refusal_form=responses_output_item, in_band_error_form=responses_error_event, retry_statuses=408/425/429/500/502/503/504, retry_after_statuses=429/503, credential_rotation_statuses=429"
        ));
        assert!(lines.contains("API mode: responses"));
        assert!(lines.contains("Endpoint path: /v1/responses"));
        assert!(lines.contains("Reasoning effort: medium"));
        assert!(!lines.contains("api_key"));
    }

    #[test]
    fn doctor_diagnostics_warn_for_canonical_openai_chat_and_reject_unsafe_urls() {
        let mut config = NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "unknown-doctor-secret-model".to_string(),
                api_key: Some("doctor-secret".to_string()),
                api: Some(LlmApiMode::ChatCompletions),
                ..ProviderEntry::default()
            },
        );
        let lines = active_provider_diagnostic_lines(&config)
            .expect("Chat diagnostics")
            .join("\n");
        assert!(lines.contains("consider api = \"responses\""));
        assert!(!lines.contains("gpt-"));
        assert!(lines.contains("Model: <redacted>"));
        assert!(!lines.contains("doctor-secret"));

        config.llm.providers.get_mut("openai").unwrap().base_url =
            Some("https://user:doctor-secret@example.test/v1".to_string());
        let error =
            active_provider_diagnostic_lines(&config).expect_err("embedded URL credentials");
        assert!(error.contains("embedded credentials"), "{error}");
        assert!(!error.contains("doctor-secret"), "{error}");
    }

    #[test]
    fn doctor_transport_repair_predicate_is_narrow_and_model_agnostic() {
        let mut config = NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "future-model-without-family-heuristics".to_string(),
                api: None,
                reasoning_effort: None,
                ..ProviderEntry::default()
            },
        );
        assert!(openai_transport_repair_needed(&config));

        config
            .llm
            .providers
            .get_mut("openai")
            .unwrap()
            .reasoning_effort = Some(ReasoningEffort::None);
        assert!(!openai_transport_repair_needed(&config));

        let entry = config.llm.providers.get_mut("openai").unwrap();
        entry.reasoning_effort = Some(ReasoningEffort::Medium);
        entry.base_url = Some("https://gateway.example.test/v1".to_string());
        assert!(!openai_transport_repair_needed(&config));

        let entry = config.llm.providers.get_mut("openai").unwrap();
        entry.base_url = Some("https://api.openai.com/v1/chat/completions".to_string());
        entry.api = Some(LlmApiMode::Responses);
        assert!(!openai_transport_repair_needed(&config));
    }

    #[test]
    fn doctor_transport_repair_is_atomic_preserving_and_idempotent() {
        let dir = tempdir().expect("tempdir");
        let mut config = NibConfig::default();
        config.skills.enabled = false;
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "gpt-5.6-luna".to_string(),
                models: Some(vec![
                    "gpt-5.6-luna".to_string(),
                    "private-model".to_string(),
                ]),
                api_key: Some("doctor-fix-secret".to_string()),
                api_keys: vec!["rotating-doctor-fix-secret".to_string()],
                base_url: Some("https://api.openai.com/v1/chat/completions".to_string()),
                api: None,
                reasoning_effort: Some(ReasoningEffort::Medium),
            },
        );
        save_nib_config_full(dir.path(), &mut config).expect("save legacy config");
        let before_revision = config.revision;

        assert!(repair_openai_transport(dir.path()).expect("repair config"));
        let repaired = nib::config::load_nib_config_full(dir.path()).expect("repaired config");
        let entry = &repaired.llm.providers["openai"];
        assert_eq!(entry.api, Some(LlmApiMode::Responses));
        assert_eq!(entry.base_url.as_deref(), Some("https://api.openai.com/v1"));
        assert_eq!(entry.model, "gpt-5.6-luna");
        assert_eq!(
            entry.models.as_deref(),
            Some(["gpt-5.6-luna".to_string(), "private-model".to_string()].as_slice())
        );
        assert_eq!(entry.api_key.as_deref(), Some("doctor-fix-secret"));
        assert_eq!(entry.api_keys, ["rotating-doctor-fix-secret".to_string()]);
        assert_eq!(entry.reasoning_effort, Some(ReasoningEffort::Medium));
        assert_eq!(repaired.revision, before_revision + 1);

        assert!(!repair_openai_transport(dir.path()).expect("idempotent repair"));
        let unchanged = nib::config::load_nib_config_full(dir.path()).expect("unchanged config");
        assert_eq!(unchanged.revision, repaired.revision);
    }

    #[test]
    fn doctor_transport_repair_never_writes_custom_gateways() {
        let dir = tempdir().expect("tempdir");
        let mut config = NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "gateway-model".to_string(),
                api: Some(LlmApiMode::ChatCompletions),
                reasoning_effort: Some(ReasoningEffort::Medium),
                base_url: Some("https://gateway.example.test/v1".to_string()),
                ..ProviderEntry::default()
            },
        );
        save_nib_config_full(dir.path(), &mut config).expect("save custom gateway");
        let revision = config.revision;

        assert!(!repair_openai_transport(dir.path()).expect("skip custom gateway"));
        let unchanged = nib::config::load_nib_config_full(dir.path()).expect("unchanged config");
        assert_eq!(unchanged.revision, revision);
        assert_eq!(
            unchanged.llm.providers["openai"].api,
            Some(LlmApiMode::ChatCompletions)
        );
        assert_eq!(
            unchanged.llm.providers["openai"].base_url.as_deref(),
            Some("https://gateway.example.test/v1")
        );
    }
}
