//! Complete-request context snapshot for admission and live `/context`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::budget::{approximate_llm_input_tokens, BoundedLlmInput};
use crate::llm::{LlmRequest, LlmUsage};
use crate::session::Session;

/// Conservative response reserve as a fraction of the configured window.
pub const RESPONSE_RESERVE_NUMERATOR: usize = 10;
pub const UNCERTAINTY_MARGIN_NUMERATOR: usize = 5;
const DENOMINATOR: usize = 100;
pub const ESTIMATOR_VERSION: &str = "chars-div-4/v1";
const MAX_INSPECT_CONTRIBUTIONS: usize = 16;
const MAX_USAGE_EVENTS_SCAN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestBudget {
    pub window: usize,
    pub window_source: String,
    pub response_reserve: usize,
    pub uncertainty_margin: usize,
    pub input_allowance: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contribution {
    pub category: String,
    pub tokens: usize,
    pub included: usize,
    pub available: usize,
    pub unknown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub unknown: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSnapshot {
    pub phase: String,
    pub state: String,
    pub estimator_version: String,
    pub budget: RequestBudget,
    pub estimated_input: usize,
    pub remaining_headroom: usize,
    pub raw_message_count: usize,
    pub included_tool_count: usize,
    pub raw_tool_count: usize,
    #[serde(default)]
    pub contributions: Vec<Contribution>,
    #[serde(default)]
    pub continuation_tokens: Option<usize>,
    #[serde(default)]
    pub usage: Option<UsageTotals>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionError {
    pub message: String,
    pub budget: RequestBudget,
    pub estimated_input: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationDispatch {
    Send,
    Rebuild,
    Block { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OccupancyStyle {
    Compact,
    Narrow,
}

pub fn request_budget(window: usize, window_source: &str) -> RequestBudget {
    let response_reserve = (window * RESPONSE_RESERVE_NUMERATOR) / DENOMINATOR;
    let uncertainty_margin = (window * UNCERTAINTY_MARGIN_NUMERATOR) / DENOMINATOR;
    let reserved = response_reserve.saturating_add(uncertainty_margin);
    let input_allowance = window.saturating_sub(reserved);
    RequestBudget {
        window,
        window_source: window_source.to_string(),
        response_reserve,
        uncertainty_margin,
        input_allowance,
    }
}

pub fn input_allowance_for(window: usize) -> usize {
    request_budget(window, "configured").input_allowance
}

pub fn admit_estimated_input(
    budget: &RequestBudget,
    estimated_input: usize,
) -> Result<usize, AdmissionError> {
    let occupied = estimated_input
        .saturating_add(budget.response_reserve)
        .saturating_add(budget.uncertainty_margin);
    if occupied > budget.window {
        return Err(AdmissionError {
            message: format!(
                "estimated input {estimated_input} plus response reserve {} and uncertainty {} exceeds window {}",
                budget.response_reserve, budget.uncertainty_margin, budget.window
            ),
            budget: budget.clone(),
            estimated_input,
        });
    }
    Ok(budget.window.saturating_sub(occupied))
}

pub fn tokens_from_encoded_bytes(bytes: usize) -> Option<usize> {
    if bytes == 0 {
        None
    } else {
        Some(bytes.div_ceil(4))
    }
}

pub fn continuation_dispatch(
    window: usize,
    estimated_input: usize,
    continuation_bytes: usize,
    unresolved_tools: usize,
) -> ContinuationDispatch {
    let budget = request_budget(window, "configured");
    let continuation_tokens = tokens_from_encoded_bytes(continuation_bytes);
    let estimated = match continuation_tokens {
        Some(tokens) => estimated_input.saturating_add(tokens),
        None if unresolved_tools == 0 => estimated_input,
        None => {
            return ContinuationDispatch::Block {
                message:
                    "native continuation footprint is unknown; admission cannot be established"
                        .to_string(),
            };
        }
    };
    match admit_estimated_input(&budget, estimated) {
        Ok(_) => ContinuationDispatch::Send,
        Err(error) if unresolved_tools > 0 => ContinuationDispatch::Block {
            message: error.message,
        },
        Err(_) => ContinuationDispatch::Rebuild,
    }
}

pub fn apply_response_reserve<'a>(request: LlmRequest<'a>, window: usize) -> LlmRequest<'a> {
    let reserve = request_budget(window, "configured").response_reserve.max(1);
    let tokens = u32::try_from(reserve).unwrap_or(u32::MAX);
    request.with_max_output_tokens(tokens)
}

pub fn snapshot_from_bounded_input(
    phase: &str,
    state: &str,
    window_source: &str,
    context_length: usize,
    bounded: &BoundedLlmInput,
) -> RequestSnapshot {
    snapshot_with_continuation(
        phase,
        state,
        window_source,
        context_length,
        bounded,
        None,
        None,
    )
}

pub fn snapshot_with_continuation(
    phase: &str,
    state: &str,
    window_source: &str,
    context_length: usize,
    bounded: &BoundedLlmInput,
    continuation_bytes: Option<usize>,
    usage: Option<&LlmUsage>,
) -> RequestSnapshot {
    let budget = request_budget(context_length, window_source);
    let continuation_tokens = continuation_bytes.and_then(tokens_from_encoded_bytes);
    let estimated_input = bounded
        .approximate_tokens
        .saturating_add(continuation_tokens.unwrap_or(0));
    let remaining_headroom = admit_estimated_input(&budget, estimated_input).unwrap_or(0);
    RequestSnapshot {
        phase: phase.to_string(),
        state: state.to_string(),
        estimator_version: ESTIMATOR_VERSION.to_string(),
        budget,
        estimated_input,
        remaining_headroom,
        raw_message_count: bounded.raw_message_count,
        included_tool_count: bounded.included_tool_count,
        raw_tool_count: bounded.raw_tool_count,
        contributions: contributions_from_bounded(bounded, continuation_tokens),
        continuation_tokens,
        usage: usage.map(usage_totals_from_validated),
    }
}

pub fn latest_from_session(session: &Session) -> Option<RequestSnapshot> {
    session
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "context_bounded")
        .and_then(|event| RequestSnapshot::from_event_details(&event.details))
}

pub fn occupancy_indicator(used: usize, window: usize, style: OccupancyStyle) -> String {
    if window == 0 {
        return "ctx ?".to_string();
    }
    match style {
        OccupancyStyle::Compact => format!(
            "ctx ~{}/{}",
            format_token_count(used),
            format_token_count(window)
        ),
        OccupancyStyle::Narrow => format!("ctx ~{}", percent_label(used, window)),
    }
}

pub fn abbreviate_occupancy_indicator(label: &str) -> String {
    let trimmed = label.trim();
    if trimmed == "ctx ?" || trimmed.ends_with('%') {
        return trimmed.to_string();
    }
    if let Some((used, window)) = parse_compact_occupancy(trimmed) {
        occupancy_indicator(used, window, OccupancyStyle::Narrow)
    } else {
        trimmed.to_string()
    }
}

pub fn occupancy_percent_from_label(label: &str) -> u16 {
    let trimmed = label.trim();
    if trimmed == "ctx ?" || trimmed == "?" {
        return 0;
    }
    if let Some(rest) = trimmed.strip_prefix("ctx ~") {
        if let Some(percent) = rest.strip_suffix('%') {
            return parse_percent_token(percent);
        }
        if let Some((used, window)) = parse_compact_occupancy(trimmed) {
            return occupancy_percent(used, window);
        }
    }
    let percent = trimmed
        .trim_end_matches('%')
        .trim_start_matches('<')
        .parse::<u16>()
        .unwrap_or(0);
    percent.min(999)
}

pub fn usage_event_details(run_id: &str, request_sequence: u64, usage: Option<&LlmUsage>) -> Value {
    match usage {
        Some(usage) => json!({
            "run_id": run_id,
            "request_sequence": request_sequence,
            "unknown": false,
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "reasoning_output_tokens": usage.reasoning_output_tokens,
        }),
        None => json!({
            "run_id": run_id,
            "request_sequence": request_sequence,
            "unknown": true,
        }),
    }
}

pub fn accumulate_session_usage(session: &Session) -> UsageTotals {
    let mut totals = UsageTotals {
        input_tokens: Some(0),
        output_tokens: Some(0),
        cached_input_tokens: Some(0),
        reasoning_output_tokens: Some(0),
        unknown: false,
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut observed = false;
    for event in session
        .events
        .iter()
        .rev()
        .filter(|event| event.kind == "context_usage")
        .take(MAX_USAGE_EVENTS_SCAN)
    {
        let run_id = event
            .details
            .get("run_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let sequence = event
            .details
            .get("request_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !seen.insert((run_id.to_string(), sequence)) {
            continue;
        }
        if event
            .details
            .get("unknown")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            totals.unknown = true;
            continue;
        }
        let Some(usage) = usage_from_event(&event.details) else {
            totals.unknown = true;
            continue;
        };
        observed = true;
        totals.input_tokens = add_optional(totals.input_tokens, Some(usage.input_tokens));
        totals.output_tokens = add_optional(totals.output_tokens, Some(usage.output_tokens));
        totals.cached_input_tokens =
            add_optional(totals.cached_input_tokens, usage.cached_input_tokens);
        totals.reasoning_output_tokens = add_optional(
            totals.reasoning_output_tokens,
            usage.reasoning_output_tokens,
        );
    }
    if !observed {
        totals.input_tokens = None;
        totals.output_tokens = None;
        totals.cached_input_tokens = None;
        totals.reasoning_output_tokens = None;
        totals.unknown = true;
    }
    totals
}

pub fn inspect_text(
    session: &Session,
    snapshot: Option<&RequestSnapshot>,
    details: bool,
) -> String {
    let (used, window) = occupancy_from_session(session, snapshot);
    let compact = occupancy_indicator(used, window, OccupancyStyle::Compact);
    if !details {
        return format!("{compact}. Use /context details for the bounded breakdown.");
    }
    let snapshot_lines = snapshot
        .map(format_snapshot_lines)
        .unwrap_or_else(|| "Request snapshot: unavailable historical accounting".to_string());
    let usage = accumulate_session_usage(session);
    let usage_line = format_usage_line(
        &usage,
        snapshot.and_then(|snapshot| snapshot.usage.as_ref()),
    );
    let compression_line = latest_compression_line(session);
    format!(
        "{compact}\n{snapshot_lines}\n{usage_line}\n{compression_line}\nHistory: {} message(s) · {} summarized · {} unsummarized\nSummary: {}\nHuman intent records: {} · unresolved clarifications: {}\nSelected skills: {}",
        session.messages.len(),
        session.summary_index.min(session.messages.len()),
        session
            .messages
            .len()
            .saturating_sub(session.summary_index.min(session.messages.len())),
        if session.summary.is_some() {
            "present"
        } else {
            "none"
        },
        session.human_intent.len(),
        session
            .clarifications
            .iter()
            .filter(|clarification| {
                !matches!(
                    clarification.status,
                    crate::session::ClarificationStatus::Answered
                )
            })
            .count(),
        session.active_skills.len(),
    )
}

pub fn inspect_json(session: &Session, snapshot: Option<&RequestSnapshot>) -> Value {
    let (used, window) = occupancy_from_session(session, snapshot);
    json!({
        "kind": "live_request",
        "session_id": session.id,
        "occupancy": {
            "estimated_input": used,
            "window": window,
            "indicator": occupancy_indicator(used, window, OccupancyStyle::Compact),
        },
        "snapshot": snapshot.map(RequestSnapshot::to_inspect_value),
        "usage": accumulate_session_usage(session),
        "historical_accounting": snapshot.is_some(),
    })
}

pub fn project_preview_json(text: &str) -> Value {
    json!({
        "kind": "project_preview",
        "text": text,
    })
}

impl RequestSnapshot {
    pub fn to_event_details(&self, run_id: &str) -> Value {
        json!({
            "run_id": run_id,
            "phase": self.phase,
            "state": self.state,
            "estimator_version": self.estimator_version,
            "window": self.budget.window,
            "window_source": self.budget.window_source,
            "response_reserve": self.budget.response_reserve,
            "uncertainty_margin": self.budget.uncertainty_margin,
            "input_allowance": self.budget.input_allowance,
            "estimated_input": self.estimated_input,
            "remaining_headroom": self.remaining_headroom,
            "raw_message_count": self.raw_message_count,
            "included_tool_count": self.included_tool_count,
            "raw_tool_count": self.raw_tool_count,
            "approximate_input_tokens": self.estimated_input,
            "context_length": self.budget.window,
            "contributions": self.contributions,
            "continuation_tokens": self.continuation_tokens,
            "usage": self.usage,
        })
    }

    pub fn to_inspect_value(&self) -> Value {
        json!({
            "phase": self.phase,
            "state": self.state,
            "estimator_version": self.estimator_version,
            "budget": self.budget,
            "estimated_input": self.estimated_input,
            "remaining_headroom": self.remaining_headroom,
            "contributions": self.contributions,
            "continuation_tokens": self.continuation_tokens,
            "usage": self.usage,
        })
    }

    pub fn from_event_details(details: &Value) -> Option<Self> {
        let window = details.get("window")?.as_u64()? as usize;
        let estimated_input = details
            .get("estimated_input")
            .or_else(|| details.get("approximate_input_tokens"))?
            .as_u64()? as usize;
        let window_source = details
            .get("window_source")
            .and_then(Value::as_str)
            .unwrap_or("configured")
            .to_string();
        let budget = if details.get("input_allowance").is_some() {
            RequestBudget {
                window,
                window_source,
                response_reserve: details.get("response_reserve")?.as_u64()? as usize,
                uncertainty_margin: details.get("uncertainty_margin")?.as_u64()? as usize,
                input_allowance: details.get("input_allowance")?.as_u64()? as usize,
            }
        } else {
            request_budget(window, &window_source)
        };
        Some(Self {
            phase: details
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            state: details
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("sent")
                .to_string(),
            estimator_version: details
                .get("estimator_version")
                .and_then(Value::as_str)
                .unwrap_or(ESTIMATOR_VERSION)
                .to_string(),
            remaining_headroom: details
                .get("remaining_headroom")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or_else(|| admit_estimated_input(&budget, estimated_input).unwrap_or(0)),
            budget,
            estimated_input,
            raw_message_count: details
                .get("raw_message_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            included_tool_count: details
                .get("included_tool_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            raw_tool_count: details
                .get("raw_tool_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize,
            contributions: details
                .get("contributions")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default(),
            continuation_tokens: details
                .get("continuation_tokens")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            usage: details
                .get("usage")
                .and_then(|value| serde_json::from_value(value.clone()).ok()),
        })
    }
}

fn contributions_from_bounded(
    bounded: &BoundedLlmInput,
    continuation_tokens: Option<usize>,
) -> Vec<Contribution> {
    let message_tokens = approximate_llm_input_tokens(&bounded.messages, None);
    let tool_tokens = bounded
        .tools
        .as_deref()
        .map(|tools| approximate_llm_input_tokens(&[], Some(tools)))
        .unwrap_or(0);
    let wrapped = bounded.approximate_tokens;
    let parts = message_tokens.saturating_add(tool_tokens);
    let overhead = wrapped.saturating_sub(parts);
    let mut contributions = vec![
        Contribution {
            category: "messages".to_string(),
            tokens: message_tokens,
            included: bounded.messages.len(),
            available: bounded.raw_message_count,
            unknown: false,
        },
        Contribution {
            category: "tools".to_string(),
            tokens: tool_tokens,
            included: bounded.included_tool_count,
            available: bounded.raw_tool_count,
            unknown: false,
        },
        Contribution {
            category: "serialization_overhead".to_string(),
            tokens: overhead,
            included: 1,
            available: 1,
            unknown: false,
        },
    ];
    if let Some(tokens) = continuation_tokens {
        contributions.push(Contribution {
            category: "native_continuation".to_string(),
            tokens,
            included: 1,
            available: 1,
            unknown: false,
        });
    }
    contributions.truncate(MAX_INSPECT_CONTRIBUTIONS);
    contributions
}

fn usage_totals_from_validated(usage: &LlmUsage) -> UsageTotals {
    UsageTotals {
        input_tokens: Some(usage.input_tokens),
        output_tokens: Some(usage.output_tokens),
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        unknown: false,
    }
}

fn usage_from_event(details: &Value) -> Option<LlmUsage> {
    LlmUsage::new(
        details.get("input_tokens")?.as_u64()?,
        details.get("output_tokens")?.as_u64()?,
        details.get("total_tokens")?.as_u64()?,
        details.get("cached_input_tokens").and_then(Value::as_u64),
        details
            .get("reasoning_output_tokens")
            .and_then(Value::as_u64),
    )
    .ok()
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => left.checked_add(right),
        _ => None,
    }
}

fn occupancy_from_session(
    _session: &Session,
    snapshot: Option<&RequestSnapshot>,
) -> (usize, usize) {
    match snapshot {
        Some(snapshot) => (snapshot.estimated_input, snapshot.budget.window),
        None => (0, 0),
    }
}

fn format_snapshot_lines(snapshot: &RequestSnapshot) -> String {
    let mut lines = format!(
        "Request snapshot: phase {} · state {} · estimator {}\nInput ~{} / allowance {} · reserve {} · uncertainty {} · headroom {}",
        snapshot.phase,
        snapshot.state,
        snapshot.estimator_version,
        snapshot.estimated_input,
        snapshot.budget.input_allowance,
        snapshot.budget.response_reserve,
        snapshot.budget.uncertainty_margin,
        snapshot.remaining_headroom,
    );
    if snapshot.contributions.is_empty() {
        lines.push_str("\nContributions: unavailable (not encoded as zero)");
    } else {
        lines.push_str("\nContributions:");
        for contribution in &snapshot.contributions {
            if contribution.unknown {
                lines.push_str(&format!("\n- {} unknown (not zero)", contribution.category));
            } else {
                lines.push_str(&format!(
                    "\n- {} ~{} ({} / {})",
                    contribution.category,
                    contribution.tokens,
                    contribution.included,
                    contribution.available
                ));
            }
        }
    }
    if let Some(tokens) = snapshot.continuation_tokens {
        lines.push_str(&format!("\nNative continuation estimate: ~{tokens}"));
    }
    lines
}

fn format_usage_line(cumulative: &UsageTotals, request: Option<&UsageTotals>) -> String {
    let request_line = match request {
        Some(usage) if !usage.unknown => format!(
            "Request occupancy usage: in {} · out {} (cache {:?} · reasoning {:?})",
            usage.input_tokens.unwrap_or(0),
            usage.output_tokens.unwrap_or(0),
            usage.cached_input_tokens,
            usage.reasoning_output_tokens
        ),
        _ => "Request occupancy usage: unknown".to_string(),
    };
    let cumulative_line = if cumulative.unknown {
        "Cumulative consumption: unknown/partial (not zero)".to_string()
    } else {
        format!(
            "Cumulative consumption: in {} · out {} (cache {:?} · reasoning {:?})",
            cumulative.input_tokens.unwrap_or(0),
            cumulative.output_tokens.unwrap_or(0),
            cumulative.cached_input_tokens,
            cumulative.reasoning_output_tokens
        )
    };
    format!("{request_line}\n{cumulative_line}")
}

fn latest_compression_line(session: &Session) -> String {
    session
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "compression")
        .map(|event| {
            format!(
                "Latest compression: before {} · after {} · covered {}-{} · chunks {}",
                event
                    .details
                    .get("before_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                event
                    .details
                    .get("after_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                event
                    .details
                    .get("summarized_from")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                event
                    .details
                    .get("summarized_through")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                event
                    .details
                    .get("chunks")
                    .and_then(Value::as_u64)
                    .unwrap_or(1),
            )
        })
        .unwrap_or_else(|| "Latest compression: none".to_string())
}

fn occupancy_percent(used: usize, window: usize) -> u16 {
    if window == 0 {
        0
    } else {
        used.saturating_mul(100)
            .checked_div(window)
            .unwrap_or(0)
            .min(999) as u16
    }
}

fn percent_label(used: usize, window: usize) -> String {
    let percent = occupancy_percent(used, window);
    if used == 0 {
        "0%".to_string()
    } else if percent == 0 {
        "<1%".to_string()
    } else {
        format!("{percent}%")
    }
}

fn format_token_count(tokens: usize) -> String {
    if tokens >= 1000 {
        format!("{}k", tokens / 1000)
    } else {
        tokens.to_string()
    }
}

fn parse_token_count(token: &str) -> Option<usize> {
    let token = token.trim();
    if let Some(k) = token.strip_suffix('k') {
        k.parse::<usize>().ok()?.checked_mul(1000)
    } else {
        token.parse().ok()
    }
}

fn parse_percent_token(token: &str) -> u16 {
    token
        .trim()
        .trim_start_matches('<')
        .parse::<u16>()
        .unwrap_or(0)
        .min(999)
}

fn parse_compact_occupancy(label: &str) -> Option<(usize, usize)> {
    let rest = label.strip_prefix("ctx ~")?;
    let (used, window) = rest.split_once('/')?;
    Some((parse_token_count(used)?, parse_token_count(window)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmMessage;
    use serde_json::json;

    fn bounded_fixture(tokens: usize) -> BoundedLlmInput {
        BoundedLlmInput {
            messages: vec![
                json!({"role": "system", "content": "instructions"}),
                json!({"role": "user", "content": "goal"}),
            ],
            tools: Some(vec![
                json!({"type":"function","function":{"name":"run_terminal"}}),
            ]),
            approximate_tokens: tokens,
            raw_message_count: 4,
            raw_tool_count: 3,
            included_tool_count: 1,
        }
    }

    #[test]
    fn admission_leaves_response_reserve_and_uncertainty() {
        let budget = request_budget(10_000, "configured");
        assert_eq!(budget.response_reserve, 1_000);
        assert_eq!(budget.uncertainty_margin, 500);
        assert_eq!(budget.input_allowance, 8_500);
        let headroom = admit_estimated_input(&budget, 8_000).expect("fits");
        assert_eq!(headroom, 500);
        let error = admit_estimated_input(&budget, 8_501).expect_err("over");
        assert!(error.message.contains("exceeds window"));
        assert_ne!(error.estimated_input, 0);
    }

    #[test]
    fn unknown_contributions_are_not_encoded_as_zero_totals() {
        let snapshot = snapshot_from_bounded_input(
            "planning",
            "prepared",
            "configured",
            4_096,
            &bounded_fixture(100),
        );
        let details = snapshot.to_event_details("run-1");
        assert_eq!(details["estimated_input"], 100);
        assert_ne!(details["window"], 0);
        assert_eq!(details["estimator_version"], ESTIMATOR_VERSION);
        assert!(details["contributions"].as_array().unwrap().len() >= 2);
        let restored = RequestSnapshot::from_event_details(&details).expect("roundtrip");
        assert_eq!(restored.estimated_input, 100);
        assert_eq!(
            restored.budget.input_allowance,
            snapshot.budget.input_allowance
        );
        assert!(!restored.contributions.is_empty());
    }

    #[test]
    fn unicode_json_and_code_estimates_are_labelled() {
        let payload = BoundedLlmInput {
            messages: vec![json!({"role":"user","content":"fn main() { println!(\"🙂\"); }"})],
            tools: None,
            approximate_tokens: 12,
            raw_message_count: 1,
            raw_tool_count: 0,
            included_tool_count: 0,
        };
        let snapshot =
            snapshot_from_bounded_input("execution", "prepared", "configured", 1_024, &payload);
        assert_eq!(snapshot.estimator_version, ESTIMATOR_VERSION);
        assert!(snapshot
            .contributions
            .iter()
            .any(|c| c.category == "messages"));
        let inspect = snapshot.to_inspect_value();
        assert_eq!(inspect["estimator_version"], ESTIMATOR_VERSION);
    }

    #[test]
    fn continuation_unknown_bytes_are_not_zero_and_block_unresolved() {
        assert_eq!(tokens_from_encoded_bytes(0), None);
        assert_eq!(
            continuation_dispatch(10_000, 1_000, 0, 1),
            ContinuationDispatch::Block {
                message:
                    "native continuation footprint is unknown; admission cannot be established"
                        .to_string()
            }
        );
        assert_eq!(
            continuation_dispatch(10_000, 1_000, 400, 1),
            ContinuationDispatch::Send
        );
        assert!(matches!(
            continuation_dispatch(1_000, 900, 4_000, 0),
            ContinuationDispatch::Rebuild
        ));
        assert!(matches!(
            continuation_dispatch(1_000, 900, 4_000, 2),
            ContinuationDispatch::Block { .. }
        ));
    }

    #[test]
    fn two_execution_rounds_keep_distinct_tool_evidence_once() {
        let first = snapshot_with_continuation(
            "execution",
            "sent",
            "configured",
            8_000,
            &bounded_fixture(400),
            Some(120),
            None,
        );
        let second = snapshot_with_continuation(
            "continuation",
            "sent",
            "configured",
            8_000,
            &bounded_fixture(400),
            Some(360),
            None,
        );
        assert_eq!(first.phase, "execution");
        assert_eq!(second.phase, "continuation");
        assert_eq!(first.continuation_tokens, Some(30));
        assert_eq!(second.continuation_tokens, Some(90));
        assert_ne!(first.estimated_input, second.estimated_input);
        let first_tools = first
            .contributions
            .iter()
            .find(|c| c.category == "native_continuation")
            .expect("first continuation");
        let second_tools = second
            .contributions
            .iter()
            .find(|c| c.category == "native_continuation")
            .expect("second continuation");
        assert_eq!(first_tools.included, 1);
        assert_eq!(second_tools.included, 1);
        assert!(second.estimated_input > first.estimated_input);
    }

    #[test]
    fn occupancy_indicator_uses_snapshot_denominator() {
        let compact = occupancy_indicator(18_000, 64_000, OccupancyStyle::Compact);
        assert_eq!(compact, "ctx ~18k/64k");
        assert_eq!(abbreviate_occupancy_indicator(&compact), "ctx ~28%");
        assert_eq!(occupancy_percent_from_label(&compact), 28);
        assert_eq!(occupancy_percent_from_label("ctx ~12%"), 12);
        assert_eq!(occupancy_percent_from_label("ctx ?"), 0);
        assert_eq!(occupancy_indicator(0, 0, OccupancyStyle::Compact), "ctx ?");
    }

    #[test]
    fn usage_unknown_is_not_zero_and_stream_events_dedupe() {
        let usage = LlmUsage::new(20, 5, 25, Some(4), Some(2)).expect("usage");
        let details = usage_event_details("run-a", 1, Some(&usage));
        assert_eq!(details["unknown"], false);
        assert_eq!(details["cached_input_tokens"], 4);
        let missing = usage_event_details("run-a", 2, None);
        assert_eq!(missing["unknown"], true);
        assert!(missing.get("input_tokens").is_none());
        let session: Session = serde_json::from_value(json!({
            "id": "usage-session",
            "messages": [],
            "events": [
                {"index": 0, "kind": "context_usage", "details": details},
                {"index": 1, "kind": "context_usage", "details": details},
                {"index": 2, "kind": "context_usage", "details": missing}
            ]
        }))
        .expect("session");
        let totals = accumulate_session_usage(&session);
        assert!(totals.unknown);
        assert_eq!(totals.input_tokens, Some(20));
        assert_eq!(totals.output_tokens, Some(5));
    }

    #[test]
    fn inspect_json_distinguishes_live_request_from_project_preview() {
        let session: Session = serde_json::from_value(json!({
            "id": "inspect-session",
            "messages": [{"index": 0, "role": "user", "content": "secret-token"}],
            "events": [{
                "index": 0,
                "kind": "context_bounded",
                "details": snapshot_from_bounded_input(
                    "execution",
                    "sent",
                    "configured",
                    8_192,
                    &bounded_fixture(700),
                ).to_event_details("run-z")
            }]
        }))
        .expect("session");
        let snapshot = latest_from_session(&session);
        let json = inspect_json(&session, snapshot.as_ref());
        assert_eq!(json["kind"], "live_request");
        assert_eq!(json["session_id"], "inspect-session");
        assert_eq!(json["historical_accounting"], true);
        assert_eq!(project_preview_json("assembled")["kind"], "project_preview");
        let text = inspect_text(&session, snapshot.as_ref(), true);
        assert!(text.contains("ctx ~"));
        assert!(text.contains("allowance"));
        assert!(text.contains("Cumulative consumption"));
        assert!(!text.contains("secret-token"));
    }

    #[test]
    fn old_sessions_without_snapshot_do_not_fabricate_usage() {
        let session: Session = serde_json::from_value(json!({
            "id": "legacy",
            "messages": [{"index": 0, "role": "user", "content": "hello"}]
        }))
        .expect("session");
        assert!(latest_from_session(&session).is_none());
        let json = inspect_json(&session, None);
        assert_eq!(json["historical_accounting"], false);
        assert_eq!(json["snapshot"], Value::Null);
        let text = inspect_text(&session, None, true);
        assert!(text.contains("unavailable historical accounting"));
    }

    #[test]
    fn apply_response_reserve_sets_output_ceiling_from_budget() {
        let messages = [LlmMessage::user("hello")];
        let request = apply_response_reserve(LlmRequest::new(&messages, None), 10_000);
        assert_eq!(request.max_output_tokens, Some(1_000));
    }

    #[test]
    fn continuation_admission_is_transport_agnostic() {
        for _transport in [
            "mock",
            "chat_completions",
            "responses",
            "anthropic",
            "gemini",
        ] {
            assert_eq!(
                continuation_dispatch(8_000, 1_000, 200, 1),
                ContinuationDispatch::Send
            );
        }
    }

    #[test]
    fn snapshot_lookup_does_not_scan_transcript_messages() {
        let mut messages = Vec::new();
        for index in 0..2_000 {
            messages.push(json!({"index": index, "role": "user", "content": "x".repeat(80)}));
        }
        let snapshot = snapshot_from_bounded_input(
            "execution",
            "sent",
            "configured",
            16_000,
            &bounded_fixture(321),
        );
        let session: Session = serde_json::from_value(json!({
            "id": "large-history",
            "messages": messages,
            "events": [{
                "index": 0,
                "kind": "context_bounded",
                "details": snapshot.to_event_details("run-large")
            }]
        }))
        .expect("session");
        let restored = latest_from_session(&session).expect("snapshot");
        assert_eq!(restored.estimated_input, 321);
        assert_eq!(session.messages.len(), 2_000);
        let inspect = inspect_text(&session, Some(&restored), false);
        assert!(inspect.starts_with("ctx ~321/16k"));
    }
}
