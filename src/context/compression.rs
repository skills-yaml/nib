use crate::config::NibConfig;
use crate::llm::LlmClient;
use crate::session::{SessionMessage, SessionStore};
use serde_json::json;
use std::sync::Arc;

pub async fn maybe_compress_session(
    store: &SessionStore,
    session_id: &str,
    llm: &Arc<dyn LlmClient>,
    cfg: &NibConfig,
) -> Result<(), String> {
    if !cfg.compression.enabled {
        return Ok(());
    }

    let mut session = match store.load(session_id) {
        Some(s) => s,
        None => return Ok(()),
    };

    let mut total_chars = 0;
    for msg in &session.messages {
        total_chars += msg.content.len();
    }
    let approx_tokens = total_chars / 4;

    let threshold_tokens = (cfg.llm.context_length as f64 * cfg.compression.threshold) as usize;

    if approx_tokens <= threshold_tokens {
        return Ok(());
    }

    if session.messages.len() <= 5 {
        return Ok(());
    }

    let keep_count = 4;
    let compress_count = session.messages.len() - keep_count;
    let to_compress = &session.messages[0..compress_count];

    let mut summary_prompt = String::from(
        "Summarize the following historic facts, code progress, decisions, and lessons learned into a compact narrative:\n\n",
    );
    for msg in to_compress {
        summary_prompt.push_str(&format!("{}: {}\n\n", msg.role, msg.content));
    }

    let messages = vec![
        json!({"role": "system", "content": "You are a context compression engine. Summarize the provided conversation history into a highly dense, factual narrative. Preserve all key decisions, code snippets, paths, and lessons learned. Discard conversational filler."}),
        json!({"role": "user", "content": summary_prompt}),
    ];

    let response = llm.complete(&messages, None, 0.3).await?;
    let summary_content = response
        .content
        .unwrap_or_else(|| "Failed to generate summary".to_string());

    let mut new_messages = Vec::new();
    new_messages.push(SessionMessage {
        role: "system".to_string(),
        content: format!("COMPRESSED CONTEXT SUMMARY:\n{}", summary_content),
        timestamp: Some(chrono::Utc::now()),
    });

    new_messages.extend(
        session.messages[session.messages.len() - keep_count..]
            .iter()
            .cloned(),
    );

    session.messages = new_messages;
    store
        .save(&session)
        .map_err(|e| format!("Failed to save compressed session: {}", e))?;

    Ok(())
}
