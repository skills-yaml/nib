use nib::config::{
    config_paths, load_nib_config_full, save_nib_config_full, LlmApiMode, McpServerEntry,
    NibConfig, ProviderEntry, ReasoningEffort,
};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

fn initialize_git(path: &Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .status()
        .expect("git init");
    assert!(status.success());
}

#[test]
fn doctor_cli_returns_zero_for_a_healthy_runtime() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    initialize_git(project.path());
    let mut config = NibConfig::default();
    config.skills.enabled = false;
    save_nib_config_full(project.path(), &mut config).expect("save config");

    let output = Command::new(env!("CARGO_BIN_EXE_nib"))
        .arg("doctor")
        .current_dir(project.path())
        .env("HOME", home.path())
        .output()
        .expect("run doctor");

    assert!(
        output.status.success(),
        "doctor failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Doctor summary: Everything looks good!")
    );
}

#[test]
fn doctor_cli_returns_nonzero_for_invalid_config() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    initialize_git(project.path());
    let paths = config_paths(project.path());
    std::fs::create_dir_all(&paths.nib_dir).expect("config dir");
    std::fs::write(&paths.toml, "[agent]\nmax_turns = 0\n").expect("invalid config");

    let output = Command::new(env!("CARGO_BIN_EXE_nib"))
        .arg("doctor")
        .current_dir(project.path())
        .env("HOME", home.path())
        .output()
        .expect("run doctor");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Doctor summary: Some checks FAILED."));
}

#[test]
fn doctor_cli_initializes_configured_mcp_server() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    initialize_git(project.path());
    let mut config = NibConfig::default();
    config.skills.enabled = false;
    config.mcp.servers = HashMap::from([(
        "self".to_string(),
        McpServerEntry {
            command: env!("CARGO_BIN_EXE_nib").to_string(),
            args: vec!["mcp-server".to_string()],
            cwd: Some(project.path().to_path_buf()),
            request_timeout_secs: 5,
            ..McpServerEntry::default()
        },
    )]);
    save_nib_config_full(project.path(), &mut config).expect("save config");

    let output = Command::new(env!("CARGO_BIN_EXE_nib"))
        .arg("doctor")
        .current_dir(project.path())
        .env("HOME", home.path())
        .output()
        .expect("run doctor");

    assert!(
        output.status.success(),
        "doctor failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Protocol initialize/list OK"));
}

#[test]
fn doctor_cli_diagnoses_and_fixes_canonical_openai_chat_transport() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    initialize_git(project.path());
    let mut config = NibConfig::default();
    config.skills.enabled = false;
    config.llm.active_provider = Some("openai".to_string());
    config.llm.providers.insert(
        "openai".to_string(),
        ProviderEntry {
            model: "gpt-5.6-luna".to_string(),
            models: Some(vec!["gpt-5.6-luna".to_string()]),
            api_key: Some("doctor-cli-secret".to_string()),
            api_keys: Vec::new(),
            base_url: None,
            api: None,
            reasoning_effort: None,
        },
    );
    save_nib_config_full(project.path(), &mut config).expect("save config");

    let diagnosis = Command::new(env!("CARGO_BIN_EXE_nib"))
        .arg("doctor")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run doctor diagnosis");
    let diagnosis_stdout = String::from_utf8_lossy(&diagnosis.stdout);
    assert!(!diagnosis.status.success());
    assert!(diagnosis_stdout.contains("OpenAI agent transport is not ready"));
    assert!(diagnosis_stdout.contains("nib doctor --fix"));
    assert!(!diagnosis_stdout.contains("doctor-cli-secret"));

    let repair = Command::new(env!("CARGO_BIN_EXE_nib"))
        .args(["doctor", "--fix"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run doctor repair");
    let repair_stdout = String::from_utf8_lossy(&repair.stdout);
    assert!(
        repair.status.success(),
        "doctor repair failed:\n{}\n{}",
        repair_stdout,
        String::from_utf8_lossy(&repair.stderr)
    );
    assert!(repair_stdout.contains("FIXED (OpenAI now uses Responses)"));
    assert!(repair_stdout.contains("Implementation: openai"));
    assert!(repair_stdout.contains("Transport: responses"));
    assert!(repair_stdout.contains(
        "Adapter capabilities: complete=true, stream=true, tools=true, tool_continuation=true, parallel_tools=true, reasoning=configurable_effort, endpoint_shape=api_root_or_transport_endpoint, terminal_form=responses_status, refusal_form=responses_output_item, in_band_error_form=responses_error_event, retry_statuses=408/425/429/500/502/503/504, retry_after_statuses=429/503, credential_rotation_statuses=429"
    ));
    assert!(repair_stdout.contains("API mode: responses"));
    assert!(repair_stdout.contains("Endpoint path: /v1/responses"));
    assert!(!repair_stdout.contains("doctor-cli-secret"));

    let repaired = load_nib_config_full(project.path()).expect("load repaired config");
    let entry = &repaired.llm.providers["openai"];
    assert_eq!(entry.api, Some(LlmApiMode::Responses));
    assert_eq!(entry.model, "gpt-5.6-luna");
    assert_eq!(entry.api_key.as_deref(), Some("doctor-cli-secret"));
    assert_eq!(entry.reasoning_effort, None);
}

#[test]
fn doctor_fix_preserves_explicit_chat_without_reasoning() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    initialize_git(project.path());
    let mut config = NibConfig::default();
    config.skills.enabled = false;
    config.llm.active_provider = Some("openai".to_string());
    config.llm.providers.insert(
        "openai".to_string(),
        ProviderEntry {
            model: "chat-compatible-model".to_string(),
            api_key: Some("doctor-none-secret".to_string()),
            api: Some(LlmApiMode::ChatCompletions),
            reasoning_effort: Some(ReasoningEffort::None),
            ..ProviderEntry::default()
        },
    );
    save_nib_config_full(project.path(), &mut config).expect("save config");
    let revision = config.revision;

    let output = Command::new(env!("CARGO_BIN_EXE_nib"))
        .args(["doctor", "--fix"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run doctor no-op repair");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OK (no eligible fixes needed)"));
    assert!(!stdout.contains("doctor-none-secret"));

    let unchanged = load_nib_config_full(project.path()).expect("load unchanged config");
    assert_eq!(unchanged.revision, revision);
    assert_eq!(
        unchanged.llm.providers["openai"].api,
        Some(LlmApiMode::ChatCompletions)
    );
}
