//! LLM client factory from project config.

use crate::config::{LlmConfig, ProviderEntry};
use std::sync::Arc;

use super::anthropic::AnthropicClient;
use super::gemini::GeminiClient;
use super::mock::MockLlmClient;
use super::openai::OpenAiCompatClient;
use super::LlmClient;

pub fn create_client(
    llm: &LlmConfig,
    provider_override: Option<&str>,
) -> Result<Arc<dyn LlmClient>, String> {
    let provider = provider_override
        .map(str::to_string)
        .unwrap_or_else(|| llm.get_active_provider());
    if provider == "mock" {
        return Ok(Arc::new(MockLlmClient::new()));
    }
    if !matches!(
        provider.as_str(),
        "openai" | "anthropic" | "google" | "grok" | "openrouter" | "meta"
    ) {
        return Err(format!("unsupported LLM provider: {provider}"));
    }

    let entry = llm.get_provider(Some(&provider));
    let model = entry
        .map(|entry| entry.model.clone())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| default_model(&provider));
    let credentials = require_credentials(provider_credentials(entry, &provider), &provider)?;

    let client: Arc<dyn LlmClient> = match provider.as_str() {
        "openai" => Arc::new(OpenAiCompatClient::new(
            model,
            credentials,
            configured_base_url(entry, "https://api.openai.com/v1/chat/completions"),
        )),
        "anthropic" => Arc::new(AnthropicClient::new(model, credentials)),
        "google" => Arc::new(GeminiClient::new(model, credentials)),
        "grok" => Arc::new(OpenAiCompatClient::new(
            model,
            credentials,
            configured_base_url(entry, "https://api.x.ai/v1/chat/completions"),
        )),
        "openrouter" => Arc::new(OpenAiCompatClient::new(
            model,
            credentials,
            configured_base_url(entry, "https://openrouter.ai/api/v1/chat/completions"),
        )),
        "meta" => Arc::new(OpenAiCompatClient::new(
            model,
            credentials,
            configured_base_url(entry, "https://api.meta.com/v1/chat/completions"),
        )),
        _ => unreachable!("provider was validated"),
    };
    Ok(client)
}

fn configured_base_url(entry: Option<&ProviderEntry>, default: &str) -> String {
    entry
        .and_then(|entry| entry.base_url.clone())
        .unwrap_or_else(|| default.to_string())
}

fn provider_credentials(entry: Option<&ProviderEntry>, provider: &str) -> Vec<String> {
    let mut credentials = Vec::new();
    if let Some(entry) = entry {
        if let Some(key) = entry
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            credentials.push(key.to_string());
        }
        for key in &entry.api_keys {
            let key = key.trim();
            if !key.is_empty() && !credentials.iter().any(|existing| existing == key) {
                credentials.push(key.to_string());
            }
        }
    }
    if let Some(key) = env_key(provider).map(|key| key.trim().to_string()) {
        if !key.is_empty() && !credentials.iter().any(|existing| existing == &key) {
            credentials.push(key);
        }
    }
    credentials
}

fn require_credentials(credentials: Vec<String>, provider: &str) -> Result<Vec<String>, String> {
    if credentials.is_empty() {
        Err(format!(
            "LLM provider '{provider}' has no credentials; configure api_key/api_keys or its provider environment variable"
        ))
    } else {
        Ok(credentials)
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
    let variable = match provider {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "google" => "GOOGLE_API_KEY",
        "grok" => "XAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "meta" => "META_API_KEY",
        _ => return None,
    };
    std::env::var(variable).ok()
}

pub fn provider_ready(entry: Option<&ProviderEntry>, provider: &str) -> bool {
    provider == "mock" || !provider_credentials(entry, provider).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn configured_key_pool_keeps_legacy_primary_and_deduplicates() {
        let entry = ProviderEntry {
            model: "model".to_string(),
            api_key: Some("primary".to_string()),
            api_keys: vec!["backup".to_string(), "primary".to_string()],
            base_url: None,
        };
        assert_eq!(
            provider_credentials(Some(&entry), "provider-without-env"),
            ["primary", "backup"]
        );
    }

    #[test]
    fn configured_real_provider_without_credentials_fails_closed() {
        let error = require_credentials(Vec::new(), "anthropic").expect_err("missing key error");
        assert!(error.contains("no credentials"));
    }

    #[test]
    fn factory_constructs_every_supported_provider_without_network_access() {
        for provider in [
            "openai",
            "anthropic",
            "google",
            "grok",
            "openrouter",
            "meta",
        ] {
            let config = LlmConfig {
                active_provider: Some(provider.to_string()),
                providers: HashMap::from([(
                    provider.to_string(),
                    ProviderEntry {
                        model: String::new(),
                        api_key: Some("fixture-key".to_string()),
                        api_keys: Vec::new(),
                        base_url: Some("http://127.0.0.1:1".to_string()),
                    },
                )]),
                context_length: 128_000,
            };
            create_client(&config, None)
                .unwrap_or_else(|error| panic!("{provider} client construction failed: {error}"));
            assert_ne!(default_model(provider), "mock-model");
        }
    }

    #[test]
    fn factory_honors_override_and_rejects_unknown_provider() {
        let config = LlmConfig::default();
        create_client(&config, Some("mock")).expect("mock override");
        let error = create_client(&config, Some("unknown"))
            .err()
            .expect("unknown provider");
        assert!(error.contains("unsupported LLM provider"));
    }

    #[test]
    fn readiness_and_base_url_defaults_are_explicit() {
        let empty = ProviderEntry::default();
        let configured = ProviderEntry {
            api_keys: vec![" backup ".to_string()],
            base_url: Some("http://localhost:1234".to_string()),
            ..ProviderEntry::default()
        };
        assert!(provider_ready(None, "mock"));
        assert!(!provider_ready(Some(&empty), "provider-without-env"));
        assert!(provider_ready(Some(&configured), "provider-without-env"));
        assert_eq!(
            configured_base_url(Some(&configured), "default"),
            "http://localhost:1234"
        );
        assert_eq!(configured_base_url(None, "default"), "default");
        assert_eq!(env_key("provider-without-env"), None);
    }
}
