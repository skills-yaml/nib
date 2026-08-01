//! Static provider metadata shared by configuration, construction, auth, and diagnostics.

use crate::config::LlmApiMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderImplementation {
    Mock,
    OpenAi,
    Xai,
    OpenRouter,
    Meta,
    Anthropic,
    Gemini,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTransport {
    Local,
    ChatCompletions,
    Responses,
    AnthropicMessages,
    GeminiGenerateContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub implementation: ProviderImplementation,
    pub default_model: &'static str,
    pub models: &'static [&'static str],
    pub credential_environment_variable: Option<&'static str>,
    pub default_base_url: Option<&'static str>,
    pub transports: &'static [ProviderTransport],
    pub auth_api_default: Option<LlmApiMode>,
}

impl ProviderDescriptor {
    pub fn is_openai_compatible(self) -> bool {
        matches!(
            self.implementation,
            ProviderImplementation::OpenAi
                | ProviderImplementation::Xai
                | ProviderImplementation::OpenRouter
                | ProviderImplementation::Meta
        )
    }
}

const OPENAI_MODELS: &[&str] = &["gpt-4o", "gpt-4o-mini", "gpt-4-turbo", "o1-preview"];
const ANTHROPIC_MODELS: &[&str] = &[
    "claude-3-5-sonnet-20241022",
    "claude-3-opus-20240229",
    "claude-3-haiku-20240307",
];
const GEMINI_MODELS: &[&str] = &["gemini-1.5-pro", "gemini-1.5-flash", "gemini-2.0-flash-exp"];
const XAI_MODELS: &[&str] = &["grok-2-1212", "grok-beta", "grok-3"];
const OPENROUTER_MODELS: &[&str] = &[
    "openrouter/anthropic/claude-3.5-sonnet",
    "openrouter/meta-llama/llama-3.1-70b-instruct",
    "openrouter/google/gemini-1.5-pro",
    "openrouter/mistralai/mistral-large",
];
const META_MODELS: &[&str] = &["muse-spark-1.1", "muse-spark-1.1-mini"];
const MOCK_MODELS: &[&str] = &["mock-model"];

const OPENAI_TRANSPORTS: &[ProviderTransport] = &[
    ProviderTransport::ChatCompletions,
    ProviderTransport::Responses,
];
const ANTHROPIC_TRANSPORTS: &[ProviderTransport] = &[ProviderTransport::AnthropicMessages];
const GEMINI_TRANSPORTS: &[ProviderTransport] = &[ProviderTransport::GeminiGenerateContent];
const MOCK_TRANSPORTS: &[ProviderTransport] = &[ProviderTransport::Local];

pub const PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "openai",
        display_name: "OpenAI",
        implementation: ProviderImplementation::OpenAi,
        default_model: "gpt-4o",
        models: OPENAI_MODELS,
        credential_environment_variable: Some("OPENAI_API_KEY"),
        default_base_url: Some("https://api.openai.com/v1"),
        transports: OPENAI_TRANSPORTS,
        auth_api_default: Some(LlmApiMode::Responses),
    },
    ProviderDescriptor {
        id: "anthropic",
        display_name: "Anthropic Claude",
        implementation: ProviderImplementation::Anthropic,
        default_model: "claude-3-5-sonnet-20241022",
        models: ANTHROPIC_MODELS,
        credential_environment_variable: Some("ANTHROPIC_API_KEY"),
        default_base_url: Some("https://api.anthropic.com"),
        transports: ANTHROPIC_TRANSPORTS,
        auth_api_default: None,
    },
    ProviderDescriptor {
        id: "google",
        display_name: "Google Gemini",
        implementation: ProviderImplementation::Gemini,
        default_model: "gemini-1.5-pro",
        models: GEMINI_MODELS,
        credential_environment_variable: Some("GOOGLE_API_KEY"),
        default_base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
        transports: GEMINI_TRANSPORTS,
        auth_api_default: None,
    },
    ProviderDescriptor {
        id: "grok",
        display_name: "xAI Grok",
        implementation: ProviderImplementation::Xai,
        default_model: "grok-2-1212",
        models: XAI_MODELS,
        credential_environment_variable: Some("XAI_API_KEY"),
        default_base_url: Some("https://api.x.ai/v1"),
        transports: OPENAI_TRANSPORTS,
        auth_api_default: Some(LlmApiMode::ChatCompletions),
    },
    ProviderDescriptor {
        id: "openrouter",
        display_name: "OpenRouter",
        implementation: ProviderImplementation::OpenRouter,
        default_model: "openrouter/anthropic/claude-3.5-sonnet",
        models: OPENROUTER_MODELS,
        credential_environment_variable: Some("OPENROUTER_API_KEY"),
        default_base_url: Some("https://openrouter.ai/api/v1"),
        transports: OPENAI_TRANSPORTS,
        auth_api_default: Some(LlmApiMode::ChatCompletions),
    },
    ProviderDescriptor {
        id: "meta",
        display_name: "Meta (OpenAI-compatible endpoint required)",
        implementation: ProviderImplementation::Meta,
        default_model: "muse-spark-1.1",
        models: META_MODELS,
        credential_environment_variable: Some("META_API_KEY"),
        // Meta has no verified public default inference endpoint for this adapter.
        default_base_url: None,
        transports: OPENAI_TRANSPORTS,
        auth_api_default: Some(LlmApiMode::ChatCompletions),
    },
    ProviderDescriptor {
        id: "mock",
        display_name: "Mock",
        implementation: ProviderImplementation::Mock,
        default_model: "mock-model",
        models: MOCK_MODELS,
        credential_environment_variable: None,
        default_base_url: None,
        transports: MOCK_TRANSPORTS,
        auth_api_default: None,
    },
];

pub fn provider_descriptor(id: &str) -> Option<&'static ProviderDescriptor> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn provider_ids_are_unique_and_metadata_is_complete() {
        let ids = PROVIDERS
            .iter()
            .map(|provider| provider.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), PROVIDERS.len());
        for provider in PROVIDERS {
            assert!(!provider.display_name.trim().is_empty());
            assert!(!provider.default_model.trim().is_empty());
            assert!(!provider.models.is_empty());
            assert!(!provider.transports.is_empty());
        }
    }

    #[test]
    fn meta_requires_an_explicit_verified_endpoint() {
        assert_eq!(provider_descriptor("meta").unwrap().default_base_url, None);
    }
}
