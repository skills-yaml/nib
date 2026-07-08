//! Google Gemini Generative Language API.

use crate::llm::types::{LlmResponse, ToolCallRequest};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};

use super::LlmClient;

pub struct GeminiClient {
    client: Client,
    model: String,
    api_key: String,
}

impl GeminiClient {
    pub fn new(model: String, api_key: String) -> Self {
        Self {
            client: Client::new(),
            model,
            api_key,
        }
    }
}

#[async_trait]
impl LlmClient for GeminiClient {
    async fn complete(
        &self,
        messages: &[Value],
        _tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<LlmResponse, String> {
        let prompt = messages
            .iter()
            .map(|m| {
                format!(
                    "{}: {}",
                    m.get("role").and_then(|r| r.as_str()).unwrap_or("user"),
                    m.get("content").and_then(|c| c.as_str()).unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        let body = json!({
            "contents": [{"parts": [{"text": prompt}]}],
            "generationConfig": {"temperature": temperature},
        });

        let resp = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!(
                "Gemini API error: {}",
                resp.text().await.unwrap_or_default()
            ));
        }

        let data: Value = resp.json().await.map_err(|e| e.to_string())?;
        let text = data["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(LlmResponse::text(text))
    }
}

// Tool calling for Gemini can be added via functionDeclarations; mock path covers CI.

impl GeminiClient {
    #[allow(dead_code)]
    fn parse_function_calls(_data: &Value) -> Vec<ToolCallRequest> {
        vec![]
    }
}
