//! Provider metadata and the bundled, source-attributed model catalog.

use crate::config::LlmApiMode;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

const MODEL_CATALOG_SCHEMA_VERSION: u32 = 1;
const MAX_CATALOG_MODELS_PER_PROVIDER: usize = 128;
const MAX_CATALOG_MODEL_BYTES: usize = 512;
const MAX_CATALOG_SOURCE_URL_BYTES: usize = 4 * 1024;
const DEFAULT_MODELS_TOML: &str = include_str!("default_models.toml");

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

    pub fn default_model(self) -> &'static str {
        &provider_model_defaults(self.id).default_model
    }

    pub fn models(self) -> &'static [String] {
        &provider_model_defaults(self.id).models
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCatalog {
    schema_version: u32,
    providers: BTreeMap<String, ProviderModelDefaults>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderModelDefaults {
    default_model: String,
    models: Vec<String>,
    source_url: String,
    verified_on: String,
}

static MODEL_CATALOG: OnceLock<ModelCatalog> = OnceLock::new();

fn model_catalog() -> &'static ModelCatalog {
    MODEL_CATALOG.get_or_init(|| {
        parse_model_catalog(DEFAULT_MODELS_TOML)
            .expect("src/llm/default_models.toml must be a valid bundled model catalog")
    })
}

fn provider_model_defaults(provider_id: &str) -> &'static ProviderModelDefaults {
    model_catalog()
        .providers
        .get(provider_id)
        .expect("every registered provider must have bundled model defaults")
}

fn parse_model_catalog(contents: &str) -> Result<ModelCatalog, String> {
    let catalog: ModelCatalog =
        toml::from_str(contents).map_err(|error| format!("invalid model catalog TOML: {error}"))?;
    validate_model_catalog(&catalog)?;
    Ok(catalog)
}

fn validate_model_catalog(catalog: &ModelCatalog) -> Result<(), String> {
    if catalog.schema_version != MODEL_CATALOG_SCHEMA_VERSION {
        return Err(format!(
            "unsupported model catalog schema version {}; expected {MODEL_CATALOG_SCHEMA_VERSION}",
            catalog.schema_version
        ));
    }

    let registered = PROVIDERS
        .iter()
        .map(|provider| provider.id)
        .collect::<BTreeSet<_>>();
    let configured = catalog
        .providers
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if configured != registered {
        return Err(format!(
            "catalog providers must exactly match registered providers; registered={registered:?}, configured={configured:?}"
        ));
    }

    for (provider_id, defaults) in &catalog.providers {
        if defaults.models.is_empty() || defaults.models.len() > MAX_CATALOG_MODELS_PER_PROVIDER {
            return Err(format!(
                "provider {provider_id} must define between 1 and {MAX_CATALOG_MODELS_PER_PROVIDER} models"
            ));
        }

        let mut unique_models = BTreeSet::new();
        for model in &defaults.models {
            if model.trim().is_empty()
                || model.len() > MAX_CATALOG_MODEL_BYTES
                || model.contains('\0')
            {
                return Err(format!(
                    "provider {provider_id} model identifiers must be non-empty, at most {MAX_CATALOG_MODEL_BYTES} bytes, and contain no NUL"
                ));
            }
            if !unique_models.insert(model) {
                return Err(format!(
                    "provider {provider_id} contains duplicate model identifier {model:?}"
                ));
            }
        }
        if !unique_models.contains(&defaults.default_model) {
            return Err(format!(
                "provider {provider_id} default_model must appear in its models list"
            ));
        }
        if defaults.source_url.trim().is_empty()
            || defaults.source_url.len() > MAX_CATALOG_SOURCE_URL_BYTES
            || defaults.source_url.chars().any(char::is_control)
        {
            return Err(format!(
                "provider {provider_id} source_url must be non-empty, at most {MAX_CATALOG_SOURCE_URL_BYTES} bytes, and contain no control characters"
            ));
        }
        if !is_iso_date(&defaults.verified_on) {
            return Err(format!(
                "provider {provider_id} verified_on must use YYYY-MM-DD"
            ));
        }
    }

    Ok(())
}

fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

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
        credential_environment_variable: Some("OPENAI_API_KEY"),
        default_base_url: Some("https://api.openai.com/v1"),
        transports: OPENAI_TRANSPORTS,
        auth_api_default: Some(LlmApiMode::Responses),
    },
    ProviderDescriptor {
        id: "anthropic",
        display_name: "Anthropic Claude",
        implementation: ProviderImplementation::Anthropic,
        credential_environment_variable: Some("ANTHROPIC_API_KEY"),
        default_base_url: Some("https://api.anthropic.com"),
        transports: ANTHROPIC_TRANSPORTS,
        auth_api_default: None,
    },
    ProviderDescriptor {
        id: "google",
        display_name: "Google Gemini",
        implementation: ProviderImplementation::Gemini,
        credential_environment_variable: Some("GOOGLE_API_KEY"),
        default_base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
        transports: GEMINI_TRANSPORTS,
        auth_api_default: None,
    },
    ProviderDescriptor {
        id: "grok",
        display_name: "xAI Grok",
        implementation: ProviderImplementation::Xai,
        credential_environment_variable: Some("XAI_API_KEY"),
        default_base_url: Some("https://api.x.ai/v1"),
        transports: OPENAI_TRANSPORTS,
        auth_api_default: Some(LlmApiMode::ChatCompletions),
    },
    ProviderDescriptor {
        id: "openrouter",
        display_name: "OpenRouter",
        implementation: ProviderImplementation::OpenRouter,
        credential_environment_variable: Some("OPENROUTER_API_KEY"),
        default_base_url: Some("https://openrouter.ai/api/v1"),
        transports: OPENAI_TRANSPORTS,
        auth_api_default: Some(LlmApiMode::ChatCompletions),
    },
    ProviderDescriptor {
        id: "meta",
        display_name: "Meta (OpenAI-compatible endpoint required)",
        implementation: ProviderImplementation::Meta,
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
            assert!(!provider.default_model().trim().is_empty());
            assert!(!provider.models().is_empty());
            assert!(!provider.transports.is_empty());
        }
    }

    #[test]
    fn bundled_model_catalog_is_valid_and_source_attributed() {
        let catalog = parse_model_catalog(DEFAULT_MODELS_TOML).expect("bundled catalog");
        for defaults in catalog.providers.values() {
            assert!(defaults.models.contains(&defaults.default_model));
            assert!(!defaults.source_url.is_empty());
            assert!(is_iso_date(&defaults.verified_on));
        }
    }

    #[test]
    fn model_catalog_rejects_unknown_schema_and_duplicate_models() {
        let unknown_schema =
            DEFAULT_MODELS_TOML.replacen("schema_version = 1", "schema_version = 2", 1);
        assert!(parse_model_catalog(&unknown_schema)
            .expect_err("unknown schema")
            .contains("unsupported model catalog schema"));

        let duplicate = DEFAULT_MODELS_TOML.replacen(
            "models = [\"gpt-5.6-sol\", \"gpt-5.6-terra\", \"gpt-5.6-luna\"]",
            "models = [\"gpt-5.6-sol\", \"gpt-5.6-sol\"]",
            1,
        );
        assert!(parse_model_catalog(&duplicate)
            .expect_err("duplicate model")
            .contains("duplicate model identifier"));
    }

    #[test]
    fn meta_requires_an_explicit_verified_endpoint() {
        assert_eq!(provider_descriptor("meta").unwrap().default_base_url, None);
    }
}
