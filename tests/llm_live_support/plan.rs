use super::config::LiveSettings;
use super::{
    registry_transports, CatalogModel, CatalogSnapshot, Classification, LiveMode, ModelProfile,
    ScenarioId, SelectedSuiteEvidence, TransportId, NETWORK_PROVIDERS,
};
use chrono::NaiveDate;
use nib::llm::registry::provider_descriptor;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const OPENROUTER_ALLOWLIST: &str = include_str!("../fixtures/llm_live/openrouter_models.toml");
const SELECTED_MODELS: &str = include_str!("../fixtures/llm_live/selected_models.toml");
// Includes the synthetic prompt, system defaults, and bounded inert tool schemas.
const ESTIMATED_PROMPT_TOKENS_PER_REQUEST: usize = 512;
const MAX_ALLOWLIST_MODELS: usize = 64;
const MAX_SELECTED_MODELS_PER_PROVIDER: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedMatrixFile {
    version: u32,
    suite_id: String,
    owner: String,
    reviewed_at: String,
    expires_at: String,
    required_scenarios: Vec<ScenarioId>,
    conditional_scenarios: Vec<ScenarioId>,
    providers: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
pub(super) struct SelectedMatrix {
    suite_id: String,
    owner: String,
    reviewed_at: String,
    expires_at: String,
    required_scenarios: BTreeSet<ScenarioId>,
    conditional_scenarios: BTreeSet<ScenarioId>,
    providers: BTreeMap<String, Vec<String>>,
    fingerprint: String,
}

impl SelectedMatrix {
    pub fn load_default() -> Result<Self, String> {
        Self::parse(SELECTED_MODELS, chrono::Utc::now().date_naive())
    }

    fn parse(input: &str, today: NaiveDate) -> Result<Self, String> {
        if input.len() > 256 * 1024 {
            return Err("selected LLM matrix exceeds the byte limit".to_string());
        }
        let file = toml::from_str::<SelectedMatrixFile>(input)
            .map_err(|error| format!("invalid selected LLM matrix: {error}"))?;
        if file.version != 1 {
            return Err("selected LLM matrix version must be 1".to_string());
        }
        validate_matrix_label(&file.suite_id, "suite_id", 128)?;
        validate_matrix_label(&file.owner, "owner", 128)?;
        if !file
            .suite_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("selected LLM matrix suite_id is invalid".to_string());
        }
        let reviewed = NaiveDate::parse_from_str(&file.reviewed_at, "%Y-%m-%d")
            .map_err(|_| "selected LLM matrix reviewed_at must use YYYY-MM-DD".to_string())?;
        let expires = NaiveDate::parse_from_str(&file.expires_at, "%Y-%m-%d")
            .map_err(|_| "selected LLM matrix expires_at must use YYYY-MM-DD".to_string())?;
        if reviewed > today || expires < reviewed || expires < today {
            return Err("selected LLM matrix review dates are invalid or expired".to_string());
        }
        let required_scenarios = file
            .required_scenarios
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let conditional_scenarios = file
            .conditional_scenarios
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if required_scenarios.len() != file.required_scenarios.len()
            || conditional_scenarios.len() != file.conditional_scenarios.len()
        {
            return Err("selected LLM matrix contains duplicate scenarios".to_string());
        }
        let baseline = BTreeSet::from([
            ScenarioId::CompleteText,
            ScenarioId::StreamedText,
            ScenarioId::SingleToolContinuation,
        ]);
        if required_scenarios != baseline {
            return Err(
                "selected LLM matrix must require exactly complete, stream, and single-tool tasks"
                    .to_string(),
            );
        }
        if conditional_scenarios != BTreeSet::from([ScenarioId::ParallelToolContinuation]) {
            return Err(
                "selected LLM matrix must conditionally require exactly parallel-tool continuation"
                    .to_string(),
            );
        }
        let expected = NETWORK_PROVIDERS
            .iter()
            .map(|provider| (*provider).to_string())
            .collect::<BTreeSet<_>>();
        let actual = file.providers.keys().cloned().collect::<BTreeSet<_>>();
        if actual != expected {
            return Err(
                "selected LLM matrix must define exactly every network provider".to_string(),
            );
        }
        for (provider, models) in &file.providers {
            if models.is_empty() || models.len() > MAX_SELECTED_MODELS_PER_PROVIDER {
                return Err(format!(
                    "selected LLM matrix provider '{provider}' must contain 1..={MAX_SELECTED_MODELS_PER_PROVIDER} models"
                ));
            }
            let mut unique = BTreeSet::new();
            for model in models {
                validate_selected_model_id(provider, model)?;
                if !unique.insert(model) {
                    return Err(format!(
                        "selected LLM matrix provider '{provider}' contains duplicate model IDs"
                    ));
                }
            }
        }
        let fingerprint = format!("{:x}", Sha256::digest(input.as_bytes()));
        Ok(Self {
            suite_id: file.suite_id,
            owner: file.owner,
            reviewed_at: file.reviewed_at,
            expires_at: file.expires_at,
            required_scenarios,
            conditional_scenarios,
            providers: file.providers,
            fingerprint,
        })
    }

    pub fn evidence(&self) -> SelectedSuiteEvidence {
        SelectedSuiteEvidence {
            suite_id: self.suite_id.clone(),
            matrix_sha256: self.fingerprint.clone(),
            owner: self.owner.clone(),
            reviewed_at: self.reviewed_at.clone(),
            expires_at: self.expires_at.clone(),
            required_task_count: self.required_scenarios.len(),
            conditional_task_count: self.conditional_scenarios.len(),
        }
    }

    fn models_for(&self, provider: &str) -> Result<&[String], String> {
        self.providers
            .get(provider)
            .map(Vec::as_slice)
            .ok_or_else(|| format!("selected LLM matrix is missing provider '{provider}'"))
    }
}

fn validate_matrix_label(value: &str, label: &str, maximum: usize) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(format!("selected LLM matrix {label} is invalid"));
    }
    Ok(())
}

fn validate_selected_model_id(provider: &str, value: &str) -> Result<(), String> {
    super::validate_catalog_identifier(value, "selected model ID")?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
        || value.contains('*')
        || value.contains(',')
        || value.rsplit('/').next().is_some_and(|part| {
            part.eq_ignore_ascii_case("auto")
                || part.eq_ignore_ascii_case("latest")
                || part.to_ascii_lowercase().ends_with("-latest")
        })
        || (provider == "openrouter" && !value.contains('/'))
    {
        return Err(format!(
            "selected LLM matrix provider '{provider}' has a non-canonical model ID"
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AllowlistFile {
    version: u32,
    #[serde(default)]
    model: Vec<AllowlistEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AllowlistEntry {
    id: String,
    approved: bool,
    transports: BTreeSet<TransportId>,
    required_scenarios: BTreeSet<ScenarioId>,
    #[serde(default)]
    required_parameters: BTreeSet<String>,
    rationale: String,
    owner: String,
    reviewed_at: String,
    expires_at: String,
    #[serde(default)]
    max_projected_cost_usd: Option<f64>,
}

#[derive(Debug, Clone)]
pub(super) struct OpenRouterAllowlist {
    entries: BTreeMap<String, AllowlistEntry>,
}

impl OpenRouterAllowlist {
    pub fn load_default() -> Result<Self, String> {
        Self::parse(OPENROUTER_ALLOWLIST, chrono::Utc::now().date_naive())
    }

    fn parse(input: &str, today: NaiveDate) -> Result<Self, String> {
        if input.len() > 256 * 1024 {
            return Err("OpenRouter allowlist exceeds the byte limit".to_string());
        }
        let file = toml::from_str::<AllowlistFile>(input)
            .map_err(|error| format!("invalid OpenRouter allowlist: {error}"))?;
        if file.version != 1 {
            return Err("OpenRouter allowlist version must be 1".to_string());
        }
        if file.model.is_empty() || file.model.len() > MAX_ALLOWLIST_MODELS {
            return Err(format!(
                "OpenRouter allowlist must contain 1..={MAX_ALLOWLIST_MODELS} models"
            ));
        }
        let mut entries = BTreeMap::new();
        for entry in file.model {
            validate_entry(&entry, today)?;
            let id = entry.id.clone();
            if entries.insert(id, entry).is_some() {
                return Err("OpenRouter allowlist contains duplicate model IDs".to_string());
            }
        }
        Ok(Self { entries })
    }
}

fn validate_entry(entry: &AllowlistEntry, today: NaiveDate) -> Result<(), String> {
    super::validate_catalog_identifier(&entry.id, "OpenRouter allowlist ID")?;
    if !entry.id.contains('/')
        || entry.id.starts_with("openrouter/")
        || entry.id.contains('*')
        || entry.id.contains(',')
        || entry.id.contains(':')
        || entry
            .id
            .split('/')
            .any(|part| part.eq_ignore_ascii_case("auto") || part.eq_ignore_ascii_case("latest"))
    {
        return Err(
            "OpenRouter allowlist IDs must be exact canonical owner/model slugs".to_string(),
        );
    }
    if entry.transports.is_empty()
        || entry.transports.iter().any(|transport| {
            !matches!(
                transport,
                TransportId::ChatCompletions | TransportId::Responses
            )
        })
    {
        return Err("OpenRouter allowlist entry has invalid transports".to_string());
    }
    if !entry.required_scenarios.contains(&ScenarioId::CompleteText)
        || !entry.required_scenarios.contains(&ScenarioId::StreamedText)
    {
        return Err(
            "OpenRouter allowlist entries must require complete and stream scenarios".to_string(),
        );
    }
    if entry
        .required_scenarios
        .contains(&ScenarioId::SingleToolContinuation)
        && !entry.required_parameters.contains("tools")
    {
        return Err(
            "OpenRouter tool scenarios must require the tools catalog parameter".to_string(),
        );
    }
    if entry
        .required_scenarios
        .contains(&ScenarioId::ParallelToolContinuation)
        && (!entry.required_parameters.contains("tools")
            || !entry.required_parameters.contains("parallel_tool_calls"))
    {
        return Err(
            "OpenRouter parallel-tool scenarios must require tools and parallel_tool_calls"
                .to_string(),
        );
    }
    if entry.required_parameters.len() > 32
        || entry.required_parameters.iter().any(|parameter| {
            parameter.is_empty()
                || parameter.len() > 64
                || !parameter
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err("OpenRouter allowlist required parameters are invalid".to_string());
    }
    for (label, value, max) in [
        ("rationale", entry.rationale.as_str(), 1024usize),
        ("owner", entry.owner.as_str(), 128usize),
    ] {
        if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
            return Err(format!("OpenRouter allowlist {label} is invalid"));
        }
    }
    let reviewed = NaiveDate::parse_from_str(&entry.reviewed_at, "%Y-%m-%d")
        .map_err(|_| "OpenRouter allowlist reviewed_at must use YYYY-MM-DD".to_string())?;
    let expires = NaiveDate::parse_from_str(&entry.expires_at, "%Y-%m-%d")
        .map_err(|_| "OpenRouter allowlist expires_at must use YYYY-MM-DD".to_string())?;
    if reviewed > today || expires < reviewed || expires < today {
        return Err("OpenRouter allowlist review dates are invalid or expired".to_string());
    }
    if entry
        .max_projected_cost_usd
        .is_some_and(|value| !value.is_finite() || value <= 0.0 || value > 100.0)
    {
        return Err("OpenRouter allowlist cost ceiling is invalid".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(super) struct AccountingEntry {
    pub model: CatalogModel,
    pub classification: Classification,
}

#[derive(Debug, Clone)]
pub(super) struct RunPlan {
    pub provider: String,
    pub accounting: Vec<AccountingEntry>,
    pub profiles: Vec<ModelProfile>,
    pub logical_requests: usize,
    pub maximum_attempts: usize,
    pub maximum_output_tokens: usize,
    pub projected_cost_usd: Option<f64>,
}

pub(super) fn build_plan(
    settings: &LiveSettings,
    snapshot: &CatalogSnapshot,
    allowlist: &OpenRouterAllowlist,
    selected_matrix: Option<&SelectedMatrix>,
) -> Result<RunPlan, String> {
    if settings.mode == LiveMode::Catalog {
        let accounting = snapshot
            .models
            .iter()
            .cloned()
            .map(|model| AccountingEntry {
                classification: if model.supports_text_generation == Some(false) {
                    Classification::NotApplicable
                } else {
                    Classification::RequiresProbe
                },
                model,
            })
            .collect();
        return Ok(RunPlan {
            provider: snapshot.provider.clone(),
            accounting,
            profiles: Vec::new(),
            logical_requests: 0,
            maximum_attempts: 0,
            maximum_output_tokens: 0,
            projected_cost_usd: Some(0.0),
        });
    }

    let selected_suite =
        if settings.mode == LiveMode::Selected {
            Some(selected_matrix.ok_or_else(|| {
                "selected mode requires a validated selected LLM matrix".to_string()
            })?)
        } else {
            None
        };
    let selected_ids = selected_suite
        .map(|matrix| matrix.models_for(&snapshot.provider))
        .transpose()?;
    let selected = if snapshot.provider == "openrouter" {
        select_openrouter_models(snapshot, allowlist, selected_ids)?
    } else {
        select_direct_models(settings.mode, snapshot, selected_ids)?
    };
    let selected_model_ids = selected
        .iter()
        .map(|(model, _)| model.id.as_str())
        .collect::<BTreeSet<_>>();
    let accounting = snapshot
        .models
        .iter()
        .cloned()
        .map(|model| AccountingEntry {
            classification: if model.supports_text_generation == Some(false)
                || ((snapshot.provider == "openrouter" || settings.mode == LiveMode::Selected)
                    && !selected_model_ids.contains(model.id.as_str()))
            {
                Classification::NotApplicable
            } else {
                Classification::RequiresProbe
            },
            model,
        })
        .collect::<Vec<_>>();

    let descriptor = provider_descriptor(&snapshot.provider)
        .ok_or_else(|| "live plan provider is not registered".to_string())?;
    let advertised_ids = descriptor
        .models()
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let default_transports = registry_transports(&snapshot.provider)?;
    let mut profiles = Vec::new();
    for (model, allowlist_entry) in selected {
        let advertised = snapshot.provider == "openrouter"
            || advertised_ids.contains(model.id.as_str())
            || model
                .aliases
                .iter()
                .any(|alias| advertised_ids.contains(alias.as_str()));
        let transports = allowlist_entry
            .map(|entry| entry.transports.iter().copied().collect::<Vec<_>>())
            .unwrap_or_else(|| default_transports.clone());
        for transport in transports {
            let (scenarios, not_applicable_scenarios) = if let Some(matrix) = selected_suite {
                let mut scenarios = matrix.required_scenarios.clone();
                let mut not_applicable = matrix.conditional_scenarios.clone();
                if model.supports_parallel_tools == Some(true) {
                    scenarios.extend(matrix.conditional_scenarios.iter().copied());
                    not_applicable.clear();
                }
                if let Some(entry) = allowlist_entry {
                    if !scenarios.is_subset(&entry.required_scenarios) {
                        return Err(format!(
                            "OpenRouter selected model '{}' is blocked_configuration because its task suite is not approved by the allowlist",
                            entry.id
                        ));
                    }
                }
                (scenarios, not_applicable)
            } else {
                let mut scenarios =
                    BTreeSet::from([ScenarioId::CompleteText, ScenarioId::StreamedText]);
                if let Some(entry) = allowlist_entry {
                    scenarios.extend(entry.required_scenarios.iter().copied());
                } else if model.supports_tools != Some(false) || advertised {
                    scenarios.insert(ScenarioId::SingleToolContinuation);
                }
                if model.supports_parallel_tools == Some(true) {
                    scenarios.insert(ScenarioId::ParallelToolContinuation);
                }
                (scenarios, BTreeSet::new())
            };
            profiles.push(ModelProfile {
                model: model.clone(),
                transport,
                advertised,
                required_scenarios: scenarios,
                not_applicable_scenarios,
                projected_cost_ceiling_usd: allowlist_entry
                    .and_then(|entry| entry.max_projected_cost_usd),
            });
        }
    }

    let logical_requests = profiles
        .iter()
        .flat_map(|profile| profile.required_scenarios.iter())
        .map(|scenario| scenario.logical_requests())
        .try_fold(0usize, usize::checked_add)
        .ok_or_else(|| "live request plan overflowed".to_string())?;
    if logical_requests > settings.limits.max_logical_requests {
        return Err(format!(
            "provider '{}' is blocked_budget: planned {logical_requests} logical requests exceeds the configured {} limit",
            snapshot.provider, settings.limits.max_logical_requests
        ));
    }
    let maximum_attempts = logical_requests
        .checked_mul(3)
        .ok_or_else(|| "live maximum attempt count overflowed".to_string())?;
    let maximum_output_tokens = logical_requests
        .checked_mul(settings.limits.max_output_tokens_per_request as usize)
        .ok_or_else(|| "live maximum output token count overflowed".to_string())?;
    let projected_cost_usd = projected_cost(&profiles, settings)?;

    Ok(RunPlan {
        provider: snapshot.provider.clone(),
        accounting,
        profiles,
        logical_requests,
        maximum_attempts,
        maximum_output_tokens,
        projected_cost_usd,
    })
}

fn select_direct_models(
    mode: LiveMode,
    snapshot: &CatalogSnapshot,
    selected_ids: Option<&[String]>,
) -> Result<Vec<(CatalogModel, Option<&'static AllowlistEntry>)>, String> {
    let descriptor = provider_descriptor(&snapshot.provider)
        .ok_or_else(|| "direct provider is not registered".to_string())?;
    if mode == LiveMode::Full {
        for advertised in descriptor.models() {
            if !snapshot.models.iter().any(|model| {
                model.id == *advertised || model.aliases.iter().any(|alias| alias == advertised)
            }) {
                return Err(format!(
                    "provider '{}' advertised model is absent from its live catalog",
                    snapshot.provider
                ));
            }
        }
    }
    if mode == LiveMode::Canary {
        let model = snapshot
            .models
            .iter()
            .find(|model| {
                model.id == descriptor.default_model()
                    || model
                        .aliases
                        .iter()
                        .any(|alias| alias == descriptor.default_model())
            })
            .cloned()
            .ok_or_else(|| {
                format!(
                    "provider '{}' default model is absent from its live catalog",
                    snapshot.provider
                )
            })?;
        return Ok(vec![(model, None)]);
    }
    if mode == LiveMode::Selected {
        let selected_ids = selected_ids
            .ok_or_else(|| "selected mode is missing direct-provider model IDs".to_string())?;
        let catalog = snapshot
            .models
            .iter()
            .map(|model| (model.id.as_str(), model))
            .collect::<BTreeMap<_, _>>();
        return selected_ids
            .iter()
            .map(|id| {
                catalog
                    .get(id.as_str())
                    .map(|model| ((*model).clone(), None))
                    .ok_or_else(|| {
                        format!(
                            "provider '{}' selected model '{}' is absent from its live canonical catalog",
                            snapshot.provider, id
                        )
                    })
            })
            .collect();
    }
    Ok(snapshot
        .models
        .iter()
        .filter(|model| model.supports_text_generation != Some(false))
        .cloned()
        .map(|model| (model, None))
        .collect())
}

fn select_openrouter_models<'a>(
    snapshot: &CatalogSnapshot,
    allowlist: &'a OpenRouterAllowlist,
    selected_ids: Option<&[String]>,
) -> Result<Vec<(CatalogModel, Option<&'a AllowlistEntry>)>, String> {
    let catalog = snapshot
        .models
        .iter()
        .map(|model| (model.id.as_str(), model))
        .collect::<BTreeMap<_, _>>();
    let mut selected = Vec::new();
    let ids = selected_ids
        .map(|ids| ids.to_vec())
        .unwrap_or_else(|| allowlist.entries.keys().cloned().collect());
    for id in ids {
        let entry = allowlist.entries.get(&id).ok_or_else(|| {
            format!(
                "OpenRouter selected model '{id}' is blocked_configuration because it is absent from the reviewed allowlist"
            )
        })?;
        if !entry.approved {
            return Err(format!(
                "OpenRouter allowlist entry '{}' is blocked_configuration pending human approval",
                entry.id
            ));
        }
        let model = catalog.get(entry.id.as_str()).ok_or_else(|| {
            format!(
                "OpenRouter allowlist entry '{}' is absent from the live canonical catalog",
                entry.id
            )
        })?;
        if model.supports_text_generation != Some(true) {
            return Err(format!(
                "OpenRouter allowlist entry '{}' is not a confirmed text model",
                entry.id
            ));
        }
        if entry.required_parameters.contains("tools") && model.supports_tools != Some(true) {
            return Err(format!(
                "OpenRouter allowlist entry '{}' no longer advertises tools",
                entry.id
            ));
        }
        if !entry
            .required_parameters
            .is_subset(&model.supported_parameters)
        {
            return Err(format!(
                "OpenRouter allowlist entry '{}' is missing a required catalog parameter",
                entry.id
            ));
        }
        selected.push(((*model).clone(), Some(entry)));
    }
    Ok(selected)
}

fn projected_cost(
    profiles: &[ModelProfile],
    settings: &LiveSettings,
) -> Result<Option<f64>, String> {
    let mut total = 0.0f64;
    let mut unpriced = false;
    let mut model_costs = BTreeMap::<&str, (f64, f64)>::new();
    for profile in profiles {
        let requests = profile
            .required_scenarios
            .iter()
            .map(|scenario| scenario.logical_requests())
            .sum::<usize>();
        let Some(pricing) = &profile.model.pricing else {
            unpriced = true;
            continue;
        };
        let (Some(prompt_price), Some(completion_price)) = (
            pricing.prompt_per_token_usd,
            pricing.completion_per_token_usd,
        ) else {
            unpriced = true;
            continue;
        };
        let prompt = prompt_price * ESTIMATED_PROMPT_TOKENS_PER_REQUEST as f64;
        let completion = completion_price * settings.limits.max_output_tokens_per_request as f64;
        let request = pricing.request_usd.unwrap_or_default();
        let profile_cost = (prompt + completion + request) * requests as f64 * 3.0;
        if !profile_cost.is_finite() {
            return Err(
                "live provider plan is blocked_budget because projected cost overflowed"
                    .to_string(),
            );
        }
        total += profile_cost;
        if !total.is_finite() {
            return Err(
                "live provider plan is blocked_budget because projected cost overflowed"
                    .to_string(),
            );
        }
        if let Some(ceiling) = profile.projected_cost_ceiling_usd {
            let entry = model_costs
                .entry(profile.model.id.as_str())
                .or_insert((0.0, ceiling));
            entry.0 += profile_cost;
            entry.1 = entry.1.min(ceiling);
        }
    }
    if unpriced && !settings.limits.allow_unpriced {
        return Err(
            "live provider plan is blocked_budget because catalog pricing is incomplete; set NIB_LIVE_ALLOW_UNPRICED=1 only with a provider-side hard spend cap"
                .to_string(),
        );
    }
    if model_costs
        .values()
        .any(|(projected, ceiling)| projected > ceiling)
    {
        return Err(
            "live provider plan is blocked_budget because an OpenRouter entry exceeds its projected cost ceiling"
                .to_string(),
        );
    }
    Ok((!unpriced).then_some(total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_live_support::config::{LiveLimits, LiveSettings};
    use chrono::Utc;
    use std::path::PathBuf;
    use std::time::Duration;

    fn model(id: &str, text: Option<bool>, tools: Option<bool>) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            aliases: Vec::new(),
            supports_text_generation: text,
            supports_tools: tools,
            supports_parallel_tools: Some(false),
            supported_parameters: tools
                .is_some_and(|supported| supported)
                .then(|| "tools".to_string())
                .into_iter()
                .collect(),
            public_identifier: true,
            owner: None,
            pricing: Some(super::super::CatalogPricing {
                prompt_per_token_usd: Some(0.0),
                completion_per_token_usd: Some(0.0),
                request_usd: Some(0.0),
            }),
            expiration_date: None,
        }
    }

    fn settings(mode: LiveMode) -> LiveSettings {
        LiveSettings {
            mode,
            providers: vec!["openai".to_string()],
            results_dir: PathBuf::from("target/test"),
            limits: LiveLimits {
                max_logical_requests: 100,
                max_output_tokens_per_request: 64,
                max_scenario_duration: Duration::from_secs(30),
                max_provider_duration: Duration::from_secs(60),
                allow_unpriced: false,
            },
            meta_base_url: None,
            sensitive_values: Vec::new(),
        }
    }

    fn snapshot(provider: &str, models: Vec<CatalogModel>) -> CatalogSnapshot {
        CatalogSnapshot {
            provider: provider.to_string(),
            captured_at: Utc::now(),
            page_count: 1,
            models,
        }
    }

    #[test]
    fn catalog_plan_accounts_for_every_entry_without_requests() {
        let allowlist = OpenRouterAllowlist::load_default().unwrap();
        let run = build_plan(
            &settings(LiveMode::Catalog),
            &snapshot(
                "openai",
                vec![
                    model("gpt-4o", Some(true), None),
                    model("embed", Some(false), None),
                ],
            ),
            &allowlist,
            None,
        )
        .unwrap();
        assert_eq!(run.accounting.len(), 2);
        assert_eq!(run.logical_requests, 0);
        assert!(run.profiles.is_empty());
        assert_eq!(
            run.accounting[1].classification,
            Classification::NotApplicable
        );
    }

    #[test]
    fn canary_uses_only_default_model_and_dry_run_is_bounded() {
        let allowlist = OpenRouterAllowlist::load_default().unwrap();
        let default_model = provider_descriptor("openai").unwrap().default_model();
        let run = build_plan(
            &settings(LiveMode::Canary),
            &snapshot(
                "openai",
                vec![
                    model(default_model, Some(true), Some(true)),
                    model("other", Some(true), None),
                ],
            ),
            &allowlist,
            None,
        )
        .unwrap();
        assert_eq!(run.profiles.len(), 2, "OpenAI has two registry transports");
        assert_eq!(run.logical_requests, 8);
        assert_eq!(run.maximum_attempts, 24);
        assert_eq!(run.maximum_output_tokens, 512);
    }

    #[test]
    fn selected_plan_runs_core_tasks_only_for_exact_configured_models() {
        let allowlist = OpenRouterAllowlist::load_default().unwrap();
        let matrix = SelectedMatrix::parse(
            SELECTED_MODELS,
            NaiveDate::from_ymd_opt(2026, 8, 17).unwrap(),
        )
        .unwrap();
        let selected = matrix.models_for("openai").unwrap()[0].clone();
        let run = build_plan(
            &settings(LiveMode::Selected),
            &snapshot(
                "openai",
                vec![
                    model(&selected, Some(true), Some(true)),
                    model("unselected-model", Some(true), Some(true)),
                ],
            ),
            &allowlist,
            Some(&matrix),
        )
        .unwrap();

        assert_eq!(run.profiles.len(), 2, "OpenAI has two registry transports");
        assert!(run.profiles.iter().all(|profile| {
            profile.model.id == selected
                && profile.required_scenarios
                    == BTreeSet::from([
                        ScenarioId::CompleteText,
                        ScenarioId::StreamedText,
                        ScenarioId::SingleToolContinuation,
                    ])
                && profile.not_applicable_scenarios
                    == BTreeSet::from([ScenarioId::ParallelToolContinuation])
        }));
        assert_eq!(run.logical_requests, 8);
        assert_eq!(run.accounting.len(), 2);
        assert_eq!(
            run.accounting[1].classification,
            Classification::NotApplicable
        );
    }

    #[test]
    fn selected_matrix_is_complete_exact_expiring_and_fingerprinted() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
        let selected_models = SELECTED_MODELS.replace("\r\n", "\n");
        let matrix = SelectedMatrix::parse(&selected_models, today).unwrap();
        let evidence = matrix.evidence();
        assert_eq!(evidence.suite_id, "nib-llm-core-v1");
        assert_eq!(evidence.matrix_sha256.len(), 64);
        assert_eq!(evidence.required_task_count, 3);
        assert_eq!(evidence.conditional_task_count, 1);
        assert_ne!(
            evidence.matrix_sha256,
            SelectedMatrix::parse(&format!("{selected_models}\n"), today)
                .unwrap()
                .evidence()
                .matrix_sha256
        );

        assert!(SelectedMatrix::parse(
            &selected_models.replace("meta = [\"muse-spark-1.1\"]\n", ""),
            today
        )
        .unwrap_err()
        .contains("exactly every network provider"));
        assert!(SelectedMatrix::parse(
            &selected_models.replace(
                "openai = [\"gpt-5.6-sol\"]",
                "openai = [\"gpt-5.6-sol\", \"gpt-5.6-sol\"]"
            ),
            today
        )
        .unwrap_err()
        .contains("duplicate model IDs"));
        assert!(SelectedMatrix::parse(
            &selected_models.replace(
                "\"complete_text\",\n  \"streamed_text\"",
                "\"complete_text\",\n  \"complete_text\",\n  \"streamed_text\""
            ),
            today
        )
        .unwrap_err()
        .contains("duplicate scenarios"));
        assert!(SelectedMatrix::parse(
            &selected_models.replace("gpt-5.6-sol\"]", "gpt-*\"]"),
            today
        )
        .is_err());
        assert!(SelectedMatrix::parse(
            &selected_models.replace("expires_at = \"2027-02-17\"", "expires_at = \"2026-08-16\""),
            today
        )
        .unwrap_err()
        .contains("expired"));
    }

    #[test]
    fn plan_fails_before_io_when_request_budget_is_too_small() {
        let allowlist = OpenRouterAllowlist::load_default().unwrap();
        let mut settings = settings(LiveMode::Full);
        settings.limits.max_logical_requests = 1;
        let models = provider_descriptor("openai")
            .unwrap()
            .models()
            .iter()
            .map(|id| model(id, Some(true), Some(true)))
            .collect();
        let error =
            build_plan(&settings, &snapshot("openai", models), &allowlist, None).unwrap_err();
        assert!(error.contains("blocked_budget"));
    }

    #[test]
    fn full_plan_fails_when_an_advertised_model_is_absent() {
        let allowlist = OpenRouterAllowlist::load_default().unwrap();
        let error = build_plan(
            &settings(LiveMode::Full),
            &snapshot(
                "openai",
                vec![model(
                    provider_descriptor("openai").unwrap().default_model(),
                    Some(true),
                    Some(true),
                )],
            ),
            &allowlist,
            None,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "provider 'openai' advertised model is absent from its live catalog"
        );
    }

    #[test]
    fn openrouter_full_plan_reports_but_does_not_execute_unlisted_models() {
        let allowlist = OpenRouterAllowlist::parse(
            r#"
version = 1
[[model]]
id = "owner/approved-model"
approved = true
transports = ["chat_completions"]
required_scenarios = ["complete_text", "streamed_text"]
required_parameters = []
rationale = "Bounded deterministic qualification fixture"
owner = "nib-maintainers"
reviewed_at = "2026-08-06"
expires_at = "2027-02-06"
"#,
            NaiveDate::from_ymd_opt(2026, 8, 6).unwrap(),
        )
        .unwrap();
        let run = build_plan(
            &settings(LiveMode::Full),
            &snapshot(
                "openrouter",
                vec![
                    model("owner/approved-model", Some(true), Some(false)),
                    model("owner/unlisted-model", Some(true), Some(true)),
                ],
            ),
            &allowlist,
            None,
        )
        .unwrap();

        assert_eq!(run.profiles.len(), 1);
        assert_eq!(run.profiles[0].model.id, "owner/approved-model");
        assert_eq!(run.accounting.len(), 2);
        assert_eq!(
            run.accounting[1].classification,
            Classification::NotApplicable
        );
    }

    #[test]
    fn selected_openrouter_models_cannot_bypass_allowlist_approval() {
        let allowlist = OpenRouterAllowlist::load_default().unwrap();
        let matrix = SelectedMatrix::parse(
            SELECTED_MODELS,
            NaiveDate::from_ymd_opt(2026, 8, 17).unwrap(),
        )
        .unwrap();
        let models = matrix
            .models_for("openrouter")
            .unwrap()
            .iter()
            .map(|id| model(id, Some(true), Some(true)))
            .collect();
        let error = build_plan(
            &settings(LiveMode::Selected),
            &snapshot("openrouter", models),
            &allowlist,
            Some(&matrix),
        )
        .unwrap_err();

        assert!(error.contains("pending human approval"));
    }

    #[test]
    fn openrouter_plan_enforces_per_entry_projected_cost_ceiling() {
        let allowlist = OpenRouterAllowlist::parse(
            r#"
version = 1
[[model]]
id = "owner/expensive-model"
approved = true
transports = ["chat_completions"]
required_scenarios = ["complete_text", "streamed_text"]
required_parameters = []
rationale = "Bounded deterministic qualification fixture"
owner = "nib-maintainers"
reviewed_at = "2026-08-06"
expires_at = "2027-02-06"
max_projected_cost_usd = 1.0
"#,
            NaiveDate::from_ymd_opt(2026, 8, 6).unwrap(),
        )
        .unwrap();
        let mut expensive = model("owner/expensive-model", Some(true), Some(false));
        expensive.pricing.as_mut().unwrap().request_usd = Some(1.0);
        let error = build_plan(
            &settings(LiveMode::Full),
            &snapshot("openrouter", vec![expensive]),
            &allowlist,
            None,
        )
        .unwrap_err();

        assert!(error.contains("exceeds its projected cost ceiling"));
    }

    #[test]
    fn allowlist_rejects_wildcards_duplicates_and_expiry() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let base = r#"
version = 1
[[model]]
id = "owner/model"
approved = false
transports = ["chat_completions"]
required_scenarios = ["complete_text", "streamed_text"]
rationale = "bounded coverage"
owner = "maintainers"
reviewed_at = "2026-08-01"
expires_at = "2026-12-01"
"#;
        assert!(OpenRouterAllowlist::parse(base, today).is_ok());
        assert!(
            OpenRouterAllowlist::parse(&base.replace("owner/model", "owner/*"), today).is_err()
        );
        assert!(OpenRouterAllowlist::parse(
            &base.replace("2026-12-01", "2026-08-01"),
            NaiveDate::from_ymd_opt(2026, 8, 2).unwrap()
        )
        .is_err());
        assert!(OpenRouterAllowlist::parse(
            &format!("{base}\n{}", base.trim_start_matches("version = 1\n")),
            today
        )
        .is_err());
    }
}
