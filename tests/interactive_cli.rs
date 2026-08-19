use nib::config::{save_nib_config_full, NibConfig};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::{tempdir, TempDir};

fn configured_project() -> TempDir {
    let project = tempdir().expect("project fixture");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    config.skills.enabled = false;
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    save_nib_config_full(project.path(), &mut config).expect("mock config");
    project
}

fn run_with_input(project: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nib"))
        .args(args)
        .env("NIB_NO_UPDATE_CHECK", "1")
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nib");
    child
        .stdin
        .take()
        .expect("nib stdin")
        .write_all(input)
        .expect("write nib input");
    child.wait_with_output().expect("wait for nib")
}

fn session_count(project: &Path) -> usize {
    let sessions = project.join(".nib/profiles/default/sessions");
    match std::fs::read_dir(sessions) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read session directory: {error}"),
    }
}

#[test]
fn root_and_chat_use_the_plain_renderer_when_stdout_is_not_a_terminal() {
    for args in [&[][..], &["chat"][..], &["chat", "--plain"][..]] {
        let project = configured_project();
        let output = run_with_input(project.path(), args, b"/quit\n");
        assert!(
            output.status.success(),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
        assert!(stdout.contains("mode: plain"), "args={args:?}: {stdout}");
        assert_eq!(session_count(project.path()), 1, "args={args:?}");
    }
}

#[test]
fn forced_tui_rejects_redirected_streams_before_session_mutation() {
    for args in [&["--tui"][..], &["chat", "--tui"][..], &["tui"][..]] {
        let project = configured_project();
        let output = run_with_input(project.path(), args, b"");
        assert!(!output.status.success(), "args={args:?}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            stderr.contains("requires terminal input and output"),
            "args={args:?}: {stderr}"
        );
        assert!(stderr.contains("use --plain instead"), "{stderr}");
        if args == &["tui"] {
            assert!(stderr.contains("compatibility alias"), "{stderr}");
        }
        assert_eq!(session_count(project.path()), 0, "args={args:?}");
    }
}

#[test]
fn help_and_one_shot_run_keep_their_non_interactive_contracts() {
    let help_project = configured_project();
    let help = run_with_input(help_project.path(), &["--help"], b"");
    assert!(help.status.success());
    assert_eq!(session_count(help_project.path()), 0);
    let help_stdout = String::from_utf8(help.stdout).expect("UTF-8 help");
    assert!(help_stdout.contains("--plain"));
    assert!(help_stdout.contains("--tui"));

    let run_project = configured_project();
    let run = run_with_input(
        run_project.path(),
        &[
            "run",
            "finish the release smoke",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--max-steps",
            "4",
            "--yes",
        ],
        b"",
    );
    assert!(
        run.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&run.stderr)
    );
    let run_stdout = String::from_utf8(run.stdout).expect("UTF-8 run output");
    assert!(run_stdout.contains("nib run: finish the release smoke"));
    assert!(run_stdout.contains("Agent run completed for session"));
    assert!(!run_stdout.contains("mode: plain"));
    assert_eq!(session_count(run_project.path()), 1);
}
