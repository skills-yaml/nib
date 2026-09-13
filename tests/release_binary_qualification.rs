use nib::config::{
    load_nib_config_full, save_nib_config_full, DaemonsConfig, LlmApiMode, LlmConfig, NibConfig,
    ProviderEntry,
};
use nib::session::SessionStore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::{tempdir, TempDir};

const COMPLETED_SESSION_ID: &str = "release-responses-round-trip";
const FAILED_SESSION_ID: &str = "release-failure-reconciliation";
const FIXTURE_API_KEY: &str = "release-fixture-api-key";
const FAILURE_SECRET: &str = "release-fixture-remote-secret";
const MAX_FIXTURE_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CHILD_OUTPUT_BYTES: usize = 1024 * 1024;
const RELEASE_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

struct FixtureServer {
    base_url: String,
    requests: Receiver<String>,
    handle: JoinHandle<()>,
}

fn git_output(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git must start")
}

fn git_stdout(root: &Path, args: &[&str]) -> String {
    let output = git_output(root, args);
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output must be UTF-8")
        .trim()
        .to_string()
}

fn fixture_repository() -> TempDir {
    let directory = tempdir().expect("fixture directory");
    git_stdout(directory.path(), &["init", "-q"]);
    git_stdout(
        directory.path(),
        &["config", "user.email", "nib-release@example.invalid"],
    );
    git_stdout(
        directory.path(),
        &["config", "user.name", "nib release qualification"],
    );
    git_stdout(directory.path(), &["config", "core.autocrlf", "false"]);
    fs::write(directory.path().join(".gitignore"), ".nib/\n").expect("fixture gitignore");
    fs::write(
        directory.path().join("README.md"),
        "release qualification fixture\n",
    )
    .expect("fixture readme");
    git_stdout(directory.path(), &["add", ".gitignore", "README.md"]);
    git_stdout(
        directory.path(),
        &["commit", "-qm", "release qualification fixture"],
    );
    directory
}

fn fixture_config(base_url: String) -> NibConfig {
    NibConfig {
        llm: LlmConfig {
            active_provider: Some("openai".to_string()),
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: "release-fixture-model".to_string(),
                    api_key: Some(FIXTURE_API_KEY.to_string()),
                    base_url: Some(base_url),
                    api: Some(LlmApiMode::Responses),
                    ..ProviderEntry::default()
                },
            )]),
            context_length: 128_000,
        },
        daemons: DaemonsConfig {
            cron_enabled: false,
            curator_enabled: false,
            ..DaemonsConfig::default()
        },
        ..NibConfig::default()
    }
}

fn read_http_request(stream: &mut impl Read) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer).expect("fixture request read");
        assert!(
            read > 0,
            "fixture connection closed before a complete request"
        );
        request.extend_from_slice(&buffer[..read]);
        assert!(
            request.len() <= MAX_FIXTURE_REQUEST_BYTES,
            "fixture request exceeded its byte bound"
        );
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
        assert!(
            content_length <= MAX_FIXTURE_REQUEST_BYTES,
            "fixture content length exceeded its byte bound"
        );
        if request.len() >= header_end + 4 + content_length {
            return request;
        }
    }
}

fn serve_completed_responses(responses: Vec<Value>) -> FixtureServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("Responses fixture listener");
    let address = listener.local_addr().expect("Responses fixture address");
    let (request_tx, request_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().expect("Responses fixture connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("Responses fixture timeout");
            let request = read_http_request(&mut stream);
            request_tx
                .send(String::from_utf8(request).expect("fixture request must be UTF-8"))
                .expect("capture Responses request");
            let body = format!(
                "data: {}\n\n",
                json!({"type": "response.completed", "response": response})
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("Responses fixture response");
        }
    });
    FixtureServer {
        base_url: format!("http://{address}/v1"),
        requests: request_rx,
        handle,
    }
}

fn serve_failed_response() -> FixtureServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failure fixture listener");
    let address = listener.local_addr().expect("failure fixture address");
    let (request_tx, request_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("failure fixture connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("failure fixture timeout");
        let request = read_http_request(&mut stream);
        request_tx
            .send(String::from_utf8(request).expect("fixture request must be UTF-8"))
            .expect("capture failure request");
        let body = format!(
            "data: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "id": "release-failed-response",
                    "status": "failed",
                    "error": {
                        "code": "unknown_release_fixture_code",
                        "message": format!("remote provider echoed {FAILURE_SECRET}")
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
            .expect("failure fixture response");
    });
    FixtureServer {
        base_url: format!("http://{address}/v1"),
        requests: request_rx,
        handle,
    }
}

fn run_binary(binary: &Path, project: &Path, home: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("NIB_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for provider in nib::llm::registry::PROVIDERS {
        if let Some(variable) = provider.credential_environment_variable {
            command.env_remove(variable);
        }
    }
    let mut child = command.spawn().expect("release binary must start");
    let stdout = child.stdout.take().expect("release stdout pipe");
    let stderr = child.stderr.take().expect("release stderr pipe");
    let stdout_reader = std::thread::spawn(move || read_bounded_child_output(stdout));
    let stderr_reader = std::thread::spawn(move || read_bounded_child_output(stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("poll release binary") {
            Some(status) => break status,
            None if started.elapsed() >= RELEASE_COMMAND_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                panic!(
                    "release binary exceeded the {}s command deadline",
                    RELEASE_COMMAND_TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    Output {
        status,
        stdout: stdout_reader.join().expect("join release stdout reader"),
        stderr: stderr_reader.join().expect("join release stderr reader"),
    }
}

fn read_bounded_child_output(mut reader: impl Read) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).expect("read release child output");
        if read == 0 {
            break;
        }
        let remaining = MAX_CHILD_OUTPUT_BYTES.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    output
}

fn assert_success(label: &str, output: &Output) -> String {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout must be UTF-8");
    let stderr = String::from_utf8(output.stderr.clone()).expect("stderr must be UTF-8");
    assert!(
        output.status.success(),
        "{label} failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    format!("{stdout}\n{stderr}")
}

fn parse_embedded_identity(version: &str, expected_commit: &str) -> Result<String, String> {
    let version = version.trim();
    let prefix = format!("nib {} (", env!("CARGO_PKG_VERSION"));
    let remainder = version
        .strip_prefix(&prefix)
        .and_then(|value| value.strip_suffix(')'))
        .ok_or_else(|| "release version output has an unexpected shape".to_string())?;
    let (channel, commit) = remainder
        .rsplit_once(" - ")
        .ok_or_else(|| "release version output lacks build identity".to_string())?;
    if commit != expected_commit {
        return Err(format!(
            "embedded build commit differs from expected source revision: expected {expected_commit}, got {commit}"
        ));
    }
    if channel.is_empty()
        || channel.len() > 64
        || channel.chars().any(char::is_control)
        || commit.len() != 40
        || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("release version output contains invalid bounded identity fields".to_string());
    }
    Ok(channel.to_string())
}

fn captured_json_body(request: &str) -> Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request must contain a body");
    serde_json::from_str(body).expect("captured request body must be JSON")
}

fn executable_sha256(binary: &Path) -> String {
    let mut file = fs::File::open(binary).expect("open release binary for hashing");
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("hash release binary");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    format!("{:x}", hasher.finalize())
}

fn release_binary_path() -> PathBuf {
    std::env::var_os("NIB_RELEASE_BINARY").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("release")
                .join(format!("nib{}", std::env::consts::EXE_SUFFIX))
        },
        PathBuf::from,
    )
}

fn expected_source_revision() -> String {
    let expected = std::env::var("NIB_EXPECTED_BUILD_COMMIT")
        .expect("NIB_EXPECTED_BUILD_COMMIT must be supplied by the Task target");
    assert_eq!(expected.len(), 40, "expected revision must be full length");
    assert!(
        expected
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "expected revision must be lowercase hexadecimal"
    );
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(
        git_stdout(repository, &["rev-parse", "HEAD"]),
        expected,
        "Task expected revision must equal the source checkout HEAD"
    );
    expected
}

#[test]
fn release_qualification_task_contract_is_stable() {
    let taskfile = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Taskfile.yml"))
        .expect("Taskfile")
        .replace("\r\n", "\n");
    let target = taskfile
        .split("  qualify:llm-release:\n")
        .nth(1)
        .and_then(|section| section.split("\n  dev:").next())
        .expect("qualify:llm-release Task target");
    for required in [
        "git rev-parse HEAD",
        "NIB_BUILD_COMMIT",
        "NIB_EXPECTED_BUILD_COMMIT",
        "task: test:llm-release",
        "task: build",
        "--ignored --exact --nocapture --test-threads=1",
    ] {
        assert!(target.contains(required), "Task target lacks {required}");
    }
    assert!(
        taskfile.contains(
            "test:llm-release:\n    desc: Run deterministic T021 release-binary harness contract tests\n    cmds:\n      - cargo test --test release_binary_qualification -- --test-threads=1"
        ),
        "focused deterministic Task target drifted"
    );
}

#[test]
fn embedded_identity_parser_requires_the_exact_commit() {
    let commit = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        parse_embedded_identity(
            &format!("nib {} (local - {commit})", env!("CARGO_PKG_VERSION")),
            commit
        ),
        Ok("local".to_string())
    );
    assert!(parse_embedded_identity(
        &format!(
            "nib {} (local - fedcba9876543210fedcba9876543210fedcba98)",
            env!("CARGO_PKG_VERSION")
        ),
        commit
    )
    .expect_err("mismatched identity")
    .contains("differs from expected source revision"));
}

#[test]
fn release_child_output_capture_is_bounded() {
    let output = read_bounded_child_output(std::io::Cursor::new(vec![
        b'x';
        MAX_CHILD_OUTPUT_BYTES + 8192
    ]));
    assert_eq!(output.len(), MAX_CHILD_OUTPUT_BYTES);
    assert!(output.iter().all(|byte| *byte == b'x'));
}

#[test]
#[ignore = "run through `task qualify:llm-release` after building the optimized binary"]
fn exact_release_binary_qualification() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let expected_commit = expected_source_revision();
    let binary = release_binary_path();
    assert!(
        binary.is_file(),
        "release binary is unavailable: {}",
        binary.display()
    );
    let initial_executable_sha256 = executable_sha256(&binary);
    assert_eq!(initial_executable_sha256.len(), 64);

    let project = fixture_repository();
    let home = tempdir().expect("isolated home");

    let help = run_binary(&binary, project.path(), home.path(), &["--help"]);
    let help = assert_success("release help", &help);
    assert!(help.contains("Usage: nib"));
    assert!(!help.contains("delegating to Python"));

    let conventional_version = run_binary(&binary, project.path(), home.path(), &["--version"]);
    let conventional_version = assert_success("release --version", &conventional_version);
    assert!(conventional_version.contains(env!("CARGO_PKG_VERSION")));

    let version = run_binary(&binary, project.path(), home.path(), &["version"]);
    let version = assert_success("release version", &version);
    let build_channel = parse_embedded_identity(&version, &expected_commit)
        .unwrap_or_else(|error| panic!("{error}: {version}"));

    let successful_responses = vec![
        json!({
            "id": "release-plan",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "status": "completed",
                "call_id": "release-private-plan-call",
                "name": "submit_plan",
                "arguments": "{\"steps\":[\"inspect the release fixture\"]}"
            }]
        }),
        json!({
            "id": "release-tool",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "status": "completed",
                "call_id": "release-private-runtime-call",
                "name": "list_directory",
                "arguments": "{\"path\":\".\"}"
            }]
        }),
        json!({
            "id": "release-final",
            "status": "completed",
            "output": [{
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Release fixture inspected."}]
            }]
        }),
    ];
    let successful_server = serve_completed_responses(successful_responses);
    let mut config = fixture_config(successful_server.base_url.clone());
    config.skills.enabled = false;
    save_nib_config_full(project.path(), &mut config).expect("save release fixture config");

    let doctor = run_binary(&binary, project.path(), home.path(), &["doctor"]);
    let doctor = assert_success("release doctor", &doctor);
    assert!(doctor.contains("Provider: openai"));
    assert!(doctor.contains("Transport: responses"));
    assert!(!doctor.contains(FIXTURE_API_KEY));

    let run = run_binary(
        &binary,
        project.path(),
        home.path(),
        &[
            "run",
            "inspect the release fixture through one tool",
            "--session",
            COMPLETED_SESSION_ID,
            "--max-steps",
            "8",
            "--yes",
        ],
    );
    let run = assert_success("release Responses tool round trip", &run);
    assert!(run.contains("Agent run completed for session"));
    assert!(!run.contains("delegating to Python"));
    assert!(!run.contains("release-private-runtime-call"));

    let requests = (0..3)
        .map(|_| {
            successful_server
                .requests
                .recv_timeout(Duration::from_secs(10))
                .expect("captured release Responses request")
        })
        .collect::<Vec<_>>();
    successful_server
        .handle
        .join()
        .expect("successful fixture thread");
    assert!(requests
        .iter()
        .all(|request| request.starts_with("POST /v1/responses HTTP/1.1")));
    let bodies = requests
        .iter()
        .map(|request| captured_json_body(request))
        .collect::<Vec<_>>();
    assert_eq!(bodies[0]["tools"][0]["name"], "submit_plan");
    assert!(bodies.iter().all(|body| body["store"] == false));
    let continued_input = bodies[2]["input"]
        .as_array()
        .expect("continued Responses input");
    assert!(continued_input.iter().any(|item| {
        item["type"] == "function_call" && item["call_id"] == "release-private-runtime-call"
    }));
    assert!(continued_input.iter().any(|item| {
        item["type"] == "function_call_output" && item["call_id"] == "release-private-runtime-call"
    }));

    let store = SessionStore::for_project(project.path()).expect("release session store");
    let completed = store
        .load(COMPLETED_SESSION_ID)
        .expect("completed release session");
    let completed_json = serde_json::to_string(&completed).expect("completed session JSON");
    assert!(completed_json.contains("\"outcome\":\"completed\""));
    assert!(!completed_json.contains("release-private-runtime-call"));

    let failure_server = serve_failed_response();
    let mut config = fixture_config(failure_server.base_url.clone());
    config.revision = load_nib_config_full(project.path())
        .expect("load current failure fixture revision")
        .revision;
    config.skills.enabled = false;
    save_nib_config_full(project.path(), &mut config).expect("save failure fixture config");
    let failure = run_binary(
        &binary,
        project.path(),
        home.path(),
        &[
            "run",
            "exercise typed failure reconciliation",
            "--session",
            FAILED_SESSION_ID,
            "--max-steps",
            "4",
            "--yes",
        ],
    );
    assert!(
        !failure.status.success(),
        "failure fixture unexpectedly passed"
    );
    let failure_stdout = String::from_utf8(failure.stdout).expect("failure stdout");
    let failure_stderr = String::from_utf8(failure.stderr).expect("failure stderr");
    assert!(failure_stderr.contains("LLM request failed [LLM-REJECTED]"));
    assert!(failure_stderr.contains("Cause: provider rejected during planning"));
    assert!(failure_stderr.contains(&format!("Session: {FAILED_SESSION_ID}")));
    for forbidden in [
        FIXTURE_API_KEY,
        FAILURE_SECRET,
        "remote provider echoed",
        "release-private-runtime-call",
    ] {
        assert!(!failure_stdout.contains(forbidden));
        assert!(!failure_stderr.contains(forbidden));
    }
    failure_server
        .requests
        .recv_timeout(Duration::from_secs(10))
        .expect("captured failure request");
    failure_server
        .handle
        .join()
        .expect("failure fixture thread");

    let failed = store
        .load(FAILED_SESSION_ID)
        .expect("failed release session");
    let failed_json = serde_json::to_string(&failed).expect("failed session JSON");
    assert!(failed_json.contains("\"outcome\":\"planning_failed\""));
    assert!(failed_json.contains("\"class\":\"provider_rejected\""));
    assert!(!failed_json.contains(FAILURE_SECRET));

    let worktree_status = git_output(repository, &["status", "--porcelain"]);
    assert!(
        worktree_status.status.success(),
        "could not determine release source worktree state: {}",
        String::from_utf8_lossy(&worktree_status.stderr)
    );
    let worktree_clean = worktree_status.stdout.is_empty();
    let final_executable_sha256 = executable_sha256(&binary);
    assert_eq!(
        final_executable_sha256, initial_executable_sha256,
        "release executable changed while it was being exercised"
    );
    let evidence = json!({
        "schema_version": 1,
        "status": "harness_passed",
        "acceptance_eligible": worktree_clean,
        "source_worktree_clean": worktree_clean,
        "source_revision": expected_commit,
        "embedded_build_commit": expected_commit,
        "build_channel": build_channel,
        "artifact_file": binary.file_name().and_then(|name| name.to_str()).unwrap_or("nib"),
        "artifact_size_bytes": fs::metadata(&binary).expect("release binary metadata").len(),
        "executable_sha256": final_executable_sha256,
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "exercised": [
            "help",
            "version_build_commit",
            "doctor",
            "structured_planning",
            "responses_tool_result_round_trip",
            "typed_failure_reconciliation"
        ]
    });
    let evidence_path = repository
        .join("target")
        .join("release-qualification")
        .join("t021-release-binary.json");
    fs::create_dir_all(evidence_path.parent().expect("evidence directory"))
        .expect("create evidence directory");
    let mut encoded = serde_json::to_vec_pretty(&evidence).expect("serialize evidence");
    encoded.push(b'\n');
    fs::write(&evidence_path, encoded).expect("write release qualification evidence");

    println!(
        "T021 release-binary harness passed: source_revision={} executable_sha256={} source_worktree_clean={} evidence={}",
        evidence["source_revision"].as_str().expect("source revision"),
        evidence["executable_sha256"].as_str().expect("executable digest"),
        worktree_clean,
        evidence_path.display()
    );
}
