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

    async fn stream(
        &self,
        messages: &[Value],
        _tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<crate::llm::types::StreamEvent, String>>, String>
    {
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
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
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

        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            use eventsource_stream::Eventsource;
            use futures_util::StreamExt;

            let mut stream = resp.bytes_stream().eventsource();
            while let Some(event_res) = stream.next().await {
                match event_res {
                    Ok(event) => {
                        if let Ok(data) = serde_json::from_str::<Value>(&event.data) {
                            if let Some(candidates) =
                                data.get("candidates").and_then(|c| c.as_array())
                            {
                                if let Some(candidate) = candidates.first() {
                                    if let Some(parts) = candidate
                                        .get("content")
                                        .and_then(|c| c.get("parts"))
                                        .and_then(|p| p.as_array())
                                    {
                                        if let Some(part) = parts.first() {
                                            if let Some(text) =
                                                part.get("text").and_then(|t| t.as_str())
                                            {
                                                let _ = tx
                                                    .send(Ok(
                                                        crate::llm::types::StreamEvent::Content(
                                                            text.to_string(),
                                                        ),
                                                    ))
                                                    .await;
                                            }
                                        }
                                    }
                                    if let Some(finish) =
                                        candidate.get("finishReason").and_then(|f| f.as_str())
                                    {
                                        if !finish.is_empty()
                                            && finish != "null"
                                            && finish != "STOP"
                                        {
                                            let _ = tx
                                                .send(Ok(crate::llm::types::StreamEvent::End(
                                                    finish.to_string(),
                                                )))
                                                .await;
                                            break;
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
            let _ = tx
                .send(Ok(crate::llm::types::StreamEvent::End("stop".to_string())))
                .await;
        });

        Ok(rx)
    }
}

// Tool calling for Gemini can be added via functionDeclarations; mock path covers CI.

impl GeminiClient {
    #[allow(dead_code)]
    fn parse_function_calls(_data: &Value) -> Vec<ToolCallRequest> {
        vec![]
    }
}
