//! LLM client factory from project config.

use crate::config::{LlmConfig, ProviderEntry};
use std::sync::Arc;

use super::anthropic::AnthropicClient;
use super::gemini::GeminiClient;
use super::mock::MockLlmClient;
use super::openai::OpenAiCompatClient;
use super::LlmClient;

pub fn create_client(llm: &LlmConfig, provider_override: Option<&str>) -> Arc<dyn LlmClient> {
    let provider = provider_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| llm.get_active_provider());

    if provider == "mock" {
        return Arc::new(MockLlmClient::new());
    }

    let entry = llm.get_provider(Some(&provider));
    let model = entry
        .map(|e| e.model.clone())
        .unwrap_or_else(|| default_model(&provider));
    let api_key = entry
        .and_then(|e| e.api_key.clone())
        .or_else(|| env_key(&provider));

    match provider.as_str() {
        "openai" => Arc::new(OpenAiCompatClient::openai(model, api_key)),
        "anthropic" => {
            let key = api_key.unwrap_or_default();
            if key.is_empty() {
                return Arc::new(MockLlmClient::new());
            }
            Arc::new(AnthropicClient::new(model, key))
        }
        "google" => {
            let key = api_key.unwrap_or_default();
            if key.is_empty() {
                return Arc::new(MockLlmClient::new());
            }
            Arc::new(GeminiClient::new(model, key))
        }
        "grok" => Arc::new(OpenAiCompatClient::xai(model, api_key)),
        "openrouter" => Arc::new(OpenAiCompatClient::openrouter(model, api_key)),
        "meta" => Arc::new(OpenAiCompatClient::meta(model, api_key)),
        _ => Arc::new(MockLlmClient::new()),
    }
}

fn default_model(provider: &str) -> String {
    match provider {
        "openai" => "gpt-4o".to_string(),
        "anthropic" => "claude-3-5-sonnet-20241022".to_string(),
        "google" => "gemini-1.5-pro".to_string(),
        "grok" => "grok-2-1212".to_string(),
        "openrouter" => "openrouter/anthropic/claude-3.5-sonnet".to_string(),
        "meta" => "muse-spark-1.1".to_string(),
        _ => "mock-model".to_string(),
    }
}

fn env_key(provider: &str) -> Option<String> {
    let var = match provider {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "google" => "GOOGLE_API_KEY",
        "grok" => "XAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "meta" => "META_API_KEY",
        _ => return None,
    };
    std::env::var(var).ok()
}

pub fn provider_ready(entry: Option<&ProviderEntry>, provider: &str) -> bool {
    if provider == "mock" {
        return true;
    }
    entry
        .and_then(|e| e.api_key.as_ref())
        .map(|k| !k.is_empty())
        .unwrap_or_else(|| env_key(provider).is_some())
}
