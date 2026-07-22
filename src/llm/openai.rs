//! OpenAI-compatible chat completions (OpenAI, Grok, OpenRouter).

use crate::llm::types::{LlmResponse, StreamEvent, ToolCallRequest};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use super::LlmClient;

pub struct OpenAiCompatClient {
    client: Client,
    model: String,
    api_keys: Vec<String>,
    base_url: String,
}

impl OpenAiCompatClient {
    pub fn new(model: String, api_keys: Vec<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model,
            api_keys,
            base_url: base_url.into(),
        }
    }

    pub fn openai(model: String, api_keys: Vec<String>) -> Self {
        Self::new(
            model,
            api_keys,
            "https://api.openai.com/v1/chat/completions",
        )
    }

    pub fn xai(model: String, api_keys: Vec<String>) -> Self {
        Self::new(model, api_keys, "https://api.x.ai/v1/chat/completions")
    }

    pub fn openrouter(model: String, api_keys: Vec<String>) -> Self {
        Self::new(
            model,
            api_keys,
            "https://openrouter.ai/api/v1/chat/completions",
        )
    }

    pub fn meta(model: String, api_keys: Vec<String>) -> Self {
        Self::new(model, api_keys, "https://api.meta.com/v1/chat/completions")
    }
}

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn complete(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<LlmResponse, String> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": temperature,
        });
        if let Some(t) = tools {
            body["tools"] = json!(t);
            body["tool_choice"] = json!("auto");
        }

        let url = if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
        };

        let resp = crate::llm::send_with_retry(
            |credential_index| {
                let mut request = self.client.post(&url).json(&body);
                request = request.bearer_auth(&self.api_keys[credential_index]);
                request
            },
            self.api_keys.len(),
        )
        .await?;
        if !resp.status().is_success() {
            let text = crate::llm::read_bounded_error_response(
                resp,
                "OpenAI-compatible API error response",
            )
            .await?;
            return Err(format!("OpenAI-compatible API error: {text}"));
        }

        let data =
            crate::llm::read_bounded_json_response(resp, "OpenAI-compatible completion response")
                .await?;
        parse_openai_response(&data)
    }

    async fn stream(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<crate::llm::types::StreamEvent, String>>, String>
    {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": temperature,
            "stream": true,
        });
        if let Some(t) = tools {
            body["tools"] = json!(t);
            body["tool_choice"] = json!("auto");
        }

        let url = if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
        };

        let resp = crate::llm::send_with_retry(
            |credential_index| {
                let mut request = self.client.post(&url).json(&body);
                request = request.bearer_auth(&self.api_keys[credential_index]);
                request
            },
            self.api_keys.len(),
        )
        .await?;
        if !resp.status().is_success() {
            let text = crate::llm::read_bounded_error_response(
                resp,
                "OpenAI-compatible API error response",
            )
            .await?;
            return Err(format!("OpenAI-compatible API error: {text}"));
        }
        crate::llm::ensure_response_content_length(
            &resp,
            crate::llm::MAX_LLM_STREAM_BYTES,
            "OpenAI-compatible stream response",
        )?;

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            use eventsource_stream::Eventsource;
            use futures_util::StreamExt;

            let mut budget = crate::llm::ResponseByteBudget::new(
                crate::llm::MAX_LLM_STREAM_BYTES,
                "OpenAI-compatible stream response",
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
                            "OpenAI-compatible stream event",
                        ) {
                            let _receiver_open =
                                crate::llm::send_stream_event(&tx, Err(error)).await;
                            return;
                        }
                        if event.data == "[DONE]" {
                            let _receiver_open = crate::llm::send_stream_event(
                                &tx,
                                Ok(StreamEvent::End("stop".to_string())),
                            )
                            .await;
                            return;
                        }

                        match serde_json::from_str::<Value>(&event.data) {
                            Ok(data) => match parse_openai_stream_chunk(&data) {
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
                let _receiver_open =
                    crate::llm::send_stream_event(&tx, Ok(StreamEvent::End("stop".to_string())))
                        .await;
            }
        });

        Ok(rx)
    }
}

pub fn parse_openai_stream_chunk(data: &Value) -> Result<Vec<StreamEvent>, String> {
    let Some(choice) = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        events.push(StreamEvent::Content(content.to_string()));
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| "OpenAI tool-call delta is missing index".to_string())?
                as usize;
            let function = call.get("function").unwrap_or(&Value::Null);
            events.push(StreamEvent::ToolCallChunk {
                index,
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                arguments: function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
        if !finish.is_empty() && finish != "null" {
            events.push(StreamEvent::End(finish.to_string()));
        }
    }
    Ok(events)
}

pub fn parse_openai_response(data: &Value) -> Result<LlmResponse, String> {
    let choice = &data["choices"][0];
    let message = &choice["message"];
    let content = message
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());

    let mut tool_calls = Vec::new();
    if let Some(tcs) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
            if name.is_empty() {
                return Err("OpenAI tool call is missing a function name".to_string());
            }
            let arguments: Value = serde_json::from_str(args_str)
                .map_err(|error| format!("invalid OpenAI tool arguments: {error}"))?;
            tool_calls.push(ToolCallRequest { name, arguments });
        }
    }

    let finish = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop")
        .to_string();

    Ok(LlmResponse {
        content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        finish_reason: finish,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::{
        serve_once, serve_once_with_declared_length, serve_open_stream,
    };
    use std::time::Duration;

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
        assert!(matches!(&events[0], StreamEvent::Content(value) if value == "working"));
        assert!(matches!(
            &events[1],
            StreamEvent::ToolCallChunk { index: 0, name: Some(name), .. } if name == "read_file"
        ));
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
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\":\"README.md\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
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
                &[json!({"role": "user", "content": "inspect"})],
                Some(&[json!({"type": "function", "function": {"name": "read_file"}})]),
                0.2,
            )
            .await
            .expect("OpenAI completion");

        assert_eq!(response.content.as_deref(), Some("inspected"));
        assert_eq!(response.finish_reason, "tool_calls");
        let call = &response.tool_calls.expect("tool call")[0];
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

    #[tokio::test]
    async fn stream_posts_stream_flag_and_accumulates_text_and_tools() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"work\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"grep\",\"arguments\":\"{\\\"pattern\\\":\"}}]},\"finish_reason\":null}]}\n\n",
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
            .stream(&[json!({"role": "user", "content": "search"})], None, 0.1)
            .await
            .expect("OpenAI stream");
        let mut content = String::new();
        let mut accumulator = crate::llm::ToolCallAccumulator::default();
        let mut finish = None;
        while let Some(event) = stream.recv().await {
            let event = event.expect("valid stream event");
            if let StreamEvent::Content(fragment) = &event {
                content.push_str(fragment);
            }
            if let StreamEvent::End(reason) = &event {
                finish = Some(reason.clone());
            }
            accumulator.push(&event);
        }

        assert_eq!(content, "work");
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        let calls = accumulator.finish().expect("complete tool call");
        assert_eq!(calls[0].name, "grep");
        assert_eq!(calls[0].arguments["pattern"], "needle");
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured stream request");
        assert!(request.contains("\"stream\":true"));
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
            .stream(&[json!({"role": "user", "content": "cancel"})], None, 0.0)
            .await
            .expect("open OpenAI stream");
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
        assert!(disconnected, "OpenAI response connection remained open");
    }

    #[tokio::test]
    async fn done_sentinel_terminates_an_open_response_without_post_terminal_events() {
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
            .stream(&[json!({"role": "user", "content": "finish"})], None, 0.0)
            .await
            .expect("open OpenAI stream");

        let terminal = tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("terminal event timeout")
            .expect("terminal event")
            .expect("valid terminal event");
        assert_eq!(terminal, StreamEvent::End("stop".to_string()));
        assert!(tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("producer closure timeout")
            .is_none());

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "OpenAI terminal event left response open");
    }

    #[tokio::test]
    async fn done_sentinel_without_finish_reason_emits_one_end_event() {
        let (base_url, _) = serve_once("200 OK", "text/event-stream", "data: [DONE]\n\n");
        let client = OpenAiCompatClient::new(
            "stream-model".to_string(),
            vec!["stream-key".to_string()],
            base_url,
        );
        let mut stream = client
            .stream(&[json!({"role": "user", "content": "finish"})], None, 0.0)
            .await
            .expect("OpenAI stream");
        let mut events = Vec::new();
        while let Some(event) = stream.recv().await {
            events.push(event.expect("valid event"));
        }
        assert_eq!(events, [StreamEvent::End("stop".to_string())]);
    }

    #[tokio::test]
    async fn complete_and_stream_surface_http_errors() {
        let (base_url, _) = serve_once("401 Unauthorized", "text/plain", "bad key");
        let client =
            OpenAiCompatClient::new("model".to_string(), vec!["bad".to_string()], base_url);
        let error = client
            .complete(&[json!({"role": "user", "content": "x"})], None, 0.0)
            .await
            .expect_err("completion must reject HTTP errors");
        assert!(error.contains("bad key"));

        let (base_url, _) = serve_once("403 Forbidden", "text/plain", "forbidden");
        let client =
            OpenAiCompatClient::new("model".to_string(), vec!["bad".to_string()], base_url);
        let error = client
            .stream(&[json!({"role": "user", "content": "x"})], None, 0.0)
            .await
            .expect_err("stream must reject HTTP errors");
        assert!(error.contains("forbidden"));
    }

    #[tokio::test]
    async fn oversized_declared_response_lengths_are_rejected() {
        let (base_url, _) = serve_once_with_declared_length(
            "200 OK",
            "application/json",
            "{}",
            Some(crate::llm::MAX_LLM_COMPLETE_RESPONSE_BYTES + 1),
        );
        let client =
            OpenAiCompatClient::new("model".to_string(), vec!["key".to_string()], base_url);
        let error = client
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
        let client =
            OpenAiCompatClient::new("model".to_string(), vec!["key".to_string()], base_url);
        let error = client
            .stream(&[json!({"role": "user", "content": "x"})], None, 0.0)
            .await
            .expect_err("oversized stream must be rejected");
        assert!(error.contains("16777216-byte limit"));
    }
}
