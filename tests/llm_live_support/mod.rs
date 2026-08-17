use chrono::{DateTime, Utc};
use nib::llm::registry::{provider_descriptor, ProviderTransport};
use nib::llm::LlmTerminalStatus;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

mod catalog;
mod config;
mod plan;
mod report;
mod scenario;

pub use report::PublishedReport;

const DIRECT_PROVIDERS: [&str; 5] = ["openai", "anthropic", "google", "grok", "meta"];
const NETWORK_PROVIDERS: [&str; 6] = [
    "openai",
    "anthropic",
    "google",
    "grok",
    "meta",
    "openrouter",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LiveMode {
    Catalog,
    Canary,
    Full,
}

impl LiveMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "catalog" => Ok(Self::Catalog),
            "canary" => Ok(Self::Canary),
            "full" => Ok(Self::Full),
            _ => Err("NIB_LIVE_MODE must be catalog, canary, or full".to_string()),
        }
    }

    fn makes_generation_requests(self) -> bool {
        !matches!(self, Self::Catalog)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransportId {
    ChatCompletions,
    Responses,
    AnthropicMessages,
    GeminiGenerateContent,
}

impl TransportId {
    fn from_registry(value: ProviderTransport) -> Option<Self> {
        match value {
            ProviderTransport::ChatCompletions => Some(Self::ChatCompletions),
            ProviderTransport::Responses => Some(Self::Responses),
            ProviderTransport::AnthropicMessages => Some(Self::AnthropicMessages),
            ProviderTransport::GeminiGenerateContent => Some(Self::GeminiGenerateContent),
            ProviderTransport::Local => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GeminiGenerateContent => "gemini_generate_content",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CatalogPricing {
    prompt_per_token_usd: Option<f64>,
    completion_per_token_usd: Option<f64>,
    request_usd: Option<f64>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct CatalogModel {
    id: String,
    aliases: Vec<String>,
    supports_text_generation: Option<bool>,
    supports_tools: Option<bool>,
    supports_parallel_tools: Option<bool>,
    supported_parameters: BTreeSet<String>,
    public_identifier: bool,
    owner: Option<String>,
    pricing: Option<CatalogPricing>,
    expiration_date: Option<String>,
}

impl std::fmt::Debug for CatalogModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CatalogModel")
            .field("id", &"<redacted>")
            .field("alias_count", &self.aliases.len())
            .field("supports_text_generation", &self.supports_text_generation)
            .field("supports_tools", &self.supports_tools)
            .field("supports_parallel_tools", &self.supports_parallel_tools)
            .field(
                "supported_parameter_count",
                &self.supported_parameters.len(),
            )
            .field("public_identifier", &self.public_identifier)
            .finish_non_exhaustive()
    }
}

impl CatalogModel {
    fn validate(&self) -> Result<(), String> {
        validate_catalog_identifier(&self.id, "model ID")?;
        if self.aliases.len() > 128 {
            return Err("catalog model exposes more than 128 aliases".to_string());
        }
        if self.supported_parameters.len() > 256
            || self.supported_parameters.iter().any(|parameter| {
                parameter.is_empty()
                    || parameter.len() > 128
                    || !parameter
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
        {
            return Err("catalog supported parameters are malformed".to_string());
        }
        let mut unique = BTreeSet::new();
        for alias in &self.aliases {
            validate_catalog_identifier(alias, "model alias")?;
            if !unique.insert(alias) {
                return Err("catalog model contains a duplicate alias".to_string());
            }
        }
        if self.owner.as_ref().is_some_and(|owner| {
            owner.is_empty() || owner.len() > 512 || owner.chars().any(char::is_control)
        }) {
            return Err("catalog model owner is malformed".to_string());
        }
        if let Some(pricing) = &self.pricing {
            for value in [
                pricing.prompt_per_token_usd,
                pricing.completion_per_token_usd,
                pricing.request_usd,
            ]
            .into_iter()
            .flatten()
            {
                if !value.is_finite() || value < 0.0 {
                    return Err("catalog pricing must be finite and non-negative".to_string());
                }
            }
        }
        Ok(())
    }
}

fn validate_catalog_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.trim().is_empty()
        || value.len() > 512
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return Err(format!(
            "catalog {label} must be 1..=512 bytes with no control characters"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
struct CatalogSnapshot {
    provider: String,
    captured_at: DateTime<Utc>,
    page_count: usize,
    models: Vec<CatalogModel>,
}

impl CatalogSnapshot {
    fn new(
        provider: &str,
        page_count: usize,
        mut models: Vec<CatalogModel>,
    ) -> Result<Self, String> {
        if !NETWORK_PROVIDERS.contains(&provider) {
            return Err("catalog provider is not supported by live qualification".to_string());
        }
        if page_count == 0 || page_count > catalog::MAX_CATALOG_PAGES {
            return Err("catalog page count is outside the supported range".to_string());
        }
        if models.len() > catalog::MAX_CATALOG_MODELS {
            return Err(format!(
                "catalog exceeds the {}-model limit",
                catalog::MAX_CATALOG_MODELS
            ));
        }
        let mut observed = BTreeMap::<String, CatalogModel>::new();
        for model in models.drain(..) {
            model.validate()?;
            match observed.entry(model.id.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(model);
                }
                std::collections::btree_map::Entry::Occupied(entry) if entry.get() == &model => {}
                std::collections::btree_map::Entry::Occupied(_) => {
                    return Err("catalog contains conflicting duplicate model IDs".to_string())
                }
            }
        }
        Ok(Self {
            provider: provider.to_string(),
            captured_at: Utc::now(),
            page_count,
            models: observed.into_values().collect(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Classification {
    Qualified,
    RequiresProbe,
    NotApplicable,
    UnsupportedTransport,
    FailedAdapter,
    CatalogDrift,
    BlockedAuth,
    BlockedQuota,
    BlockedBilling,
    BlockedRegion,
    BlockedRateLimit,
    BlockedConfiguration,
    BlockedBudget,
    Unknown,
}

impl Classification {
    fn is_pass_for_catalog(self) -> bool {
        matches!(
            self,
            Self::RequiresProbe | Self::NotApplicable | Self::Qualified
        )
    }

    fn is_qualified(self) -> bool {
        self == Self::Qualified
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScenarioId {
    CompleteText,
    StreamedText,
    SingleToolContinuation,
    ParallelToolContinuation,
}

impl ScenarioId {
    fn logical_requests(self) -> usize {
        match self {
            Self::CompleteText | Self::StreamedText => 1,
            Self::SingleToolContinuation | Self::ParallelToolContinuation => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ModelProfile {
    model: CatalogModel,
    transport: TransportId,
    advertised: bool,
    required_scenarios: BTreeSet<ScenarioId>,
    projected_cost_ceiling_usd: Option<f64>,
}

fn registry_transports(provider: &str) -> Result<Vec<TransportId>, String> {
    let descriptor = provider_descriptor(provider)
        .ok_or_else(|| format!("unsupported provider '{provider}'"))?;
    let transports = descriptor
        .transports
        .iter()
        .copied()
        .filter_map(TransportId::from_registry)
        .collect::<Vec<_>>();
    if transports.is_empty() {
        return Err(format!("provider '{provider}' has no live transport"));
    }
    Ok(transports)
}

pub async fn run_from_environment() -> Result<PublishedReport, String> {
    let settings = config::LiveSettings::from_environment()?;
    let started_at = Utc::now();
    let run_id = uuid::Uuid::new_v4().to_string();
    let allowlist = plan::OpenRouterAllowlist::load_default()?;
    let mut provider_reports = Vec::new();

    for provider in &settings.providers {
        let snapshot = catalog::fetch_catalog(provider, &settings).await?;
        let run_plan = plan::build_plan(&settings, &snapshot, &allowlist)?;
        let mut report = if settings.mode == LiveMode::Catalog {
            report::catalog_provider_report(&run_id, &snapshot, &run_plan)
        } else {
            scenario::execute_provider_plan(&settings, &run_id, &snapshot, run_plan).await
        };
        if settings.mode.makes_generation_requests() {
            let confirmation = catalog::fetch_catalog(provider, &settings).await?;
            if snapshot.models != confirmation.models {
                report::mark_catalog_drift(&mut report);
            }
        }
        provider_reports.push(report);
    }

    let report = report::QualificationReport::new(
        run_id,
        settings.mode,
        started_at,
        Utc::now(),
        provider_reports,
    );
    report::publish(&settings, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            aliases: Vec::new(),
            supports_text_generation: Some(true),
            supports_tools: None,
            supports_parallel_tools: None,
            supported_parameters: BTreeSet::new(),
            public_identifier: true,
            owner: None,
            pricing: None,
            expiration_date: None,
        }
    }

    #[test]
    fn snapshot_deduplicates_identical_models_and_rejects_conflicts() {
        let snapshot = CatalogSnapshot::new("openai", 1, vec![model("a"), model("a")]).unwrap();
        assert_eq!(snapshot.models.len(), 1);

        let mut conflicting = model("a");
        conflicting.supports_text_generation = Some(false);
        assert!(
            CatalogSnapshot::new("openai", 1, vec![model("a"), conflicting])
                .unwrap_err()
                .contains("conflicting duplicate")
        );
    }

    #[test]
    fn model_debug_never_renders_raw_identifier() {
        let rendered = format!("{:?}", model("private-model-id"));
        assert!(!rendered.contains("private-model-id"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn all_network_providers_have_live_transports() {
        for provider in NETWORK_PROVIDERS {
            assert!(!registry_transports(provider).unwrap().is_empty());
        }
        assert!(!DIRECT_PROVIDERS.contains(&"openrouter"));
    }
}
