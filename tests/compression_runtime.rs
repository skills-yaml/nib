use async_trait::async_trait;
use nib::config::NibConfig;
use nib::context::bounded_session_context;
use nib::context::compression::{approximate_tokens, maybe_compress_session};
use nib::llm::{LlmClient, LlmRequest, LlmResponse};
use nib::session::SessionStore;
use serde_json::Value;
use std::sync::Arc;
use tempfile::tempdir;

struct SummaryLlm;

#[async_trait]
impl LlmClient for SummaryLlm {
    async fn complete(&self, _request: LlmRequest<'_>) -> Result<LlmResponse, String> {
        Ok(LlmResponse::text(format!(
            "Dense retained facts: {}",
            "verified context ".repeat(80)
        )))
    }
}

#[tokio::test]
async fn compression_bounds_hot_context_and_retains_raw_audit_history() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::new(directory.path());
    let session = store.create_session();
    for index in 0..8 {
        store
            .try_append_message(
                &session.id,
                "user",
                &format!("request {index}: {}", "user detail ".repeat(35)),
            )
            .expect("user message");
        store
            .try_append_message(
                &session.id,
                "assistant",
                &format!("response {index}: {}", "assistant fact ".repeat(35)),
            )
            .expect("assistant message");
    }
    let before = store.load(&session.id).expect("session before compression");
    let raw_before = before
        .messages
        .iter()
        .map(|message| (message.index, message.role.clone(), message.content.clone()))
        .collect::<Vec<_>>();

    let mut config = NibConfig::default();
    config.llm.context_length = 1_000;
    config.compression.threshold = 0.50;
    config.compression.target_ratio = 0.20;
    let llm: Arc<dyn LlmClient> = Arc::new(SummaryLlm);
    let report = maybe_compress_session(&store, &session.id, &llm, &config)
        .await
        .expect("compression")
        .expect("threshold exceeded");

    assert!(report.before_tokens > 500);
    assert_eq!(report.target_tokens, 200);
    assert!(report.after_tokens <= report.target_tokens);
    let after = store.load(&session.id).expect("session after compression");
    let raw_after = after
        .messages
        .iter()
        .map(|message| (message.index, message.role.clone(), message.content.clone()))
        .collect::<Vec<_>>();
    assert_eq!(raw_after, raw_before, "compression must retain raw history");
    assert!(after.summary.is_some());
    assert!(after.summary_index > 0);
    let audit = after
        .events
        .iter()
        .find(|event| event.kind == "compression")
        .expect("compression audit event");
    assert_eq!(audit.details["raw_message_count"], raw_before.len());

    let bounded = bounded_session_context(&after, report.target_tokens);
    let actual_tokens = bounded
        .summary
        .as_deref()
        .map(approximate_tokens)
        .unwrap_or(0)
        + bounded
            .messages
            .iter()
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .map(approximate_tokens)
            .sum::<usize>();
    assert_eq!(bounded.approximate_tokens, actual_tokens);
    assert!(actual_tokens <= report.target_tokens);
}
