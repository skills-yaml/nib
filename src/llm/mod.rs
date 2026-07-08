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
pub use types::{LlmResponse, ToolCallRequest};

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
        temperature: f64,
    ) -> Result<LlmResponse, String>;
}
