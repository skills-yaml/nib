//! Exercise diagnostic phases through the real scenario and production adapter.
use super::*;
use nib::llm::anthropic::AnthropicClient;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn request_body(stream: &mut TcpStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0 && bytes.len() + count <= 65536);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
            }
        }
    }
}

fn serve(
    initial_refusal: bool,
    final_refusal: bool,
    requests: usize,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        for index in 0..requests {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "fixture request deadline");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            let request = request_body(&mut stream);
            let body = if (index == 0 && initial_refusal) || (index == 1 && final_refusal) {
                json!({"content": [{"type":"text", "text":"PRIVATE_REFUSAL"}], "stop_reason":"refusal"})
            } else if index == 0 {
                let prompt = request["messages"][0]["content"].as_str().unwrap();
                let nonce = prompt.split("Pass nonce ").nth(1).unwrap().split_whitespace().next().unwrap();
                json!({"content":[{"type":"tool_use", "id":"private-call", "name":"record_probe", "input":{"nonce":nonce}}], "stop_reason":"tool_use"})
            } else {
                let messages = request["messages"].as_array().unwrap();
                let result = &messages.last().unwrap()["content"][0];
                assert_eq!(result["tool_use_id"], "private-call");
                let payload: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
                json!({"content":[{"type":"text", "text":payload["receipt"]}], "stop_reason":"end_turn"})
            }.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    (address, handle)
}

fn profile() -> ModelProfile {
    ModelProfile {
        model: serde_json::from_value(json!({
            "id":"fixture-model", "aliases":[], "supported_parameters":[], "public_identifier":true
        }))
        .unwrap(),
        transport: TransportId::AnthropicMessages,
        advertised: true,
        required_scenarios: Default::default(),
        not_applicable_scenarios: Default::default(),
        projected_cost_ceiling_usd: None,
    }
}

#[tokio::test]
async fn tool_scenario_reports_actual_failure_phase_and_admission_boundary() {
    for (initial_refusal, final_refusal, budget, requests, phase, class) in [
        (
            true,
            false,
            6,
            1,
            Some(ToolFailurePhase::InitialRequest),
            Some("response_mismatch"),
        ),
        (
            false,
            true,
            6,
            2,
            Some(ToolFailurePhase::Continuation),
            Some("response_refused"),
        ),
        (
            false,
            false,
            1,
            1,
            Some(ToolFailurePhase::Continuation),
            Some("blocked_budget"),
        ),
        (false, false, 6, 2, None, None),
    ] {
        let (url, handle) = serve(initial_refusal, final_refusal, requests);
        let client =
            AnthropicClient::with_base_url("fixture-model".into(), vec!["fixture-key".into()], url)
                .unwrap();
        let mut settings = super::tests::settings();
        settings.limits.allow_unpriced = true;
        settings.limits.max_logical_requests = budget;
        let run = RunBudget::new();
        let mut provider = ProviderBudget::new(&settings, &run);
        let execution = tool_continuation(
            &client,
            &settings,
            "fixture-run",
            false,
            &mut provider,
            &profile(),
        )
        .await;
        handle.join().unwrap();
        let report = execution.evidence.report(
            ScenarioId::SingleToolContinuation,
            1,
            None,
            execution.result,
        );
        assert_eq!(report.tool_failure_phase, phase);
        assert_eq!(report.safe_error_class.as_deref(), class);
        assert_eq!(report.actual_logical_requests, requests);
        assert_eq!(report.passed, class.is_none());
        let serialized = serde_json::to_string(&report).unwrap();
        for secret in ["PRIVATE_REFUSAL", "private-call", "fixture-key"] {
            assert!(!serialized.contains(secret));
        }
    }
}
