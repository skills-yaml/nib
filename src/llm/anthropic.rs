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
