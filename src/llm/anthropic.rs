//! Anthropic Messages API client.

use crate::llm::types::{LlmResponse, StreamEvent, ToolCallRequest};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use super::LlmClient;

pub struct AnthropicClient {
    client: Client,
    model: String,
    api_keys: Vec<String>,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(model: String, api_keys: Vec<String>) -> Self {
        Self {
            client: Client::new(),
            model,
            api_keys,
            base_url: "https://api.anthropic.com/v1/messages".to_string(),
        }
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<LlmResponse, String> {
        let system = messages
            .iter()
            .find(|m| m.get("role") == Some(&json!("system")))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("You are nib, a coding agent.");

        let user_messages: Vec<Value> = messages
            .iter()
            .filter(|m| m.get("role") != Some(&json!("system")))
            .map(|m| {
                json!({
                    "role": if m.get("role") == Some(&json!("assistant")) { "assistant" } else { "user" },
                    "content": m.get("content").unwrap_or(&json!("")),
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "temperature": temperature,
            "system": system,
            "messages": user_messages,
        });

        if let Some(t) = tools {
            let anthropic_tools: Vec<Value> = t
                .iter()
                .filter_map(|tool| {
                    let f = tool.get("function")?;
                    Some(json!({
                        "name": f.get("name")?,
                        "description": f.get("description").unwrap_or(&json!("")),
                        "input_schema": f.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
                    }))
                })
                .collect();
            body["tools"] = json!(anthropic_tools);
        }

        let resp = crate::llm::send_with_retry(
            |credential_index| {
                self.client
                    .post(&self.base_url)
                    .header("x-api-key", &self.api_keys[credential_index])
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
            },
            self.api_keys.len(),
        )
        .await?;

        if !resp.status().is_success() {
            let text =
                crate::llm::read_bounded_error_response(resp, "Anthropic API error response")
                    .await?;
            return Err(format!("Anthropic API error: {text}"));
        }

        let data =
            crate::llm::read_bounded_json_response(resp, "Anthropic completion response").await?;
        parse_anthropic_response(&data)
    }

    async fn stream(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<crate::llm::types::StreamEvent, String>>, String>
    {
        let system = messages
            .iter()
            .find(|m| m.get("role") == Some(&json!("system")))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("You are nib, a coding agent.");

        let user_messages: Vec<Value> = messages
            .iter()
            .filter(|m| m.get("role") != Some(&json!("system")))
            .map(|m| {
                json!({
                    "role": if m.get("role") == Some(&json!("assistant")) { "assistant" } else { "user" },
                    "content": m.get("content").unwrap_or(&json!("")),
                })
            })
            .collect();

        let mut body = json!({
            "model": self.model,
            "max_tokens": 4096,
            "temperature": temperature,
            "system": system,
            "messages": user_messages,
            "stream": true,
        });

        if let Some(t) = tools {
            let anthropic_tools: Vec<Value> = t
                .iter()
                .filter_map(|tool| {
                    let f = tool.get("function")?;
                    Some(json!({
                        "name": f.get("name")?,
                        "description": f.get("description").unwrap_or(&json!("")),
                        "input_schema": f.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
                    }))
                })
                .collect();
            body["tools"] = json!(anthropic_tools);
        }

        let resp = crate::llm::send_with_retry(
            |credential_index| {
                self.client
                    .post(&self.base_url)
                    .header("x-api-key", &self.api_keys[credential_index])
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
            },
            self.api_keys.len(),
        )
        .await?;

        if !resp.status().is_success() {
            let text =
                crate::llm::read_bounded_error_response(resp, "Anthropic API error response")
                    .await?;
            return Err(format!("Anthropic API error: {text}"));
        }
        crate::llm::ensure_response_content_length(
            &resp,
            crate::llm::MAX_LLM_STREAM_BYTES,
            "Anthropic stream response",
        )?;

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            use eventsource_stream::Eventsource;
            use futures_util::StreamExt;

            let mut budget = crate::llm::ResponseByteBudget::new(
                crate::llm::MAX_LLM_STREAM_BYTES,
                "Anthropic stream response",
            );
            let bounded_bytes = resp.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|error| error.to_string())?;
                budget.account(chunk.len())?;
                Ok::<_, String>(chunk)
            });
            let mut stream = bounded_bytes.eventsource();
            while let Some(event_res) =
                crate::llm::next_stream_item_or_closed(&tx, &mut stream).await
            {
                match event_res {
                    Ok(event) => {
                        if let Err(error) = crate::llm::ensure_stream_event_size(
                            event.data.len(),
                            "Anthropic stream event",
                        ) {
                            let _receiver_open =
                                crate::llm::send_stream_event(&tx, Err(error)).await;
                            return;
                        }
                        let ev_type = event.event;
                        if ev_type == "message_stop" {
                            let _receiver_open = crate::llm::send_stream_event(
                                &tx,
                                Ok(StreamEvent::End("end_turn".to_string())),
                            )
                            .await;
                            return;
                        }

                        match serde_json::from_str::<Value>(&event.data) {
                            Ok(data) => match parse_anthropic_stream_event(&ev_type, &data) {
                                Ok(events) => {
                                    for parsed in events {
                                        let is_terminal = matches!(parsed, StreamEvent::End(_));
                                        if !crate::llm::send_stream_event(&tx, Ok(parsed)).await {
                                            return;
                                        }
                                        if is_terminal {
                                            return;
                                        }
                                    }
                                }
                                Err(error) => {
                                    let _receiver_open =
                                        crate::llm::send_stream_event(&tx, Err(error)).await;
                                    return;
                                }
                            },
                            Err(error) => {
                                let _receiver_open =
                                    crate::llm::send_stream_event(&tx, Err(error.to_string()))
                                        .await;
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _receiver_open =
                            crate::llm::send_stream_event(&tx, Err(e.to_string())).await;
                        return;
                    }
                }
            }
            if !tx.is_closed() {
                let _receiver_open = crate::llm::send_stream_event(
                    &tx,
                    Ok(StreamEvent::End("end_turn".to_string())),
                )
                .await;
            }
        });

        Ok(rx)
    }
}

pub fn parse_anthropic_stream_event(
    event_type: &str,
    data: &Value,
) -> Result<Vec<StreamEvent>, String> {
    let mut events = Vec::new();
    match event_type {
        "content_block_start" => {
            if let Some(block) = data.get("content_block") {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let index = data
                        .get("index")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| "Anthropic tool block is missing index".to_string())?
                        as usize;
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "Anthropic tool block is missing name".to_string())?;
                    events.push(StreamEvent::ToolCallChunk {
                        index,
                        name: Some(name.to_string()),
                        arguments: Some(String::new()),
                    });
                }
            }
        }
        "content_block_delta" => {
            let delta = data
                .get("delta")
                .ok_or_else(|| "Anthropic content delta is missing delta".to_string())?;
            let index = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                events.push(StreamEvent::Content(text.to_string()));
            }
            if let Some(arguments) = delta.get("partial_json").and_then(Value::as_str) {
                events.push(StreamEvent::ToolCallChunk {
                    index,
                    name: None,
                    arguments: Some(arguments.to_string()),
                });
            }
        }
        "message_delta" => {
            if let Some(reason) = data
                .get("delta")
                .and_then(|delta| delta.get("stop_reason"))
                .and_then(Value::as_str)
            {
                events.push(StreamEvent::End(reason.to_string()));
            }
        }
        _ => {}
    }
    Ok(events)
}

pub fn parse_anthropic_response(data: &Value) -> Result<LlmResponse, String> {
    let content_blocks = data["content"].as_array().ok_or("missing content")?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    for block in content_blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let name = block["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or("Anthropic tool use is missing a name")?;
                tool_calls.push(ToolCallRequest {
                    name: name.to_string(),
                    arguments: block.get("input").cloned().unwrap_or(json!({})),
                });
            }
            _ => {}
        }
    }

    Ok(LlmResponse {
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        finish_reason: data["stop_reason"]
            .as_str()
            .unwrap_or("end_turn")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::{
        serve_once, serve_once_with_declared_length, serve_open_stream,
    };
    use std::time::Duration;

    fn test_client(base_url: String) -> AnthropicClient {
        AnthropicClient {
            client: Client::new(),
            model: "claude-test".to_string(),
            api_keys: vec!["anthropic-test-key".to_string()],
            base_url: format!("{base_url}/v1/messages"),
        }
    }

    #[test]
    fn parses_streamed_tool_start_and_arguments() {
        let start = parse_anthropic_stream_event(
            "content_block_start",
            &json!({"index": 2, "content_block": {"type": "tool_use", "name": "grep"}}),
        )
        .unwrap();
        let delta = parse_anthropic_stream_event(
            "content_block_delta",
            &json!({"index": 2, "delta": {"partial_json": "{\"pattern\":\"x\"}"}}),
        )
        .unwrap();
        assert!(matches!(
            &start[0],
            StreamEvent::ToolCallChunk { index: 2, name: Some(name), .. } if name == "grep"
        ));
        assert!(matches!(
            &delta[0],
            StreamEvent::ToolCallChunk { index: 2, arguments: Some(arguments), .. }
                if arguments.contains("pattern")
        ));
    }

    #[tokio::test]
    async fn complete_posts_anthropic_payload_and_parses_mixed_content() {
        let (base_url, request_rx) = serve_once(
            "200 OK",
            "application/json",
            json!({
                "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "name": "grep", "input": {"pattern": "needle"}}
                ],
                "stop_reason": "tool_use"
            })
            .to_string(),
        );
        let response = test_client(base_url)
            .complete(
                &[
                    json!({"role": "system", "content": "follow project rules"}),
                    json!({"role": "user", "content": "search"}),
                ],
                Some(&[json!({
                    "type": "function",
                    "function": {
                        "name": "grep",
                        "description": "search files",
                        "parameters": {"type": "object", "required": ["pattern"]}
                    }
                })]),
                0.3,
            )
            .await
            .expect("Anthropic completion");

        assert_eq!(response.content.as_deref(), Some("checking"));
        assert_eq!(response.finish_reason, "tool_use");
        let call = &response.tool_calls.expect("tool use")[0];
        assert_eq!(call.name, "grep");
        assert_eq!(call.arguments["pattern"], "needle");

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured Anthropic request");
        assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-api-key: anthropic-test-key"));
        assert!(request.contains("anthropic-version: 2023-06-01"));
        assert!(request.contains("\"system\":\"follow project rules\""));
        assert!(request.contains("\"input_schema\""));
        assert!(!request.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn stream_consumes_named_sse_events_and_partial_tool_json() {
        let body = concat!(
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"text\":\"work\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"name\":\"read_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"partial_json\":\"\\\"README.md\\\"}\"}}\n\n",
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\n",
            "data: {}\n\n"
        );
        let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", body);
        let mut stream = test_client(base_url)
            .stream(&[json!({"role": "user", "content": "read"})], None, 0.0)
            .await
            .expect("Anthropic stream");
        let mut content = String::new();
        let mut accumulator = crate::llm::ToolCallAccumulator::default();
        let mut finish = None;
        while let Some(event) = stream.recv().await {
            let event = event.expect("valid Anthropic event");
            if let StreamEvent::Content(fragment) = &event {
                content.push_str(fragment);
            }
            if let StreamEvent::End(reason) = &event {
                finish = Some(reason.clone());
            }
            accumulator.push(&event);
        }

        assert_eq!(content, "work");
        assert_eq!(finish.as_deref(), Some("tool_use"));
        let calls = accumulator.finish().expect("streamed tool call");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "README.md");
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured Anthropic stream request");
        assert!(request.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn dropping_stream_receiver_closes_the_open_http_response() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"text\":\"first\"}}\n\n"
        ));
        let mut stream = test_client(base_url)
            .stream(&[json!({"role": "user", "content": "cancel"})], None, 0.0)
            .await
            .expect("open Anthropic stream");
        let first = tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("first event timeout")
            .expect("first event")
            .expect("valid first event");
        assert_eq!(first, StreamEvent::Content("first".to_string()));
        drop(stream);

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "Anthropic response connection remained open");
    }

    #[tokio::test]
    async fn terminal_delta_terminates_an_open_response_without_post_terminal_events() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"text\":\"late\"}}\n\n"
        ));
        let mut stream = test_client(base_url)
            .stream(&[json!({"role": "user", "content": "finish"})], None, 0.0)
            .await
            .expect("open Anthropic stream");

        let terminal = tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("terminal event timeout")
            .expect("terminal event")
            .expect("valid terminal event");
        assert_eq!(terminal, StreamEvent::End("end_turn".to_string()));
        assert!(tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("producer closure timeout")
            .is_none());

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "Anthropic terminal event left response open");
    }

    #[tokio::test]
    async fn message_stop_without_delta_emits_one_end_event() {
        let (base_url, _) = serve_once(
            "200 OK",
            "text/event-stream",
            "event: message_stop\ndata: {}\n\n",
        );
        let mut stream = test_client(base_url)
            .stream(&[json!({"role": "user", "content": "finish"})], None, 0.0)
            .await
            .expect("Anthropic stream");
        let mut events = Vec::new();
        while let Some(event) = stream.recv().await {
            events.push(event.expect("valid event"));
        }
        assert_eq!(events, [StreamEvent::End("end_turn".to_string())]);
    }

    #[tokio::test]
    async fn complete_and_stream_surface_http_errors() {
        let (base_url, _) = serve_once("401 Unauthorized", "text/plain", "invalid key");
        let error = test_client(base_url)
            .complete(&[json!({"role": "user", "content": "x"})], None, 0.0)
            .await
            .expect_err("completion must reject HTTP error");
        assert!(error.contains("invalid key"));

        let (base_url, _) = serve_once("403 Forbidden", "text/plain", "not allowed");
        let error = test_client(base_url)
            .stream(&[json!({"role": "user", "content": "x"})], None, 0.0)
            .await
            .expect_err("stream must reject HTTP error");
        assert!(error.contains("not allowed"));
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
            .complete(&[json!({"role": "user", "content": "x"})], None, 0.0)
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
            .stream(&[json!({"role": "user", "content": "x"})], None, 0.0)
            .await
            .expect_err("oversized stream must be rejected");
        assert!(error.contains("16777216-byte limit"));
    }
}
