use super::config::LiveSettings;
use super::plan::RunPlan;
use super::report::{self, ProfileReport, ProviderReport, ScenarioReport};
use super::{
    CatalogSnapshot, Classification, LlmTerminalStatus, ModelProfile, ScenarioId, TransportId,
};
use nib::config::{LlmApiMode, LlmConfig, NibConfig, ProviderEntry};
use nib::llm::{create_client, LlmRequest, LlmRequestScope, StreamEvent};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::time::{Duration, Instant};

pub(super) async fn execute_provider_plan(
    settings: &LiveSettings,
    run_id: &str,
    snapshot: &CatalogSnapshot,
    plan: RunPlan,
) -> ProviderReport {
    let provider_started = Instant::now();
    let mut profiles = Vec::new();
    for profile in &plan.profiles {
        let profile_report = if provider_started.elapsed() >= settings.limits.max_provider_duration
        {
            report::profile_report(
                run_id,
                &snapshot.provider,
                &profile.model,
                profile.transport,
                profile.advertised,
                Vec::new(),
                Classification::Unknown,
            )
        } else {
            execute_profile(settings, run_id, &snapshot.provider, profile).await
        };
        profiles.push(profile_report);
    }
    report::generation_provider_report(run_id, snapshot, &plan, profiles, settings.mode)
}

async fn execute_profile(
    settings: &LiveSettings,
    run_id: &str,
    provider: &str,
    profile: &ModelProfile,
) -> ProfileReport {
    let client = match client_for_profile(settings, provider, profile) {
        Ok(client) => client,
        Err(error) => {
            let (classification, safe_error_class) = classify_error(&error);
            return report::profile_report(
                run_id,
                provider,
                &profile.model,
                profile.transport,
                profile.advertised,
                vec![ScenarioReport {
                    scenario: ScenarioId::CompleteText,
                    passed: false,
                    duration_ms: 0,
                    safe_error_class: Some(safe_error_class),
                }],
                classification,
            );
        }
    };
    let mut scenario_reports = Vec::new();
    let mut classification = Classification::Qualified;

    for scenario in &profile.required_scenarios {
        let started = Instant::now();
        let result = match scenario {
            ScenarioId::CompleteText => {
                bounded(
                    settings.limits.max_scenario_duration,
                    complete_text(client.as_ref(), settings, run_id),
                )
                .await
            }
            ScenarioId::StreamedText => {
                bounded(
                    settings.limits.max_scenario_duration,
                    streamed_text(client.as_ref(), settings, run_id),
                )
                .await
            }
            ScenarioId::SingleToolContinuation => {
                bounded(
                    settings.limits.max_scenario_duration,
                    tool_continuation(client.as_ref(), settings, run_id, false),
                )
                .await
            }
            ScenarioId::ParallelToolContinuation => {
                bounded(
                    settings.limits.max_scenario_duration,
                    tool_continuation(client.as_ref(), settings, run_id, true),
                )
                .await
            }
        };
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        match result {
            Ok(()) => scenario_reports.push(ScenarioReport {
                scenario: *scenario,
                passed: true,
                duration_ms,
                safe_error_class: None,
            }),
            Err(error) => {
                let (next_classification, safe_error_class) = classify_error(&error);
                classification = next_classification;
                scenario_reports.push(ScenarioReport {
                    scenario: *scenario,
                    passed: false,
                    duration_ms,
                    safe_error_class: Some(safe_error_class),
                });
                break;
            }
        }
    }

    report::profile_report(
        run_id,
        provider,
        &profile.model,
        profile.transport,
        profile.advertised,
        scenario_reports,
        classification,
    )
}

fn client_for_profile(
    settings: &LiveSettings,
    provider: &str,
    profile: &ModelProfile,
) -> Result<std::sync::Arc<dyn nib::llm::LlmClient>, String> {
    let api = match profile.transport {
        TransportId::ChatCompletions => Some(LlmApiMode::ChatCompletions),
        TransportId::Responses => Some(LlmApiMode::Responses),
        TransportId::AnthropicMessages | TransportId::GeminiGenerateContent => None,
    };
    let entry = ProviderEntry {
        model: profile.model.id.clone(),
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
    config
        .validate()
        .map_err(|_| "live provider configuration failed validation".to_string())?;
    let diagnostics = nib::llm::factory::provider_diagnostics(&llm, Some(provider))?;
    if diagnostics.provider != provider
        || diagnostics.model != profile.model.id
        || (api.is_some_and(|api| diagnostics.api_mode != api.as_str()))
    {
        return Err("production provider diagnostics do not match the live plan".to_string());
    }
    create_client(&llm, Some(provider))
}

async fn complete_text(
    client: &dyn nib::llm::LlmClient,
    settings: &LiveSettings,
    run_id: &str,
) -> Result<(), String> {
    let nonce = nonce("complete");
    let messages = [json!({
        "role": "user",
        "content": format!("Return the exact token {nonce} and no other text.")
    })];
    let scope = scope(run_id, "complete")?;
    let response = client
        .complete(live_request(&messages, None, scope, settings))
        .await?;
    validate_text_response(response, &nonce)
}

async fn streamed_text(
    client: &dyn nib::llm::LlmClient,
    settings: &LiveSettings,
    run_id: &str,
) -> Result<(), String> {
    let nonce = nonce("stream");
    let messages = [json!({
        "role": "user",
        "content": format!("Return the exact token {nonce} and no other text.")
    })];
    let scope = scope(run_id, "stream")?;
    let mut stream = client
        .stream(live_request(&messages, None, scope, settings))
        .await?;
    let mut public_text = String::new();
    let mut terminal_count = 0usize;
    let mut terminal_seen = false;
    while let Some(event) = stream.recv().await {
        let event = event?;
        if terminal_seen {
            return Err("stream emitted an event after its terminal event".to_string());
        }
        match event {
            StreamEvent::Content(fragment) => {
                public_text.push_str(&fragment);
                if public_text.len() > 64 * 1024 {
                    return Err("streamed qualification text exceeded its byte limit".to_string());
                }
            }
            StreamEvent::End(_) => {
                terminal_seen = true;
                terminal_count += 1;
            }
            StreamEvent::ToolCallChunk { .. } => {
                return Err("text stream unexpectedly produced a tool call".to_string())
            }
            _ => return Err("provider stream exposed a non-LLM lifecycle event".to_string()),
        }
    }
    if terminal_count != 1 || !public_text.contains(&nonce) {
        return Err("stream did not produce one nonce-bearing terminal result".to_string());
    }
    let response = stream.finish().await?;
    validate_text_response(response, &nonce)?;
    Ok(())
}

async fn tool_continuation(
    client: &dyn nib::llm::LlmClient,
    settings: &LiveSettings,
    run_id: &str,
    parallel: bool,
) -> Result<(), String> {
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
    let messages = [json!({"role": "user", "content": content})];
    let scope = scope(run_id, if parallel { "parallel" } else { "tool" })?;
    let response = client
        .complete(live_request(
            &messages,
            Some(&tools),
            scope.clone(),
            settings,
        ))
        .await?;
    if response.terminal_status != LlmTerminalStatus::Completed {
        return Err("tool qualification request was not completed".to_string());
    }
    let calls = response
        .tool_calls
        .as_ref()
        .filter(|calls| calls.len() == tool_names.len())
        .ok_or_else(|| "tool qualification returned the wrong number of tool calls".to_string())?;
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
            return Err("tool qualification returned a mismatched name or nonce".to_string());
        }
        if !invocation_ids.insert(call.invocation_id) {
            return Err("tool qualification reused a neutral invocation ID".to_string());
        }
    }
    let mut continuation = response.continuation.ok_or_else(|| {
        "tool qualification did not return private continuation state".to_string()
    })?;
    for call in calls {
        continuation.record_tool_output(
            call.invocation_id,
            &json!({"success": true, "receipt": receipt}),
        )?;
    }
    let follow_up = [json!({
        "role": "user",
        "content": format!("Return the exact receipt {receipt} and no other text.")
    })];
    let final_response = client
        .complete(
            live_request(&follow_up, None, scope, settings).with_continuation(Some(continuation)),
        )
        .await?;
    validate_text_response(final_response, &receipt)
}

fn live_request<'a>(
    messages: &'a [Value],
    tools: Option<&'a [Value]>,
    scope: LlmRequestScope,
    settings: &LiveSettings,
) -> LlmRequest<'a> {
    LlmRequest::new(messages, tools, 0.0)
        .with_scope(scope)
        .with_max_output_tokens(settings.limits.max_output_tokens_per_request)
}

fn qualification_tool(name: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": "Record one synthetic qualification nonce without side effects.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {"nonce": {"type": "string"}},
                "required": ["nonce"],
                "additionalProperties": false
            }
        }
    })
}

fn validate_text_response(response: nib::llm::LlmResponse, nonce: &str) -> Result<(), String> {
    if response.terminal_status != LlmTerminalStatus::Completed {
        return Err("qualification response was refused".to_string());
    }
    if response
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty())
        || response.continuation.is_some()
    {
        return Err("text qualification unexpectedly returned tool state".to_string());
    }
    let content = response
        .content
        .filter(|content| content.len() <= 64 * 1024 && content.contains(nonce))
        .ok_or_else(|| "qualification response did not contain its nonce".to_string())?;
    if content.trim().is_empty() || response.finish_reason.trim().is_empty() {
        return Err(
            "qualification response is missing text or a finish classification".to_string(),
        );
    }
    Ok(())
}

fn scope(run_id: &str, scenario: &str) -> Result<LlmRequestScope, String> {
    LlmRequestScope::new(
        format!("llm-live-{run_id}"),
        format!("{scenario}-{}", uuid::Uuid::new_v4()),
    )
}

fn nonce(prefix: &str) -> String {
    format!("NIB_{prefix}_{}", uuid::Uuid::new_v4().simple())
}

async fn bounded<T>(
    duration: Duration,
    future: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(duration, future)
        .await
        .map_err(|_| "live qualification scenario timed out".to_string())?
}

fn classify_error(error: &str) -> (Classification, String) {
    // Current T022 clients return bounded display-safe strings rather than a typed
    // error. Only locally reconstructed status markers are recognized. Ambiguous
    // provider rejections stay failed_adapter; they are never guessed unsupported.
    if error.contains("HTTP 401") || error.contains("HTTP 403") || error.contains("no credentials")
    {
        (Classification::BlockedAuth, "blocked_auth".to_string())
    } else if error.contains("HTTP 402") {
        (
            Classification::BlockedBilling,
            "blocked_billing".to_string(),
        )
    } else if error.contains("HTTP 429") {
        (
            Classification::BlockedRateLimit,
            "blocked_rate_limit".to_string(),
        )
    } else if error.contains("HTTP 451") {
        (Classification::BlockedRegion, "blocked_region".to_string())
    } else if error.contains("timed out") {
        (Classification::Unknown, "scenario_timeout".to_string())
    } else if error.contains("configuration")
        || error.contains("base_url")
        || error.contains("diagnostics")
    {
        (
            Classification::BlockedConfiguration,
            "blocked_configuration".to_string(),
        )
    } else {
        (Classification::FailedAdapter, "failed_adapter".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualification_tool_is_strict_and_inert() {
        let tool = qualification_tool("record_probe");
        assert_eq!(tool["function"]["strict"], true);
        assert_eq!(
            tool["function"]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(tool["function"]["parameters"]["required"][0], "nonce");
    }

    #[test]
    fn error_classification_never_uses_remote_free_form_detail() {
        assert_eq!(
            classify_error("OpenAI Responses API HTTP 429").0,
            Classification::BlockedRateLimit
        );
        assert_eq!(
            classify_error("model unsupported because remote said so").0,
            Classification::FailedAdapter
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
}
