//! Multi-round wire history, including the third request that exposed T090.
use super::*;

async fn turn(
    client: &dyn LlmProvider,
    request: crate::llm::LlmRequest<'_>,
    streamed: bool,
) -> LlmResponse {
    if !streamed {
        return client.complete(request).await.unwrap();
    }
    let mut stream = client.stream(request).await.unwrap();
    while let Some(event) = stream.recv_private().await {
        event.unwrap();
    }
    stream.finish().await.unwrap()
}

#[tokio::test]
async fn multi_round_history_keeps_every_prior_exchange_once() {
    for adapter in NETWORK_ADAPTERS {
        for streamed in [false, true] {
            let (endpoint, requests) = serve_sequence(
                (0..4)
                    .map(|_| parallel_tool_fixture(adapter, streamed))
                    .collect(),
            );
            let client = adapter.client(endpoint);
            let messages = [
                LlmMessage::system("fixed system"),
                LlmMessage::user("repo status"),
            ];
            let scope = LlmRequestScope::new("history", "run").unwrap();
            let tools = ["probe_alpha", "probe_beta"].map(|name| {
                ToolDefinition::new(name, "Inspect", json!({"type":"object"})).unwrap()
            });
            let mut continuation = None;
            let mut previous = Vec::new();
            let mut replay_bound = 0;
            for round in 0..4 {
                let mut response = turn(
                    client.as_ref(),
                    crate::llm::LlmRequest::new(&messages, Some(&tools))
                        .with_scope(scope.clone())
                        .with_continuation(continuation),
                    streamed,
                )
                .await;
                let raw = requests.recv_timeout(Duration::from_secs(5)).unwrap();
                let body: Value =
                    serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
                let key = match adapter {
                    ConformanceAdapter::Responses(_) => "input",
                    ConformanceAdapter::Gemini => "contents",
                    _ => "messages",
                };
                let history = body[key].as_array().unwrap();
                if round > 0 {
                    assert!(
                        history.len() > previous.len(),
                        "{adapter:?} stream={streamed} round={round}: history lost"
                    );
                    assert_eq!(
                        &history[..previous.len()],
                        &previous,
                        "{adapter:?} changed prefix"
                    );
                }
                if round > 0 {
                    let base_len = match adapter {
                        ConformanceAdapter::Anthropic | ConformanceAdapter::Gemini => 1,
                        _ => messages.len(),
                    };
                    let native_bytes = serde_json::to_vec(&history[base_len..]).unwrap().len();
                    assert!(
                        native_bytes <= replay_bound,
                        "{adapter:?}: native replay exceeded conservative bound"
                    );
                }
                // Each result carries a round-specific marker, which must survive
                // exactly once in every subsequent request.
                let encoded = history.iter().map(Value::to_string).collect::<String>();
                for earlier in 0..round {
                    for slot in 0..2 {
                        assert_eq!(
                            encoded
                                .matches(&format!("receipt-{earlier}-{slot}"))
                                .count(),
                            1
                        );
                    }
                }
                previous = history.clone();
                let mut next = response.continuation.take().unwrap();
                for (slot, call) in response.tool_calls.take().unwrap().into_iter().enumerate() {
                    let result = if slot == 0 {
                        ToolResult::success
                    } else {
                        ToolResult::error
                    };
                    next.record_tool_result(
                        result(
                            call.invocation_id,
                            json!({"receipt": format!("receipt-{round}-{slot}"), "escaped": "\"\\".repeat(1024)}),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
                replay_bound = next.encoded_bytes();
                assert_eq!(
                    crate::context::snapshot::continuation_dispatch(1000, 100, 0, 0),
                    crate::context::snapshot::ContinuationDispatch::Send
                );
                assert_eq!(
                    crate::context::snapshot::continuation_dispatch(
                        1000,
                        100,
                        replay_bound,
                        next.unresolved_count()
                    ),
                    crate::context::snapshot::ContinuationDispatch::Rebuild
                );
                continuation = Some(next);
            }
        }
    }
}
