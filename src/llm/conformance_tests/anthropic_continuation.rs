//! Offline native-content continuation fixtures (T068).
use super::*;

const THOUGHT: &str = "private-thinking-sentinel";
const SIGNATURE: &str = "opaque-signature-sentinel";
const REDACTED: &str = "encrypted-redacted-sentinel";

fn native_blocks() -> Vec<Value> {
    vec![
        json!({"type": "thinking", "thinking": THOUGHT, "signature": SIGNATURE, "provider_metadata": {"opaque": "private-metadata-sentinel"}}),
        json!({"type": "redacted_thinking", "data": REDACTED}),
        json!({"type": "tool_use", "id": "toolu_alpha", "name": "probe_alpha", "input": {"slot": 1}}),
        json!({"type": "text", "text": "visible-between-tools"}),
        json!({"type": "thinking", "thinking": "", "signature": "omitted-thinking-signature"}),
        json!({"type": "tool_use", "id": "toolu_beta", "name": "probe_beta", "input": {"slot": 2}}),
    ]
}

fn event(wire: &mut String, kind: &str, data: Value) {
    wire.push_str(&format!("event: {kind}\ndata: {data}\n\n"));
}

fn native_stream(blocks: &[Value]) -> String {
    let mut wire = String::new();
    event(
        &mut wire,
        "message_start",
        json!({"message": {"role": "assistant", "content": []}}),
    );
    for (index, block) in blocks.iter().enumerate() {
        let mut initial = block.clone();
        let delta = match block["type"].as_str().unwrap() {
            "thinking" => {
                initial["thinking"] = json!("");
                initial.as_object_mut().unwrap().remove("signature");
                Some(json!({"type": "thinking_delta", "thinking": block["thinking"]}))
            }
            "text" => {
                initial["text"] = json!("");
                Some(json!({"type": "text_delta", "text": block["text"]}))
            }
            "tool_use" => {
                initial["input"] = json!({});
                Some(
                    json!({"type": "input_json_delta", "partial_json": block["input"].to_string()}),
                )
            }
            "redacted_thinking" => None,
            other => panic!("unsupported fixture block {other}"),
        };
        event(
            &mut wire,
            "content_block_start",
            json!({"index": index, "content_block": initial}),
        );
        if let Some(delta) = delta {
            event(
                &mut wire,
                "content_block_delta",
                json!({"index": index, "delta": delta}),
            );
        }
        if block["type"] == "thinking" {
            let signature = block["signature"].as_str().unwrap();
            let chunk_size = (signature.len() / 2).clamp(1, 64 * 1024);
            for bytes in signature.as_bytes().chunks(chunk_size) {
                let fragment = std::str::from_utf8(bytes).expect("ASCII fixture signature");
                event(
                    &mut wire,
                    "content_block_delta",
                    json!({"index": index, "delta": {"type": "signature_delta", "signature": fragment}}),
                );
            }
        }
        event(&mut wire, "content_block_stop", json!({"index": index}));
    }
    event(
        &mut wire,
        "message_delta",
        json!({"delta": {"stop_reason": "tool_use"}}),
    );
    event(&mut wire, "message_stop", json!({}));
    wire
}

async fn captured_continuation(streamed: bool, blocks: &[Value], error_result: bool) -> Value {
    let first = ScriptedHttpResponse::new(
        "200 OK",
        if streamed {
            "text/event-stream"
        } else {
            "application/json"
        },
        if streamed {
            native_stream(blocks)
        } else {
            json!({"content": blocks, "stop_reason": "tool_use"}).to_string()
        },
    );
    let final_response = ScriptedHttpResponse::new(
        "200 OK",
        "application/json",
        json!({"content": [{"type": "text", "text": "receipt"}], "stop_reason": "end_turn"})
            .to_string(),
    );
    let (endpoint, requests) = serve_sequence(vec![first, final_response]);
    let client = ConformanceAdapter::Anthropic.client(endpoint);
    let messages = [LlmMessage::user("inspect synthetic slots")];
    let tools = ["probe_alpha", "probe_beta"].map(|name| ToolDefinition::new(name, "Inspect one synthetic slot", json!({"type": "object", "properties": {"slot": {"type": "integer"}}, "required": ["slot"]})).unwrap());
    let scope = LlmRequestScope::new("native-session", "native-run").unwrap();
    let response = if streamed {
        let mut stream = client
            .stream(LlmRequest::new(&messages, Some(&tools)).with_scope(scope.clone()))
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = stream.recv_private().await {
            events.push(event.unwrap());
        }
        let projected = format!("{events:?}");
        for private in [
            THOUGHT,
            SIGNATURE,
            REDACTED,
            "omitted-thinking-signature",
            "private-metadata-sentinel",
        ] {
            assert!(!projected.contains(private));
        }
        stream.finish().await.unwrap()
    } else {
        client
            .complete(LlmRequest::new(&messages, Some(&tools)).with_scope(scope.clone()))
            .await
            .unwrap()
    };
    let debug = format!("{response:?}");
    for private in [
        THOUGHT,
        SIGNATURE,
        REDACTED,
        "omitted-thinking-signature",
        "private-metadata-sentinel",
    ] {
        assert!(!debug.contains(private));
    }
    assert_eq!(response.finish_reason, LlmFinishReason::ToolCalls);
    let mut continuation = response.continuation.unwrap();
    for call in response.tool_calls.unwrap().into_iter().rev() {
        let result = if error_result {
            ToolResult::error(call.invocation_id, json!({"receipt": call.name})).unwrap()
        } else {
            ToolResult::success(call.invocation_id, json!({"receipt": call.name})).unwrap()
        };
        continuation.record_tool_result(result).unwrap();
    }
    let final_response = client
        .complete(
            LlmRequest::new(&messages, Some(&tools))
                .with_scope(scope)
                .with_continuation(Some(continuation)),
        )
        .await
        .unwrap();
    assert_eq!(final_response.content.as_deref(), Some("receipt"));
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    serde_json::from_str(second.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn streamed_thinking_and_redacted_blocks_survive_exact_parallel_continuation() {
    let blocks = native_blocks();
    let streamed = captured_continuation(true, &blocks, false).await;
    assert_eq!(streamed["messages"][1]["content"], json!(blocks));
    assert_eq!(
        streamed["messages"][2]["content"][0]["tool_use_id"],
        "toolu_alpha"
    );
    assert_eq!(
        streamed["messages"][2]["content"][1]["tool_use_id"],
        "toolu_beta"
    );
    assert_eq!(
        streamed["messages"][2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_alpha", "content": "{\"receipt\":\"probe_alpha\"}", "is_error": false},
            {"type": "tool_result", "tool_use_id": "toolu_beta", "content": "{\"receipt\":\"probe_beta\"}", "is_error": false}
        ])
    );
    let complete = captured_continuation(false, &blocks, false).await;
    assert_eq!(streamed, complete);
}

#[tokio::test]
async fn omitted_thinking_single_tool_preserves_explicit_error_classification() {
    let all = native_blocks();
    let blocks = vec![all[4].clone(), all[5].clone()];
    let body = captured_continuation(true, &blocks, true).await;
    assert_eq!(body["messages"][1]["content"], json!(blocks));
    assert_eq!(
        body["messages"][2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_beta", "content": "{\"receipt\":\"probe_beta\"}", "is_error": true}
        ])
    );
}

async fn assert_invalid_native_turn(wire: String) -> LlmError {
    let (endpoint, _) = serve_once("200 OK", "text/event-stream", &wire);
    let client = ConformanceAdapter::Anthropic.client(endpoint);
    let messages = [LlmMessage::user("synthetic validation")];
    let request = LlmRequest::new(&messages, None)
        .with_scope(LlmRequestScope::new("invalid-session", "invalid-run").unwrap());
    let stream = client.stream(request).await.unwrap();
    let error = stream
        .finish()
        .await
        .expect_err("invalid native turn must provide no tool authority");
    let debug = format!("{error:?}");
    for private in [THOUGHT, SIGNATURE, REDACTED] {
        assert!(!debug.contains(private));
    }
    error
}

#[tokio::test]
async fn malformed_native_blocks_fail_closed_without_private_authority() {
    let blocks = native_blocks();
    let valid = native_stream(&blocks);
    let wrong_type = valid.replace("\"type\":\"signature_delta\"", "\"type\":\"text_delta\"");
    let missing_stop = valid.replace("event: content_block_stop\ndata: {\"index\":0}\n\n", "");
    let duplicate_start = valid.replace("event: content_block_stop\ndata: {\"index\":0}\n\n", "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n");
    let missing_terminal = valid.replace("event: message_stop\ndata: {}\n\n", "");
    let truncated = valid.replace("tool_use\"}}", "max_tokens\"}}");
    let mut missing_signature = blocks.clone();
    missing_signature[0]["signature"] = json!("");
    let mut invalid_redaction = blocks.clone();
    invalid_redaction[1]["data"] = json!(false);
    let mut after_terminal = valid.replace("event: message_stop\ndata: {}\n\n", "");
    event(
        &mut after_terminal,
        "content_block_start",
        json!({"index": 6, "content_block": {"type": "thinking", "thinking": THOUGHT}}),
    );
    event(&mut after_terminal, "message_stop", json!({}));
    for wire in [
        wrong_type,
        missing_stop,
        duplicate_start,
        missing_terminal,
        truncated,
        native_stream(&missing_signature),
        native_stream(&invalid_redaction),
        after_terminal,
    ] {
        assert_ne!(wire, valid);
        assert_invalid_native_turn(wire).await;
    }
}

#[tokio::test]
async fn preserved_private_content_obeys_continuation_byte_and_item_bounds() {
    let mut large = native_blocks();
    large[0]["signature"] = json!("s".repeat(crate::llm::types::MAX_CONTINUATION_BYTES + 1));
    let bytes = assert_invalid_native_turn(native_stream(&large)).await;
    assert!(bytes.contains("continuation"));
    assert!(bytes.contains("byte limit"));
    let mut many = vec![native_blocks()[4].clone(); crate::llm::types::MAX_CONTINUATION_ITEMS - 1];
    many.push(native_blocks()[2].clone());
    let items = assert_invalid_native_turn(native_stream(&many)).await;
    assert!(items.contains("continuation"));
    assert!(items.contains("item limit"));
}

#[tokio::test]
async fn provider_refusal_retains_no_executable_streamed_authority() {
    let refused = native_stream(&native_blocks()).replace("tool_use\"}}", "refusal\"}}");
    let error = assert_invalid_native_turn(refused).await;
    assert!(error.contains("refusal"));
}
