//! Anthropic Messages API client.

use crate::llm::types::{LlmResponse, ToolCallRequest};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use super::LlmClient;

pub struct AnthropicClient {
    client: Client,
    model: String,
    api_key: String,
}

impl AnthropicClient {
    pub fn new(model: String, api_key: String) -> Self {
        Self {
            client: Client::new(),
            model,
            api_key,
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

        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!(
                "Anthropic API error: {}",
                resp.text().await.unwrap_or_default()
            ));
        }

        let data: Value = resp.json().await.map_err(|e| e.to_string())?;
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

        let resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!(
                "Anthropic API error: {}",
                resp.text().await.unwrap_or_default()
            ));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            use eventsource_stream::Eventsource;
            use futures_util::StreamExt;

            let mut stream = resp.bytes_stream().eventsource();
            while let Some(event_res) = stream.next().await {
                match event_res {
                    Ok(event) => {
                        let ev_type = event.event;
                        if ev_type == "message_stop" {
                            break;
                        }

                        if let Ok(data) = serde_json::from_str::<Value>(&event.data) {
                            match ev_type.as_str() {
                                "content_block_start" => {
                                    if let Some(cb) = data.get("content_block") {
                                        if cb.get("type").and_then(|t| t.as_str())
                                            == Some("tool_use")
                                        {
                                            let index = data
                                                .get("index")
                                                .and_then(|i| i.as_u64())
                                                .unwrap_or(0)
                                                as usize;
                                            let name = cb
                                                .get("name")
                                                .and_then(|n| n.as_str())
                                                .map(|s| s.to_string());
                                            let _ = tx
                                                .send(Ok(
                                                    crate::llm::types::StreamEvent::ToolCallChunk {
                                                        index,
                                                        name,
                                                        arguments: Some(String::new()),
                                                    },
                                                ))
                                                .await;
                                        }
                                    }
                                }
                                "content_block_delta" => {
                                    if let Some(delta) = data.get("delta") {
                                        let index =
                                            data.get("index").and_then(|i| i.as_u64()).unwrap_or(0)
                                                as usize;
                                        if let Some(text) =
                                            delta.get("text").and_then(|t| t.as_str())
                                        {
                                            let _ = tx
                                                .send(Ok(crate::llm::types::StreamEvent::Content(
                                                    text.to_string(),
                                                )))
                                                .await;
                                        }
                                        if let Some(json_delta) =
                                            delta.get("partial_json").and_then(|j| j.as_str())
                                        {
                                            let _ = tx
                                                .send(Ok(
                                                    crate::llm::types::StreamEvent::ToolCallChunk {
                                                        index,
                                                        name: None,
                                                        arguments: Some(json_delta.to_string()),
                                                    },
                                                ))
                                                .await;
                                        }
                                    }
                                }
                                "message_delta" => {
                                    if let Some(delta) = data.get("delta") {
                                        if let Some(stop_reason) =
                                            delta.get("stop_reason").and_then(|s| s.as_str())
                                        {
                                            let _ = tx
                                                .send(Ok(crate::llm::types::StreamEvent::End(
                                                    stop_reason.to_string(),
                                                )))
                                                .await;
                                        }
                                    }
                                }
                                _ => {}
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

fn parse_anthropic_response(data: &Value) -> Result<LlmResponse, String> {
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
                tool_calls.push(ToolCallRequest {
                    name: block["name"].as_str().unwrap_or("").to_string(),
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
