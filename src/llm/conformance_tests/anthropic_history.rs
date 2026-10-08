//! Repeated native tool batches retain the signed conversation prefix (T069).
use super::anthropic_continuation::{
    assert_private_thinking_redacted, native_blocks, native_stream,
};
use super::*;

fn tool_response(streamed: bool, blocks: &[Value]) -> ScriptedHttpResponse {
    ScriptedHttpResponse::new(
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
    )
}

fn client(endpoint: String) -> crate::llm::anthropic::AnthropicClient {
    crate::llm::anthropic::AnthropicClient::with_base_url(
        crate::llm::registry::provider_descriptor("anthropic")
            .unwrap()
            .default_model()
            .to_string(),
        vec![ACTIVE_CONFORMANCE_KEY.to_string()],
        endpoint,
    )
    .unwrap()
}

async fn execute(
    client: &crate::llm::anthropic::AnthropicClient,
    request: crate::llm::LlmRequest<'_>,
    streamed: bool,
) -> Result<LlmResponse, Box<LlmError>> {
    if !streamed {
        return client.complete(request).await.map_err(Box::new);
    }
    let mut stream = client.stream(request).await?;
    while let Some(event) = stream.recv_private().await {
        if let Ok(event) = event {
            assert_private_thinking_redacted(&format!("{event:?}"));
        }
    }
    stream.finish().await.map_err(Box::new)
}

fn resolve_tools(mut response: LlmResponse) -> crate::llm::ProviderContinuation {
    assert_private_thinking_redacted(&format!("{response:?}"));
    let mut continuation = response.continuation.take().unwrap();
    for call in response.tool_calls.take().unwrap().into_iter().rev() {
        continuation
            .record_tool_result(
                ToolResult::success(call.invocation_id, json!({"receipt": call.name})).unwrap(),
            )
            .unwrap();
    }
    continuation
}

fn captured_body(requests: &Receiver<String>) -> Value {
    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn repeated_tool_batches_preserve_exact_prefix_for_complete_and_stream() {
    for streamed in [false, true] {
        let first_blocks = native_blocks();
        let mut second_blocks = native_blocks();
        second_blocks[2]["id"] = json!("toolu_next_alpha");
        second_blocks[5]["id"] = json!("toolu_next_beta");
        second_blocks[0]["signature"] = json!("second-turn-signature");
        let final_response = ScriptedHttpResponse::new(
            "200 OK",
            "application/json",
            json!({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"})
                .to_string(),
        );
        let (endpoint, requests) = serve_sequence(vec![
            tool_response(streamed, &first_blocks),
            tool_response(streamed, &second_blocks),
            final_response,
        ]);
        let client = client(endpoint);
        let messages = [
            LlmMessage::system("fixed system prefix"),
            LlmMessage::user("inspect slots"),
        ];
        let tools = ["probe_alpha", "probe_beta"].map(|name| {
            ToolDefinition::new(name, "Inspect slot", json!({"type": "object"})).unwrap()
        });
        let scope = LlmRequestScope::new("history-session", "history-run").unwrap();
        let mut continuation = None;
        for _ in 0..2 {
            let response = execute(
                &client,
                crate::llm::LlmRequest::new(&messages, Some(&tools))
                    .with_scope(scope.clone())
                    .with_max_output_tokens(512)
                    .with_continuation(continuation),
                streamed,
            )
            .await
            .unwrap();
            continuation = Some(resolve_tools(response));
        }
        let response = client
            .complete(
                crate::llm::LlmRequest::new(&messages, Some(&tools))
                    .with_scope(scope)
                    .with_max_output_tokens(512)
                    .with_continuation(continuation),
            )
            .await
            .unwrap();
        assert_eq!(response.content.as_deref(), Some("done"));
        let first = captured_body(&requests);
        let second = captured_body(&requests);
        let third = captured_body(&requests);
        let first_messages = first["messages"].as_array().unwrap();
        let second_messages = second["messages"].as_array().unwrap();
        let third_messages = third["messages"].as_array().unwrap();
        assert_eq!(second_messages.len(), 3);
        assert_eq!(third_messages.len(), 5);
        assert_eq!(&second_messages[..first_messages.len()], first_messages);
        assert_eq!(&third_messages[..second_messages.len()], second_messages);
        assert_eq!(third_messages[1]["content"], json!(first_blocks));
        assert_eq!(third_messages[3]["content"], json!(second_blocks));
        for body in [&second, &third] {
            assert_eq!(body["system"], first["system"]);
            assert_eq!(body["tools"], first["tools"]);
            assert_eq!(body["max_tokens"], 512);
            assert!(body.get("thinking").is_none());
        }
        assert_eq!(
            third_messages[4]["content"][0]["tool_use_id"],
            "toolu_next_alpha"
        );
        assert_eq!(
            third_messages[4]["content"][1]["tool_use_id"],
            "toolu_next_beta"
        );
    }
}

async fn assert_cumulative_limit(blocks: Vec<Value>, expected: &str) {
    for streamed in [false, true] {
        let (endpoint, requests) = serve_sequence(vec![
            tool_response(streamed, &blocks),
            tool_response(streamed, &blocks),
        ]);
        let client = client(endpoint);
        let messages = [LlmMessage::user("exercise cumulative limits")];
        let scope = LlmRequestScope::new("limit-session", "limit-run").unwrap();
        let response = execute(
            &client,
            crate::llm::LlmRequest::new(&messages, None).with_scope(scope.clone()),
            streamed,
        )
        .await
        .unwrap();
        let continuation = resolve_tools(response);
        let error = execute(
            &client,
            crate::llm::LlmRequest::new(&messages, None)
                .with_scope(scope)
                .with_continuation(Some(continuation)),
            streamed,
        )
        .await
        .expect_err("cumulative history must fail closed");
        assert!(error.contains("continuation"));
        assert!(error.contains(expected));
        assert_private_thinking_redacted(&format!("{error:?}"));
        captured_body(&requests);
        captured_body(&requests);
    }
}

#[tokio::test]
async fn cumulative_native_history_enforces_item_limit() {
    let mut blocks = vec![native_blocks()[4].clone(); 128];
    blocks.push(native_blocks()[2].clone());
    assert_cumulative_limit(blocks, "item limit").await;
}

#[tokio::test]
async fn cumulative_native_history_enforces_byte_limit() {
    let mut blocks = vec![native_blocks()[0].clone(), native_blocks()[2].clone()];
    blocks[0]["signature"] = json!("s".repeat(crate::llm::types::MAX_CONTINUATION_BYTES / 2 + 1));
    assert_cumulative_limit(blocks, "byte limit").await;
}

#[tokio::test]
async fn required_to_auto_tool_turns_preserve_absent_system_prefix() {
    for streamed in [false, true] {
        let blocks = native_blocks();
        let final_response = ScriptedHttpResponse::new(
            "200 OK",
            "application/json",
            json!({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"})
                .to_string(),
        );
        let (endpoint, requests) = serve_sequence(vec![
            tool_response(streamed, &blocks),
            tool_response(streamed, &blocks),
            final_response,
        ]);
        let client = client(endpoint);
        let messages = [LlmMessage::user(
            "inspect slots without changing the prefix",
        )];
        let tools = ["probe_alpha", "probe_beta"].map(|name| {
            ToolDefinition::new(name, "Inspect slot", json!({"type": "object"})).unwrap()
        });
        let scope = LlmRequestScope::new("required-session", "required-run").unwrap();
        let mut continuation = None;
        for turn in 0..2 {
            let response = execute(
                &client,
                crate::llm::LlmRequest::new(&messages, Some(&tools))
                    .with_scope(scope.clone())
                    .with_max_output_tokens(512)
                    .with_continuation(continuation)
                    .with_tool_choice(if turn == 0 {
                        crate::llm::ToolChoice::Required
                    } else {
                        crate::llm::ToolChoice::Auto
                    }),
                streamed,
            )
            .await
            .unwrap();
            continuation = Some(resolve_tools(response));
        }
        client
            .complete(
                crate::llm::LlmRequest::new(&messages, Some(&tools))
                    .with_scope(scope)
                    .with_max_output_tokens(512)
                    .with_continuation(continuation),
            )
            .await
            .unwrap();
        let first = captured_body(&requests);
        let second = captured_body(&requests);
        let third = captured_body(&requests);
        for body in [&first, &second, &third] {
            assert!(body.get("system").is_none());
            assert_eq!(body["tools"], first["tools"]);
        }
        assert_eq!(first["output_config"]["effort"], "low");
        assert!(second.get("output_config").is_none());
        let earlier = second["messages"].as_array().unwrap();
        let later = third["messages"].as_array().unwrap();
        assert_eq!(&later[..earlier.len()], earlier);
    }
}
