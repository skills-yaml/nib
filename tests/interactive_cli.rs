use nib::config::{save_nib_config_full, NibConfig};
use nib::session::SessionStore;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
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
        if args == ["tui"] {
            assert!(stderr.contains("compatibility alias"), "{stderr}");
        }
        assert_eq!(session_count(project.path()), 0, "args={args:?}");
    }
}

#[test]
fn plain_help_lists_ft019_commands_and_incomplete_slash_is_not_a_goal() {
    let project = configured_project();
    let output = run_with_input(project.path(), &["--plain"], b"/help\n/quit\n");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("mode: plain"), "{stdout}");
    for command in [
        "/status",
        "/model",
        "/permissions",
        "/plan",
        "/review",
        "/diff",
        "/compact",
        "/session",
        "/clear",
        "/new",
        "/resume",
        "/fork",
        "/rename",
        "/copy",
        "/history",
        "/ps",
        "/stop",
        "/providers",
        "/skills",
        "/mcp",
        "/help",
        "/quit",
    ] {
        assert!(stdout.contains(command), "missing {command} in {stdout}");
    }
    assert!(stdout.contains("/stop [task-id]"), "{stdout}");
    assert!(
        !stdout.contains("explicit compact waits on T003"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("process listing waits on FT-017"),
        "{stdout}"
    );
    assert_eq!(session_count(project.path()), 1);

    let incomplete = configured_project();
    let incomplete_out = run_with_input(incomplete.path(), &["--plain"], b"/statu\n\n/quit\n");
    assert!(incomplete_out.status.success());
    let incomplete_stdout = String::from_utf8(incomplete_out.stdout).expect("UTF-8");
    assert!(
        incomplete_stdout.contains("unknown command")
            || incomplete_stdout.contains("Command completions"),
        "{incomplete_stdout}"
    );
    assert!(!incomplete_stdout.contains("Thinking..."));
}

#[test]
fn plain_status_reports_session_identity_and_queue() {
    let project = configured_project();
    let output = run_with_input(project.path(), &["--plain"], b"/status\n/quit\n");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(
        stdout.contains("/status") || stdout.contains("sess "),
        "{stdout}"
    );
    assert!(stdout.contains("queue"), "{stdout}");
}

#[test]
fn plain_compact_and_background_commands_use_session_scoped_runtime_effects() {
    let project = configured_project();
    let output = run_with_input(
        project.path(),
        &["--plain"],
        b"/ps\n/stop\n/compact\n/quit\n",
    );
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("Session-owned background work"), "{stdout}");
    assert!(
        stdout.contains("Session-owned running background work"),
        "{stdout}"
    );
    assert!(stdout.contains("context_unchanged"), "{stdout}");
    assert!(!stdout.contains("waits on T003"), "{stdout}");
    assert!(!stdout.contains("waits on FT-017"), "{stdout}");

    let store = SessionStore::for_project(project.path()).expect("session store");
    let ids = store.list_result().expect("session IDs");
    assert_eq!(ids.len(), 1);
    let session = store.load_result(&ids[0]).unwrap().unwrap();
    assert!(
        session.messages.is_empty(),
        "compact must not synthesize chat"
    );
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "compression_requested")
            .count(),
        1
    );
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "run_started")
            .count(),
        1
    );
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.kind == "run_terminal")
            .count(),
        1
    );
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
    assert!(run_stdout.contains("nib run: starting"));
    assert!(!run_stdout.contains("finish the release smoke"));
    assert!(run_stdout.contains("Agent run completed for session"));
    assert_eq!(
        run_stdout.matches("Final answer: task complete").count(),
        1,
        "one-shot final output must not be duplicated: {run_stdout}"
    );
    assert!(!run_stdout.contains("mode: plain"));
    assert_eq!(session_count(run_project.path()), 1);
}

#[test]
fn one_shot_run_preserves_long_structured_final_output() {
    let project = configured_project();
    let run = run_with_input(
        project.path(),
        &[
            "run",
            "one-shot long structured final",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--max-steps",
            "6",
            "--yes",
        ],
        b"",
    );
    assert!(run.status.success(), "{run:?}");
    let stdout = String::from_utf8(run.stdout).expect("UTF-8 output");
    assert!(stdout.contains("# Verified result\n\n"), "{stdout}");
    assert!(
        stdout.contains("```text\nLONG_FINAL_SENTINEL\n```"),
        "{stdout}"
    );
    assert!(stdout.len() > 512, "{stdout}");
    assert_eq!(stdout.matches("LONG_FINAL_SENTINEL").count(), 1, "{stdout}");
}

#[test]
fn one_shot_plan_mode_prints_ordered_plan_without_execution() {
    let project = configured_project();
    let run = run_with_input(
        project.path(),
        &[
            "run",
            "plan the release fixture",
            "--mode",
            "plan",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--max-steps",
            "4",
        ],
        b"",
    );
    assert!(run.status.success(), "{run:?}");
    let stdout = String::from_utf8(run.stdout).expect("UTF-8 plan output");
    assert!(stdout.contains("Plan plan-"), "{stdout}");
    assert!(stdout.contains("  1. explore"), "{stdout}");
    assert!(stdout.contains("  2. finish"), "{stdout}");

    let store = SessionStore::for_project(project.path()).expect("session store");
    let session_id = store
        .list_result()
        .expect("sessions")
        .into_iter()
        .next()
        .expect("plan session");
    let session = store.load(&session_id).expect("persisted plan session");
    assert!(session.tool_calls.iter().all(|call| {
        call.tool_name.as_deref() != Some("list_directory")
            && call.tool_name.as_deref() != Some("run_terminal")
    }));
}

#[test]
fn redirected_copy_reports_unavailable_without_clipboard_escapes() {
    let project = configured_project();
    let run = run_with_input(
        project.path(),
        &[
            "run",
            "finish the copy fixture",
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
    assert!(run.status.success(), "{run:?}");
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session_id = store
        .list_result()
        .expect("sessions")
        .into_iter()
        .next()
        .expect("one-shot session");
    let output = run_with_input(
        project.path(),
        &["--plain", "--session", &session_id],
        b"/copy\n/quit\n",
    );
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stdout.contains("Copy unavailable: output is not a terminal"),
        "{stdout}"
    );
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
}

#[cfg(unix)]
fn initialize_interrupt_repository(project: &Path) {
    let git_status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(project)
        .status()
        .expect("initialize interrupt fixture repository");
    assert!(
        git_status.success(),
        "initialize interrupt fixture repository"
    );
    let commit_status = Command::new("git")
        .args([
            "-c",
            "user.name=nib test",
            "-c",
            "user.email=nib-test@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ])
        .current_dir(project)
        .status()
        .expect("commit interrupt fixture repository");
    assert!(
        commit_status.success(),
        "commit interrupt fixture repository"
    );
}

#[cfg(unix)]
fn wait_for_interrupt_state(
    child: &mut std::process::Child,
    open_stdin: &mut Option<std::process::ChildStdin>,
    store: &SessionStore,
    ready_event: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(id) = store
            .list_result()
            .expect("list interrupt session")
            .into_iter()
            .find(|id| {
                store.load(id).is_some_and(|session| {
                    session.events.iter().any(|event| event.kind == ready_event)
                })
            })
        {
            return id;
        }
        if Instant::now() >= deadline {
            drop(open_stdin.take());
            child.kill().expect("stop timed-out interrupt fixture");
            child.wait().expect("collect timed-out interrupt fixture");
            let sessions = store
                .list_result()
                .expect("list timed-out interrupt sessions")
                .into_iter()
                .filter_map(|id| store.load(&id))
                .map(|session| {
                    (
                        session.id,
                        session
                            .events
                            .into_iter()
                            .map(|event| event.kind)
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>();
            panic!("one-shot {ready_event} state did not become ready; sessions={sessions:?}",);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn assert_one_shot_sigint_case(
    goal: &str,
    ready_event: &str,
    auto_yes: bool,
    forbidden_file: Option<&str>,
) {
    let project = configured_project();
    initialize_interrupt_repository(project.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_nib"));
    command.args([
        "run",
        goal,
        "--provider",
        "mock",
        "--model",
        "mock-model",
        "--max-steps",
        "5",
    ]);
    if auto_yes {
        command.arg("--yes");
    }
    let mut child = command
        .env("NIB_NO_UPDATE_CHECK", "1")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn interruptible one-shot run");
    let mut open_stdin = Some(child.stdin.take().expect("keep prompt input open"));
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session_id = wait_for_interrupt_state(&mut child, &mut open_stdin, &store, ready_event);

    // SAFETY: `child.id()` is the live process created above and `SIGINT` has no
    // pointer or lifetime requirements.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    assert_eq!(result, 0, "deliver SIGINT");
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("poll interrupted child").is_none() {
        assert!(
            Instant::now() < deadline,
            "one-shot SIGINT reconciliation exceeded its bound"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child
        .wait_with_output()
        .expect("collect interrupted output");
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("Run cancelled."), "{stderr}");
    let session = store.load(&session_id).expect("interrupted session");
    assert!(session.events.iter().any(|event| {
        event.kind == "run_terminal" && event.details["outcome"] == "cancelled_by_user"
    }));
    if let Some(path) = forbidden_file {
        assert!(!project.path().join(path).exists(), "{path} was created");
    }
}

#[test]
#[cfg(unix)]
fn one_shot_sigint_reconciles_and_exits_130() {
    for (goal, ready_event, auto_yes, forbidden_file) in [
        (
            "ask a question before continuing",
            "question_required",
            false,
            None,
        ),
        (
            "one-shot interrupt approval",
            "approval_required",
            false,
            Some("one-shot-approval-ran.txt"),
        ),
        (
            "one-shot interrupt terminal",
            "tool_started",
            true,
            Some("one-shot-interrupt-completed.txt"),
        ),
    ] {
        assert_one_shot_sigint_case(goal, ready_event, auto_yes, forbidden_file);
    }
}
