use super::config::LiveSettings;
use super::plan::RunPlan;
use super::{
    AccountingJustification, CatalogModel, CatalogSnapshot, Classification, LiveMode, ScenarioId,
    ScenarioNotApplicableJustification, ScenarioNotExecutedReason, SelectedSuiteEvidence,
    TransportId,
};
use chrono::{DateTime, Utc};
use nib::fs_security::FileIdentity;
use nib::llm::LlmUsage;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

const REPORT_SCHEMA_VERSION: u32 = 2;
const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SAFE_MODEL_REF_BYTES: usize = 512;
const MAX_RUN_ID_BYTES: usize = 128;
const MAX_ATTEMPTS_PER_LOGICAL_REQUEST: usize = 3;

#[derive(Clone)]
pub(super) struct ReportPrivacyKey([u8; 32]);

impl ReportPrivacyKey {
    pub(super) fn random() -> Self {
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(first.as_bytes());
        key[16..].copy_from_slice(second.as_bytes());
        Self(key)
    }

    #[cfg(test)]
    fn fixture(byte: u8) -> Self {
        Self([byte; 32])
    }
}

impl std::fmt::Debug for ReportPrivacyKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ReportPrivacyKey(<redacted>)")
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct CatalogEntryReport {
    model_ref: String,
    classification: Classification,
    justification: AccountingJustification,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ScenarioReport {
    pub scenario: ScenarioId,
    pub passed: bool,
    pub duration_ms: u64,
    pub actual_logical_requests: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_attempts: Option<usize>,
    pub usage_requests: usize,
    pub usage_complete_requests: usize,
    pub usage_completeness: UsageCompleteness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<LlmUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub budget_blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_executed_reason: Option<ScenarioNotExecutedReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_error_class: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum UsageCompleteness {
    Complete,
    Partial,
    Unknown,
}

impl UsageCompleteness {
    pub(super) fn from_counts(
        actual_requests: usize,
        usage_requests: usize,
        usage_complete_requests: usize,
    ) -> Self {
        if actual_requests > 0 && usage_complete_requests == actual_requests {
            Self::Complete
        } else if usage_requests > 0 {
            Self::Partial
        } else {
            Self::Unknown
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) struct ScenarioNotApplicableEvidence {
    pub scenario: ScenarioId,
    pub justification: ScenarioNotApplicableJustification,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ProfileReport {
    pub model_ref: String,
    pub transport: TransportId,
    pub advertised: bool,
    pub classification: Classification,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<super::CatalogPricing>,
    pub required_scenarios: Vec<ScenarioId>,
    pub scenarios: Vec<ScenarioReport>,
    pub not_applicable_scenarios: Vec<ScenarioId>,
    pub not_applicable_evidence: Vec<ScenarioNotApplicableEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ProviderReport {
    pub provider: String,
    pub catalog_captured_at: DateTime<Utc>,
    pub catalog_pages: usize,
    pub catalog_models: usize,
    pub catalog_hash: String,
    pub catalog_drift: bool,
    pub duration_ms: u64,
    pub logical_requests: usize,
    pub maximum_attempts: usize,
    pub maximum_output_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_cost_usd: Option<f64>,
    pub effective_concurrency: usize,
    pub actual_logical_requests: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_attempts: Option<usize>,
    pub usage_requests: usize,
    pub usage_complete_requests: usize,
    pub usage_completeness: UsageCompleteness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<LlmUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker_classification: Option<Classification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_error_class: Option<String>,
    pub evidence_complete: bool,
    pub budget_truncated: bool,
    pub accounting: Vec<CatalogEntryReport>,
    pub profiles: Vec<ProfileReport>,
    pub complete: bool,
    pub passed: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(super) struct BudgetLimitsReport {
    configured_concurrency: usize,
    max_logical_requests: usize,
    max_attempts: usize,
    max_output_tokens_per_request: u32,
    max_output_tokens: u64,
    max_scenario_elapsed_ms: u64,
    max_provider_elapsed_ms: u64,
    max_run_elapsed_ms: u64,
    max_actual_cost_usd: f64,
}

impl BudgetLimitsReport {
    fn from_settings(settings: &LiveSettings) -> Self {
        let limits = &settings.limits;
        Self {
            configured_concurrency: settings.concurrency,
            max_logical_requests: limits.max_logical_requests,
            max_attempts: limits.max_attempts,
            max_output_tokens_per_request: limits.max_output_tokens_per_request,
            max_output_tokens: limits.max_total_output_tokens,
            max_scenario_elapsed_ms: limits
                .max_scenario_duration
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            max_provider_elapsed_ms: limits
                .max_provider_duration
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            max_run_elapsed_ms: limits
                .max_run_duration
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            max_actual_cost_usd: limits.max_actual_cost_usd,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct QualificationReport {
    schema_version: u32,
    run_id: String,
    source_revision: String,
    platform: String,
    mode: LiveMode,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_suite: Option<SelectedSuiteEvidence>,
    providers: Vec<ProviderReport>,
    budget_limits: BudgetLimitsReport,
    duration_ms: u64,
    actual_logical_requests: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_attempts: Option<usize>,
    usage_requests: usize,
    usage_complete_requests: usize,
    usage_completeness: UsageCompleteness,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<LlmUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_cost_usd: Option<f64>,
    evidence_complete: bool,
    budget_truncated: bool,
    complete: bool,
    passed: bool,
}

impl QualificationReport {
    pub fn new(
        run_id: String,
        mode: LiveMode,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
        providers: Vec<ProviderReport>,
        selected_suite: Option<SelectedSuiteEvidence>,
        settings: &LiveSettings,
    ) -> Self {
        let aggregate = aggregate_provider_evidence(&providers);
        let duration_ms = completed_at
            .signed_duration_since(started_at)
            .num_milliseconds();
        let duration_ms = u64::try_from(duration_ms).unwrap_or_default();
        let evidence_complete =
            aggregate.valid && providers.iter().all(|provider| provider.evidence_complete);
        let budget_truncated = providers.iter().any(|provider| provider.budget_truncated);
        let complete = !providers.is_empty()
            && evidence_complete
            && !budget_truncated
            && providers.iter().all(|provider| provider.complete);
        let passed = complete && providers.iter().all(|provider| provider.passed);
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            run_id,
            source_revision: env!("NIB_BUILD_COMMIT").to_string(),
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            mode,
            started_at,
            completed_at,
            selected_suite,
            providers,
            budget_limits: BudgetLimitsReport::from_settings(settings),
            duration_ms,
            actual_logical_requests: aggregate.actual_logical_requests,
            actual_attempts: aggregate.actual_attempts,
            usage_requests: aggregate.usage_requests,
            usage_complete_requests: aggregate.usage_complete_requests,
            usage_completeness: aggregate.usage_completeness,
            usage: aggregate.usage,
            actual_cost_usd: aggregate.actual_cost_usd,
            evidence_complete,
            budget_truncated,
            complete,
            passed,
        }
    }
}

#[derive(Debug)]
pub struct PublishedReport {
    pub json: PathBuf,
    pub markdown: PathBuf,
    pub passed: bool,
}

#[derive(Debug)]
struct EvidenceAggregate {
    actual_logical_requests: usize,
    actual_attempts: Option<usize>,
    usage_requests: usize,
    usage_complete_requests: usize,
    usage_completeness: UsageCompleteness,
    usage: Option<LlmUsage>,
    actual_cost_usd: Option<f64>,
    valid: bool,
}

fn checked_cost_sum(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut total = 0.0;
    for value in values {
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        total += value;
        if !total.is_finite() {
            return None;
        }
    }
    Some(total)
}

pub(super) fn calculate_actual_cost(
    pricing: Option<&super::CatalogPricing>,
    usage: Option<LlmUsage>,
    actual_attempts: Option<usize>,
) -> Result<Option<f64>, String> {
    let (Some(pricing), Some(usage), Some(actual_attempts)) = (pricing, usage, actual_attempts)
    else {
        return Ok(None);
    };
    let (Some(prompt_price), Some(completion_price)) = (
        pricing.prompt_per_token_usd,
        pricing.completion_per_token_usd,
    ) else {
        return Ok(None);
    };
    let request_price = pricing.request_usd.unwrap_or_default();
    for price in [prompt_price, completion_price, request_price] {
        if !price.is_finite() || price < 0.0 {
            return Err("live actual-cost pricing is invalid".to_string());
        }
    }
    let input_cost = prompt_price * usage.input_tokens as f64;
    let output_cost = completion_price * usage.output_tokens as f64;
    let request_cost = request_price * actual_attempts as f64;
    checked_cost_sum([input_cost, output_cost, request_cost].into_iter())
        .ok_or_else(|| "live actual-cost calculation overflowed".to_string())
        .map(Some)
}

fn checked_optional_usize_sum(
    values: impl Iterator<Item = Option<usize>>,
) -> (Option<usize>, bool) {
    let mut total = 0usize;
    let mut complete = true;
    for value in values {
        match value {
            Some(value) => match total.checked_add(value) {
                Some(next) => total = next,
                None => return (None, false),
            },
            None => complete = false,
        }
    }
    (complete.then_some(total), true)
}

fn aggregate_scenario_evidence<'a>(
    scenarios: impl Iterator<Item = &'a ScenarioReport>,
) -> EvidenceAggregate {
    let scenarios = scenarios.collect::<Vec<_>>();
    let actual_logical_requests = scenarios.iter().try_fold(0usize, |total, scenario| {
        total.checked_add(scenario.actual_logical_requests)
    });
    let usage_requests = scenarios.iter().try_fold(0usize, |total, scenario| {
        total.checked_add(scenario.usage_requests)
    });
    let usage_complete_requests = scenarios.iter().try_fold(0usize, |total, scenario| {
        total.checked_add(scenario.usage_complete_requests)
    });
    let (actual_attempts, attempts_valid) =
        checked_optional_usize_sum(scenarios.iter().map(|scenario| scenario.actual_attempts));
    let usage = scenarios
        .iter()
        .filter_map(|scenario| scenario.usage)
        .try_fold(None, |total: Option<LlmUsage>, usage| {
            total.map_or(Ok(Some(usage)), |total| total.checked_add(usage).map(Some))
        });
    let (actual_cost_usd, cost_valid) = if scenarios
        .iter()
        .any(|scenario| scenario.actual_logical_requests > 0 && scenario.actual_cost_usd.is_none())
    {
        (None, true)
    } else {
        match checked_cost_sum(
            scenarios
                .iter()
                .filter_map(|scenario| scenario.actual_cost_usd),
        ) {
            Some(cost) => (Some(cost), true),
            None => (None, false),
        }
    };
    let valid = actual_logical_requests.is_some()
        && usage_requests.is_some()
        && usage_complete_requests.is_some()
        && attempts_valid
        && (actual_logical_requests == Some(0) || actual_attempts.is_some())
        && usage.is_ok()
        && cost_valid;
    let actual_logical_requests = actual_logical_requests.unwrap_or_default();
    let usage_requests = usage_requests.unwrap_or_default();
    let usage_complete_requests = usage_complete_requests.unwrap_or_default();
    EvidenceAggregate {
        actual_logical_requests,
        actual_attempts,
        usage_requests,
        usage_complete_requests,
        usage_completeness: UsageCompleteness::from_counts(
            actual_logical_requests,
            usage_requests,
            usage_complete_requests,
        ),
        usage: usage.unwrap_or(None),
        actual_cost_usd,
        valid,
    }
}

fn aggregate_provider_evidence(providers: &[ProviderReport]) -> EvidenceAggregate {
    let actual_logical_requests = providers.iter().try_fold(0usize, |total, provider| {
        total.checked_add(provider.actual_logical_requests)
    });
    let usage_requests = providers.iter().try_fold(0usize, |total, provider| {
        total.checked_add(provider.usage_requests)
    });
    let usage_complete_requests = providers.iter().try_fold(0usize, |total, provider| {
        total.checked_add(provider.usage_complete_requests)
    });
    let (actual_attempts, attempts_valid) =
        checked_optional_usize_sum(providers.iter().map(|provider| provider.actual_attempts));
    let usage = providers
        .iter()
        .filter_map(|provider| provider.usage)
        .try_fold(None, |total: Option<LlmUsage>, usage| {
            total.map_or(Ok(Some(usage)), |total| total.checked_add(usage).map(Some))
        });
    let (actual_cost_usd, cost_valid) = if providers
        .iter()
        .any(|provider| provider.actual_logical_requests > 0 && provider.actual_cost_usd.is_none())
    {
        (None, true)
    } else {
        match checked_cost_sum(
            providers
                .iter()
                .filter_map(|provider| provider.actual_cost_usd),
        ) {
            Some(cost) => (Some(cost), true),
            None => (None, false),
        }
    };
    let valid = actual_logical_requests.is_some()
        && usage_requests.is_some()
        && usage_complete_requests.is_some()
        && attempts_valid
        && (actual_logical_requests == Some(0) || actual_attempts.is_some())
        && usage.is_ok()
        && cost_valid;
    let actual_logical_requests = actual_logical_requests.unwrap_or_default();
    let usage_requests = usage_requests.unwrap_or_default();
    let usage_complete_requests = usage_complete_requests.unwrap_or_default();
    EvidenceAggregate {
        actual_logical_requests,
        actual_attempts,
        usage_requests,
        usage_complete_requests,
        usage_completeness: UsageCompleteness::from_counts(
            actual_logical_requests,
            usage_requests,
            usage_complete_requests,
        ),
        usage: usage.unwrap_or(None),
        actual_cost_usd,
        valid,
    }
}

pub(super) fn catalog_provider_report(
    privacy_key: &ReportPrivacyKey,
    _run_id: &str,
    snapshot: &CatalogSnapshot,
    plan: &RunPlan,
) -> ProviderReport {
    let accounting = plan
        .accounting
        .iter()
        .map(|entry| CatalogEntryReport {
            model_ref: model_reference(privacy_key, &snapshot.provider, &entry.model),
            classification: entry.classification,
            justification: entry.justification,
        })
        .collect::<Vec<_>>();
    let complete = accounting.len() == snapshot.models.len();
    let passed = complete
        && accounting
            .iter()
            .all(|entry| entry.classification.is_pass_for_catalog());
    ProviderReport {
        provider: plan.provider.clone(),
        catalog_captured_at: snapshot.captured_at,
        catalog_pages: snapshot.page_count,
        catalog_models: snapshot.models.len(),
        catalog_hash: catalog_hash(privacy_key, snapshot),
        catalog_drift: false,
        duration_ms: 0,
        logical_requests: plan.logical_requests,
        maximum_attempts: plan.maximum_attempts,
        maximum_output_tokens: plan.maximum_output_tokens,
        projected_cost_usd: plan.projected_cost_usd,
        effective_concurrency: 0,
        actual_logical_requests: 0,
        actual_attempts: Some(0),
        usage_requests: 0,
        usage_complete_requests: 0,
        usage_completeness: UsageCompleteness::Unknown,
        usage: None,
        actual_cost_usd: Some(0.0),
        blocker_classification: None,
        safe_error_class: None,
        evidence_complete: true,
        budget_truncated: false,
        accounting,
        profiles: Vec::new(),
        complete,
        passed,
    }
}

pub(super) fn blocked_provider_report(
    provider: &str,
    classification: Classification,
    safe_error_class: &'static str,
) -> ProviderReport {
    ProviderReport {
        provider: provider.to_string(),
        catalog_captured_at: Utc::now(),
        catalog_pages: 0,
        catalog_models: 0,
        catalog_hash: format!("{:x}", Sha256::digest([])),
        catalog_drift: false,
        duration_ms: 0,
        logical_requests: 0,
        maximum_attempts: 0,
        maximum_output_tokens: 0,
        projected_cost_usd: Some(0.0),
        effective_concurrency: 0,
        actual_logical_requests: 0,
        actual_attempts: Some(0),
        usage_requests: 0,
        usage_complete_requests: 0,
        usage_completeness: UsageCompleteness::Unknown,
        usage: None,
        actual_cost_usd: Some(0.0),
        blocker_classification: Some(classification),
        safe_error_class: Some(safe_error_class.to_string()),
        evidence_complete: true,
        budget_truncated: classification == Classification::BlockedBudget,
        accounting: Vec::new(),
        profiles: Vec::new(),
        complete: false,
        passed: false,
    }
}

pub(super) fn blocked_planned_provider_report(
    privacy_key: &ReportPrivacyKey,
    run_id: &str,
    snapshot: &CatalogSnapshot,
    plan: &RunPlan,
    classification: Classification,
    safe_error_class: &'static str,
) -> ProviderReport {
    let mut report = catalog_provider_report(privacy_key, run_id, snapshot, plan);
    report.logical_requests = plan.logical_requests;
    report.maximum_attempts = plan.maximum_attempts;
    report.maximum_output_tokens = plan.maximum_output_tokens;
    report.projected_cost_usd = plan.projected_cost_usd;
    report.blocker_classification = Some(classification);
    report.safe_error_class = Some(safe_error_class.to_string());
    report.budget_truncated = classification == Classification::BlockedBudget;
    report.complete = false;
    report.passed = false;
    report
}

pub(super) fn generation_provider_report(
    privacy_key: &ReportPrivacyKey,
    _run_id: &str,
    snapshot: &CatalogSnapshot,
    plan: &RunPlan,
    profiles: Vec<ProfileReport>,
    mode: LiveMode,
    effective_concurrency: usize,
) -> ProviderReport {
    let aggregate =
        aggregate_scenario_evidence(profiles.iter().flat_map(|profile| profile.scenarios.iter()));
    let budget_truncated = profiles
        .iter()
        .flat_map(|profile| profile.scenarios.iter())
        .any(|scenario| scenario.budget_blocked);
    let accounting = plan
        .accounting
        .iter()
        .map(|entry| {
            let profile_classifications = profiles
                .iter()
                .filter(|profile| {
                    profile.model_ref
                        == model_reference(privacy_key, &snapshot.provider, &entry.model)
                })
                .map(|profile| profile.classification)
                .collect::<Vec<_>>();
            let (classification, justification) = if matches!(
                entry.classification,
                Classification::NotApplicable | Classification::OmittedByPolicy
            ) {
                (entry.classification, entry.justification)
            } else if profile_classifications.is_empty() {
                (
                    Classification::Unknown,
                    AccountingJustification::QualificationProfileEvidence,
                )
            } else if profile_classifications
                .iter()
                .any(|classification| classification.is_qualified())
                && profile_classifications.iter().all(|classification| {
                    matches!(
                        classification,
                        Classification::Qualified | Classification::UnsupportedTransport
                    )
                })
            {
                (
                    Classification::Qualified,
                    AccountingJustification::QualificationProfileEvidence,
                )
            } else {
                (
                    profile_classifications
                        .into_iter()
                        .find(|classification| !classification.is_qualified())
                        .unwrap_or(Classification::Unknown),
                    AccountingJustification::QualificationProfileEvidence,
                )
            };
            CatalogEntryReport {
                model_ref: model_reference(privacy_key, &snapshot.provider, &entry.model),
                classification,
                justification,
            }
        })
        .collect::<Vec<_>>();
    let full_complete = accounting.len() == snapshot.models.len()
        && accounting.iter().all(|entry| {
            !matches!(
                entry.classification,
                Classification::RequiresProbe
                    | Classification::CatalogDrift
                    | Classification::BlockedAuth
                    | Classification::BlockedQuota
                    | Classification::BlockedBilling
                    | Classification::BlockedRegion
                    | Classification::BlockedRateLimit
                    | Classification::BlockedConfiguration
                    | Classification::BlockedBudget
                    | Classification::Unknown
            )
        });
    let canary_complete = !profiles.is_empty()
        && profiles.iter().all(|profile| {
            !matches!(
                profile.classification,
                Classification::RequiresProbe
                    | Classification::CatalogDrift
                    | Classification::BlockedAuth
                    | Classification::BlockedQuota
                    | Classification::BlockedBilling
                    | Classification::BlockedRegion
                    | Classification::BlockedRateLimit
                    | Classification::BlockedConfiguration
                    | Classification::BlockedBudget
                    | Classification::Unknown
            )
        });
    let complete = aggregate.valid
        && !budget_truncated
        && if mode == LiveMode::Canary {
            canary_complete
        } else {
            full_complete
        };
    let accounting_passed = mode == LiveMode::Canary
        || accounting.iter().all(|entry| {
            matches!(
                entry.classification,
                Classification::Qualified
                    | Classification::NotApplicable
                    | Classification::OmittedByPolicy
                    | Classification::UnsupportedTransport
            )
        });
    let passed = complete
        && profiles.iter().all(|profile| {
            profile.classification.is_qualified()
                || (profile.classification == Classification::UnsupportedTransport
                    && !profile.advertised)
        })
        && accounting_passed;
    ProviderReport {
        provider: plan.provider.clone(),
        catalog_captured_at: snapshot.captured_at,
        catalog_pages: snapshot.page_count,
        catalog_models: snapshot.models.len(),
        catalog_hash: catalog_hash(privacy_key, snapshot),
        catalog_drift: false,
        duration_ms: 0,
        logical_requests: plan.logical_requests,
        maximum_attempts: plan.maximum_attempts,
        maximum_output_tokens: plan.maximum_output_tokens,
        projected_cost_usd: plan.projected_cost_usd,
        effective_concurrency,
        actual_logical_requests: aggregate.actual_logical_requests,
        actual_attempts: aggregate.actual_attempts,
        usage_requests: aggregate.usage_requests,
        usage_complete_requests: aggregate.usage_complete_requests,
        usage_completeness: aggregate.usage_completeness,
        usage: aggregate.usage,
        actual_cost_usd: aggregate.actual_cost_usd,
        blocker_classification: None,
        safe_error_class: None,
        evidence_complete: aggregate.valid,
        budget_truncated,
        accounting,
        profiles,
        complete,
        passed,
    }
}

pub(super) fn mark_catalog_drift(report: &mut ProviderReport) {
    report.catalog_drift = true;
    report.complete = false;
    report.passed = false;
}

pub(super) fn mark_provider_blocker(
    report: &mut ProviderReport,
    classification: Classification,
    safe_error_class: &'static str,
) {
    report.blocker_classification = Some(classification);
    report.safe_error_class = Some(safe_error_class.to_string());
    report.budget_truncated |= classification == Classification::BlockedBudget;
    report.complete = false;
    report.passed = false;
}

pub(super) fn profile_report(
    privacy_key: &ReportPrivacyKey,
    _run_id: &str,
    provider: &str,
    model: &CatalogModel,
    transport: TransportId,
    advertised: bool,
    required_scenarios: Vec<ScenarioId>,
    scenarios: Vec<ScenarioReport>,
    not_applicable_evidence: Vec<ScenarioNotApplicableEvidence>,
    classification: Classification,
) -> ProfileReport {
    let not_applicable_scenarios = not_applicable_evidence
        .iter()
        .map(|evidence| evidence.scenario)
        .collect();
    ProfileReport {
        model_ref: model_reference(privacy_key, provider, model),
        transport,
        advertised,
        classification,
        pricing: model.pricing.clone(),
        required_scenarios,
        scenarios,
        not_applicable_scenarios,
        not_applicable_evidence,
    }
}

fn catalog_hash(privacy_key: &ReportPrivacyKey, snapshot: &CatalogSnapshot) -> String {
    let privacy_safe_models = snapshot
        .models
        .iter()
        .map(|model| {
            let mut model = model.clone();
            if !model.public_identifier {
                model.id = keyed_private_reference(privacy_key, &snapshot.provider, &model.id);
                model.generation_target = model
                    .generation_target
                    .as_deref()
                    .map(|target| keyed_private_reference(privacy_key, &snapshot.provider, target));
                model.aliases = model
                    .aliases
                    .iter()
                    .map(|alias| keyed_private_reference(privacy_key, &snapshot.provider, alias))
                    .collect();
                model.owner = model
                    .owner
                    .as_deref()
                    .map(|owner| keyed_private_reference(privacy_key, &snapshot.provider, owner));
            }
            model
        })
        .collect::<Vec<_>>();
    let mut hasher = Sha256::new();
    hasher.update(snapshot.provider.as_bytes());
    hasher.update([0]);
    hasher.update(
        serde_json::to_vec(&privacy_safe_models)
            .expect("validated catalog models must serialize deterministically"),
    );
    format!("{:x}", hasher.finalize())
}

fn model_reference(privacy_key: &ReportPrivacyKey, provider: &str, model: &CatalogModel) -> String {
    if model.public_identifier {
        return sanitize_identifier(&model.id);
    }
    keyed_private_reference(privacy_key, provider, &model.id)
}

fn keyed_private_reference(privacy_key: &ReportPrivacyKey, provider: &str, value: &str) -> String {
    let mut message = Vec::with_capacity(provider.len() + value.len() + 1);
    message.extend_from_slice(provider.as_bytes());
    message.push(0);
    message.extend_from_slice(value.as_bytes());
    let digest = hmac_sha256(&privacy_key.0, &message);
    format!("private:{}", hex_bytes(&digest[..12]))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut normalized = [0u8; BLOCK_BYTES];
    if key.len() > BLOCK_BYTES {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK_BYTES];
    let mut outer_pad = [0x5cu8; BLOCK_BYTES];
    for index in 0..BLOCK_BYTES {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn sanitize_identifier(value: &str) -> String {
    let mut rendered = String::new();
    for character in value.chars() {
        if character.is_ascii_graphic() {
            rendered.push(character);
        } else {
            rendered.extend(character.escape_default());
        }
        if rendered.len() >= MAX_SAFE_MODEL_REF_BYTES {
            rendered.truncate(MAX_SAFE_MODEL_REF_BYTES.saturating_sub(3));
            rendered.push_str("...");
            break;
        }
    }
    rendered
}

pub(super) fn publish(
    settings: &LiveSettings,
    report: QualificationReport,
) -> Result<PublishedReport, String> {
    publish_with_hook(settings, report, |_, _, _, _| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationPoint {
    AfterDirectoryAnchor,
    BeforeFirstPublication,
    BetweenPublications,
    BeforeVisibleRevalidation,
}

fn publish_with_hook(
    settings: &LiveSettings,
    report: QualificationReport,
    mut hook: impl FnMut(PublicationPoint, &Path, &Path, &Path) -> Result<(), String>,
) -> Result<PublishedReport, String> {
    validate_report_consistency(&report)?;
    let json = serde_json::to_vec_pretty(&report)
        .map_err(|_| "failed to serialize qualification report".to_string())?;
    let markdown = markdown_summary(&report).into_bytes();
    for (label, bytes) in [("JSON", json.as_slice()), ("Markdown", markdown.as_slice())] {
        if bytes.len() > MAX_REPORT_BYTES {
            return Err(format!(
                "qualification {label} report exceeds its byte limit"
            ));
        }
        scan_artifact(bytes, settings.sensitive_values())?;
    }

    let json_path = settings.results_dir.join(format!("{}.json", report.run_id));
    let markdown_path = settings.results_dir.join(format!("{}.md", report.run_id));
    let json_name = direct_child_name(&json_path)?;
    let markdown_name = direct_child_name(&markdown_path)?;
    let directory = AnchoredResultsDirectory::open_or_create(&settings.results_dir)?;
    hook(
        PublicationPoint::AfterDirectoryAnchor,
        &settings.results_dir,
        &json_path,
        &markdown_path,
    )?;

    let json_staged = directory.stage(&json)?;
    let markdown_staged = match directory.stage(&markdown) {
        Ok(staged) => staged,
        Err(error) => {
            let cleanup = directory.cleanup_staged(&json_staged);
            return Err(combine_publication_errors(error, cleanup.err()));
        }
    };
    let mut json_publication = None;
    let mut markdown_publication = None;
    let publication = (|| {
        hook(
            PublicationPoint::BeforeFirstPublication,
            &settings.results_dir,
            &json_path,
            &markdown_path,
        )?;
        json_publication = Some(directory.publish_staged(&json_staged, &json_name)?);
        let json_receipt = json_publication
            .as_ref()
            .ok_or_else(|| "JSON publication ownership receipt is missing".to_string())?;
        directory.verify_publication(json_receipt, &json, settings.sensitive_values())?;
        hook(
            PublicationPoint::BetweenPublications,
            &settings.results_dir,
            &json_path,
            &markdown_path,
        )?;
        markdown_publication = Some(directory.publish_staged(&markdown_staged, &markdown_name)?);
        let markdown_receipt = markdown_publication
            .as_ref()
            .ok_or_else(|| "Markdown publication ownership receipt is missing".to_string())?;
        directory.verify_publication(markdown_receipt, &markdown, settings.sensitive_values())?;
        hook(
            PublicationPoint::BeforeVisibleRevalidation,
            &settings.results_dir,
            &json_path,
            &markdown_path,
        )?;
        directory.verify_visible()?;
        directory.verify_publication(
            json_publication
                .as_ref()
                .ok_or_else(|| "JSON publication ownership receipt is missing".to_string())?,
            &json,
            settings.sensitive_values(),
        )?;
        directory.verify_publication(
            markdown_publication
                .as_ref()
                .ok_or_else(|| "Markdown publication ownership receipt is missing".to_string())?,
            &markdown,
            settings.sensitive_values(),
        )?;
        directory.cleanup_staged(&json_staged)?;
        directory.cleanup_staged(&markdown_staged)?;
        directory.verify_visible()?;
        Ok(PublishedReport {
            json: json_path,
            markdown: markdown_path,
            passed: report.passed,
        })
    })();

    match publication {
        Ok(published) => Ok(published),
        Err(error) => {
            let mut cleanup_errors = Vec::new();
            if let Some(publication) = markdown_publication.as_ref() {
                if let Err(cleanup) = directory.rollback_publication(publication) {
                    cleanup_errors.push(cleanup);
                }
            }
            if let Some(publication) = json_publication.as_ref() {
                if let Err(cleanup) = directory.rollback_publication(publication) {
                    cleanup_errors.push(cleanup);
                }
            }
            for staged in [&json_staged, &markdown_staged] {
                if let Err(cleanup) = directory.cleanup_staged(staged) {
                    cleanup_errors.push(cleanup);
                }
            }
            let cleanup = (!cleanup_errors.is_empty()).then(|| cleanup_errors.join("; "));
            Err(combine_publication_errors(error, cleanup))
        }
    }
}

fn validate_report_consistency(report: &QualificationReport) -> Result<(), String> {
    if report.run_id.is_empty()
        || report.run_id.len() > MAX_RUN_ID_BYTES
        || !report
            .run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("qualification report run ID is not a safe filename".to_string());
    }
    if !(1..=super::config::MAX_LIVE_CONCURRENCY)
        .contains(&report.budget_limits.configured_concurrency)
        || report.budget_limits.max_logical_requests == 0
        || report.budget_limits.max_attempts == 0
        || report.budget_limits.max_output_tokens_per_request == 0
        || report.budget_limits.max_output_tokens == 0
        || report.budget_limits.max_scenario_elapsed_ms == 0
        || report.budget_limits.max_provider_elapsed_ms == 0
        || report.budget_limits.max_run_elapsed_ms == 0
        || !report.budget_limits.max_actual_cost_usd.is_finite()
        || report.budget_limits.max_actual_cost_usd <= 0.0
    {
        return Err("qualification report budget limits are invalid".to_string());
    }
    let aggregate = aggregate_provider_evidence(&report.providers);
    if report.actual_logical_requests != aggregate.actual_logical_requests
        || report.actual_attempts != aggregate.actual_attempts
        || report.usage_requests != aggregate.usage_requests
        || report.usage_complete_requests != aggregate.usage_complete_requests
        || report.usage_completeness != aggregate.usage_completeness
        || report.usage != aggregate.usage
        || !same_cost(report.actual_cost_usd, aggregate.actual_cost_usd)
        || report.evidence_complete
            != (aggregate.valid
                && report
                    .providers
                    .iter()
                    .all(|provider| provider.evidence_complete))
        || report.budget_truncated
            != report
                .providers
                .iter()
                .any(|provider| provider.budget_truncated)
    {
        return Err(
            "qualification report aggregate execution evidence is inconsistent".to_string(),
        );
    }
    validate_matrix_plan_evidence(report)?;
    if report.actual_logical_requests > report.budget_limits.max_logical_requests
        || report
            .actual_attempts
            .is_some_and(|attempts| attempts > report.budget_limits.max_attempts)
        || report
            .usage
            .is_some_and(|usage| usage.output_tokens > report.budget_limits.max_output_tokens)
        || report
            .actual_cost_usd
            .is_some_and(|cost| cost > report.budget_limits.max_actual_cost_usd)
    {
        return Err("qualification report actuals exceed configured run budgets".to_string());
    }
    let run_duration_ms = report
        .completed_at
        .signed_duration_since(report.started_at)
        .num_milliseconds();
    if run_duration_ms < 0
        || u64::try_from(run_duration_ms).ok() != Some(report.duration_ms)
        || (report.passed
            && u64::try_from(run_duration_ms)
                .is_ok_and(|duration| duration > report.budget_limits.max_run_elapsed_ms))
    {
        return Err("qualification report elapsed evidence exceeds its run deadline".to_string());
    }
    let complete = !report.providers.is_empty()
        && report.evidence_complete
        && !report.budget_truncated
        && report.providers.iter().all(|provider| provider.complete);
    let passed = complete && report.providers.iter().all(|provider| provider.passed);
    if report.complete != complete || report.passed != passed {
        return Err("qualification report completion state is inconsistent".to_string());
    }
    if report
        .providers
        .iter()
        .any(|provider| provider.passed && !provider.complete)
    {
        return Err("qualification provider cannot pass an incomplete report".to_string());
    }
    match (report.mode, report.selected_suite.as_ref()) {
        (LiveMode::Selected, Some(_)) => {}
        (LiveMode::Selected, None)
            if !report.complete
                && !report.passed
                && report.providers.iter().all(|provider| {
                    provider.blocker_classification == Some(Classification::BlockedConfiguration)
                        && provider.profiles.is_empty()
                }) => {}
        (LiveMode::Selected, None) | (_, Some(_)) => {
            return Err("qualification selected-suite provenance is inconsistent".to_string())
        }
        (_, None) => {}
    }

    let mut providers = BTreeSet::new();
    for provider in &report.providers {
        if !super::NETWORK_PROVIDERS.contains(&provider.provider.as_str())
            || !providers.insert(provider.provider.as_str())
        {
            return Err(
                "qualification report provider identities must be supported and unique".to_string(),
            );
        }
        if provider.accounting.len() != provider.catalog_models {
            return Err(
                "qualification provider accounting does not match its catalog denominator"
                    .to_string(),
            );
        }
        validate_provider_execution_evidence(provider, &report.budget_limits)?;
        match (
            provider.blocker_classification,
            provider.safe_error_class.as_deref(),
        ) {
            (None, None) => {}
            (Some(classification), Some(class))
                if matches!(
                    classification,
                    Classification::BlockedAuth
                        | Classification::BlockedQuota
                        | Classification::BlockedBilling
                        | Classification::BlockedRegion
                        | Classification::BlockedRateLimit
                        | Classification::BlockedConfiguration
                        | Classification::BlockedBudget
                        | Classification::Unknown
                ) && !class.is_empty()
                    && class.len() <= 128
                    && !class.chars().any(char::is_control)
                    && !provider.complete
                    && !provider.passed
                    && (classification != Classification::BlockedBudget
                        || provider.budget_truncated) => {}
            _ => return Err("qualification provider blocker evidence is inconsistent".to_string()),
        }
        let mut accounting = BTreeSet::new();
        if provider.accounting.iter().any(|entry| {
            entry.model_ref.is_empty()
                || entry.model_ref.len() > MAX_SAFE_MODEL_REF_BYTES
                || !accounting.insert(entry.model_ref.as_str())
        }) {
            return Err(
                "qualification provider accounting identities must be bounded and unique"
                    .to_string(),
            );
        }
        for entry in &provider.accounting {
            let valid_justification = match entry.classification {
                Classification::NotApplicable => {
                    entry.justification == AccountingJustification::CatalogTextGenerationUnsupported
                }
                Classification::OmittedByPolicy => matches!(
                    entry.justification,
                    AccountingJustification::CanaryDefaultPolicy
                        | AccountingJustification::SelectedMatrixPolicy
                        | AccountingJustification::OpenRouterAllowlistPolicy
                ),
                Classification::RequiresProbe => {
                    entry.justification == AccountingJustification::CatalogMetadataRequiresProbe
                }
                _ => entry.justification == AccountingJustification::QualificationProfileEvidence,
            };
            if !valid_justification {
                return Err(
                    "qualification catalog accounting justification is inconsistent".to_string(),
                );
            }
            if entry.classification == Classification::OmittedByPolicy {
                let mode_allows_policy_omission = match entry.justification {
                    AccountingJustification::CanaryDefaultPolicy => report.mode == LiveMode::Canary,
                    AccountingJustification::SelectedMatrixPolicy => {
                        report.mode == LiveMode::Selected && provider.provider != "openrouter"
                    }
                    AccountingJustification::OpenRouterAllowlistPolicy => {
                        provider.provider == "openrouter"
                            && matches!(
                                report.mode,
                                LiveMode::Canary | LiveMode::Selected | LiveMode::Full
                            )
                    }
                    _ => false,
                };
                if !mode_allows_policy_omission {
                    return Err(
                        "qualification catalog policy omission is invalid for this mode/provider"
                            .to_string(),
                    );
                }
            }
        }

        if provider.passed {
            if provider.catalog_drift
                || provider.catalog_pages == 0
                || provider.catalog_models == 0
                || provider.catalog_hash.len() != 64
                || !provider
                    .catalog_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(
                    "passing qualification provider has incomplete catalog evidence".to_string(),
                );
            }
            match report.mode {
                LiveMode::Catalog => {
                    if !provider.profiles.is_empty()
                        || provider
                            .accounting
                            .iter()
                            .any(|entry| !entry.classification.is_pass_for_catalog())
                    {
                        return Err(
                            "passing catalog report has inconsistent qualification evidence"
                                .to_string(),
                        );
                    }
                }
                LiveMode::Canary | LiveMode::Selected | LiveMode::Full => {
                    validate_passing_generation_provider(provider, report.mode)?;
                }
            }
        }
    }
    Ok(())
}

fn same_cost(left: Option<f64>, right: Option<f64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.is_finite() && right.is_finite() && left.to_bits() == right.to_bits()
        }
        (None, None) => true,
        _ => false,
    }
}

fn validate_matrix_plan_evidence(report: &QualificationReport) -> Result<(), String> {
    let logical_requests = report
        .providers
        .iter()
        .try_fold(0usize, |total, provider| {
            total.checked_add(provider.logical_requests)
        })
        .ok_or_else(|| "qualification matrix request denominator overflowed".to_string())?;
    let maximum_attempts = report
        .providers
        .iter()
        .try_fold(0usize, |total, provider| {
            total.checked_add(provider.maximum_attempts)
        })
        .ok_or_else(|| "qualification matrix attempt denominator overflowed".to_string())?;
    let maximum_output_tokens = report
        .providers
        .iter()
        .try_fold(0usize, |total, provider| {
            total.checked_add(provider.maximum_output_tokens)
        })
        .ok_or_else(|| "qualification matrix output denominator overflowed".to_string())?;
    let projected_cost = if report
        .providers
        .iter()
        .all(|provider| provider.projected_cost_usd.is_some())
    {
        checked_cost_sum(
            report
                .providers
                .iter()
                .filter_map(|provider| provider.projected_cost_usd),
        )
    } else {
        None
    };
    let exceeds_budget = logical_requests > report.budget_limits.max_logical_requests
        || maximum_attempts > report.budget_limits.max_attempts
        || u64::try_from(maximum_output_tokens)
            .ok()
            .is_none_or(|tokens| tokens > report.budget_limits.max_output_tokens)
        || projected_cost.is_some_and(|cost| cost > report.budget_limits.max_actual_cost_usd);
    if exceeds_budget
        && (report.passed
            || report.complete
            || !report.providers.iter().any(|provider| {
                provider.blocker_classification == Some(Classification::BlockedBudget)
            }))
    {
        return Err(
            "over-budget qualification matrix lacks bounded blocked-budget evidence".to_string(),
        );
    }
    Ok(())
}

fn validate_provider_execution_evidence(
    provider: &ProviderReport,
    limits: &BudgetLimitsReport,
) -> Result<(), String> {
    let planned_logical_requests = provider
        .profiles
        .iter()
        .flat_map(|profile| profile.required_scenarios.iter())
        .try_fold(0usize, |total, scenario| {
            total.checked_add(scenario.logical_requests())
        })
        .ok_or_else(|| "qualification provider planned request evidence overflowed".to_string())?;
    if !provider.profiles.is_empty()
        && (planned_logical_requests != provider.logical_requests
            || provider.maximum_attempts
                != provider
                    .logical_requests
                    .checked_mul(MAX_ATTEMPTS_PER_LOGICAL_REQUEST)
                    .ok_or_else(|| {
                        "qualification provider planned attempt evidence overflowed".to_string()
                    })?
            || provider.maximum_output_tokens
                != provider
                    .logical_requests
                    .checked_mul(
                        usize::try_from(limits.max_output_tokens_per_request).map_err(|_| {
                            "qualification request output limit does not fit this platform"
                                .to_string()
                        })?,
                    )
                    .ok_or_else(|| {
                        "qualification provider planned output evidence overflowed".to_string()
                    })?)
    {
        return Err("qualification provider planned denominators are inconsistent".to_string());
    }
    let aggregate = aggregate_scenario_evidence(
        provider
            .profiles
            .iter()
            .flat_map(|profile| profile.scenarios.iter()),
    );
    let budget_truncated = provider
        .profiles
        .iter()
        .flat_map(|profile| profile.scenarios.iter())
        .any(|scenario| scenario.budget_blocked);
    let terminally_skipped_requests = provider
        .profiles
        .iter()
        .flat_map(|profile| profile.scenarios.iter())
        .filter(|scenario| scenario.not_executed_reason.is_some())
        .try_fold(0usize, |total, scenario| {
            total.checked_add(scenario.scenario.logical_requests())
        })
        .ok_or_else(|| "qualification skipped request evidence overflowed".to_string())?;
    if provider.actual_logical_requests != aggregate.actual_logical_requests
        || provider.actual_attempts != aggregate.actual_attempts
        || provider.usage_requests != aggregate.usage_requests
        || provider.usage_complete_requests != aggregate.usage_complete_requests
        || provider.usage_completeness != aggregate.usage_completeness
        || provider.usage != aggregate.usage
        || !same_cost(provider.actual_cost_usd, aggregate.actual_cost_usd)
        || provider.evidence_complete != aggregate.valid
        || provider.budget_truncated != budget_truncated
        || provider.effective_concurrency
            != provider.profiles.len().min(limits.configured_concurrency)
    {
        return Err(
            "qualification provider aggregate execution evidence is inconsistent".to_string(),
        );
    }
    if provider.actual_logical_requests > provider.logical_requests
        || provider
            .actual_attempts
            .is_some_and(|attempts| attempts > provider.maximum_attempts)
        || provider.usage.is_some_and(|usage| {
            usize::try_from(usage.output_tokens)
                .ok()
                .is_none_or(|tokens| tokens > provider.maximum_output_tokens)
        })
        || provider.actual_logical_requests > limits.max_logical_requests
        || provider
            .actual_attempts
            .is_some_and(|attempts| attempts > limits.max_attempts)
        || provider
            .usage
            .is_some_and(|usage| usage.output_tokens > limits.max_output_tokens)
        || provider
            .actual_cost_usd
            .is_some_and(|cost| cost > limits.max_actual_cost_usd)
        || (provider.passed && provider.duration_ms > limits.max_provider_elapsed_ms)
    {
        return Err("qualification provider actuals exceed plan or configured budgets".to_string());
    }
    if provider.budget_truncated && (provider.complete || provider.passed) {
        return Err(
            "budget-truncated qualification provider cannot be complete or passed".to_string(),
        );
    }
    if provider.passed
        && (!provider.evidence_complete
            || provider
                .actual_logical_requests
                .checked_add(terminally_skipped_requests)
                != Some(provider.logical_requests)
            || provider.actual_attempts.is_none())
    {
        return Err(
            "passing qualification provider is missing actual execution evidence".to_string(),
        );
    }
    for profile in &provider.profiles {
        validate_pricing(profile.pricing.as_ref())?;
        let mandatory = BTreeSet::from([ScenarioId::CompleteText, ScenarioId::StreamedText]);
        let required = profile
            .required_scenarios
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let scenarios = profile
            .scenarios
            .iter()
            .map(|scenario| scenario.scenario)
            .collect::<BTreeSet<_>>();
        let not_applicable = profile
            .not_applicable_scenarios
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let not_applicable_evidence = profile
            .not_applicable_evidence
            .iter()
            .map(|evidence| evidence.scenario)
            .collect::<BTreeSet<_>>();
        if required.len() != profile.required_scenarios.len()
            || scenarios.len() != profile.scenarios.len()
            || not_applicable.len() != profile.not_applicable_scenarios.len()
            || not_applicable_evidence.len() != profile.not_applicable_evidence.len()
            || not_applicable != not_applicable_evidence
            || required != scenarios
            || !required.is_disjoint(&not_applicable)
            || !mandatory.is_subset(&required)
            || (profile.advertised && !required.contains(&ScenarioId::SingleToolContinuation))
        {
            return Err(
                "qualification profile required-scenario evidence is inconsistent".to_string(),
            );
        }
        if profile.not_applicable_evidence.iter().any(|evidence| {
            evidence.scenario != ScenarioId::ParallelToolContinuation
                || !matches!(
                    evidence.justification,
                    ScenarioNotApplicableJustification::CatalogParallelToolsUnsupported
                        | ScenarioNotApplicableJustification::CatalogParallelToolsNotAdvertised
                )
        }) {
            return Err(
                "qualification scenario not-applicable evidence is inconsistent".to_string(),
            );
        }
        for scenario in &profile.scenarios {
            validate_scenario_execution_evidence(profile, scenario, limits)?;
        }
        let complete_probe = profile
            .scenarios
            .iter()
            .find(|scenario| scenario.scenario == ScenarioId::CompleteText);
        if profile.classification == Classification::UnsupportedTransport {
            if complete_probe.is_none_or(|scenario| {
                scenario.passed
                    || scenario.not_executed_reason.is_some()
                    || scenario.actual_logical_requests != 1
                    || scenario.http_status.is_none()
                    || scenario.safe_error_class.as_deref() != Some("unsupported_transport")
            }) || profile
                .scenarios
                .iter()
                .filter(|scenario| scenario.scenario != ScenarioId::CompleteText)
                .any(|scenario| {
                    scenario.not_executed_reason
                        != Some(ScenarioNotExecutedReason::TransportUnsupportedByBasicProbe)
                })
            {
                return Err(
                    "qualification unsupported transport did not terminate after its documented basic probe"
                        .to_string(),
                );
            }
        } else if profile
            .scenarios
            .iter()
            .any(|scenario| scenario.not_executed_reason.is_some())
        {
            return Err(
                "qualification non-unsupported profile contains terminally skipped scenarios"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn validate_pricing(pricing: Option<&super::CatalogPricing>) -> Result<(), String> {
    if pricing.is_some_and(|pricing| {
        [
            pricing.prompt_per_token_usd,
            pricing.completion_per_token_usd,
            pricing.request_usd,
        ]
        .into_iter()
        .flatten()
        .any(|price| !price.is_finite() || price < 0.0)
    }) {
        return Err("qualification profile pricing evidence is invalid".to_string());
    }
    Ok(())
}

fn validate_scenario_execution_evidence(
    profile: &ProfileReport,
    scenario: &ScenarioReport,
    limits: &BudgetLimitsReport,
) -> Result<(), String> {
    let expected_requests = scenario.scenario.logical_requests();
    let expected_usage_completeness = UsageCompleteness::from_counts(
        scenario.actual_logical_requests,
        scenario.usage_requests,
        scenario.usage_complete_requests,
    );
    let maximum_actual_attempts = scenario
        .actual_logical_requests
        .checked_mul(MAX_ATTEMPTS_PER_LOGICAL_REQUEST)
        .ok_or_else(|| "qualification scenario attempt evidence overflowed".to_string())?;
    let maximum_actual_output = u64::try_from(scenario.actual_logical_requests)
        .ok()
        .and_then(|requests| requests.checked_mul(u64::from(limits.max_output_tokens_per_request)))
        .ok_or_else(|| "qualification scenario output evidence overflowed".to_string())?;
    if scenario.actual_logical_requests > expected_requests
        || scenario.usage_requests > scenario.actual_logical_requests
        || scenario.usage_complete_requests > scenario.usage_requests
        || scenario.usage_completeness != expected_usage_completeness
        || scenario
            .actual_attempts
            .is_some_and(|attempts| attempts > maximum_actual_attempts)
        || scenario
            .usage
            .is_some_and(|usage| usage.output_tokens > maximum_actual_output)
        || scenario
            .http_status
            .is_some_and(|status| !(100..=599).contains(&status))
        || (scenario.passed
            && (scenario.actual_logical_requests != expected_requests
                || scenario.actual_attempts.is_none()
                || scenario
                    .actual_attempts
                    .is_some_and(|attempts| attempts < expected_requests)
                || scenario.safe_error_class.is_some()
                || scenario.http_status.is_some()
                || scenario.budget_blocked
                || scenario.not_executed_reason.is_some()
                || scenario.duration_ms > limits.max_scenario_elapsed_ms))
        || (!scenario.passed && scenario.safe_error_class.is_none())
        || scenario.safe_error_class.as_ref().is_some_and(|class| {
            class.is_empty() || class.len() > 128 || class.chars().any(char::is_control)
        })
    {
        return Err("qualification scenario request/attempt evidence is inconsistent".to_string());
    }
    if let Some(reason) = scenario.not_executed_reason {
        if reason != ScenarioNotExecutedReason::TransportUnsupportedByBasicProbe
            || profile.classification != Classification::UnsupportedTransport
            || scenario.scenario == ScenarioId::CompleteText
            || scenario.actual_logical_requests != 0
            || scenario.actual_attempts != Some(0)
            || scenario.usage_requests != 0
            || scenario.usage_complete_requests != 0
            || scenario.usage.is_some()
            || scenario.actual_cost_usd != Some(0.0)
            || scenario.http_status.is_some()
            || scenario.budget_blocked
            || scenario.safe_error_class.as_deref() != Some("not_executed_unsupported_transport")
        {
            return Err(
                "qualification unsupported-transport terminal evidence is inconsistent".to_string(),
            );
        }
    }
    match scenario.usage_completeness {
        UsageCompleteness::Complete
            if scenario.actual_logical_requests == 0 || scenario.usage.is_none() =>
        {
            return Err("complete qualification usage evidence is missing".to_string())
        }
        UsageCompleteness::Partial
            if scenario.usage_requests == 0
                || scenario.usage_complete_requests >= scenario.actual_logical_requests
                || scenario.usage.is_none() =>
        {
            return Err("partial qualification usage evidence is inconsistent".to_string())
        }
        UsageCompleteness::Unknown
            if scenario.usage_requests != 0
                || scenario.usage_complete_requests != 0
                || scenario.usage.is_some() =>
        {
            return Err("unknown qualification usage must not claim token totals".to_string())
        }
        _ => {}
    }
    let expected_cost = if scenario.actual_logical_requests == 0 {
        Some(0.0)
    } else if scenario.usage_completeness == UsageCompleteness::Complete {
        calculate_actual_cost(
            profile.pricing.as_ref(),
            scenario.usage,
            scenario.actual_attempts,
        )?
    } else {
        None
    };
    if !same_cost(scenario.actual_cost_usd, expected_cost) {
        return Err("qualification scenario actual-cost evidence is inconsistent".to_string());
    }
    Ok(())
}

fn validate_passing_generation_provider(
    provider: &ProviderReport,
    mode: LiveMode,
) -> Result<(), String> {
    if provider.profiles.is_empty() {
        return Err("passing generation provider has no qualified profiles".to_string());
    }
    let invalid_accounting = provider.accounting.iter().any(|entry| match mode {
        LiveMode::Canary => !matches!(
            entry.classification,
            Classification::Qualified
                | Classification::NotApplicable
                | Classification::OmittedByPolicy
                | Classification::Unknown
        ),
        LiveMode::Selected | LiveMode::Full => !matches!(
            entry.classification,
            Classification::Qualified
                | Classification::NotApplicable
                | Classification::OmittedByPolicy
                | Classification::UnsupportedTransport
        ),
        LiveMode::Catalog => true,
    });
    if invalid_accounting {
        return Err("passing generation provider has unresolved catalog accounting".to_string());
    }

    let mandatory = BTreeSet::from([ScenarioId::CompleteText, ScenarioId::StreamedText]);
    let mut profiles = BTreeSet::new();
    for profile in &provider.profiles {
        let supported_profile = profile.classification.is_qualified()
            || (profile.classification == Classification::UnsupportedTransport
                && !profile.advertised);
        let accounting_matches = provider.accounting.iter().any(|entry| {
            entry.model_ref == profile.model_ref
                && match profile.classification {
                    Classification::Qualified => entry.classification == Classification::Qualified,
                    Classification::UnsupportedTransport => matches!(
                        entry.classification,
                        Classification::Qualified | Classification::UnsupportedTransport
                    ),
                    _ => false,
                }
        });
        if profile.model_ref.is_empty()
            || profile.model_ref.len() > MAX_SAFE_MODEL_REF_BYTES
            || !profiles.insert((profile.model_ref.as_str(), profile.transport))
            || !supported_profile
            || !accounting_matches
        {
            return Err(
                "passing generation profiles must be bounded, unique, and qualified".to_string(),
            );
        }
        let scenarios = profile
            .scenarios
            .iter()
            .map(|scenario| scenario.scenario)
            .collect::<BTreeSet<_>>();
        let required = profile
            .required_scenarios
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let not_applicable = profile
            .not_applicable_scenarios
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let not_applicable_evidence = profile
            .not_applicable_evidence
            .iter()
            .map(|evidence| evidence.scenario)
            .collect::<BTreeSet<_>>();
        if profile.classification == Classification::UnsupportedTransport {
            continue;
        }
        if scenarios.len() != profile.scenarios.len()
            || required.len() != profile.required_scenarios.len()
            || not_applicable.len() != profile.not_applicable_scenarios.len()
            || not_applicable_evidence.len() != profile.not_applicable_evidence.len()
            || not_applicable != not_applicable_evidence
            || profile
                .scenarios
                .iter()
                .any(|scenario| !scenario.passed || scenario.safe_error_class.is_some())
            || !mandatory.is_subset(&scenarios)
            || scenarios != required
            || !scenarios.is_disjoint(&not_applicable)
            || (profile.advertised && !scenarios.contains(&ScenarioId::SingleToolContinuation))
            || (mode == LiveMode::Selected
                && (!scenarios.contains(&ScenarioId::SingleToolContinuation)
                    || usize::from(scenarios.contains(&ScenarioId::ParallelToolContinuation))
                        + usize::from(
                            not_applicable.contains(&ScenarioId::ParallelToolContinuation),
                        )
                        != 1))
        {
            return Err(
                "passing generation profile has incomplete or inconsistent scenario evidence"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn direct_child_name(path: &Path) -> Result<OsString, String> {
    let name = path
        .file_name()
        .ok_or_else(|| "qualification report target has no filename".to_string())?;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("qualification report target is not a direct child".to_string());
    }
    Ok(name.to_os_string())
}

fn combine_publication_errors(primary: String, cleanup: Option<String>) -> String {
    cleanup.map_or(primary.clone(), |cleanup| {
        format!("{primary}; cleanup preserved ambiguous entries: {cleanup}")
    })
}

fn markdown_summary(report: &QualificationReport) -> String {
    let attempts = report
        .actual_attempts
        .map_or_else(|| "unknown".to_string(), |attempts| attempts.to_string());
    let usage = markdown_usage(report.usage_completeness, report.usage);
    let cost = markdown_cost(report.actual_cost_usd);
    let mut output = format!(
        "# Live LLM qualification {}\n\n- Revision: `{}`\n- Mode: `{:?}`\n- Configured concurrency: `{}`\n- Actual logical requests: `{}`\n- Actual attempts: `{attempts}`\n- Usage: `{usage}`\n- Actual cost: `{cost}`\n- Duration: `{}` ms\n- Budget truncated: `{}`\n- Complete: `{}`\n- Passed: `{}`\n\n",
        report.run_id,
        report.source_revision,
        report.mode,
        report.budget_limits.configured_concurrency,
        report.actual_logical_requests,
        report.duration_ms,
        report.budget_truncated,
        report.complete,
        report.passed
    );
    if let Some(suite) = &report.selected_suite {
        output.push_str(&format!(
            "- Selected suite: `{}`\n- Matrix SHA-256: `{}`\n- Matrix review: `{}` through `{}`\n- Required tasks: `{}`\n- Conditional tasks: `{}`\n\n",
            suite.suite_id,
            suite.matrix_sha256,
            suite.reviewed_at,
            suite.expires_at,
            suite.required_task_count,
            suite.conditional_task_count
        ));
    }
    for provider in &report.providers {
        let attempts = provider
            .actual_attempts
            .map_or_else(|| "unknown".to_string(), |attempts| attempts.to_string());
        let usage = markdown_usage(provider.usage_completeness, provider.usage);
        let cost = markdown_cost(provider.actual_cost_usd);
        output.push_str(&format!(
            "## {}\n\n- Catalog: {} models across {} page(s)\n- Planned logical requests: {}\n- Maximum attempts: {}\n- Effective concurrency: {}\n- Actual logical requests: {}\n- Actual attempts: `{attempts}`\n- Usage: `{usage}`\n- Actual cost: `{cost}`\n- Duration: {} ms\n- Budget truncated: `{}`\n- Complete: `{}`\n- Passed: `{}`\n\n",
            provider.provider,
            provider.catalog_models,
            provider.catalog_pages,
            provider.logical_requests,
            provider.maximum_attempts,
            provider.effective_concurrency,
            provider.actual_logical_requests,
            provider.duration_ms,
            provider.budget_truncated,
            provider.complete,
            provider.passed
        ));
        if provider.catalog_drift {
            output.push_str("- Catalog drift: `true`\n\n");
        }
        for profile in &provider.profiles {
            output.push_str(&format!(
                "- `{}` / `{}`: `{:?}`\n",
                profile.model_ref,
                profile.transport.as_str(),
                profile.classification
            ));
            for scenario in &profile.scenarios {
                let attempts = scenario
                    .actual_attempts
                    .map_or_else(|| "unknown".to_string(), |attempts| attempts.to_string());
                output.push_str(&format!(
                    "  - `{:?}`: `{}` ({} request(s), {attempts} attempt(s), {} ms{})\n",
                    scenario.scenario,
                    scenario.passed,
                    scenario.actual_logical_requests,
                    scenario.duration_ms,
                    scenario
                        .not_executed_reason
                        .map_or_else(String::new, |reason| format!(", not executed: {reason:?}"))
                ));
            }
            for evidence in &profile.not_applicable_evidence {
                output.push_str(&format!(
                    "  - `{:?}`: `not_applicable` (`{:?}`)\n",
                    evidence.scenario, evidence.justification
                ));
            }
        }
        output.push('\n');
    }
    output
}

fn markdown_usage(completeness: UsageCompleteness, usage: Option<LlmUsage>) -> String {
    match usage {
        Some(usage) => format!(
            "{:?}; input={}, output={}, total={}",
            completeness, usage.input_tokens, usage.output_tokens, usage.total_tokens
        ),
        None => format!("{:?}", completeness),
    }
}

fn markdown_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "unknown".to_string(), |cost| format!("${cost:.9}"))
}

fn scan_artifact(bytes: &[u8], sensitive_values: &[String]) -> Result<(), String> {
    for sensitive in sensitive_values {
        for variant in sensitive_variants(sensitive) {
            if !variant.is_empty() && find_bytes(bytes, variant.as_bytes()) {
                return Err(
                    "qualification artifact contains a configured sensitive value; publication suppressed"
                        .to_string(),
                );
            }
        }
    }
    Ok(())
}

fn sensitive_variants(value: &str) -> Vec<String> {
    let mut variants = vec![value.to_string(), value.trim().to_string()];
    let encoded_upper = percent_encode(value.as_bytes(), false);
    let encoded_lower = percent_encode(value.as_bytes(), true);
    variants.push(encoded_upper.clone());
    variants.push(encoded_lower.clone());
    variants.push(percent_encode(encoded_upper.as_bytes(), false));
    variants.push(percent_encode(encoded_lower.as_bytes(), true));
    if let Ok(json) = serde_json::to_string(value) {
        variants.push(json.trim_matches('"').to_string());
    }
    variants.push(base64_encode(
        value.as_bytes(),
        Base64Alphabet::Standard,
        true,
    ));
    variants.push(base64_encode(
        value.as_bytes(),
        Base64Alphabet::Standard,
        false,
    ));
    variants.push(base64_encode(
        value.as_bytes(),
        Base64Alphabet::UrlSafe,
        true,
    ));
    variants.push(base64_encode(
        value.as_bytes(),
        Base64Alphabet::UrlSafe,
        false,
    ));
    variants.sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    variants.dedup();
    variants
}

#[derive(Clone, Copy)]
enum Base64Alphabet {
    Standard,
    UrlSafe,
}

fn base64_encode(value: &[u8], alphabet: Base64Alphabet, padded: bool) -> String {
    let table = match alphabet {
        Base64Alphabet::Standard => {
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        }
        Base64Alphabet::UrlSafe => {
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        }
    };
    let mut encoded = String::with_capacity(value.len().div_ceil(3) * 4);
    for chunk in value.chunks(3) {
        let bits = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        encoded.push(table[((bits >> 18) & 0x3f) as usize] as char);
        encoded.push(table[((bits >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(table[((bits >> 6) & 0x3f) as usize] as char);
        } else if padded {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(table[(bits & 0x3f) as usize] as char);
        } else if padded {
            encoded.push('=');
        }
    }
    encoded
}

fn percent_encode(value: &[u8], lowercase_hex: bool) -> String {
    let mut encoded = String::new();
    for byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            if lowercase_hex {
                encoded.push_str(&format!("{byte:02x}"));
            } else {
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

struct AnchoredResultsDirectory {
    visible_path: PathBuf,
    directory: cap_std::fs::Dir,
    identity: FileIdentity,
}

struct StagedFile {
    name: OsString,
    file: File,
}

struct OwnedPublication {
    name: OsString,
    file: File,
}

impl AnchoredResultsDirectory {
    fn open_or_create(path: &Path) -> Result<Self, String> {
        let (visible_path, directory) = walk_results_directory(path, true)?;
        let identity = directory_identity(&directory, &visible_path)?;
        let anchored = Self {
            visible_path,
            directory,
            identity,
        };
        anchored.verify_visible()?;
        Ok(anchored)
    }

    fn verify_visible(&self) -> Result<(), String> {
        let (_, visible) = walk_results_directory(&self.visible_path, false)?;
        let identity = directory_identity(&visible, &self.visible_path)?;
        if identity != self.identity {
            return Err(
                "live results directory identity changed during report publication".to_string(),
            );
        }
        Ok(())
    }

    fn stage(&self, bytes: &[u8]) -> Result<StagedFile, String> {
        let name = OsString::from(format!(".llm-live-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        configure_file_options(&mut options, true, true);
        let file = self
            .directory
            .open_with(Path::new(&name), &options)
            .map_err(|_| "failed to create qualification report staging file".to_string())?;
        let mut staged = StagedFile {
            name,
            file: file.into_std(),
        };
        let staged_result = staged
            .file
            .write_all(bytes)
            .and_then(|_| staged.file.sync_all())
            .map_err(|_| "failed to durably write qualification report".to_string())
            .and_then(|()| self.verify_owned_alias(&staged.name, &staged.file))
            .and_then(|()| self.sync());
        match staged_result {
            Ok(()) => Ok(staged),
            Err(error) => {
                let cleanup = self.cleanup_staged(&staged).err();
                Err(combine_publication_errors(error, cleanup))
            }
        }
    }

    fn publish_staged(
        &self,
        staged: &StagedFile,
        destination: &OsStr,
    ) -> Result<OwnedPublication, String> {
        self.require_missing(destination)?;
        self.verify_owned_alias(&staged.name, &staged.file)?;
        let publication = OwnedPublication {
            name: destination.to_os_string(),
            file: staged
                .file
                .try_clone()
                .map_err(|_| "failed to retain qualification report ownership".to_string())?,
        };
        publish_open_file_no_replace(
            &self.directory,
            Path::new(&staged.name),
            &staged.file,
            Path::new(destination),
        )
        .map_err(|_| "failed to publish qualification report atomically".to_string())?;
        match self
            .sync()
            .and_then(|()| self.verify_owned_alias(&publication.name, &publication.file))
        {
            Ok(()) => Ok(publication),
            Err(error) => {
                let cleanup = self.rollback_publication(&publication).err();
                Err(combine_publication_errors(error, cleanup))
            }
        }
    }

    fn verify_publication(
        &self,
        publication: &OwnedPublication,
        expected_bytes: &[u8],
        sensitive_values: &[String],
    ) -> Result<(), String> {
        self.verify_owned_alias(&publication.name, &publication.file)?;
        let actual = read_open_file_bounded(&publication.file, MAX_REPORT_BYTES)?;
        if actual != expected_bytes {
            return Err(
                "qualification report changed or was truncated during publication".to_string(),
            );
        }
        scan_artifact(&actual, sensitive_values)?;
        self.verify_owned_alias(&publication.name, &publication.file)
    }

    fn rollback_publication(&self, publication: &OwnedPublication) -> Result<(), String> {
        self.cleanup_owned_alias(&publication.name, &publication.file)
    }

    fn cleanup_staged(&self, staged: &StagedFile) -> Result<(), String> {
        self.cleanup_owned_alias(&staged.name, &staged.file)
    }

    fn cleanup_owned_alias(&self, name: &OsStr, expected: &File) -> Result<(), String> {
        if !self.entry_exists(name)? {
            return Ok(());
        }
        let quarantine = OsString::from(format!(".llm-live-cleanup-{}.tmp", uuid::Uuid::new_v4()));
        rename_alias_no_replace(
            &self.directory,
            Path::new(name),
            expected,
            Path::new(&quarantine),
        )
        .map_err(|_| "failed to quarantine an owned report alias".to_string())?;
        self.sync()?;

        match self.open_regular(&quarantine) {
            Ok(quarantined) if same_file_identity(&quarantined, expected)? => {
                self.directory
                    .remove_file(Path::new(&quarantine))
                    .map_err(|_| "failed to remove an owned report alias".to_string())?;
                self.sync()?;
                if self.entry_exists(name)? {
                    return Err(
                        "an unowned replacement appeared during report cleanup and was preserved"
                            .to_string(),
                    );
                }
                Ok(())
            }
            Ok(quarantined) => {
                let restore = rename_alias_no_replace(
                    &self.directory,
                    Path::new(&quarantine),
                    &quarantined,
                    Path::new(name),
                );
                let _ = self.sync();
                Err(if restore.is_ok() {
                    "an unowned report replacement was restored and preserved during cleanup"
                        .to_string()
                } else {
                    "an unowned report replacement was preserved in cleanup quarantine".to_string()
                })
            }
            Err(identity_error) => {
                let restore = rename_alias_no_replace(
                    &self.directory,
                    Path::new(&quarantine),
                    expected,
                    Path::new(name),
                );
                let _ = self.sync();
                Err(if restore.is_ok() {
                    format!(
                        "an unowned report replacement was restored after identity validation failed: {identity_error}"
                    )
                } else {
                    format!(
                        "an unowned report replacement was preserved in cleanup quarantine: {identity_error}"
                    )
                })
            }
        }
    }

    fn verify_owned_alias(&self, name: &OsStr, expected: &File) -> Result<(), String> {
        let visible = self.open_regular(name)?;
        if !same_file_identity(&visible, expected)? {
            return Err("qualification report file identity changed".to_string());
        }
        Ok(())
    }

    fn open_regular(&self, name: &OsStr) -> Result<File, String> {
        let path = Path::new(name);
        let before = self
            .directory
            .symlink_metadata(path)
            .map_err(|_| "failed to inspect qualification report entry".to_string())?;
        if before.is_symlink() || !before.is_file() {
            return Err(
                "qualification report entry must be a regular file, not a link".to_string(),
            );
        }
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        configure_file_options(&mut options, false, false);
        let file = self
            .directory
            .open_with(path, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|_| "failed to open qualification report entry".to_string())?;
        let after = self
            .directory
            .symlink_metadata(path)
            .map_err(|_| "failed to re-inspect qualification report entry".to_string())?;
        if after.is_symlink() || !after.is_file() {
            return Err("qualification report entry changed while it was opened".to_string());
        }
        let reopened = self
            .directory
            .open_with(path, &options)
            .map(cap_std::fs::File::into_std)
            .map_err(|_| "failed to re-open qualification report entry".to_string())?;
        if !same_file_identity(&file, &reopened)? {
            return Err("qualification report entry changed while it was opened".to_string());
        }
        Ok(file)
    }

    fn require_missing(&self, name: &OsStr) -> Result<(), String> {
        if self.entry_exists(name)? {
            Err("qualification report target already exists".to_string())
        } else {
            Ok(())
        }
    }

    fn entry_exists(&self, name: &OsStr) -> Result<bool, String> {
        match self.directory.symlink_metadata(Path::new(name)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("failed to inspect qualification report target".to_string()),
        }
    }

    fn sync(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;

            let mut options = cap_std::fs::OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
            self.directory
                .open_with(Path::new("."), &options)
                .map(cap_std::fs::File::into_std)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| "failed to sync live results directory".to_string())
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Wdk::Storage::FileSystem::NtFlushBuffersFile;
            use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
            use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

            let mut io_status = IO_STATUS_BLOCK::default();
            let status =
                unsafe { NtFlushBuffersFile(self.directory.as_raw_handle(), &mut io_status) };
            if status >= 0 {
                Ok(())
            } else {
                let code = unsafe { RtlNtStatusToDosError(status) };
                Err(format!(
                    "failed to sync live results directory (OS error {code})"
                ))
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err("this platform cannot durably sync the live results directory".to_string())
        }
    }
}

fn walk_results_directory(
    requested: &Path,
    create: bool,
) -> Result<(PathBuf, cap_std::fs::Dir), String> {
    let absolute = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| "failed to resolve live results directory".to_string())?
            .join(requested)
    };
    let mut root = PathBuf::new();
    let mut children = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => root.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err("live results directory must not contain parent components".to_string())
            }
            Component::Normal(name) => children.push(name.to_os_string()),
        }
    }
    if root.as_os_str().is_empty() || children.is_empty() {
        return Err("live results directory must be below a filesystem root".to_string());
    }
    let mut directory = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())
        .map_err(|_| "failed to anchor live results filesystem root".to_string())?;
    let mut visible = root;
    for child in children {
        let relative = Path::new(&child);
        let metadata = match directory.symlink_metadata(relative) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                match directory.create_dir(relative) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => {
                        return Err("failed to create live results directory component".to_string())
                    }
                }
                directory
                    .symlink_metadata(relative)
                    .map_err(|_| "failed to inspect created results component".to_string())?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err("live results directory disappeared".to_string())
            }
            Err(_) => return Err("failed to inspect live results directory component".to_string()),
        };
        if metadata.is_symlink() || !metadata.is_dir() {
            return Err(
                "live results directory components must be real directories, not links".to_string(),
            );
        }
        let child_directory = directory
            .open_dir(relative)
            .map_err(|_| "failed to open live results directory component".to_string())?;
        let after = directory
            .symlink_metadata(relative)
            .map_err(|_| "failed to re-inspect live results directory component".to_string())?;
        if after.is_symlink() || !after.is_dir() {
            return Err("live results directory component changed while opening".to_string());
        }
        let reopened = directory
            .open_dir(relative)
            .map_err(|_| "failed to re-open live results directory component".to_string())?;
        if directory_identity(&child_directory, &visible.join(&child))?
            != directory_identity(&reopened, &visible.join(&child))?
        {
            return Err("live results directory component identity changed".to_string());
        }
        visible.push(&child);
        directory = child_directory;
    }
    Ok((visible, directory))
}

fn directory_identity(directory: &cap_std::fs::Dir, path: &Path) -> Result<FileIdentity, String> {
    FileIdentity::from_file(
        directory
            .try_clone()
            .map(cap_std::fs::Dir::into_std_file)
            .map_err(|_| format!("failed to retain results directory {}", path.display()))?,
    )
    .map_err(|_| format!("failed to identify results directory {}", path.display()))
}

fn configure_file_options(options: &mut cap_std::fs::OpenOptions, _creating: bool, _write: bool) {
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        if _creating {
            options.mode(0o600);
        }
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let mut access = DELETE | GENERIC_READ;
        if _write {
            access |= GENERIC_WRITE;
        }
        options
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (options, _creating, _write);
    }
}

fn same_file_identity(left: &File, right: &File) -> Result<bool, String> {
    let left = FileIdentity::from_file(
        left.try_clone()
            .map_err(|_| "failed to retain qualification report file".to_string())?,
    )
    .map_err(|_| "failed to identify qualification report file".to_string())?;
    let right = FileIdentity::from_file(
        right
            .try_clone()
            .map_err(|_| "failed to retain qualification report file".to_string())?,
    )
    .map_err(|_| "failed to identify qualification report file".to_string())?;
    Ok(left == right)
}

fn read_open_file_bounded(file: &File, limit: usize) -> Result<Vec<u8>, String> {
    let mut file = file
        .try_clone()
        .map_err(|_| "failed to retain qualification report for verification".to_string())?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "failed to seek qualification report".to_string())?;
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| "failed to verify qualification report bytes".to_string())?;
    if bytes.len() > limit {
        return Err("qualification report exceeds its byte limit".to_string());
    }
    Ok(bytes)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn publish_open_file_no_replace(
    directory: &cap_std::fs::Dir,
    _source: &Path,
    source_file: &File,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let empty = c"";
    let linked = unsafe {
        libc::linkat(
            source_file.as_raw_fd(),
            empty.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if linked == 0 {
        return Ok(());
    }
    let descriptor = CString::new(format!("/proc/self/fd/{}", source_file.as_raw_fd()))
        .expect("numeric descriptor has no NUL");
    let linked = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            descriptor.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if linked == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn publish_open_file_no_replace(
    directory: &cap_std::fs::Dir,
    source: &Path,
    _source_file: &File,
    destination: &Path,
) -> std::io::Result<()> {
    rename_alias_no_replace_unix(directory, source, destination)
}

#[cfg(windows)]
fn publish_open_file_no_replace(
    directory: &cap_std::fs::Dir,
    _source: &Path,
    source_file: &File,
    destination: &Path,
) -> std::io::Result<()> {
    nib::fs_security::rename_open_entry_no_replace_windows(directory, source_file, destination)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn publish_open_file_no_replace(
    _directory: &cap_std::fs::Dir,
    _source: &Path,
    _source_file: &File,
    _destination: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no safe no-replace report publication primitive",
    ))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_alias_no_replace(
    directory: &cap_std::fs::Dir,
    source: &Path,
    _source_file: &File,
    destination: &Path,
) -> std::io::Result<()> {
    rename_alias_no_replace_unix(directory, source, destination)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn rename_alias_no_replace(
    directory: &cap_std::fs::Dir,
    source: &Path,
    _source_file: &File,
    destination: &Path,
) -> std::io::Result<()> {
    rename_alias_no_replace_unix(directory, source, destination)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
fn rename_alias_no_replace_unix(
    directory: &cap_std::fs::Dir,
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_alias_no_replace(
    directory: &cap_std::fs::Dir,
    _source: &Path,
    source_file: &File,
    destination: &Path,
) -> std::io::Result<()> {
    nib::fs_security::rename_open_entry_no_replace_windows(directory, source_file, destination)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
fn rename_alias_no_replace(
    _directory: &cap_std::fs::Dir,
    _source: &Path,
    _source_file: &File,
    _destination: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no safe no-replace report rollback primitive",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_live_support::config::LiveLimits;
    use crate::llm_live_support::plan::{
        build_plan, AccountingEntry, OpenRouterAllowlist, RunPlan,
    };
    use chrono::Utc;
    use std::time::Duration;

    fn limits() -> LiveLimits {
        LiveLimits {
            max_logical_requests: 10_000,
            max_attempts: 30_000,
            max_output_tokens_per_request: 64,
            max_total_output_tokens: 640_000,
            max_actual_cost_usd: 100.0,
            max_scenario_duration: Duration::from_secs(30),
            max_provider_duration: Duration::from_secs(60),
            max_run_duration: Duration::from_secs(120),
            allow_unpriced: true,
        }
    }

    fn settings(results_dir: PathBuf, sensitive_values: Vec<String>) -> LiveSettings {
        LiveSettings {
            mode: LiveMode::Catalog,
            providers: vec!["openai".to_string()],
            concurrency: 1,
            results_dir,
            limits: LiveLimits {
                max_logical_requests: 1,
                max_attempts: 3,
                max_output_tokens_per_request: 1,
                max_total_output_tokens: 1,
                max_actual_cost_usd: 1.0,
                max_scenario_duration: Duration::from_secs(1),
                max_provider_duration: Duration::from_secs(1),
                max_run_duration: Duration::from_secs(1),
                allow_unpriced: false,
            },
            meta_base_url: None,
            sensitive_values,
        }
    }

    fn qualification_settings() -> LiveSettings {
        let mut settings = settings(PathBuf::from("unused-live-results"), Vec::new());
        settings.limits = limits();
        settings
    }

    fn provider_report(complete: bool, passed: bool) -> ProviderReport {
        ProviderReport {
            provider: "openai".to_string(),
            catalog_captured_at: Utc::now(),
            catalog_pages: 1,
            catalog_models: 1,
            catalog_hash: "a".repeat(64),
            catalog_drift: false,
            duration_ms: 0,
            logical_requests: 0,
            maximum_attempts: 0,
            maximum_output_tokens: 0,
            projected_cost_usd: Some(0.0),
            effective_concurrency: 0,
            actual_logical_requests: 0,
            actual_attempts: Some(0),
            usage_requests: 0,
            usage_complete_requests: 0,
            usage_completeness: UsageCompleteness::Unknown,
            usage: None,
            actual_cost_usd: Some(0.0),
            blocker_classification: None,
            safe_error_class: None,
            evidence_complete: true,
            budget_truncated: false,
            accounting: vec![CatalogEntryReport {
                model_ref: "gpt-test".to_string(),
                classification: if passed {
                    Classification::Qualified
                } else {
                    Classification::RequiresProbe
                },
                justification: if passed {
                    AccountingJustification::QualificationProfileEvidence
                } else {
                    AccountingJustification::CatalogMetadataRequiresProbe
                },
            }],
            profiles: Vec::new(),
            complete,
            passed,
        }
    }

    fn passing_scenario(scenario: ScenarioId) -> ScenarioReport {
        let requests = scenario.logical_requests();
        let requests_u64 = requests as u64;
        ScenarioReport {
            scenario,
            passed: true,
            duration_ms: 1,
            actual_logical_requests: requests,
            actual_attempts: Some(requests),
            usage_requests: requests,
            usage_complete_requests: requests,
            usage_completeness: UsageCompleteness::Complete,
            usage: Some(
                LlmUsage::new(requests_u64 * 2, requests_u64, requests_u64 * 3, None, None)
                    .unwrap(),
            ),
            actual_cost_usd: Some(0.0),
            http_status: None,
            budget_blocked: false,
            not_executed_reason: None,
            safe_error_class: None,
        }
    }

    fn report(run_id: &str, complete: bool, passed: bool) -> QualificationReport {
        let now = Utc::now();
        QualificationReport::new(
            run_id.to_string(),
            LiveMode::Catalog,
            now,
            now,
            vec![provider_report(complete, passed)],
            None,
            &qualification_settings(),
        )
    }

    fn passing_generation_report(mode: LiveMode) -> QualificationReport {
        let mut provider = provider_report(true, true);
        provider.accounting[0].classification = Classification::Qualified;
        provider.profiles = vec![ProfileReport {
            model_ref: "gpt-test".to_string(),
            transport: TransportId::Responses,
            advertised: true,
            classification: Classification::Qualified,
            pricing: Some(super::super::CatalogPricing {
                prompt_per_token_usd: Some(0.0),
                completion_per_token_usd: Some(0.0),
                request_usd: Some(0.0),
            }),
            required_scenarios: vec![
                ScenarioId::CompleteText,
                ScenarioId::StreamedText,
                ScenarioId::SingleToolContinuation,
            ],
            scenarios: vec![
                passing_scenario(ScenarioId::CompleteText),
                passing_scenario(ScenarioId::StreamedText),
                passing_scenario(ScenarioId::SingleToolContinuation),
            ],
            not_applicable_scenarios: Vec::new(),
            not_applicable_evidence: Vec::new(),
        }];
        provider.effective_concurrency = 1;
        let aggregate = aggregate_scenario_evidence(
            provider
                .profiles
                .iter()
                .flat_map(|profile| profile.scenarios.iter()),
        );
        provider.logical_requests = aggregate.actual_logical_requests;
        provider.maximum_attempts = aggregate.actual_logical_requests * 3;
        provider.maximum_output_tokens = aggregate.actual_logical_requests * 64;
        provider.actual_logical_requests = aggregate.actual_logical_requests;
        provider.actual_attempts = aggregate.actual_attempts;
        provider.usage_requests = aggregate.usage_requests;
        provider.usage_complete_requests = aggregate.usage_complete_requests;
        provider.usage_completeness = aggregate.usage_completeness;
        provider.usage = aggregate.usage;
        provider.actual_cost_usd = aggregate.actual_cost_usd;
        let now = Utc::now();
        QualificationReport::new(
            "run-1".to_string(),
            mode,
            now,
            now,
            vec![provider],
            None,
            &qualification_settings(),
        )
    }

    fn refresh_provider_evidence(provider: &mut ProviderReport) {
        let aggregate = aggregate_scenario_evidence(
            provider
                .profiles
                .iter()
                .flat_map(|profile| profile.scenarios.iter()),
        );
        provider.logical_requests = provider
            .profiles
            .iter()
            .flat_map(|profile| profile.required_scenarios.iter())
            .map(|scenario| scenario.logical_requests())
            .sum();
        provider.maximum_attempts = provider.logical_requests * 3;
        provider.maximum_output_tokens = provider.logical_requests * 64;
        provider.actual_logical_requests = aggregate.actual_logical_requests;
        provider.actual_attempts = aggregate.actual_attempts;
        provider.usage_requests = aggregate.usage_requests;
        provider.usage_complete_requests = aggregate.usage_complete_requests;
        provider.usage_completeness = aggregate.usage_completeness;
        provider.usage = aggregate.usage;
        provider.actual_cost_usd = aggregate.actual_cost_usd;
        provider.evidence_complete = aggregate.valid;
        provider.budget_truncated = provider
            .profiles
            .iter()
            .flat_map(|profile| profile.scenarios.iter())
            .any(|scenario| scenario.budget_blocked);
    }

    fn qualification_with_provider(
        mode: LiveMode,
        provider: ProviderReport,
    ) -> QualificationReport {
        let now = Utc::now();
        QualificationReport::new(
            "run-1".to_string(),
            mode,
            now,
            now,
            vec![provider],
            None,
            &qualification_settings(),
        )
    }

    fn planned_model(id: &str, supports_tools: bool) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            generation_target: None,
            aliases: Vec::new(),
            supports_text_generation: Some(true),
            supports_tools: Some(supports_tools),
            supports_parallel_tools: Some(false),
            input_modalities: ["text".to_string()].into_iter().collect(),
            output_modalities: ["text".to_string()].into_iter().collect(),
            supported_parameters: Default::default(),
            public_identifier: true,
            owner: Some("provider".to_string()),
            pricing: Some(super::super::CatalogPricing {
                prompt_per_token_usd: Some(0.0),
                completion_per_token_usd: Some(0.0),
                request_usd: Some(0.0),
            }),
            expiration_date: None,
        }
    }

    fn qualification_from_real_plan(
        mode: LiveMode,
        snapshot: &CatalogSnapshot,
    ) -> QualificationReport {
        let directory = tempfile::tempdir().unwrap();
        let mut live_settings = settings(directory.path().join("results"), Vec::new());
        live_settings.mode = mode;
        live_settings.limits.max_logical_requests = 10_000;
        live_settings.limits.max_attempts = 30_000;
        live_settings.limits.max_output_tokens_per_request = 64;
        live_settings.limits.max_total_output_tokens = 640_000;
        live_settings.limits.max_actual_cost_usd = 100.0;
        live_settings.limits.allow_unpriced = true;
        let allowlist = OpenRouterAllowlist::load_default().unwrap();
        let plan = build_plan(&live_settings, snapshot, &allowlist, None).unwrap();
        let privacy_key = ReportPrivacyKey::fixture(7);
        let profiles = plan
            .profiles
            .iter()
            .map(|profile| {
                profile_report(
                    &privacy_key,
                    "run-1",
                    &snapshot.provider,
                    &profile.model,
                    profile.transport,
                    profile.advertised,
                    profile.required_scenarios.iter().copied().collect(),
                    profile
                        .required_scenarios
                        .iter()
                        .map(|scenario| passing_scenario(*scenario))
                        .collect(),
                    profile
                        .not_applicable_scenarios
                        .iter()
                        .map(|(scenario, justification)| ScenarioNotApplicableEvidence {
                            scenario: *scenario,
                            justification: *justification,
                        })
                        .collect(),
                    Classification::Qualified,
                )
            })
            .collect();
        let provider = generation_provider_report(
            &privacy_key,
            "run-1",
            snapshot,
            &plan,
            profiles,
            mode,
            live_settings.concurrency.min(plan.profiles.len()),
        );
        let now = Utc::now();
        QualificationReport::new(
            "run-1".to_string(),
            mode,
            now,
            now,
            vec![provider],
            None,
            &live_settings,
        )
    }

    fn assert_no_internal_aliases(directory: &Path) {
        let aliases = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(".llm-live-"))
            .collect::<Vec<_>>();
        assert!(aliases.is_empty(), "internal aliases remained: {aliases:?}");
    }

    fn private_model() -> CatalogModel {
        CatalogModel {
            id: "ft:customer:private-name".to_string(),
            generation_target: None,
            aliases: Vec::new(),
            supports_text_generation: Some(true),
            supports_tools: None,
            supports_parallel_tools: None,
            input_modalities: ["text".to_string()].into_iter().collect(),
            output_modalities: ["text".to_string()].into_iter().collect(),
            supported_parameters: Default::default(),
            public_identifier: false,
            owner: Some("customer".to_string()),
            pricing: None,
            expiration_date: None,
        }
    }

    #[test]
    fn private_model_reference_is_run_stable_and_pseudonymized() {
        let model = private_model();
        let key = ReportPrivacyKey::fixture(1);
        let first = model_reference(&key, "openai", &model);
        let second = model_reference(&key, "openai", &model);
        assert_eq!(first, second);
        assert!(first.starts_with("private:"));
        assert!(!first.contains("customer"));
        assert_ne!(
            first,
            model_reference(&ReportPrivacyKey::fixture(2), "openai", &model)
        );
    }

    #[test]
    fn artifact_scan_detects_raw_percent_json_and_base64_secrets() {
        let secret = "~~~~".to_string();
        assert!(scan_artifact(b"safe", std::slice::from_ref(&secret)).is_ok());
        assert!(scan_artifact(secret.as_bytes(), std::slice::from_ref(&secret)).is_err());
        assert!(scan_artifact(
            percent_encode(secret.as_bytes(), false).as_bytes(),
            std::slice::from_ref(&secret)
        )
        .is_err());
        assert!(scan_artifact(
            percent_encode(secret.as_bytes(), true).as_bytes(),
            std::slice::from_ref(&secret)
        )
        .is_err());
        let escaped = serde_json::to_string(&secret).unwrap();
        assert!(scan_artifact(escaped.as_bytes(), std::slice::from_ref(&secret)).is_err());
        for encoded in [
            base64_encode(secret.as_bytes(), Base64Alphabet::Standard, true),
            base64_encode(secret.as_bytes(), Base64Alphabet::Standard, false),
            base64_encode(secret.as_bytes(), Base64Alphabet::UrlSafe, true),
            base64_encode(secret.as_bytes(), Base64Alphabet::UrlSafe, false),
        ] {
            assert!(scan_artifact(encoded.as_bytes(), std::slice::from_ref(&secret)).is_err());
        }
        assert_eq!(
            base64_encode(secret.as_bytes(), Base64Alphabet::Standard, true),
            "fn5+fg=="
        );
        assert_eq!(
            base64_encode(secret.as_bytes(), Base64Alphabet::UrlSafe, false),
            "fn5-fg"
        );
    }

    #[test]
    fn catalog_report_accounts_for_private_model_without_disclosure() {
        let model = private_model();
        let snapshot = CatalogSnapshot {
            provider: "openai".to_string(),
            captured_at: Utc::now(),
            page_count: 1,
            models: vec![model.clone()],
        };
        let plan = RunPlan {
            provider: "openai".to_string(),
            accounting: vec![AccountingEntry {
                model,
                classification: Classification::RequiresProbe,
                justification: AccountingJustification::CatalogMetadataRequiresProbe,
            }],
            profiles: Vec::new(),
            logical_requests: 0,
            maximum_attempts: 0,
            maximum_output_tokens: 0,
            projected_cost_usd: Some(0.0),
        };
        let report =
            catalog_provider_report(&ReportPrivacyKey::fixture(1), "run", &snapshot, &plan);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("customer"));
        assert!(!json.contains("private-name"));
        assert!(report.passed);
    }

    #[test]
    fn catalog_drift_is_explicit_and_invalidates_a_passing_report() {
        let model = private_model();
        let snapshot = CatalogSnapshot {
            provider: "openai".to_string(),
            captured_at: Utc::now(),
            page_count: 1,
            models: vec![model.clone()],
        };
        let plan = RunPlan {
            provider: "openai".to_string(),
            accounting: vec![AccountingEntry {
                model,
                classification: Classification::RequiresProbe,
                justification: AccountingJustification::CatalogMetadataRequiresProbe,
            }],
            profiles: Vec::new(),
            logical_requests: 0,
            maximum_attempts: 0,
            maximum_output_tokens: 0,
            projected_cost_usd: Some(0.0),
        };
        let mut report =
            catalog_provider_report(&ReportPrivacyKey::fixture(1), "run", &snapshot, &plan);

        mark_catalog_drift(&mut report);

        assert!(report.catalog_drift);
        assert!(!report.complete);
        assert!(!report.passed);
    }

    #[test]
    fn selected_report_records_suite_provenance_and_task_results() {
        let now = Utc::now();
        let report = QualificationReport::new(
            "run".to_string(),
            LiveMode::Selected,
            now,
            now,
            Vec::new(),
            Some(SelectedSuiteEvidence {
                suite_id: "nib-llm-core-v1".to_string(),
                matrix_sha256: "a".repeat(64),
                owner: "nib-maintainers".to_string(),
                reviewed_at: "2026-08-17".to_string(),
                expires_at: "2027-02-17".to_string(),
                required_task_count: 3,
                conditional_task_count: 1,
            }),
            &qualification_settings(),
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], 2);
        assert_eq!(json["selected_suite"]["suite_id"], "nib-llm-core-v1");
        assert_eq!(json["selected_suite"]["required_task_count"], 3);
        let markdown = markdown_summary(&report);
        assert!(markdown.contains("Selected suite: `nib-llm-core-v1`"));
        assert!(markdown.contains(&"a".repeat(64)));
    }

    #[test]
    fn publication_writes_one_complete_bounded_pair_without_internal_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let results = directory.path().join("nested/results");
        let settings = settings(results.clone(), Vec::new());

        let published = publish(&settings, report("run-1", true, true)).unwrap();

        assert!(published.passed);
        assert_eq!(published.json, results.join("run-1.json"));
        assert_eq!(published.markdown, results.join("run-1.md"));
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&published.json).unwrap()).unwrap();
        assert_eq!(json["complete"], true);
        assert_eq!(json["passed"], true);
        let markdown = fs::read_to_string(&published.markdown).unwrap();
        assert!(markdown.contains("Complete: `true`"));
        assert!(markdown.contains("Passed: `true`"));
        assert_no_internal_aliases(&results);
    }

    #[test]
    fn claimed_pass_requires_consistent_provider_accounting_profiles_and_scenarios() {
        assert!(validate_report_consistency(&passing_generation_report(LiveMode::Canary)).is_ok());

        let mut duplicate_provider = report("run-1", true, true);
        duplicate_provider
            .providers
            .push(provider_report(true, true));
        assert!(validate_report_consistency(&duplicate_provider).is_err());

        let mut accounting_mismatch = report("run-1", true, true);
        accounting_mismatch.providers[0].accounting.clear();
        assert!(validate_report_consistency(&accounting_mismatch).is_err());

        let mut duplicate_accounting = report("run-1", true, true);
        duplicate_accounting.providers[0].catalog_models = 2;
        duplicate_accounting.providers[0]
            .accounting
            .push(CatalogEntryReport {
                model_ref: "gpt-test".to_string(),
                classification: Classification::Qualified,
                justification: AccountingJustification::QualificationProfileEvidence,
            });
        assert!(validate_report_consistency(&duplicate_accounting).is_err());

        let mut drift = report("run-1", true, true);
        drift.providers[0].catalog_drift = true;
        assert!(validate_report_consistency(&drift).is_err());

        let mut no_profiles = passing_generation_report(LiveMode::Canary);
        no_profiles.providers[0].profiles.clear();
        assert!(validate_report_consistency(&no_profiles).is_err());

        let mut duplicate_profiles = passing_generation_report(LiveMode::Canary);
        let duplicate = duplicate_profiles.providers[0].profiles[0].clone();
        duplicate_profiles.providers[0].profiles.push(duplicate);
        assert!(validate_report_consistency(&duplicate_profiles).is_err());

        let mut unqualified = passing_generation_report(LiveMode::Canary);
        unqualified.providers[0].profiles[0].classification = Classification::FailedAdapter;
        assert!(validate_report_consistency(&unqualified).is_err());

        let mut failed_scenario = passing_generation_report(LiveMode::Canary);
        failed_scenario.providers[0].profiles[0].scenarios[0].passed = false;
        assert!(validate_report_consistency(&failed_scenario).is_err());

        let mut duplicate_scenario = passing_generation_report(LiveMode::Canary);
        let repeated = duplicate_scenario.providers[0].profiles[0].scenarios[0].clone();
        duplicate_scenario.providers[0].profiles[0]
            .scenarios
            .push(repeated);
        assert!(validate_report_consistency(&duplicate_scenario).is_err());

        let mut unresolved_accounting = passing_generation_report(LiveMode::Full);
        unresolved_accounting.providers[0].accounting[0].classification =
            Classification::RequiresProbe;
        assert!(validate_report_consistency(&unresolved_accounting).is_err());
    }

    #[test]
    fn passing_reports_reject_forged_actuals_costs_and_budget_truncation() {
        let baseline = passing_generation_report(LiveMode::Canary);

        let mut excessive_requests = baseline.providers[0].clone();
        excessive_requests.actual_logical_requests = excessive_requests.logical_requests + 1;
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            excessive_requests,
        ))
        .is_err());

        let mut forged_output_plan = baseline.providers[0].clone();
        forged_output_plan.maximum_output_tokens += 1;
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            forged_output_plan,
        ))
        .is_err());

        let mut missing_attempts = baseline.providers[0].clone();
        missing_attempts.actual_attempts = None;
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            missing_attempts,
        ))
        .is_err());

        let mut forged_usage = baseline.providers[0].clone();
        forged_usage.usage = Some(LlmUsage::new(1, 1, 2, None, None).unwrap());
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            forged_usage,
        ))
        .is_err());

        let mut forged_cost = baseline.providers[0].clone();
        forged_cost.profiles[0].scenarios[0].actual_cost_usd = Some(1.0);
        refresh_provider_evidence(&mut forged_cost);
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            forged_cost,
        ))
        .is_err());

        let mut success_with_status = baseline.providers[0].clone();
        success_with_status.profiles[0].scenarios[0].http_status = Some(200);
        refresh_provider_evidence(&mut success_with_status);
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            success_with_status,
        ))
        .is_err());

        let mut forged_duration = passing_generation_report(LiveMode::Canary);
        forged_duration.duration_ms = 1;
        assert!(validate_report_consistency(&forged_duration).is_err());

        let mut budget_truncated = baseline.providers[0].clone();
        budget_truncated.profiles[0].scenarios[0].budget_blocked = true;
        budget_truncated.profiles[0].scenarios[0].passed = false;
        budget_truncated.profiles[0].scenarios[0].safe_error_class =
            Some("blocked_budget".to_string());
        refresh_provider_evidence(&mut budget_truncated);
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            budget_truncated,
        ))
        .is_err());
    }

    #[test]
    fn passing_report_requires_every_advertised_scenario_but_allows_partial_usage() {
        let baseline = passing_generation_report(LiveMode::Canary);
        let mut absent_execution = baseline.providers[0].clone();
        absent_execution.profiles[0]
            .scenarios
            .retain(|scenario| scenario.scenario != ScenarioId::SingleToolContinuation);
        refresh_provider_evidence(&mut absent_execution);
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            absent_execution,
        ))
        .is_err());

        let mut missing_required = baseline.providers[0].clone();
        missing_required.profiles[0]
            .required_scenarios
            .retain(|scenario| *scenario != ScenarioId::SingleToolContinuation);
        missing_required.profiles[0]
            .scenarios
            .retain(|scenario| scenario.scenario != ScenarioId::SingleToolContinuation);
        refresh_provider_evidence(&mut missing_required);
        assert!(validate_report_consistency(&qualification_with_provider(
            LiveMode::Canary,
            missing_required,
        ))
        .is_err());

        let mut partial_usage = baseline.providers[0].clone();
        let tool = partial_usage.profiles[0]
            .scenarios
            .iter_mut()
            .find(|scenario| scenario.scenario == ScenarioId::SingleToolContinuation)
            .unwrap();
        tool.usage_requests = 1;
        tool.usage_complete_requests = 1;
        tool.usage_completeness = UsageCompleteness::Partial;
        tool.usage = Some(LlmUsage::new(2, 1, 3, None, None).unwrap());
        tool.actual_cost_usd = None;
        refresh_provider_evidence(&mut partial_usage);
        let partial = qualification_with_provider(LiveMode::Canary, partial_usage);
        assert!(validate_report_consistency(&partial).is_ok());
        assert_eq!(partial.usage_completeness, UsageCompleteness::Partial);
        assert!(partial.actual_cost_usd.is_none());
    }

    #[test]
    fn real_canary_and_full_plans_accept_only_the_scenarios_the_planner_persisted() {
        let descriptor = nib::llm::registry::provider_descriptor("openai").unwrap();
        let canary_snapshot = CatalogSnapshot {
            provider: "openai".to_string(),
            captured_at: Utc::now(),
            page_count: 1,
            models: vec![planned_model(descriptor.default_model(), true)],
        };
        let canary = qualification_from_real_plan(LiveMode::Canary, &canary_snapshot);
        assert!(canary.providers[0].profiles.iter().all(|profile| {
            profile.scenarios.len() == 3
                && profile.not_applicable_scenarios.is_empty()
                && !profile
                    .scenarios
                    .iter()
                    .any(|scenario| scenario.scenario == ScenarioId::ParallelToolContinuation)
        }));
        assert!(validate_report_consistency(&canary).is_ok());

        let mut full_models = descriptor
            .models()
            .iter()
            .map(|model| planned_model(model, true))
            .collect::<Vec<_>>();
        full_models.push(planned_model("text-only-extra", false));
        let full_snapshot = CatalogSnapshot {
            provider: "openai".to_string(),
            captured_at: Utc::now(),
            page_count: 1,
            models: full_models,
        };
        let full = qualification_from_real_plan(LiveMode::Full, &full_snapshot);
        assert!(full.providers[0].profiles.iter().any(|profile| {
            profile.model_ref == "text-only-extra"
                && profile.scenarios.len() == 2
                && profile.not_applicable_scenarios.is_empty()
                && profile.scenarios.iter().all(|scenario| {
                    matches!(
                        scenario.scenario,
                        ScenarioId::CompleteText | ScenarioId::StreamedText
                    )
                })
        }));
        assert!(validate_report_consistency(&full).is_ok());
    }

    #[test]
    fn existing_regular_file_and_directory_targets_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let results = directory.path().join("results");
        fs::create_dir(&results).unwrap();
        let json = results.join("run-1.json");
        fs::write(&json, b"existing").unwrap();
        let settings = settings(results.clone(), Vec::new());

        assert!(publish(&settings, report("run-1", true, true)).is_err());
        assert_eq!(fs::read(&json).unwrap(), b"existing");
        assert!(!results.join("run-1.md").exists());
        assert_no_internal_aliases(&results);

        fs::remove_file(&json).unwrap();
        fs::create_dir(results.join("run-1.md")).unwrap();
        assert!(publish(&settings, report("run-1", true, true)).is_err());
        assert!(!json.exists());
        assert!(results.join("run-1.md").is_dir());
        assert_no_internal_aliases(&results);
    }

    #[cfg(unix)]
    #[test]
    fn existing_and_broken_target_symlinks_are_preserved() {
        use std::os::unix::fs::symlink;

        for broken in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let results = directory.path().join("results");
            fs::create_dir(&results).unwrap();
            let outside = directory.path().join("outside");
            if !broken {
                fs::write(&outside, b"outside").unwrap();
            }
            let json = results.join("run-1.json");
            symlink(&outside, &json).unwrap();
            let settings = settings(results.clone(), Vec::new());

            assert!(publish(&settings, report("run-1", true, true)).is_err());
            assert!(json.symlink_metadata().unwrap().file_type().is_symlink());
            if !broken {
                assert_eq!(fs::read(&outside).unwrap(), b"outside");
            }
            assert!(!results.join("run-1.md").exists());
            assert_no_internal_aliases(&results);
        }
    }

    #[cfg(unix)]
    #[test]
    fn results_directory_and_parent_component_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let linked_results = directory.path().join("linked-results");
        symlink(&outside, &linked_results).unwrap();
        let linked_settings = settings(linked_results, Vec::new());
        assert!(publish(&linked_settings, report("run-1", true, true)).is_err());

        let parent = directory.path().join("parent");
        fs::create_dir(&parent).unwrap();
        let linked_parent = parent.join("linked-parent");
        symlink(&outside, &linked_parent).unwrap();
        let nested_settings = settings(linked_parent.join("results"), Vec::new());
        assert!(publish(&nested_settings, report("run-1", true, true)).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn directory_replacement_before_or_between_publications_is_detected_and_cleaned() {
        for point in [
            PublicationPoint::AfterDirectoryAnchor,
            PublicationPoint::BetweenPublications,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let results = directory.path().join("results");
            let displaced = directory.path().join("displaced");
            let settings = settings(results.clone(), Vec::new());
            let mut replaced = false;

            let outcome = publish_with_hook(
                &settings,
                report("run-1", true, true),
                |current, results, _, _| {
                    if current == point && !replaced {
                        fs::rename(results, &displaced).map_err(|error| error.to_string())?;
                        fs::create_dir(results).map_err(|error| error.to_string())?;
                        replaced = true;
                    }
                    Ok(())
                },
            );

            assert!(outcome.is_err());
            assert!(replaced);
            assert!(fs::read_dir(&results).unwrap().next().is_none());
            assert!(fs::read_dir(&displaced).unwrap().next().is_none());
        }
    }

    #[test]
    fn second_file_failure_removes_only_the_owned_first_publication() {
        let directory = tempfile::tempdir().unwrap();
        let results = directory.path().join("results");
        let settings = settings(results.clone(), Vec::new());

        let outcome = publish_with_hook(
            &settings,
            report("run-1", true, true),
            |point, _, _, markdown| {
                if point == PublicationPoint::BetweenPublications {
                    fs::write(markdown, b"unowned markdown").map_err(|error| error.to_string())?;
                }
                Ok(())
            },
        );

        assert!(outcome.is_err());
        assert!(!results.join("run-1.json").exists());
        assert_eq!(
            fs::read(results.join("run-1.md")).unwrap(),
            b"unowned markdown"
        );
        assert_no_internal_aliases(&results);
    }

    #[test]
    fn rollback_preserves_a_raced_unowned_first_file_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let results = directory.path().join("results");
        let settings = settings(results.clone(), Vec::new());

        let outcome = publish_with_hook(
            &settings,
            report("run-1", true, true),
            |point, _, json, markdown| {
                if point == PublicationPoint::BetweenPublications {
                    fs::remove_file(json).map_err(|error| error.to_string())?;
                    fs::write(json, b"unowned json").map_err(|error| error.to_string())?;
                    fs::write(markdown, b"unowned markdown").map_err(|error| error.to_string())?;
                }
                Ok(())
            },
        );

        assert!(outcome.is_err());
        assert_eq!(
            fs::read(results.join("run-1.json")).unwrap(),
            b"unowned json"
        );
        assert_eq!(
            fs::read(results.join("run-1.md")).unwrap(),
            b"unowned markdown"
        );
        assert_no_internal_aliases(&results);
    }

    #[test]
    fn oversize_and_sensitive_reports_are_suppressed_before_directory_creation() {
        let directory = tempfile::tempdir().unwrap();
        let oversize_results = directory.path().join("oversize");
        let oversize_settings = settings(oversize_results.clone(), Vec::new());
        let mut oversize = report("run-1", true, true);
        oversize.platform = "x".repeat(MAX_REPORT_BYTES + 1);
        assert!(publish(&oversize_settings, oversize).is_err());
        assert!(!oversize_results.exists());

        let secret = "credential-never-publish".to_string();
        let sensitive_results = directory.path().join("sensitive");
        let sensitive_settings = settings(sensitive_results.clone(), vec![secret.clone()]);
        let mut sensitive = report("run-1", true, true);
        sensitive.platform = secret;
        assert!(publish(&sensitive_settings, sensitive).is_err());
        assert!(!sensitive_results.exists());
    }

    #[test]
    fn incomplete_or_truncated_report_is_never_accepted_as_passed() {
        let directory = tempfile::tempdir().unwrap();
        let incomplete_results = directory.path().join("incomplete");
        let incomplete_settings = settings(incomplete_results.clone(), Vec::new());
        let published = publish(&incomplete_settings, report("run-1", false, false)).unwrap();
        assert!(!published.passed);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(published.json).unwrap()).unwrap();
        assert_eq!(json["complete"], false);
        assert_eq!(json["passed"], false);

        let mut forged = report("run-2", false, false);
        forged.passed = true;
        assert!(publish(&incomplete_settings, forged).is_err());
        assert!(!incomplete_results.join("run-2.json").exists());

        let truncated_results = directory.path().join("truncated");
        let truncated_settings = settings(truncated_results.clone(), Vec::new());
        let outcome = publish_with_hook(
            &truncated_settings,
            report("run-3", true, true),
            |point, _, json, _| {
                if point == PublicationPoint::BeforeVisibleRevalidation {
                    fs::write(json, b"{").map_err(|error| error.to_string())?;
                }
                Ok(())
            },
        );
        assert!(outcome.is_err());
        assert!(!truncated_results.join("run-3.json").exists());
        assert!(!truncated_results.join("run-3.md").exists());
        assert_no_internal_aliases(&truncated_results);
    }
}
