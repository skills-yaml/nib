//! Google Gemini Generative Language API.

use crate::llm::types::{LlmResponse, StreamEvent, ToolCallRequest};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::LlmClient;

pub struct GeminiClient {
    client: Client,
    model: String,
    api_keys: Vec<String>,
    base_url: String,
}

impl GeminiClient {
    pub fn new(model: String, api_keys: Vec<String>) -> Self {
        Self {
            client: Client::new(),
            model,
            api_keys,
            base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
        }
    }

    fn request_body(&self, messages: &[Value], tools: Option<&[Value]>, temperature: f64) -> Value {
        let mut body = json!({
            "contents": gemini_contents(messages),
            "generationConfig": {"temperature": temperature},
        });
        if let Some(system) = messages
            .iter()
            .find(|message| message.get("role") == Some(&json!("system")))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
        {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }
        if let Some(declarations) = tools.map(gemini_function_declarations) {
            if !declarations.is_empty() {
                body["tools"] = json!([{"functionDeclarations": declarations}]);
            }
        }
        body
    }
}

#[async_trait]
impl LlmClient for GeminiClient {
    async fn complete(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<LlmResponse, String> {
        let body = self.request_body(messages, tools, temperature);
        let response = crate::llm::send_with_retry(
            |credential_index| {
                let url = format!(
                    "{}/models/{}:generateContent",
                    self.base_url.trim_end_matches('/'),
                    self.model
                );
                self.client
                    .post(url)
                    .header("x-goog-api-key", &self.api_keys[credential_index])
                    .json(&body)
            },
            self.api_keys.len(),
        )
        .await?;
        if !response.status().is_success() {
            let text =
                crate::llm::read_bounded_error_response(response, "Gemini API error response")
                    .await?;
            return Err(format!("Gemini API error: {text}"));
        }
        let data =
            crate::llm::read_bounded_json_response(response, "Gemini completion response").await?;
        parse_gemini_response(&data)
    }

    async fn stream(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamEvent, String>>, String> {
        let body = self.request_body(messages, tools, temperature);
        let response = crate::llm::send_with_retry(
            |credential_index| {
                let url = format!(
                    "{}/models/{}:streamGenerateContent?alt=sse",
                    self.base_url.trim_end_matches('/'),
                    self.model
                );
                self.client
                    .post(url)
                    .header("x-goog-api-key", &self.api_keys[credential_index])
                    .json(&body)
            },
            self.api_keys.len(),
        )
        .await?;
        if !response.status().is_success() {
            let text =
                crate::llm::read_bounded_error_response(response, "Gemini API error response")
                    .await?;
            return Err(format!("Gemini API error: {text}"));
        }
        crate::llm::ensure_response_content_length(
            &response,
            crate::llm::MAX_LLM_STREAM_BYTES,
            "Gemini stream response",
        )?;

        let (tx, rx) = tokio::sync::mpsc::channel(100);
        tokio::spawn(async move {
            use eventsource_stream::Eventsource;
            use futures_util::StreamExt;

            let mut budget = crate::llm::ResponseByteBudget::new(
                crate::llm::MAX_LLM_STREAM_BYTES,
                "Gemini stream response",
            );
            let bounded_bytes = response.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|error| error.to_string())?;
                budget.account(chunk.len())?;
                Ok::<_, String>(chunk)
            });
            let mut stream = bounded_bytes.eventsource();
            let mut parser = GeminiStreamParser::default();
            while let Some(event_result) =
                crate::llm::next_stream_item_or_closed(&tx, &mut stream).await
            {
                match event_result {
                    Ok(event) => {
                        if let Err(error) = crate::llm::ensure_stream_event_size(
                            event.data.len(),
                            "Gemini stream event",
                        ) {
                            let _receiver_open =
                                crate::llm::send_stream_event(&tx, Err(error)).await;
                            return;
                        }
                        match serde_json::from_str::<Value>(&event.data) {
                            Ok(data) => match parser.parse_chunk(&data) {
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
                    Err(error) => {
                        let _receiver_open =
                            crate::llm::send_stream_event(&tx, Err(error.to_string())).await;
                        return;
                    }
                }
            }
            if !tx.is_closed() {
                let _receiver_open =
                    crate::llm::send_stream_event(&tx, Ok(StreamEvent::End("stop".to_string())))
                        .await;
            }
        });
        Ok(rx)
    }
}

fn gemini_contents(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message.get("role") != Some(&json!("system")))
        .map(|message| {
            let role = if message.get("role") == Some(&json!("assistant")) {
                "model"
            } else {
                "user"
            };
            json!({
                "role": role,
                "parts": [{"text": message.get("content").and_then(Value::as_str).unwrap_or_default()}]
            })
        })
        .collect()
}

pub fn gemini_function_declarations(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| tool.get("function"))
        .filter_map(|function| {
            Some(json!({
                "name": function.get("name")?.as_str()?,
                "description": function.get("description").and_then(Value::as_str).unwrap_or_default(),
                "parameters": function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            }))
        })
        .collect()
}

pub fn parse_gemini_response(data: &Value) -> Result<LlmResponse, String> {
    let candidate = data
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .ok_or_else(|| "Gemini response is missing a candidate".to_string())?;
    let parts = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .ok_or_else(|| "Gemini response is missing content parts".to_string())?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            content.push_str(text);
        }
        if let Some(function) = part.get("functionCall") {
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "Gemini function call is missing a name".to_string())?;
            tool_calls.push(ToolCallRequest {
                name: name.to_string(),
                arguments: function.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }
    Ok(LlmResponse {
        content: (!content.is_empty()).then_some(content),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        finish_reason: candidate
            .get("finishReason")
            .and_then(Value::as_str)
            .unwrap_or("STOP")
            .to_string(),
    })
}

#[derive(Default)]
struct GeminiStreamParser {
    next_call_index: usize,
    provider_call_indexes: BTreeMap<String, usize>,
}

impl GeminiStreamParser {
    fn parse_chunk(&mut self, data: &Value) -> Result<Vec<StreamEvent>, String> {
        let Some(candidate) = data
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        if let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    events.push(StreamEvent::Content(text.to_string()));
                }
                if let Some(function) = part.get("functionCall") {
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| "Gemini function call is missing a name".to_string())?;
                    let index = self.call_index(function)?;
                    events.push(StreamEvent::ToolCallChunk {
                        index,
                        name: Some(name.to_string()),
                        arguments: Some(
                            function
                                .get("args")
                                .cloned()
                                .unwrap_or_else(|| json!({}))
                                .to_string(),
                        ),
                    });
                }
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            if !reason.is_empty() && reason != "null" {
                events.push(StreamEvent::End(reason.to_string()));
            }
        }
        Ok(events)
    }

    fn call_index(&mut self, function: &Value) -> Result<usize, String> {
        let provider_id = function
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        if let Some(index) = provider_id.and_then(|id| self.provider_call_indexes.get(id)) {
            return Ok(*index);
        }

        let index = self.next_call_index;
        self.next_call_index = self
            .next_call_index
            .checked_add(1)
            .ok_or_else(|| "Gemini stream contains too many function calls".to_string())?;
        if let Some(provider_id) = provider_id {
            self.provider_call_indexes
                .insert(provider_id.to_string(), index);
        }
        Ok(index)
    }
}

pub fn parse_gemini_stream_chunk(data: &Value) -> Result<Vec<StreamEvent>, String> {
    GeminiStreamParser::default().parse_chunk(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::{
        serve_once, serve_once_with_declared_length, serve_open_stream,
    };
    use std::time::Duration;

    fn test_client(base_url: String) -> GeminiClient {
        GeminiClient {
            client: Client::new(),
            model: "gemini-test".to_string(),
            api_keys: vec!["gemini-test-key".to_string()],
            base_url: format!("{base_url}/v1beta"),
        }
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
        })]);
        assert_eq!(declarations[0]["name"], "read_file");
        assert_eq!(declarations[0]["parameters"]["required"][0], "path");
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
            StreamEvent::ToolCallChunk { name: Some(name), arguments: Some(arguments), .. }
                if name == "grep" && arguments.contains("needle")
        ));
        assert_eq!(events[1], StreamEvent::End("STOP".to_string()));
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
                }]
            }))
            .expect("second Gemini chunk");

        assert_eq!(parser.provider_call_indexes["provider-call-one"], 0);
        assert_eq!(parser.provider_call_indexes["provider-call-two"], 1);
        let mut accumulator = crate::llm::ToolCallAccumulator::default();
        for event in first.iter().chain(&second) {
            accumulator.push(event);
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
            StreamEvent::ToolCallChunk { index: 0, .. }
        ));
        assert!(matches!(
            second[0],
            StreamEvent::ToolCallChunk { index: 1, .. }
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
                }]
            })
            .to_string(),
        );
        let response = test_client(base_url)
            .complete(
                &[
                    json!({"role": "system", "content": "follow project rules"}),
                    json!({"role": "user", "content": "search"}),
                    json!({"role": "assistant", "content": "working"}),
                ],
                Some(&[json!({
                    "type": "function",
                    "function": {
                        "name": "grep",
                        "description": "search files",
                        "parameters": {"type": "object", "required": ["pattern"]}
                    }
                })]),
                0.4,
            )
            .await
            .expect("Gemini completion");

        assert_eq!(response.content.as_deref(), Some("checking"));
        assert_eq!(response.finish_reason, "STOP");
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
        assert!(request.contains("\"systemInstruction\""));
        assert!(request.contains("\"functionDeclarations\""));
        assert!(request.contains("\"role\":\"model\""));
    }

    #[tokio::test]
    async fn stream_consumes_sse_text_and_function_calls() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"work\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}}}]},\"finishReason\":\"STOP\"}]}\n\n"
        );
        let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", body);
        let mut stream = test_client(base_url)
            .stream(&[json!({"role": "user", "content": "read"})], None, 0.0)
            .await
            .expect("Gemini stream");
        let mut content = String::new();
        let mut accumulator = crate::llm::ToolCallAccumulator::default();
        let mut finish = None;
        while let Some(event) = stream.recv().await {
            let event = event.expect("valid Gemini event");
            if let StreamEvent::Content(fragment) = &event {
                content.push_str(fragment);
            }
            if let StreamEvent::End(reason) = &event {
                finish = Some(reason.clone());
            }
            accumulator.push(&event);
        }

        assert_eq!(content, "work");
        assert_eq!(finish.as_deref(), Some("STOP"));
        let calls = accumulator.finish().expect("streamed Gemini call");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "README.md");
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured Gemini stream request");
        assert!(request
            .starts_with("POST /v1beta/models/gemini-test:streamGenerateContent?alt=sse HTTP/1.1"));
    }

    #[tokio::test]
    async fn dropping_stream_receiver_closes_the_open_http_response() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"first\"}]}}]}\n\n",
        );
        let mut stream = test_client(base_url)
            .stream(&[json!({"role": "user", "content": "cancel"})], None, 0.0)
            .await
            .expect("open Gemini stream");
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
        assert!(disconnected, "Gemini response connection remained open");
    }

    #[tokio::test]
    async fn terminal_chunk_terminates_an_open_response_without_post_terminal_events() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"late\"}]}}]}\n\n"
        ));
        let mut stream = test_client(base_url)
            .stream(&[json!({"role": "user", "content": "finish"})], None, 0.0)
            .await
            .expect("open Gemini stream");

        let terminal = tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("terminal event timeout")
            .expect("terminal event")
            .expect("valid terminal event");
        assert_eq!(terminal, StreamEvent::End("STOP".to_string()));
        assert!(tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("producer closure timeout")
            .is_none());

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "Gemini terminal event left response open");
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
