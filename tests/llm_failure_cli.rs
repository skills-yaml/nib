use nib::config::{save_nib_config_full, LlmApiMode, NibConfig, ProviderEntry};
use nib::session::SessionStore;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

fn serve_failed_responses_once(secret: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let address = listener.local_addr().expect("fixture address");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("fixture connection");
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
