use super::*;
use crate::llm::test_support::serve_once;
use crate::llm::types::ToolChoice;
use std::time::Duration;

fn completed_with_call() -> Value {
    json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "opaque-reasoning-secret"
            },
            {
                "type": "message",
                "status": "completed",
                "content": [
                    {"type": "output_text", "text": "inspected"},
                    {"type": "output_text", "text": " successfully"}
                ]
            },
            {
                "type": "function_call",
                "status": "completed",
                "call_id": "call_private_123",
                "name": "read_file",
                "arguments": "{\"path\":\"README.md\"}"
            }
        ],
        "usage": {
            "input_tokens": 21,
            "output_tokens": 9,
            "total_tokens": 30,
            "input_tokens_details": {"cached_tokens": 5},
            "output_tokens_details": {"reasoning_tokens": 4}
        }
    })
}

fn request_json(request: &str) -> Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("HTTP request body separator");
    serde_json::from_str(body).expect("JSON request body")
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {}\n\n", event))
        .collect()
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn posts_responses_native_payload_and_preserves_private_continuation() {
    let terminal = completed_with_call();
    let (base_url, request_rx) = serve_once("200 OK", "application/json", terminal.to_string());
    let endpoint = format!("{base_url}/v1/responses?route=test");
    let client = OpenAiResponsesClient::configured(
        "openai",
        "gpt-test".to_string(),
        vec!["test-api-key".to_string()],
        endpoint,
        Some(ReasoningEffort::Low),
    );
    let messages = [LlmMessage::user("inspect")];
    let tools = [
        ToolDefinition::new("read_file", "Read a file", json!({"type": "object"}))
            .expect("read_file tool"),
    ];
    let scope = LlmRequestScope::new("session-1", "run-1").unwrap();
    let response = client
        .complete(
            LlmRequest::new(&messages, Some(&tools))
                .with_reasoning_effort(Some(ReasoningEffort::Medium))
                .with_max_output_tokens(41)
                .with_scope(scope.clone()),
        )
        .await
        .expect("Responses completion");

    assert_eq!(response.content.as_deref(), Some("inspected successfully"));
    assert_eq!(response.terminal_status, LlmTerminalStatus::Completed);
    assert_eq!(response.finish_reason, LlmFinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Some(LlmUsage::new(21, 9, 30, Some(5), Some(4)).unwrap())
    );
    let call = &response.tool_calls.as_ref().expect("tool calls")[0];
    assert_eq!(call.name, "read_file");
    assert_eq!(call.arguments["path"], "README.md");
    assert_eq!(
        call.call_id.as_ref().expect("provider call ID").as_str(),
        "call_private_123"
    );
    assert_eq!(
        format!("{:?}", response.continuation.as_ref().unwrap()),
        "ProviderContinuation { value: \"<redacted>\" }"
    );

    let request = request_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured request");
    assert!(request.starts_with("POST /v1/responses?route=test HTTP/1.1"));
    let body = request_json(&request);
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], false);
    assert_eq!(body["max_output_tokens"], 41);
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(
        body["include"],
        json!(["reasoning.encrypted_content"]),
        "stateless tool turns must request replayable encrypted reasoning"
    );
    assert!(body.get("temperature").is_none());
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["strict"], false);
    assert!(body["tools"][0].get("function").is_none());

    let invocation_id = call.invocation_id;
    let mut continuation = response.continuation.expect("continuation");
    continuation
        .record_tool_result(ToolResult::success(invocation_id, json!({"ok": true})).unwrap())
        .unwrap();
    let (continued_body, _, replay_tail) = client
        .request_body(
            LlmRequest::new(&messages, Some(&tools))
                .with_scope(scope.clone())
                .with_continuation(Some(continuation)),
            false,
        )
        .expect("matching continuation");
    let input = continued_body["input"].as_array().unwrap();
    assert_eq!(
        continued_body["include"],
        json!(["reasoning.encrypted_content"])
    );
    assert_eq!(input.len(), 5);
    assert_eq!(input[1]["encrypted_content"], "opaque-reasoning-secret");
    assert_eq!(input[3]["call_id"], "call_private_123");
    assert_eq!(input[4]["type"], "function_call_output");
    assert_eq!(input[4]["call_id"], "call_private_123");
    assert_eq!(input[4]["output"], "{\"ok\":true}");

    let second_terminal = json!({
        "status": "completed",
        "output": [
            {"type": "reasoning", "id": "rs_2", "encrypted_content": "second-private"},
            {"type": "function_call", "call_id": "call_private_456", "name": "write_file", "arguments": "{\"path\":\"out.txt\"}"}
        ]
    });
    let second_response = parse_terminal_response(
        &second_terminal,
        "openai",
        "gpt-test",
        Some(scope.clone()),
        replay_tail,
        &[],
    )
    .expect("second tool round");
    let second_invocation_id = second_response.tool_calls.as_ref().unwrap()[0].invocation_id;
    let mut second_continuation = second_response.continuation.unwrap();
    second_continuation
        .record_tool_result(
            ToolResult::success(second_invocation_id, json!({"written": true})).unwrap(),
        )
        .unwrap();
    let next_messages = [LlmMessage::user("latest runtime context")];
    let (third_body, _, _) = client
        .request_body(
            LlmRequest::new(&next_messages, None)
                .with_scope(scope)
                .with_continuation(Some(second_continuation)),
            false,
        )
        .expect("second continuation");
    let third_input = third_body["input"].as_array().unwrap();
    assert_eq!(third_input.len(), 8);
    assert_eq!(third_input[0]["content"], "latest runtime context");
    assert_eq!(third_input[1]["id"], "rs_1");
    assert_eq!(third_input[2]["type"], "message");
    assert_eq!(third_input[3]["call_id"], "call_private_123");
    assert_eq!(third_input[4]["type"], "function_call_output");
    assert_eq!(third_input[4]["call_id"], "call_private_123");
    assert_eq!(third_input[5]["id"], "rs_2");
    assert_eq!(third_input[6]["call_id"], "call_private_456");
    assert_eq!(third_input[7]["type"], "function_call_output");
    assert_eq!(third_input[7]["call_id"], "call_private_456");
}

#[test]
fn rejects_mismatched_continuation_binding() {
    let response = parse_terminal_response(
        &completed_with_call(),
        "openai",
        "gpt-a",
        Some(LlmRequestScope::new("session", "run").unwrap()),
        Vec::new(),
        &[],
    )
    .unwrap();
    let mut continuation = response.continuation.unwrap();
    let invocation_id = response.tool_calls.unwrap()[0].invocation_id;
    continuation
        .record_tool_result(ToolResult::success(invocation_id, json!("done")).unwrap())
        .unwrap();
    let client = OpenAiResponsesClient::new(
        "openai",
        "gpt-b".to_string(),
        vec!["key".to_string()],
        "http://unused.invalid/v1/responses",
    );
    let messages = [LlmMessage::user("continue")];
    let error = client
        .request_body(
            LlmRequest::new(&messages, None)
                .with_scope(LlmRequestScope::new("session", "run").unwrap())
                .with_continuation(Some(continuation)),
            false,
        )
        .unwrap_err();
    assert!(error.contains("does not match"));
}

#[tokio::test]
async fn concurrent_session_continuations_are_scope_bound_and_isolated() {
    use std::sync::Arc;

    fn terminal(call_id: &str, name: &str, opaque: &str) -> Value {
        json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "encrypted_content": opaque},
                {"type": "function_call", "call_id": call_id, "name": name, "arguments": "{}"}
            ]
        })
    }

    let client = Arc::new(OpenAiResponsesClient::new(
        "openai",
        "gpt-test".to_string(),
        vec!["key".to_string()],
        "http://unused.invalid/v1/responses",
    ));

    let cross_response = parse_terminal_response(
        &terminal("call_cross", "cross", "opaque-cross"),
        "openai",
        "gpt-test",
        Some(LlmRequestScope::new("session-a", "run-a").unwrap()),
        Vec::new(),
        &[],
    )
    .unwrap();
    let cross_invocation_id = cross_response.tool_calls.as_ref().unwrap()[0].invocation_id;
    let mut cross_continuation = cross_response.continuation.unwrap();
    cross_continuation
        .record_tool_result(ToolResult::success(cross_invocation_id, json!({"ok": true})).unwrap())
        .unwrap();
    let cross_messages = [LlmMessage::user("cross")];
    let error = client
        .request_body(
            LlmRequest::new(&cross_messages, None)
                .with_scope(LlmRequestScope::new("session-b", "run-b").unwrap())
                .with_continuation(Some(cross_continuation)),
            false,
        )
        .expect_err("another session must not consume a continuation");
    assert!(error.contains("does not match"));

    let build =
        |session: &'static str, run: &'static str, call_id: &'static str, opaque: &'static str| {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                let scope = LlmRequestScope::new(session, run).unwrap();
                let response = parse_terminal_response(
                    &terminal(call_id, "inspect", opaque),
                    "openai",
                    "gpt-test",
                    Some(scope.clone()),
                    Vec::new(),
                    &[],
                )
                .unwrap();
                let invocation_id = response.tool_calls.as_ref().unwrap()[0].invocation_id;
                let mut continuation = response.continuation.unwrap();
                continuation
                    .record_tool_result(
                        ToolResult::success(invocation_id, json!({"session": session})).unwrap(),
                    )
                    .unwrap();
                let messages = [LlmMessage::user(session)];
                client
                    .request_body(
                        LlmRequest::new(&messages, None)
                            .with_scope(scope)
                            .with_continuation(Some(continuation)),
                        false,
                    )
                    .unwrap()
                    .0
            })
        };
    let (body_a, body_b) = tokio::join!(
        build("session-a", "run-a", "call_a", "opaque-a"),
        build("session-b", "run-b", "call_b", "opaque-b")
    );
    let body_a = body_a.unwrap().to_string();
    let body_b = body_b.unwrap().to_string();
    assert!(body_a.contains("call_a"));
    assert!(body_a.contains("opaque-a"));
    assert!(!body_a.contains("call_b"));
    assert!(!body_a.contains("opaque-b"));
    assert!(body_b.contains("call_b"));
    assert!(body_b.contains("opaque-b"));
    assert!(!body_b.contains("call_a"));
    assert!(!body_b.contains("opaque-a"));
}

#[test]
fn same_raw_call_id_from_another_session_cannot_complete_a_continuation() {
    let response_a = parse_terminal_response(
        &completed_with_call(),
        "openai",
        "gpt-test",
        Some(LlmRequestScope::new("session-a", "run-a").unwrap()),
        Vec::new(),
        &[],
    )
    .unwrap();
    let response_b = parse_terminal_response(
        &completed_with_call(),
        "openai",
        "gpt-test",
        Some(LlmRequestScope::new("session-b", "run-b").unwrap()),
        Vec::new(),
        &[],
    )
    .unwrap();
    let call_a = response_a.tool_calls.as_ref().unwrap()[0]
        .call_id
        .clone()
        .unwrap();
    let call_b = response_b.tool_calls.as_ref().unwrap()[0]
        .call_id
        .clone()
        .unwrap();
    let invocation_a = response_a.tool_calls.as_ref().unwrap()[0].invocation_id;
    let invocation_b = response_b.tool_calls.as_ref().unwrap()[0].invocation_id;
    assert_eq!(call_a.as_str(), call_b.as_str());
    assert_ne!(
        call_a, call_b,
        "private continuation provenance was not bound"
    );

    let mut continuation_a = response_a.continuation.unwrap();
    let error = continuation_a
        .record_tool_result(ToolResult::success(invocation_b, json!({"wrong": true})).unwrap())
        .expect_err("another session's token must be rejected");
    assert!(error.contains("does not belong"));
    continuation_a
        .record_tool_result(ToolResult::success(invocation_a, json!({"right": true})).unwrap())
        .expect("originating token remains valid");
}

#[test]
fn incomplete_and_failed_statuses_are_not_completed_results() {
    for terminal in [
        json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": []
        }),
        json!({
            "status": "failed",
            "error": {"code": "server_error", "message": "failed"},
            "output": []
        }),
    ] {
        let error = parse_terminal_response(&terminal, "openai", "gpt", None, Vec::new(), &[])
            .expect_err("terminal must fail");
        assert!(error.contains("Responses API"));
    }
}

#[test]
fn refusal_is_typed_and_redacted_but_mixed_tool_output_fails() {
    let refusal = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "status": "completed",
            "content": [{"type": "refusal", "refusal": "private refusal text"}]
        }]
    });
    let response =
        parse_terminal_response(&refusal, "openai", "gpt", None, Vec::new(), &[]).unwrap();
    assert_eq!(response.terminal_status, LlmTerminalStatus::Refused);
    assert_eq!(response.finish_reason, LlmFinishReason::Refusal);
    assert!(response.content.is_none());
    assert!(response.tool_calls.is_none());
    assert!(response.continuation.is_none());
    assert!(!format!("{response:?}").contains("private refusal text"));

    let mixed = json!({
        "status": "completed",
        "output": [
            {"type": "message", "content": [
                {"type": "refusal", "refusal": "private refusal text"}
            ]},
            {"type": "function_call", "call_id": "private_call", "name": "unsafe", "arguments": "{}"}
        ]
    });
    let error = parse_terminal_response(&mixed, "openai", "gpt", None, Vec::new(), &[])
        .expect_err("mixed refusal and function call must fail");
    assert!(!error.contains("private refusal text"));
    assert!(!error.contains("private_call"));
}

#[tokio::test]
async fn stream_projects_deltas_but_uses_completed_envelope_privately() {
    let body = sse(&[
        json!({"type": "response.output_text.delta", "delta": "inspected"}),
        json!({"type": "response.output_text.delta", "delta": " successfully"}),
        json!({"type": "response.output_item.added", "output_index": 2, "item": {
            "type": "function_call",
            "call_id": "call_private_123",
            "name": "read_file",
            "arguments": ""
        }}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"path\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "\"README.md\"}"}),
        json!({"type": "response.completed", "response": completed_with_call()}),
    ]);
    let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
    let client = OpenAiResponsesClient::new(
        "openai",
        "gpt-test".to_string(),
        vec!["key".to_string()],
        format!("{base_url}/v1/responses"),
    );
    let messages = [LlmMessage::user("inspect")];
    let mut stream = client
        .stream(
            LlmRequest::new(&messages, None)
                .with_scope(LlmRequestScope::new("session", "run").unwrap()),
        )
        .await
        .expect("Responses stream");
    let mut public = Vec::new();
    while let Some(event) = stream.recv_private().await {
        public.push(event.expect("valid public event"));
    }

    assert!(matches!(
        &public[0],
        LlmStreamEvent::Delta(LlmDelta::Content(text)) if text == "inspected"
    ));
    assert!(matches!(
        &public[1],
        LlmStreamEvent::Delta(LlmDelta::Content(text)) if text == " successfully"
    ));
    assert!(matches!(
        &public[2],
        LlmStreamEvent::Delta(LlmDelta::ToolCallChunk {
            index: 2,
            name: Some(name),
            arguments: None
        })
            if name == "read_file"
    ));
    assert!(matches!(
        public.last(),
        Some(LlmStreamEvent::Terminal(LlmFinishReason::ToolCalls))
    ));
    assert!(!format!("{public:?}").contains("call_private_123"));

    let completion = stream.finish().await.expect("private completion");
    assert_eq!(
        completion.usage,
        Some(LlmUsage::new(21, 9, 30, Some(5), Some(4)).unwrap())
    );
    let call = &completion.tool_calls.unwrap()[0];
    assert_eq!(call.call_id.as_ref().unwrap().as_str(), "call_private_123");
    assert!(completion.continuation.is_some());
}

#[test]
fn complete_and_stream_terminal_usage_fail_closed_when_malformed() {
    for usage in [
        json!({"input_tokens": 2.5, "output_tokens": 1, "total_tokens": 3}),
        json!({"input_tokens": 2, "output_tokens": 1, "total_tokens": 4}),
        json!({"input_tokens": 1_000_000_001_u64, "output_tokens": 0, "total_tokens": 1_000_000_001_u64}),
    ] {
        let terminal = json!({
            "status": "completed",
            "output": [{"type": "message", "status": "completed", "content": [{"type": "output_text", "text": "ok"}]}],
            "usage": usage,
        });
        let complete_error =
            parse_terminal_response(&terminal, "openai", "gpt-test", None, Vec::new(), &[])
                .expect_err("malformed complete usage must fail");
        let stream_error = parse_stream_event(
            &json!({"type": "response.completed", "response": terminal}),
            "openai",
            "gpt-test",
            None,
            &[],
            &[],
        )
        .expect_err("malformed streamed usage must fail");
        assert!(complete_error.contains("usage"), "{complete_error}");
        assert!(stream_error.contains("usage"), "{stream_error}");
    }
}

#[tokio::test]
async fn stream_reconstructs_multiple_interleaved_fragmented_calls_privately() {
    let terminal = json!({
        "status": "completed",
        "output": [
            {"type": "reasoning", "encrypted_content": "private-reasoning"},
            {"type": "function_call", "call_id": "call_alpha", "name": "alpha", "arguments": "{\"value\":1}"},
            {"type": "function_call", "call_id": "call_beta", "name": "beta", "arguments": "{\"value\":2}"}
        ]
    });
    let body = sse(&[
        json!({"type": "response.output_item.added", "output_index": 1, "item": {
            "type": "function_call", "call_id": "call_beta", "name": "beta", "arguments": ""
        }}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {
            "type": "function_call", "call_id": "call_alpha", "name": "alpha", "arguments": ""
        }}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"value\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"value\":"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "2}"}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "1}"}),
        json!({"type": "response.completed", "response": terminal}),
    ]);
    let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
    let client = OpenAiResponsesClient::new(
        "openai",
        "gpt-test".to_string(),
        vec!["key".to_string()],
        format!("{base_url}/v1/responses"),
    );
    let messages = [LlmMessage::user("run both")];
    let mut stream = client
        .stream(
            LlmRequest::new(&messages, None)
                .with_scope(LlmRequestScope::new("session", "run").unwrap()),
        )
        .await
        .expect("Responses stream");
    let mut public = Vec::new();
    while let Some(event) = stream.recv_private().await {
        public.push(event.expect("valid public event"));
    }
    assert!(!format!("{public:?}").contains("call_alpha"));
    assert!(!format!("{public:?}").contains("private-reasoning"));

    let completion = stream.finish().await.expect("private completion");
    let calls = completion.tool_calls.expect("multiple calls");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].call_id.as_ref().unwrap().as_str(), "call_alpha");
    assert_eq!(calls[0].arguments["value"], 1);
    assert_eq!(calls[1].call_id.as_ref().unwrap().as_str(), "call_beta");
    assert_eq!(calls[1].arguments["value"], 2);
}

#[test]
fn terminal_parser_aggregates_messages_and_multiple_function_calls_in_order() {
    let terminal = json!({
        "status": "completed",
        "output": [
            {"type": "message", "content": [
                {"type": "output_text", "text": "first "}
            ]},
            {"type": "function_call", "call_id": "call_1", "name": "one", "arguments": "{\"n\":1}"},
            {"type": "reasoning", "encrypted_content": "private"},
            {"type": "message", "content": [
                {"type": "output_text", "text": "second"}
            ]},
            {"type": "function_call", "call_id": "call_2", "name": "two", "arguments": "{\"n\":2}"}
        ]
    });
    let response = parse_terminal_response(
        &terminal,
        "openai",
        "gpt",
        Some(LlmRequestScope::new("session", "run").unwrap()),
        Vec::new(),
        &[],
    )
    .unwrap();

    assert_eq!(response.content.as_deref(), Some("first second"));
    let calls = response.tool_calls.unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "one");
    assert_eq!(calls[0].call_id.as_ref().unwrap().as_str(), "call_1");
    assert_eq!(calls[1].name, "two");
    assert_eq!(calls[1].call_id.as_ref().unwrap().as_str(), "call_2");
    assert!(response.continuation.is_some());
}

#[test]
fn whitespace_stream_deltas_are_preserved() {
    let StreamAction::Public(text) = parse_stream_event(
        &json!({"type": "response.output_text.delta", "delta": " "}),
        "openai",
        "gpt",
        None,
        &[],
        &[],
    )
    .unwrap() else {
        panic!("text projection");
    };
    assert_eq!(
        text,
        [LlmStreamEvent::Delta(LlmDelta::Content(" ".to_string()))]
    );

    let StreamAction::Public(arguments) = parse_stream_event(
        &json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": " "
        }),
        "openai",
        "gpt",
        None,
        &[],
        &[],
    )
    .unwrap() else {
        panic!("arguments projection");
    };
    assert!(matches!(
        &arguments[0],
        LlmStreamEvent::Delta(LlmDelta::ToolCallChunk {
            arguments: Some(delta),
            ..
        }) if delta == " "
    ));
}

#[tokio::test]
async fn dropping_stream_closes_the_open_http_response() {
    let (base_url, _request_rx, disconnect_rx) =
        crate::llm::test_support::serve_open_stream(sse(&[json!({
            "type": "response.output_text.delta",
            "delta": "first"
        })]));
    let client = OpenAiResponsesClient::new(
        "openai",
        "gpt-test".to_string(),
        vec!["key".to_string()],
        format!("{base_url}/v1/responses"),
    );
    let messages = [LlmMessage::user("inspect")];
    let mut stream = client
        .stream(LlmRequest::new(&messages, None))
        .await
        .expect("open Responses stream");
    assert_eq!(
        stream.recv_private().await.unwrap().unwrap(),
        LlmStreamEvent::Delta(LlmDelta::Content("first".to_string()))
    );
    drop(stream);

    let disconnected =
        tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
            .await
            .unwrap()
            .unwrap();
    assert!(disconnected, "Responses connection remained open");
}

#[tokio::test]
async fn stream_rejects_eof_and_done_without_typed_completion() {
    for body in [
        sse(&[json!({
            "type": "response.output_text.delta",
            "delta": "partial"
        })]),
        "data: [DONE]\n\n".to_string(),
    ] {
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let client = OpenAiResponsesClient::new(
            "openai",
            "gpt-test".to_string(),
            vec!["key".to_string()],
            format!("{base_url}/v1/responses"),
        );
        let messages = [LlmMessage::user("inspect")];
        let stream = client
            .stream(LlmRequest::new(&messages, None))
            .await
            .expect("stream starts");
        let error = stream.finish().await.expect_err("terminal event required");
        assert!(error.contains("response.completed"));
    }
}

#[test]
fn typed_stream_failures_are_terminal() {
    for event in [
        json!({"type": "response.failed", "response": {
            "status": "failed", "error": {"code": "server_error", "message": "failed"}
        }}),
        json!({"type": "response.incomplete", "response": {
            "status": "incomplete", "incomplete_details": {"reason": "content_filter"}
        }}),
        json!({"type": "error", "code": "bad_request", "message": "failed"}),
    ] {
        assert!(parse_stream_event(&event, "openai", "gpt", None, &[], &[]).is_err());
    }
}

#[tokio::test]
async fn complete_and_stream_structural_errors_keep_their_typed_classification() {
    const SENTINEL: &str = "remote-provider-message";
    let terminal = json!({
        "id": "resp_failed",
        "status": "failed",
        "error": {"code": "server_error", "message": SENTINEL},
        "output": []
    });
    let (base_url, _) = serve_once("200 OK", "application/json", terminal.to_string());
    let client = OpenAiResponsesClient::new(
        "openai",
        "gpt-test".to_string(),
        vec!["key".to_string()],
        format!("{base_url}/v1/responses"),
    );
    let messages = [LlmMessage::user("inspect")];
    let complete_error = client
        .complete(LlmRequest::new(&messages, None))
        .await
        .expect_err("provider failure must reject completion");

    let body = sse(&[json!({
        "type": "response.failed",
        "response": {
            "status": "failed",
            "error": {"code": "server_error", "message": SENTINEL},
            "output": []
        }
    })]);
    let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
    let client = OpenAiResponsesClient::new(
        "openai",
        "gpt-test".to_string(),
        vec!["key".to_string()],
        format!("{base_url}/v1/responses"),
    );
    let stream_error = client
        .stream(LlmRequest::new(&messages, None))
        .await
        .expect("stream starts")
        .finish()
        .await
        .expect_err("provider failure must terminate the stream");

    assert_eq!(
        complete_error.class,
        crate::llm::LlmErrorClass::ProviderRejected
    );
    assert_eq!(
        stream_error.class,
        crate::llm::LlmErrorClass::ProviderRejected
    );
    assert_eq!(
        complete_error.phase,
        crate::llm::LlmErrorPhase::TerminalValidation
    );
    assert_eq!(stream_error.phase, crate::llm::LlmErrorPhase::Stream);
    assert_eq!(stream_error.provider, "openai");
    assert_eq!(stream_error.transport, RESPONSES_TRANSPORT);
    assert!(!complete_error.user_report(None).contains(SENTINEL));
    assert!(!stream_error.user_report(None).contains(SENTINEL));
}

#[tokio::test]
async fn http_errors_are_bounded_and_redact_credentials() {
    let secret = "test-api-key-secret";
    let prompt_echo = "private-user-prompt-must-not-escape";
    let body = json!({
        "error": {
            "type": prompt_echo,
            "code": prompt_echo,
            "param": prompt_echo,
            "message": format!("credential {secret}: {prompt_echo}\n\u{1b}[31m{}", "x".repeat(10_000))
        }
    });
    let (base_url, _) = serve_once("400 Bad Request", "application/json", body.to_string());
    let client = OpenAiResponsesClient::new(
        format!("openai-{secret}"),
        format!("gpt-{secret}\n"),
        vec![secret.to_string()],
        format!("{base_url}/v1/responses"),
    );
    let messages = [LlmMessage::user("inspect")];
    let error = client
        .complete(LlmRequest::new(&messages, None))
        .await
        .expect_err("HTTP error");
    assert_eq!(error.class, crate::llm::LlmErrorClass::ProviderRejected);
    assert_eq!(error.phase, crate::llm::LlmErrorPhase::HttpResponse);
    assert_eq!(error.http_status, Some(400));
    assert!(error.len() <= MAX_DIAGNOSTIC_BYTES);
    assert!(error.contains("[REDACTED]"));
    assert!(!error.contains(secret));
    assert!(!error.contains(prompt_echo));
    assert!(!error.contains('\n'));
    assert!(!error.contains('\u{1b}'));
    assert!(!error.contains(&"x".repeat(5_000)));
}

#[test]
fn strict_defaults_false_but_explicit_values_are_preserved() {
    let loose = ToolDefinition::function("loose").to_responses_tool();
    let strict = ToolDefinition::function("strict")
        .with_strict(true)
        .to_responses_tool();
    assert_eq!(loose["strict"], false);
    assert_eq!(strict["strict"], true);
}

#[test]
fn complete_status_and_text_content_parts_are_accepted() {
    let response = parse_terminal_response(
        &json!({
            "status": "complete",
            "output": [{
                "type": "message",
                "status": "completed",
                "content": [{"type": "text", "text": "NIB_ok"}]
            }]
        }),
        "meta",
        "muse-spark-1.1",
        None,
        Vec::new(),
        &[],
    )
    .expect("complete/text are valid terminal shapes");
    assert_eq!(response.content.as_deref(), Some("NIB_ok"));
    assert_eq!(response.finish_reason, LlmFinishReason::Complete);
}

#[test]
fn responses_request_encodes_required_tool_choice() {
    let client = OpenAiResponsesClient::new(
        "openai",
        "gpt-5.6-sol".to_string(),
        vec!["test-key".to_string()],
        "https://api.openai.com/v1/responses",
    );
    let messages = [crate::llm::types::LlmMessage::user("call")];
    let tools = [ToolDefinition::function("record_probe")];
    let (body, _, _) = client
        .request_body(
            LlmRequest::new(&messages, Some(&tools)).with_tool_choice(ToolChoice::Required),
            false,
        )
        .expect("valid Responses request");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["tools"][0]["strict"], false);
}

#[test]
fn compatible_responses_tools_omit_strict_and_meta_keeps_auto_choice() {
    let messages = [crate::llm::types::LlmMessage::user("call")];
    let tools = [ToolDefinition::function("record_probe").with_strict(true)];
    let grok = OpenAiResponsesClient::new(
        "grok",
        "grok-4.5".to_string(),
        vec!["test-key".to_string()],
        "https://api.x.ai/v1/responses",
    )
    .request_body(
        LlmRequest::new(&messages, Some(&tools)).with_tool_choice(ToolChoice::Required),
        false,
    )
    .expect("valid Grok Responses request")
    .0;
    assert_eq!(grok["tools"][0]["strict"], true);
    assert_eq!(grok["tool_choice"], "required");

    let openrouter_claude = OpenAiResponsesClient::new(
        "openrouter",
        "anthropic/claude-opus-5".to_string(),
        vec!["test-key".to_string()],
        "https://openrouter.ai/api/v1/responses",
    )
    .request_body(
        LlmRequest::new(&messages, Some(&tools)).with_tool_choice(ToolChoice::Required),
        false,
    )
    .expect("valid OpenRouter Responses request")
    .0;
    assert!(openrouter_claude["tools"][0].get("strict").is_none());
    assert!(openrouter_claude.get("tool_choice").is_none());

    let meta = OpenAiResponsesClient::new(
        "meta",
        "muse-spark-1.1".to_string(),
        vec!["test-key".to_string()],
        "https://api.meta.ai/v1/responses",
    )
    .request_body(
        LlmRequest::new(&messages, Some(&tools)).with_tool_choice(ToolChoice::Required),
        false,
    )
    .expect("valid Meta Responses request")
    .0;
    assert!(meta["tools"][0].get("strict").is_none());
    assert!(meta.get("tool_choice").is_none());
}
