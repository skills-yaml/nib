//! Mock LLM for tests and offline development.

use crate::llm::types::{LlmResponse, ToolCallRequest};
use async_trait::async_trait;
use serde_json::json;

use super::LlmClient;

pub struct MockLlmClient {
    step: std::sync::atomic::AtomicUsize,
}

impl Default for MockLlmClient {
    fn default() -> Self {
        Self {
            step: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl MockLlmClient {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn complete(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&[serde_json::Value]>,
        _temperature: f64,
    ) -> Result<LlmResponse, String> {
        let last = messages
            .last()
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_lowercase();

        let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        if tools.is_some() && step == 0 {
            if last.contains("explore") || last.contains("list") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest {
                    name: "list_directory".to_string(),
                    arguments: json!({"path": "."}),
                }]));
            }
            if last.contains("read_file") || last.contains(" open file") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest {
                    name: "read_file".to_string(),
                    arguments: json!({"path": "README.md"}),
                }]));
            }
            return Ok(LlmResponse::with_tools(vec![ToolCallRequest {
                name: "list_directory".to_string(),
                arguments: json!({"path": "."}),
            }]));
        }

        Ok(LlmResponse::text(
            "Final answer: task complete. (mock LLM response)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_returns_tool_then_answer() {
        let client = MockLlmClient::new();
        let tools = vec![json!({"type": "function"})];
        let msgs = vec![json!({"role": "user", "content": "explore project"})];
        let r1 = client.complete(&msgs, Some(&tools), 0.7).await.unwrap();
        assert!(r1.tool_calls.is_some());
        let r2 = client
            .complete(
                &[json!({"role": "user", "content": "summarize results"})],
                None,
                0.7,
            )
            .await
            .unwrap();
        assert!(r2.content.is_some());
    }
}
