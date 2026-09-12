use super::config::LiveSettings;
use super::{CatalogModel, CatalogPricing, CatalogSnapshot};
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde_json::Value;
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

pub(super) const MAX_CATALOG_PAGES: usize = 100;
pub(super) const MAX_CATALOG_MODELS: usize = 10_000;
const MAX_CATALOG_PAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOG_URL_BYTES: usize = 4 * 1024;
const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct CatalogBounds {
    pages: usize,
    models: usize,
}

const PRODUCTION_CATALOG_BOUNDS: CatalogBounds = CatalogBounds {
    pages: MAX_CATALOG_PAGES,
    models: MAX_CATALOG_MODELS,
};

pub(super) struct CatalogCapture {
    pub(super) snapshot: CatalogSnapshot,
    pub(super) protected_client: Client,
}

pub(super) async fn fetch_catalog(
    provider: &str,
    settings: &LiveSettings,
) -> Result<CatalogCapture, String> {
    tokio::time::timeout(
        settings.limits.max_provider_duration,
        fetch_catalog_with_deadline(provider, settings),
    )
    .await
    .map_err(|_| format!("{provider} catalog exceeded the provider deadline"))?
}

async fn fetch_catalog_with_deadline(
    provider: &str,
    settings: &LiveSettings,
) -> Result<CatalogCapture, String> {
    let credential = settings.credential(provider)?;
    let endpoint = catalog_endpoint(provider, settings)?;
    let client = protected_catalog_client(&endpoint, &SystemCatalogResolver).await?;
    let snapshot = match provider {
        "anthropic" => {
            fetch_anthropic_from_endpoint(&client, &credential, endpoint, PRODUCTION_CATALOG_BOUNDS)
                .await
        }
        "google" => {
            fetch_gemini_from_endpoint(&client, &credential, endpoint, PRODUCTION_CATALOG_BOUNDS)
                .await
        }
        "grok" => {
            fetch_single_page(
                provider,
                &client,
                authenticated_bearer(&client, endpoint, &credential),
                parse_xai_page,
            )
            .await
        }
        "openrouter" => {
            fetch_single_page(
                provider,
                &client,
                authenticated_bearer(&client, endpoint, &credential),
                parse_openrouter_page,
            )
            .await
        }
        "openai" => {
            fetch_single_page(
                provider,
                &client,
                authenticated_bearer(&client, endpoint, &credential),
                |value| parse_openai_compatible_page(value, "openai"),
            )
            .await
        }
        "meta" => {
            fetch_single_page(
                provider,
                &client,
                authenticated_bearer(&client, endpoint, &credential),
                |value| parse_openai_compatible_page(value, "meta"),
            )
            .await
        }
        _ => Err("unsupported live catalog provider".to_string()),
    }?;
    Ok(CatalogCapture {
        snapshot,
        protected_client: client,
    })
}

fn catalog_endpoint(provider: &str, settings: &LiveSettings) -> Result<Url, String> {
    match provider {
        "anthropic" => catalog_url("https://api.anthropic.com", "v1/models"),
        "google" => catalog_url("https://generativelanguage.googleapis.com/v1beta", "models"),
        "grok" => catalog_url("https://api.x.ai/v1", "language-models"),
        "openrouter" => openrouter_catalog_url(),
        "openai" => catalog_url("https://api.openai.com/v1", "models"),
        "meta" => settings
            .meta_base_url
            .as_deref()
            .ok_or_else(|| {
                "Meta catalog is blocked_configuration because its base URL is missing".to_string()
            })
            .and_then(|root| catalog_url(root, "models")),
        _ => Err("unsupported live catalog provider".to_string()),
    }
}

#[async_trait::async_trait]
trait CatalogResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String>;
}

struct SystemCatalogResolver;

#[async_trait::async_trait]
impl CatalogResolver for SystemCatalogResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let host = host.to_string();
        tokio::task::spawn_blocking(move || {
            (host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect())
                .map_err(|_| "live catalog DNS resolution failed".to_string())
        })
        .await
        .map_err(|_| "live catalog DNS resolver task failed".to_string())?
    }
}

async fn protected_catalog_client<R: CatalogResolver + ?Sized>(
    endpoint: &Url,
    resolver: &R,
) -> Result<Client, String> {
    let raw_host = endpoint
        .host_str()
        .ok_or_else(|| "live catalog endpoint has no host".to_string())?;
    if raw_host.ends_with('.') {
        return Err("live catalog endpoint host must use canonical DNS spelling".to_string());
    }
    let host = raw_host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return Err("live catalog destination is not publicly routable".to_string());
    }
    let port = endpoint
        .port_or_known_default()
        .ok_or_else(|| "live catalog endpoint has no destination port".to_string())?;
    let mut addresses = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, port)]
    } else {
        resolver.resolve(&host, port).await?
    };
    if addresses.is_empty() {
        return Err("live catalog DNS resolution returned no addresses".to_string());
    }
    for address in &mut addresses {
        address.set_port(port);
        if catalog_ip_is_non_public(address.ip()) {
            return Err("live catalog destination is not publicly routable".to_string());
        }
    }
    addresses.sort_unstable();
    addresses.dedup();
    Client::builder()
        .connect_timeout(CATALOG_REQUEST_TIMEOUT)
        .timeout(CATALOG_REQUEST_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(&host, &addresses)
        .build()
        .map_err(|_| "failed to construct bounded catalog HTTP client".to_string())
}

fn catalog_ip_is_non_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => catalog_ipv4_is_non_public(address),
        IpAddr::V6(address) => catalog_ipv6_is_non_public(address),
    }
}

fn catalog_ipv4_is_non_public(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    first == 0
        || first == 10
        || first == 127
        || (first == 100 && (64..=127).contains(&second))
        || (first == 169 && second == 254)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 0 && third == 0)
        || (first == 192 && second == 0 && third == 2)
        || (first == 192 && second == 88 && third == 99)
        || (first == 192 && second == 168)
        || (first == 198 && matches!(second, 18 | 19))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        || first >= 224
}

fn catalog_ipv6_is_non_public(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    // Fail closed unless IANA has allocated the address from the global-unicast
    // 2000::/3 block. This positive gate excludes mapped/compatible IPv4, NAT64,
    // ULA, link/site-local, multicast, and all other reserved top-level space.
    let global_unicast_allocation = octets[0] & 0xe0 == 0x20;
    // IANA special-purpose suballocations within 2000::/3 are not suitable direct
    // catalog destinations even where an individual subrange has limited global use.
    let ietf_protocol_assignments = octets[..2] == [0x20, 0x01] && octets[2] <= 0x01;
    let documentation_2001 = octets[..4] == [0x20, 0x01, 0x0d, 0xb8];
    let transition_6to4 = octets[..2] == [0x20, 0x02];
    let documentation_3fff = octets[..2] == [0x3f, 0xff] && octets[2] & 0xf0 == 0x00;

    !global_unicast_allocation
        || ietf_protocol_assignments
        || documentation_2001
        || transition_6to4
        || documentation_3fff
}

fn authenticated_bearer(client: &Client, url: Url, credential: &str) -> RequestBuilder {
    client.get(url).bearer_auth(credential)
}

async fn fetch_single_page(
    provider: &str,
    _client: &Client,
    request: RequestBuilder,
    parser: impl FnOnce(&Value) -> Result<Vec<CatalogModel>, String>,
) -> Result<CatalogSnapshot, String> {
    let value = send_catalog_request(provider, request).await?;
    CatalogSnapshot::new(provider, 1, parser(&value)?)
}

async fn fetch_anthropic_from_endpoint(
    client: &Client,
    credential: &str,
    endpoint: Url,
    bounds: CatalogBounds,
) -> Result<CatalogSnapshot, String> {
    let mut after_id: Option<String> = None;
    let mut seen_cursors = BTreeSet::new();
    let mut models = Vec::new();
    let mut pages = 0;
    loop {
        pages += 1;
        if pages > bounds.pages {
            return Err("Anthropic catalog exceeds the page limit".to_string());
        }
        let mut url = endpoint.clone();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("limit", "1000");
            if let Some(after_id) = after_id.as_deref() {
                query.append_pair("after_id", after_id);
            }
        }
        let value = send_catalog_request(
            "anthropic",
            client
                .get(url)
                .header("x-api-key", credential)
                .header("anthropic-version", "2023-06-01"),
        )
        .await?;
        let page = parse_anthropic_page(&value)?;
        models.extend(page.models);
        if models.len() > bounds.models {
            return Err("Anthropic catalog exceeds the model limit".to_string());
        }
        if !page.has_more {
            break;
        }
        let next = page
            .last_id
            .filter(|cursor| !cursor.trim().is_empty())
            .ok_or_else(|| "Anthropic catalog has_more without last_id".to_string())?;
        if !seen_cursors.insert(next.clone()) || after_id.as_ref() == Some(&next) {
            return Err("Anthropic catalog cursor did not progress".to_string());
        }
        after_id = Some(next);
    }
    CatalogSnapshot::new("anthropic", pages, models)
}

async fn fetch_gemini_from_endpoint(
    client: &Client,
    credential: &str,
    endpoint: Url,
    bounds: CatalogBounds,
) -> Result<CatalogSnapshot, String> {
    let mut page_token: Option<String> = None;
    let mut seen_tokens = BTreeSet::new();
    let mut models = Vec::new();
    let mut pages = 0;
    loop {
        pages += 1;
        if pages > bounds.pages {
            return Err("Gemini catalog exceeds the page limit".to_string());
        }
        let mut url = endpoint.clone();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("pageSize", "1000");
            if let Some(token) = page_token.as_deref() {
                query.append_pair("pageToken", token);
            }
        }
        let value = send_catalog_request(
            "google",
            client.get(url).header("x-goog-api-key", credential),
        )
        .await?;
        let page = parse_gemini_page(&value)?;
        models.extend(page.models);
        if models.len() > bounds.models {
            return Err("Gemini catalog exceeds the model limit".to_string());
        }
        let Some(next) = page.next_page_token else {
            break;
        };
        if next.trim().is_empty()
            || !seen_tokens.insert(next.clone())
            || page_token.as_ref() == Some(&next)
        {
            return Err("Gemini catalog page token did not progress".to_string());
        }
        page_token = Some(next);
    }
    CatalogSnapshot::new("google", pages, models)
}

async fn send_catalog_request(provider: &str, request: RequestBuilder) -> Result<Value, String> {
    let response = request
        .send()
        .await
        .map_err(|_| format!("{provider} catalog request failed before a valid response"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(safe_catalog_status(provider, status));
    }
    read_bounded_catalog_json(response, provider).await
}

fn safe_catalog_status(provider: &str, status: StatusCode) -> String {
    let class = match status.as_u16() {
        401 | 403 => "blocked_auth",
        402 => "blocked_billing",
        429 => "blocked_rate_limit",
        451 => "blocked_region",
        500..=599 => "provider_unavailable",
        _ => "catalog_rejected",
    };
    format!("{provider} catalog {class} with HTTP {}", status.as_u16())
}

async fn read_bounded_catalog_json(response: Response, provider: &str) -> Result<Value, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_PAGE_BYTES as u64)
    {
        return Err(format!("{provider} catalog page exceeds its byte limit"));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| format!("{provider} catalog body read failed"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_CATALOG_PAGE_BYTES {
            return Err(format!("{provider} catalog page exceeds its byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| format!("{provider} catalog returned invalid JSON"))
}

pub(super) fn catalog_url(root: &str, suffix: &str) -> Result<Url, String> {
    let root = root.trim();
    if root.is_empty() || root.len() > MAX_CATALOG_URL_BYTES || root.contains('\0') {
        return Err("catalog root is invalid".to_string());
    }
    let parsed =
        Url::parse(root).map_err(|_| "catalog root must be an absolute URL".to_string())?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err("live catalog root must be an absolute HTTPS URL".to_string());
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(
            "live catalog root must not contain credentials, a query, or a fragment".to_string(),
        );
    }
    let mut normalized = parsed;
    let next_path = format!(
        "{}/{}",
        normalized.path().trim_end_matches('/'),
        suffix.trim_start_matches('/')
    );
    normalized.set_path(&next_path);
    Ok(normalized)
}

fn openrouter_catalog_url() -> Result<Url, String> {
    let mut url = catalog_url("https://openrouter.ai/api/v1", "models")?;
    url.query_pairs_mut()
        .append_pair("output_modalities", "all");
    Ok(url)
}

fn parse_openai_compatible_page(
    value: &Value,
    public_owner: &str,
) -> Result<Vec<CatalogModel>, String> {
    required_array(value, "data")?
        .iter()
        .map(|model| {
            let id = required_string(model, "id")?;
            let owner = optional_string(model, "owned_by")?;
            let public_identifier = owner
                .as_deref()
                .is_some_and(|owner| owner == public_owner || owner == "system");
            Ok(CatalogModel {
                id,
                generation_target: None,
                aliases: Vec::new(),
                supports_text_generation: None,
                supports_tools: None,
                supports_parallel_tools: None,
                input_modalities: BTreeSet::new(),
                output_modalities: BTreeSet::new(),
                supported_parameters: BTreeSet::new(),
                public_identifier,
                owner,
                pricing: None,
                expiration_date: None,
            })
        })
        .collect()
}

fn parse_xai_page(value: &Value) -> Result<Vec<CatalogModel>, String> {
    required_array(value, "models")?
        .iter()
        .map(|model| {
            let id = required_string(model, "id")?;
            let aliases = optional_string_array(model, "aliases")?;
            let input_modalities = optional_string_array(model, "input_modalities")?;
            let output_modalities = optional_string_array(model, "output_modalities")?;
            let supports_text = if input_modalities.is_empty() && output_modalities.is_empty() {
                None
            } else {
                Some(
                    input_modalities.iter().any(|value| value == "text")
                        && output_modalities.iter().any(|value| value == "text"),
                )
            };
            let owner = optional_string(model, "owned_by")?;
            Ok(CatalogModel {
                id,
                generation_target: None,
                aliases,
                supports_text_generation: supports_text,
                supports_tools: None,
                supports_parallel_tools: None,
                input_modalities: input_modalities.into_iter().collect(),
                output_modalities: output_modalities.into_iter().collect(),
                supported_parameters: BTreeSet::new(),
                public_identifier: owner.as_deref() == Some("xai"),
                owner,
                pricing: parse_xai_pricing(model)?,
                expiration_date: None,
            })
        })
        .collect()
}

fn parse_xai_pricing(model: &Value) -> Result<Option<CatalogPricing>, String> {
    let prompt = optional_number(model, "prompt_text_token_price")?;
    let completion = optional_number(model, "completion_text_token_price")?;
    if prompt.is_none() && completion.is_none() {
        return Ok(None);
    }
    // xAI reports integer USD cents per 100 million tokens: divide by 100 to
    // convert cents to dollars, then by 100 million for a per-token value.
    Ok(Some(CatalogPricing {
        prompt_per_token_usd: prompt.map(|value| value / 10_000_000_000.0),
        completion_per_token_usd: completion.map(|value| value / 10_000_000_000.0),
        request_usd: None,
    }))
}

fn parse_openrouter_page(value: &Value) -> Result<Vec<CatalogModel>, String> {
    required_array(value, "data")?
        .iter()
        .map(|model| {
            let id = required_string(model, "canonical_slug")
                .or_else(|_| required_string(model, "id"))?;
            let architecture = model.get("architecture").and_then(Value::as_object);
            let input = architecture
                .and_then(|value| value.get("input_modalities"))
                .map(string_array)
                .transpose()?
                .unwrap_or_default();
            let output = architecture
                .and_then(|value| value.get("output_modalities"))
                .map(string_array)
                .transpose()?
                .unwrap_or_default();
            let parameters = optional_string_array(model, "supported_parameters")?;
            let supports_text = if input.is_empty() && output.is_empty() {
                None
            } else {
                Some(
                    input.iter().any(|value| value == "text")
                        && output.iter().any(|value| value == "text"),
                )
            };
            Ok(CatalogModel {
                id,
                generation_target: None,
                aliases: Vec::new(),
                supports_text_generation: supports_text,
                supports_tools: Some(parameters.iter().any(|value| value == "tools")),
                supports_parallel_tools: Some(
                    parameters
                        .iter()
                        .any(|value| value == "parallel_tool_calls"),
                ),
                input_modalities: input.into_iter().collect(),
                output_modalities: output.into_iter().collect(),
                supported_parameters: parameters.into_iter().collect(),
                // OpenRouter exposes an account-visible slug but no ownership
                // discriminator. Keep catalog/accounting identities private; the
                // planner may elevate only an exact approved allowlist profile.
                public_identifier: false,
                owner: None,
                pricing: parse_openrouter_pricing(model)?,
                expiration_date: optional_string(model, "expiration_date")?,
            })
        })
        .collect()
}

fn parse_openrouter_pricing(model: &Value) -> Result<Option<CatalogPricing>, String> {
    let Some(pricing) = model.get("pricing") else {
        return Ok(None);
    };
    if pricing.is_null() {
        return Ok(None);
    }
    let pricing = pricing
        .as_object()
        .ok_or_else(|| "OpenRouter pricing must be an object".to_string())?;
    Ok(Some(CatalogPricing {
        prompt_per_token_usd: parse_decimal(pricing.get("prompt"))?,
        completion_per_token_usd: parse_decimal(pricing.get("completion"))?,
        request_usd: parse_decimal(pricing.get("request"))?,
    }))
}

#[derive(Debug)]
struct AnthropicPage {
    models: Vec<CatalogModel>,
    has_more: bool,
    last_id: Option<String>,
}

fn parse_anthropic_page(value: &Value) -> Result<AnthropicPage, String> {
    let models = required_array(value, "data")?
        .iter()
        .map(|model| {
            let capabilities = model.get("capabilities").and_then(Value::as_object);
            let tools = capabilities
                .and_then(|value| value.get("tools"))
                .and_then(|value| value.get("supported"))
                .and_then(Value::as_bool);
            Ok(CatalogModel {
                id: required_string(model, "id")?,
                generation_target: None,
                aliases: Vec::new(),
                supports_text_generation: Some(true),
                supports_tools: tools,
                supports_parallel_tools: None,
                input_modalities: BTreeSet::new(),
                output_modalities: BTreeSet::new(),
                supported_parameters: BTreeSet::new(),
                // The catalog response exposes no ownership evidence. Treat the
                // account-visible identifier as private until a provider-owned field
                // or reviewed public-ID source proves otherwise.
                public_identifier: false,
                owner: None,
                pricing: None,
                expiration_date: None,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let has_more = value
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or_else(|| "Anthropic catalog has_more must be a boolean".to_string())?;
    let last_id = optional_string(value, "last_id")?;
    Ok(AnthropicPage {
        models,
        has_more,
        last_id,
    })
}

#[derive(Debug)]
struct GeminiPage {
    models: Vec<CatalogModel>,
    next_page_token: Option<String>,
}

fn parse_gemini_page(value: &Value) -> Result<GeminiPage, String> {
    let models = required_array(value, "models")?
        .iter()
        .map(|model| {
            let resource_name = required_string(model, "name")?;
            resource_name
                .strip_prefix("models/")
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    "Gemini model name must use the models/ resource prefix".to_string()
                })?;
            let generation_target = required_string(model, "baseModelId")?;
            if generation_target.starts_with("models/") {
                return Err(
                    "Gemini baseModelId must be a generation target without the models/ prefix"
                        .to_string(),
                );
            }
            let actions = model
                .get("supportedGenerationMethods")
                .or_else(|| model.get("supported_actions"))
                .map(string_array)
                .transpose()?
                .unwrap_or_default();
            Ok(CatalogModel {
                id: resource_name,
                generation_target: Some(generation_target),
                aliases: Vec::new(),
                supports_text_generation: Some(
                    actions
                        .iter()
                        .any(|action| action.eq_ignore_ascii_case("generateContent")),
                ),
                supports_tools: None,
                supports_parallel_tools: None,
                input_modalities: BTreeSet::new(),
                output_modalities: BTreeSet::new(),
                supported_parameters: BTreeSet::new(),
                // `models` does not carry an ownership discriminator. Keep both the
                // resource name and generation target private in persisted reports.
                public_identifier: false,
                owner: None,
                pricing: None,
                expiration_date: None,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(GeminiPage {
        models,
        next_page_token: optional_string(value, "nextPageToken")?,
    })
}

fn required_array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>, String> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("catalog field '{field}' must be an array"))
}

fn required_string(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("catalog field '{field}' must be a non-empty string"))
}

fn optional_string(value: &Value, field: &str) -> Result<Option<String>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        _ => Err(format!(
            "catalog field '{field}' must be a non-empty string or null"
        )),
    }
}

fn optional_string_array(value: &Value, field: &str) -> Result<Vec<String>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => string_array(value),
    }
}

fn string_array(value: &Value) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| "catalog capability must be a string array".to_string())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(ToString::to_string)
                .ok_or_else(|| "catalog capability array contains a non-string".to_string())
        })
        .collect()
}

fn optional_number(value: &Value, field: &str) -> Result<Option<f64>, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(Some)
            .ok_or_else(|| format!("catalog field '{field}' must be non-negative")),
    }
}

fn parse_decimal(value: Option<&Value>) -> Result<Option<f64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(Some)
            .ok_or_else(|| "catalog pricing contains an invalid decimal".to_string()),
        Some(Value::Number(value)) => value
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(Some)
            .ok_or_else(|| "catalog pricing contains an invalid number".to_string()),
        _ => Err("catalog pricing must be a decimal string or number".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::thread::JoinHandle;

    const MAX_FIXTURE_REQUEST_BYTES: usize = 64 * 1024;

    #[derive(Clone)]
    struct FixtureResponse {
        status: &'static str,
        body: String,
        declared_length: Option<usize>,
    }

    impl FixtureResponse {
        fn json(value: Value) -> Self {
            Self {
                status: "200 OK",
                body: value.to_string(),
                declared_length: None,
            }
        }

        fn status(status: &'static str, body: impl Into<String>) -> Self {
            Self {
                status,
                body: body.into(),
                declared_length: None,
            }
        }
    }

    fn read_fixture_request(stream: &mut impl Read) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        loop {
            let read = stream.read(&mut buffer).expect("catalog fixture request");
            assert!(read > 0, "catalog fixture connection closed before headers");
            request.extend_from_slice(&buffer[..read]);
            assert!(request.len() <= MAX_FIXTURE_REQUEST_BYTES);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                return String::from_utf8(request).expect("catalog fixture request UTF-8");
            }
        }
    }

    fn serve(responses: Vec<FixtureResponse>) -> (Url, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("catalog fixture listener");
        let address = listener.local_addr().expect("catalog fixture address");
        let (request_tx, request_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("catalog fixture connection");
                let request = read_fixture_request(&mut stream);
                // Some negative-path tests intentionally discard captured requests.
                // The fixture must still finish serving its bounded response instead
                // of turning a dropped observation channel into a server failure.
                let _ = request_tx.send(request);
                let declared_length = response.declared_length.unwrap_or(response.body.len());
                let encoded = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n{}",
                    response.status, response.body
                );
                stream
                    .write_all(encoded.as_bytes())
                    .expect("catalog fixture response");
            }
        });
        (
            Url::parse(&format!("http://{address}/models")).expect("catalog fixture URL"),
            request_rx,
            handle,
        )
    }

    fn test_client(timeout: Duration) -> Client {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .expect("catalog fixture client")
    }

    struct FixedResolver {
        addresses: Vec<SocketAddr>,
    }

    #[async_trait::async_trait]
    impl CatalogResolver for FixedResolver {
        async fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<SocketAddr>, String> {
            Ok(self.addresses.clone())
        }
    }

    #[test]
    fn parses_openai_and_marks_customer_owned_ids_private() {
        let models = parse_openai_compatible_page(
            &json!({"data": [
                {"id": "gpt-public", "owned_by": "openai"},
                {"id": "ft:private", "owned_by": "customer"}
            ]}),
            "openai",
        )
        .unwrap();
        assert!(models[0].public_identifier);
        assert!(!models[1].public_identifier);
        assert_eq!(models[0].supports_text_generation, None);
    }

    #[test]
    fn parses_anthropic_pagination_metadata() {
        let page = parse_anthropic_page(&json!({
            "data": [{"id": "claude-test", "capabilities": {}}],
            "has_more": true,
            "last_id": "claude-test"
        }))
        .unwrap();
        assert!(page.has_more);
        assert_eq!(page.last_id.as_deref(), Some("claude-test"));
        assert_eq!(page.models.len(), 1);
        assert!(!page.models[0].public_identifier);
        assert_eq!(page.models[0].owner, None);
        assert!(parse_anthropic_page(&json!({"data": [], "has_more": "yes"})).is_err());
    }

    #[test]
    fn parses_gemini_capabilities_and_exact_resource_prefix() {
        let page = parse_gemini_page(&json!({
            "models": [
                {"name": "models/gemini-chat", "baseModelId": "gemini-chat", "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/embed", "baseModelId": "embed", "supportedGenerationMethods": ["embedContent"]}
            ],
            "nextPageToken": "next"
        }))
        .unwrap();
        assert_eq!(page.models[0].id, "models/gemini-chat");
        assert_eq!(page.models[0].generation_target(), "gemini-chat");
        assert_eq!(page.models[0].supports_text_generation, Some(true));
        assert!(!page.models[0].public_identifier);
        assert_eq!(page.models[0].owner, None);
        assert_eq!(page.models[1].supports_text_generation, Some(false));
        assert!(parse_gemini_page(&json!({"models": [{"name": "gemini-chat"}]})).is_err());
        assert!(parse_gemini_page(&json!({
            "models": [{"name": "models/gemini-chat", "supportedGenerationMethods": ["generateContent"]}]
        }))
        .unwrap_err()
        .contains("baseModelId"));
        assert!(
            parse_gemini_page(&json!({
                "models": [{"name": "models/gemini-chat", "baseModelId": "models/gemini-chat", "supportedGenerationMethods": ["generateContent"]}]
            }))
            .unwrap_err()
            .contains("without the models/ prefix")
        );
    }

    #[test]
    fn parses_xai_canonical_ids_aliases_and_modalities() {
        let models = parse_xai_page(&json!({"models": [{
            "id": "grok-test",
            "aliases": ["grok-latest"],
            "owned_by": "xai",
            "input_modalities": ["text"],
            "output_modalities": ["text"],
            "prompt_text_token_price": 20000,
            "completion_text_token_price": 80000
        }]}))
        .unwrap();
        assert_eq!(models[0].aliases, ["grok-latest"]);
        assert_eq!(models[0].supports_text_generation, Some(true));
        assert_eq!(
            models[0].pricing.as_ref().unwrap().prompt_per_token_usd,
            Some(0.000002)
        );
        assert_eq!(
            models[0].pricing.as_ref().unwrap().completion_per_token_usd,
            Some(0.000008)
        );
    }

    #[test]
    fn parses_openrouter_capabilities_pricing_and_canonical_slug() {
        let models = parse_openrouter_page(&json!({"data": [{
            "id": "alias/value",
            "canonical_slug": "owner/model",
            "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
            "supported_parameters": ["tools", "parallel_tool_calls"],
            "pricing": {"prompt": "0.000001", "completion": "0.000002", "request": "0"}
        }]}))
        .unwrap();
        assert_eq!(models[0].id, "owner/model");
        assert_eq!(models[0].supports_tools, Some(true));
        assert_eq!(models[0].supports_parallel_tools, Some(true));
        assert_eq!(
            models[0].input_modalities,
            BTreeSet::from(["text".to_string()])
        );
        assert_eq!(
            models[0].output_modalities,
            BTreeSet::from(["text".to_string()])
        );
        assert!(models[0].supported_parameters.contains("tools"));
        assert!(models[0]
            .supported_parameters
            .contains("parallel_tool_calls"));
        assert_eq!(
            models[0].pricing.as_ref().unwrap().completion_per_token_usd,
            Some(0.000002)
        );
    }

    #[test]
    fn catalog_roots_are_https_and_credential_free() {
        assert_eq!(
            catalog_url("https://api.example/v1", "models")
                .unwrap()
                .as_str(),
            "https://api.example/v1/models"
        );
        assert!(catalog_url("http://api.example/v1", "models").is_err());
        assert!(catalog_url("https://user:secret@api.example/v1", "models").is_err());
        assert!(catalog_url("https://api.example/v1?key=secret", "models").is_err());
        assert_eq!(
            openrouter_catalog_url().unwrap().as_str(),
            "https://openrouter.ai/api/v1/models?output_modalities=all"
        );
    }

    #[tokio::test]
    async fn protected_catalog_origins_reject_non_public_and_mixed_destinations() {
        let unused = FixedResolver {
            addresses: Vec::new(),
        };
        for raw in [
            "https://127.0.0.1/v1/models",
            "https://10.0.0.1/v1/models",
            "https://169.254.169.254/v1/models",
            "https://192.168.1.10/v1/models",
            "https://[::1]/v1/models",
            "https://[fc00::1]/v1/models",
            "https://[fe80::1]/v1/models",
            "https://[4000::1]/v1/models",
            "https://[2001:2::1]/v1/models",
            "https://[2001:db8::1]/v1/models",
            "https://[3fff::1]/v1/models",
            "https://[2001::1]/v1/models",
            "https://[2001:10::1]/v1/models",
            "https://[2001:20::1]/v1/models",
            "https://[2002:c000:201::1]/v1/models",
            "https://[64:ff9b::7f00:1]/v1/models",
            "https://[64:ff9b:1::c0a8:1]/v1/models",
            "https://[::ffff:c0a8:101]/v1/models",
            "https://[::c0a8:101]/v1/models",
        ] {
            let endpoint = Url::parse(raw).expect("syntactically valid protected endpoint");
            let error = protected_catalog_client(&endpoint, &unused)
                .await
                .expect_err("non-public literal destination");
            assert_eq!(
                error, "live catalog destination is not publicly routable",
                "{raw}"
            );
            assert!(!error.contains(endpoint.host_str().unwrap_or_default()));
        }

        let endpoint =
            Url::parse("https://reviewed.example/v1/models").expect("reviewed catalog endpoint");
        for addresses in [
            vec!["10.0.0.2:443".parse().unwrap()],
            vec![
                "93.184.216.34:443".parse().unwrap(),
                "192.168.1.2:443".parse().unwrap(),
            ],
        ] {
            let error = protected_catalog_client(&endpoint, &FixedResolver { addresses })
                .await
                .expect_err("private or mixed DNS destination");
            assert_eq!(error, "live catalog destination is not publicly routable");
            assert!(!error.contains("reviewed.example"));
            assert!(!error.contains("192.168"));
        }

        protected_catalog_client(
            &endpoint,
            &FixedResolver {
                addresses: vec!["93.184.216.34:443".parse().unwrap()],
            },
        )
        .await
        .expect("one exact reviewed public destination is pinned");

        protected_catalog_client(
            &endpoint,
            &FixedResolver {
                addresses: vec!["[2606:2800:220:1:248:1893:25c8:1946]:443".parse().unwrap()],
            },
        )
        .await
        .expect("one exact reviewed global-unicast IPv6 destination is pinned");
    }

    #[test]
    fn safe_status_never_contains_remote_body_text() {
        assert_eq!(
            safe_catalog_status("openai", StatusCode::UNAUTHORIZED),
            "openai catalog blocked_auth with HTTP 401"
        );
        assert_eq!(
            safe_catalog_status("google", StatusCode::TOO_MANY_REQUESTS),
            "google catalog blocked_rate_limit with HTTP 429"
        );
    }

    #[test]
    fn every_provider_catalog_parser_rejects_malformed_top_level_evidence() {
        let malformed = json!({"unexpected": []});
        for (provider, result) in [
            (
                "openai",
                parse_openai_compatible_page(&malformed, "openai").map(|_| ()),
            ),
            (
                "meta",
                parse_openai_compatible_page(&malformed, "meta").map(|_| ()),
            ),
            ("grok", parse_xai_page(&malformed).map(|_| ())),
            ("openrouter", parse_openrouter_page(&malformed).map(|_| ())),
            ("anthropic", parse_anthropic_page(&malformed).map(|_| ())),
            ("google", parse_gemini_page(&malformed).map(|_| ())),
        ] {
            let error = result.expect_err("malformed provider catalog");
            assert!(
                error.contains("catalog field") || error.contains("has_more"),
                "{provider}: {error}"
            );
            assert!(!error.contains("unexpected"), "{provider}: {error}");
        }
    }

    #[tokio::test]
    async fn anthropic_and_gemini_catalogs_paginate_with_exact_cursor_headers() {
        let (endpoint, requests, server) = serve(vec![
            FixtureResponse::json(json!({
                "data": [{"id": "claude-a", "capabilities": {"tools": {"supported": true}}}],
                "has_more": true,
                "last_id": "cursor-a"
            })),
            FixtureResponse::json(json!({
                "data": [{"id": "claude-b", "capabilities": {}}],
                "has_more": false,
                "last_id": "cursor-b"
            })),
        ]);
        let snapshot = fetch_anthropic_from_endpoint(
            &test_client(Duration::from_secs(2)),
            "fixture-key",
            endpoint,
            CatalogBounds {
                pages: 3,
                models: 4,
            },
        )
        .await
        .expect("paginated Anthropic catalog");
        assert_eq!(snapshot.page_count, 2);
        assert_eq!(snapshot.models.len(), 2);
        let first = requests.recv().expect("first Anthropic request");
        let second = requests.recv().expect("second Anthropic request");
        server.join().expect("Anthropic fixture server");
        assert!(first.starts_with("GET /models?limit=1000 HTTP/1.1"));
        assert!(first
            .to_ascii_lowercase()
            .contains("x-api-key: fixture-key"));
        assert!(first
            .to_ascii_lowercase()
            .contains("anthropic-version: 2023-06-01"));
        assert!(second.starts_with("GET /models?limit=1000&after_id=cursor-a HTTP/1.1"));

        let (endpoint, requests, server) = serve(vec![
            FixtureResponse::json(json!({
                "models": [{"name": "models/gemini-a", "baseModelId": "gemini-a", "supportedGenerationMethods": ["generateContent"]}],
                "nextPageToken": "next-a"
            })),
            FixtureResponse::json(json!({
                "models": [{"name": "models/gemini-b", "baseModelId": "gemini-b", "supportedGenerationMethods": ["embedContent"]}]
            })),
        ]);
        let snapshot = fetch_gemini_from_endpoint(
            &test_client(Duration::from_secs(2)),
            "gemini-key",
            endpoint,
            CatalogBounds {
                pages: 3,
                models: 4,
            },
        )
        .await
        .expect("paginated Gemini catalog");
        assert_eq!(snapshot.page_count, 2);
        assert_eq!(snapshot.models.len(), 2);
        let first = requests.recv().expect("first Gemini request");
        let second = requests.recv().expect("second Gemini request");
        server.join().expect("Gemini fixture server");
        assert!(first.starts_with("GET /models?pageSize=1000 HTTP/1.1"));
        assert!(first
            .to_ascii_lowercase()
            .contains("x-goog-api-key: gemini-key"));
        assert!(second.starts_with("GET /models?pageSize=1000&pageToken=next-a HTTP/1.1"));
    }

    #[tokio::test]
    async fn every_single_page_catalog_client_executes_a_success_fixture() {
        let responses = vec![
            FixtureResponse::json(json!({"data": [{"id": "openai-model", "owned_by": "openai"}]})),
            FixtureResponse::json(json!({
                "models": [{"id": "grok-model", "owned_by": "xai", "input_modalities": ["text"], "output_modalities": ["text"]}]
            })),
            FixtureResponse::json(json!({
                "data": [{"id": "owner/model", "canonical_slug": "owner/model", "supported_parameters": []}]
            })),
            FixtureResponse::json(json!({"data": [{"id": "meta-model", "owned_by": "meta"}]})),
        ];
        let (endpoint, requests, server) = serve(responses);
        let client = test_client(Duration::from_secs(2));
        let openai = fetch_single_page(
            "openai",
            &client,
            authenticated_bearer(&client, endpoint.clone(), "fixture-key"),
            |value| parse_openai_compatible_page(value, "openai"),
        )
        .await
        .expect("OpenAI catalog fixture");
        let grok = fetch_single_page(
            "grok",
            &client,
            authenticated_bearer(&client, endpoint.clone(), "fixture-key"),
            parse_xai_page,
        )
        .await
        .expect("xAI catalog fixture");
        let openrouter = fetch_single_page(
            "openrouter",
            &client,
            authenticated_bearer(&client, endpoint.clone(), "fixture-key"),
            parse_openrouter_page,
        )
        .await
        .expect("OpenRouter catalog fixture");
        let meta = fetch_single_page(
            "meta",
            &client,
            authenticated_bearer(&client, endpoint, "fixture-key"),
            |value| parse_openai_compatible_page(value, "meta"),
        )
        .await
        .expect("Meta catalog fixture");
        assert_eq!(openai.models[0].id, "openai-model");
        assert_eq!(grok.models[0].id, "grok-model");
        assert_eq!(openrouter.models[0].id, "owner/model");
        assert_eq!(meta.models[0].id, "meta-model");
        for _ in 0..4 {
            let request = requests.recv().expect("single-page catalog request");
            assert!(request.starts_with("GET /models HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-key"));
        }
        server.join().expect("single-page fixture server");
    }

    #[tokio::test]
    async fn pagination_loops_page_and_item_bounds_fail_closed() {
        let (endpoint, _, server) = serve(vec![
            FixtureResponse::json(json!({
                "data": [{"id": "claude-a"}],
                "has_more": true,
                "last_id": "same"
            })),
            FixtureResponse::json(json!({
                "data": [{"id": "claude-b"}],
                "has_more": true,
                "last_id": "same"
            })),
        ]);
        let error = fetch_anthropic_from_endpoint(
            &test_client(Duration::from_secs(2)),
            "key",
            endpoint,
            CatalogBounds {
                pages: 3,
                models: 4,
            },
        )
        .await
        .expect_err("repeated Anthropic cursor");
        server.join().expect("cursor fixture server");
        assert!(error.contains("cursor did not progress"), "{error}");

        let (endpoint, _, server) = serve(vec![
            FixtureResponse::json(json!({
                "models": [{"name": "models/gemini-a", "baseModelId": "gemini-a"}],
                "nextPageToken": "same"
            })),
            FixtureResponse::json(json!({
                "models": [{"name": "models/gemini-b", "baseModelId": "gemini-b"}],
                "nextPageToken": "same"
            })),
        ]);
        let error = fetch_gemini_from_endpoint(
            &test_client(Duration::from_secs(2)),
            "key",
            endpoint,
            CatalogBounds {
                pages: 3,
                models: 4,
            },
        )
        .await
        .expect_err("repeated Gemini page token");
        server.join().expect("page-token fixture server");
        assert!(error.contains("page token did not progress"), "{error}");

        let (endpoint, _, server) = serve(vec![FixtureResponse::json(json!({
            "models": [{"name": "models/gemini-a", "baseModelId": "gemini-a"}],
            "nextPageToken": "more"
        }))]);
        let error = fetch_gemini_from_endpoint(
            &test_client(Duration::from_secs(2)),
            "key",
            endpoint,
            CatalogBounds {
                pages: 1,
                models: 4,
            },
        )
        .await
        .expect_err("Gemini page limit");
        server.join().expect("page-bound fixture server");
        assert!(error.contains("page limit"), "{error}");

        let (endpoint, _, server) = serve(vec![FixtureResponse::json(json!({
            "data": [{"id": "claude-a"}, {"id": "claude-b"}],
            "has_more": false,
            "last_id": null
        }))]);
        let error = fetch_anthropic_from_endpoint(
            &test_client(Duration::from_secs(2)),
            "key",
            endpoint,
            CatalogBounds {
                pages: 1,
                models: 1,
            },
        )
        .await
        .expect_err("Anthropic item limit");
        server.join().expect("item-bound fixture server");
        assert!(error.contains("model limit"), "{error}");
    }

    #[tokio::test]
    async fn single_page_catalog_rejects_remote_errors_malformed_bytes_and_conflicts() {
        let remote_sentinel = "remote-catalog-secret";
        let (endpoint, _, server) = serve(vec![FixtureResponse::status(
            "401 Unauthorized",
            remote_sentinel,
        )]);
        let error = fetch_single_page(
            "openai",
            &test_client(Duration::from_secs(2)),
            test_client(Duration::from_secs(2)).get(endpoint),
            |value| parse_openai_compatible_page(value, "openai"),
        )
        .await
        .expect_err("catalog HTTP error");
        server.join().expect("HTTP error fixture server");
        assert_eq!(error, "openai catalog blocked_auth with HTTP 401");
        assert!(!error.contains(remote_sentinel));

        let (endpoint, _, server) = serve(vec![FixtureResponse::status("200 OK", "{broken")]);
        let error =
            send_catalog_request("openai", test_client(Duration::from_secs(2)).get(endpoint))
                .await
                .expect_err("malformed catalog JSON");
        server.join().expect("malformed fixture server");
        assert_eq!(error, "openai catalog returned invalid JSON");

        let mut oversized = FixtureResponse::json(json!({"data": []}));
        oversized.declared_length = Some(MAX_CATALOG_PAGE_BYTES + 1);
        let (endpoint, _, server) = serve(vec![oversized]);
        let error =
            send_catalog_request("openai", test_client(Duration::from_secs(2)).get(endpoint))
                .await
                .expect_err("oversized catalog page");
        server.join().expect("oversized fixture server");
        assert!(error.contains("byte limit"), "{error}");

        let conflicting = json!({"data": [
            {"id": "duplicate", "owned_by": "openai"},
            {"id": "duplicate", "owned_by": "customer"}
        ]});
        let (endpoint, _, server) = serve(vec![FixtureResponse::json(conflicting)]);
        let error = fetch_single_page(
            "openai",
            &test_client(Duration::from_secs(2)),
            test_client(Duration::from_secs(2)).get(endpoint),
            |value| parse_openai_compatible_page(value, "openai"),
        )
        .await
        .expect_err("conflicting catalog duplicate");
        server.join().expect("duplicate fixture server");
        assert!(error.contains("conflicting duplicate"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catalog_deadline_and_future_cancellation_are_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("deadline fixture listener");
        let address = listener.local_addr().expect("deadline fixture address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("deadline fixture connection");
            let _ = read_fixture_request(&mut stream);
            std::thread::sleep(Duration::from_millis(150));
        });
        let error = send_catalog_request(
            "openai",
            test_client(Duration::from_millis(25)).get(format!("http://{address}/models")),
        )
        .await
        .expect_err("catalog request deadline");
        server.join().expect("deadline fixture server");
        assert_eq!(
            error,
            "openai catalog request failed before a valid response"
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("cancel fixture listener");
        let address = listener.local_addr().expect("cancel fixture address");
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("cancel fixture connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("cancel fixture timeout");
            let _ = read_fixture_request(&mut stream);
            accepted_tx.send(()).expect("cancel fixture accepted");
            let mut remainder = Vec::new();
            let closed = stream.read_to_end(&mut remainder).is_ok();
            closed_tx
                .send(closed)
                .expect("cancel fixture closed result");
        });
        let request = test_client(Duration::from_secs(5)).get(format!("http://{address}/models"));
        let task = tokio::spawn(send_catalog_request("openai", request));
        accepted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("catalog request reached cancellation fixture");
        task.abort();
        assert!(task
            .await
            .expect_err("cancelled catalog task")
            .is_cancelled());
        assert!(
            closed_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("cancelled connection result"),
            "aborting the catalog future must close its response socket"
        );
        server.join().expect("cancel fixture server");
    }
}
