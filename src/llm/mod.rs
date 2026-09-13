//! Pluggable LLM providers (HTTP) and mock for CI.

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde_json::Value;
use std::future::Future;
use std::time::Duration;

pub mod anthropic;
pub(crate) mod compatible;
pub mod conformance;
pub mod error;
pub mod factory;
pub mod gemini;
pub mod mock;
pub mod openai;
pub mod registry;
pub mod responses;
pub mod types;

pub use error::{
    LlmError, LlmErrorClass, LlmErrorMetadata, LlmErrorPhase, LlmProviderErrorDiscriminator,
    RetryDisposition,
};
pub use factory::{create_client, provider_ready};
pub use mock::MockLlmClient;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, oneshot};
pub use types::{
    GenerationOptions, LlmDelta, LlmFinishReason, LlmMessage, LlmMessageRole, LlmRequest,
    LlmRequestScope, LlmResponse, LlmStreamEvent, LlmTerminalStatus, LlmUsage, ProviderCallId,
    ProviderContinuation, ReasoningOption, StreamEvent, ToolCallAccumulator, ToolCallRequest,
    ToolDefinition, ToolResult, ToolResultClass,
};

pub(crate) const MAX_LLM_COMPLETE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_LLM_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LLM_STREAM_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_LLM_STREAM_EVENT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_LLM_RESPONSE_ITEMS: usize = 256;
pub(crate) const MAX_LLM_STREAM_EVENTS: usize = 65_536;

pub(crate) struct ResponseByteBudget {
    consumed: usize,
    limit: usize,
    label: &'static str,
}

pub(crate) struct ResponseItemBudget {
    consumed: usize,
    limit: usize,
    label: &'static str,
}

impl ResponseItemBudget {
    pub(crate) fn new(limit: usize, label: &'static str) -> Self {
        Self {
            consumed: 0,
            limit,
            label,
        }
    }

    pub(crate) fn account(&mut self, items: usize) -> Result<(), String> {
        self.consumed = self
            .consumed
            .checked_add(items)
            .ok_or_else(|| format!("{} item count overflowed", self.label))?;
        if self.consumed > self.limit {
            return Err(format!(
                "{} exceeds the {}-item limit",
                self.label, self.limit
            ));
        }
        Ok(())
    }
}

pub(crate) fn ensure_response_item_count(items: usize, label: &'static str) -> Result<(), String> {
    if items > MAX_LLM_RESPONSE_ITEMS {
        return Err(format!(
            "{label} exceeds the {MAX_LLM_RESPONSE_ITEMS}-item limit"
        ));
    }
    Ok(())
}

impl ResponseByteBudget {
    pub(crate) fn new(limit: usize, label: &'static str) -> Self {
        Self {
            consumed: 0,
            limit,
            label,
        }
    }

    pub(crate) fn account(&mut self, bytes: usize) -> Result<(), String> {
        self.consumed = self
            .consumed
            .checked_add(bytes)
            .ok_or_else(|| format!("{} byte count overflowed", self.label))?;
        if self.consumed > self.limit {
            return Err(format!(
                "{} exceeds the {}-byte limit",
                self.label, self.limit
            ));
        }
        Ok(())
    }
}

pub(crate) fn ensure_response_content_length(
    response: &Response,
    limit: usize,
    label: &'static str,
) -> Result<(), String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("{label} exceeds the {limit}-byte limit"));
    }
    Ok(())
}

pub(crate) fn ensure_stream_event_size(bytes: usize, label: &'static str) -> Result<(), String> {
    if bytes > MAX_LLM_STREAM_EVENT_BYTES {
        return Err(format!(
            "{label} exceeds the {MAX_LLM_STREAM_EVENT_BYTES}-byte event limit"
        ));
    }
    Ok(())
}

pub(crate) async fn next_stream_item_or_closed<S>(
    sender: &Sender<Result<LlmStreamEvent, LlmStreamFailure>>,
    stream: &mut S,
) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    tokio::select! {
        biased;
        _ = sender.closed() => None,
        item = stream.next() => item,
    }
}

pub(crate) async fn send_stream_event(
    sender: &Sender<Result<LlmStreamEvent, LlmStreamFailure>>,
    event: Result<LlmStreamEvent, LlmStreamFailure>,
) -> bool {
    sender.send(event).await.is_ok()
}

#[derive(Clone)]
pub(crate) enum LlmStreamFailure {
    Protocol(String),
    Typed(LlmError),
}

impl std::fmt::Debug for LlmStreamFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(message) => formatter
                .debug_struct("Protocol")
                .field("message_bytes", &message.len())
                .finish(),
            Self::Typed(error) => formatter
                .debug_struct("Typed")
                .field("class", &error.class)
                .field("phase", &error.phase)
                .finish(),
        }
    }
}

impl From<String> for LlmStreamFailure {
    fn from(message: String) -> Self {
        Self::Protocol(message)
    }
}

impl From<&str> for LlmStreamFailure {
    fn from(message: &str) -> Self {
        Self::Protocol(message.to_string())
    }
}

impl From<LlmError> for LlmStreamFailure {
    fn from(error: LlmError) -> Self {
        Self::Typed(error)
    }
}

pub struct LlmStream {
    receiver: Receiver<Result<LlmStreamEvent, LlmStreamFailure>>,
    private_completion: Option<oneshot::Receiver<Result<LlmResponse, LlmStreamFailure>>>,
    error_context: Option<error::LlmErrorContext>,
    content: String,
    tool_calls: ToolCallAccumulator,
    finish_reason: Option<LlmFinishReason>,
    stream_error: Option<LlmError>,
    exhausted: bool,
}

impl std::fmt::Debug for LlmStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmStream")
            .field("private_completion", &self.private_completion.is_some())
            .field("has_error_context", &self.error_context.is_some())
            .field("content_bytes", &self.content.len())
            .field("has_finish_reason", &self.finish_reason.is_some())
            .field("has_error", &self.stream_error.is_some())
            .field("exhausted", &self.exhausted)
            .finish_non_exhaustive()
    }
}

impl LlmStream {
    pub(crate) fn from_public_receiver(
        receiver: Receiver<Result<LlmStreamEvent, LlmStreamFailure>>,
    ) -> Self {
        Self {
            receiver,
            private_completion: None,
            error_context: None,
            content: String::new(),
            tool_calls: ToolCallAccumulator::default(),
            finish_reason: None,
            stream_error: None,
            exhausted: false,
        }
    }

    pub(crate) fn with_private_completion(
        receiver: Receiver<Result<LlmStreamEvent, LlmStreamFailure>>,
        completion: oneshot::Receiver<Result<LlmResponse, LlmStreamFailure>>,
    ) -> Self {
        Self {
            private_completion: Some(completion),
            ..Self::from_public_receiver(receiver)
        }
    }

    pub(crate) fn with_error_context(mut self, context: error::LlmErrorContext) -> Self {
        self.error_context = Some(context);
        self
    }

    fn from_response(response: LlmResponse) -> Self {
        let (tx, rx) = mpsc::channel(100);
        let (completion_tx, completion_rx) = oneshot::channel();
        let content = response.content.clone();
        let calls = response.tool_calls.clone().unwrap_or_default();
        let finish_reason = response.finish_reason;
        tokio::spawn(async move {
            if let Some(content) = content {
                if !send_stream_event(&tx, Ok(LlmStreamEvent::Delta(LlmDelta::Content(content))))
                    .await
                {
                    return;
                }
            }
            for (index, call) in calls.into_iter().enumerate() {
                if !send_stream_event(
                    &tx,
                    Ok(LlmStreamEvent::Delta(LlmDelta::ToolCallChunk {
                        index,
                        name: Some(call.name),
                        arguments: Some(call.arguments.to_string()),
                    })),
                )
                .await
                {
                    return;
                }
            }
            if !send_stream_event(&tx, Ok(LlmStreamEvent::Terminal(finish_reason))).await {
                return;
            }
            let _ = completion_tx.send(Ok(response));
        });
        Self::with_private_completion(rx, completion_rx)
    }

    /// Consumes one unvalidated provider event inside the LLM adapter boundary.
    ///
    /// This is intentionally unavailable outside `crate::llm`: a delta can precede an
    /// in-band error, refusal, malformed terminal, or premature EOF and therefore is not a
    /// public observation. Application consumers must use `finish()` and project only the
    /// returned terminal-authoritative response.
    pub(in crate::llm) async fn recv_private(
        &mut self,
    ) -> Option<Result<LlmStreamEvent, Box<LlmError>>> {
        let next = self
            .receiver
            .recv()
            .await
            .map(|result| result.map_err(|error| Box::new(self.stream_failure(error))));
        match &next {
            Some(Ok(event)) => match event {
                LlmStreamEvent::Delta(delta) => {
                    self.tool_calls.push(delta);
                    if let LlmDelta::Content(fragment) = delta {
                        self.content.push_str(fragment);
                    }
                }
                LlmStreamEvent::Terminal(reason) => self.finish_reason = Some(*reason),
            },
            Some(Err(error)) => {
                if self.stream_error.is_none() {
                    self.stream_error = Some((**error).clone());
                }
            }
            None => self.exhausted = true,
        }
        next
    }

    /// Returns the canonical provider-neutral typed failure so terminal validation retains its
    /// retry, phase, and redaction-safe reporting metadata. Boxing this sole stream boundary would
    /// split the public LLM error contract while the authoritative error remains unchanged.
    #[allow(clippy::result_large_err)]
    pub async fn finish(mut self) -> Result<LlmResponse, LlmError> {
        while !self.exhausted {
            if self.recv_private().await.is_none() {
                break;
            }
        }
        if let Some(error) = self.stream_error {
            return Err(error);
        }
        if let Some(completion) = self.private_completion.take() {
            return completion
                .await
                .map_err(|_| {
                    self.stream_failure("provider stream ended without a private completion".into())
                })?
                .map_err(|error| self.stream_failure(error));
        }
        let finish_reason = match self.finish_reason.take() {
            Some(reason) => reason,
            None => {
                return Err(
                    self.stream_failure("provider stream ended before a terminal event".into())
                )
            }
        };
        let tool_calls = match std::mem::take(&mut self.tool_calls).finish() {
            Ok(tool_calls) => tool_calls,
            Err(error) => return Err(self.stream_failure(error.into())),
        };
        let terminal_status = finish_reason.terminal_status();
        let finish_matches_calls = match finish_reason {
            LlmFinishReason::Complete | LlmFinishReason::Refusal => tool_calls.is_empty(),
            LlmFinishReason::ToolCalls => !tool_calls.is_empty(),
        };
        if !finish_matches_calls {
            return Err(self.stream_failure(
                "provider terminal reason did not match the completed tool-call set".into(),
            ));
        }
        Ok(LlmResponse {
            terminal_status,
            content: (!self.content.trim().is_empty()).then_some(self.content),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            finish_reason,
            continuation: None,
            usage: None,
            attempts: RetryAttemptMetadata::no_network_attempt(),
        })
    }

    fn stream_failure(&self, failure: LlmStreamFailure) -> LlmError {
        match failure {
            LlmStreamFailure::Typed(error) => match self.error_context.as_ref() {
                Some(context) => context.attach_retry_attempts(error),
                None => error,
            },
            LlmStreamFailure::Protocol(message) => match self.error_context.as_ref() {
                Some(context) => context.protocol(LlmErrorPhase::Stream, message),
                None => LlmError::local(LlmErrorClass::Protocol, LlmErrorPhase::Stream, message),
            },
        }
    }
}

pub(crate) async fn read_bounded_response(
    response: Response,
    limit: usize,
    label: &'static str,
) -> Result<Vec<u8>, String> {
    ensure_response_content_length(&response, limit, label)?;
    let mut budget = ResponseByteBudget::new(limit, label);
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("failed to read {label}: {error}"))?;
        budget.account(chunk.len())?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) async fn read_bounded_json_response(
    response: Response,
    label: &'static str,
) -> Result<Value, String> {
    let body = read_bounded_response(response, MAX_LLM_COMPLETE_RESPONSE_BYTES, label).await?;
    serde_json::from_slice(&body).map_err(|error| format!("invalid {label}: {error}"))
}

pub(crate) async fn read_bounded_error_response(
    response: Response,
    label: &'static str,
) -> Result<String, String> {
    let body = read_bounded_response(response, MAX_LLM_ERROR_RESPONSE_BYTES, label).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub base_delay: Duration,
    pub request_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            request_timeout: Duration::from_secs(60),
        }
    }
}

const MAX_RETRY_ATTEMPTS: usize = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(try_from = "RetryAttemptMetadataWire")]
pub struct RetryAttemptMetadata {
    attempts: u8,
    credential_rotation_occurred: bool,
    retry_exhausted: bool,
    final_retry_after_seconds: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryAttemptMetadataWire {
    attempts: u8,
    credential_rotation_occurred: bool,
    retry_exhausted: bool,
    final_retry_after_seconds: Option<u64>,
}

impl TryFrom<RetryAttemptMetadataWire> for RetryAttemptMetadata {
    type Error = &'static str;

    fn try_from(wire: RetryAttemptMetadataWire) -> Result<Self, Self::Error> {
        if wire.attempts > MAX_RETRY_ATTEMPTS as u8 {
            return Err("retry attempt count exceeds the global limit");
        }
        if wire.credential_rotation_occurred && wire.attempts < 2 {
            return Err("credential rotation requires at least two attempts");
        }
        if (wire.retry_exhausted || wire.final_retry_after_seconds.is_some()) && wire.attempts == 0
        {
            return Err("retry outcome metadata requires a network attempt");
        }
        if wire
            .final_retry_after_seconds
            .is_some_and(|seconds| seconds > MAX_RETRY_AFTER.as_secs())
        {
            return Err("Retry-After metadata exceeds the bounded limit");
        }
        Ok(Self {
            attempts: wire.attempts,
            credential_rotation_occurred: wire.credential_rotation_occurred,
            retry_exhausted: wire.retry_exhausted,
            final_retry_after_seconds: wire.final_retry_after_seconds,
        })
    }
}

impl RetryAttemptMetadata {
    pub const fn no_network_attempt() -> Self {
        Self {
            attempts: 0,
            credential_rotation_occurred: false,
            retry_exhausted: false,
            final_retry_after_seconds: None,
        }
    }

    fn new(
        attempts: usize,
        credential_rotation_occurred: bool,
        retry_exhausted: bool,
        final_retry_after: Option<Duration>,
    ) -> Self {
        debug_assert!(attempts <= MAX_RETRY_ATTEMPTS);
        Self {
            attempts: attempts.min(MAX_RETRY_ATTEMPTS) as u8,
            credential_rotation_occurred,
            retry_exhausted,
            final_retry_after_seconds: final_retry_after
                .filter(|delay| *delay <= MAX_RETRY_AFTER)
                .map(|delay| delay.as_secs()),
        }
    }

    pub fn attempts(self) -> u8 {
        self.attempts
    }

    pub fn credential_rotation_occurred(self) -> bool {
        self.credential_rotation_occurred
    }

    pub fn retry_exhausted(self) -> bool {
        self.retry_exhausted
    }

    pub fn final_retry_after_seconds(self) -> Option<u64> {
        self.final_retry_after_seconds
    }

    /// Maps a terminal retry failure into its error-only public disposition.
    /// Successful responses retain the numeric facts above and do not use this method.
    pub fn error_disposition(self) -> RetryDisposition {
        if self.attempts == 0 {
            RetryDisposition::NotAttempted
        } else if !self.retry_exhausted {
            RetryDisposition::NotRetryable
        } else if self.credential_rotation_occurred {
            RetryDisposition::ExhaustedAfterCredentialRotation
        } else {
            RetryDisposition::Exhausted
        }
    }
}

impl Default for RetryAttemptMetadata {
    fn default() -> Self {
        Self::no_network_attempt()
    }
}

#[derive(Debug)]
pub struct RetryOutcome<T> {
    pub value: T,
    pub attempts: RetryAttemptMetadata,
}

#[derive(Debug)]
pub struct RetryFailure<E> {
    pub error: E,
    pub attempts: RetryAttemptMetadata,
}

impl<E> RetryFailure<E> {
    fn map_error<M>(self, map: impl FnOnce(E) -> M) -> RetryFailure<M> {
        RetryFailure {
            error: map(self.error),
            attempts: self.attempts,
        }
    }
}

/// Sends a provider request with bounded retry and credential rotation.
///
/// The request factory receives the credential index to use, allowing every
/// provider to rotate through its configured key pool after HTTP 429.
pub async fn send_with_retry<F>(
    request_factory: F,
    credential_count: usize,
    retry_capabilities: registry::ProviderRetryCapabilities,
) -> Result<RetryOutcome<Response>, RetryFailure<String>>
where
    F: FnMut(usize) -> RequestBuilder,
{
    let mut request_factory = request_factory;
    if credential_count == 0 {
        return Err(RetryFailure {
            error: "provider request requires at least one credential".to_string(),
            attempts: RetryAttemptMetadata::new(0, false, false, None),
        });
    }
    let policy = RetryPolicy::default();
    let request_timeout = policy.request_timeout;
    send_with_retry_using(
        move |credential_index| {
            request_factory(credential_index)
                .timeout(request_timeout)
                .send()
        },
        credential_count,
        policy,
        |response: &Response| response.status(),
        retry_capabilities,
        |response| retry_after_delay(response, retry_capabilities),
        |error: &reqwest::Error| error.is_timeout() || error.is_connect(),
    )
    .await
    .map_err(|failure| failure.map_error(|error| error.to_string()))
}

fn retry_after_delay(
    response: &Response,
    retry_capabilities: registry::ProviderRetryCapabilities,
) -> Option<Duration> {
    if !retry_capabilities.accepts_retry_after(response.status()) {
        return None;
    }
    let value = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    parse_retry_after_value(value, chrono::Utc::now())
}

fn parse_retry_after_value(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        let delay = Duration::from_secs(seconds);
        return (delay <= MAX_RETRY_AFTER).then_some(delay);
    }

    let retry_at = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&chrono::Utc);
    let delay = retry_at.signed_duration_since(now);
    let milliseconds = delay.num_milliseconds();
    if milliseconds <= 0 {
        return None;
    }
    let delay = Duration::from_millis(milliseconds as u64);
    (delay <= MAX_RETRY_AFTER).then_some(delay)
}

async fn send_with_retry_using<T, E, F, Fut, Status, RetryAfter, RetryError>(
    mut send: F,
    credential_count: usize,
    policy: RetryPolicy,
    status: Status,
    retry_capabilities: registry::ProviderRetryCapabilities,
    retry_after: RetryAfter,
    retry_error: RetryError,
) -> Result<RetryOutcome<T>, RetryFailure<E>>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    Status: Fn(&T) -> StatusCode,
    RetryAfter: Fn(&T) -> Option<Duration>,
    RetryError: Fn(&E) -> bool,
{
    assert!(
        credential_count > 0,
        "retry requires at least one credential"
    );
    let attempts = policy.max_attempts.clamp(1, MAX_RETRY_ATTEMPTS);
    let mut credential_index = 0;
    let mut credential_rotation_occurred = false;

    for attempt in 0..attempts {
        let actual_attempts = attempt + 1;
        match send(credential_index).await {
            Ok(response) if retry_capabilities.retries_status(status(&response)) => {
                let final_retry_after = retry_after(&response);
                if actual_attempts == attempts {
                    return Ok(RetryOutcome {
                        value: response,
                        attempts: RetryAttemptMetadata::new(
                            actual_attempts,
                            credential_rotation_occurred,
                            true,
                            final_retry_after,
                        ),
                    });
                }
                let response_status = status(&response);
                let delay = final_retry_after.unwrap_or_else(|| {
                    let multiplier = 1u32 << attempt.min(6);
                    policy
                        .base_delay
                        .saturating_mul(multiplier)
                        .min(MAX_RETRY_AFTER)
                });
                if retry_capabilities.rotates_credential(response_status) && credential_count > 1 {
                    let next_credential = (credential_index + 1) % credential_count;
                    credential_rotation_occurred |= next_credential != credential_index;
                    credential_index = next_credential;
                }
                tokio::time::sleep(delay).await;
            }
            Ok(response) => {
                return Ok(RetryOutcome {
                    value: response,
                    attempts: RetryAttemptMetadata::new(
                        actual_attempts,
                        credential_rotation_occurred,
                        false,
                        None,
                    ),
                });
            }
            Err(error) if retry_error(&error) && actual_attempts < attempts => {
                let multiplier = 1u32 << attempt.min(6);
                tokio::time::sleep(
                    policy
                        .base_delay
                        .saturating_mul(multiplier)
                        .min(MAX_RETRY_AFTER),
                )
                .await;
            }
            Err(error) => {
                let retry_exhausted = retry_error(&error) && actual_attempts == attempts;
                return Err(RetryFailure {
                    error,
                    attempts: RetryAttemptMetadata::new(
                        actual_attempts,
                        credential_rotation_occurred,
                        retry_exhausted,
                        None,
                    ),
                });
            }
        }
    }

    unreachable!("bounded provider retry loop always returns")
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, LlmError>;

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        let response = self.complete(request).await?;
        Ok(LlmStream::from_response(response))
    }
}

/// Compatibility name retained for callers while `LlmProvider` is the canonical
/// provider-neutral contract.
pub use LlmProvider as LlmClient;

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    pub struct ScriptedHttpResponse {
        status: String,
        content_type: String,
        body: String,
        headers: Vec<(String, String)>,
    }

    impl ScriptedHttpResponse {
        pub fn new(
            status: impl Into<String>,
            content_type: impl Into<String>,
            body: impl Into<String>,
        ) -> Self {
            Self {
                status: status.into(),
                content_type: content_type.into(),
                body: body.into(),
                headers: Vec::new(),
            }
        }

        pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
            self.headers.push((name.into(), value.into()));
            self
        }
    }

    pub fn serve_once(
        status: &str,
        content_type: &str,
        body: impl Into<String>,
    ) -> (String, Receiver<String>) {
        serve_once_with_declared_length(status, content_type, body, None)
    }

    pub fn serve_once_with_headers(
        status: &str,
        content_type: &str,
        body: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> (String, Receiver<String>) {
        serve_once_with_declared_length_and_headers(status, content_type, body, None, headers)
    }

    pub fn serve_once_with_declared_length(
        status: &str,
        content_type: &str,
        body: impl Into<String>,
        declared_length: Option<usize>,
    ) -> (String, Receiver<String>) {
        serve_once_with_declared_length_and_headers(
            status,
            content_type,
            body,
            declared_length,
            Vec::new(),
        )
    }

    fn serve_once_with_declared_length_and_headers(
        status: &str,
        content_type: &str,
        body: impl Into<String>,
        declared_length: Option<usize>,
        headers: Vec<(String, String)>,
    ) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test HTTP listener");
        let address = listener.local_addr().expect("test HTTP address");
        let status = status.to_string();
        let content_type = content_type.to_string();
        let body = body.into();
        let headers = headers
            .into_iter()
            .map(|(name, value)| {
                assert!(!name.contains(['\r', '\n']), "test response header name");
                assert!(!value.contains(['\r', '\n']), "test response header value");
                format!("{name}: {value}\r\n")
            })
            .collect::<String>();
        let (request_tx, request_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test HTTP connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read timeout");
            let request = read_request(&mut stream);
            let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
                declared_length.unwrap_or(body.len())
            );
            stream
                .write_all(response.as_bytes())
                .expect("test HTTP response");
        });

        (format!("http://{address}"), request_rx)
    }

    pub fn serve_sequence(responses: Vec<ScriptedHttpResponse>) -> (String, Receiver<String>) {
        assert!(!responses.is_empty(), "test HTTP sequence");
        let listener = TcpListener::bind("127.0.0.1:0").expect("test HTTP listener");
        let address = listener.local_addr().expect("test HTTP address");
        let (request_tx, request_rx) = mpsc::channel();

        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("test HTTP connection");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("request read timeout");
                let request = read_request(&mut stream);
                let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());
                let headers = response
                    .headers
                    .into_iter()
                    .map(|(name, value)| {
                        assert!(!name.contains(['\r', '\n']), "test response header name");
                        assert!(!value.contains(['\r', '\n']), "test response header value");
                        format!("{name}: {value}\r\n")
                    })
                    .collect::<String>();
                let encoded = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{}",
                    response.status,
                    response.content_type,
                    response.body.len(),
                    response.body,
                );
                stream
                    .write_all(encoded.as_bytes())
                    .expect("test HTTP response");
            }
        });

        (format!("http://{address}"), request_rx)
    }

    pub fn serve_open_stream(
        first_event: impl Into<String>,
    ) -> (String, Receiver<String>, Receiver<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test HTTP listener");
        let address = listener.local_addr().expect("test HTTP address");
        let first_event = first_event.into();
        let (request_tx, request_rx) = mpsc::channel();
        let (disconnect_tx, disconnect_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test HTTP connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read timeout");
            let request = read_request(&mut stream);
            let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{first_event}"
            );
            stream
                .write_all(response.as_bytes())
                .and_then(|_| stream.flush())
                .expect("first streaming event");

            let mut probe = [0_u8; 1];
            let disconnected = match stream.read(&mut probe) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error) => !matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ),
            };
            let _ = disconnect_tx.send(disconnected);
        });

        (format!("http://{address}"), request_rx, disconnect_rx)
    }

    fn read_request(stream: &mut impl Read) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream.read(&mut buffer).expect("test HTTP request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let content_length = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::time::timeout;

    fn common_retry_capabilities() -> registry::ProviderRetryCapabilities {
        registry::retry_capabilities("openai", registry::ProviderTransport::ChatCompletions)
    }

    #[test]
    fn retry_statuses_are_explicit_and_bounded() {
        let retry = common_retry_capabilities();
        for code in [408, 425, 429, 500, 502, 503, 504] {
            assert!(retry.retries_status(StatusCode::from_u16(code).unwrap()));
        }
        assert!(!retry.retries_status(StatusCode::BAD_REQUEST));
        assert!(!retry.retries_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn retry_policy_has_finite_defaults() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_attempts, 3);
        assert!(policy.base_delay > Duration::ZERO);
        assert!(policy.request_timeout > Duration::ZERO);
    }

    #[tokio::test]
    async fn retries_429_with_the_next_configured_credential() {
        let mut responses = VecDeque::from([StatusCode::TOO_MANY_REQUESTS, StatusCode::OK]);
        let mut credential_indices = Vec::new();
        let response = send_with_retry_using(
            |credential_index| {
                credential_indices.push(credential_index);
                std::future::ready(Ok::<StatusCode, String>(
                    responses.pop_front().expect("scripted response"),
                ))
            },
            2,
            RetryPolicy {
                max_attempts: 2,
                base_delay: Duration::ZERO,
                request_timeout: Duration::from_secs(1),
            },
            |status| *status,
            common_retry_capabilities(),
            |_| None,
            |_| false,
        )
        .await
        .expect("backup credential succeeds");
        assert_eq!(response.value, StatusCode::OK);
        assert_eq!(response.attempts.attempts(), 2);
        assert!(response.attempts.credential_rotation_occurred());
        assert!(!response.attempts.retry_exhausted());
        assert_eq!(credential_indices, [0, 1]);
    }

    #[tokio::test]
    async fn retry_budget_is_global_and_non_rate_limits_keep_the_credential() {
        let mut responses = VecDeque::from([
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE,
        ]);
        let mut credential_indices = Vec::new();
        let response = send_with_retry_using(
            |credential_index| {
                credential_indices.push(credential_index);
                std::future::ready(Ok::<StatusCode, String>(
                    responses.pop_front().expect("scripted response"),
                ))
            },
            4,
            RetryPolicy {
                max_attempts: usize::MAX,
                base_delay: Duration::ZERO,
                request_timeout: Duration::from_secs(1),
            },
            |status| *status,
            common_retry_capabilities(),
            |_| None,
            |_| false,
        )
        .await
        .expect("last transient response is returned to the adapter");
        assert_eq!(response.value, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.attempts.attempts(), 3);
        assert!(response.attempts.retry_exhausted());
        assert!(!response.attempts.credential_rotation_occurred());
        assert_eq!(
            response.attempts.error_disposition(),
            RetryDisposition::Exhausted
        );
        assert_eq!(credential_indices, [0, 0, 0]);
        assert!(responses.is_empty());
        let encoded = serde_json::to_string(&response.attempts).unwrap();
        assert!(encoded.len() < 192);
        assert_eq!(
            serde_json::from_str::<RetryAttemptMetadata>(&encoded).unwrap(),
            response.attempts
        );
        for invalid in [
            r#"{"attempts":4,"credential_rotation_occurred":false,"retry_exhausted":true,"final_retry_after_seconds":null}"#,
            r#"{"attempts":3,"credential_rotation_occurred":false,"retry_exhausted":true,"final_retry_after_seconds":31}"#,
        ] {
            assert!(serde_json::from_str::<RetryAttemptMetadata>(invalid).is_err());
        }
    }

    #[test]
    fn anthropic_retry_policy_adds_only_overload() {
        let anthropic = registry::retry_capabilities(
            "anthropic",
            registry::ProviderTransport::AnthropicMessages,
        );
        let common = common_retry_capabilities();
        assert!(anthropic.retries_status(StatusCode::from_u16(529).unwrap()));
        assert!(!common.retries_status(StatusCode::from_u16(529).unwrap()));
        assert!(!anthropic.retries_status(StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn authentication_and_semantic_http_failures_are_not_retried() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::BAD_REQUEST] {
            let attempts = Arc::new(AtomicUsize::new(0));
            let observed = attempts.clone();
            let outcome = send_with_retry_using(
                move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok::<StatusCode, String>(status))
                },
                2,
                RetryPolicy {
                    max_attempts: 3,
                    base_delay: Duration::ZERO,
                    request_timeout: Duration::from_secs(1),
                },
                |status| *status,
                common_retry_capabilities(),
                |_| None,
                |_| false,
            )
            .await
            .expect("non-retryable response reaches the adapter");
            assert_eq!(outcome.value, status);
            assert_eq!(outcome.attempts.attempts(), 1);
            assert!(!outcome.attempts.retry_exhausted());
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
        }

        let client = reqwest::Client::new();
        let missing = send_with_retry(
            |_| client.get("http://127.0.0.1:9"),
            0,
            common_retry_capabilities(),
        )
        .await
        .expect_err("missing credentials fail before I/O");
        assert_eq!(missing.attempts.attempts(), 0);
        assert_eq!(
            missing.attempts.error_disposition(),
            RetryDisposition::NotAttempted
        );
    }

    #[test]
    fn retry_after_seconds_and_dates_are_strict_and_bounded() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-23T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            parse_retry_after_value("12", now),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after_value("Sun, 23 Aug 2026 00:00:20 +0000", now),
            Some(Duration::from_secs(20))
        );
        for invalid in [
            "invalid",
            "-1",
            "31",
            "Sat, 22 Aug 2026 23:59:59 +0000",
            "Sun, 23 Aug 2026 00:00:31 +0000",
        ] {
            assert_eq!(parse_retry_after_value(invalid, now), None, "{invalid}");
        }
    }

    #[tokio::test]
    async fn abort_and_drop_during_backoff_start_no_later_attempt() {
        fn pending_retry(
            attempts: Arc<AtomicUsize>,
        ) -> impl Future<Output = Result<RetryOutcome<StatusCode>, RetryFailure<String>>> {
            send_with_retry_using(
                move |_| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok::<StatusCode, String>(StatusCode::SERVICE_UNAVAILABLE))
                },
                1,
                RetryPolicy {
                    max_attempts: 3,
                    base_delay: Duration::from_secs(60),
                    request_timeout: Duration::from_secs(1),
                },
                |status| *status,
                common_retry_capabilities(),
                |_| None,
                |_| false,
            )
        }

        let aborted_attempts = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(pending_retry(aborted_attempts.clone()));
        while aborted_attempts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(aborted_attempts.load(Ordering::SeqCst), 1);

        let dropped_attempts = Arc::new(AtomicUsize::new(0));
        let mut future = Box::pin(pending_retry(dropped_attempts.clone()));
        tokio::select! {
            result = &mut future => panic!("retry completed during backoff: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        drop(future);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(dropped_attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn response_and_event_byte_budgets_are_strict_at_the_boundary() {
        let mut budget = ResponseByteBudget::new(8, "fixture response");
        budget.account(3).expect("first chunk");
        budget.account(5).expect("exact boundary");
        assert!(budget.account(1).unwrap_err().contains("8-byte limit"));

        assert!(ensure_stream_event_size(MAX_LLM_STREAM_EVENT_BYTES, "fixture event").is_ok());
        assert!(
            ensure_stream_event_size(MAX_LLM_STREAM_EVENT_BYTES + 1, "fixture event")
                .unwrap_err()
                .contains("event limit")
        );
    }

    #[tokio::test]
    async fn stream_producer_helpers_treat_receiver_drop_as_terminal() {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        assert!(
            !send_stream_event(
                &sender,
                Ok(LlmStreamEvent::Delta(LlmDelta::Content(
                    "ignored".to_string()
                )))
            )
            .await
        );

        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let producer = tokio::spawn(async move {
            let mut pending = futures_util::stream::pending::<Result<(), String>>();
            next_stream_item_or_closed(&sender, &mut pending)
                .await
                .is_none()
        });
        tokio::task::yield_now().await;
        drop(receiver);

        assert!(timeout(Duration::from_secs(1), producer)
            .await
            .expect("producer observes receiver closure")
            .expect("producer task"));
    }

    #[tokio::test]
    async fn projected_tool_deltas_cannot_override_private_completion() {
        let (public_tx, public_rx) = mpsc::channel(4);
        public_tx
            .send(Ok(LlmStreamEvent::Delta(LlmDelta::ToolCallChunk {
                index: 0,
                name: Some("untrusted_public_call".to_string()),
                arguments: Some("{}".to_string()),
            })))
            .await
            .unwrap();
        public_tx
            .send(Ok(LlmStreamEvent::Terminal(LlmFinishReason::ToolCalls)))
            .await
            .unwrap();
        drop(public_tx);
        let (completion_tx, completion_rx) = oneshot::channel();
        completion_tx
            .send(Ok(LlmResponse::text("validated terminal response")))
            .unwrap();

        let mut stream = LlmStream::with_private_completion(public_rx, completion_rx);
        while stream.recv_private().await.is_some() {}
        let debug = format!("{stream:?}");
        let completed = stream.finish().await.unwrap();

        assert_eq!(
            completed.content.as_deref(),
            Some("validated terminal response")
        );
        assert!(completed.tool_calls.is_none());
        assert!(!debug.contains("untrusted_public_call"));
    }

    #[test]
    fn stream_debug_omits_context_finish_and_protocol_values() {
        let active = "active/credential-123".to_string();
        let inactive = "inactive/credential-456".to_string();
        let variants = [
            active.as_str(),
            inactive.as_str(),
            "active%2Fcredential-123",
            r#"inactive\/credential-456"#,
            "YWN0aXZlL2NyZWRlbnRpYWwtMTIz",
            "aW5hY3RpdmUvY3JlZGVudGlhbC00NTY=",
            "control\u{1b}sentinel",
        ];
        let joined = variants.join("|");
        let (_sender, receiver) = mpsc::channel(1);
        let mut stream = LlmStream::from_public_receiver(receiver).with_error_context(
            error::LlmErrorContext::new(
                format!("provider-{joined}"),
                format!("transport-{joined}"),
                Some(format!("model-{joined}")),
                vec![active.clone(), inactive.clone()],
                RetryAttemptMetadata::no_network_attempt(),
            ),
        );
        stream.finish_reason = Some(LlmFinishReason::Complete);
        let failure = LlmStreamFailure::Protocol(joined);

        for debug in [format!("{stream:?}"), format!("{failure:?}")] {
            assert!(debug.len() < 1_024);
            assert!(!debug.contains('\u{1b}'));
            for variant in variants {
                assert!(!debug.contains(variant), "Debug leaked {variant}: {debug}");
            }
        }
    }
}
