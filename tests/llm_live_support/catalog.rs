use super::config::LiveSettings;
use super::{CatalogModel, CatalogPricing, CatalogSnapshot};
use futures_util::StreamExt;
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde_json::Value;
use std::collections::BTreeSet;
use std::time::Duration;

pub(super) const MAX_CATALOG_PAGES: usize = 100;
pub(super) const MAX_CATALOG_MODELS: usize = 10_000;
const MAX_CATALOG_PAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOG_URL_BYTES: usize = 4 * 1024;
const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) async fn fetch_catalog(
    provider: &str,
    settings: &LiveSettings,
) -> Result<CatalogSnapshot, String> {
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
) -> Result<CatalogSnapshot, String> {
    let credential = settings.credential(provider)?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(CATALOG_REQUEST_TIMEOUT)
        .build()
        .map_err(|_| "failed to construct bounded catalog HTTP client".to_string())?;
    match provider {
        "anthropic" => fetch_anthropic(&client, &credential).await,
        "google" => fetch_gemini(&client, &credential).await,
        "grok" => {
            fetch_single_page(
                provider,
                &client,
                authenticated_bearer(
                    &client,
                    catalog_url("https://api.x.ai/v1", "language-models")?,
                    &credential,
                ),
                parse_xai_page,
            )
            .await
        }
        "openrouter" => {
            fetch_single_page(
                provider,
                &client,
                authenticated_bearer(
                    &client,
                    catalog_url("https://openrouter.ai/api/v1", "models")?,
                    &credential,
                ),
                parse_openrouter_page,
            )
            .await
        }
        "openai" => {
            fetch_single_page(
                provider,
                &client,
                authenticated_bearer(
                    &client,
                    catalog_url("https://api.openai.com/v1", "models")?,
                    &credential,
                ),
                |value| parse_openai_compatible_page(value, "openai"),
            )
            .await
        }
        "meta" => {
            let root = settings.meta_base_url.as_deref().ok_or_else(|| {
                "Meta catalog is blocked_configuration because its base URL is missing".to_string()
            })?;
            fetch_single_page(
                provider,
                &client,
                authenticated_bearer(&client, catalog_url(root, "models")?, &credential),
                |value| parse_openai_compatible_page(value, "meta"),
            )
            .await
        }
        _ => Err("unsupported live catalog provider".to_string()),
    }
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

async fn fetch_anthropic(client: &Client, credential: &str) -> Result<CatalogSnapshot, String> {
    let endpoint = catalog_url("https://api.anthropic.com", "v1/models")?;
    let mut after_id: Option<String> = None;
    let mut seen_cursors = BTreeSet::new();
    let mut models = Vec::new();
    let mut pages = 0;
    loop {
        pages += 1;
        if pages > MAX_CATALOG_PAGES {
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
        if models.len() > MAX_CATALOG_MODELS {
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

async fn fetch_gemini(client: &Client, credential: &str) -> Result<CatalogSnapshot, String> {
    let endpoint = catalog_url("https://generativelanguage.googleapis.com/v1beta", "models")?;
    let mut page_token: Option<String> = None;
    let mut seen_tokens = BTreeSet::new();
    let mut models = Vec::new();
    let mut pages = 0;
    loop {
        pages += 1;
        if pages > MAX_CATALOG_PAGES {
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
        if models.len() > MAX_CATALOG_MODELS {
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

fn catalog_url(root: &str, suffix: &str) -> Result<Url, String> {
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
                aliases: Vec::new(),
                supports_text_generation: None,
                supports_tools: None,
                supports_parallel_tools: None,
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
                aliases,
                supports_text_generation: supports_text,
                supports_tools: None,
                supports_parallel_tools: None,
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
                aliases: Vec::new(),
                supports_text_generation: supports_text,
                supports_tools: Some(parameters.iter().any(|value| value == "tools")),
                supports_parallel_tools: Some(
                    parameters
                        .iter()
                        .any(|value| value == "parallel_tool_calls"),
                ),
                supported_parameters: parameters.into_iter().collect(),
                public_identifier: true,
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
                aliases: Vec::new(),
                supports_text_generation: Some(true),
                supports_tools: tools,
                supports_parallel_tools: None,
                supported_parameters: BTreeSet::new(),
                public_identifier: true,
                owner: Some("anthropic".to_string()),
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
            let id = resource_name
                .strip_prefix("models/")
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    "Gemini model name must use the models/ resource prefix".to_string()
                })?
                .to_string();
            let actions = model
                .get("supportedGenerationMethods")
                .or_else(|| model.get("supported_actions"))
                .map(string_array)
                .transpose()?
                .unwrap_or_default();
            Ok(CatalogModel {
                id,
                aliases: Vec::new(),
                supports_text_generation: Some(
                    actions
                        .iter()
                        .any(|action| action.eq_ignore_ascii_case("generateContent")),
                ),
                supports_tools: None,
                supports_parallel_tools: None,
                supported_parameters: BTreeSet::new(),
                public_identifier: true,
                owner: Some("google".to_string()),
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
        assert!(parse_anthropic_page(&json!({"data": [], "has_more": "yes"})).is_err());
    }

    #[test]
    fn parses_gemini_capabilities_and_exact_resource_prefix() {
        let page = parse_gemini_page(&json!({
            "models": [
                {"name": "models/gemini-chat", "supportedGenerationMethods": ["generateContent"]},
                {"name": "models/embed", "supportedGenerationMethods": ["embedContent"]}
            ],
            "nextPageToken": "next"
        }))
        .unwrap();
        assert_eq!(page.models[0].id, "gemini-chat");
        assert_eq!(page.models[0].supports_text_generation, Some(true));
        assert_eq!(page.models[1].supports_text_generation, Some(false));
        assert!(parse_gemini_page(&json!({"models": [{"name": "gemini-chat"}]})).is_err());
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
}
