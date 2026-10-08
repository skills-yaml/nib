//! Capped requests retain provider-controlled thinking and total output bounds (T069).
use super::*;

const PRIVATE_SIGNATURE: &str = "bounded-thinking-signature";

fn bounded_response(streamed: bool, stop_reason: &str) -> String {
    if !streamed {
        return json!({
            "content": [
                {"type": "thinking", "thinking": "", "signature": PRIVATE_SIGNATURE},
                {"type": "text", "text": "receipt"}
            ],
            "stop_reason": stop_reason
        })
        .to_string();
    }
    let events = [
        (
            "message_start",
            json!({"message": {"role": "assistant", "content": []}}),
        ),
        (
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        ),
        (
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "signature_delta", "signature": PRIVATE_SIGNATURE}}),
        ),
        ("content_block_stop", json!({"index": 0})),
        (
            "content_block_start",
            json!({"index": 1, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"index": 1, "delta": {"type": "text_delta", "text": "receipt"}}),
        ),
        ("content_block_stop", json!({"index": 1})),
        (
            "message_delta",
            json!({"delta": {"stop_reason": stop_reason}}),
        ),
        ("message_stop", json!({})),
    ];
    events
        .iter()
        .map(|(kind, data)| format!("event: {kind}\ndata: {data}\n\n"))
        .collect()
}

async fn capped_text_turn(streamed: bool, stop_reason: &str) -> Result<LlmResponse, Box<LlmError>> {
    let (endpoint, requests) = serve_once(
        "200 OK",
        if streamed {
            "text/event-stream"
        } else {
            "application/json"
        },
        bounded_response(streamed, stop_reason),
    );
    let model = crate::llm::registry::provider_descriptor("anthropic")
        .unwrap()
        .default_model();
    let client = crate::llm::anthropic::AnthropicClient::with_base_url(
        model.to_string(),
        vec![ACTIVE_CONFORMANCE_KEY.to_string()],
        endpoint,
    )
    .unwrap();
    let messages = [LlmMessage::user("Return one synthetic receipt")];
    let request = LlmRequest::new(&messages, None).with_max_output_tokens(512);
    let result = if streamed {
        client.stream(request).await.unwrap().finish().await
    } else {
        client.complete(request).await
    };
    let captured = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    let body: Value = serde_json::from_str(captured.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["model"], model);
    assert_eq!(body["max_tokens"], 512);
    assert_eq!(
        body.get("stream").and_then(Value::as_bool),
        streamed.then_some(true)
    );
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
    assert!(!format!("{result:?}").contains(PRIVATE_SIGNATURE));
    result.map_err(Box::new)
}

#[tokio::test]
async fn capped_complete_and_stream_text_preserve_default_thinking_and_caps() {
    for streamed in [false, true] {
        let response = capped_text_turn(streamed, "end_turn").await.unwrap();
        assert_eq!(response.content.as_deref(), Some("receipt"));
        assert_eq!(response.terminal_status, LlmTerminalStatus::Completed);
        assert!(response.tool_calls.is_none());
        assert!(response.continuation.is_none());
    }
}

#[tokio::test]
async fn capped_thinking_exhaustion_and_refusal_never_authorize_tools() {
    for streamed in [false, true] {
        let error = capped_text_turn(streamed, "max_tokens").await.unwrap_err();
        assert_eq!(error.class, LlmErrorClass::ProviderRejected);
        let refused = capped_text_turn(streamed, "refusal").await.unwrap();
        assert_eq!(refused.terminal_status, LlmTerminalStatus::Refused);
        assert!(refused.tool_calls.is_none());
        assert!(refused.continuation.is_none());
    }
}

#[tokio::test]
async fn zero_anthropic_output_cap_is_rejected_before_io() {
    let client = crate::llm::anthropic::AnthropicClient::with_base_url(
        "claude-opus-5-5".to_string(),
        vec![ACTIVE_CONFORMANCE_KEY.to_string()],
        "http://127.0.0.1:9",
    )
    .unwrap();
    let messages = [LlmMessage::user("Return one synthetic receipt")];
    for streamed in [false, true] {
        let request = LlmRequest::new(&messages, None).with_max_output_tokens(0);
        let error = if streamed {
            client.stream(request).await.unwrap_err()
        } else {
            client.complete(request).await.unwrap_err()
        };
        assert_eq!(error.class, LlmErrorClass::UnsupportedRequest);
        assert_eq!(error.phase, LlmErrorPhase::Request);
    }
}
