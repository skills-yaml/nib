//! LLM client factory from project config.

use crate::config::{
    is_openai_compatible_provider, LlmApiMode, LlmConfig, ProviderEntry, ReasoningEffort,
};
use reqwest::Url;
use std::sync::Arc;

use super::anthropic::{normalize_anthropic_endpoint, AnthropicClient};
use super::compatible::{MetaProvider, OpenAiProvider, OpenRouterProvider, XaiProvider};
use super::gemini::{normalize_gemini_api_root, GeminiClient};
use super::mock::MockLlmClient;
use super::registry::{
    provider_descriptor, ProviderDescriptor, ProviderImplementation, ProviderTransport,
    ProviderTransportCapabilities, PROVIDERS,
};
use super::LlmClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDiagnostics {
    pub provider: String,
    pub model: String,
    pub implementation: ProviderImplementation,
    pub transport: ProviderTransport,
    pub capabilities: ProviderTransportCapabilities,
    pub api_mode: String,
    pub endpoint_path: String,
    pub reasoning_effort: String,
    pub warning: Option<String>,
}

impl ProviderDiagnostics {
    pub fn lines(&self) -> Vec<String> {
        self.rendered_lines(diagnostic_value)
    }

    pub fn redacted_lines(&self, sensitive_values: &[String]) -> Vec<String> {
        let sensitive_values = provider_error_sensitive_values(sensitive_values.iter().cloned());
        self.rendered_lines(|value| {
            diagnostic_value(&redact_sensitive_value(value, &sensitive_values))
        })
    }

    fn rendered_lines(&self, render_value: impl Fn(&str) -> String) -> Vec<String> {
        let mut lines = vec![
            format!("Provider: {}", render_value(&self.provider)),
            format!("Model: {}", render_value(&self.model)),
            format!("Implementation: {}", self.implementation),
            format!("Transport: {}", self.transport),
            format!(
                "Adapter capabilities: {}",
                self.capabilities.diagnostic_summary()
            ),
            format!("API mode: {}", render_value(&self.api_mode)),
            format!("Endpoint path: {}", render_value(&self.endpoint_path)),
            format!("Reasoning effort: {}", render_value(&self.reasoning_effort)),
        ];
        if let Some(warning) = &self.warning {
            lines.push(format!("Warning: {}", render_value(warning)));
        }
        lines
    }
}

pub fn redact_sensitive_value(value: &str, sensitive_values: &[String]) -> String {
    let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
        value,
        sensitive_values.iter().cloned(),
    );
    if redacted != value {
        "<redacted>".to_string()
    } else {
        value.to_string()
    }
}

pub fn provider_environment_credentials() -> Vec<String> {
    let mut credentials = PROVIDERS
        .iter()
        .filter_map(|provider| env_key(provider.id))
        .map(|credential| credential.trim().to_string())
        .filter(|credential| !credential.is_empty())
        .collect::<Vec<_>>();
    credentials.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    credentials.dedup();
    credentials
}

pub(crate) fn provider_error_sensitive_values(
    configured_values: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut values = configured_values.into_iter().collect::<Vec<_>>();
    values.extend(provider_environment_credentials());
    values.extend(
        values
            .iter()
            .map(|value| value.trim().to_string())
            .collect::<Vec<_>>(),
    );
    values.retain(|value| !value.is_empty());
    values.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    values.dedup();
    values
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedAdapter {
    Mock,
    Anthropic {
        endpoint: String,
    },
    Google {
        api_root: String,
    },
    OpenAiCompatible {
        api_mode: LlmApiMode,
        endpoint: String,
        reasoning_effort: Option<ReasoningEffort>,
    },
}

pub(crate) struct ParsedProviderConfiguration {
    adapter: ResolvedAdapter,
    transport: ProviderTransport,
    api_mode: String,
    endpoint_path: String,
    warning: Option<String>,
}

pub(crate) type ProviderConfigParser = fn(
    &'static ProviderDescriptor,
    Option<&ProviderEntry>,
    &str,
) -> Result<ParsedProviderConfiguration, String>;

pub(crate) struct ProviderConstruction {
    adapter: ResolvedAdapter,
    model: String,
    credentials: Vec<String>,
    diagnostic_secrets: Vec<String>,
    http_client: Option<reqwest::Client>,
}

pub(crate) type ProviderConstructor =
    fn(ProviderConstruction) -> Result<Arc<dyn LlmClient>, String>;

#[derive(Debug, Clone)]
struct ResolvedProvider {
    descriptor: &'static ProviderDescriptor,
    diagnostics: ProviderDiagnostics,
    adapter: ResolvedAdapter,
}

pub fn create_client(
    llm: &LlmConfig,
    provider_override: Option<&str>,
) -> Result<Arc<dyn LlmClient>, String> {
    create_client_with_sensitive_values_and_http_client(
        llm,
        provider_override,
        &llm_configured_sensitive_values(llm),
        None,
    )
}

/// Creates an OpenAI-compatible production adapter over a caller-reviewed HTTP client.
///
/// This is used by credentialed qualification after it has resolved, rejected, and
/// pinned a configured endpoint origin. Ordinary runtime callers should use
/// [`create_client`]. Non-OpenAI-compatible providers reject this override rather than
/// silently dropping its transport protections.
pub fn create_client_with_http_client(
    llm: &LlmConfig,
    provider_override: Option<&str>,
    client: reqwest::Client,
) -> Result<Arc<dyn LlmClient>, String> {
    create_client_with_sensitive_values_and_http_client(
        llm,
        provider_override,
        &llm_configured_sensitive_values(llm),
        Some(client),
    )
}

pub(crate) fn create_client_with_sensitive_values(
    llm: &LlmConfig,
    provider_override: Option<&str>,
    configured_sensitive_values: &[String],
) -> Result<Arc<dyn LlmClient>, String> {
    create_client_with_sensitive_values_and_http_client(
        llm,
        provider_override,
        configured_sensitive_values,
        None,
    )
}

fn create_client_with_sensitive_values_and_http_client(
    llm: &LlmConfig,
    provider_override: Option<&str>,
    configured_sensitive_values: &[String],
    http_client: Option<reqwest::Client>,
) -> Result<Arc<dyn LlmClient>, String> {
    let resolved = resolve_provider(llm, provider_override)?;
    let provider = resolved.diagnostics.provider.clone();
    let model = resolved.diagnostics.model.clone();
    let implementation = resolved.diagnostics.implementation;

    if http_client.is_some() && !resolved.descriptor.is_openai_compatible() {
        return Err(
            "reviewed HTTP client override requires an OpenAI-compatible provider".to_string(),
        );
    }

    let entry = llm.get_provider(Some(&provider));
    let credentials = if implementation == ProviderImplementation::Mock {
        Vec::new()
    } else {
        require_credentials(provider_credentials(entry, &provider), &provider)?
    };
    let diagnostic_secrets =
        provider_error_sensitive_values(configured_sensitive_values.iter().cloned());
    (resolved.descriptor.constructor)(ProviderConstruction {
        adapter: resolved.adapter,
        model,
        credentials,
        diagnostic_secrets,
        http_client,
    })
}

pub fn provider_diagnostics(
    llm: &LlmConfig,
    provider_override: Option<&str>,
) -> Result<ProviderDiagnostics, String> {
    resolve_provider(llm, provider_override).map(|resolved| resolved.diagnostics)
}

pub fn validate_provider_endpoints(llm: &LlmConfig) -> Result<(), String> {
    let mut providers = llm
        .providers
        .keys()
        .filter(|provider| is_openai_compatible_provider(provider))
        .map(String::as_str)
        .collect::<Vec<_>>();
    providers.sort_unstable();
    for provider in providers {
        resolve_provider(llm, Some(provider))?;
    }
    Ok(())
}

fn resolve_provider(
    llm: &LlmConfig,
    provider_override: Option<&str>,
) -> Result<ResolvedProvider, String> {
    let provider = provider_override
        .map(str::to_string)
        .unwrap_or_else(|| llm.get_active_provider());
    let descriptor = provider_descriptor(&provider)
        .ok_or_else(|| format!("unsupported LLM provider: {provider}"))?;
    if descriptor.id != provider {
        return Err(format!("unsupported LLM provider: {provider}"));
    }

    let entry = llm.get_provider(Some(&provider));
    if descriptor.implementation == ProviderImplementation::Mock
        && entry.is_some_and(|entry| {
            entry.api_key.is_some() || !entry.api_keys.is_empty() || entry.base_url.is_some()
        })
    {
        return Err(
            "LLM provider 'mock' does not consume api_key, api_keys, or base_url".to_string(),
        );
    }
    if entry.is_some_and(|entry| {
        (entry.api.is_some() && !descriptor.supports_api_mode_selection())
            || (entry.reasoning_effort.is_some() && !descriptor.supports_configurable_reasoning())
    }) {
        return Err(format!(
            "LLM provider '{provider}' does not consume api or reasoning_effort"
        ));
    }
    let model = entry
        .map(|entry| entry.model.clone())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| descriptor.default_model().to_string());
    let reasoning_label = entry.and_then(|entry| entry.reasoning_effort).map_or_else(
        || "provider_default".to_string(),
        |effort| effort.to_string(),
    );

    let parsed = (descriptor.config_parser)(descriptor, entry, &model)?;
    let capabilities = descriptor
        .capabilities_for(parsed.transport)
        .ok_or_else(|| {
            format!(
                "LLM provider '{provider}' registry does not declare transport '{}'",
                parsed.transport
            )
        })?;

    Ok(ResolvedProvider {
        descriptor,
        diagnostics: ProviderDiagnostics {
            provider,
            model,
            implementation: descriptor.implementation,
            transport: parsed.transport,
            capabilities,
            api_mode: parsed.api_mode,
            endpoint_path: parsed.endpoint_path,
            reasoning_effort: reasoning_label,
            warning: parsed.warning,
        },
        adapter: parsed.adapter,
    })
}

pub(crate) fn parse_mock_provider_config(
    _descriptor: &'static ProviderDescriptor,
    _entry: Option<&ProviderEntry>,
    _model: &str,
) -> Result<ParsedProviderConfiguration, String> {
    Ok(ParsedProviderConfiguration {
        adapter: ResolvedAdapter::Mock,
        transport: ProviderTransport::Local,
        api_mode: "local".to_string(),
        endpoint_path: "local".to_string(),
        warning: None,
    })
}

pub(crate) fn parse_anthropic_provider_config(
    descriptor: &'static ProviderDescriptor,
    entry: Option<&ProviderEntry>,
    _model: &str,
) -> Result<ParsedProviderConfiguration, String> {
    let configured = resolved_native_base_url(descriptor, entry)?;
    let endpoint = normalize_anthropic_endpoint(configured)?;
    let path = Url::parse(&endpoint)
        .map_err(|_| "resolved Anthropic endpoint is invalid".to_string())?
        .path()
        .to_string();
    Ok(ParsedProviderConfiguration {
        adapter: ResolvedAdapter::Anthropic { endpoint },
        transport: ProviderTransport::AnthropicMessages,
        api_mode: "provider_native".to_string(),
        endpoint_path: path,
        warning: None,
    })
}

pub(crate) fn parse_gemini_provider_config(
    descriptor: &'static ProviderDescriptor,
    entry: Option<&ProviderEntry>,
    model: &str,
) -> Result<ParsedProviderConfiguration, String> {
    let configured = resolved_native_base_url(descriptor, entry)?;
    let api_root = normalize_gemini_api_root(configured)?;
    let path = format!(
        "{}/models/{model}:generateContent",
        Url::parse(&api_root)
            .map_err(|_| "resolved Gemini API root is invalid".to_string())?
            .path()
            .trim_end_matches('/')
    );
    Ok(ParsedProviderConfiguration {
        adapter: ResolvedAdapter::Google { api_root },
        transport: ProviderTransport::GeminiGenerateContent,
        api_mode: "provider_native".to_string(),
        endpoint_path: path,
        warning: None,
    })
}

fn parse_openai_compatible_provider_config(
    descriptor: &'static ProviderDescriptor,
    entry: Option<&ProviderEntry>,
) -> Result<ParsedProviderConfiguration, String> {
    let api_mode = entry.map_or(LlmApiMode::default(), ProviderEntry::resolved_api_mode);
    let endpoint = resolve_openai_compatible_endpoint(descriptor.id, entry, api_mode)?;
    Ok(ParsedProviderConfiguration {
        adapter: ResolvedAdapter::OpenAiCompatible {
            api_mode,
            endpoint: endpoint.url,
            reasoning_effort: entry.and_then(|entry| entry.reasoning_effort),
        },
        transport: ProviderTransport::from_api_mode(api_mode),
        api_mode: api_mode.to_string(),
        endpoint_path: endpoint.path,
        warning: chat_compatibility_warning(api_mode, endpoint.canonical_openai),
    })
}

macro_rules! compatible_config_parser {
    ($name:ident) => {
        pub(crate) fn $name(
            descriptor: &'static ProviderDescriptor,
            entry: Option<&ProviderEntry>,
            _model: &str,
        ) -> Result<ParsedProviderConfiguration, String> {
            parse_openai_compatible_provider_config(descriptor, entry)
        }
    };
}

compatible_config_parser!(parse_openai_provider_config);
compatible_config_parser!(parse_xai_provider_config);
compatible_config_parser!(parse_openrouter_provider_config);
compatible_config_parser!(parse_meta_provider_config);

type CompatibleConstructionParts = (
    String,
    Vec<String>,
    Vec<String>,
    String,
    Option<ReasoningEffort>,
    LlmApiMode,
    Option<reqwest::Client>,
);

fn compatible_construction_parts(
    construction: ProviderConstruction,
) -> Result<CompatibleConstructionParts, String> {
    let ResolvedAdapter::OpenAiCompatible {
        api_mode,
        endpoint,
        reasoning_effort,
    } = construction.adapter
    else {
        return Err("provider constructor received an incompatible resolved adapter".to_string());
    };
    Ok((
        construction.model,
        construction.credentials,
        construction.diagnostic_secrets,
        endpoint,
        reasoning_effort,
        api_mode,
        construction.http_client,
    ))
}

macro_rules! compatible_constructor {
    ($name:ident, $provider:ty) => {
        pub(crate) fn $name(
            construction: ProviderConstruction,
        ) -> Result<Arc<dyn LlmClient>, String> {
            let (model, credentials, secrets, endpoint, reasoning, api_mode, http_client) =
                compatible_construction_parts(construction)?;
            Ok(Arc::new(<$provider>::configured(
                model,
                credentials,
                secrets,
                endpoint,
                reasoning,
                api_mode,
                http_client,
            )))
        }
    };
}

compatible_constructor!(construct_openai_provider, OpenAiProvider);
compatible_constructor!(construct_xai_provider, XaiProvider);
compatible_constructor!(construct_openrouter_provider, OpenRouterProvider);
compatible_constructor!(construct_meta_provider, MetaProvider);

pub(crate) fn construct_anthropic_provider(
    construction: ProviderConstruction,
) -> Result<Arc<dyn LlmClient>, String> {
    let ResolvedAdapter::Anthropic { endpoint } = construction.adapter else {
        return Err("Anthropic constructor received an incompatible resolved adapter".to_string());
    };
    if construction.http_client.is_some() {
        return Err("Anthropic constructor does not accept a reviewed HTTP client".to_string());
    }
    Ok(Arc::new(
        AnthropicClient::configured_with_diagnostic_secrets(
            construction.model,
            construction.credentials,
            construction.diagnostic_secrets,
            endpoint,
        )?,
    ))
}

pub(crate) fn construct_gemini_provider(
    construction: ProviderConstruction,
) -> Result<Arc<dyn LlmClient>, String> {
    let ResolvedAdapter::Google { api_root } = construction.adapter else {
        return Err("Gemini constructor received an incompatible resolved adapter".to_string());
    };
    if construction.http_client.is_some() {
        return Err("Gemini constructor does not accept a reviewed HTTP client".to_string());
    }
    Ok(Arc::new(GeminiClient::configured_with_diagnostic_secrets(
        construction.model,
        construction.credentials,
        construction.diagnostic_secrets,
        api_root,
    )?))
}

pub(crate) fn construct_mock_provider(
    construction: ProviderConstruction,
) -> Result<Arc<dyn LlmClient>, String> {
    if construction.adapter != ResolvedAdapter::Mock
        || !construction.credentials.is_empty()
        || construction.http_client.is_some()
    {
        return Err("Mock constructor received incompatible provider configuration".to_string());
    }
    Ok(Arc::new(MockLlmClient::new()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedEndpoint {
    url: String,
    path: String,
    canonical_openai: bool,
}

fn resolved_native_base_url<'a>(
    descriptor: &'static ProviderDescriptor,
    entry: Option<&'a ProviderEntry>,
) -> Result<&'a str, String> {
    if let Some(base_url) = entry.and_then(|entry| entry.base_url.as_deref()) {
        return Ok(base_url);
    }
    descriptor.default_base_url.ok_or_else(|| {
        format!(
            "LLM provider '{}' requires an explicit verified base_url",
            descriptor.id
        )
    })
}

fn resolve_openai_compatible_endpoint(
    provider: &str,
    entry: Option<&ProviderEntry>,
    api_mode: LlmApiMode,
) -> Result<ResolvedEndpoint, String> {
    let configured = entry
        .and_then(|entry| entry.base_url.as_deref())
        .or_else(|| {
            provider_descriptor(provider).and_then(|provider| provider.default_base_url)
        })
        .ok_or_else(|| {
            format!(
                "LLM provider '{provider}' requires an explicit verified base_url; no public default endpoint is configured"
            )
        })?;
    let parsed = Url::parse(configured.trim())
        .map_err(|error| format!("LLM provider '{provider}' base_url is invalid: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(format!(
            "LLM provider '{provider}' base_url must be an absolute HTTP(S) URL"
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!(
            "LLM provider '{provider}' base_url must not contain embedded credentials"
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(format!(
            "LLM provider '{provider}' base_url must not contain a query string or fragment"
        ));
    }

    let selected = api_mode.endpoint_suffix();
    let path = parsed.path().trim_end_matches('/');
    let decoded_path = percent_decode_repeated(path).ok_or_else(|| {
        format!(
            "LLM provider '{provider}' base_url percent-encoding nesting exceeds the supported limit"
        )
    })?;
    if decoded_path != path && terminal_api_modes(&decoded_path) != terminal_api_modes(path) {
        return Err(format!(
            "LLM provider '{provider}' base_url API endpoint suffix must not be percent-encoded"
        ));
    }
    let terminal_mode = terminal_api_mode(path);
    if terminal_mode.is_some_and(|mode| mode != api_mode) {
        return Err(format!(
            "LLM provider '{provider}' base_url conflicts with api = '{api_mode}'"
        ));
    }
    let matching_full_endpoint = terminal_mode == Some(api_mode);
    if matching_full_endpoint {
        let prefix = path.strip_suffix(selected).expect("matching suffix");
        if terminal_api_mode(prefix).is_some() {
            return Err(format!(
                "LLM provider '{provider}' base_url contains a doubled API suffix"
            ));
        }
    }

    let url = if matching_full_endpoint {
        parsed.as_str().trim_end_matches('/').to_string()
    } else {
        format!("{}{selected}", parsed.as_str().trim_end_matches('/'))
    };
    let endpoint = Url::parse(&url)
        .map_err(|error| format!("failed to construct {provider} API endpoint: {error}"))?;
    let canonical_openai = provider == "openai"
        && endpoint.scheme() == "https"
        && endpoint.host_str() == Some("api.openai.com")
        && endpoint.port_or_known_default() == Some(443)
        && endpoint.path() == format!("/v1{selected}");

    Ok(ResolvedEndpoint {
        url,
        path: endpoint.path().to_string(),
        canonical_openai,
    })
}

fn terminal_api_mode(path: &str) -> Option<LlmApiMode> {
    let path = path.trim_end_matches('/');
    [LlmApiMode::ChatCompletions, LlmApiMode::Responses]
        .into_iter()
        .find(|mode| path.ends_with(mode.endpoint_suffix()))
}

fn terminal_api_modes(mut path: &str) -> Vec<LlmApiMode> {
    let mut modes = Vec::new();
    while let Some(mode) = terminal_api_mode(path) {
        modes.push(mode);
        path = path
            .trim_end_matches('/')
            .strip_suffix(mode.endpoint_suffix())
            .expect("terminal mode suffix");
    }
    modes
}

fn percent_decode_repeated(value: &str) -> Option<String> {
    const MAX_PERCENT_DECODE_PASSES: usize = 8;

    let mut decoded = value.to_string();
    let mut passes = 0;
    loop {
        let next = percent_decode_once(&decoded);
        if next == decoded {
            return Some(decoded);
        }
        if passes == MAX_PERCENT_DECODE_PASSES {
            return None;
        }
        decoded = next;
        passes += 1;
    }
}

fn percent_decode_once(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn chat_compatibility_warning(api_mode: LlmApiMode, canonical_openai: bool) -> Option<String> {
    if api_mode != LlmApiMode::ChatCompletions {
        return None;
    }
    Some(if canonical_openai {
        "canonical OpenAI Chat Completions is selected; tool and reasoning compatibility is model-dependent, so consider api = \"responses\" for those workflows"
            .to_string()
    } else {
        "Chat Completions tool and reasoning compatibility is provider- and model-dependent"
            .to_string()
    })
}

fn diagnostic_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_graphic() || character == ' ' {
            escaped.push(character);
        } else {
            escaped.extend(character.escape_default());
        }
    }
    escaped
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

fn llm_configured_sensitive_values(llm: &LlmConfig) -> Vec<String> {
    let mut values = llm
        .providers
        .values()
        .flat_map(|entry| entry.api_key.iter().chain(entry.api_keys.iter()).cloned())
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>();
    values.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    values.dedup();
    values
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

#[cfg(test)]
fn default_model(provider: &str) -> String {
    provider_descriptor(provider)
        .map_or("mock-model", |provider| provider.default_model())
        .to_string()
}

fn env_key(provider: &str) -> Option<String> {
    let variable = provider_descriptor(provider)?.credential_environment_variable?;
    std::env::var(variable).ok()
}

pub fn provider_ready(entry: Option<&ProviderEntry>, provider: &str) -> bool {
    let Some(descriptor) = provider_descriptor(provider) else {
        return false;
    };
    if descriptor.implementation == ProviderImplementation::Mock {
        return true;
    }
    let endpoint_ready = descriptor.default_base_url.is_some()
        || entry
            .and_then(|entry| entry.base_url.as_deref())
            .is_some_and(|base_url| !base_url.trim().is_empty());
    endpoint_ready && !provider_credentials(entry, provider).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::LlmRequest;
    use serde_json::json;
    use serial_test::serial;
    use std::collections::HashMap;

    struct EnvironmentGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvironmentGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    fn compatible_config(
        provider: &str,
        api: Option<LlmApiMode>,
        base_url: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> LlmConfig {
        LlmConfig {
            active_provider: Some(provider.to_string()),
            providers: HashMap::from([(
                provider.to_string(),
                ProviderEntry {
                    model: "fixture-model".to_string(),
                    api_key: Some("fixture-key".to_string()),
                    base_url: base_url.map(str::to_string),
                    api,
                    reasoning_effort,
                    ..ProviderEntry::default()
                },
            )]),
            context_length: 128_000,
        }
    }

    #[test]
    fn configured_key_pool_keeps_legacy_primary_and_deduplicates() {
        let entry = ProviderEntry {
            model: "model".to_string(),
            api_key: Some("primary".to_string()),
            api_keys: vec!["backup".to_string(), "primary".to_string()],
            ..ProviderEntry::default()
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
                        base_url: Some("http://127.0.0.1:1".to_string()),
                        ..ProviderEntry::default()
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
    fn reviewed_http_client_override_is_limited_to_openai_compatible_adapters() {
        let reviewed = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reviewed client");
        let meta = compatible_config(
            "meta",
            Some(LlmApiMode::ChatCompletions),
            Some("https://meta.example.invalid/v1"),
            None,
        );
        create_client_with_http_client(&meta, None, reviewed.clone())
            .expect("Meta uses the production compatible adapter with reviewed transport");

        for provider in ["mock", "anthropic", "google"] {
            let (model, api_key) = if provider == "mock" {
                ("mock-model", None)
            } else {
                ("fixture-model", Some("fixture-key".to_string()))
            };
            let config = LlmConfig {
                active_provider: Some(provider.to_string()),
                providers: HashMap::from([(
                    provider.to_string(),
                    ProviderEntry {
                        model: model.to_string(),
                        api_key,
                        ..ProviderEntry::default()
                    },
                )]),
                context_length: 128_000,
            };
            let error = create_client_with_http_client(&config, None, reviewed.clone())
                .err()
                .expect("native adapter must not drop reviewed transport");
            assert!(error.contains("requires an OpenAI-compatible provider"));
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
    fn factory_rejects_every_unused_mock_transport_or_credential_field() {
        let entries = [
            ProviderEntry {
                model: "mock-model".to_string(),
                api_key: Some("private-primary".to_string()),
                ..ProviderEntry::default()
            },
            ProviderEntry {
                model: "mock-model".to_string(),
                api_keys: vec!["private-backup".to_string()],
                ..ProviderEntry::default()
            },
            ProviderEntry {
                model: "mock-model".to_string(),
                base_url: Some("http://127.0.0.1:9".to_string()),
                ..ProviderEntry::default()
            },
        ];
        for entry in entries {
            let config = LlmConfig {
                active_provider: Some("mock".to_string()),
                providers: HashMap::from([("mock".to_string(), entry)]),
                ..LlmConfig::default()
            };
            for operation in ["complete", "stream"] {
                let error = create_client(&config, None)
                    .err()
                    .expect("unused Mock field must fail before either request mode");
                assert!(error.contains("does not consume api_key, api_keys, or base_url"));
                assert!(!error.contains("private-"), "{operation}: {error}");
            }
            assert!(provider_diagnostics(&config, None).is_err());
        }
    }

    #[tokio::test]
    async fn factory_mock_consumes_model_fields_for_complete_and_stream() {
        let config = LlmConfig {
            active_provider: Some("mock".to_string()),
            providers: HashMap::from([(
                "mock".to_string(),
                ProviderEntry {
                    model: "mock-selected".to_string(),
                    models: Some(vec!["mock-selected".to_string(), "mock-alt".to_string()]),
                    ..ProviderEntry::default()
                },
            )]),
            ..LlmConfig::default()
        };
        let diagnostics = provider_diagnostics(&config, None).expect("Mock diagnostics");
        assert_eq!(diagnostics.model, "mock-selected");
        let client = create_client(&config, None).expect("configured Mock client");
        let messages = [crate::llm::LlmMessage::user("safe Mock completion")];
        let complete = client
            .complete(crate::llm::LlmRequest::new(&messages, None))
            .await
            .expect("Mock complete");
        let streamed = client
            .stream(crate::llm::LlmRequest::new(&messages, None))
            .await
            .expect("Mock stream")
            .finish()
            .await
            .expect("Mock streamed completion");
        assert_eq!(complete.content, streamed.content);
        assert_eq!(complete.finish_reason, streamed.finish_reason);
    }

    #[test]
    fn factory_rejects_transport_fields_for_provider_native_adapters() {
        let config = LlmConfig {
            active_provider: Some("anthropic".to_string()),
            providers: HashMap::from([(
                "anthropic".to_string(),
                ProviderEntry {
                    model: "claude".to_string(),
                    api: Some(LlmApiMode::Responses),
                    ..ProviderEntry::default()
                },
            )]),
            ..LlmConfig::default()
        };
        let error = provider_diagnostics(&config, None).expect_err("unused transport field");
        assert!(error.contains("does not consume api or reasoning_effort"));
    }

    #[test]
    fn legacy_and_typed_modes_resolve_one_exact_endpoint() {
        let legacy = compatible_config("openai", None, None, None);
        let legacy = provider_diagnostics(&legacy, None).expect("legacy diagnostics");
        assert_eq!(legacy.implementation, ProviderImplementation::OpenAi);
        assert_eq!(legacy.transport, ProviderTransport::ChatCompletions);
        assert_eq!(legacy.api_mode, "chat_completions");
        assert_eq!(legacy.endpoint_path, "/v1/chat/completions");
        assert_eq!(
            legacy.capabilities.reasoning,
            super::super::registry::ProviderReasoningSupport::ConfigurableEffort
        );

        let responses = compatible_config(
            "openai",
            Some(LlmApiMode::Responses),
            Some("https://api.openai.com/v1/responses/"),
            Some(ReasoningEffort::High),
        );
        create_client(&responses, None).expect("Responses client construction");
        let responses = provider_diagnostics(&responses, None).expect("Responses diagnostics");
        assert_eq!(responses.implementation, ProviderImplementation::OpenAi);
        assert_eq!(responses.transport, ProviderTransport::Responses);
        assert_eq!(responses.api_mode, "responses");
        assert_eq!(responses.endpoint_path, "/v1/responses");
        assert_eq!(responses.reasoning_effort, "high");
        assert_eq!(responses.warning, None);
    }

    #[test]
    fn factory_diagnostics_select_native_registry_capabilities() {
        for (provider, implementation, transport) in [
            (
                "anthropic",
                ProviderImplementation::Anthropic,
                ProviderTransport::AnthropicMessages,
            ),
            (
                "google",
                ProviderImplementation::Gemini,
                ProviderTransport::GeminiGenerateContent,
            ),
            (
                "mock",
                ProviderImplementation::Mock,
                ProviderTransport::Local,
            ),
        ] {
            let config = LlmConfig {
                active_provider: Some(provider.to_string()),
                ..LlmConfig::default()
            };
            let diagnostics = provider_diagnostics(&config, None).expect("native diagnostics");
            assert_eq!(diagnostics.implementation, implementation, "{provider}");
            assert_eq!(diagnostics.transport, transport, "{provider}");
            assert_eq!(
                diagnostics.capabilities,
                provider_descriptor(provider)
                    .expect("registered provider")
                    .capabilities_for(transport)
                    .expect("registered transport"),
                "{provider}"
            );
        }
    }

    #[test]
    fn endpoint_resolution_rejects_conflicts_and_unsafe_url_components() {
        for (base_url, expected) in [
            ("https://example.test/v1/chat/completions", "conflicts"),
            ("https://example.test/v1/responses/responses", "doubled"),
            (
                "https://example.test/v1/%72esponses",
                "must not be percent-encoded",
            ),
            (
                "https://example.test/v1/responses/%2572esponses",
                "must not be percent-encoded",
            ),
            ("https://user:secret@example.test/v1", "credentials"),
            ("https://example.test/v1?token=secret", "query string"),
            ("https://example.test/v1#secret", "fragment"),
        ] {
            let config =
                compatible_config("openai", Some(LlmApiMode::Responses), Some(base_url), None);
            let error = provider_diagnostics(&config, None).expect_err("unsafe endpoint");
            assert!(error.contains(expected), "{base_url}: {error}");
            assert!(!error.contains("secret"), "credential leaked: {error}");
        }

        let nested_encoding = format!("https://example.test/v1/%{}72esponses", "25".repeat(32));
        let config = compatible_config(
            "openai",
            Some(LlmApiMode::Responses),
            Some(&nested_encoding),
            None,
        );
        let error = provider_diagnostics(&config, None).expect_err("nested encoding rejected");
        assert!(error.contains("nesting exceeds"), "{error}");
    }

    #[test]
    fn endpoint_resolution_accepts_reserved_nonterminal_root_segments() {
        let config = compatible_config(
            "openai",
            Some(LlmApiMode::Responses),
            Some("https://gateway.test/proxy/responses/v1"),
            None,
        );
        let diagnostics = provider_diagnostics(&config, None).expect("custom root diagnostics");
        assert_eq!(diagnostics.endpoint_path, "/proxy/responses/v1/responses");

        let encoded_tenant = compatible_config(
            "openai",
            Some(LlmApiMode::Responses),
            Some("https://gateway.test/tenant/acme%20corp/v1/responses"),
            None,
        );
        let diagnostics =
            provider_diagnostics(&encoded_tenant, None).expect("encoded tenant path diagnostics");
        assert_eq!(
            diagnostics.endpoint_path,
            "/tenant/acme%20corp/v1/responses"
        );
    }

    #[test]
    fn canonical_openai_chat_warning_is_model_agnostic_and_redacted_safe() {
        let config = compatible_config(
            "openai",
            Some(LlmApiMode::ChatCompletions),
            None,
            Some(ReasoningEffort::Medium),
        );
        let diagnostics = provider_diagnostics(&config, None).expect("diagnostics");
        let lines = diagnostics.lines().join("\n");
        assert!(lines.contains("Provider: openai"));
        assert!(lines.contains("Model: fixture-model"));
        assert!(lines.contains("Implementation: openai"));
        assert!(lines.contains("Transport: chat_completions"));
        assert!(lines.contains(
            "Adapter capabilities: complete=true, stream=true, tools=true, tool_continuation=true, parallel_tools=true, reasoning=configurable_effort, endpoint_shape=api_root_or_transport_endpoint, terminal_form=chat_finish_reason, refusal_form=chat_message, in_band_error_form=chat_error_envelope, retry_statuses=408/425/429/500/502/503/504, retry_after_statuses=429/503, credential_rotation_statuses=429"
        ));
        assert!(lines.contains("API mode: chat_completions"));
        assert!(lines.contains("Endpoint path: /v1/chat/completions"));
        assert!(lines.contains("Reasoning effort: medium"));
        assert!(lines.contains("consider api = \"responses\""));
        assert!(!lines.contains("fixture-key"));
    }

    #[test]
    fn diagnostic_lines_escape_control_and_non_ascii_content() {
        let mut config = compatible_config("openai", None, None, None);
        config.providers.get_mut("openai").unwrap().model = "model\n\u{2028}suffix".to_string();

        let lines = provider_diagnostics(&config, None)
            .expect("escaped diagnostics")
            .lines();
        assert!(lines.iter().all(|line| !line.contains('\n')));
        assert!(lines.iter().all(|line| !line.contains('\u{2028}')));
        let model = lines
            .iter()
            .find(|line| line.starts_with("Model: "))
            .expect("model diagnostic");
        assert!(model.contains(r"\n"), "{model}");
        assert!(model.contains(r"\u{2028}"), "{model}");
    }

    #[test]
    fn diagnostic_lines_redact_credentials_reused_in_safe_fields() {
        let mut config = compatible_config("openai", None, None, None);
        config.providers.get_mut("openai").unwrap().model = "model-fixture-key-suffix".to_string();

        let lines = provider_diagnostics(&config, None)
            .expect("provider diagnostics")
            .redacted_lines(&[" fixture-key ".to_string()])
            .join("\n");
        assert!(lines.contains("Model: <redacted>"));
        assert!(!lines.contains("fixture-key"));

        for (model, secret) in [
            (r#"active\/credential"#, "active/credential"),
            ("Zm9v", "foo"),
        ] {
            let mut config = compatible_config("openai", None, None, None);
            config.providers.get_mut("openai").unwrap().model = model.to_string();
            let lines = provider_diagnostics(&config, None)
                .expect("encoded provider diagnostics")
                .redacted_lines(&[secret.to_string()])
                .join("\n");
            assert!(lines.contains("Model: <redacted>"), "{lines}");
            assert!(!lines.contains(model), "{lines}");
        }
    }

    #[test]
    fn diagnostic_redaction_precedes_escaping_and_detects_url_encoding() {
        let mut config = compatible_config(
            "openai",
            Some(LlmApiMode::Responses),
            Some("https://gateway.test/abc%2Fdef/v1"),
            None,
        );
        config.providers.get_mut("openai").unwrap().model = "prefix-part\nkey-suffix".to_string();
        let diagnostics = provider_diagnostics(&config, None).expect("provider diagnostics");

        let control_lines = diagnostics.redacted_lines(&["part\nkey".to_string()]);
        assert!(control_lines.iter().any(|line| line == "Model: <redacted>"));
        assert!(control_lines
            .iter()
            .all(|line| !line.contains("part\\nkey")));

        let encoded_lines = diagnostics.redacted_lines(&["abc/def".to_string()]);
        assert!(encoded_lines
            .iter()
            .any(|line| line == "Endpoint path: <redacted>"));
        assert!(encoded_lines.iter().all(|line| !line.contains("abc%2Fdef")));

        let intermediate_lines = diagnostics.redacted_lines(&["abc%2Fdef".to_string()]);
        assert!(intermediate_lines
            .iter()
            .any(|line| line == "Endpoint path: <redacted>"));

        let decoded_config = compatible_config(
            "openai",
            Some(LlmApiMode::Responses),
            Some("https://gateway.test/abc/def/v1"),
            None,
        );
        let decoded_lines = provider_diagnostics(&decoded_config, None)
            .expect("decoded provider diagnostics")
            .redacted_lines(&["abc%2Fdef".to_string()]);
        assert!(decoded_lines
            .iter()
            .any(|line| line == "Endpoint path: <redacted>"));

        let adversarial = format!("prefix-%{}41-suffix", "25".repeat(32));
        assert_eq!(
            redact_sensitive_value(&adversarial, &["unrelated-secret".to_string()]),
            "<redacted>"
        );
    }

    #[tokio::test]
    #[serial]
    async fn factory_passes_inactive_environment_credentials_only_to_diagnostics() {
        const RAW_SECRET: &str = "inactive/env-only-secret";
        const ENCODED_SECRET: &str = "inactive%2Fenv-only-secret";
        let _environment = EnvironmentGuard::set("ANTHROPIC_API_KEY", RAW_SECRET);
        let (base_url, request_rx) = crate::llm::test_support::serve_once(
            "400 Bad Request",
            "application/json",
            json!({"error": {"type": "bad_request"}}).to_string(),
        );
        let config = LlmConfig {
            active_provider: Some("openai".to_string()),
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: format!("model-{ENCODED_SECRET}"),
                    api_key: Some("active-openai-key".to_string()),
                    base_url: Some(format!("{base_url}/v1/chat/completions")),
                    api: Some(LlmApiMode::ChatCompletions),
                    ..ProviderEntry::default()
                },
            )]),
            context_length: 128_000,
        };

        let client = create_client(&config, None).expect("OpenAI Chat client");
        let messages = [crate::llm::LlmMessage::user("inspect")];
        let error = client
            .complete(LlmRequest::new(&messages, None))
            .await
            .expect_err("fixture rejects request");
        let request = request_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("captured request");

        assert!(!error.contains(RAW_SECRET), "{error}");
        assert!(!error.contains(ENCODED_SECRET), "{error}");
        assert!(error.contains("[REDACTED]"), "{error}");
        let authorization = request
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
            .expect("Authorization header");
        assert_eq!(
            authorization.to_ascii_lowercase(),
            "authorization: bearer active-openai-key"
        );
        assert!(!authorization.contains(RAW_SECRET));
        assert!(!authorization.contains(ENCODED_SECRET));
    }

    #[test]
    fn readiness_and_provider_native_diagnostics_are_explicit() {
        let empty = ProviderEntry::default();
        let configured = ProviderEntry {
            api_keys: vec![" backup ".to_string()],
            base_url: Some("http://localhost:1234".to_string()),
            ..ProviderEntry::default()
        };
        assert!(provider_ready(None, "mock"));
        assert!(!provider_ready(Some(&empty), "provider-without-env"));
        assert!(!provider_ready(Some(&configured), "provider-without-env"));
        assert_eq!(env_key("provider-without-env"), None);

        let meta_without_endpoint = ProviderEntry {
            api_keys: vec!["key".to_string()],
            ..ProviderEntry::default()
        };
        assert!(!provider_ready(Some(&meta_without_endpoint), "meta"));

        let diagnostics = provider_diagnostics(&LlmConfig::default(), None).unwrap();
        assert_eq!(diagnostics.provider, "mock");
        assert_eq!(diagnostics.api_mode, "local");
        assert_eq!(diagnostics.endpoint_path, "local");
        assert_eq!(diagnostics.reasoning_effort, "provider_default");
    }
}
