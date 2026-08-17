use crate::config::NibConfig;
use crate::context::budget::bound_single_turn_input;
use crate::llm::{LlmClient, LlmError, LlmErrorPhase, LlmRequest};
use crate::session::{SessionError, SessionEvent, SessionStore};
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionReport {
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub summarized_from: usize,
    pub summarized_through: usize,
    pub target_tokens: usize,
}

pub fn approximate_tokens(content: &str) -> usize {
    content.chars().count().div_ceil(4)
}

pub fn truncate_to_tokens(content: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    let char_count = content.chars().count();
    if char_count <= max_chars {
        return content.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }

    let marker = "\n...[bounded]...\n";
    let marker_len = marker.chars().count();
    if max_chars <= marker_len + 2 {
        return content.chars().take(max_chars).collect();
    }
    let available = max_chars - marker_len;
    let head_len = available / 2;
    let tail_len = available - head_len;
    let head: String = content.chars().take(head_len).collect();
    let tail: String = content
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}{marker}{tail}")
}

pub async fn maybe_compress_session(
    store: &SessionStore,
    session_id: &str,
    llm: &Arc<dyn LlmClient>,
    cfg: &NibConfig,
) -> Result<Option<CompressionReport>, LlmError> {
    if !cfg.compression.enabled {
        return Ok(None);
    }
    if !(0.0..=1.0).contains(&cfg.compression.threshold) || cfg.compression.threshold == 0.0 {
        return Err(
            LlmError::configuration("compression.threshold must be in (0, 1]")
                .with_phase(LlmErrorPhase::Compression),
        );
    }
    if !(0.0..1.0).contains(&cfg.compression.target_ratio)
        || cfg.compression.target_ratio == 0.0
        || cfg.compression.target_ratio >= cfg.compression.threshold
    {
        return Err(LlmError::configuration(
            "compression.target_ratio must be in (0, compression.threshold)",
        )
        .with_phase(LlmErrorPhase::Compression));
    }

    let session = match store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session for compression: {error}"))?
    {
        Some(s) => s,
        None => return Ok(None),
    };

    let summary_start = session.summary_index.min(session.messages.len());
    let uncompressed_messages = &session.messages[summary_start..];
    let before_tokens = uncompressed_messages
        .iter()
        .map(|message| approximate_tokens(&message.content))
        .sum::<usize>();

    let threshold_tokens = (cfg.llm.context_length as f64 * cfg.compression.threshold) as usize;

    if before_tokens <= threshold_tokens {
        return Ok(None);
    }

    let target_tokens =
        ((cfg.llm.context_length as f64 * cfg.compression.target_ratio) as usize).max(1);
    let recent_budget = (target_tokens / 2).max(1);
    let mut retained_start = session.messages.len();
    let mut retained_tokens = 0usize;
    for (index, message) in session.messages[summary_start..].iter().enumerate().rev() {
        let tokens = approximate_tokens(&message.content);
        if retained_tokens > 0 && retained_tokens.saturating_add(tokens) > recent_budget {
            break;
        }
        retained_tokens = retained_tokens.saturating_add(tokens);
        retained_start = summary_start + index;
        if retained_tokens >= recent_budget {
            break;
        }
    }

    if retained_start == summary_start {
        retained_start = session.messages.len();
        retained_tokens = 0;
    }
    let to_compress = &session.messages[summary_start..retained_start];
    if to_compress.is_empty() {
        return Ok(None);
    }

    let mut summary_prompt = if let Some(existing) = &session.summary {
        format!(
            "Previous summary:\n{}\n\nNew messages to append to summary:\n\n",
            existing
        )
    } else {
        String::from("Summarize the following historic facts, code progress, decisions, and lessons learned into a compact narrative:\n\n")
    };

    for msg in to_compress {
        summary_prompt.push_str(&format!(
            "message[{}] {}: {}\n\n",
            msg.index, msg.role, msg.content
        ));
    }

    let bounded = bound_single_turn_input(
        "You are a context compression engine. Summarize the provided conversation history into a highly dense, factual narrative. Preserve all key decisions, code snippets, paths, and lessons learned. Discard conversational filler.",
        &summary_prompt,
        None,
        cfg.llm.context_length,
        8,
    )?;
    let response = llm
        .complete(LlmRequest::new(
            &bounded.messages,
            bounded.tools.as_deref(),
            0.3,
        ))
        .await
        .map_err(|error| error.with_phase(LlmErrorPhase::Compression))?;
    let summary_content = response
        .content
        .filter(|content| !content.trim().is_empty())
        .ok_or_else(|| "compression model returned an empty summary".to_string())?;

    let summary_budget = target_tokens.saturating_sub(retained_tokens.min(target_tokens));
    let bounded_summary = truncate_to_tokens(&summary_content, summary_budget.max(1));

    let expected_summary = session.summary.clone();
    let expected_summary_index = session.summary_index;
    let expected_prefix = session.messages[..retained_start].to_vec();
    let report = store
        .update_session(session_id, move |current| {
            if current.summary != expected_summary
                || current.summary_index != expected_summary_index
                || current.messages.len() < retained_start
                || current.messages[..retained_start] != expected_prefix
            {
                return Err(SessionError::InvalidMutation(
                    "session history changed while compression was in flight".to_string(),
                ));
            }

            current.summary = Some(bounded_summary);
            current.summary_index = retained_start;
            let bounded = crate::context::bounded_session_context(current, target_tokens);
            let report = CompressionReport {
                before_tokens,
                after_tokens: bounded.approximate_tokens,
                summarized_from: summary_start,
                summarized_through: retained_start.saturating_sub(1),
                target_tokens,
            };
            current.events.push(SessionEvent {
                index: current.events.len(),
                kind: "compression".to_string(),
                details: json!({
                    "before_tokens": report.before_tokens,
                    "after_tokens": report.after_tokens,
                    "summarized_from": report.summarized_from,
                    "summarized_through": report.summarized_through,
                    "target_tokens": report.target_tokens,
                    "raw_message_count": current.messages.len(),
                }),
                timestamp: Some(Utc::now()),
            });
            Ok(report)
        })
        .map_err(|error| format!("failed to persist compressed session: {error}"))?;

    Ok(Some(report))
}
