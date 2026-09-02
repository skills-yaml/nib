use super::config::LiveSettings;
use super::plan::RunPlan;
use super::report::{
    self, ProfileReport, ProviderReport, ReportPrivacyKey, ScenarioNotApplicableEvidence,
    ScenarioReport,
};
use super::{
    CatalogSnapshot, Classification, LlmTerminalStatus, ModelProfile, ScenarioId, TransportId,
};
use futures::{stream, StreamExt as _};
use nib::config::{LlmApiMode, LlmConfig, NibConfig, ProviderEntry};
use nib::llm::{
    create_client, LlmError, LlmErrorClass, LlmErrorPhase, LlmFinishReason, LlmMessage, LlmRequest,
    LlmRequestScope, LlmResponse, LlmStream, LlmUsage, RetryAttemptMetadata, ToolDefinition,
    ToolResult,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const MAX_HARNESS_FAILURE_DETAIL_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HarnessFailureCode {
    Budget,
    ScenarioTimeout,
    Evidence,
    Configuration,
    Scope,
    ResponseMismatch,
    ToolCorrelation,
}

impl HarnessFailureCode {
    fn safe_error_class(self) -> &'static str {
        match self {
            Self::Budget => "blocked_budget",
            Self::ScenarioTimeout => "scenario_timeout",
            Self::Evidence => "invalid_execution_evidence",
            Self::Configuration => "blocked_configuration",
            Self::Scope => "harness_scope",
            Self::ResponseMismatch => "response_mismatch",
            Self::ToolCorrelation => "tool_correlation",
        }
    }

    fn classification(self) -> Classification {
        match self {
            Self::Budget | Self::ScenarioTimeout => Classification::BlockedBudget,
            Self::Configuration => Classification::BlockedConfiguration,
            Self::Evidence | Self::Scope | Self::ResponseMismatch | Self::ToolCorrelation => {
                Classification::FailedAdapter
            }
        }
    }
}

const MAX_ATTEMPTS_PER_LOGICAL_REQUEST: usize = 3;

#[derive(Debug, Clone)]
pub(super) struct RunBudget {
    started: Instant,
    ledger: Arc<Mutex<BudgetLedger>>,
}

impl RunBudget {
    pub(super) fn new() -> Self {
        Self::new_at(Instant::now())
    }

    pub(super) fn new_at(started: Instant) -> Self {
        Self {
            started,
            ledger: Arc::new(Mutex::new(BudgetLedger::default())),
        }
    }
}

#[derive(Debug, Default)]
struct BudgetLedger {
    logical_requests: usize,
    attempts_charged: usize,
    output_tokens_charged: u64,
    cost_charged_usd: f64,
}

#[derive(Clone)]
struct ProviderBudget<'a> {
    settings: &'a LiveSettings,
    run: RunBudget,
    started: Instant,
    ledger: Arc<Mutex<BudgetLedger>>,
}

impl<'a> ProviderBudget<'a> {
    fn new(settings: &'a LiveSettings, run: &RunBudget) -> Self {
        Self {
            settings,
            run: run.clone(),
            started: Instant::now(),
            ledger: Arc::new(Mutex::new(BudgetLedger::default())),
        }
    }

    fn remaining(&self, scenario_started: Instant) -> Result<Duration, ScenarioFailure> {
        let exhausted = || {
            ScenarioFailure::harness(
                HarnessFailureCode::Budget,
                "live execution deadline was exhausted before the next provider call",
            )
        };
        let scenario_remaining = self
            .settings
            .limits
            .max_scenario_duration
            .checked_sub(scenario_started.elapsed())
            .ok_or_else(exhausted)?;
        let provider_remaining = self
            .settings
            .limits
            .max_provider_duration
            .checked_sub(self.started.elapsed())
            .ok_or_else(exhausted)?;
        let run_remaining = self
            .settings
            .limits
            .max_run_duration
            .checked_sub(self.run.started.elapsed())
            .ok_or_else(exhausted)?;
        let remaining = scenario_remaining
            .min(provider_remaining)
            .min(run_remaining);
        if remaining.is_zero() {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::Budget,
                "live execution deadline was exhausted before the next provider call",
            ));
        }
        Ok(remaining)
    }

    fn begin_request(
        &mut self,
        scenario_started: Instant,
        pricing: Option<&super::CatalogPricing>,
    ) -> Result<Duration, ScenarioFailure> {
        let remaining = self.remaining(scenario_started)?;
        let limits = &self.settings.limits;
        // One lock order (run, then provider) makes admission atomic across all
        // concurrently scheduled profiles without holding either lock over network I/O.
        let mut run_ledger = lock_ledger(&self.run.ledger)?;
        let mut provider_ledger = lock_ledger(&self.ledger)?;
        let next_provider_requests = provider_ledger
            .logical_requests
            .checked_add(1)
            .ok_or_else(budget_overflow)?;
        let next_run_requests = run_ledger
            .logical_requests
            .checked_add(1)
            .ok_or_else(budget_overflow)?;
        let next_provider_attempts = provider_ledger
            .attempts_charged
            .checked_add(MAX_ATTEMPTS_PER_LOGICAL_REQUEST)
            .ok_or_else(budget_overflow)?;
        let next_run_attempts = run_ledger
            .attempts_charged
            .checked_add(MAX_ATTEMPTS_PER_LOGICAL_REQUEST)
            .ok_or_else(budget_overflow)?;
        let request_output = u64::from(limits.max_output_tokens_per_request);
        let next_provider_output = provider_ledger
            .output_tokens_charged
            .checked_add(request_output)
            .ok_or_else(budget_overflow)?;
        let next_run_output = run_ledger
            .output_tokens_charged
            .checked_add(request_output)
            .ok_or_else(budget_overflow)?;
        let maximum_cost = maximum_request_cost(pricing, limits)?;
        let provider_cost = checked_cost_add(provider_ledger.cost_charged_usd, maximum_cost)?;
        let run_cost = checked_cost_add(run_ledger.cost_charged_usd, maximum_cost)?;
        if next_provider_requests > limits.max_logical_requests
            || next_run_requests > limits.max_logical_requests
            || next_provider_attempts > limits.max_attempts
            || next_run_attempts > limits.max_attempts
            || next_provider_output > limits.max_total_output_tokens
            || next_run_output > limits.max_total_output_tokens
            || provider_cost.is_some_and(|cost| cost > limits.max_actual_cost_usd)
            || run_cost.is_some_and(|cost| cost > limits.max_actual_cost_usd)
        {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::Budget,
                "live cumulative budget cannot admit the next provider call",
            ));
        }
        provider_ledger.logical_requests = next_provider_requests;
        run_ledger.logical_requests = next_run_requests;
        provider_ledger.attempts_charged = next_provider_attempts;
        run_ledger.attempts_charged = next_run_attempts;
        provider_ledger.output_tokens_charged = next_provider_output;
        run_ledger.output_tokens_charged = next_run_output;
        if let Some(cost) = provider_cost {
            provider_ledger.cost_charged_usd = cost;
        }
        if let Some(cost) = run_cost {
            run_ledger.cost_charged_usd = cost;
        }
        Ok(remaining)
    }

    fn finish_request(
        &mut self,
        attempts: Option<usize>,
        usage: Option<LlmUsage>,
        pricing: Option<&super::CatalogPricing>,
    ) -> Result<(), ScenarioFailure> {
        if attempts.is_some_and(|attempts| attempts > MAX_ATTEMPTS_PER_LOGICAL_REQUEST) {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::Evidence,
                "provider reported more attempts than the production retry bound",
            ));
        }
        let attempts_charged = attempts.unwrap_or(MAX_ATTEMPTS_PER_LOGICAL_REQUEST);
        // Provider usage belongs to the terminal attempt. If retrying occurred (or
        // attempt evidence is absent), failed-attempt token usage is unknowable: keep
        // the conservative output/cost reservation for admission accounting.
        let usage_is_attempt_complete = attempts == Some(1) && usage.is_some();
        let output_charged = if usage_is_attempt_complete {
            usage.map(|usage| usage.output_tokens).unwrap_or(u64::from(
                self.settings.limits.max_output_tokens_per_request,
            ))
        } else {
            u64::from(self.settings.limits.max_output_tokens_per_request)
        };
        let actual_cost = if usage_is_attempt_complete {
            report::calculate_actual_cost(pricing, usage, attempts)
                .map_err(|_| budget_overflow())?
        } else {
            None
        };
        let cost_charged = match actual_cost {
            Some(cost) => Some(cost),
            None => maximum_request_cost(pricing, &self.settings.limits)?,
        };
        let maximum_cost = maximum_request_cost(pricing, &self.settings.limits)?;
        let mut run_ledger = lock_ledger(&self.run.ledger)?;
        let mut provider_ledger = lock_ledger(&self.ledger)?;
        for ledger in [&mut *provider_ledger, &mut *run_ledger] {
            ledger.attempts_charged = ledger
                .attempts_charged
                .checked_sub(MAX_ATTEMPTS_PER_LOGICAL_REQUEST)
                .and_then(|attempts| attempts.checked_add(attempts_charged))
                .ok_or_else(budget_overflow)?;
            ledger.output_tokens_charged = ledger
                .output_tokens_charged
                .checked_sub(u64::from(
                    self.settings.limits.max_output_tokens_per_request,
                ))
                .and_then(|tokens| tokens.checked_add(output_charged))
                .ok_or_else(budget_overflow)?;
            if let Some(maximum_cost) = maximum_cost {
                let retained = ledger.cost_charged_usd - maximum_cost;
                let replacement = cost_charged.unwrap_or(maximum_cost);
                let next = retained + replacement;
                if !retained.is_finite()
                    || !replacement.is_finite()
                    || !next.is_finite()
                    || retained < -f64::EPSILON
                    || replacement < 0.0
                {
                    return Err(budget_overflow());
                }
                ledger.cost_charged_usd = next.max(0.0);
            }
        }
        let limits = &self.settings.limits;
        if provider_ledger.attempts_charged > limits.max_attempts
            || run_ledger.attempts_charged > limits.max_attempts
            || provider_ledger.output_tokens_charged > limits.max_total_output_tokens
            || run_ledger.output_tokens_charged > limits.max_total_output_tokens
            || provider_ledger.cost_charged_usd > limits.max_actual_cost_usd
            || run_ledger.cost_charged_usd > limits.max_actual_cost_usd
            || self.started.elapsed() > limits.max_provider_duration
            || self.run.started.elapsed() > limits.max_run_duration
        {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::Budget,
                "live cumulative budget was exhausted by the completed provider call",
            ));
        }
        Ok(())
    }
}

fn lock_ledger(
    ledger: &Arc<Mutex<BudgetLedger>>,
) -> Result<MutexGuard<'_, BudgetLedger>, ScenarioFailure> {
    ledger.lock().map_err(|_| {
        ScenarioFailure::harness(
            HarnessFailureCode::Evidence,
            "live cumulative budget ledger was poisoned",
        )
    })
}

fn budget_overflow() -> ScenarioFailure {
    ScenarioFailure::harness(
        HarnessFailureCode::Budget,
        "live cumulative budget arithmetic overflowed",
    )
}

fn checked_cost_add(left: f64, right: Option<f64>) -> Result<Option<f64>, ScenarioFailure> {
    let Some(right) = right else {
        return Ok(None);
    };
    let total = left + right;
    if !left.is_finite() || !right.is_finite() || !total.is_finite() || left < 0.0 || right < 0.0 {
        return Err(budget_overflow());
    }
    Ok(Some(total))
}

fn maximum_request_cost(
    pricing: Option<&super::CatalogPricing>,
    limits: &super::config::LiveLimits,
) -> Result<Option<f64>, ScenarioFailure> {
    let Some(pricing) = pricing else {
        return Ok(None);
    };
    if pricing.prompt_per_token_usd.is_none() || pricing.completion_per_token_usd.is_none() {
        return Ok(None);
    }
    let usage = LlmUsage::new(
        super::plan::ESTIMATED_PROMPT_TOKENS_PER_REQUEST as u64,
        u64::from(limits.max_output_tokens_per_request),
        super::plan::ESTIMATED_PROMPT_TOKENS_PER_REQUEST as u64
            + u64::from(limits.max_output_tokens_per_request),
        None,
        None,
    )
    .map_err(|_| budget_overflow())?;
    let one_attempt = report::calculate_actual_cost(Some(pricing), Some(usage), Some(1))
        .map_err(|_| budget_overflow())?;
    let maximum = one_attempt.map(|cost| cost * MAX_ATTEMPTS_PER_LOGICAL_REQUEST as f64);
    if maximum.is_some_and(|cost| !cost.is_finite() || cost < 0.0) {
        return Err(budget_overflow());
    }
    Ok(maximum)
}

#[derive(Clone, PartialEq, Eq)]
struct HarnessFailure {
    code: HarnessFailureCode,
    detail: String,
}

impl HarnessFailure {
    fn new(code: HarnessFailureCode, detail: impl AsRef<str>) -> Self {
        Self {
            code,
            detail: bounded_local_detail(detail.as_ref()),
        }
    }
}

impl std::fmt::Debug for HarnessFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessFailure")
            .field("code", &self.code)
            .field("detail_bytes", &self.detail.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
enum ScenarioFailure {
    Llm(LlmError),
    Harness(HarnessFailure),
}

impl ScenarioFailure {
    fn harness(code: HarnessFailureCode, detail: impl AsRef<str>) -> Self {
        Self::Harness(HarnessFailure::new(code, detail))
    }
}

impl std::fmt::Debug for ScenarioFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Llm(error) => formatter.debug_tuple("Llm").field(error).finish(),
            Self::Harness(error) => formatter.debug_tuple("Harness").field(error).finish(),
        }
    }
}

impl From<LlmError> for ScenarioFailure {
    fn from(error: LlmError) -> Self {
        Self::Llm(error)
    }
}

#[derive(Debug)]
struct ScenarioEvidence {
    actual_logical_requests: usize,
    actual_attempts: Option<usize>,
    usage_requests: usize,
    usage_complete_requests: usize,
    usage: Option<LlmUsage>,
}

impl ScenarioEvidence {
    fn new() -> Self {
        Self {
            actual_logical_requests: 0,
            actual_attempts: Some(0),
            usage_requests: 0,
            usage_complete_requests: 0,
            usage: None,
        }
    }

    fn begin_request(&mut self) -> Result<(), ScenarioFailure> {
        self.actual_logical_requests = self
            .actual_logical_requests
            .checked_add(1)
            .ok_or_else(budget_overflow)?;
        Ok(())
    }

    fn record_result(
        &mut self,
        attempts: Option<usize>,
        usage: Option<LlmUsage>,
    ) -> Result<(), ScenarioFailure> {
        self.actual_attempts = match (self.actual_attempts, attempts) {
            (Some(total), Some(attempts)) => {
                Some(total.checked_add(attempts).ok_or_else(budget_overflow)?)
            }
            _ => None,
        };
        if let Some(usage) = usage {
            self.usage_requests = self
                .usage_requests
                .checked_add(1)
                .ok_or_else(budget_overflow)?;
            if attempts == Some(1) {
                self.usage_complete_requests = self
                    .usage_complete_requests
                    .checked_add(1)
                    .ok_or_else(budget_overflow)?;
            }
            self.usage = Some(match self.usage {
                Some(total) => total.checked_add(usage).map_err(|_| budget_overflow())?,
                None => usage,
            });
        }
        Ok(())
    }

    fn report(
        self,
        scenario: ScenarioId,
        duration_ms: u64,
        pricing: Option<&super::CatalogPricing>,
        result: Result<(), ScenarioFailure>,
    ) -> ScenarioReport {
        let mut result = result;
        let usage_completeness = report::UsageCompleteness::from_counts(
            self.actual_logical_requests,
            self.usage_requests,
            self.usage_complete_requests,
        );
        let actual_cost_usd = if usage_completeness == report::UsageCompleteness::Complete {
            match report::calculate_actual_cost(pricing, self.usage, self.actual_attempts) {
                Ok(cost) => cost,
                Err(error) => {
                    if result.is_ok() {
                        result = Err(ScenarioFailure::harness(HarnessFailureCode::Budget, error));
                    }
                    None
                }
            }
        } else if self.actual_logical_requests == 0 {
            Some(0.0)
        } else {
            None
        };
        let (passed, safe_error_class, http_status, budget_blocked) = match result {
            Ok(()) => (true, None, None, false),
            Err(ref failure) => {
                let (_, safe_error_class) = classify_failure(failure, Some(scenario));
                let http_status = match failure {
                    ScenarioFailure::Llm(error) => error
                        .http_status
                        .filter(|status| (100..=599).contains(status)),
                    ScenarioFailure::Harness(_) => None,
                };
                (
                    false,
                    Some(safe_error_class.to_string()),
                    http_status,
                    matches!(
                        failure,
                        ScenarioFailure::Harness(HarnessFailure {
                            code: HarnessFailureCode::Budget | HarnessFailureCode::ScenarioTimeout,
                            ..
                        })
                    ),
                )
            }
        };
        ScenarioReport {
            scenario,
            passed,
            duration_ms,
            actual_logical_requests: self.actual_logical_requests,
            actual_attempts: self.actual_attempts,
            usage_requests: self.usage_requests,
            usage_complete_requests: self.usage_complete_requests,
            usage_completeness,
            usage: self.usage,
            actual_cost_usd,
            http_status,
            budget_blocked,
            not_executed_reason: None,
            safe_error_class,
        }
    }

    fn not_executed_unsupported(scenario: ScenarioId) -> ScenarioReport {
        ScenarioReport {
            scenario,
            passed: false,
            duration_ms: 0,
            actual_logical_requests: 0,
            actual_attempts: Some(0),
            usage_requests: 0,
            usage_complete_requests: 0,
            usage_completeness: report::UsageCompleteness::Unknown,
            usage: None,
            actual_cost_usd: Some(0.0),
            http_status: None,
            budget_blocked: false,
            not_executed_reason: Some(
                super::ScenarioNotExecutedReason::TransportUnsupportedByBasicProbe,
            ),
            safe_error_class: Some("not_executed_unsupported_transport".to_string()),
        }
    }
}

#[derive(Debug)]
struct ScenarioExecution {
    evidence: ScenarioEvidence,
    result: Result<(), ScenarioFailure>,
}

impl ScenarioExecution {
    fn blocked(error: ScenarioFailure) -> Self {
        Self {
            evidence: ScenarioEvidence::new(),
            result: Err(error),
        }
    }
}

struct ScenarioContext<'a, 'b> {
    budget: &'a mut ProviderBudget<'b>,
    pricing: Option<&'a super::CatalogPricing>,
    started: Instant,
    evidence: ScenarioEvidence,
}

impl<'a, 'b> ScenarioContext<'a, 'b> {
    fn new(budget: &'a mut ProviderBudget<'b>, pricing: Option<&'a super::CatalogPricing>) -> Self {
        Self {
            budget,
            pricing,
            started: Instant::now(),
            evidence: ScenarioEvidence::new(),
        }
    }

    async fn complete(
        &mut self,
        client: &dyn nib::llm::LlmClient,
        request: LlmRequest<'_>,
    ) -> Result<LlmResponse, ScenarioFailure> {
        let remaining = self.budget.begin_request(self.started, self.pricing)?;
        self.evidence.begin_request()?;
        match tokio::time::timeout(remaining, client.complete(request)).await {
            Ok(Ok(response)) => {
                self.record_result(Some(response.attempts), response.usage)?;
                Ok(response)
            }
            Ok(Err(error)) => {
                self.record_result(Some(error.attempts), None)?;
                Err(ScenarioFailure::from(error))
            }
            Err(_) => {
                self.record_result(None, None)?;
                Err(ScenarioFailure::harness(
                    HarnessFailureCode::ScenarioTimeout,
                    "live qualification request exceeded its remaining deadline",
                ))
            }
        }
    }

    async fn open_stream(
        &mut self,
        client: &dyn nib::llm::LlmClient,
        request: LlmRequest<'_>,
    ) -> Result<LlmStream, ScenarioFailure> {
        let remaining = self.budget.begin_request(self.started, self.pricing)?;
        self.evidence.begin_request()?;
        match tokio::time::timeout(remaining, client.stream(request)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(error)) => {
                self.record_result(Some(error.attempts), None)?;
                Err(ScenarioFailure::from(error))
            }
            Err(_) => {
                self.record_result(None, None)?;
                Err(ScenarioFailure::harness(
                    HarnessFailureCode::ScenarioTimeout,
                    "live qualification stream open exceeded its remaining deadline",
                ))
            }
        }
    }

    async fn finish_stream(&mut self, stream: LlmStream) -> Result<LlmResponse, ScenarioFailure> {
        let remaining = match self.budget.remaining(self.started) {
            Ok(remaining) => remaining,
            Err(error) => {
                self.record_result(None, None)?;
                return Err(error);
            }
        };
        match tokio::time::timeout(remaining, stream.finish()).await {
            Ok(Ok(response)) => {
                self.record_result(Some(response.attempts), response.usage)?;
                Ok(response)
            }
            Ok(Err(error)) => {
                self.record_result(Some(error.attempts), None)?;
                Err(ScenarioFailure::from(error))
            }
            Err(_) => {
                self.record_result(None, None)?;
                Err(ScenarioFailure::harness(
                    HarnessFailureCode::ScenarioTimeout,
                    "live qualification stream finish exceeded its remaining deadline",
                ))
            }
        }
    }

    fn record_result(
        &mut self,
        attempts: Option<RetryAttemptMetadata>,
        usage: Option<LlmUsage>,
    ) -> Result<(), ScenarioFailure> {
        let attempts = attempts.map(|attempts| usize::from(attempts.attempts()));
        self.evidence.record_result(attempts, usage)?;
        self.budget.finish_request(attempts, usage, self.pricing)
    }

    fn finish(self, result: Result<(), ScenarioFailure>) -> ScenarioExecution {
        let result = match self.budget.remaining(self.started) {
            Ok(_) => result,
            Err(budget_failure) => Err(budget_failure),
        };
        ScenarioExecution {
            evidence: self.evidence,
            result,
        }
    }
}

pub(super) async fn execute_provider_plan(
    settings: &LiveSettings,
    privacy_key: &ReportPrivacyKey,
    run_id: &str,
    snapshot: &CatalogSnapshot,
    plan: RunPlan,
    run_budget: &RunBudget,
    protected_http_client: Option<Client>,
) -> ProviderReport {
    let budget = ProviderBudget::new(settings, run_budget);
    let provider_started = Instant::now();
    let effective_concurrency = settings.concurrency.min(plan.profiles.len());
    let mut indexed_profiles = stream::iter(plan.profiles.iter().enumerate())
        .map(|(index, profile)| {
            let budget = budget.clone();
            let protected_http_client = protected_http_client.clone();
            async move {
                let profile_report = if budget.remaining(Instant::now()).is_err() {
                    let failure = ScenarioFailure::harness(
                        HarnessFailureCode::Budget,
                        "live provider qualification exceeded its deadline",
                    );
                    let (classification, _) = classify_failure(&failure, None);
                    report::profile_report(
                        privacy_key,
                        run_id,
                        &snapshot.provider,
                        &profile.model,
                        profile.transport,
                        profile.advertised,
                        profile.required_scenarios.iter().copied().collect(),
                        profile
                            .required_scenarios
                            .iter()
                            .map(|scenario| {
                                ScenarioEvidence::new().report(
                                    *scenario,
                                    0,
                                    profile.model.pricing.as_ref(),
                                    Err(failure.clone()),
                                )
                            })
                            .collect(),
                        profile_not_applicable_evidence(profile),
                        classification,
                    )
                } else {
                    execute_profile(
                        settings,
                        privacy_key,
                        run_id,
                        &snapshot.provider,
                        profile,
                        budget,
                        protected_http_client,
                    )
                    .await
                };
                (index, profile_report)
            }
        })
        .buffer_unordered(effective_concurrency.max(1))
        .collect::<Vec<_>>()
        .await;
    indexed_profiles.sort_by_key(|(index, _)| *index);
    let profiles = indexed_profiles
        .into_iter()
        .map(|(_, profile)| profile)
        .collect();
    let mut report = report::generation_provider_report(
        privacy_key,
        run_id,
        snapshot,
        &plan,
        profiles,
        settings.mode,
        effective_concurrency,
    );
    report.duration_ms = provider_started
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    report
}

async fn execute_profile(
    settings: &LiveSettings,
    privacy_key: &ReportPrivacyKey,
    run_id: &str,
    provider: &str,
    profile: &ModelProfile,
    mut budget: ProviderBudget<'_>,
    protected_http_client: Option<Client>,
) -> ProfileReport {
    let client = match client_for_profile(settings, provider, profile, protected_http_client) {
        Ok(client) => client,
        Err(error) => {
            let (classification, _) = classify_failure(&error, None);
            return report::profile_report(
                privacy_key,
                run_id,
                provider,
                &profile.model,
                profile.transport,
                profile.advertised,
                profile.required_scenarios.iter().copied().collect(),
                profile
                    .required_scenarios
                    .iter()
                    .map(|scenario| {
                        ScenarioEvidence::new().report(
                            *scenario,
                            0,
                            profile.model.pricing.as_ref(),
                            Err(error.clone()),
                        )
                    })
                    .collect(),
                profile_not_applicable_evidence(profile),
                classification,
            );
        }
    };
    let mut scenario_reports = Vec::new();
    let mut classification = Classification::Qualified;
    let mut budget_blocked = false;
    let mut transport_unsupported = false;

    for scenario in &profile.required_scenarios {
        let started = Instant::now();
        if transport_unsupported {
            scenario_reports.push(ScenarioEvidence::not_executed_unsupported(*scenario));
            continue;
        }
        let execution = if budget_blocked {
            ScenarioExecution::blocked(ScenarioFailure::harness(
                HarnessFailureCode::Budget,
                "live cumulative budget blocked the remaining required scenario",
            ))
        } else {
            match scenario {
                ScenarioId::CompleteText => {
                    complete_text(client.as_ref(), settings, run_id, &mut budget, profile).await
                }
                ScenarioId::StreamedText => {
                    streamed_text(client.as_ref(), settings, run_id, &mut budget, profile).await
                }
                ScenarioId::SingleToolContinuation => {
                    tool_continuation(
                        client.as_ref(),
                        settings,
                        run_id,
                        false,
                        &mut budget,
                        profile,
                    )
                    .await
                }
                ScenarioId::ParallelToolContinuation => {
                    tool_continuation(
                        client.as_ref(),
                        settings,
                        run_id,
                        true,
                        &mut budget,
                        profile,
                    )
                    .await
                }
            }
        };
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if let Err(error) = &execution.result {
            let (next_classification, _) = classify_failure(error, Some(*scenario));
            if classification == Classification::Qualified {
                classification = next_classification;
            }
            budget_blocked |= next_classification == Classification::BlockedBudget;
            transport_unsupported |= next_classification == Classification::UnsupportedTransport;
        }
        scenario_reports.push(execution.evidence.report(
            *scenario,
            duration_ms,
            profile.model.pricing.as_ref(),
            execution.result,
        ));
    }

    report::profile_report(
        privacy_key,
        run_id,
        provider,
        &profile.model,
        profile.transport,
        profile.advertised,
        profile.required_scenarios.iter().copied().collect(),
        scenario_reports,
        profile_not_applicable_evidence(profile),
        classification,
    )
}

fn profile_not_applicable_evidence(profile: &ModelProfile) -> Vec<ScenarioNotApplicableEvidence> {
    profile
        .not_applicable_scenarios
        .iter()
        .map(|(scenario, justification)| ScenarioNotApplicableEvidence {
            scenario: *scenario,
            justification: *justification,
        })
        .collect()
}

fn client_for_profile(
    settings: &LiveSettings,
    provider: &str,
    profile: &ModelProfile,
    protected_http_client: Option<Client>,
) -> Result<std::sync::Arc<dyn nib::llm::LlmClient>, ScenarioFailure> {
    let api = match profile.transport {
        TransportId::ChatCompletions => Some(LlmApiMode::ChatCompletions),
        TransportId::Responses => Some(LlmApiMode::Responses),
        TransportId::AnthropicMessages | TransportId::GeminiGenerateContent => None,
    };
    let entry = ProviderEntry {
        model: profile.model.generation_target().to_string(),
        models: None,
        api_key: None,
        api_keys: Vec::new(),
        base_url: (provider == "meta")
            .then(|| settings.meta_base_url.clone())
            .flatten(),
        api,
        reasoning_effort: None,
    };
    let llm = LlmConfig {
        active_provider: Some(provider.to_string()),
        providers: HashMap::from([(provider.to_string(), entry)]),
        context_length: 128_000,
    };
    let config = NibConfig {
        llm: llm.clone(),
        ..NibConfig::default()
    };
    config.validate().map_err(|error| {
        ScenarioFailure::harness(HarnessFailureCode::Configuration, error.to_string())
    })?;
    let diagnostics = nib::llm::factory::provider_diagnostics(&llm, Some(provider))
        .map_err(|error| ScenarioFailure::harness(HarnessFailureCode::Configuration, error))?;
    if diagnostics.provider != provider
        || diagnostics.model != profile.model.generation_target()
        || (api.is_some_and(|api| diagnostics.api_mode != api.as_str()))
    {
        return Err(ScenarioFailure::harness(
            HarnessFailureCode::Configuration,
            "production provider diagnostics do not match the live plan",
        ));
    }
    let client = match (provider, protected_http_client) {
        ("meta", Some(client)) => {
            nib::llm::factory::create_client_with_http_client(&llm, Some(provider), client)
        }
        ("meta", None) => {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::Configuration,
                "Meta qualification requires its initial protected catalog client",
            ));
        }
        (_, Some(_)) => {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::Configuration,
                "protected catalog client was supplied to a non-Meta provider",
            ));
        }
        (_, None) => create_client(&llm, Some(provider)),
    };
    client.map_err(|error| ScenarioFailure::harness(HarnessFailureCode::Configuration, error))
}

async fn complete_text(
    client: &dyn nib::llm::LlmClient,
    settings: &LiveSettings,
    run_id: &str,
    budget: &mut ProviderBudget<'_>,
    profile: &ModelProfile,
) -> ScenarioExecution {
    let mut context = ScenarioContext::new(budget, profile.model.pricing.as_ref());
    let result = async {
        let nonce = nonce("complete");
        let messages = [LlmMessage::user(format!(
            "Return the exact token {nonce} and no other text."
        ))];
        let scope = scope(run_id, "complete")?;
        let response = context
            .complete(client, live_request(&messages, None, scope, settings))
            .await?;
        validate_text_response(response, &nonce)
    }
    .await;
    context.finish(result)
}

async fn streamed_text(
    client: &dyn nib::llm::LlmClient,
    settings: &LiveSettings,
    run_id: &str,
    budget: &mut ProviderBudget<'_>,
    profile: &ModelProfile,
) -> ScenarioExecution {
    let mut context = ScenarioContext::new(budget, profile.model.pricing.as_ref());
    let result = async {
        let nonce = nonce("stream");
        let messages = [LlmMessage::user(format!(
            "Return the exact token {nonce} and no other text."
        ))];
        let scope = scope(run_id, "stream")?;
        let stream = context
            .open_stream(client, live_request(&messages, None, scope, settings))
            .await?;
        let response = context.finish_stream(stream).await?;
        if response.finish_reason != LlmFinishReason::Complete
            || !response
                .content
                .as_deref()
                .is_some_and(|content| content.contains(&nonce))
        {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::ResponseMismatch,
                "stream did not finish with one nonce-bearing authoritative response",
            ));
        }
        validate_text_response(response, &nonce)?;
        Ok(())
    }
    .await;
    context.finish(result)
}

async fn tool_continuation(
    client: &dyn nib::llm::LlmClient,
    settings: &LiveSettings,
    run_id: &str,
    parallel: bool,
    budget: &mut ProviderBudget<'_>,
    profile: &ModelProfile,
) -> ScenarioExecution {
    let mut context = ScenarioContext::new(budget, profile.model.pricing.as_ref());
    let result = async {
    let first_nonce = nonce("tool-a");
    let second_nonce = nonce("tool-b");
    let receipt = nonce("receipt");
    let tool_names = if parallel {
        vec!["record_probe_a", "record_probe_b"]
    } else {
        vec!["record_probe"]
    };
    let tools = tool_names
        .iter()
        .map(|name| qualification_tool(name))
        .collect::<Vec<_>>();
    let content = if parallel {
        format!(
            "Call both record_probe_a with nonce {first_nonce} and record_probe_b with nonce {second_nonce}. Do not answer directly."
        )
    } else {
        format!("Call record_probe with nonce {first_nonce}. Do not answer directly.")
    };
    let messages = [LlmMessage::user(content)];
    let scope = scope(run_id, if parallel { "parallel" } else { "tool" })?;
    let response = context
        .complete(client, live_request(
            &messages,
            Some(&tools),
            scope.clone(),
            settings,
        ))
        .await?;
    if response.terminal_status != LlmTerminalStatus::Completed {
        return Err(ScenarioFailure::harness(
            HarnessFailureCode::ResponseMismatch,
            "tool qualification request was not completed",
        ));
    }
    if response.finish_reason != LlmFinishReason::ToolCalls {
        return Err(ScenarioFailure::harness(
            HarnessFailureCode::ResponseMismatch,
            "tool qualification did not finish with the tool-calls classification",
        ));
    }
    let calls = response
        .tool_calls
        .as_ref()
        .filter(|calls| calls.len() == tool_names.len())
        .ok_or_else(|| {
            ScenarioFailure::harness(
                HarnessFailureCode::ToolCorrelation,
                "tool qualification returned the wrong number of tool calls",
            )
        })?;
    let expected = if parallel {
        BTreeMap::from([
            ("record_probe_a", first_nonce.as_str()),
            ("record_probe_b", second_nonce.as_str()),
        ])
    } else {
        BTreeMap::from([("record_probe", first_nonce.as_str())])
    };
    let mut invocation_ids = std::collections::BTreeSet::new();
    for call in calls {
        if expected.get(call.name.as_str()).copied()
            != call.arguments.get("nonce").and_then(Value::as_str)
        {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::ToolCorrelation,
                "tool qualification returned a mismatched name or nonce",
            ));
        }
        if !invocation_ids.insert(call.invocation_id) {
            return Err(ScenarioFailure::harness(
                HarnessFailureCode::ToolCorrelation,
                "tool qualification reused a neutral invocation ID",
            ));
        }
    }
    let mut continuation = response.continuation.ok_or_else(|| {
        ScenarioFailure::harness(
            HarnessFailureCode::ToolCorrelation,
            "tool qualification did not return private continuation state",
        )
    })?;
    for call in calls {
        let result = ToolResult::success(
            call.invocation_id,
            json!({"success": true, "receipt": receipt}),
        )
        .map_err(|error| ScenarioFailure::harness(HarnessFailureCode::ToolCorrelation, error))?;
        continuation.record_tool_result(result).map_err(|error| {
            ScenarioFailure::harness(HarnessFailureCode::ToolCorrelation, error)
        })?;
    }
    let final_response = context
        .complete(client,
            live_request(&messages, Some(&tools), scope, settings)
                .with_continuation(Some(continuation)),
        )
        .await?;
    validate_text_response(final_response, &receipt)
    }
    .await;
    context.finish(result)
}

fn live_request<'a>(
    messages: &'a [LlmMessage],
    tools: Option<&'a [ToolDefinition]>,
    scope: LlmRequestScope,
    settings: &LiveSettings,
) -> LlmRequest<'a> {
    LlmRequest::new(messages, tools)
        .with_scope(scope)
        .with_max_output_tokens(settings.limits.max_output_tokens_per_request)
}

fn qualification_tool(name: &str) -> ToolDefinition {
    ToolDefinition::new(
        name,
        "Record one synthetic qualification nonce without side effects.",
        json!({
            "type": "object",
            "properties": {"nonce": {"type": "string"}},
            "required": ["nonce"],
            "additionalProperties": false
        }),
    )
    .expect("qualification tool")
    .with_strict(true)
}

fn validate_text_response(
    response: nib::llm::LlmResponse,
    nonce: &str,
) -> Result<(), ScenarioFailure> {
    if response.terminal_status != LlmTerminalStatus::Completed {
        return Err(ScenarioFailure::harness(
            HarnessFailureCode::ResponseMismatch,
            "qualification response was refused",
        ));
    }
    if response.finish_reason != LlmFinishReason::Complete {
        return Err(ScenarioFailure::harness(
            HarnessFailureCode::ResponseMismatch,
            "text qualification did not finish with the complete classification",
        ));
    }
    if response
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty())
        || response.continuation.is_some()
    {
        return Err(ScenarioFailure::harness(
            HarnessFailureCode::ResponseMismatch,
            "text qualification unexpectedly returned tool state",
        ));
    }
    let content = response
        .content
        .filter(|content| content.len() <= 64 * 1024 && content.contains(nonce))
        .ok_or_else(|| {
            ScenarioFailure::harness(
                HarnessFailureCode::ResponseMismatch,
                "qualification response did not contain its nonce",
            )
        })?;
    if content.trim().is_empty() {
        return Err(ScenarioFailure::harness(
            HarnessFailureCode::ResponseMismatch,
            "qualification response is missing text or a finish classification",
        ));
    }
    Ok(())
}

fn scope(run_id: &str, scenario: &str) -> Result<LlmRequestScope, ScenarioFailure> {
    LlmRequestScope::new(
        format!("llm-live-{run_id}"),
        format!("{scenario}-{}", uuid::Uuid::new_v4()),
    )
    .map_err(|error| ScenarioFailure::harness(HarnessFailureCode::Scope, error))
}

fn nonce(prefix: &str) -> String {
    format!("NIB_{prefix}_{}", uuid::Uuid::new_v4().simple())
}

async fn bounded<T>(
    duration: Duration,
    future: impl Future<Output = Result<T, ScenarioFailure>>,
) -> Result<T, ScenarioFailure> {
    tokio::time::timeout(duration, future).await.map_err(|_| {
        ScenarioFailure::harness(
            HarnessFailureCode::ScenarioTimeout,
            "live qualification scenario timed out",
        )
    })?
}

fn classify_failure(
    failure: &ScenarioFailure,
    scenario: Option<ScenarioId>,
) -> (Classification, &'static str) {
    match failure {
        ScenarioFailure::Harness(failure) => (
            failure.code.classification(),
            failure.code.safe_error_class(),
        ),
        ScenarioFailure::Llm(error) => classify_llm_error(error, scenario),
    }
}

fn classify_llm_error(
    error: &LlmError,
    scenario: Option<ScenarioId>,
) -> (Classification, &'static str) {
    match error.class {
        LlmErrorClass::Configuration => (
            Classification::BlockedConfiguration,
            "blocked_configuration",
        ),
        LlmErrorClass::Authentication => (Classification::BlockedAuth, "blocked_auth"),
        LlmErrorClass::RateLimited => (Classification::BlockedRateLimit, "blocked_rate_limit"),
        LlmErrorClass::QuotaOrBilling => {
            if error.http_status == Some(402) {
                (Classification::BlockedBilling, "blocked_billing")
            } else {
                (Classification::BlockedQuota, "blocked_quota")
            }
        }
        LlmErrorClass::UnsupportedRequest
            if scenario == Some(ScenarioId::CompleteText)
                && error.phase == LlmErrorPhase::HttpResponse
                && error.http_status.is_some()
                && error.provider_discriminator()
                    == Some(
                        nib::llm::LlmProviderErrorDiscriminator::DocumentedTransportIncompatibility,
                    ) =>
        {
            (
                Classification::UnsupportedTransport,
                "unsupported_transport",
            )
        }
        LlmErrorClass::ProviderRejected if error.http_status == Some(402) => {
            (Classification::BlockedBilling, "blocked_billing")
        }
        LlmErrorClass::ProviderRejected if error.http_status == Some(451) => {
            (Classification::BlockedRegion, "blocked_region")
        }
        LlmErrorClass::ProviderUnavailable => (Classification::Unknown, "provider_unavailable"),
        LlmErrorClass::Transport => (Classification::Unknown, "transport_failure"),
        LlmErrorClass::Cancelled => (Classification::Unknown, "cancelled"),
        LlmErrorClass::ModelUnavailable => (Classification::FailedAdapter, "model_unavailable"),
        LlmErrorClass::UnsupportedRequest => (Classification::FailedAdapter, "unsupported_request"),
        LlmErrorClass::Protocol => (Classification::FailedAdapter, "protocol"),
        LlmErrorClass::ProviderRejected => (Classification::FailedAdapter, "provider_rejected"),
    }
}

fn bounded_local_detail(value: &str) -> String {
    let mut output = String::new();
    let mut truncated = false;
    for character in value.chars() {
        let escaped = character.escape_default().to_string();
        if output.len().saturating_add(escaped.len()) > MAX_HARNESS_FAILURE_DETAIL_BYTES - 3 {
            truncated = true;
            break;
        }
        output.push_str(&escaped);
    }
    if truncated {
        output.push_str("...");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_live_support::config::LiveLimits;
    use nib::llm::{LlmErrorMetadata, RetryDisposition};
    use std::path::PathBuf;

    fn settings() -> LiveSettings {
        LiveSettings {
            mode: super::super::LiveMode::Canary,
            providers: vec!["openai".to_string()],
            concurrency: 1,
            results_dir: PathBuf::from("target/test"),
            limits: LiveLimits {
                max_logical_requests: 8,
                max_attempts: 24,
                max_output_tokens_per_request: 16,
                max_total_output_tokens: 128,
                max_actual_cost_usd: 10.0,
                max_scenario_duration: Duration::from_secs(30),
                max_provider_duration: Duration::from_secs(60),
                max_run_duration: Duration::from_secs(120),
                allow_unpriced: false,
            },
            meta_base_url: None,
            sensitive_values: Vec::new(),
        }
    }

    fn pricing() -> super::super::CatalogPricing {
        super::super::CatalogPricing {
            prompt_per_token_usd: Some(0.001),
            completion_per_token_usd: Some(0.002),
            request_usd: Some(0.1),
        }
    }

    #[test]
    fn qualification_tool_is_strict_and_inert() {
        let tool = qualification_tool("record_probe");
        let encoded = tool.to_openai_tool();
        assert_eq!(encoded["function"]["strict"], true);
        assert_eq!(
            encoded["function"]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(encoded["function"]["parameters"]["required"][0], "nonce");
    }

    fn typed_error(
        class: LlmErrorClass,
        phase: LlmErrorPhase,
        status: Option<u16>,
        message: &str,
    ) -> ScenarioFailure {
        ScenarioFailure::from(LlmError::new(
            class,
            phase,
            RetryDisposition::NotRetryable,
            LlmErrorMetadata::new("openai", "responses", Some("fixture-model"), status, &[]),
            message,
        ))
    }

    #[test]
    fn typed_error_classification_uses_only_class_status_phase_and_scenario() {
        let fixtures = [
            (
                typed_error(
                    LlmErrorClass::Authentication,
                    LlmErrorPhase::HttpResponse,
                    Some(401),
                    "remote says unsupported quota HTTP 429",
                ),
                Some(ScenarioId::CompleteText),
                Classification::BlockedAuth,
                "blocked_auth",
            ),
            (
                typed_error(
                    LlmErrorClass::RateLimited,
                    LlmErrorPhase::HttpResponse,
                    Some(429),
                    "remote says authentication",
                ),
                Some(ScenarioId::CompleteText),
                Classification::BlockedRateLimit,
                "blocked_rate_limit",
            ),
            (
                typed_error(
                    LlmErrorClass::QuotaOrBilling,
                    LlmErrorPhase::HttpResponse,
                    Some(429),
                    "remote says rate limit",
                ),
                Some(ScenarioId::CompleteText),
                Classification::BlockedQuota,
                "blocked_quota",
            ),
            (
                typed_error(
                    LlmErrorClass::QuotaOrBilling,
                    LlmErrorPhase::HttpResponse,
                    Some(402),
                    "remote says anything",
                ),
                Some(ScenarioId::CompleteText),
                Classification::BlockedBilling,
                "blocked_billing",
            ),
            (
                typed_error(
                    LlmErrorClass::ProviderRejected,
                    LlmErrorPhase::HttpResponse,
                    Some(451),
                    "remote says model unsupported",
                ),
                Some(ScenarioId::CompleteText),
                Classification::BlockedRegion,
                "blocked_region",
            ),
        ];

        for (failure, scenario, expected, safe_class) in fixtures {
            assert_eq!(classify_failure(&failure, scenario), (expected, safe_class));
        }
    }

    #[test]
    fn unsupported_transport_requires_a_typed_http_basic_probe_failure() {
        let local = typed_error(
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::Request,
            None,
            "HTTP 400 unsupported transport",
        );
        assert_eq!(
            classify_failure(&local, Some(ScenarioId::CompleteText)).0,
            Classification::FailedAdapter
        );

        let tool = typed_error(
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::HttpResponse,
            Some(400),
            "complete transport unsupported",
        );
        assert_eq!(
            classify_failure(&tool, Some(ScenarioId::SingleToolContinuation)).0,
            Classification::FailedAdapter
        );

        let prose_only = typed_error(
            LlmErrorClass::ProviderRejected,
            LlmErrorPhase::HttpResponse,
            Some(400),
            "model unsupported because remote said so; HTTP 429; authentication",
        );
        assert_eq!(
            classify_failure(&prose_only, Some(ScenarioId::CompleteText)).0,
            Classification::FailedAdapter
        );

        let generic_http = typed_error(
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::HttpResponse,
            Some(422),
            "generic invalid request",
        );
        assert_eq!(
            classify_failure(&generic_http, Some(ScenarioId::CompleteText)).0,
            Classification::FailedAdapter
        );

        let documented = match typed_error(
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::HttpResponse,
            Some(400),
            "provider-owned structural incompatibility",
        ) {
            ScenarioFailure::Llm(error) => {
                ScenarioFailure::Llm(error.with_documented_transport_incompatibility())
            }
            ScenarioFailure::Harness(_) => unreachable!("typed error fixture"),
        };
        assert_eq!(
            classify_failure(&documented, Some(ScenarioId::CompleteText)).0,
            Classification::UnsupportedTransport
        );
    }

    #[test]
    fn local_harness_failures_are_bounded_escaped_and_structural() {
        let failure = HarnessFailure::new(
            HarnessFailureCode::Configuration,
            format!("{}\nsecret", "x".repeat(1_000)),
        );
        assert!(failure.detail.len() <= MAX_HARNESS_FAILURE_DETAIL_BYTES);
        assert!(!failure.detail.contains('\n'));
        assert_eq!(failure.code, HarnessFailureCode::Configuration);
        assert!(!format!("{failure:?}").contains("secret"));
        assert_eq!(
            classify_failure(&ScenarioFailure::Harness(failure), None),
            (
                Classification::BlockedConfiguration,
                "blocked_configuration"
            )
        );
    }

    #[tokio::test]
    async fn scenario_timeout_is_a_local_failure_code() {
        let failure = bounded(
            Duration::from_millis(1),
            std::future::pending::<Result<(), ScenarioFailure>>(),
        )
        .await
        .expect_err("pending scenario must time out");
        assert_eq!(
            classify_failure(&failure, Some(ScenarioId::CompleteText)),
            (Classification::BlockedBudget, "scenario_timeout")
        );

        let mut settings = settings();
        settings.limits.max_scenario_duration = Duration::ZERO;
        let mut run = RunBudget::new();
        let mut provider = ProviderBudget::new(&settings, &mut run);
        let execution = ScenarioContext::new(&mut provider, None).finish(Ok(()));
        assert_eq!(
            classify_failure(execution.result.as_ref().unwrap_err(), None).0,
            Classification::BlockedBudget
        );
    }

    #[test]
    fn nonces_are_unique_short_ascii_tokens() {
        let first = nonce("test");
        let second = nonce("test");
        assert_ne!(first, second);
        assert!(first.is_ascii());
        assert!(first.len() < 128);
    }

    #[test]
    fn text_qualification_requires_the_complete_finish_class() {
        let nonce = "NIB_TEXT_FIXTURE";
        validate_text_response(LlmResponse::text(nonce), nonce).unwrap();

        let mut wrong_finish = LlmResponse::text(nonce);
        wrong_finish.finish_reason = LlmFinishReason::ToolCalls;
        let failure = validate_text_response(wrong_finish, nonce).unwrap_err();

        assert_eq!(
            classify_failure(&failure, Some(ScenarioId::CompleteText)),
            (Classification::FailedAdapter, "response_mismatch")
        );
    }

    #[test]
    fn scenario_evidence_aggregates_two_requests_attempts_usage_and_cost() {
        let mut evidence = ScenarioEvidence::new();
        evidence.begin_request().unwrap();
        evidence
            .record_result(
                Some(2),
                Some(LlmUsage::new(10, 5, 15, Some(2), Some(1)).unwrap()),
            )
            .unwrap();
        evidence.begin_request().unwrap();
        evidence
            .record_result(Some(1), Some(LlmUsage::new(4, 2, 6, None, None).unwrap()))
            .unwrap();
        let report = evidence.report(
            ScenarioId::SingleToolContinuation,
            7,
            Some(&pricing()),
            Ok(()),
        );

        assert!(report.passed);
        assert_eq!(report.actual_logical_requests, 2);
        assert_eq!(report.actual_attempts, Some(3));
        assert_eq!(report.usage_requests, 2);
        assert_eq!(report.usage_complete_requests, 1);
        assert_eq!(
            report.usage_completeness,
            report::UsageCompleteness::Partial
        );
        assert_eq!(
            report.usage.unwrap(),
            LlmUsage::new(14, 7, 21, None, None).unwrap()
        );
        assert_eq!(report.actual_cost_usd, None);
    }

    #[test]
    fn typed_success_and_error_attempt_metadata_is_persisted_without_parsing() {
        let success_attempts: RetryAttemptMetadata = serde_json::from_value(json!({
            "attempts": 2,
            "credential_rotation_occurred": false,
            "retry_exhausted": false,
            "final_retry_after_seconds": null
        }))
        .unwrap();
        let error_attempts: RetryAttemptMetadata = serde_json::from_value(json!({
            "attempts": 3,
            "credential_rotation_occurred": true,
            "retry_exhausted": true,
            "final_retry_after_seconds": 1
        }))
        .unwrap();
        let success = LlmResponse::text("fixture").with_retry_attempts(success_attempts);
        let error = LlmError::transport("openai", "responses", Some("fixture"), "fixture", &[])
            .with_retry_attempts(error_attempts);
        let mut evidence = ScenarioEvidence::new();
        evidence.begin_request().unwrap();
        evidence
            .record_result(Some(usize::from(success.attempts.attempts())), None)
            .unwrap();
        evidence.begin_request().unwrap();
        evidence
            .record_result(Some(usize::from(error.attempts.attempts())), None)
            .unwrap();

        let report = evidence.report(
            ScenarioId::SingleToolContinuation,
            1,
            Some(&pricing()),
            Err(ScenarioFailure::from(error)),
        );
        assert_eq!(report.actual_logical_requests, 2);
        assert_eq!(report.actual_attempts, Some(5));
        assert_eq!(
            report.usage_completeness,
            report::UsageCompleteness::Unknown
        );
        assert!(!report.passed);
    }

    #[test]
    fn scenario_evidence_marks_partial_and_unknown_usage_without_guessing_cost() {
        let mut partial = ScenarioEvidence::new();
        partial.begin_request().unwrap();
        partial
            .record_result(Some(1), Some(LlmUsage::new(3, 2, 5, None, None).unwrap()))
            .unwrap();
        partial.begin_request().unwrap();
        partial.record_result(Some(2), None).unwrap();
        let partial = partial.report(
            ScenarioId::SingleToolContinuation,
            1,
            Some(&pricing()),
            Err(ScenarioFailure::harness(
                HarnessFailureCode::ResponseMismatch,
                "fixture",
            )),
        );
        assert_eq!(
            partial.usage_completeness,
            report::UsageCompleteness::Partial
        );
        assert!(partial.usage.is_some());
        assert!(partial.actual_cost_usd.is_none());

        let mut unknown = ScenarioEvidence::new();
        unknown.begin_request().unwrap();
        unknown.record_result(Some(1), None).unwrap();
        let unknown = unknown.report(
            ScenarioId::CompleteText,
            1,
            Some(&pricing()),
            Err(ScenarioFailure::harness(
                HarnessFailureCode::ResponseMismatch,
                "fixture",
            )),
        );
        assert_eq!(
            unknown.usage_completeness,
            report::UsageCompleteness::Unknown
        );
        assert!(unknown.usage.is_none());
        assert!(unknown.actual_cost_usd.is_none());
    }

    #[test]
    fn scenario_usage_aggregation_fails_checked_on_the_bounded_limit() {
        let mut evidence = ScenarioEvidence::new();
        evidence.begin_request().unwrap();
        evidence
            .record_result(
                Some(1),
                Some(LlmUsage::new(1_000_000_000, 0, 1_000_000_000, None, None).unwrap()),
            )
            .unwrap();
        evidence.begin_request().unwrap();
        assert!(evidence
            .record_result(Some(1), Some(LlmUsage::new(1, 0, 1, None, None).unwrap()))
            .is_err());
    }

    #[test]
    fn every_cumulative_budget_blocks_before_mutating_request_counts() {
        enum Limit {
            Logical,
            Attempts,
            Output,
            Cost,
            ScenarioDeadline,
            ProviderDeadline,
            RunDeadline,
        }
        for limit in [
            Limit::Logical,
            Limit::Attempts,
            Limit::Output,
            Limit::Cost,
            Limit::ScenarioDeadline,
            Limit::ProviderDeadline,
            Limit::RunDeadline,
        ] {
            let mut settings = settings();
            let priced = pricing();
            match limit {
                Limit::Logical => settings.limits.max_logical_requests = 0,
                Limit::Attempts => settings.limits.max_attempts = 2,
                Limit::Output => settings.limits.max_total_output_tokens = 15,
                Limit::Cost => settings.limits.max_actual_cost_usd = 0.01,
                Limit::ScenarioDeadline => settings.limits.max_scenario_duration = Duration::ZERO,
                Limit::ProviderDeadline => settings.limits.max_provider_duration = Duration::ZERO,
                Limit::RunDeadline => settings.limits.max_run_duration = Duration::ZERO,
            }
            let run = RunBudget::new();
            let mut provider = ProviderBudget::new(&settings, &run);
            let failure = provider
                .begin_request(Instant::now(), Some(&priced))
                .expect_err("budget must reject before I/O");
            assert_eq!(
                classify_failure(&failure, None).0,
                Classification::BlockedBudget
            );
            assert_eq!(lock_ledger(&provider.ledger).unwrap().logical_requests, 0);
            assert_eq!(
                lock_ledger(&provider.run.ledger).unwrap().logical_requests,
                0
            );
        }
    }

    #[test]
    fn provider_and_run_ledgers_enforce_cumulative_headroom_independently() {
        let settings = settings();
        let priced = pricing();
        let run = RunBudget::new();
        lock_ledger(&run.ledger).unwrap().logical_requests = settings.limits.max_logical_requests;
        let mut provider = ProviderBudget::new(&settings, &run);
        assert!(provider
            .begin_request(Instant::now(), Some(&priced))
            .is_err());
        assert_eq!(lock_ledger(&provider.ledger).unwrap().logical_requests, 0);

        lock_ledger(&provider.run.ledger).unwrap().logical_requests = 0;
        lock_ledger(&provider.ledger).unwrap().logical_requests =
            settings.limits.max_logical_requests;
        assert!(provider
            .begin_request(Instant::now(), Some(&priced))
            .is_err());
        assert_eq!(
            lock_ledger(&provider.run.ledger).unwrap().logical_requests,
            0
        );
    }
}
