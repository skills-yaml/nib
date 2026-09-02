use async_trait::async_trait;
use nib::config::{NibConfig, ProviderEntry};
use nib::context::bounded_session_context;
use nib::context::compression::{
    approximate_tokens, explicitly_compress_session, maybe_compress_session,
};
use nib::llm::{LlmClient, LlmRequest, LlmResponse};
use nib::session::SessionStore;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio::sync::oneshot;

struct SummaryLlm;

#[async_trait]
impl LlmClient for SummaryLlm {
    async fn complete(&self, _request: LlmRequest<'_>) -> Result<LlmResponse, nib::llm::LlmError> {
        Ok(LlmResponse::text(format!(
            "Dense retained facts: {}",
            "verified context ".repeat(80)
        )))
    }
}

struct SensitiveSummaryLlm {
    content: String,
}

#[async_trait]
impl LlmClient for SensitiveSummaryLlm {
    async fn complete(&self, _request: LlmRequest<'_>) -> Result<LlmResponse, nib::llm::LlmError> {
        Ok(LlmResponse::text(self.content.clone()))
    }
}

struct BlockingSummaryLlm {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl LlmClient for BlockingSummaryLlm {
    async fn complete(&self, _request: LlmRequest<'_>) -> Result<LlmResponse, nib::llm::LlmError> {
        if let Some(entered) = self.entered.lock().expect("entered lock").take() {
            let _ = entered.send(());
        }
        let release = self.release.lock().expect("release lock").take();
        if let Some(release) = release {
            let _ = release.await;
        }
        Ok(LlmResponse::text("bounded concurrent summary"))
    }
}

#[tokio::test]
async fn explicit_compression_rejects_concurrently_appended_history() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::new(directory.path());
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "fact before compression")
        .expect("user message");
    store
        .try_append_message(&session.id, "assistant", "answer before compression")
        .expect("assistant message");

    let mut config = NibConfig::default();
    config.llm.context_length = 100_000;
    config.compression.threshold = 0.50;
    config.compression.target_ratio = 0.20;
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let llm: Arc<dyn LlmClient> = Arc::new(BlockingSummaryLlm {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
    });
    let worker_store = store.clone();
    let worker_session_id = session.id.clone();
    let worker_llm = llm.clone();
    let worker_config = config.clone();
    let worker = tokio::spawn(async move {
        explicitly_compress_session(
            &worker_store,
            &worker_session_id,
            &worker_llm,
            &worker_config,
        )
        .await
    });

    entered_rx.await.expect("provider request entered");
    store
        .try_append_message(&session.id, "user", "concurrently appended history")
        .expect("concurrent append");
    release_tx.send(()).expect("release provider");
    let error = worker
        .await
        .expect("compression worker")
        .expect_err("stale summary publication must fail closed");
    assert!(error
        .to_string()
        .contains("session history changed while compression was in flight"));

    let after = store.load(&session.id).expect("session after rejected CAS");
    assert_eq!(after.summary, None);
    assert_eq!(after.summary_index, 0);
    assert_eq!(after.messages.len(), 3);
    assert_eq!(after.messages[2].content, "concurrently appended history");
    assert!(!after.events.iter().any(|event| event.kind == "compression"));
}

#[tokio::test]
async fn explicit_compression_bypasses_only_the_automatic_threshold() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::new(directory.path());
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "small explicit compact request")
        .expect("user message");
    store
        .try_append_message(&session.id, "assistant", "small retained response")
        .expect("assistant message");
    let raw_before = store.load(&session.id).expect("session").messages;

    let mut config = NibConfig::default();
    config.llm.context_length = 100_000;
    config.compression.threshold = 0.50;
    config.compression.target_ratio = 0.20;
    let llm: Arc<dyn LlmClient> = Arc::new(SummaryLlm);
    assert!(maybe_compress_session(&store, &session.id, &llm, &config)
        .await
        .expect("automatic check")
        .is_none());

    let report = explicitly_compress_session(&store, &session.id, &llm, &config)
        .await
        .expect("explicit compression")
        .expect("uncompressed history");
    assert!(report.target_tokens < report.before_tokens);
    let after = store.load(&session.id).expect("compressed session");
    assert_eq!(after.messages, raw_before, "raw audit history is immutable");
    assert_eq!(
        after
            .events
            .iter()
            .filter(|event| event.kind == "compression")
            .count(),
        1
    );

    config.compression.enabled = false;
    store
        .try_append_message(&session.id, "user", "new history")
        .expect("new user message");
    assert!(
        explicitly_compress_session(&store, &session.id, &llm, &config)
            .await
            .expect("disabled compression")
            .is_none()
    );
}

#[tokio::test]
async fn compression_projects_provider_summary_before_persistence() {
    let directory = tempdir().expect("tempdir");
    let store = SessionStore::new(directory.path());
    let session = store.create_session();
    for index in 0..6 {
        store
            .try_append_message(
                &session.id,
                "user",
                &format!("request {index}: {}", "historic detail ".repeat(20)),
            )
            .expect("user message");
        store
            .try_append_message(
                &session.id,
                "assistant",
                &format!("response {index}: {}", "verified fact ".repeat(20)),
            )
            .expect("assistant message");
    }

    let secret = "compress/provider-secret";
    let mut config = NibConfig::default();
    config.llm.context_length = 400;
    config.compression.threshold = 0.50;
    config.compression.target_ratio = 0.20;
    config.llm.providers.insert(
        "private-fixture".to_string(),
        ProviderEntry {
            model: "fixture-model".to_string(),
            api_key: Some(secret.to_string()),
            ..ProviderEntry::default()
        },
    );
    let llm: Arc<dyn LlmClient> = Arc::new(SensitiveSummaryLlm {
        content: format!(
            "{secret} compress\\/provider-secret Y29tcHJlc3MvcHJvdmlkZXItc2VjcmV0 \u{1b}[2J {} SUMMARY_PRIVATE_TAIL",
            "s".repeat(2_000),
        ),
    });

    explicitly_compress_session(&store, &session.id, &llm, &config)
        .await
        .expect("compression succeeds")
        .expect("history was compressed");

    let persisted = store.load(&session.id).expect("compressed session");
    let summary = persisted.summary.as_deref().expect("public summary");
    for forbidden in [
        secret,
        r"compress\/provider-secret",
        "Y29tcHJlc3MvcHJvdmlkZXItc2VjcmV0",
        "SUMMARY_PRIVATE_TAIL",
    ] {
        assert!(!summary.contains(forbidden));
    }
    assert!(!summary.contains('\u{1b}'));
    assert!(summary.len() <= 320);
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
