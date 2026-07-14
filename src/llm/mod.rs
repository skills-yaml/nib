//! Pluggable LLM providers (HTTP) and mock for CI.

use async_trait::async_trait;
use serde_json::Value;

pub mod anthropic;
pub mod factory;
pub mod gemini;
pub mod mock;
pub mod openai;
pub mod types;

pub use factory::{create_client, provider_ready};
pub use mock::MockLlmClient;
use tokio::sync::mpsc::Receiver;
pub use types::{LlmResponse, StreamEvent, ToolCallRequest};

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<LlmResponse, String>;

    async fn stream(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<Receiver<Result<StreamEvent, String>>, String> {
        // default impl just calls complete and yields one big chunk
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let resp = self.complete(messages, tools, temperature).await;
        tokio::spawn(async move {
            match resp {
                Ok(r) => {
                    if let Some(content) = r.content {
                        let _ = tx.send(Ok(StreamEvent::Content(content))).await;
                    }
                    if let Some(tool_calls) = r.tool_calls {
                        for (i, tc) in tool_calls.into_iter().enumerate() {
                            let _ = tx
                                .send(Ok(StreamEvent::ToolCallChunk {
                                    index: i,
                                    name: Some(tc.name),
                                    arguments: Some(tc.arguments.to_string()),
                                }))
                                .await;
                        }
                    }
                    let _ = tx.send(Ok(StreamEvent::End(r.finish_reason))).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                }
            }
        });
        Ok(rx)
    }
}
