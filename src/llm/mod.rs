//! Pluggable LLM providers (HTTP) and mock for CI.

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde_json::Value;
use std::future::Future;
use std::time::Duration;

pub mod anthropic;
pub mod conformance;
pub mod error;
pub mod factory;
pub mod gemini;
pub mod mock;
pub mod openai;
pub mod registry;
pub mod responses;
pub mod types;

pub use error::{LlmError, LlmErrorClass, LlmErrorMetadata, LlmErrorPhase, RetryDisposition};
pub use factory::{create_client, provider_ready};
pub use mock::MockLlmClient;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, oneshot};
pub use types::{
    GenerationOptions, LlmMessage, LlmMessageRole, LlmRequest, LlmRequestScope, LlmResponse,
    LlmTerminalStatus, ProviderCallId, ProviderContinuation, ReasoningOption, StreamEvent,
    ToolCallAccumulator, ToolCallRequest, ToolDefinition,
};

pub(crate) const MAX_LLM_COMPLETE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_LLM_ERROR_RESPONSE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LLM_STREAM_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_LLM_STREAM_EVENT_BYTES: usize = 1024 * 1024;

pub(crate) struct ResponseByteBudget {
    consumed: usize,
    limit: usize,
    label: &'static str,
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
    sender: &Sender<Result<StreamEvent, LlmStreamFailure>>,
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
    sender: &Sender<Result<StreamEvent, LlmStreamFailure>>,
    event: Result<StreamEvent, LlmStreamFailure>,
) -> bool {
    sender.send(event).await.is_ok()
}

#[derive(Clone, Debug)]
pub(crate) enum LlmStreamFailure {
    Protocol(String),
    Typed(LlmError),
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
    receiver: Receiver<Result<StreamEvent, LlmStreamFailure>>,
    private_completion: Option<oneshot::Receiver<Result<LlmResponse, LlmStreamFailure>>>,
    error_context: Option<error::LlmErrorContext>,
    content: String,
    tool_calls: ToolCallAccumulator,
    finish_reason: Option<String>,
    stream_error: Option<LlmError>,
    exhausted: bool,
}

impl std::fmt::Debug for LlmStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmStream")
            .field("private_completion", &self.private_completion.is_some())
            .field("error_context", &self.error_context)
            .field("content_bytes", &self.content.len())
            .field("finish_reason", &self.finish_reason)
            .field("has_error", &self.stream_error.is_some())
            .field("exhausted", &self.exhausted)
            .finish_non_exhaustive()
    }
}

impl LlmStream {
    pub(crate) fn from_public_receiver(
        receiver: Receiver<Result<StreamEvent, LlmStreamFailure>>,
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
        receiver: Receiver<Result<StreamEvent, LlmStreamFailure>>,
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
        let finish_reason = response.finish_reason.clone();
        tokio::spawn(async move {
            if let Some(content) = content {
                if !send_stream_event(&tx, Ok(StreamEvent::Content(content))).await {
                    return;
                }
            }
            for (index, call) in calls.into_iter().enumerate() {
                if !send_stream_event(
                    &tx,
                    Ok(StreamEvent::ToolCallChunk {
                        index,
                        name: Some(call.name),
                        arguments: Some(call.arguments.to_string()),
                    }),
                )
                .await
                {
                    return;
                }
            }
            if !send_stream_event(&tx, Ok(StreamEvent::End(finish_reason))).await {
                return;
            }
            let _ = completion_tx.send(Ok(response));
        });
        Self::with_private_completion(rx, completion_rx)
    }

    pub async fn recv(&mut self) -> Option<Result<StreamEvent, Box<LlmError>>> {
        let next = self
            .receiver
            .recv()
            .await
            .map(|result| result.map_err(|error| Box::new(self.stream_failure(error))));
        match &next {
            Some(Ok(event)) => {
                self.tool_calls.push(event);
                match event {
                    StreamEvent::Content(fragment) => self.content.push_str(fragment),
                    StreamEvent::End(reason) => self.finish_reason = Some(reason.clone()),
                    _ => {}
                }
            }
            Some(Err(error)) => {
                if self.stream_error.is_none() {
                    self.stream_error = Some((**error).clone());
                }
            }
            None => self.exhausted = true,
        }
        next
    }

    pub async fn finish(mut self) -> Result<LlmResponse, LlmError> {
        while !self.exhausted {
            if self.recv().await.is_none() {
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
        Ok(LlmResponse {
            terminal_status: LlmTerminalStatus::Completed,
            content: (!self.content.trim().is_empty()).then_some(self.content),
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            finish_reason,
            continuation: None,
        })
    }

    fn stream_failure(&self, failure: LlmStreamFailure) -> LlmError {
        match failure {
            LlmStreamFailure::Typed(error) => error,
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

pub fn is_transient_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

pub fn is_anthropic_transient_status(status: StatusCode) -> bool {
    is_transient_status(status) || status.as_u16() == 529
}

const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Sends a provider request with bounded retry and credential rotation.
///
/// The request factory receives the credential index to use, allowing every
/// provider to rotate through its configured key pool after HTTP 429.
pub async fn send_with_retry<F>(
    request_factory: F,
    credential_count: usize,
) -> Result<Response, String>
where
    F: FnMut(usize) -> RequestBuilder,
{
    send_with_retry_for(request_factory, credential_count, is_transient_status).await
}

pub async fn send_with_retry_for<F, RetryStatus>(
    mut request_factory: F,
    credential_count: usize,
    retry_status: RetryStatus,
) -> Result<Response, String>
where
    F: FnMut(usize) -> RequestBuilder,
    RetryStatus: Fn(StatusCode) -> bool,
{
    if credential_count == 0 {
        return Err("provider request requires at least one credential".to_string());
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
        retry_status,
        retry_after_delay,
        |error: &reqwest::Error| error.is_timeout() || error.is_connect(),
    )
    .await
}

fn retry_after_delay(response: &Response) -> Option<Duration> {
    if !matches!(response.status().as_u16(), 429 | 503 | 529) {
        return None;
    }
    let value = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        let delay = Duration::from_secs(seconds);
        return (delay <= MAX_RETRY_AFTER).then_some(delay);
    }

    let retry_at = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&chrono::Utc);
    let delay = retry_at.signed_duration_since(chrono::Utc::now());
    let milliseconds = delay.num_milliseconds();
    if milliseconds <= 0 {
        return None;
    }
    let delay = Duration::from_millis(milliseconds as u64);
    (delay <= MAX_RETRY_AFTER).then_some(delay)
}

async fn send_with_retry_using<T, E, F, Fut, Status, RetryStatus, RetryAfter, RetryError>(
    mut send: F,
    credential_count: usize,
    policy: RetryPolicy,
    status: Status,
    retry_status: RetryStatus,
    retry_after: RetryAfter,
    retry_error: RetryError,
) -> Result<T, String>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<T, E>>,
    Status: Fn(&T) -> StatusCode,
    RetryStatus: Fn(StatusCode) -> bool,
    RetryAfter: Fn(&T) -> Option<Duration>,
    RetryError: Fn(&E) -> bool,
    E: ToString,
{
    if credential_count == 0 {
        return Err("provider retry requires at least one credential".to_string());
    }
    let attempts = policy.max_attempts.max(1);
    let mut credential_index = 0;

    for attempt in 0..attempts {
        match send(credential_index).await {
            Ok(response) if retry_status(status(&response)) => {
                if attempt + 1 == attempts {
                    return Ok(response);
                }
                let response_status = status(&response);
                let delay = retry_after(&response).unwrap_or_else(|| {
                    let multiplier = 1u32 << attempt.min(6);
                    policy
                        .base_delay
                        .saturating_mul(multiplier)
                        .min(MAX_RETRY_AFTER)
                });
                if response_status == StatusCode::TOO_MANY_REQUESTS {
                    credential_index = (credential_index + 1) % credential_count;
                }
                tokio::time::sleep(delay).await;
            }
            Ok(response) => return Ok(response),
            Err(error) if retry_error(&error) && attempt + 1 < attempts => {
                let multiplier = 1u32 << attempt.min(6);
                tokio::time::sleep(
                    policy
                        .base_delay
                        .saturating_mul(multiplier)
                        .min(MAX_RETRY_AFTER),
                )
                .await;
            }
            Err(error) => return Err(error.to_string()),
        }
    }

    Err("provider retry loop ended without a response".to_string())
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, LlmError>;

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        let response = self.complete(request).await?;
        Ok(LlmStream::from_response(response))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    pub fn serve_once(
        status: &str,
        content_type: &str,
        body: impl Into<String>,
    ) -> (String, Receiver<String>) {
        serve_once_with_declared_length(status, content_type, body, None)
    }

    pub fn serve_once_with_declared_length(
        status: &str,
        content_type: &str,
        body: impl Into<String>,
        declared_length: Option<usize>,
    ) -> (String, Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test HTTP listener");
        let address = listener.local_addr().expect("test HTTP address");
        let status = status.to_string();
        let content_type = content_type.to_string();
        let body = body.into();
        let (request_tx, request_rx) = mpsc::channel();

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test HTTP connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read timeout");
            let request = read_request(&mut stream);
            let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                declared_length.unwrap_or(body.len())
            );
            stream
                .write_all(response.as_bytes())
                .expect("test HTTP response");
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
    use tokio::time::timeout;

    #[test]
    fn retry_statuses_are_explicit_and_bounded() {
        for code in [408, 425, 429, 500, 502, 503, 504] {
            assert!(is_transient_status(StatusCode::from_u16(code).unwrap()));
        }
        assert!(!is_transient_status(StatusCode::BAD_REQUEST));
        assert!(!is_transient_status(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn retry_policy_has_finite_defaults() {
        let policy = RetryPolicy::default();
        assert!((2..=5).contains(&policy.max_attempts));
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
            is_transient_status,
            |_| None,
            |_| false,
        )
        .await
        .expect("backup credential succeeds");
        assert_eq!(response, StatusCode::OK);
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
                max_attempts: 3,
                base_delay: Duration::ZERO,
                request_timeout: Duration::from_secs(1),
            },
            |status| *status,
            is_transient_status,
            |_| None,
            |_| false,
        )
        .await
        .expect("last transient response is returned to the adapter");
        assert_eq!(response, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(credential_indices, [0, 0, 0]);
        assert!(responses.is_empty());
    }

    #[test]
    fn anthropic_retry_policy_adds_only_overload() {
        assert!(is_anthropic_transient_status(
            StatusCode::from_u16(529).unwrap()
        ));
        assert!(!is_transient_status(StatusCode::from_u16(529).unwrap()));
        assert!(!is_anthropic_transient_status(StatusCode::BAD_REQUEST));
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
        assert!(!send_stream_event(&sender, Ok(StreamEvent::Content("ignored".to_string()))).await);

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
            .send(Ok(StreamEvent::ToolCallChunk {
                index: 0,
                name: Some("untrusted_public_call".to_string()),
                arguments: Some("{}".to_string()),
            }))
            .await
            .unwrap();
        public_tx
            .send(Ok(StreamEvent::End("tool_calls".to_string())))
            .await
            .unwrap();
        drop(public_tx);
        let (completion_tx, completion_rx) = oneshot::channel();
        completion_tx
            .send(Ok(LlmResponse::text("validated terminal response")))
            .unwrap();

        let mut stream = LlmStream::with_private_completion(public_rx, completion_rx);
        while stream.recv().await.is_some() {}
        let debug = format!("{stream:?}");
        let completed = stream.finish().await.unwrap();

        assert_eq!(
            completed.content.as_deref(),
            Some("validated terminal response")
        );
        assert!(completed.tool_calls.is_none());
        assert!(!debug.contains("untrusted_public_call"));
    }
}
