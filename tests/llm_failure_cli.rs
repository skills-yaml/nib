use nib::config::{save_nib_config_full, LlmApiMode, NibConfig, ProviderEntry};
use nib::session::SessionStore;
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use tempfile::tempdir;

const INTERACTIVE_SECRET: &str = "observer/secret+token";
const INTERACTIVE_SECRET_PERCENT: &str = "observer%2Fsecret%2Btoken";
const INTERACTIVE_SECRET_BASE64: &str = "b2JzZXJ2ZXIvc2VjcmV0K3Rva2Vu";
const REMOTE_BODY_SENTINEL: &str = "REMOTE_PROVIDER_BODY_SENTINEL";
const RECOVERY_SESSION_ID: &str = "plain-recovery-session";

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("fixture timeout");
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).expect("fixture request read");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    request
}

fn serve_failed_responses_once(secret: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let address = listener.local_addr().expect("fixture address");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("fixture connection");
        let _request = read_http_request(&mut stream);

        let body = format!(
            "data: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "id": "cli-failed-response",
                    "status": "failed",
                    "error": {
                        "code": "unknown_fixture_code",
                        "message": format!("remote provider echoed {secret}")
                    },
                    "output": []
                }
            })
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("fixture response write");
    });
    format!("http://{address}/v1")
}

fn write_responses_event(stream: &mut TcpStream, event: serde_json::Value) {
    write_responses_events(stream, [event]);
}

fn write_responses_events(
    stream: &mut TcpStream,
    events: impl IntoIterator<Item = serde_json::Value>,
) {
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .expect("interactive fixture response");
}

fn serve_interactive_failure_then_success() -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("interactive fixture listener");
    let address = listener.local_addr().expect("interactive fixture address");
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let (mut stream, _) = listener.accept().expect("interactive fixture connection");
        requests.push(read_http_request(&mut stream));
        write_responses_event(
            &mut stream,
            json!({
                "type": "response.failed",
                "response": {
                    "id": "interactive-failed-response",
                    "status": "failed",
                    "error": {
                        "code": "unknown_fixture_code",
                        "message": format!(
                            "{REMOTE_BODY_SENTINEL} <red>[bold] {} {} {} \u{1b}[31m Agent run failed: LLM request failed",
                            INTERACTIVE_SECRET,
                            INTERACTIVE_SECRET_PERCENT,
                            INTERACTIVE_SECRET_BASE64,
                        )
                    },
                    "output": []
                }
            }),
        );

        let (mut stream, _) = listener.accept().expect("recovery planner connection");
        requests.push(read_http_request(&mut stream));
        write_responses_event(
            &mut stream,
            json!({
                "type": "response.completed",
                "response": {
                    "id": "interactive-recovery-plan",
                    "status": "completed",
                    "output": [{
                        "type": "function_call",
                        "status": "completed",
                        "call_id": "private-recovery-plan-call",
                        "name": "submit_plan",
                        "arguments": "{\"steps\":[\"finish the recovered turn\"]}"
                    }]
                }
            }),
        );

        let (mut stream, _) = listener.accept().expect("recovery completion connection");
        requests.push(read_http_request(&mut stream));
        write_responses_events(
            &mut stream,
            [
                json!({
                    "type": "response.output_text.delta",
                    "delta": "Recovered assistant success."
                }),
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "interactive-recovery-final",
                        "status": "completed",
                        "output": [{
                            "type": "message",
                            "status": "completed",
                            "role": "assistant",
                            "content": [{
                                "type": "output_text",
                                "text": "Recovered assistant success."
                            }]
                        }]
                    }
                }),
            ],
        );
        requests
    });
    (format!("http://{address}/v1"), server)
}

fn configure_interactive_failure(project: &Path, base_url: String) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(project)
        .status()
        .expect("interactive git init");
    assert!(status.success());

    let mut config = NibConfig::default();
    config.llm.active_provider = Some("openai".to_string());
    config.llm.providers.clear();
    config.llm.providers.insert(
        "openai".to_string(),
        ProviderEntry {
            model: "fixture-model".to_string(),
            api_key: Some(INTERACTIVE_SECRET.to_string()),
            base_url: Some(base_url),
            api: Some(LlmApiMode::Responses),
            ..ProviderEntry::default()
        },
    );
    config.skills.enabled = false;
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    save_nib_config_full(project, &mut config).expect("interactive fixture config");
}

fn run_plain_recovery(project: &Path, no_color: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nib"));
    command
        .args(["chat", "--plain", "--session", RECOVERY_SESSION_ID])
        .env("NIB_NO_UPDATE_CHECK", "1")
        .env("COLUMNS", "24")
        .env("LINES", "6")
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_color {
        command.env("NO_COLOR", "1");
    } else {
        command.env_remove("NO_COLOR");
    }
    let mut child = command.spawn().expect("spawn plain recovery fixture");
    let mut stdin = child.stdin.take().expect("plain recovery stdin");
    stdin
        .write_all(b"first goal\n")
        .expect("plain recovery first goal");
    stdin.flush().expect("flush plain recovery first goal");

    let store = SessionStore::for_project(project).expect("plain recovery session store");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let failure_reconciled = store
            .load_result(RECOVERY_SESSION_ID)
            .expect("read plain recovery failure")
            .is_some_and(|session| {
                session.events.iter().any(|event| {
                    event.kind == "reconciliation" && event.details["outcome"] == "planning_failed"
                })
            });
        if failure_reconciled {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "plain recovery did not reconcile the first failure"
        );
        std::thread::yield_now();
    }
    stdin
        .write_all(b"second goal\n")
        .expect("plain recovery second goal");
    stdin.flush().expect("flush plain recovery second goal");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let approval_ready = store
            .load_result(RECOVERY_SESSION_ID)
            .expect("read plain recovery session")
            .is_some_and(|session| {
                session
                    .events
                    .iter()
                    .any(|event| event.kind == "approval_required")
            });
        if approval_ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "plain recovery did not reach approval"
        );
        std::thread::yield_now();
    }
    stdin
        .write_all(b"y\n\n")
        .expect("plain recovery approval frame");
    drop(stdin);
    let mut output = child.wait_with_output().expect("plain recovery output");

    let mut status_command = Command::new(env!("CARGO_BIN_EXE_nib"));
    status_command
        .args(["chat", "--plain", "--session", RECOVERY_SESSION_ID])
        .env("NIB_NO_UPDATE_CHECK", "1")
        .env("COLUMNS", "24")
        .env("LINES", "6")
        .current_dir(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_color {
        status_command.env("NO_COLOR", "1");
    } else {
        status_command.env_remove("NO_COLOR");
    }
    let mut status_child = status_command
        .spawn()
        .expect("spawn recovery status fixture");
    status_child
        .stdin
        .take()
        .expect("recovery status stdin")
        .write_all(b"/status\n/quit\n")
        .expect("recovery status input");
    let status_output = status_child
        .wait_with_output()
        .expect("recovery status output");
    if output.status.success() {
        output.status = status_output.status;
    }
    output.stdout.extend(status_output.stdout);
    output.stderr.extend(status_output.stderr);
    output
}

fn failure_report(stdout: &str) -> &str {
    let start = stdout
        .find("LLM request failed")
        .expect("plain failure report start");
    let relative_end = stdout[start..]
        .find("\n[stream ended]")
        .expect("plain failure report end");
    stdout[start..start + relative_end].trim_end()
}

fn post_failure_status_semantics(stdout: &str) -> Vec<String> {
    let report_end = stdout
        .find("\n[stream ended]")
        .expect("stream end after failure");
    let after_failure = &stdout[report_end..];
    let header = after_failure
        .lines()
        .find(|line| line.contains(&format!("sess {RECOVERY_SESSION_ID}")))
        .expect("status header after failure");
    let status = after_failure
        .lines()
        .find(|line| line.contains("openai/fixture-model") && line.contains("queue 0"))
        .expect("status line after failure");
    vec![
        header[header.find("sess ").expect("session marker")..].to_string(),
        status.to_string(),
    ]
}

#[test]
fn run_prints_one_plain_redacted_actionable_failure_and_exits_nonzero() {
    const SECRET: &str = "cli-provider-secret";
    const SESSION_ID: &str = "cli-failure-session";
    let project = tempdir().expect("project");
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(project.path())
        .status()
        .expect("git init");
    assert!(status.success());

    let mut config = NibConfig::default();
    config.llm.active_provider = Some("openai".to_string());
    config.llm.providers.clear();
    config.llm.providers.insert(
        "openai".to_string(),
        ProviderEntry {
            model: "fixture-model".to_string(),
            api_key: Some(SECRET.to_string()),
            base_url: Some(serve_failed_responses_once(SECRET)),
            api: Some(LlmApiMode::Responses),
            ..ProviderEntry::default()
        },
    );
    config.skills.enabled = false;
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    save_nib_config_full(project.path(), &mut config).expect("fixture config");

    let output = Command::new(env!("CARGO_BIN_EXE_nib"))
        .args([
            "run",
            "inspect the workspace",
            "--session",
            SESSION_ID,
            "--yes",
        ])
        .env("NIB_NO_UPDATE_CHECK", "1")
        .current_dir(project.path())
        .output()
        .expect("nib run");

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert_eq!(stderr.matches("LLM request failed").count(), 1, "{stderr}");
    for expected in [
        "LLM request failed [LLM-REJECTED]",
        "Cause: provider rejected during planning",
        "Provider: openai (responses), model: fixture-model",
        "Retry: not retryable",
        "Action: Run `nib doctor`",
        "Session: cli-failure-session",
    ] {
        assert!(stderr.contains(expected), "missing {expected}: {stderr}");
    }
    for forbidden in [
        SECRET,
        "remote provider echoed",
        "provider-supplied detail omitted",
        "agent run failed",
        "llm_stream_failed",
        "[red]",
        "[dim]",
        "\u{1b}",
    ] {
        assert!(!stderr.contains(forbidden), "found {forbidden}: {stderr}");
    }
    assert!(!stdout.contains("LLM request failed"), "{stdout}");

    let session = SessionStore::for_project(project.path())
        .expect("session store")
        .load(SESSION_ID)
        .expect("failure session");
    assert!(!session.messages.iter().any(|message| {
        message.role == "assistant"
            && (message.content.contains("LLM") || message.content.contains("failed"))
    }));
    let serialized = serde_json::to_string(&session).expect("session JSON");
    assert!(!serialized.contains(SECRET), "{serialized}");
    assert!(
        !serialized.contains("remote provider echoed"),
        "{serialized}"
    );
    assert!(serialized.contains("\"outcome\":\"planning_failed\""));
    assert!(serialized.contains("\"class\":\"provider_rejected\""));
}

#[test]
fn plain_chat_recovers_after_one_structured_failure_with_identical_safe_output() {
    const MAX_STDOUT_BYTES: usize = 16 * 1024;
    const MAX_STDERR_BYTES: usize = 4 * 1024;
    let mut reports = Vec::new();
    let mut statuses = Vec::new();

    for no_color in [false, true] {
        let project = tempdir().expect("plain recovery project");
        let (base_url, server) = serve_interactive_failure_then_success();
        configure_interactive_failure(project.path(), base_url);
        let store = SessionStore::for_project(project.path()).expect("recovery session store");
        let initial = store.create_session_with_id(RECOVERY_SESSION_ID);
        assert!(initial.messages.is_empty());
        assert!(initial.events.is_empty());

        let output = run_plain_recovery(project.path(), no_color);
        let requests = server.join().expect("interactive fixture server");
        assert_eq!(requests.len(), 3);
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with(b"POST /v1/responses HTTP/1.1")),
            "{requests:?}"
        );
        assert!(
            output.status.success(),
            "NO_COLOR={no_color}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.len() <= MAX_STDOUT_BYTES);
        assert!(output.stderr.len() <= MAX_STDERR_BYTES);
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 recovery stdout");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 recovery stderr");
        let combined = format!("{stdout}\n{stderr}");

        assert_eq!(
            combined.matches("LLM request failed").count(),
            1,
            "{combined}"
        );
        let report = failure_report(&stdout);
        assert!(report.len() <= 512, "report length: {}", report.len());
        for expected in [
            "LLM request failed [LLM-REJECTED]",
            "Cause: provider rejected during planning",
            "Provider: openai (responses), model: fixture-model",
            "Retry: not retryable",
            "Action: Run `nib doctor`",
            "Session: plain-recovery-session",
        ] {
            assert!(report.contains(expected), "missing {expected}: {report}");
        }
        assert!(
            stdout.contains("[stream ended] planning_failed"),
            "{stdout}"
        );
        assert!(stdout.contains("Recovered assistant success."), "{stdout}");
        assert!(stdout.contains("[stream ended] completed"), "{stdout}");
        assert!(stdout.contains("Goodbye. Session saved to"), "{stdout}");
        let status = post_failure_status_semantics(&stdout);

        for forbidden in [
            INTERACTIVE_SECRET,
            INTERACTIVE_SECRET_PERCENT,
            INTERACTIVE_SECRET_BASE64,
            REMOTE_BODY_SENTINEL,
            "remote provider",
            "Agent run failed",
            "LLM request failed: LLM request failed",
            "[red]",
            "[/red]",
            "[bold]",
            "[/bold]",
            "\u{1b}",
        ] {
            assert!(
                !combined.contains(forbidden),
                "found {forbidden}: {combined}"
            );
        }
        assert!(combined.chars().all(|character| {
            !character.is_control() || matches!(character, '\n' | '\r' | '\t')
        }));

        assert_eq!(store.list(), vec![RECOVERY_SESSION_ID.to_string()]);
        let session = store.load(RECOVERY_SESSION_ID).expect("recovery session");
        session
            .validate_message_sequence()
            .expect("authoritative message sequence");
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| {
                    message.role == "user"
                        && matches!(message.content.as_str(), "first goal" | "second goal")
                })
                .count(),
            2
        );
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|message| {
                    message.role == "assistant" && message.content == "Recovered assistant success."
                })
                .count(),
            1
        );
        assert!(!session.messages.iter().any(|message| {
            message.role == "assistant"
                && (message.content.contains("LLM") || message.content.contains("failed"))
        }));
        let serialized = serde_json::to_string(&session).expect("recovery session JSON");
        assert!(serialized.contains("\"outcome\":\"planning_failed\""));
        assert!(serialized.contains("\"outcome\":\"completed\""));
        assert!(serialized.contains("\"class\":\"provider_rejected\""));
        for forbidden in [
            INTERACTIVE_SECRET,
            INTERACTIVE_SECRET_PERCENT,
            INTERACTIVE_SECRET_BASE64,
            REMOTE_BODY_SENTINEL,
        ] {
            assert!(!serialized.contains(forbidden), "{serialized}");
        }

        reports.push(report.to_string());
        statuses.push(status);
    }

    assert_eq!(reports[0], reports[1]);
    assert_eq!(statuses[0], statuses[1]);
}
