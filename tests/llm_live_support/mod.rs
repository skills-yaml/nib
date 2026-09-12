use chrono::{DateTime, Utc};
use nib::llm::registry::{provider_descriptor, ProviderTransport};
use nib::llm::LlmTerminalStatus;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

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
    Selected,
    Full,
}

impl LiveMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "catalog" => Ok(Self::Catalog),
            "canary" => Ok(Self::Canary),
            "selected" => Ok(Self::Selected),
            "full" => Ok(Self::Full),
            _ => Err("NIB_LIVE_MODE must be catalog, canary, selected, or full".to_string()),
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
    generation_target: Option<String>,
    aliases: Vec<String>,
    supports_text_generation: Option<bool>,
    supports_tools: Option<bool>,
    supports_parallel_tools: Option<bool>,
    #[serde(default)]
    input_modalities: BTreeSet<String>,
    #[serde(default)]
    output_modalities: BTreeSet<String>,
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
            .field(
                "generation_target",
                &self.generation_target.as_ref().map(|_| "<redacted>"),
            )
            .field("alias_count", &self.aliases.len())
            .field("supports_text_generation", &self.supports_text_generation)
            .field("supports_tools", &self.supports_tools)
            .field("supports_parallel_tools", &self.supports_parallel_tools)
            .field("input_modality_count", &self.input_modalities.len())
            .field("output_modality_count", &self.output_modalities.len())
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
        if let Some(generation_target) = &self.generation_target {
            validate_catalog_identifier(generation_target, "generation target")?;
        }
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
        for (label, modalities) in [
            ("input", &self.input_modalities),
            ("output", &self.output_modalities),
        ] {
            if modalities.len() > 32
                || modalities.iter().any(|modality| {
                    modality.is_empty()
                        || modality.len() > 64
                        || !modality
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
            {
                return Err(format!("catalog {label} modalities are malformed"));
            }
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

    fn generation_target(&self) -> &str {
        self.generation_target.as_deref().unwrap_or(&self.id)
    }

    fn matches_reference(&self, reference: &str) -> bool {
        self.id == reference
            || self.generation_target.as_deref() == Some(reference)
            || self.aliases.iter().any(|alias| alias == reference)
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
    OmittedByPolicy,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AccountingJustification {
    CatalogTextGenerationUnsupported,
    CatalogMetadataRequiresProbe,
    CanaryDefaultPolicy,
    SelectedMatrixPolicy,
    OpenRouterAllowlistPolicy,
    QualificationProfileEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScenarioNotApplicableJustification {
    CatalogParallelToolsUnsupported,
    CatalogParallelToolsNotAdvertised,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScenarioNotExecutedReason {
    TransportUnsupportedByBasicProbe,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SelectedSuiteEvidence {
    suite_id: String,
    matrix_sha256: String,
    owner: String,
    reviewed_at: String,
    expires_at: String,
    required_task_count: usize,
    conditional_task_count: usize,
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
    not_applicable_scenarios: BTreeMap<ScenarioId, ScenarioNotApplicableJustification>,
    projected_cost_ceiling_usd: Option<f64>,
}

fn registry_transports(provider: &str) -> Result<Vec<TransportId>, String> {
    let descriptor = provider_descriptor(provider)
        .ok_or_else(|| format!("unsupported provider '{provider}'"))?;
    let transports = descriptor
        .transports()
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
    let run_started = Instant::now();
    let run_id = uuid::Uuid::new_v4().to_string();
    let privacy_key = report::ReportPrivacyKey::random();
    let allowlist = match plan::OpenRouterAllowlist::load_default() {
        Ok(allowlist) => allowlist,
        Err(_) => {
            return publish_uniform_blocker(
                &settings,
                run_id,
                started_at,
                None,
                Classification::BlockedConfiguration,
                "blocked_allowlist_configuration",
            )
        }
    };
    let selected_matrix = if settings.mode == LiveMode::Selected {
        match plan::SelectedMatrix::load_default() {
            Ok(matrix) => Some(matrix),
            Err(_) => {
                return publish_uniform_blocker(
                    &settings,
                    run_id,
                    started_at,
                    None,
                    Classification::BlockedConfiguration,
                    "blocked_selected_matrix_configuration",
                )
            }
        }
    } else {
        None
    };
    let mut prepared = Vec::with_capacity(settings.providers.len());
    let mut preparation_failed = false;

    for provider in &settings.providers {
        let capture =
            match fetch_catalog_before_run_deadline(provider, &settings, run_started).await {
                Ok(capture) => capture,
                Err(error) => {
                    preparation_failed = true;
                    let (classification, safe_class) = classify_local_blocker(&error);
                    prepared.push(Err(report::blocked_provider_report(
                        provider,
                        classification,
                        safe_class,
                    )));
                    continue;
                }
            };
        let snapshot = &capture.snapshot;
        if remaining_run_duration(&settings, run_started).is_err() {
            preparation_failed = true;
            prepared.push(Err(report::blocked_provider_report(
                provider,
                Classification::BlockedBudget,
                "blocked_run_deadline",
            )));
            continue;
        }
        match plan::build_plan(&settings, snapshot, &allowlist, selected_matrix.as_ref()) {
            Ok(run_plan) => prepared.push(Ok((capture, run_plan))),
            Err(error) => {
                preparation_failed = true;
                let (classification, safe_class) = classify_local_blocker(&error);
                prepared.push(Err(report::blocked_provider_report(
                    provider,
                    classification,
                    safe_class,
                )));
            }
        }
    }
    let plans = prepared
        .iter()
        .filter_map(|prepared| prepared.as_ref().ok().map(|(_, plan)| plan.clone()))
        .collect::<Vec<_>>();

    if settings.mode == LiveMode::Catalog {
        let provider_reports = prepared
            .into_iter()
            .map(|prepared| match prepared {
                Ok((capture, plan)) => {
                    report::catalog_provider_report(&privacy_key, &run_id, &capture.snapshot, &plan)
                }
                Err(report) => report,
            })
            .collect();
        let qualification = report::QualificationReport::new(
            run_id,
            settings.mode,
            started_at,
            Utc::now(),
            provider_reports,
            None,
            &settings,
        );
        return report::publish(&settings, qualification);
    }

    let matrix_failure = if preparation_failed {
        Some((
            Classification::BlockedConfiguration,
            "blocked_incomplete_matrix_preflight",
        ))
    } else if remaining_run_duration(&settings, run_started).is_err() {
        Some((Classification::BlockedBudget, "blocked_run_deadline"))
    } else {
        plan::validate_generation_matrix(&settings, &plans)
            .err()
            .map(|error| classify_local_blocker(&error))
    };

    if let Some((classification, safe_class)) = matrix_failure {
        let provider_reports = prepared
            .into_iter()
            .map(|prepared| match prepared {
                Ok((capture, plan)) => report::blocked_planned_provider_report(
                    &privacy_key,
                    &run_id,
                    &capture.snapshot,
                    &plan,
                    classification,
                    safe_class,
                ),
                Err(report) => report,
            })
            .collect();
        let qualification = report::QualificationReport::new(
            run_id,
            settings.mode,
            started_at,
            Utc::now(),
            provider_reports,
            selected_matrix.as_ref().map(plan::SelectedMatrix::evidence),
            &settings,
        );
        return report::publish(&settings, qualification);
    }

    // No generation request is reachable until every selected provider catalog and
    // plan has contributed to the exact whole-run preflight denominator above.
    let mut provider_reports = Vec::with_capacity(prepared.len());
    let run_budget = scenario::RunBudget::new_at(run_started);
    for prepared in prepared {
        let (capture, run_plan) = prepared.expect("complete matrix preflight");
        let snapshot = capture.snapshot;
        let mut provider_report = scenario::execute_provider_plan(
            &settings,
            &privacy_key,
            &run_id,
            &snapshot,
            run_plan,
            &run_budget,
            (snapshot.provider == "meta").then_some(capture.protected_client),
        )
        .await;
        match fetch_catalog_before_run_deadline(&snapshot.provider, &settings, run_started).await {
            Ok(confirmation) if snapshot.models != confirmation.snapshot.models => {
                report::mark_catalog_drift(&mut provider_report);
            }
            Ok(_) => {}
            Err(error) => {
                let (classification, safe_class) = classify_local_blocker(&error);
                report::mark_provider_blocker(&mut provider_report, classification, safe_class);
            }
        }
        provider_reports.push(provider_report);
    }

    let report = report::QualificationReport::new(
        run_id,
        settings.mode,
        started_at,
        Utc::now(),
        provider_reports,
        selected_matrix.as_ref().map(plan::SelectedMatrix::evidence),
        &settings,
    );
    report::publish(&settings, report)
}

fn remaining_run_duration(
    settings: &config::LiveSettings,
    started: Instant,
) -> Result<Duration, String> {
    settings
        .limits
        .max_run_duration
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "blocked_budget: live run deadline exhausted".to_string())
}

async fn fetch_catalog_before_run_deadline(
    provider: &str,
    settings: &config::LiveSettings,
    started: Instant,
) -> Result<catalog::CatalogCapture, String> {
    let remaining = remaining_run_duration(settings, started)?;
    tokio::time::timeout(remaining, catalog::fetch_catalog(provider, settings))
        .await
        .map_err(|_| "blocked_budget: live run deadline exhausted".to_string())?
}

fn classify_local_blocker(error: &str) -> (Classification, &'static str) {
    for (token, classification, safe_class) in [
        ("blocked_auth", Classification::BlockedAuth, "blocked_auth"),
        (
            "blocked_rate_limit",
            Classification::BlockedRateLimit,
            "blocked_rate_limit",
        ),
        (
            "blocked_billing",
            Classification::BlockedBilling,
            "blocked_billing",
        ),
        (
            "blocked_region",
            Classification::BlockedRegion,
            "blocked_region",
        ),
        (
            "blocked_quota",
            Classification::BlockedQuota,
            "blocked_quota",
        ),
        (
            "blocked_budget",
            Classification::BlockedBudget,
            "blocked_budget",
        ),
        (
            "blocked_configuration",
            Classification::BlockedConfiguration,
            "blocked_configuration",
        ),
    ] {
        if error.contains(token) {
            return (classification, safe_class);
        }
    }
    (Classification::Unknown, "catalog_or_plan_failure")
}

fn publish_uniform_blocker(
    settings: &config::LiveSettings,
    run_id: String,
    started_at: DateTime<Utc>,
    selected_suite: Option<SelectedSuiteEvidence>,
    classification: Classification,
    safe_error_class: &'static str,
) -> Result<PublishedReport, String> {
    let providers = settings
        .providers
        .iter()
        .map(|provider| report::blocked_provider_report(provider, classification, safe_error_class))
        .collect();
    let qualification = report::QualificationReport::new(
        run_id,
        settings.mode,
        started_at,
        Utc::now(),
        providers,
        selected_suite,
        settings,
    );
    report::publish(settings, qualification)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            generation_target: None,
            aliases: Vec::new(),
            supports_text_generation: Some(true),
            supports_tools: None,
            supports_parallel_tools: None,
            input_modalities: ["text".to_string()].into_iter().collect(),
            output_modalities: ["text".to_string()].into_iter().collect(),
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
