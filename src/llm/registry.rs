//! Provider metadata and the bundled, source-attributed model catalog.

use crate::config::{LlmApiMode, ProviderEntry};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
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

impl ProviderImplementation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::OpenAi => "openai",
            Self::Xai => "xai",
            Self::OpenRouter => "openrouter",
            Self::Meta => "meta",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
        }
    }
}

impl fmt::Display for ProviderImplementation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderTransport {
    Local,
    ChatCompletions,
    Responses,
    AnthropicMessages,
    GeminiGenerateContent,
}

impl ProviderTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GeminiGenerateContent => "gemini_generate_content",
        }
    }

    pub fn from_api_mode(api_mode: LlmApiMode) -> Self {
        match api_mode {
            LlmApiMode::ChatCompletions => Self::ChatCompletions,
            LlmApiMode::Responses => Self::Responses,
        }
    }
}

impl fmt::Display for ProviderTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderReasoningSupport {
    ProviderDefaultOnly,
    ConfigurableEffort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderEndpointShape {
    Local,
    ApiRootOrTransportEndpoint,
    ExplicitApiRootOrTransportEndpoint,
    AnthropicApiRootOrMessagesEndpoint,
    GeminiApiRoot,
}

impl ProviderEndpointShape {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::ApiRootOrTransportEndpoint => "api_root_or_transport_endpoint",
            Self::ExplicitApiRootOrTransportEndpoint => "explicit_api_root_or_transport_endpoint",
            Self::AnthropicApiRootOrMessagesEndpoint => "anthropic_api_root_or_messages_endpoint",
            Self::GeminiApiRoot => "gemini_api_root",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTerminalForm {
    LocalDeterministic,
    ChatFinishReason,
    ResponsesStatus,
    AnthropicStopReason,
    GeminiFinishReason,
}

impl ProviderTerminalForm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalDeterministic => "local_deterministic",
            Self::ChatFinishReason => "chat_finish_reason",
            Self::ResponsesStatus => "responses_status",
            Self::AnthropicStopReason => "anthropic_stop_reason",
            Self::GeminiFinishReason => "gemini_finish_reason",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderRefusalForm {
    None,
    ChatMessage,
    ResponsesOutputItem,
    AnthropicStopReason,
    GeminiSafetyBlock,
}

impl ProviderRefusalForm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ChatMessage => "chat_message",
            Self::ResponsesOutputItem => "responses_output_item",
            Self::AnthropicStopReason => "anthropic_stop_reason",
            Self::GeminiSafetyBlock => "gemini_safety_block",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderInBandErrorForm {
    None,
    ChatErrorEnvelope,
    ResponsesErrorEvent,
    AnthropicSseError,
    GeminiErrorEnvelope,
}

impl ProviderInBandErrorForm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ChatErrorEnvelope => "chat_error_envelope",
            Self::ResponsesErrorEvent => "responses_error_event",
            Self::AnthropicSseError => "anthropic_sse_error",
            Self::GeminiErrorEnvelope => "gemini_error_envelope",
        }
    }
}

impl ProviderReasoningSupport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderDefaultOnly => "provider_default_only",
            Self::ConfigurableEffort => "configurable_effort",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderRetryCapabilities {
    pub retryable_http_statuses: &'static [u16],
    pub retry_after_http_statuses: &'static [u16],
    pub credential_rotation_http_statuses: &'static [u16],
}

impl ProviderRetryCapabilities {
    pub fn retries_status(self, status: reqwest::StatusCode) -> bool {
        self.retryable_http_statuses.contains(&status.as_u16())
    }

    pub fn accepts_retry_after(self, status: reqwest::StatusCode) -> bool {
        self.retry_after_http_statuses.contains(&status.as_u16())
    }

    pub fn rotates_credential(self, status: reqwest::StatusCode) -> bool {
        self.credential_rotation_http_statuses
            .contains(&status.as_u16())
    }

    fn status_summary(statuses: &[u16]) -> String {
        if statuses.is_empty() {
            return "none".to_string();
        }
        statuses
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join("/")
    }

    pub fn diagnostic_summary(self) -> String {
        format!(
            "retry_statuses={}, retry_after_statuses={}, credential_rotation_statuses={}",
            Self::status_summary(self.retryable_http_statuses),
            Self::status_summary(self.retry_after_http_statuses),
            Self::status_summary(self.credential_rotation_http_statuses),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderTransportCapabilities {
    pub transport: ProviderTransport,
    pub complete: bool,
    pub stream: bool,
    pub tools: bool,
    pub tool_continuation: bool,
    pub parallel_tools: bool,
    pub reasoning: ProviderReasoningSupport,
    pub endpoint_shape: ProviderEndpointShape,
    pub terminal_form: ProviderTerminalForm,
    pub refusal_form: ProviderRefusalForm,
    pub in_band_error_form: ProviderInBandErrorForm,
    pub retry: ProviderRetryCapabilities,
}

impl ProviderTransportCapabilities {
    pub fn diagnostic_summary(self) -> String {
        format!(
            "complete={}, stream={}, tools={}, tool_continuation={}, parallel_tools={}, reasoning={}, endpoint_shape={}, terminal_form={}, refusal_form={}, in_band_error_form={}, {}",
            self.complete,
            self.stream,
            self.tools,
            self.tool_continuation,
            self.parallel_tools,
            self.reasoning.as_str(),
            self.endpoint_shape.as_str(),
            self.terminal_form.as_str(),
            self.refusal_form.as_str(),
            self.in_band_error_form.as_str(),
            self.retry.diagnostic_summary(),
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub display_name: &'static str,
    pub implementation: ProviderImplementation,
    pub credential_environment_variable: Option<&'static str>,
    pub default_base_url: Option<&'static str>,
    pub capabilities: &'static [ProviderTransportCapabilities],
    pub auth_api_default: Option<LlmApiMode>,
    pub(crate) config_parser: crate::llm::factory::ProviderConfigParser,
    pub(crate) constructor: crate::llm::factory::ProviderConstructor,
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

    pub fn transports(self) -> impl Iterator<Item = ProviderTransport> {
        self.capabilities
            .iter()
            .map(|capability| capability.transport)
    }

    pub fn capabilities_for(
        self,
        transport: ProviderTransport,
    ) -> Option<ProviderTransportCapabilities> {
        self.capabilities
            .iter()
            .copied()
            .find(|capability| capability.transport == transport)
    }

    pub fn supports_api_mode_selection(self) -> bool {
        self.capabilities.iter().all(|capability| {
            matches!(
                capability.transport,
                ProviderTransport::ChatCompletions | ProviderTransport::Responses
            )
        }) && self.capabilities.len() > 1
    }

    pub fn supports_configurable_reasoning(self) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability.reasoning == ProviderReasoningSupport::ConfigurableEffort)
    }

    pub fn configured_transport(self, entry: Option<&ProviderEntry>) -> ProviderTransport {
        if self.supports_api_mode_selection() {
            return ProviderTransport::from_api_mode(
                entry.map_or(LlmApiMode::default(), ProviderEntry::resolved_api_mode),
            );
        }
        self.capabilities
            .first()
            .map_or(ProviderTransport::Local, |capability| capability.transport)
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

const COMMON_RETRYABLE_HTTP_STATUSES: &[u16] = &[408, 425, 429, 500, 502, 503, 504];
const COMMON_RETRY_AFTER_HTTP_STATUSES: &[u16] = &[429, 503];
const ANTHROPIC_RETRYABLE_HTTP_STATUSES: &[u16] = &[408, 425, 429, 500, 502, 503, 504, 529];
const ANTHROPIC_RETRY_AFTER_HTTP_STATUSES: &[u16] = &[429, 503, 529];
const RATE_LIMIT_CREDENTIAL_ROTATION_STATUSES: &[u16] = &[429];

const COMMON_RETRY_CAPABILITIES: ProviderRetryCapabilities = ProviderRetryCapabilities {
    retryable_http_statuses: COMMON_RETRYABLE_HTTP_STATUSES,
    retry_after_http_statuses: COMMON_RETRY_AFTER_HTTP_STATUSES,
    credential_rotation_http_statuses: RATE_LIMIT_CREDENTIAL_ROTATION_STATUSES,
};
const ANTHROPIC_RETRY_CAPABILITIES: ProviderRetryCapabilities = ProviderRetryCapabilities {
    retryable_http_statuses: ANTHROPIC_RETRYABLE_HTTP_STATUSES,
    retry_after_http_statuses: ANTHROPIC_RETRY_AFTER_HTTP_STATUSES,
    credential_rotation_http_statuses: RATE_LIMIT_CREDENTIAL_ROTATION_STATUSES,
};
const NO_RETRY_CAPABILITIES: ProviderRetryCapabilities = ProviderRetryCapabilities {
    retryable_http_statuses: &[],
    retry_after_http_statuses: &[],
    credential_rotation_http_statuses: &[],
};

const fn compatible_capabilities(
    endpoint_shape: ProviderEndpointShape,
) -> [ProviderTransportCapabilities; 2] {
    [
        ProviderTransportCapabilities {
            transport: ProviderTransport::ChatCompletions,
            complete: true,
            stream: true,
            tools: true,
            tool_continuation: true,
            parallel_tools: true,
            reasoning: ProviderReasoningSupport::ConfigurableEffort,
            endpoint_shape,
            terminal_form: ProviderTerminalForm::ChatFinishReason,
            refusal_form: ProviderRefusalForm::ChatMessage,
            in_band_error_form: ProviderInBandErrorForm::ChatErrorEnvelope,
            retry: COMMON_RETRY_CAPABILITIES,
        },
        ProviderTransportCapabilities {
            transport: ProviderTransport::Responses,
            complete: true,
            stream: true,
            tools: true,
            tool_continuation: true,
            parallel_tools: true,
            reasoning: ProviderReasoningSupport::ConfigurableEffort,
            endpoint_shape,
            terminal_form: ProviderTerminalForm::ResponsesStatus,
            refusal_form: ProviderRefusalForm::ResponsesOutputItem,
            in_band_error_form: ProviderInBandErrorForm::ResponsesErrorEvent,
            retry: COMMON_RETRY_CAPABILITIES,
        },
    ]
}

const OPENAI_CAPABILITIES: &[ProviderTransportCapabilities] =
    &compatible_capabilities(ProviderEndpointShape::ApiRootOrTransportEndpoint);
const XAI_CAPABILITIES: &[ProviderTransportCapabilities] =
    &compatible_capabilities(ProviderEndpointShape::ApiRootOrTransportEndpoint);
const OPENROUTER_CAPABILITIES: &[ProviderTransportCapabilities] =
    &compatible_capabilities(ProviderEndpointShape::ApiRootOrTransportEndpoint);
const META_CAPABILITIES: &[ProviderTransportCapabilities] =
    &compatible_capabilities(ProviderEndpointShape::ExplicitApiRootOrTransportEndpoint);
const ANTHROPIC_CAPABILITIES: &[ProviderTransportCapabilities] = &[ProviderTransportCapabilities {
    transport: ProviderTransport::AnthropicMessages,
    complete: true,
    stream: true,
    tools: true,
    tool_continuation: true,
    parallel_tools: true,
    reasoning: ProviderReasoningSupport::ProviderDefaultOnly,
    endpoint_shape: ProviderEndpointShape::AnthropicApiRootOrMessagesEndpoint,
    terminal_form: ProviderTerminalForm::AnthropicStopReason,
    refusal_form: ProviderRefusalForm::AnthropicStopReason,
    in_band_error_form: ProviderInBandErrorForm::AnthropicSseError,
    retry: ANTHROPIC_RETRY_CAPABILITIES,
}];
const GEMINI_CAPABILITIES: &[ProviderTransportCapabilities] = &[ProviderTransportCapabilities {
    transport: ProviderTransport::GeminiGenerateContent,
    complete: true,
    stream: true,
    tools: true,
    tool_continuation: true,
    parallel_tools: true,
    reasoning: ProviderReasoningSupport::ProviderDefaultOnly,
    endpoint_shape: ProviderEndpointShape::GeminiApiRoot,
    terminal_form: ProviderTerminalForm::GeminiFinishReason,
    refusal_form: ProviderRefusalForm::GeminiSafetyBlock,
    in_band_error_form: ProviderInBandErrorForm::GeminiErrorEnvelope,
    retry: COMMON_RETRY_CAPABILITIES,
}];
const MOCK_CAPABILITIES: &[ProviderTransportCapabilities] = &[ProviderTransportCapabilities {
    transport: ProviderTransport::Local,
    complete: true,
    stream: true,
    tools: true,
    tool_continuation: true,
    parallel_tools: true,
    reasoning: ProviderReasoningSupport::ProviderDefaultOnly,
    endpoint_shape: ProviderEndpointShape::Local,
    terminal_form: ProviderTerminalForm::LocalDeterministic,
    refusal_form: ProviderRefusalForm::None,
    in_band_error_form: ProviderInBandErrorForm::None,
    retry: NO_RETRY_CAPABILITIES,
}];

pub const PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "openai",
        display_name: "OpenAI",
        implementation: ProviderImplementation::OpenAi,
        credential_environment_variable: Some("OPENAI_API_KEY"),
        default_base_url: Some("https://api.openai.com/v1"),
        capabilities: OPENAI_CAPABILITIES,
        auth_api_default: Some(LlmApiMode::Responses),
        config_parser: crate::llm::factory::parse_openai_provider_config,
        constructor: crate::llm::factory::construct_openai_provider,
    },
    ProviderDescriptor {
        id: "anthropic",
        display_name: "Anthropic Claude",
        implementation: ProviderImplementation::Anthropic,
        credential_environment_variable: Some("ANTHROPIC_API_KEY"),
        default_base_url: Some("https://api.anthropic.com"),
        capabilities: ANTHROPIC_CAPABILITIES,
        auth_api_default: None,
        config_parser: crate::llm::factory::parse_anthropic_provider_config,
        constructor: crate::llm::factory::construct_anthropic_provider,
    },
    ProviderDescriptor {
        id: "google",
        display_name: "Google Gemini",
        implementation: ProviderImplementation::Gemini,
        credential_environment_variable: Some("GOOGLE_API_KEY"),
        default_base_url: Some("https://generativelanguage.googleapis.com/v1beta"),
        capabilities: GEMINI_CAPABILITIES,
        auth_api_default: None,
        config_parser: crate::llm::factory::parse_gemini_provider_config,
        constructor: crate::llm::factory::construct_gemini_provider,
    },
    ProviderDescriptor {
        id: "grok",
        display_name: "xAI Grok",
        implementation: ProviderImplementation::Xai,
        credential_environment_variable: Some("XAI_API_KEY"),
        default_base_url: Some("https://api.x.ai/v1"),
        capabilities: XAI_CAPABILITIES,
        auth_api_default: Some(LlmApiMode::ChatCompletions),
        config_parser: crate::llm::factory::parse_xai_provider_config,
        constructor: crate::llm::factory::construct_xai_provider,
    },
    ProviderDescriptor {
        id: "openrouter",
        display_name: "OpenRouter",
        implementation: ProviderImplementation::OpenRouter,
        credential_environment_variable: Some("OPENROUTER_API_KEY"),
        default_base_url: Some("https://openrouter.ai/api/v1"),
        capabilities: OPENROUTER_CAPABILITIES,
        auth_api_default: Some(LlmApiMode::ChatCompletions),
        config_parser: crate::llm::factory::parse_openrouter_provider_config,
        constructor: crate::llm::factory::construct_openrouter_provider,
    },
    ProviderDescriptor {
        id: "meta",
        display_name: "Meta (OpenAI-compatible endpoint required)",
        implementation: ProviderImplementation::Meta,
        credential_environment_variable: Some("META_API_KEY"),
        // Meta has no verified public default inference endpoint for this adapter.
        default_base_url: None,
        capabilities: META_CAPABILITIES,
        auth_api_default: Some(LlmApiMode::ChatCompletions),
        config_parser: crate::llm::factory::parse_meta_provider_config,
        constructor: crate::llm::factory::construct_meta_provider,
    },
    ProviderDescriptor {
        id: "mock",
        display_name: "Mock",
        implementation: ProviderImplementation::Mock,
        credential_environment_variable: None,
        default_base_url: None,
        capabilities: MOCK_CAPABILITIES,
        auth_api_default: None,
        config_parser: crate::llm::factory::parse_mock_provider_config,
        constructor: crate::llm::factory::construct_mock_provider,
    },
];

pub fn provider_descriptor(id: &str) -> Option<&'static ProviderDescriptor> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

pub fn retry_capabilities(
    provider_id: &str,
    transport: ProviderTransport,
) -> ProviderRetryCapabilities {
    provider_descriptor(provider_id)
        .and_then(|provider| provider.capabilities_for(transport))
        .map(|capabilities| capabilities.retry)
        .unwrap_or(NO_RETRY_CAPABILITIES)
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
            assert!(!provider.capabilities.is_empty());
        }
    }

    #[test]
    fn structural_capability_matrix_is_explicit_and_has_unique_transports() {
        for provider in PROVIDERS {
            let transports = provider.transports().collect::<BTreeSet<_>>();
            assert_eq!(
                transports.len(),
                provider.capabilities.len(),
                "{}",
                provider.id
            );
            for capability in provider.capabilities {
                assert!(
                    capability.complete,
                    "{} {} complete",
                    provider.id, capability.transport
                );
                assert!(
                    capability.stream,
                    "{} {} stream",
                    provider.id, capability.transport
                );
                assert!(
                    capability.tools,
                    "{} {} tools",
                    provider.id, capability.transport
                );
                assert_eq!(
                    provider.capabilities_for(capability.transport),
                    Some(*capability)
                );
                if capability.transport == ProviderTransport::Local {
                    assert!(capability.retry.retryable_http_statuses.is_empty());
                    assert!(capability.retry.retry_after_http_statuses.is_empty());
                    assert!(capability
                        .retry
                        .credential_rotation_http_statuses
                        .is_empty());
                } else {
                    assert_eq!(
                        &capability.retry.retryable_http_statuses[..7],
                        COMMON_RETRYABLE_HTTP_STATUSES
                    );
                    assert_eq!(
                        &capability.retry.retry_after_http_statuses[..2],
                        COMMON_RETRY_AFTER_HTTP_STATUSES
                    );
                    assert_eq!(
                        capability.retry.credential_rotation_http_statuses,
                        RATE_LIMIT_CREDENTIAL_ROTATION_STATUSES
                    );
                }
            }
        }

        let anthropic = provider_descriptor("anthropic")
            .expect("Anthropic descriptor")
            .capabilities[0]
            .retry;
        assert_eq!(
            anthropic.retryable_http_statuses,
            ANTHROPIC_RETRYABLE_HTTP_STATUSES
        );
        assert_eq!(
            anthropic.retry_after_http_statuses,
            ANTHROPIC_RETRY_AFTER_HTTP_STATUSES
        );
        assert_eq!(
            retry_capabilities("unknown", ProviderTransport::ChatCompletions),
            NO_RETRY_CAPABILITIES
        );
        assert_eq!(
            retry_capabilities("anthropic", ProviderTransport::Responses),
            NO_RETRY_CAPABILITIES
        );

        let mock = provider_descriptor("mock")
            .expect("Mock descriptor")
            .capabilities[0];
        assert!(mock.tools);
        assert!(mock.tool_continuation);
        assert!(mock.parallel_tools);

        for provider in PROVIDERS {
            assert!(
                provider
                    .capabilities
                    .iter()
                    .all(|capability| capability.tool_continuation && capability.parallel_tools),
                "{}",
                provider.id
            );
        }

        for provider in PROVIDERS {
            let configurable = provider.is_openai_compatible();
            assert_eq!(provider.supports_api_mode_selection(), configurable);
            assert_eq!(provider.supports_configurable_reasoning(), configurable);
        }
    }

    #[test]
    fn registry_labels_and_capability_diagnostics_are_stable() {
        let openai = provider_descriptor("openai").expect("OpenAI descriptor");
        let responses = openai
            .capabilities_for(ProviderTransport::Responses)
            .expect("Responses capabilities");
        assert_eq!(openai.implementation.to_string(), "openai");
        assert_eq!(responses.transport.to_string(), "responses");
        assert_eq!(
            responses.diagnostic_summary(),
            "complete=true, stream=true, tools=true, tool_continuation=true, parallel_tools=true, reasoning=configurable_effort, endpoint_shape=api_root_or_transport_endpoint, terminal_form=responses_status, refusal_form=responses_output_item, in_band_error_form=responses_error_event, retry_statuses=408/425/429/500/502/503/504, retry_after_statuses=429/503, credential_rotation_statuses=429"
        );

        let anthropic = provider_descriptor("anthropic").expect("Anthropic descriptor");
        assert_eq!(
            anthropic.capabilities[0].reasoning,
            ProviderReasoningSupport::ProviderDefaultOnly
        );
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
