//! OpenAI-compatible chat completions (OpenAI, Grok, OpenRouter).

use crate::llm::types::{LlmResponse, ToolCallRequest};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use super::LlmClient;

pub struct OpenAiCompatClient {
    client: Client,
    model: String,
    api_key: Option<String>,
    base_url: String,
}

impl OpenAiCompatClient {
    pub fn new(model: String, api_key: Option<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            model,
            api_key,
            base_url: base_url.into(),
        }
    }

    pub fn openai(model: String, api_key: Option<String>) -> Self {
        Self::new(model, api_key, "https://api.openai.com/v1/chat/completions")
    }

    pub fn xai(model: String, api_key: Option<String>) -> Self {
        Self::new(model, api_key, "https://api.x.ai/v1/chat/completions")
    }

    pub fn openrouter(model: String, api_key: Option<String>) -> Self {
        Self::new(
            model,
            api_key,
            "https://openrouter.ai/api/v1/chat/completions",
        )
    }

    pub fn meta(model: String, api_key: Option<String>) -> Self {
        Self::new(model, api_key, "https://api.meta.com/v1/chat/completions")
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

        let mut req = self.client.post(url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("OpenAI-compatible API error: {text}"));
        }

        let data: Value = resp.json().await.map_err(|e| e.to_string())?;
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

        let mut req = self.client.post(url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("OpenAI-compatible API error: {text}"));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            use eventsource_stream::Eventsource;
            use futures_util::StreamExt;

            let mut stream = resp.bytes_stream().eventsource();
            while let Some(event_res) = stream.next().await {
                match event_res {
                    Ok(event) => {
                        if event.data == "[DONE]" {
                            let _ = tx
                                .send(Ok(crate::llm::types::StreamEvent::End("stop".to_string())))
                                .await;
                            break;
                        }

                        if let Ok(data) = serde_json::from_str::<Value>(&event.data) {
                            if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
                                if let Some(choice) = choices.first() {
                                    let delta = &choice["delta"];

                                    if let Some(content) =
                                        delta.get("content").and_then(|c| c.as_str())
                                    {
                                        let _ = tx
                                            .send(Ok(crate::llm::types::StreamEvent::Content(
                                                content.to_string(),
                                            )))
                                            .await;
                                    }

                                    if let Some(tool_calls) =
                                        delta.get("tool_calls").and_then(|t| t.as_array())
                                    {
                                        for tc in tool_calls {
                                            if let Some(idx) =
                                                tc.get("index").and_then(|i| i.as_u64())
                                            {
                                                let func = &tc["function"];
                                                let name = func
                                                    .get("name")
                                                    .and_then(|n| n.as_str())
                                                    .map(|s| s.to_string());
                                                let arguments = func
                                                    .get("arguments")
                                                    .and_then(|a| a.as_str())
                                                    .map(|s| s.to_string());

                                                let _ = tx.send(Ok(crate::llm::types::StreamEvent::ToolCallChunk {
                                                    index: idx as usize,
                                                    name,
                                                    arguments,
                                                })).await;
                                            }
                                        }
                                    }

                                    if let Some(finish) =
                                        choice.get("finish_reason").and_then(|f| f.as_str())
                                    {
                                        if !finish.is_empty() && finish != "null" {
                                            let _ = tx
                                                .send(Ok(crate::llm::types::StreamEvent::End(
                                                    finish.to_string(),
                                                )))
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string())).await;
                        break;
                    }
                }
            }
        });

        Ok(rx)
    }
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
            let arguments: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
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
