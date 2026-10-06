//! Offline native-content continuation fixtures (T068).
use super::*;

const THOUGHT: &str = "private-thinking-sentinel";
const SIGNATURE: &str = "opaque-signature-sentinel";
const REDACTED: &str = "encrypted-redacted-sentinel";

fn native_blocks() -> Vec<Value> {
    vec![
        json!({"type": "thinking", "thinking": THOUGHT, "signature": SIGNATURE}),
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
            let (first, last) = signature.split_at(signature.len() / 2);
            for fragment in [first, last] {
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

async fn captured_continuation(streamed: bool, blocks: &[Value]) -> Value {
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
        for private in [THOUGHT, SIGNATURE, REDACTED, "omitted-thinking-signature"] {
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
    for private in [THOUGHT, SIGNATURE, REDACTED, "omitted-thinking-signature"] {
        assert!(!debug.contains(private));
    }
    assert_eq!(response.finish_reason, LlmFinishReason::ToolCalls);
    let mut continuation = response.continuation.unwrap();
    for call in response.tool_calls.unwrap().into_iter().rev() {
        continuation
            .record_tool_result(
                ToolResult::success(call.invocation_id, json!({"receipt": call.name})).unwrap(),
            )
            .unwrap();
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
    let streamed = captured_continuation(true, &blocks).await;
    assert_eq!(streamed["messages"][1]["content"], json!(blocks));
    assert_eq!(
        streamed["messages"][2]["content"][0]["tool_use_id"],
        "toolu_alpha"
    );
    assert_eq!(
        streamed["messages"][2]["content"][1]["tool_use_id"],
        "toolu_beta"
    );
    assert_eq!(streamed["messages"][2]["content"][0]["is_error"], false);
    let complete = captured_continuation(false, &blocks).await;
    assert_eq!(streamed, complete);
}
