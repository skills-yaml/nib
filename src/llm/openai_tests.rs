use super::*;
use crate::llm::test_support::{serve_once, serve_once_with_declared_length, serve_open_stream};
use std::time::Duration;

#[test]
fn chat_continuation_encodes_a_typed_tool_result() {
    let scope = LlmRequestScope::new("session", "run").unwrap();
    let call = ToolCallRequest::with_provider_call(
        ProviderCallId::new("call_typed").unwrap(),
        "inspect",
        json!({}),
    );
    let assistant_message = json!({
        "role": "assistant",
        "tool_calls": [{
            "id": "call_typed",
            "type": "function",
            "function": {"name": "inspect", "arguments": "{}"}
        }]
    });
    let mut continuation = chat_continuation(
        "openai",
        "gpt-test",
        Some(scope.clone()),
        assistant_message,
        std::slice::from_ref(&call),
    )
    .unwrap();
    continuation
        .record_tool_result(ToolResult::success(call.invocation_id, json!({"value": 1})).unwrap())
        .unwrap();

    let messages = into_chat_messages(continuation, "openai", "gpt-test", Some(&scope)).unwrap();
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "call_typed");
    assert_eq!(messages[1]["content"], "{\"value\":1}");
}

#[test]
fn parses_streamed_text_and_tool_fragments() {
    let events = parse_openai_stream_chunk(&json!({
        "choices": [{
            "delta": {
                "content": "working",
                "tool_calls": [{
                    "index": 0,
                    "function": {"name": "read_file", "arguments": "{\"path\":"}
                }]
            },
            "finish_reason": null
        }]
    }))
    .unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0],
        LlmStreamEvent::Delta(LlmDelta::Content(value)) if value == "working"
    ));
    assert!(matches!(
        &events[1],
        LlmStreamEvent::Delta(LlmDelta::ToolCallChunk {
            index: 0,
            name: Some(name),
            ..
        }) if name == "read_file"
    ));
}

#[test]
fn complete_parser_rejects_unsafe_and_unknown_terminal_states() {
    for (finish_reason, expected_detail) in [
        ("length", "truncated"),
        ("content_filter", "safety controls"),
        ("function_call", "unsupported finish_reason"),
        ("remote-secret-terminal", "unsupported finish_reason"),
        ("error", "in-band error"),
    ] {
        let error = parse_openai_response(&json!({
            "choices": [{
                "message": {"content": "not authorized"},
                "finish_reason": finish_reason
            }]
        }))
        .expect_err("unsafe terminal state must fail");
        assert!(error.contains(expected_detail));
        assert!(!error.contains("remote-secret-terminal"));
    }

    let missing = parse_openai_response(&json!({
        "choices": [{"message": {"content": "not authorized"}}]
    }))
    .expect_err("missing terminal state must fail");
    assert!(missing.contains("missing finish_reason"));
}

#[test]
fn complete_and_stream_parsers_reject_provider_errors_and_refusals() {
    for response in [
        json!({
            "error": {"message": "remote-secret-error"},
            "choices": []
        }),
        json!({
            "choices": [{
                "error": {"message": "remote-secret-error"},
                "message": {"content": "not authorized"},
                "finish_reason": "stop"
            }]
        }),
        json!({
            "choices": [{
                "message": {
                    "content": null,
                    "refusal": "remote-secret-refusal"
                },
                "finish_reason": "stop"
            }]
        }),
    ] {
        let complete_error = parse_openai_response(&response)
            .expect_err("provider failure must not become a completed response");
        let stream_error = parse_openai_stream_chunk(&response)
            .expect_err("provider failure must not become a stream event");
        for error in [complete_error, stream_error] {
            assert!(!error.contains("remote-secret"));
            assert!(error.contains("in-band error") || error.contains("refusal"));
        }
    }
}

#[test]
fn chat_request_construction_is_provider_and_model_name_agnostic() {
    let messages = [LlmMessage::user("inspect")];
    let tools = [ToolDefinition::function("read_file")];
    for (provider, model, base_url, expected_endpoint) in [
        (
            "openai",
            "gpt-3.5-legacy",
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/chat/completions",
        ),
        (
            "openai",
            "gateway-unknown-model",
            "https://gateway.example/compatible/v1",
            "https://gateway.example/compatible/v1/chat/completions",
        ),
        (
            "openrouter",
            "unrecognized/model",
            "https://openrouter.ai/api/v1",
            "https://openrouter.ai/api/v1/chat/completions",
        ),
    ] {
        let client = OpenAiCompatClient::configured(
            provider.to_string(),
            model.to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            None,
        );
        assert_eq!(client.endpoint(), expected_endpoint);
        let body = client
            .request_body(
                LlmRequest::new(&messages, Some(&tools)).with_max_output_tokens(37),
                false,
            )
            .expect("valid Chat request");
        assert_eq!(body["model"], model);
        assert!(body.get("messages").is_some());
        assert!(body.get("tools").is_some());
        assert!(body.get("input").is_none());
        assert!(body.get("store").is_none());
        assert!(body.get("include").is_none());
        if provider == "openai" {
            assert_eq!(body["max_completion_tokens"], 37);
            assert!(body.get("max_tokens").is_none());
        } else {
            assert_eq!(body["max_tokens"], 37);
            assert!(body.get("max_completion_tokens").is_none());
        }
    }
}

#[tokio::test]
async fn complete_posts_chat_payload_and_parses_tool_calls() {
    let (base_url, request_rx) = serve_once(
        "200 OK",
        "application/json",
        json!({
            "choices": [{
                "message": {
                    "content": "inspected",
                    "tool_calls": [{
                        "id": "call_read_1",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"README.md\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "total_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 4},
                "completion_tokens_details": {"reasoning_tokens": 3}
            }
        })
        .to_string(),
    );
    let client = OpenAiCompatClient::new(
        "test-model".to_string(),
        vec!["test-key".to_string()],
        base_url,
    );
    let response = client
        .complete(
            LlmRequest::new(
                &[LlmMessage::user("inspect")],
                Some(&[ToolDefinition::function("read_file")]),
            )
            .with_scope(
                crate::llm::types::LlmRequestScope::new("test-session", "test-run")
                    .expect("test scope"),
            ),
        )
        .await
        .expect("OpenAI completion");

    assert_eq!(response.content.as_deref(), Some("inspected"));
    assert_eq!(response.finish_reason, LlmFinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Some(LlmUsage::new(12, 8, 20, Some(4), Some(3)).unwrap())
    );
    let call = &response.tool_calls.expect("tool call")[0];
    assert_eq!(call.call_id.as_ref().unwrap().as_str(), "call_read_1");
    assert_eq!(call.name, "read_file");
    assert_eq!(call.arguments["path"], "README.md");

    let request = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured request");
    assert!(request.starts_with("POST /chat/completions HTTP/1.1"));
    assert!(request
        .to_ascii_lowercase()
        .contains("authorization: bearer test-key"));
    assert!(request.contains("\"model\":\"test-model\""));
    assert!(request.contains("\"tool_choice\":\"auto\""));
}

#[test]
fn complete_usage_rejects_malformed_fractional_negative_and_overflow_counts() {
    for usage in [
        json!({"prompt_tokens": 1.5, "completion_tokens": 2, "total_tokens": 3}),
        json!({"prompt_tokens": -1, "completion_tokens": 2, "total_tokens": 1}),
        json!({"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 4}),
        json!({"prompt_tokens": 1_000_000_001_u64, "completion_tokens": 0, "total_tokens": 1_000_000_001_u64}),
    ] {
        let error = parse_openai_response(&json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
            "usage": usage,
        }))
        .expect_err("malformed usage must fail the completion");
        assert!(error.contains("usage"), "{error}");
    }
}

#[tokio::test]
async fn http_200_error_envelope_is_safe_and_keeps_reasoning_tool_guidance() {
    let remote_secret = "remote-provider-detail-secret";
    let (base_url, _) = serve_once(
        "200 OK",
        "application/json",
        json!({
            "error": {
                "message": format!(
                    "Function tools with reasoning_effort failed: {remote_secret}"
                ),
                "param": "reasoning_effort"
            }
        })
        .to_string(),
    );
    let client = OpenAiCompatClient::configured(
        "openrouter".to_string(),
        "provider/model".to_string(),
        vec!["fixture-key".to_string()],
        base_url,
        Some(ReasoningEffort::Medium),
    );
    let messages = [LlmMessage::user("plan")];
    let tools = [
        ToolDefinition::new("submit_plan", "", json!({"type": "object"}))
            .expect("submit_plan tool"),
    ];

    let error = client
        .complete(LlmRequest::new(&messages, Some(&tools)))
        .await
        .expect_err("HTTP-200 provider error must fail");

    assert_eq!(error.class, crate::llm::LlmErrorClass::ProviderRejected);
    assert_eq!(error.phase, crate::llm::LlmErrorPhase::TerminalValidation);
    assert!(error.contains("provider failure"));
    assert!(error.contains("in-band error"));
    assert!(error.contains("api = \"responses\""));
    assert!(error.contains("reasoning_effort = \"none\""));
    assert!(!error.contains(remote_secret));
}

#[tokio::test]
async fn chat_reasoning_tool_rejection_is_actionable_redacted_and_not_retried() {
    let (base_url, request_rx) = serve_once(
            "400 Bad Request",
            "application/json",
            json!({
                "error": {
                    "message": "Function tools with reasoning_effort are not supported for gpt-5.6-luna in /v1/chat/completions; credential incident-secret",
                    "type": "invalid_request_error",
                    "param": "reasoning_effort"
                }
            })
            .to_string(),
        );
    let client = OpenAiCompatClient::configured(
        "openai".to_string(),
        "gpt-5.6-luna".to_string(),
        vec!["incident-secret".to_string()],
        format!("{base_url}/v1"),
        Some(ReasoningEffort::Medium),
    );
    let messages = [LlmMessage::user("plan")];
    let tools = [
        ToolDefinition::new("submit_plan", "", json!({"type": "object"}))
            .expect("submit_plan tool"),
    ];

    let error = client
        .complete(LlmRequest::new(&messages, Some(&tools)))
        .await
        .expect_err("reported Chat tuple must fail");

    assert!(error.contains("openai Chat Completions API error"));
    assert!(error.contains("model gpt-5.6-luna"));
    assert!(error.contains("api = \"responses\""));
    assert!(error.contains("reasoning_effort = \"none\""));
    assert!(!error.contains("incident-secret"));
    let request = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("single captured request");
    assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(request.contains("\"reasoning_effort\":\"medium\""));
    assert!(request.contains("\"tools\""));
    assert!(!request.contains("\"input\""));
    assert!(!request.contains("\"store\""));
}

#[tokio::test]
async fn stream_posts_stream_flag_and_accumulates_text_and_tools() {
    let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"work\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_private_chat\",\"function\":{\"name\":\"grep\",\"arguments\":\"{\\\"pattern\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"needle\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
    let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", body);
    let client = OpenAiCompatClient::new(
        "stream-model".to_string(),
        vec!["stream-key".to_string()],
        base_url,
    );
    let mut stream = client
        .stream(
            LlmRequest::new(&[LlmMessage::user("search")], None).with_scope(
                crate::llm::types::LlmRequestScope::new("test-session", "test-run")
                    .expect("test scope"),
            ),
        )
        .await
        .expect("OpenAI stream");
    let mut content = String::new();
    let mut accumulator = crate::llm::ToolCallAccumulator::default();
    let mut finish = None;
    while let Some(event) = stream.recv_private().await {
        let event = event.expect("valid stream event");
        if let LlmStreamEvent::Delta(LlmDelta::Content(fragment)) = &event {
            content.push_str(fragment);
        }
        if let LlmStreamEvent::Terminal(reason) = &event {
            finish = Some(*reason);
        }
        if let LlmStreamEvent::Delta(delta) = &event {
            accumulator.push(delta);
        }
    }

    assert_eq!(content, "work");
    assert_eq!(finish, Some(LlmFinishReason::ToolCalls));
    let calls = accumulator.finish().expect("complete tool call");
    assert!(
        calls[0].call_id.is_none(),
        "public projection exposed a call ID"
    );
    assert_eq!(calls[0].name, "grep");
    assert_eq!(calls[0].arguments["pattern"], "needle");
    let private = stream.finish().await.expect("private terminal completion");
    let private_call = &private.tool_calls.expect("private tool call")[0];
    assert_eq!(
        private_call.call_id.as_ref().unwrap().as_str(),
        "call_private_chat"
    );
    let request = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured stream request");
    assert!(request.contains("\"stream\":true"));
    assert!(!request.contains("\"stream_options\""));
}

#[tokio::test]
async fn canonical_chat_stream_waits_for_optional_usage_and_done_without_public_events() {
    for (usage_chunk, expected_usage) in [
            (
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4,\"total_tokens\":13,\"prompt_tokens_details\":{\"cached_tokens\":2},\"completion_tokens_details\":{\"reasoning_tokens\":1}}}\n\n",
                Some(LlmUsage::new(9, 4, 13, Some(2), Some(1)).unwrap()),
            ),
            ("", None),
        ] {
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"ok\"}},\"finish_reason\":null}}]}}\n\n\
                 data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
                 {usage_chunk}data: [DONE]\n\n"
            );
            let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", body);
            let client = OpenAiCompatClient::configured(
                "openai".to_string(),
                "stream-model".to_string(),
                vec!["stream-key".to_string()],
                base_url,
                None,
            );
            let messages = [LlmMessage::user("stream")];
            let mut stream = client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect("canonical OpenAI stream");
            assert!(matches!(
                stream.recv_private().await,
                Some(Ok(LlmStreamEvent::Delta(LlmDelta::Content(content)))) if content == "ok"
            ));
            assert!(matches!(
                stream.recv_private().await,
                Some(Ok(LlmStreamEvent::Terminal(LlmFinishReason::Complete)))
            ));
            assert!(stream.recv_private().await.is_none(), "event followed public End");
            let completion = stream.finish().await.expect("private completion");
            assert_eq!(completion.usage, expected_usage);

            let request = request_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("captured canonical stream request");
            assert!(request.contains("\"stream_options\":{\"include_usage\":true}"));
        }
}

#[tokio::test]
async fn malformed_usage_after_chat_end_fails_privately_without_a_post_end_event() {
    let body = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":\"bad\",\"completion_tokens\":1,\"total_tokens\":1}}\n\n",
            "data: [DONE]\n\n"
        );
    let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
    let client = OpenAiCompatClient::configured(
        "openai".to_string(),
        "stream-model".to_string(),
        vec!["stream-key".to_string()],
        base_url,
        None,
    );
    let messages = [LlmMessage::user("stream")];
    let mut stream = client
        .stream(LlmRequest::new(&messages, None))
        .await
        .expect("canonical OpenAI stream");
    assert!(matches!(
        stream.recv_private().await,
        Some(Ok(LlmStreamEvent::Terminal(LlmFinishReason::Complete)))
    ));
    assert!(
        stream.recv_private().await.is_none(),
        "error followed public End"
    );
    let error = stream
        .finish()
        .await
        .expect_err("malformed terminal usage must fail");
    assert_eq!(error.class, crate::llm::LlmErrorClass::Protocol);
    assert!(error.contains("usage"));
}

#[tokio::test]
async fn streaming_in_band_errors_fail_without_remote_detail() {
    let remote_secret = "remote-stream-error-secret";
    for body in [
            format!(
                "data: {{\"error\":{{\"message\":\"{remote_secret}\"}}}}\n\n"
            ),
            format!(
                "data: {{\"choices\":[{{\"error\":{{\"message\":\"{remote_secret}\"}},\"delta\":{{}},\"finish_reason\":null}}]}}\n\n"
            ),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\"}]}\n\n"
                .to_string(),
            format!("event: error\ndata: {{\"message\":\"{remote_secret}\"}}\n\n"),
        ] {
            let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
            let client = OpenAiCompatClient::configured(
                "openrouter".to_string(),
                "provider/model".to_string(),
                vec!["fixture-key".to_string()],
                base_url,
                None,
            );
            let messages = [LlmMessage::user("stream")];
            let error = client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect("HTTP stream response")
                .finish()
                .await
                .expect_err("in-band stream error must fail");
            assert_eq!(error.class, crate::llm::LlmErrorClass::ProviderRejected);
            assert_eq!(error.phase, crate::llm::LlmErrorPhase::Stream);
            assert!(error.contains("in-band error"));
            assert!(!error.contains(remote_secret));
        }
}

#[tokio::test]
async fn streaming_reasoning_tool_error_keeps_only_local_guidance() {
    let remote_secret = "remote-stream-reasoning-secret";
    let body = format!(
            "data: {{\"error\":{{\"message\":\"Function tools with reasoning_effort failed: {remote_secret}\",\"param\":\"reasoning_effort\"}}}}\n\n"
        );
    let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
    let client = OpenAiCompatClient::configured(
        "openrouter".to_string(),
        "provider/model".to_string(),
        vec!["fixture-key".to_string()],
        base_url,
        Some(ReasoningEffort::Medium),
    );
    let messages = [LlmMessage::user("stream")];
    let tools = [
        ToolDefinition::new("submit_plan", "", json!({"type": "object"}))
            .expect("submit_plan tool"),
    ];
    let error = client
        .stream(LlmRequest::new(&messages, Some(&tools)))
        .await
        .expect("HTTP stream response")
        .finish()
        .await
        .expect_err("reasoning/tool error must fail");

    assert_eq!(error.class, crate::llm::LlmErrorClass::ProviderRejected);
    assert_eq!(error.phase, crate::llm::LlmErrorPhase::Stream);
    assert!(error.contains("api = \"responses\""));
    assert!(error.contains("reasoning_effort = \"none\""));
    assert!(!error.contains(remote_secret));
}

#[tokio::test]
async fn streaming_unsafe_and_unknown_terminal_states_fail_closed() {
    for (finish_reason, expected_detail) in [
        ("length", "truncated"),
        ("content_filter", "safety controls"),
        ("remote-secret-terminal", "unsupported finish_reason"),
    ] {
        let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"partial\"}},\"finish_reason\":\"{finish_reason}\"}}]}}\n\n"
            );
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let client = OpenAiCompatClient::new(
            "stream-model".to_string(),
            vec!["stream-key".to_string()],
            base_url,
        );
        let messages = [LlmMessage::user("stream")];
        let error = client
            .stream(LlmRequest::new(&messages, None))
            .await
            .expect("HTTP stream response")
            .finish()
            .await
            .expect_err("unsafe terminal state must fail");
        assert!(error.contains(expected_detail));
        assert!(!error.contains("remote-secret-terminal"));
    }
}

#[tokio::test]
async fn dropping_stream_receiver_closes_the_open_http_response() {
    let (base_url, _request_rx, disconnect_rx) = serve_open_stream(
        "data: {\"choices\":[{\"delta\":{\"content\":\"first\"},\"finish_reason\":null}]}\n\n",
    );
    let client = OpenAiCompatClient::new(
        "stream-model".to_string(),
        vec!["stream-key".to_string()],
        base_url,
    );
    let mut stream = client
        .stream(LlmRequest::new(&[LlmMessage::user("cancel")], None))
        .await
        .expect("open OpenAI stream");
    let first = tokio::time::timeout(Duration::from_secs(1), stream.recv_private())
        .await
        .expect("first event timeout")
        .expect("first event")
        .expect("valid first event");
    assert_eq!(
        first,
        LlmStreamEvent::Delta(LlmDelta::Content("first".to_string()))
    );
    drop(stream);

    let disconnected =
        tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .expect("disconnect observer")
            .expect("disconnect signal");
    assert!(disconnected, "OpenAI response connection remained open");
}

#[tokio::test]
async fn done_sentinel_without_finish_reason_fails_and_closes_open_response() {
    let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
        "data: [DONE]\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"late\"},\"finish_reason\":null}]}\n\n"
    ));
    let client = OpenAiCompatClient::new(
        "stream-model".to_string(),
        vec!["stream-key".to_string()],
        base_url,
    );
    let mut stream = client
        .stream(LlmRequest::new(&[LlmMessage::user("finish")], None))
        .await
        .expect("open OpenAI stream");

    let error = tokio::time::timeout(Duration::from_secs(1), stream.recv_private())
        .await
        .expect("terminal event timeout")
        .expect("terminal event")
        .expect_err("DONE without finish_reason must fail");
    assert!(error.contains("explicit finish_reason"));
    assert!(
        tokio::time::timeout(Duration::from_secs(1), stream.recv_private())
            .await
            .expect("producer closure timeout")
            .is_none()
    );

    let disconnected =
        tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .expect("disconnect observer")
            .expect("disconnect signal");
    assert!(disconnected, "OpenAI terminal event left response open");
}

#[tokio::test]
async fn done_sentinel_without_finish_reason_cannot_authorize_completion() {
    let (base_url, _) = serve_once("200 OK", "text/event-stream", "data: [DONE]\n\n");
    let client = OpenAiCompatClient::new(
        "stream-model".to_string(),
        vec!["stream-key".to_string()],
        base_url,
    );
    let stream = client
        .stream(LlmRequest::new(&[LlmMessage::user("finish")], None))
        .await
        .expect("OpenAI stream");
    let error = stream
        .finish()
        .await
        .expect_err("typed finish reason required");
    assert!(error.contains("explicit finish_reason"));
}

#[tokio::test]
async fn partial_tool_delta_followed_by_eof_cannot_authorize_a_tool() {
    let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_partial\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"unsafe.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n"
        );
    let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
    let client = OpenAiCompatClient::new(
        "stream-model".to_string(),
        vec!["stream-key".to_string()],
        base_url,
    );
    let messages = [LlmMessage::user("write")];
    let stream = client
        .stream(LlmRequest::new(&messages, None))
        .await
        .expect("OpenAI stream");
    let error = stream
        .finish()
        .await
        .expect_err("partial public tool delta must not authorize execution");
    assert!(error.contains("explicit finish_reason"));
}

#[tokio::test]
async fn complete_and_stream_surface_http_errors() {
    let (base_url, _) = serve_once("401 Unauthorized", "text/plain", "bad key");
    let client = OpenAiCompatClient::new("model".to_string(), vec!["bad".to_string()], base_url);
    let error = client
        .complete(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect_err("completion must reject HTTP errors");
    assert!(error.contains("Chat Completions API error HTTP 401"));
    assert_eq!(error.class, crate::llm::LlmErrorClass::Authentication);
    assert_eq!(error.phase, crate::llm::LlmErrorPhase::HttpResponse);
    assert_eq!(error.http_status, Some(401));
    assert!(!error.contains("bad key"));

    let (base_url, _) = serve_once("403 Forbidden", "text/plain", "forbidden");
    let client = OpenAiCompatClient::new("model".to_string(), vec!["bad".to_string()], base_url);
    let error = client
        .stream(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect_err("stream must reject HTTP errors");
    assert!(error.contains("Chat Completions API error HTTP 403"));
    assert_eq!(error.class, crate::llm::LlmErrorClass::Authentication);
    assert_eq!(error.phase, crate::llm::LlmErrorPhase::HttpResponse);
    assert_eq!(error.http_status, Some(403));
    assert!(!error.contains("forbidden"));
}

#[tokio::test]
async fn every_openai_compatible_provider_keeps_complete_and_stream_error_classes_equal() {
    for provider in ["openai", "grok", "openrouter", "meta"] {
        let (base_url, _) = serve_once(
            "401 Unauthorized",
            "application/json",
            json!({"error": {"code": "invalid_api_key", "message": "private"}}).to_string(),
        );
        let client = OpenAiCompatClient::configured(
            provider.to_string(),
            "fixture-model".to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            None,
        );
        let messages = [LlmMessage::user("inspect")];
        let complete_error = client
            .complete(LlmRequest::new(&messages, None))
            .await
            .expect_err("completion authentication error");

        let (base_url, _) = serve_once(
            "401 Unauthorized",
            "application/json",
            json!({"error": {"code": "invalid_api_key", "message": "private"}}).to_string(),
        );
        let client = OpenAiCompatClient::configured(
            provider.to_string(),
            "fixture-model".to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            None,
        );
        let stream_error = client
            .stream(LlmRequest::new(&messages, None))
            .await
            .expect_err("stream authentication error");

        assert_eq!(
            complete_error.class,
            crate::llm::LlmErrorClass::Authentication
        );
        assert_eq!(stream_error.class, complete_error.class);
        assert_eq!(
            complete_error.phase,
            crate::llm::LlmErrorPhase::HttpResponse
        );
        assert_eq!(stream_error.phase, complete_error.phase);
        assert_eq!(complete_error.provider, provider);
        assert_eq!(stream_error.provider, provider);
        assert_eq!(complete_error.transport, CHAT_TRANSPORT);
        assert_eq!(stream_error.transport, CHAT_TRANSPORT);
    }
}

#[tokio::test]
async fn http_errors_omit_prompt_echo_and_redact_full_context() {
    let secret = "env-only-context-secret";
    let prompt_echo = "user-prompt-that-must-not-be-diagnostic";
    let (base_url, _) = serve_once(
            "400 Bad Request",
            "application/json",
            json!({
                "error": {
                    "type": prompt_echo,
                    "code": prompt_echo,
                    "param": format!("reasoning_effort-{prompt_echo}"),
                    "message": format!("Function tools with reasoning_effort failed: {prompt_echo}\n\u{1b}[31m")
                }
            })
            .to_string(),
        );
    let client = OpenAiCompatClient::configured(
        format!("gateway-{secret}\n"),
        format!("model-{secret}\u{1b}"),
        vec![secret.to_string()],
        format!("{base_url}/{secret}"),
        Some(ReasoningEffort::Medium),
    );
    let messages = [LlmMessage::user(prompt_echo)];
    let tools = [
        ToolDefinition::new("submit_plan", "", json!({"type": "object"}))
            .expect("submit_plan tool"),
    ];
    let error = client
        .complete(LlmRequest::new(&messages, Some(&tools)))
        .await
        .expect_err("HTTP error");
    assert!(error.contains("[REDACTED]"));
    assert!(error.contains("api = \"responses\""));
    assert!(!error.contains(secret));
    assert!(!error.contains(prompt_echo));
    assert!(!error.contains('\n'));
    assert!(!error.contains('\u{1b}'));
    assert!(error.len() <= MAX_DIAGNOSTIC_BYTES);
}

#[tokio::test]
async fn oversized_http_errors_keep_chat_context() {
    let (base_url, _) = serve_once(
        "400 Bad Request",
        "text/plain",
        "x".repeat(crate::llm::MAX_LLM_ERROR_RESPONSE_BYTES + 1),
    );
    let client = OpenAiCompatClient::configured(
        "openai".to_string(),
        "gpt-test".to_string(),
        vec!["key".to_string()],
        base_url,
        None,
    );
    let messages = [LlmMessage::user("inspect")];
    let error = client
        .complete(LlmRequest::new(&messages, None))
        .await
        .expect_err("oversized provider error");
    assert!(error.contains("openai Chat Completions API error HTTP 400"));
    assert!(error.contains("65536-byte limit"));
    assert!(error.len() <= MAX_DIAGNOSTIC_BYTES);
}

#[tokio::test]
async fn oversized_declared_response_lengths_are_rejected() {
    let (base_url, _) = serve_once_with_declared_length(
        "200 OK",
        "application/json",
        "{}",
        Some(crate::llm::MAX_LLM_COMPLETE_RESPONSE_BYTES + 1),
    );
    let client = OpenAiCompatClient::new("model".to_string(), vec!["key".to_string()], base_url);
    let error = client
        .complete(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect_err("oversized completion must be rejected");
    assert!(error.contains("4194304-byte limit"));

    let (base_url, _) = serve_once_with_declared_length(
        "200 OK",
        "text/event-stream",
        "",
        Some(crate::llm::MAX_LLM_STREAM_BYTES + 1),
    );
    let client = OpenAiCompatClient::new("model".to_string(), vec!["key".to_string()], base_url);
    let error = client
        .stream(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect_err("oversized stream must be rejected");
    assert!(error.contains("16777216-byte limit"));
}

#[test]
fn chat_request_encodes_required_tool_choice() {
    let client = OpenAiCompatClient::configured(
        "openai".to_string(),
        "gpt-5.6-sol".to_string(),
        vec!["test-key".to_string()],
        "https://api.openai.com/v1",
        None,
    );
    let messages = [LlmMessage::user("call")];
    let tools = [ToolDefinition::function("record_probe")];
    let body = client
        .request_body(
            LlmRequest::new(&messages, Some(&tools)).with_tool_choice(ToolChoice::Required),
            false,
        )
        .expect("valid Chat request");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["tools"][0]["function"]["strict"], false);
}

#[test]
fn compatible_chat_tools_omit_strict_and_meta_keeps_auto_choice() {
    let messages = [LlmMessage::user("call")];
    let tools = [ToolDefinition::function("record_probe").with_strict(true)];
    let grok = OpenAiCompatClient::configured(
        "grok".to_string(),
        "grok-4.5".to_string(),
        vec!["test-key".to_string()],
        "https://api.x.ai/v1",
        None,
    )
    .request_body(
        LlmRequest::new(&messages, Some(&tools)).with_tool_choice(ToolChoice::Required),
        false,
    )
    .expect("valid Grok Chat request");
    assert_eq!(grok["tools"][0]["function"]["strict"], true);
    assert_eq!(grok["tool_choice"], "required");

    let openrouter_claude = OpenAiCompatClient::configured(
        "openrouter".to_string(),
        "anthropic/claude-opus-5".to_string(),
        vec!["test-key".to_string()],
        "https://openrouter.ai/api/v1",
        None,
    )
    .request_body(
        LlmRequest::new(&messages, Some(&tools)).with_tool_choice(ToolChoice::Required),
        false,
    )
    .expect("valid OpenRouter Chat request");
    assert!(openrouter_claude["tools"][0]["function"]
        .get("strict")
        .is_none());
    assert!(openrouter_claude.get("tool_choice").is_none());
    assert!(openrouter_claude["tools"][0]["function"]["parameters"]
        .get("additionalProperties")
        .is_none());

    let meta = OpenAiCompatClient::configured(
        "meta".to_string(),
        "muse-spark-1.1".to_string(),
        vec!["test-key".to_string()],
        "https://api.meta.ai/v1",
        None,
    )
    .request_body(
        LlmRequest::new(&messages, Some(&tools)).with_tool_choice(ToolChoice::Required),
        false,
    )
    .expect("valid Meta Chat request");
    assert!(meta["tools"][0]["function"].get("strict").is_none());
    assert!(meta.get("tool_choice").is_none());
}

#[test]
fn boolean_false_refusal_is_not_a_provider_refusal() {
    let response = parse_openai_response(&json!({
        "choices": [{
            "message": {"content": "ok", "refusal": false},
            "finish_reason": "stop"
        }]
    }))
    .expect("false refusal is an absent refusal");
    assert_eq!(response.content.as_deref(), Some("ok"));
    assert_eq!(response.finish_reason, LlmFinishReason::Complete);
}

#[test]
fn chat_parsers_read_array_content_parts() {
    let complete = parse_openai_response(&json!({
        "choices": [{
            "message": {
                "content": [
                    {"type": "text", "text": "NIB_"},
                    {"type": "text", "text": "ok"}
                ]
            },
            "finish_reason": "stop"
        }]
    }))
    .expect("array content");
    assert_eq!(complete.content.as_deref(), Some("NIB_ok"));

    let events = parse_openai_stream_chunk(&json!({
        "choices": [{
            "delta": {
                "content": [{"type": "text", "text": "NIB_ok"}]
            },
            "finish_reason": null
        }]
    }))
    .expect("array stream content");
    assert!(matches!(
        &events[0],
        LlmStreamEvent::Delta(LlmDelta::Content(value)) if value == "NIB_ok"
    ));

    let message_events = parse_openai_stream_chunk(&json!({
        "choices": [{
            "message": {"content": "NIB_ok"},
            "finish_reason": "stop"
        }]
    }))
    .expect("message-shaped stream chunk");
    assert!(matches!(
        &message_events[0],
        LlmStreamEvent::Delta(LlmDelta::Content(value)) if value == "NIB_ok"
    ));
}

#[test]
fn complete_usage_counts_separate_reasoning_tokens_toward_output() {
    let response = parse_openai_response(&json!({
        "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 37,
            "completion_tokens": 530,
            "total_tokens": 800,
            "completion_tokens_details": {"reasoning_tokens": 233}
        }
    }))
    .expect("xAI-style usage with separate reasoning tokens");
    assert_eq!(
        response.usage,
        Some(LlmUsage::new(37, 763, 800, None, Some(233)).unwrap())
    );
}
