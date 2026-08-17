//! LLM client factory from project config.

use crate::config::{
    is_openai_compatible_provider, LlmApiMode, LlmConfig, ProviderEntry, ReasoningEffort,
};
use reqwest::Url;
use std::sync::Arc;

use super::anthropic::{normalize_anthropic_endpoint, AnthropicClient};
use super::gemini::{normalize_gemini_api_root, GeminiClient};
use super::mock::MockLlmClient;
use super::openai::OpenAiCompatClient;
use super::registry::{provider_descriptor, ProviderDescriptor, ProviderImplementation, PROVIDERS};
use super::responses::OpenAiResponsesClient;
use super::LlmClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDiagnostics {
    pub provider: String,
    pub model: String,
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
    if sensitive_values.iter().any(|sensitive| {
        [sensitive.as_str(), sensitive.trim()]
            .into_iter()
            .any(|candidate| {
                !candidate.is_empty() && contains_at_any_percent_decode_stage(value, candidate)
            })
    }) {
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
enum ResolvedAdapter {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedProvider {
    diagnostics: ProviderDiagnostics,
    adapter: ResolvedAdapter,
}

pub fn create_client(
    llm: &LlmConfig,
    provider_override: Option<&str>,
) -> Result<Arc<dyn LlmClient>, String> {
    create_client_with_sensitive_values(
        llm,
        provider_override,
        &llm_configured_sensitive_values(llm),
    )
}

pub(crate) fn create_client_with_sensitive_values(
    llm: &LlmConfig,
    provider_override: Option<&str>,
    configured_sensitive_values: &[String],
) -> Result<Arc<dyn LlmClient>, String> {
    let resolved = resolve_provider(llm, provider_override)?;
    let provider = resolved.diagnostics.provider.clone();
    let model = resolved.diagnostics.model.clone();

    if resolved.adapter == ResolvedAdapter::Mock {
        return Ok(Arc::new(MockLlmClient::new()));
    }
    let entry = llm.get_provider(Some(&provider));
    let credentials = require_credentials(provider_credentials(entry, &provider), &provider)?;
    let diagnostic_secrets =
        provider_error_sensitive_values(configured_sensitive_values.iter().cloned());

    let client: Arc<dyn LlmClient> = match resolved.adapter {
        ResolvedAdapter::Mock => unreachable!("mock returned before credential resolution"),
        ResolvedAdapter::Anthropic { endpoint } => {
            Arc::new(AnthropicClient::configured_with_diagnostic_secrets(
                model,
                credentials,
                diagnostic_secrets,
                endpoint,
            )?)
        }
        ResolvedAdapter::Google { api_root } => {
            Arc::new(GeminiClient::configured_with_diagnostic_secrets(
                model,
                credentials,
                diagnostic_secrets,
                api_root,
            )?)
        }
        ResolvedAdapter::OpenAiCompatible {
            api_mode: LlmApiMode::ChatCompletions,
            endpoint,
            reasoning_effort,
        } => Arc::new(OpenAiCompatClient::configured_with_diagnostic_secrets(
            provider,
            model,
            credentials,
            diagnostic_secrets,
            endpoint,
            reasoning_effort,
        )),
        ResolvedAdapter::OpenAiCompatible {
            api_mode: LlmApiMode::Responses,
            endpoint,
            reasoning_effort,
        } => Arc::new(OpenAiResponsesClient::configured_with_diagnostic_secrets(
            provider,
            model,
            credentials,
            diagnostic_secrets,
            endpoint,
            reasoning_effort,
        )),
    };
    Ok(client)
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
    if !descriptor.is_openai_compatible()
        && entry.is_some_and(|entry| entry.api.is_some() || entry.reasoning_effort.is_some())
    {
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

    let (adapter, api_mode, endpoint_path, warning) = match descriptor.implementation {
        ProviderImplementation::Mock => (
            ResolvedAdapter::Mock,
            "local".to_string(),
            "local".to_string(),
            None,
        ),
        ProviderImplementation::Anthropic => {
            let configured = resolved_native_base_url(descriptor, entry)?;
            let endpoint = normalize_anthropic_endpoint(configured)?;
            let path = Url::parse(&endpoint)
                .map_err(|_| "resolved Anthropic endpoint is invalid".to_string())?
                .path()
                .to_string();
            (
                ResolvedAdapter::Anthropic { endpoint },
                "provider_native".to_string(),
                path,
                None,
            )
        }
        ProviderImplementation::Gemini => {
            let configured = resolved_native_base_url(descriptor, entry)?;
            let api_root = normalize_gemini_api_root(configured)?;
            let path = format!(
                "{}/models/{model}:generateContent",
                Url::parse(&api_root)
                    .map_err(|_| "resolved Gemini API root is invalid".to_string())?
                    .path()
                    .trim_end_matches('/')
            );
            (
                ResolvedAdapter::Google { api_root },
                "provider_native".to_string(),
                path,
                None,
            )
        }
        ProviderImplementation::OpenAi
        | ProviderImplementation::Xai
        | ProviderImplementation::OpenRouter
        | ProviderImplementation::Meta => {
            let api_mode = entry.map_or(LlmApiMode::default(), ProviderEntry::resolved_api_mode);
            let endpoint = resolve_openai_compatible_endpoint(&provider, entry, api_mode)?;
            let warning = chat_compatibility_warning(api_mode, endpoint.canonical_openai);
            (
                ResolvedAdapter::OpenAiCompatible {
                    api_mode,
                    endpoint: endpoint.url,
                    reasoning_effort: entry.and_then(|entry| entry.reasoning_effort),
                },
                api_mode.to_string(),
                endpoint.path,
                warning,
            )
        }
    };

    Ok(ResolvedProvider {
        diagnostics: ProviderDiagnostics {
            provider,
            model,
            api_mode,
            endpoint_path,
            reasoning_effort: reasoning_label,
            warning,
        },
        adapter,
    })
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

fn contains_at_any_percent_decode_stage(value: &str, needle: &str) -> bool {
    let Some(needle_stages) = percent_decode_stages(needle) else {
        return true;
    };
    let Some(value_stages) = percent_decode_stages(value) else {
        return true;
    };
    value_stages.iter().any(|value_stage| {
        needle_stages
            .iter()
            .any(|needle_stage| !needle_stage.is_empty() && value_stage.contains(needle_stage))
    })
}

fn percent_decode_stages(value: &str) -> Option<Vec<String>> {
    const MAX_PERCENT_DECODE_PASSES: usize = 8;

    let mut stages = vec![value.to_string()];
    let mut passes = 0;
    loop {
        let next = percent_decode_once(stages.last().expect("decode stage"));
        if stages.last().is_some_and(|current| current == &next) {
            return Some(stages);
        }
        if passes == MAX_PERCENT_DECODE_PASSES {
            return None;
        }
        stages.push(next);
        passes += 1;
    }
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
    use serde_json::{json, Value};
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
    fn factory_honors_override_and_rejects_unknown_provider() {
        let config = LlmConfig::default();
        create_client(&config, Some("mock")).expect("mock override");
        let error = create_client(&config, Some("unknown"))
            .err()
            .expect("unknown provider");
        assert!(error.contains("unsupported LLM provider"));
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
        assert_eq!(legacy.api_mode, "chat_completions");
        assert_eq!(legacy.endpoint_path, "/v1/chat/completions");

        let responses = compatible_config(
            "openai",
            Some(LlmApiMode::Responses),
            Some("https://api.openai.com/v1/responses/"),
            Some(ReasoningEffort::High),
        );
        create_client(&responses, None).expect("Responses client construction");
        let responses = provider_diagnostics(&responses, None).expect("Responses diagnostics");
        assert_eq!(responses.api_mode, "responses");
        assert_eq!(responses.endpoint_path, "/v1/responses");
        assert_eq!(responses.reasoning_effort, "high");
        assert_eq!(responses.warning, None);
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
        let messages = [Value::Object(serde_json::Map::from_iter([
            ("role".to_string(), json!("user")),
            ("content".to_string(), json!("inspect")),
        ]))];
        let error = client
            .complete(LlmRequest::new(&messages, None, 0.0))
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
