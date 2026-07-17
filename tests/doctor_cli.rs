use nib::config::{config_paths, save_nib_config_full, McpServerEntry, NibConfig};
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
