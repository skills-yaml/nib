use super::*;
use crate::config::{save_nib_config_full, LlmConfig, NibConfig, ProviderEntry};
use crate::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};
use std::collections::HashMap;
use std::io::Write as _;
use std::time::Duration;
use tempfile::tempdir;

const WORKER_MODEL: &str = "worker-model";
const ESCALATION_MODEL: &str = "escalation-model";
const GOAL: &str = "finish work";

fn usage(total: u64) -> crate::llm::LlmUsage {
    crate::llm::LlmUsage::new(total - 1, 1, total, None, None).expect("usage")
}

fn blank_session() -> Session {
    serde_json::from_value(json!({"id": "escalation-test"})).expect("session")
}

fn session_with_events(events: Vec<(&str, Value)>) -> Session {
    let mut session = blank_session();
    for (kind, details) in events {
        session.events.push(SessionEvent {
            index: session.events.len(),
            kind: kind.to_string(),
            details,
            timestamp: None,
        });
    }
    session
}

#[test]
fn budget_admits_until_the_dispatch_limit() {
    let mut budget = EscalationBudget::new(2);
    assert_eq!(budget.admit(), Ok(()));
    budget.dispatched = 2;
    assert_eq!(budget.admit(), Err("dispatch_limit_reached"));
    assert_eq!(
        EscalationBudget::new(0).admit(),
        Err("dispatch_limit_reached")
    );
}

#[test]
fn budget_enforces_the_token_share_only_on_reported_usage() {
    let mut budget = EscalationBudget::new(5);
    budget.record_worker_usage(Some(&usage(300)));
    budget.record_escalation_usage(Some(&usage(99)));
    assert_eq!(budget.token_share_percent(), Some(24));
    assert_eq!(budget.admit(), Ok(()));
    budget.record_escalation_usage(Some(&usage(2)));
    // 101 of 401 tokens is just over 25%.
    assert_eq!(budget.admit(), Err("token_share_exceeded"));

    // A call without usage makes the share unknown; only calls are counted.
    budget.record_worker_usage(None);
    assert_eq!(budget.token_share_percent(), None);
    assert_eq!(budget.admit(), Ok(()));
    let snapshot = budget.snapshot();
    assert_eq!(snapshot["usage_basis"], "calls");
    assert_eq!(snapshot["token_share_percent"], Value::Null);
    assert_eq!(snapshot["worker_calls_without_usage"], 1);
}

#[test]
fn budget_counts_cache_reads_and_writes_reported_by_the_provider() {
    let mut budget = EscalationBudget::new(2);
    let cached = crate::llm::LlmUsage::new(100, 10, 110, Some(60), None)
        .and_then(|usage| usage.with_cache_write_input_tokens(Some(30)))
        .expect("cache usage");
    budget.record_escalation_usage(Some(&cached));
    assert_eq!(budget.escalation_tokens, 110);
    assert_eq!(budget.escalation_cache_read_tokens, 60);
    assert_eq!(budget.escalation_cache_write_tokens, 30);
}

#[test]
fn restored_budget_keeps_interrupted_reservations_and_ignores_other_runs() {
    let session = session_with_events(vec![
        (
            "escalation_dispatched",
            json!({"run_id": "run-1", "kind": "advice"}),
        ),
        (
            "escalation_completed",
            json!({"run_id": "run-1", "outcome": "advice_delivered",
                   "usage": {"input_tokens": 9, "output_tokens": 1, "total_tokens": 10}}),
        ),
        // Cancelled before completion: still reserved, usage unknown.
        (
            "escalation_dispatched",
            json!({"run_id": "run-1", "kind": "takeover"}),
        ),
        (
            "escalation_dispatched",
            json!({"run_id": "run-2", "kind": "advice"}),
        ),
        (
            "context_usage",
            json!({"run_id": "run-1", "request_sequence": 1, "unknown": false,
                   "input_tokens": 39, "output_tokens": 1, "total_tokens": 40}),
        ),
        (
            "context_usage",
            json!({"run_id": "run-1", "request_sequence": 1, "unknown": false,
                   "model_role": "escalation_advice",
                   "input_tokens": 9, "output_tokens": 1, "total_tokens": 10}),
        ),
    ]);
    let budget = EscalationBudget::from_session(2, &session, "run-1");
    assert_eq!(budget.dispatched, 2);
    assert_eq!(budget.advice_delivered, 1);
    assert_eq!(budget.escalation_tokens, 10);
    assert_eq!(budget.worker_tokens, 40);
    assert_eq!(budget.escalation_calls_without_usage, 1);
    assert_eq!(budget.admit(), Err("dispatch_limit_reached"));
    assert_eq!(
        EscalationBudget::from_session(2, &session, "run-2").dispatched,
        1
    );
}

fn runtime(budget: EscalationBudget) -> EscalationRuntime {
    EscalationRuntime {
        client: Arc::new(crate::llm::mock::MockLlmClient::new()),
        provider: "mock".to_string(),
        model: ESCALATION_MODEL.to_string(),
        budget,
        consecutive_failures: 0,
        pending: None,
    }
}

#[test]
fn advice_comes_first_and_later_escalations_take_over_one_turn() {
    let mut escalation = runtime(EscalationBudget::new(2));
    for trigger in [
        EscalationTrigger::WorkerRequest,
        EscalationTrigger::RepeatedFailure,
        EscalationTrigger::UserCommand,
    ] {
        assert_eq!(escalation.next_kind(trigger), EscalationKind::Advice);
    }
    escalation.budget.advice_delivered = 1;
    assert_eq!(
        escalation.next_kind(EscalationTrigger::RepeatedFailure),
        EscalationKind::Takeover
    );
    assert_eq!(
        escalation.next_kind(EscalationTrigger::UserCommand),
        EscalationKind::Takeover
    );
    // The worker's own request is always advice.
    assert_eq!(
        escalation.next_kind(EscalationTrigger::WorkerRequest),
        EscalationKind::Advice
    );
}

#[test]
fn failure_trigger_needs_three_consecutive_failures() {
    let mut escalation = runtime(EscalationBudget::new(2));
    assert!(!escalation.observe_failure());
    assert!(!escalation.observe_failure());
    escalation.observe_success();
    assert!(!escalation.observe_failure());
    assert!(!escalation.observe_failure());
    assert!(escalation.observe_failure());
    // The streak restarts after a trigger.
    assert_eq!(escalation.consecutive_failures, 0);
}

#[test]
fn user_requests_are_consumed_once() {
    let mut session = session_with_events(vec![(
        "escalation_requested",
        json!({"note": "check the parser"}),
    )]);
    assert_eq!(
        pending_user_escalation_request(&session),
        Some((0, "check the parser".to_string()))
    );
    session.events.push(SessionEvent {
        index: 1,
        kind: "escalation_skipped".to_string(),
        details: json!({"request_event_index": 0}),
        timestamp: None,
    });
    assert_eq!(pending_user_escalation_request(&session), None);
    session.events.push(SessionEvent {
        index: 2,
        kind: "escalation_requested".to_string(),
        details: json!({"note": null}),
        timestamp: None,
    });
    assert_eq!(
        pending_user_escalation_request(&session),
        Some((2, String::new()))
    );
}

#[test]
fn advice_question_is_required_and_bounded() {
    assert_eq!(
        advice_question_argument(&json!({"question": "  why?  "})),
        Ok("why?".to_string())
    );
    assert!(advice_question_argument(&json!({"question": "   "})).is_err());
    assert!(advice_question_argument(&json!({})).is_err());
    let long = "x".repeat(MAX_ADVICE_QUESTION_BYTES + 1);
    assert!(advice_question_argument(&json!({ "question": long })).is_err());
}

#[test]
fn advice_context_is_bounded_and_keeps_plan_obligations_and_instructions() {
    let mut session = blank_session();
    let mut plan = super::super::tests::pending_plan(GOAL, "fix the parser");
    plan.steps[0]
        .verification_obligations
        .push(crate::session::VerificationObligation::pending(
            "cargo-test",
            "cargo test",
            Vec::new(),
        ));
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    for index in 0..20 {
        session.messages.push(SessionMessage {
            index,
            role: "tool".to_string(),
            content: format!("observation {index} {}", "y".repeat(10_000)),
            timestamp: None,
            attachments: Vec::new(),
        });
    }
    let pending = PendingEscalation {
        trigger: EscalationTrigger::RepeatedFailure,
        kind: EscalationKind::Advice,
        question: "What next?".to_string(),
        request_event_index: None,
    };
    let instructions = format!("Always run cargo test. {}", "z".repeat(50_000));
    let messages = advice_messages(
        GOAL,
        &instructions,
        &session,
        Some(&plan_id),
        &pending,
        &["secret-token".to_string()],
    );
    assert_eq!(messages.len(), 2);
    let body = &messages[1].content;
    assert!(body.contains("Always run cargo test."));
    assert!(body.contains("fix the parser"));
    assert!(body.contains("Open verification obligations: cargo-test"));
    assert!(body.contains("observation 19"));
    assert!(!body.contains("observation 13"), "only recent observations");
    assert!(body.contains("What next?"));
    let bound = 5 * MAX_ADVICE_SECTION_BYTES + MAX_ADVICE_QUESTION_BYTES + 1_024;
    assert!(
        body.len() < bound,
        "advice context is bounded: {}",
        body.len()
    );
}

fn escalation_nib_config(model: Option<&str>) -> NibConfig {
    let mut config = NibConfig {
        llm: super::super::tests::mock_config(),
        ..NibConfig::default()
    };
    config.agent.escalation.model = model.map(str::to_string);
    config
}

#[test]
fn escalation_is_off_unless_configured_allowed_and_unpinned() {
    let session = blank_session();
    let enabled = escalation_nib_config(Some(ESCALATION_MODEL));
    let prepare = |config: &NibConfig, cfg: &AgentLoopConfig| {
        prepare_escalation_runtime(config, cfg, &session, "run", &[])
            .expect("prepare")
            .map(|runtime| runtime.label())
    };
    assert_eq!(
        prepare(&enabled, &AgentLoopConfig::default()),
        Some(format!("mock/{ESCALATION_MODEL}"))
    );
    assert_eq!(
        prepare(&escalation_nib_config(None), &AgentLoopConfig::default()),
        None
    );
    let pinned = AgentLoopConfig {
        model: Some("mock-model".to_string()),
        ..Default::default()
    };
    assert_eq!(prepare(&enabled, &pinned), None, "a pin overrides routing");
    let delegated = AgentLoopConfig {
        escalation: false,
        ..Default::default()
    };
    assert_eq!(prepare(&enabled, &delegated), None);
    let plan_only = AgentLoopConfig {
        mode: "plan".to_string(),
        ..Default::default()
    };
    assert_eq!(prepare(&enabled, &plan_only), None);
    let mut zero = enabled.clone();
    zero.agent.escalation.max_per_run = 0;
    assert_eq!(prepare(&zero, &AgentLoopConfig::default()), None);
}

#[test]
fn escalation_configuration_is_validated_and_omitted_by_default() {
    let mut config = escalation_nib_config(Some(ESCALATION_MODEL));
    assert!(config.validate().is_ok());
    config.agent.escalation.provider = Some("missing".to_string());
    config.agent.escalation.max_per_run = crate::config::MAX_ESCALATIONS_PER_RUN + 1;
    config.agent.escalation.model = Some(" ".to_string());
    let error = config
        .validate()
        .expect_err("invalid escalation")
        .to_string();
    assert!(error.contains("agent.escalation.provider"));
    assert!(error.contains("agent.escalation.max_per_run"));
    assert!(error.contains("agent.escalation.model"));

    let serialized = toml::to_string(&NibConfig::default()).expect("toml");
    assert!(!serialized.contains("escalation"));
    let parsed: NibConfig =
        toml::from_str("[agent.escalation]\nmodel = \"escalation-model\"\nmax_per_run = 1\n")
            .expect("parse");
    assert_eq!(parsed.agent.escalation.max_per_run, 1);
    assert!(parsed.agent.escalation.enabled());
    assert!(toml::from_str::<NibConfig>("[agent.escalation]\nunknown = 1\n").is_err());
}

#[test]
fn status_line_reports_configuration_usage_and_pending_requests() {
    let config = escalation_nib_config(Some(ESCALATION_MODEL));
    assert_eq!(
        format_escalation_status(&escalation_nib_config(None), None),
        "Escalation: off (set agent.escalation.model to enable)"
    );
    let session = session_with_events(vec![
        ("escalation_dispatched", json!({"run_id": "r"})),
        (
            "escalation_completed",
            json!({"run_id": "r", "outcome": "advice_delivered"}),
        ),
        ("escalation_requested", json!({ "note": null })),
    ]);
    let line = format_escalation_status(&config, Some(&session));
    assert!(line.contains("mock/escalation-model, up to 2 per run, at most 25%"));
    assert!(line.contains("session: 1 used, latest advice_delivered"));
    assert!(line.ends_with("/escalate pending"));
}

#[test]
fn plain_and_tui_share_the_escalation_status_text() {
    assert_eq!(
        escalation_status_text("advice", "repeated_failure", "openai/big", "started", None),
        "advice from openai/big (repeated failures): started"
    );
    assert_eq!(
        escalation_status_text(
            "takeover",
            "user_command",
            "openai/big",
            "skipped",
            Some("dispatch_limit_reached")
        ),
        "takeover from openai/big (/escalate): skipped - dispatch_limit_reached"
    );
}

// ---- Loop-level fixtures with a scripted Responses provider ----

fn with_usage(mut response: Value, total: Option<u64>) -> Value {
    if let Some(total) = total {
        response["usage"] =
            json!({"input_tokens": total - 1, "output_tokens": 1, "total_tokens": total});
    }
    response
}

fn tool_response(id: &str, name: &str, arguments: Value, total: Option<u64>) -> Value {
    with_usage(
        json!({"id": format!("response-{id}"), "status": "completed", "output": [{
            "type": "function_call", "status": "completed", "call_id": format!("private-{id}"),
            "name": name, "arguments": arguments.to_string()}]}),
        total,
    )
}

fn text_response(id: &str, text: &str, total: Option<u64>) -> Value {
    with_usage(
        json!({"id": format!("response-{id}"), "status": "completed", "output": [{
            "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": text}]}]}),
        total,
    )
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Value> {
    use std::io::Read as _;
    let mut data = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..count]);
        let Some(end) = data.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let header = String::from_utf8_lossy(&data[..end]).to_string();
        if !header.starts_with("POST /v1/responses ") {
            return None;
        }
        let length = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if data.len() >= end + 4 + length {
            return serde_json::from_slice(&data[end + 4..end + 4 + length]).ok();
        }
    }
}

/// Serves the scripted responses in order and captures each request body.
fn scripted_server(responses: Vec<Value>) -> (String, std::sync::mpsc::Receiver<Value>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut responses = responses.into_iter();
        while let Ok((mut stream, _)) = listener.accept() {
            stream.set_nonblocking(false).ok();
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let Some(request) = read_request(&mut stream) else {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            };
            let _ = sender.send(request);
            let Some(response) = responses.next() else {
                let _ = stream.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            };
            let body = format!(
                "data: {}\n\n",
                json!({"type": "response.completed", "response": response})
            );
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{address}/v1"), receiver)
}

struct Fixture {
    root: tempfile::TempDir,
    store: SessionStore,
    session_id: String,
}

fn fixture(url: &str, escalation_model: Option<&str>, max_per_run: u32) -> Fixture {
    let root = tempdir().expect("root");
    super::super::tests::initialize_git_repository(root.path());
    let mut config = NibConfig::default();
    config.execution.provider = "internal".to_string();
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    config.agent.answer_only = false;
    config.agent.escalation.model = escalation_model.map(str::to_string);
    config.agent.escalation.max_per_run = max_per_run;
    config.llm = LlmConfig {
        active_provider: Some("openai".to_string()),
        providers: HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                model: WORKER_MODEL.to_string(),
                api_key: Some("fixture-key".to_string()),
                base_url: Some(url.to_string()),
                api: Some(crate::config::LlmApiMode::Responses),
                ..Default::default()
            },
        )]),
        context_length: 128_000,
    };
    save_nib_config_full(root.path(), &mut config).expect("config");
    let store = SessionStore::for_project(root.path()).expect("store");
    let mut session = store.create_session();
    let mut plan = super::super::tests::pending_plan(GOAL, "finish the approved step");
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).expect("save");
    Fixture {
        session_id: session.id.clone(),
        root,
        store,
    }
}

struct DenyEverything;

#[async_trait::async_trait]
impl crate::tools::executor::ApprovalHandler for DenyEverything {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        ApprovalDecision::denied()
    }
}

async fn run(fixture: &Fixture, cfg: AgentLoopConfig) -> (AgentRunSummary, Session) {
    let summary = tokio::time::timeout(
        Duration::from_secs(60),
        run_agent_loop(
            fixture.root.path().to_path_buf(),
            &fixture.session_id,
            GOAL,
            cfg,
        ),
    )
    .await
    .expect("run finishes")
    .expect("run result");
    let session = fixture.store.load(&fixture.session_id).expect("session");
    (summary, session)
}

fn auto_cfg() -> AgentLoopConfig {
    AgentLoopConfig {
        max_steps: 20,
        auto_approve: true,
        ..Default::default()
    }
}

fn events<'a>(session: &'a Session, kind: &str) -> Vec<&'a SessionEvent> {
    session
        .events
        .iter()
        .filter(|event| event.kind == kind)
        .collect()
}

fn models(requests: &[Value]) -> Vec<String> {
    requests
        .iter()
        .map(|request| request["model"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn offers_ask_advice(request: &Value) -> bool {
    request["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == ASK_ADVICE_TOOL))
}

fn replays_function_calls(request: &Value) -> bool {
    request["input"].as_array().is_some_and(|input| {
        input.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call" | "function_call_output")
            )
        })
    })
}

#[tokio::test]
async fn routing_disabled_runs_keep_the_worker_and_record_no_escalation() {
    let (url, requests) = scripted_server(vec![text_response("final", "Done.", Some(20))]);
    let fixture = fixture(&url, None, 2);
    let (summary, session) = run(&fixture, auto_cfg()).await;
    assert_eq!(summary.outcome, "completed");
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(models(&requests), [WORKER_MODEL]);
    assert!(!offers_ask_advice(&requests[0]));
    assert!(session
        .events
        .iter()
        .all(|event| !event.kind.starts_with("escalation")));
}

#[tokio::test]
async fn pinned_runs_do_not_escalate() {
    let (url, requests) = scripted_server(vec![text_response("final", "Done.", None)]);
    let fixture = fixture(&url, Some(ESCALATION_MODEL), 2);
    let cfg = AgentLoopConfig {
        provider: Some("openai".to_string()),
        model: Some(WORKER_MODEL.to_string()),
        ..auto_cfg()
    };
    let (summary, _session) = run(&fixture, cfg).await;
    assert_eq!(summary.outcome, "completed");
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(models(&requests), [WORKER_MODEL]);
    assert!(!offers_ask_advice(&requests[0]));
}

#[tokio::test]
async fn worker_advice_is_bounded_audited_and_the_worker_continues() {
    let (url, requests) = scripted_server(vec![
        tool_response(
            "ask-1",
            ASK_ADVICE_TOOL,
            json!({"question": "Which file holds the parser?"}),
            None,
        ),
        text_response("advice", "Look at README.md first.", None),
        tool_response(
            "ask-2",
            ASK_ADVICE_TOOL,
            json!({"question": "Anything else?"}),
            None,
        ),
        tool_response("read", "read_file", json!({"path": "README.md"}), None),
        text_response("final", "Done.", None),
    ]);
    let fixture = fixture(&url, Some(ESCALATION_MODEL), 1);
    let (summary, session) = run(&fixture, auto_cfg()).await;
    assert_eq!(summary.outcome, "completed");
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(
        models(&requests),
        [
            WORKER_MODEL,
            ESCALATION_MODEL,
            WORKER_MODEL,
            WORKER_MODEL,
            WORKER_MODEL
        ]
    );
    assert!(offers_ask_advice(&requests[0]));
    // The advisor gets a fresh, tool-free, output-bounded request.
    let advice_request = &requests[1];
    assert!(advice_request.get("tools").is_none());
    assert_eq!(
        advice_request["max_output_tokens"],
        MAX_ADVICE_OUTPUT_TOKENS
    );
    assert!(!replays_function_calls(advice_request));
    assert!(advice_request
        .to_string()
        .contains("Which file holds the parser?"));
    // The worker continues its own turn with the advice as the tool result.
    assert!(requests[2].to_string().contains("Look at README.md first."));

    let dispatched = events(&session, "escalation_dispatched");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].details["trigger"], "worker_request");
    assert_eq!(dispatched[0].details["kind"], "advice");
    assert_eq!(dispatched[0].details["model"], ESCALATION_MODEL);
    assert_eq!(dispatched[0].details["run_id"], summary.run_id.as_str());
    let completed = events(&session, "escalation_completed");
    assert_eq!(completed[0].details["outcome"], "advice_delivered");
    // Missing usage is explicit, never zero.
    assert_eq!(completed[0].details["usage_available"], false);
    assert_eq!(completed[0].details["budget"]["usage_basis"], "calls");
    // The second request exceeds the per-run limit and is refused, not dispatched.
    let skipped = events(&session, "escalation_skipped");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].details["reason"], "dispatch_limit_reached");
    assert!(requests[3]
        .to_string()
        .contains("budget for this run is used up"));
}

#[tokio::test]
async fn token_share_limit_refuses_further_escalation() {
    let (url, requests) = scripted_server(vec![
        tool_response(
            "ask-1",
            ASK_ADVICE_TOOL,
            json!({"question": "Help?"}),
            Some(100),
        ),
        text_response("advice", "Try reading the file.", Some(1_000)),
        tool_response(
            "ask-2",
            ASK_ADVICE_TOOL,
            json!({"question": "More help?"}),
            Some(100),
        ),
        tool_response("read", "read_file", json!({"path": "README.md"}), Some(100)),
        text_response("final", "Done.", Some(100)),
    ]);
    let fixture = fixture(&url, Some(ESCALATION_MODEL), 2);
    let (summary, session) = run(&fixture, auto_cfg()).await;
    assert_eq!(summary.outcome, "completed");
    assert_eq!(requests.try_iter().count(), 5);
    let completed = events(&session, "escalation_completed");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].details["usage"]["total_tokens"], 1_000);
    assert_eq!(completed[0].details["budget"]["usage_basis"], "tokens");
    let skipped = events(&session, "escalation_skipped");
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].details["reason"], "token_share_exceeded");
    assert!(skipped[0].details["budget"]["token_share_percent"].as_u64() > Some(25));
    // Advice tokens are part of the session's reported usage.
    let totals = crate::context::snapshot::accumulate_session_usage(&session);
    assert_eq!(totals.request_count, 5);
}

#[tokio::test]
async fn repeated_failures_escalate_to_advice_then_a_gated_one_turn_takeover() {
    let failing = || tool_response("fail", "read_file", json!({"path": "missing.txt"}), None);
    let (url, requests) = scripted_server(vec![
        failing(),
        failing(),
        failing(),
        text_response("advice", "The file does not exist; create it.", None),
        failing(),
        failing(),
        failing(),
        // The takeover proposes a mutation; the denied approval still applies.
        tool_response(
            "takeover",
            "run_terminal",
            json!({"command": "touch created.txt", "affected_paths": ["created.txt"]}),
            None,
        ),
    ]);
    let fixture = fixture(&url, Some(ESCALATION_MODEL), 2);
    let cfg = AgentLoopConfig {
        max_steps: 20,
        auto_approve: false,
        approval_handler: Some(Arc::new(DenyEverything)),
        ..Default::default()
    };
    let (summary, session) = run(&fixture, cfg).await;
    assert_eq!(summary.outcome, "tool_execution_failed");
    assert!(!fixture.root.path().join("created.txt").exists());

    let requests = requests.try_iter().collect::<Vec<_>>();
    let models = models(&requests);
    assert_eq!(models.len(), 8);
    assert_eq!(models[3], ESCALATION_MODEL);
    assert_eq!(models[7], ESCALATION_MODEL);
    assert!(models[..3]
        .iter()
        .chain(&models[4..7])
        .all(|model| model == WORKER_MODEL));
    // The worker resumes from the session after advice, not its old continuation.
    assert!(requests[4]
        .to_string()
        .contains("The file does not exist; create it."));
    assert!(!replays_function_calls(&requests[4]));
    // The takeover is a fresh request with tools but no worker continuation.
    assert!(!replays_function_calls(&requests[7]));
    assert!(requests[7]["tools"].is_array());

    let dispatched = events(&session, "escalation_dispatched");
    let kinds = dispatched
        .iter()
        .map(|event| event.details["kind"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["advice", "takeover"]);
    assert!(dispatched
        .iter()
        .all(|event| event.details["trigger"] == "repeated_failure"));
    let outcomes = events(&session, "escalation_completed")
        .iter()
        .map(|event| event.details["outcome"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(outcomes, ["advice_delivered", "takeover_completed"]);
    assert!(events(&session, "provider_continuation_abandoned")
        .iter()
        .any(|event| event.details["reason"] == "model_escalation"));
    assert!(events(&session, "context_usage")
        .iter()
        .any(|event| event.details["model_role"] == "escalation"));
}

#[tokio::test]
async fn user_escalation_request_gives_advice_before_the_next_worker_turn_once() {
    let (url, requests) = scripted_server(vec![
        text_response("advice", "Keep the change small.", None),
        text_response("final", "Done.", None),
    ]);
    let fixture = fixture(&url, Some(ESCALATION_MODEL), 2);
    record_user_escalation_request(
        &fixture.store,
        &fixture.session_id,
        Some("Is the plan sound?"),
    )
    .expect("request");
    let (summary, session) = run(&fixture, auto_cfg()).await;
    assert_eq!(summary.outcome, "completed");
    let requests = requests.try_iter().collect::<Vec<_>>();
    assert_eq!(models(&requests), [ESCALATION_MODEL, WORKER_MODEL]);
    assert!(requests[0].to_string().contains("Is the plan sound?"));
    assert!(requests[1].to_string().contains("Keep the change small."));
    let dispatched = events(&session, "escalation_dispatched");
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0].details["trigger"], "user_command");
    assert_eq!(pending_user_escalation_request(&session), None);
    let advice = session
        .messages
        .iter()
        .find(|message| message.content.starts_with(ESCALATION_ADVICE_PREFIX))
        .expect("advice message");
    // The escalation model's output keeps its model provenance; the worker is
    // then resumed by a runtime continuation, never a human-origin message.
    assert_eq!(
        session.message_origin(advice.index),
        MessageOrigin::ModelOutput
    );
    assert_eq!(
        session.message_origin(advice.index + 1),
        MessageOrigin::RuntimeContinuation
    );
}

#[test]
fn escalate_command_records_a_request_only_when_configured() {
    use crate::interactive::{
        command_effect_class, execute_interactive_command, parse_interactive_command,
        CommandEffectClass, InteractiveCommand, InteractiveEffect,
    };
    let command = parse_interactive_command("/escalate why does the parser fail?")
        .expect("parse")
        .expect("command");
    assert_eq!(
        command,
        InteractiveCommand::Escalate {
            note: Some("why does the parser fail?".to_string())
        }
    );
    // Allowed while a run is active; the run picks it up at a safe boundary.
    assert_eq!(command_effect_class(&command), CommandEffectClass::Always);

    let off = fixture("http://127.0.0.1:9/v1", None, 2);
    let output = execute_interactive_command(
        command.clone(),
        off.root.path(),
        &off.store,
        &off.session_id,
    )
    .expect("execute");
    assert!(
        matches!(&output, InteractiveEffect::Output(text) if text.starts_with("Escalation is off"))
    );
    let session = off.store.load(&off.session_id).expect("session");
    assert_eq!(pending_user_escalation_request(&session), None);

    let on = fixture("http://127.0.0.1:9/v1", Some(ESCALATION_MODEL), 2);
    let output = execute_interactive_command(command, on.root.path(), &on.store, &on.session_id)
        .expect("execute");
    assert!(
        matches!(&output, InteractiveEffect::Output(text) if text.starts_with("Escalation requested (openai/escalation-model"))
    );
    let session = on.store.load(&on.session_id).expect("session");
    assert_eq!(
        pending_user_escalation_request(&session).map(|(_, note)| note),
        Some("why does the parser fail?".to_string())
    );
    let status = crate::interactive::execute_interactive_command(
        InteractiveCommand::Status,
        on.root.path(),
        &on.store,
        &on.session_id,
    )
    .expect("status");
    assert!(
        matches!(&status, InteractiveEffect::Output(text) if text.contains("Escalation: openai/escalation-model") && text.contains("/escalate pending"))
    );
}
