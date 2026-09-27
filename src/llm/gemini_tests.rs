use super::*;
use crate::config::ReasoningEffort;
use crate::llm::test_support::{serve_once, serve_once_with_declared_length, serve_open_stream};
use crate::llm::types::{LlmRequestScope, ProviderContinuation, ToolChoice};
use std::time::Duration;

fn test_client(base_url: String) -> GeminiClient {
    GeminiClient::with_base_url(
        "gemini-test".to_string(),
        vec!["gemini-test-key".to_string()],
        base_url,
    )
    .expect("test Gemini endpoint")
}

#[test]
fn gemini_continuation_emits_structured_typed_tool_results() {
    let scope = LlmRequestScope::new("test-session", "test-run").unwrap();
    let call = ToolCallRequest::new("inspect", json!({}));
    let model_content = json!({
        "role": "model",
        "parts": [{"functionCall": {"name": "inspect", "args": {}}}],
    });
    let mut continuation = gemini_continuation(
        "gemini-test",
        Some(scope.clone()),
        model_content,
        std::slice::from_ref(&call),
    )
    .unwrap();
    continuation
        .record_tool_result(ToolResult::success(call.invocation_id, json!({"value": 1})).unwrap())
        .unwrap();

    let contents = into_gemini_contents(continuation, "gemini-test", Some(&scope)).unwrap();
    assert_eq!(
        contents[1]["parts"][0]["functionResponse"]["name"],
        "inspect"
    );
    assert_eq!(
        contents[1]["parts"][0]["functionResponse"]["response"]["value"],
        1
    );
}

#[test]
fn idless_parallel_continuation_retains_thought_signatures_and_exact_order() {
    let scope = LlmRequestScope::new("test-session", "parallel-run").unwrap();
    let calls = [
        ToolCallRequest::new("first", json!({"slot": 1})),
        ToolCallRequest::new("second", json!({"slot": 2})),
    ];
    let model_content = json!({
        "role": "model",
        "parts": [
            {"functionCall": {"name": "first", "args": {"slot": 1}},
             "thoughtSignature": "private-signature-one"},
            {"functionCall": {"name": "second", "args": {"slot": 2}},
             "thoughtSignature": "private-signature-two"}
        ]
    });
    let mut continuation =
        gemini_continuation("gemini-test", Some(scope.clone()), model_content, &calls)
            .expect("parallel Gemini continuation");
    continuation
        .record_tool_result(
            ToolResult::success(calls[0].invocation_id, json!({"receipt": 1})).unwrap(),
        )
        .unwrap();
    continuation
        .record_tool_result(
            ToolResult::success(calls[1].invocation_id, json!({"receipt": 2})).unwrap(),
        )
        .unwrap();

    let contents = into_gemini_contents(continuation, "gemini-test", Some(&scope)).unwrap();
    assert_eq!(
        contents[0]["parts"][0]["thoughtSignature"],
        "private-signature-one"
    );
    assert_eq!(
        contents[0]["parts"][1]["thoughtSignature"],
        "private-signature-two"
    );
    assert_eq!(contents[1]["parts"][0]["functionResponse"]["name"], "first");
    assert_eq!(
        contents[1]["parts"][0]["functionResponse"]["response"]["receipt"],
        1
    );
    assert_eq!(
        contents[1]["parts"][1]["functionResponse"]["name"],
        "second"
    );
    assert_eq!(
        contents[1]["parts"][1]["functionResponse"]["response"]["receipt"],
        2
    );
    let public = format!("{:?}", contents[1]);
    assert!(!public.contains("private-signature"));
}

#[test]
fn idless_parallel_continuation_rejects_order_count_and_signature_mismatch() {
    let scope = LlmRequestScope::new("test-session", "parallel-run").unwrap();
    let calls = [
        ToolCallRequest::new("first", json!({})),
        ToolCallRequest::new("second", json!({})),
    ];
    let model_content = json!({
        "role": "model",
        "parts": [
            {"functionCall": {"name": "first", "args": {}}, "thoughtSignature": "sig-a"},
            {"functionCall": {"name": "second", "args": {}}, "thoughtSignature": "sig-b"}
        ]
    });
    let reversed = [calls[1].clone(), calls[0].clone()];
    assert!(gemini_continuation(
        "gemini-test",
        Some(scope.clone()),
        model_content.clone(),
        &reversed,
    )
    .expect_err("order mismatch")
    .contains("order"));
    assert!(gemini_continuation(
        "gemini-test",
        Some(scope.clone()),
        model_content.clone(),
        &calls[..1],
    )
    .expect_err("count mismatch")
    .contains("order"));
    let mut result_order = gemini_continuation(
        "gemini-test",
        Some(scope.clone()),
        model_content.clone(),
        &calls,
    )
    .expect("valid ordered continuation");
    assert!(result_order
        .record_tool_result(
            ToolResult::success(calls[1].invocation_id, json!({"receipt": 2})).unwrap(),
        )
        .expect_err("result order mismatch")
        .contains("out of order"));
    let mut invalid_signature = model_content;
    invalid_signature["parts"][0]["thoughtSignature"] = json!("");
    assert!(
        gemini_continuation("gemini-test", Some(scope), invalid_signature, &calls,)
            .expect_err("invalid signature")
            .contains("thought-signature")
    );
}

fn request_with_continuation(messages: &[LlmMessage]) -> LlmRequest<'_> {
    let scope = LlmRequestScope::new("test-session", "test-run").unwrap();
    let continuation = ProviderContinuation::new(
        "openai",
        "test-model",
        "responses",
        Some(scope.clone()),
        vec![crate::tools::ToolInvocationId::new()],
        1,
        0,
        (),
    )
    .unwrap();
    LlmRequest::new(messages, None)
        .with_scope(scope)
        .with_continuation(Some(continuation))
}

#[test]
fn converts_tools_to_gemini_function_declarations() {
    let declarations = gemini_function_declarations(&[json!({
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read a file",
            "parameters": {"type": "object", "required": ["path"]}
        }
    })])
    .expect("valid declarations");
    assert_eq!(declarations[0]["name"], "read_file");
    assert_eq!(declarations[0]["parameters"]["required"][0], "path");
}

#[test]
fn normalizes_and_validates_custom_gemini_api_roots() {
    let root = GeminiClient::with_base_url(
        "gemini-test".to_string(),
        vec!["test-key".to_string()],
        "https://gateway.example/proxy",
    )
    .expect("custom root");
    assert_eq!(root.base_url, "https://gateway.example/proxy/v1beta");

    let exact = GeminiClient::with_base_url(
        "gemini-test".to_string(),
        vec!["test-key".to_string()],
        "https://gateway.example/proxy/v1beta/",
    )
    .expect("exact API root");
    assert_eq!(exact.base_url, "https://gateway.example/proxy/v1beta");

    for invalid in [
        "https://user:secret@gateway.example/proxy",
        "https://gateway.example/proxy?token=secret",
        "https://gateway.example/proxy#fragment",
        "https://gateway.example/v1beta/v1beta",
        "https://gateway.example/v1beta/models/gemini-test:generateContent",
    ] {
        assert!(GeminiClient::with_base_url(
            "gemini-test".to_string(),
            vec!["test-key".to_string()],
            invalid,
        )
        .is_err());
    }
}

#[tokio::test]
async fn complete_and_stream_reject_foreign_continuations_and_unsupported_reasoning() {
    let client = test_client("http://127.0.0.1:9".to_string());
    let messages = [LlmMessage::user("x")];

    let error = client
        .complete(request_with_continuation(&messages))
        .await
        .expect_err("completion must reject a foreign provider continuation");
    assert_eq!(
        error,
        "provider continuation does not match the provider, model, API mode, session, or run"
    );
    let error = client
        .stream(request_with_continuation(&messages))
        .await
        .expect_err("stream must reject a foreign provider continuation");
    assert_eq!(
        error,
        "provider continuation does not match the provider, model, API mode, session, or run"
    );

    let reasoning_request =
        || LlmRequest::new(&messages, None).with_reasoning_effort(Some(ReasoningEffort::Medium));
    let error = client
        .complete(reasoning_request())
        .await
        .expect_err("completion must reject unsupported reasoning");
    assert_eq!(error, "Gemini requests do not support reasoning_effort");
    let error = client
        .stream(reasoning_request())
        .await
        .expect_err("stream must reject unsupported reasoning");
    assert_eq!(error, "Gemini requests do not support reasoning_effort");
}

#[test]
fn complete_and_stream_reject_malformed_tools_before_io() {
    let error = ToolDefinition::from_openai_value(&json!({
        "type": "function",
        "function": {"name": "read"}
    }))
    .expect_err("malformed tool");
    assert_eq!(error, "tool definition parameters must be an object");
}

#[test]
fn complete_parser_rejects_missing_unsafe_and_malformed_terminal_states() {
    let missing = json!({
        "candidates": [{"content": {"parts": [{"text": "partial"}]}}]
    });
    assert!(parse_gemini_response(&missing)
        .expect_err("missing finish reason")
        .contains("missing a finish reason"));

    let truncated = json!({
        "candidates": [{
            "content": {"parts": [{"text": "partial"}]},
            "finishReason": "MAX_TOKENS"
        }]
    });
    assert!(parse_gemini_response(&truncated)
        .expect_err("truncation")
        .contains("truncated"));

    let malformed_call = json!({
        "candidates": [{
            "content": {"parts": [{"functionCall": {"name": "read_file"}}]},
            "finishReason": "STOP"
        }]
    });
    assert!(parse_gemini_response(&malformed_call)
        .expect_err("missing function arguments")
        .contains("arguments must be an object"));

    let unknown = json!({
        "candidates": [{"content": {"parts": []}, "finishReason": "REMOTE_FUTURE_VALUE"}]
    });
    let error = parse_gemini_response(&unknown).expect_err("unknown finish reason");
    assert_eq!(
        error,
        "Gemini response contained an unsupported terminal reason"
    );
    assert!(!error.contains("REMOTE_FUTURE_VALUE"));
}

#[test]
fn complete_parser_maps_prompt_and_candidate_safety_blocks_to_refusal() {
    let prompt = parse_gemini_response(&json!({
        "promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}
    }))
    .expect("prompt refusal");
    assert_eq!(prompt.terminal_status, LlmTerminalStatus::Refused);
    assert_eq!(prompt.finish_reason, LlmFinishReason::Refusal);
    assert!(prompt.tool_calls.is_none());

    let candidate = parse_gemini_response(&json!({
        "candidates": [{"finishReason": "SAFETY"}]
    }))
    .expect("candidate refusal");
    assert_eq!(candidate.terminal_status, LlmTerminalStatus::Refused);
    assert!(candidate.tool_calls.is_none());
}

#[test]
fn parses_gemini_streamed_function_call() {
    let events = parse_gemini_stream_chunk(&json!({
        "candidates": [{
            "content": {"parts": [{
                "functionCall": {"name": "grep", "args": {"pattern": "needle"}}
            }]},
            "finishReason": "STOP"
        }]
    }))
    .unwrap();
    assert!(matches!(
        &events[0],
        LlmStreamEvent::Delta(LlmDelta::ToolCallChunk {
            name: Some(name),
            arguments: Some(arguments),
            ..
        })
            if name == "grep" && arguments.contains("needle")
    ));
    assert_eq!(
        events[1],
        LlmStreamEvent::Terminal(LlmFinishReason::ToolCalls)
    );
}

#[test]
fn streamed_function_calls_keep_distinct_provider_identities_across_chunks() {
    let mut parser = GeminiStreamParser::default();
    let first = parser
        .parse_chunk(&json!({
            "candidates": [{
                "content": {"parts": [{
                    "functionCall": {
                        "id": "provider-call-one",
                        "name": "read_file",
                        "args": {"path": "README.md"}
                    }
                }]}
            }]
        }))
        .expect("first Gemini chunk");
    let second = parser
        .parse_chunk(&json!({
            "candidates": [{
                "content": {"parts": [{
                    "functionCall": {
                        "id": "provider-call-two",
                        "name": "grep",
                        "args": {"pattern": "needle"}
                    }
                }]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 4,
                "thoughtsTokenCount": 3,
                "totalTokenCount": 17,
                "cachedContentTokenCount": 2
            }
        }))
        .expect("second Gemini chunk");

    assert_eq!(parser.provider_call_indexes["provider-call-one"], 0);
    assert_eq!(parser.provider_call_indexes["provider-call-two"], 1);
    let mut accumulator = crate::llm::ToolCallAccumulator::default();
    for event in first.iter().chain(&second) {
        if let LlmStreamEvent::Delta(delta) = event {
            accumulator.push(delta);
        }
    }
    let calls = accumulator.finish().expect("two Gemini function calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments["path"], "README.md");
    assert_eq!(calls[1].name, "grep");
    assert_eq!(calls[1].arguments["pattern"], "needle");
}

#[test]
fn idless_function_calls_from_separate_chunks_get_distinct_indexes() {
    let mut parser = GeminiStreamParser::default();
    let first = parser
        .parse_chunk(&json!({
            "candidates": [{"content": {"parts": [{
                "functionCall": {"name": "first", "args": {}}
            }]}}]
        }))
        .expect("first Gemini chunk");
    let second = parser
        .parse_chunk(&json!({
            "candidates": [{"content": {"parts": [{
                "functionCall": {"name": "second", "args": {}}
            }]}}]
        }))
        .expect("second Gemini chunk");

    assert!(matches!(
        first[0],
        LlmStreamEvent::Delta(LlmDelta::ToolCallChunk { index: 0, .. })
    ));
    assert!(matches!(
        second[0],
        LlmStreamEvent::Delta(LlmDelta::ToolCallChunk { index: 1, .. })
    ));
}

#[tokio::test]
async fn complete_posts_gemini_payload_and_parses_text_and_function_calls() {
    let (base_url, request_rx) = serve_once(
        "200 OK",
        "application/json",
        json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "checking"},
                    {"functionCall": {"name": "grep", "args": {"pattern": "needle"}}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 4,
                "thoughtsTokenCount": 3,
                "totalTokenCount": 17,
                "cachedContentTokenCount": 2
            }
        })
        .to_string(),
    );
    let response = test_client(base_url)
        .complete(
            LlmRequest::new(
                &[
                    LlmMessage::system("follow project rules"),
                    LlmMessage::user("search"),
                    LlmMessage::assistant("working"),
                ],
                Some(&[ToolDefinition::new(
                    "grep",
                    "search files",
                    json!({"type": "object", "required": ["pattern"]}),
                )
                .expect("grep tool")]),
            )
            .with_max_output_tokens(47)
            .with_scope(LlmRequestScope::new("test-session", "test-run").unwrap()),
        )
        .await
        .expect("Gemini completion");

    assert_eq!(response.content.as_deref(), Some("checking"));
    assert_eq!(response.finish_reason, LlmFinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Some(LlmUsage::new(10, 7, 17, Some(2), Some(3)).unwrap())
    );
    let call = &response.tool_calls.expect("function call")[0];
    assert_eq!(call.name, "grep");
    assert_eq!(call.arguments["pattern"], "needle");

    let request = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured Gemini request");
    assert!(request.starts_with("POST /v1beta/models/gemini-test:generateContent HTTP/1.1"));
    assert!(request
        .to_ascii_lowercase()
        .contains("x-goog-api-key: gemini-test-key"));
    assert!(request.contains("\"maxOutputTokens\":47"));
    assert!(request.contains("\"systemInstruction\""));
    assert!(request.contains("\"functionDeclarations\""));
    assert!(request.contains("\"role\":\"model\""));
}

#[tokio::test]
async fn stream_consumes_sse_text_and_function_calls() {
    let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"work\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":4,\"thoughtsTokenCount\":3,\"totalTokenCount\":17,\"cachedContentTokenCount\":2}}\n\n"
        );
    let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", body);
    let mut stream = test_client(base_url)
        .stream(
            LlmRequest::new(&[LlmMessage::user("read")], None)
                .with_scope(LlmRequestScope::new("test-session", "test-run").unwrap()),
        )
        .await
        .expect("Gemini stream");
    let mut content = String::new();
    let mut accumulator = crate::llm::ToolCallAccumulator::default();
    let mut finish = None;
    while let Some(event) = stream.recv_private().await {
        let event = event.expect("valid Gemini event");
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
    let calls = accumulator.finish().expect("streamed Gemini call");
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments["path"], "README.md");
    let completion = stream.finish().await.expect("private Gemini completion");
    assert_eq!(completion.content.as_deref(), Some("work"));
    assert_eq!(completion.finish_reason, LlmFinishReason::ToolCalls);
    assert_eq!(
        completion.usage,
        Some(LlmUsage::new(10, 7, 17, Some(2), Some(3)).unwrap())
    );
    let private_calls = completion.tool_calls.expect("private tool authority");
    assert_eq!(private_calls[0].name, "read_file");
    assert_eq!(private_calls[0].arguments["path"], "README.md");
    let request = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured Gemini stream request");
    assert!(request
        .starts_with("POST /v1beta/models/gemini-test:streamGenerateContent?alt=sse HTTP/1.1"));
}

#[test]
fn gemini_usage_includes_thoughts_and_rejects_malformed_or_overflow_counts() {
    let usage = parse_gemini_usage(&json!({
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 2,
            "thoughtsTokenCount": 3,
            "totalTokenCount": 10
        }
    }))
    .unwrap()
    .unwrap();
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.reasoning_output_tokens, Some(3));

    for usage in [
        json!({"promptTokenCount": 1.5, "candidatesTokenCount": 1, "totalTokenCount": 2}),
        json!({"promptTokenCount": 2, "candidatesTokenCount": 1, "thoughtsTokenCount": 1, "totalTokenCount": 3}),
        json!({"promptTokenCount": 1, "candidatesTokenCount": u64::MAX, "thoughtsTokenCount": 1, "totalTokenCount": u64::MAX}),
        json!({"promptTokenCount": 1_000_000_001_u64, "candidatesTokenCount": 0, "totalTokenCount": 1_000_000_001_u64}),
    ] {
        let error = parse_gemini_usage(&json!({"usageMetadata": usage}))
            .expect_err("malformed Gemini usage must fail");
        assert!(
            error.contains("usage") || error.contains("overflow"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn dropping_stream_receiver_closes_the_open_http_response() {
    let (base_url, _request_rx, disconnect_rx) = serve_open_stream(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"first\"}]}}]}\n\n",
    );
    let mut stream = test_client(base_url)
        .stream(LlmRequest::new(&[LlmMessage::user("cancel")], None))
        .await
        .expect("open Gemini stream");
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
    assert!(disconnected, "Gemini response connection remained open");
}

#[tokio::test]
async fn terminal_chunk_terminates_an_open_response_without_post_terminal_events() {
    let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
        "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"late\"}]}}]}\n\n"
    ));
    let mut stream = test_client(base_url)
        .stream(LlmRequest::new(&[LlmMessage::user("finish")], None))
        .await
        .expect("open Gemini stream");

    let terminal = tokio::time::timeout(Duration::from_secs(1), stream.recv_private())
        .await
        .expect("terminal event timeout")
        .expect("terminal event")
        .expect("valid terminal event");
    assert_eq!(
        terminal,
        LlmStreamEvent::Terminal(LlmFinishReason::Complete)
    );
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
    assert!(disconnected, "Gemini terminal event left response open");
}

#[tokio::test]
async fn tool_chunk_followed_by_eof_cannot_produce_private_authority() {
    let body = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{",
        "\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}}}]}}]}\n\n"
    );
    let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
    let stream = test_client(base_url)
        .stream(LlmRequest::new(&[LlmMessage::user("read")], None))
        .await
        .expect("Gemini stream");

    let error = stream
        .finish()
        .await
        .expect_err("EOF before finishReason must fail closed");
    assert!(error.contains("ended before an explicit finishReason"));
}

#[tokio::test]
async fn complete_and_stream_hide_http_error_bodies() {
    const SENTINEL: &str = "remote-secret-prompt-and-key";
    let (base_url, _) = serve_once("401 Unauthorized", "text/plain", SENTINEL);
    let error = test_client(base_url)
        .complete(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect_err("completion must reject HTTP error");
    assert_eq!(error, "Gemini API request failed with HTTP 401");
    assert_eq!(error.class, crate::llm::LlmErrorClass::Authentication);
    assert_eq!(error.phase, crate::llm::LlmErrorPhase::HttpResponse);
    assert!(!error.contains(SENTINEL));

    let (base_url, _) = serve_once("403 Forbidden", "text/plain", SENTINEL);
    let error = test_client(base_url)
        .stream(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect_err("stream must reject HTTP error");
    assert_eq!(error, "Gemini API request failed with HTTP 403");
    assert_eq!(error.class, crate::llm::LlmErrorClass::Authentication);
    assert_eq!(error.phase, crate::llm::LlmErrorPhase::HttpResponse);
    assert!(!error.contains(SENTINEL));

    const INACTIVE: &str = "inactive-google-credential";
    let (base_url, _) = serve_once("401 Unauthorized", "text/plain", "private");
    let error = GeminiClient::configured_with_diagnostic_secrets(
        format!("model-{INACTIVE}"),
        vec!["active-key".to_string()],
        vec!["active-key".to_string(), INACTIVE.to_string()],
        base_url,
    )
    .expect("configured Gemini client")
    .complete(LlmRequest::new(&[LlmMessage::user("x")], None))
    .await
    .expect_err("diagnostic redaction fixture");
    let report = error.user_report(None);
    assert!(report.contains("model-[REDACTED]"), "{report}");
    assert!(!report.contains(INACTIVE), "{report}");
}

#[tokio::test]
async fn stream_in_band_error_hides_remote_message() {
    const SENTINEL: &str = "remote-stream-secret";
    let (base_url, _) = serve_once(
        "200 OK",
        "text/event-stream",
        format!("data: {{\"error\":{{\"message\":\"{SENTINEL}\"}}}}\n\n"),
    );
    let stream = test_client(base_url)
        .stream(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect("Gemini stream response");
    let error = stream.finish().await.expect_err("provider stream error");
    assert_eq!(error, "Gemini stream reported a provider error");
    assert_eq!(error.class, crate::llm::LlmErrorClass::ProviderRejected);
    assert_eq!(error.phase, crate::llm::LlmErrorPhase::Stream);
    assert!(!error.contains(SENTINEL));
}

#[tokio::test]
async fn stream_rejects_truncation_before_private_completion() {
    let (base_url, _) = serve_once(
            "200 OK",
            "text/event-stream",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"finishReason\":\"MAX_TOKENS\"}]}\n\n",
        );
    let stream = test_client(base_url)
        .stream(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect("Gemini stream response");
    let error = stream.finish().await.expect_err("truncated stream");
    assert!(error.contains("truncated"));
}

#[tokio::test]
async fn oversized_declared_response_lengths_are_rejected() {
    let (base_url, _) = serve_once_with_declared_length(
        "200 OK",
        "application/json",
        "{}",
        Some(crate::llm::MAX_LLM_COMPLETE_RESPONSE_BYTES + 1),
    );
    let error = test_client(base_url)
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
    let error = test_client(base_url)
        .stream(LlmRequest::new(&[LlmMessage::user("x")], None))
        .await
        .expect_err("oversized stream must be rejected");
    assert!(error.contains("16777216-byte limit"));
}

#[test]
fn gemini_tool_schema_omits_json_schema_keywords_the_api_rejects() {
    let client = GeminiClient::new(
        "gemini-test".to_string(),
        vec!["gemini-test-key".to_string()],
    );
    let tool = ToolDefinition::new(
        "record_probe",
        "Record one nonce",
        json!({
            "type": "object",
            "properties": {"nonce": {"type": "string"}},
            "required": ["nonce"],
            "additionalProperties": false
        }),
    )
    .expect("qualification tool")
    .with_strict(true);
    let messages = [LlmMessage::user("call")];
    let body = client
        .request_body(
            LlmRequest::new(&messages, Some(&[tool])).with_tool_choice(ToolChoice::Required),
        )
        .expect("valid Gemini request");
    let parameters = &body["tools"][0]["functionDeclarations"][0]["parameters"];
    assert!(parameters.get("additionalProperties").is_none());
    assert_eq!(parameters["required"][0], "nonce");
    assert_eq!(parameters["properties"]["nonce"]["type"], "string");
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
}
