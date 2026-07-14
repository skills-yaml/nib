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
