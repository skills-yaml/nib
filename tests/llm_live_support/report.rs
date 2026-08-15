use super::config::LiveSettings;
use super::plan::RunPlan;
use super::{CatalogModel, CatalogSnapshot, Classification, LiveMode, ScenarioId, TransportId};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const REPORT_SCHEMA_VERSION: u32 = 1;
const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SAFE_MODEL_REF_BYTES: usize = 512;

#[derive(Debug, Clone, Serialize)]
pub(super) struct CatalogEntryReport {
    model_ref: String,
    classification: Classification,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ScenarioReport {
    pub scenario: ScenarioId,
    pub passed: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_error_class: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ProfileReport {
    pub model_ref: String,
    pub transport: TransportId,
    pub advertised: bool,
    pub classification: Classification,
    pub scenarios: Vec<ScenarioReport>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ProviderReport {
    pub provider: String,
    pub catalog_captured_at: DateTime<Utc>,
    pub catalog_pages: usize,
    pub catalog_models: usize,
    pub catalog_hash: String,
    pub catalog_drift: bool,
    pub logical_requests: usize,
    pub maximum_attempts: usize,
    pub maximum_output_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_cost_usd: Option<f64>,
    pub accounting: Vec<CatalogEntryReport>,
    pub profiles: Vec<ProfileReport>,
    pub complete: bool,
    pub passed: bool,
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
    providers: Vec<ProviderReport>,
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
    ) -> Self {
        let complete = !providers.is_empty() && providers.iter().all(|provider| provider.complete);
        let passed = complete && providers.iter().all(|provider| provider.passed);
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            run_id,
            source_revision: env!("NIB_BUILD_COMMIT").to_string(),
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            mode,
            started_at,
            completed_at,
            providers,
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

pub(super) fn catalog_provider_report(
    run_id: &str,
    snapshot: &CatalogSnapshot,
    plan: &RunPlan,
) -> ProviderReport {
    let accounting = plan
        .accounting
        .iter()
        .map(|entry| CatalogEntryReport {
            model_ref: model_reference(run_id, &snapshot.provider, &entry.model),
            classification: entry.classification,
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
        catalog_hash: catalog_hash(snapshot),
        catalog_drift: false,
        logical_requests: plan.logical_requests,
        maximum_attempts: plan.maximum_attempts,
        maximum_output_tokens: plan.maximum_output_tokens,
        projected_cost_usd: plan.projected_cost_usd,
        accounting,
        profiles: Vec::new(),
        complete,
        passed,
    }
}

pub(super) fn generation_provider_report(
    run_id: &str,
    snapshot: &CatalogSnapshot,
    plan: &RunPlan,
    profiles: Vec<ProfileReport>,
    mode: LiveMode,
) -> ProviderReport {
    let accounting = plan
        .accounting
        .iter()
        .map(|entry| {
            let profile_classifications = profiles
                .iter()
                .filter(|profile| {
                    profile.model_ref == model_reference(run_id, &snapshot.provider, &entry.model)
                })
                .map(|profile| profile.classification)
                .collect::<Vec<_>>();
            let classification = if entry.classification == Classification::NotApplicable {
                Classification::NotApplicable
            } else if profile_classifications.is_empty() {
                Classification::Unknown
            } else if profile_classifications
                .iter()
                .all(|classification| classification.is_qualified())
            {
                Classification::Qualified
            } else {
                profile_classifications
                    .into_iter()
                    .find(|classification| !classification.is_qualified())
                    .unwrap_or(Classification::Unknown)
            };
            CatalogEntryReport {
                model_ref: model_reference(run_id, &snapshot.provider, &entry.model),
                classification,
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
    let complete = if mode == LiveMode::Canary {
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
                    | Classification::UnsupportedTransport
            )
        });
    let passed = complete
        && profiles
            .iter()
            .all(|profile| profile.classification.is_qualified())
        && accounting_passed;
    ProviderReport {
        provider: plan.provider.clone(),
        catalog_captured_at: snapshot.captured_at,
        catalog_pages: snapshot.page_count,
        catalog_models: snapshot.models.len(),
        catalog_hash: catalog_hash(snapshot),
        catalog_drift: false,
        logical_requests: plan.logical_requests,
        maximum_attempts: plan.maximum_attempts,
        maximum_output_tokens: plan.maximum_output_tokens,
        projected_cost_usd: plan.projected_cost_usd,
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

pub(super) fn profile_report(
    run_id: &str,
    provider: &str,
    model: &CatalogModel,
    transport: TransportId,
    advertised: bool,
    scenarios: Vec<ScenarioReport>,
    classification: Classification,
) -> ProfileReport {
    ProfileReport {
        model_ref: model_reference(run_id, provider, model),
        transport,
        advertised,
        classification,
        scenarios,
    }
}

fn catalog_hash(snapshot: &CatalogSnapshot) -> String {
    let mut hasher = Sha256::new();
    hasher.update(snapshot.provider.as_bytes());
    hasher.update([0]);
    hasher.update(
        serde_json::to_vec(&snapshot.models)
            .expect("validated catalog models must serialize deterministically"),
    );
    format!("{:x}", hasher.finalize())
}

fn model_reference(run_id: &str, provider: &str, model: &CatalogModel) -> String {
    if model.public_identifier {
        return sanitize_identifier(&model.id);
    }
    let mut hasher = Sha256::new();
    hasher.update(run_id.as_bytes());
    hasher.update([0]);
    hasher.update(provider.as_bytes());
    hasher.update([0]);
    hasher.update(model.id.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("private:{}", &digest[..24])
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

    prepare_results_directory(&settings.results_dir)?;
    let json_path = settings.results_dir.join(format!("{}.json", report.run_id));
    let markdown_path = settings.results_dir.join(format!("{}.md", report.run_id));
    atomic_create(&json_path, &json)?;
    if let Err(error) = atomic_create(&markdown_path, &markdown) {
        let _ = fs::remove_file(&json_path);
        return Err(error);
    }
    Ok(PublishedReport {
        json: json_path,
        markdown: markdown_path,
        passed: report.passed,
    })
}

fn markdown_summary(report: &QualificationReport) -> String {
    let mut output = format!(
        "# Live LLM qualification {}\n\n- Revision: `{}`\n- Mode: `{:?}`\n- Complete: `{}`\n- Passed: `{}`\n\n",
        report.run_id, report.source_revision, report.mode, report.complete, report.passed
    );
    for provider in &report.providers {
        output.push_str(&format!(
            "## {}\n\n- Catalog: {} models across {} page(s)\n- Planned logical requests: {}\n- Maximum attempts: {}\n- Complete: `{}`\n- Passed: `{}`\n\n",
            provider.provider,
            provider.catalog_models,
            provider.catalog_pages,
            provider.logical_requests,
            provider.maximum_attempts,
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
        }
        output.push('\n');
    }
    output
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
    variants.sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    variants.dedup();
    variants
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

fn prepare_results_directory(path: &Path) -> Result<(), String> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| "failed to inspect live results directory".to_string())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("live results path must be a real directory, not a link".to_string());
        }
    } else {
        fs::create_dir_all(path)
            .map_err(|_| "failed to create live results directory".to_string())?;
    }
    Ok(())
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if path.exists() {
        return Err("qualification report target already exists".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "qualification report has no parent directory".to_string())?;
    let temporary = parent.join(format!(".llm-live-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| "failed to create qualification report staging file".to_string())?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "failed to durably write qualification report".to_string())?;
        fs::hard_link(&temporary, path)
            .map_err(|_| "failed to publish qualification report atomically".to_string())?;
        let _ = fs::remove_file(&temporary);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_live_support::plan::{AccountingEntry, RunPlan};
    use chrono::Utc;

    fn private_model() -> CatalogModel {
        CatalogModel {
            id: "ft:customer:private-name".to_string(),
            aliases: Vec::new(),
            supports_text_generation: Some(true),
            supports_tools: None,
            supports_parallel_tools: None,
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
        let first = model_reference("run", "openai", &model);
        let second = model_reference("run", "openai", &model);
        assert_eq!(first, second);
        assert!(first.starts_with("private:"));
        assert!(!first.contains("customer"));
        assert_ne!(first, model_reference("other-run", "openai", &model));
    }

    #[test]
    fn artifact_scan_detects_raw_encoded_and_json_escaped_secrets() {
        let secret = "key/line\nvalue".to_string();
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
        assert!(scan_artifact(escaped.as_bytes(), &[secret]).is_err());
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
            }],
            profiles: Vec::new(),
            logical_requests: 0,
            maximum_attempts: 0,
            maximum_output_tokens: 0,
            projected_cost_usd: Some(0.0),
        };
        let report = catalog_provider_report("run", &snapshot, &plan);
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
            }],
            profiles: Vec::new(),
            logical_requests: 0,
            maximum_attempts: 0,
            maximum_output_tokens: 0,
            projected_cost_usd: Some(0.0),
        };
        let mut report = catalog_provider_report("run", &snapshot, &plan);

        mark_catalog_drift(&mut report);

        assert!(report.catalog_drift);
        assert!(!report.complete);
        assert!(!report.passed);
    }

    #[test]
    fn atomic_create_refuses_to_overwrite_existing_report() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        atomic_create(&path, b"first").unwrap();
        assert!(atomic_create(&path, b"second").is_err());
        assert_eq!(fs::read(path).unwrap(), b"first");
    }
}
