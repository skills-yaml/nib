use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

const MAX_PROVIDER_BYTES: usize = 64;
const MAX_TRANSPORT_BYTES: usize = 64;
const MAX_MODEL_BYTES: usize = 512;
const MAX_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;

#[derive(Clone)]
pub(crate) struct LlmErrorContext {
    provider: String,
    transport: String,
    model: Option<String>,
    sensitive_values: Vec<String>,
}

impl fmt::Debug for LlmErrorContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmErrorContext")
            .field("provider", &self.provider)
            .field("transport", &self.transport)
            .field("model", &self.model)
            .field("sensitive_value_count", &self.sensitive_values.len())
            .finish()
    }
}

impl LlmErrorContext {
    pub(crate) fn new(
        provider: impl Into<String>,
        transport: impl Into<String>,
        model: Option<String>,
        sensitive_values: Vec<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            transport: transport.into(),
            model,
            sensitive_values,
        }
    }

    pub(crate) fn protocol(&self, phase: LlmErrorPhase, message: impl AsRef<str>) -> LlmError {
        LlmError::provider_protocol(
            &self.provider,
            &self.transport,
            self.model.as_deref(),
            phase,
            message,
            &self.sensitive_values,
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorClass {
    Configuration,
    Authentication,
    RateLimited,
    QuotaOrBilling,
    ModelUnavailable,
    UnsupportedRequest,
    ProviderUnavailable,
    Transport,
    Protocol,
    ProviderRejected,
    Cancelled,
}

impl LlmErrorClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Authentication => "authentication",
            Self::RateLimited => "rate limited",
            Self::QuotaOrBilling => "quota or billing",
            Self::ModelUnavailable => "model unavailable",
            Self::UnsupportedRequest => "unsupported request",
            Self::ProviderUnavailable => "provider unavailable",
            Self::Transport => "transport",
            Self::Protocol => "protocol",
            Self::ProviderRejected => "provider rejected",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorPhase {
    Configuration,
    Request,
    Connect,
    HttpResponse,
    Stream,
    TerminalValidation,
    Continuation,
    Planning,
    Compression,
}

impl LlmErrorPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Request => "request construction",
            Self::Connect => "connection",
            Self::HttpResponse => "HTTP response",
            Self::Stream => "stream",
            Self::TerminalValidation => "terminal validation",
            Self::Continuation => "continuation",
            Self::Planning => "planning",
            Self::Compression => "compression",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryDisposition {
    NotAttempted,
    NotRetryable,
    Exhausted,
    Cancelled,
}

impl RetryDisposition {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotAttempted => "not attempted",
            Self::NotRetryable => "not retryable",
            Self::Exhausted => "exhausted",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Provider-neutral, redaction-safe LLM failure evidence.
///
/// `safe_message` exists for compatibility with lower-level diagnostics and is already
/// bounded, escaped, and redacted. User-facing surfaces should use `user_report` rather
/// than presenting the compatibility message directly.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmError {
    pub class: LlmErrorClass,
    pub phase: LlmErrorPhase,
    pub retry: RetryDisposition,
    pub provider: String,
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, rename = "incident_code")]
    code: String,
    #[serde(default, rename = "action")]
    operator_action: String,
    #[serde(skip, default)]
    safe_message: String,
}

impl fmt::Debug for LlmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmError")
            .field("class", &self.class)
            .field("phase", &self.phase)
            .field("retry", &self.retry)
            .field("provider", &self.provider)
            .field("transport", &self.transport)
            .field("model", &self.model)
            .field("http_status", &self.http_status)
            .field("retry_after_seconds", &self.retry_after_seconds)
            .field("request_id", &self.request_id)
            .field("incident_code", &self.incident_code())
            .field("safe_message_bytes", &self.safe_message.len())
            .finish()
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_message)
    }
}

impl std::error::Error for LlmError {}

pub trait LlmErrorPattern {
    fn occurs_in(self, value: &str) -> bool;
}

impl LlmErrorPattern for &str {
    fn occurs_in(self, value: &str) -> bool {
        value.contains(self)
    }
}

impl LlmErrorPattern for &String {
    fn occurs_in(self, value: &str) -> bool {
        value.contains(self.as_str())
    }
}

impl LlmErrorPattern for char {
    fn occurs_in(self, value: &str) -> bool {
        value.contains(self)
    }
}

/// Request metadata sanitized into a provider-neutral LLM failure.
#[derive(Debug, Clone, Copy)]
pub struct LlmErrorMetadata<'a> {
    provider: &'a str,
    transport: &'a str,
    model: Option<&'a str>,
    http_status: Option<u16>,
    sensitive_values: &'a [String],
}

impl<'a> LlmErrorMetadata<'a> {
    pub fn new(
        provider: &'a str,
        transport: &'a str,
        model: Option<&'a str>,
        http_status: Option<u16>,
        sensitive_values: &'a [String],
    ) -> Self {
        Self {
            provider,
            transport,
            model,
            http_status,
            sensitive_values,
        }
    }
}

impl PartialEq<&str> for LlmError {
    fn eq(&self, other: &&str) -> bool {
        self.safe_message == *other
    }
}

impl PartialEq<String> for LlmError {
    fn eq(&self, other: &String) -> bool {
        self.safe_message == *other
    }
}

impl From<String> for LlmError {
    fn from(message: String) -> Self {
        Self::local(
            LlmErrorClass::Protocol,
            LlmErrorPhase::TerminalValidation,
            message,
        )
    }
}

impl From<&str> for LlmError {
    fn from(message: &str) -> Self {
        Self::from(message.to_string())
    }
}

impl LlmError {
    pub fn contains(&self, pattern: impl LlmErrorPattern) -> bool {
        pattern.occurs_in(&self.safe_message)
    }

    pub fn len(&self) -> usize {
        self.safe_message.len()
    }

    pub fn is_empty(&self) -> bool {
        self.safe_message.is_empty()
    }

    pub fn new(
        class: LlmErrorClass,
        phase: LlmErrorPhase,
        retry: RetryDisposition,
        metadata: LlmErrorMetadata<'_>,
        safe_message: impl AsRef<str>,
    ) -> Self {
        Self {
            class,
            phase,
            retry,
            provider: sanitize(
                metadata.provider,
                metadata.sensitive_values,
                MAX_PROVIDER_BYTES,
            ),
            transport: sanitize(
                metadata.transport,
                metadata.sensitive_values,
                MAX_TRANSPORT_BYTES,
            ),
            model: metadata
                .model
                .map(|value| sanitize(value, metadata.sensitive_values, MAX_MODEL_BYTES)),
            http_status: metadata.http_status,
            retry_after_seconds: None,
            request_id: None,
            code: incident_code_for(class).to_string(),
            operator_action: action_for(class).to_string(),
            safe_message: sanitize(
                safe_message.as_ref(),
                metadata.sensitive_values,
                MAX_MESSAGE_BYTES,
            ),
        }
    }

    pub fn local(
        class: LlmErrorClass,
        phase: LlmErrorPhase,
        safe_message: impl AsRef<str>,
    ) -> Self {
        Self::new(
            class,
            phase,
            RetryDisposition::NotRetryable,
            LlmErrorMetadata::new("nib", "local", None, None, &[]),
            safe_message,
        )
    }

    pub fn configuration(safe_message: impl AsRef<str>) -> Self {
        Self::new(
            LlmErrorClass::Configuration,
            LlmErrorPhase::Configuration,
            RetryDisposition::NotAttempted,
            LlmErrorMetadata::new("nib", "local", None, None, &[]),
            safe_message,
        )
    }

    pub fn cancelled(provider: &str, transport: &str, model: Option<&str>) -> Self {
        Self::new(
            LlmErrorClass::Cancelled,
            LlmErrorPhase::Request,
            RetryDisposition::Cancelled,
            LlmErrorMetadata::new(provider, transport, model, None, &[]),
            "LLM request was cancelled locally",
        )
    }

    pub fn provider_protocol(
        provider: &str,
        transport: &str,
        model: Option<&str>,
        phase: LlmErrorPhase,
        safe_message: impl AsRef<str>,
        sensitive_values: &[String],
    ) -> Self {
        Self::new(
            LlmErrorClass::Protocol,
            phase,
            RetryDisposition::NotRetryable,
            LlmErrorMetadata::new(provider, transport, model, None, sensitive_values),
            safe_message,
        )
    }

    pub fn transport(
        provider: &str,
        transport: &str,
        model: Option<&str>,
        safe_message: impl AsRef<str>,
        sensitive_values: &[String],
    ) -> Self {
        Self::new(
            LlmErrorClass::Transport,
            LlmErrorPhase::Connect,
            RetryDisposition::Exhausted,
            LlmErrorMetadata::new(provider, transport, model, None, sensitive_values),
            safe_message,
        )
    }

    pub fn request_rejected(
        provider: &str,
        transport: &str,
        model: Option<&str>,
        safe_message: impl AsRef<str>,
        sensitive_values: &[String],
    ) -> Self {
        Self::new(
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::Request,
            RetryDisposition::NotAttempted,
            LlmErrorMetadata::new(provider, transport, model, None, sensitive_values),
            safe_message,
        )
    }

    pub fn http(
        provider: &str,
        transport: &str,
        model: Option<&str>,
        status: StatusCode,
        structured_error: Option<&Value>,
        safe_message: impl AsRef<str>,
        sensitive_values: &[String],
    ) -> Self {
        let structural_code = structural_code(structured_error);
        let class = classify_http(provider, status, structural_code);
        let retry = if is_retryable_status(provider, status) {
            RetryDisposition::Exhausted
        } else {
            RetryDisposition::NotRetryable
        };
        Self::new(
            class,
            LlmErrorPhase::HttpResponse,
            retry,
            LlmErrorMetadata::new(
                provider,
                transport,
                model,
                Some(status.as_u16()),
                sensitive_values,
            ),
            safe_message,
        )
    }

    pub fn provider_rejected(
        provider: &str,
        transport: &str,
        model: Option<&str>,
        phase: LlmErrorPhase,
        structured_error: Option<&Value>,
        safe_message: impl AsRef<str>,
        sensitive_values: &[String],
    ) -> Self {
        let structural_code = structural_code(structured_error);
        let class = classify_structural_code(provider, structural_code)
            .unwrap_or(LlmErrorClass::ProviderRejected);
        Self::new(
            class,
            phase,
            RetryDisposition::NotRetryable,
            LlmErrorMetadata::new(provider, transport, model, None, sensitive_values),
            safe_message,
        )
    }

    pub fn redacted_with(&self, sensitive_values: &[String]) -> Self {
        let mut redacted = Self::new(
            self.class,
            self.phase,
            self.retry,
            LlmErrorMetadata::new(
                &self.provider,
                &self.transport,
                self.model.as_deref(),
                self.http_status,
                sensitive_values,
            ),
            &self.safe_message,
        );
        redacted.retry_after_seconds = self.retry_after_seconds.filter(|value| *value <= 30);
        redacted.request_id = self
            .request_id
            .as_deref()
            .and_then(|request_id| validated_request_id(request_id, sensitive_values));
        redacted
    }

    pub fn with_retry_after_seconds(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = (seconds <= 30).then_some(seconds);
        self
    }

    pub fn with_phase(mut self, phase: LlmErrorPhase) -> Self {
        self.phase = phase;
        self
    }

    pub fn with_request_id(mut self, request_id: &str, sensitive_values: &[String]) -> Self {
        self.request_id = validated_request_id(request_id, sensitive_values);
        self
    }

    pub fn incident_code(&self) -> &'static str {
        let code = incident_code_for(self.class);
        debug_assert!(self.code.is_empty() || self.code == code);
        code
    }

    pub fn action(&self) -> &'static str {
        let action = action_for(self.class);
        debug_assert!(self.operator_action.is_empty() || self.operator_action == action);
        action
    }

    pub fn user_report(&self, session_id: Option<&str>) -> String {
        let mut report = format!("LLM request failed [{}]\n", self.incident_code());
        report.push_str(&format!(
            "Cause: {} during {}\n",
            self.class.label(),
            self.phase.label()
        ));
        let provider = sanitize(&self.provider, &[], MAX_PROVIDER_BYTES);
        let transport = sanitize(&self.transport, &[], MAX_TRANSPORT_BYTES);
        report.push_str(&format!("Provider: {provider} ({transport})"));
        let model = self
            .model
            .as_deref()
            .map(|model| sanitize(model, &[], MAX_MODEL_BYTES));
        if let Some(model) = model.as_deref().filter(|model| !model.is_empty()) {
            report.push_str(&format!(", model: {model}"));
        }
        report.push('\n');
        if let Some(status) = self.http_status {
            report.push_str(&format!("HTTP: {status}; retry: {}\n", self.retry.label()));
        } else {
            report.push_str(&format!("Retry: {}\n", self.retry.label()));
        }
        if let Some(seconds) = self.retry_after_seconds.filter(|seconds| *seconds <= 30) {
            report.push_str(&format!("Retry after: {seconds}s\n"));
        }
        if let Some(request_id) = self
            .request_id
            .as_deref()
            .and_then(|request_id| validated_request_id(request_id, &[]))
        {
            report.push_str(&format!("Request ID: {request_id}\n"));
        }
        report.push_str(&format!("Action: {}", self.action()));
        if let Some(session_id) = session_id {
            report.push_str(&format!("\nSession: {}", sanitize(session_id, &[], 256)));
        }
        truncate_utf8(&report, MAX_MESSAGE_BYTES)
    }
}

fn structural_code(value: Option<&Value>) -> Option<&str> {
    let value = value?;
    [
        "/response/error/code",
        "/response/error/type",
        "/response/error/status",
        "/error/code",
        "/error/type",
        "/error/status",
        "/code",
        "/type",
        "/status",
    ]
    .into_iter()
    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_str))
}

fn incident_code_for(class: LlmErrorClass) -> &'static str {
    match class {
        LlmErrorClass::Configuration => "LLM-CONFIG",
        LlmErrorClass::Authentication => "LLM-AUTH",
        LlmErrorClass::RateLimited => "LLM-RATE",
        LlmErrorClass::QuotaOrBilling => "LLM-QUOTA",
        LlmErrorClass::ModelUnavailable => "LLM-MODEL",
        LlmErrorClass::UnsupportedRequest => "LLM-REQUEST",
        LlmErrorClass::ProviderUnavailable => "LLM-UNAVAILABLE",
        LlmErrorClass::Transport => "LLM-TRANSPORT",
        LlmErrorClass::Protocol => "LLM-PROTOCOL",
        LlmErrorClass::ProviderRejected => "LLM-REJECTED",
        LlmErrorClass::Cancelled => "LLM-CANCELLED",
    }
}

fn action_for(class: LlmErrorClass) -> &'static str {
    match class {
        LlmErrorClass::Configuration => {
            "Run `nib config validate` or complete `nib auth`, then retry."
        }
        LlmErrorClass::Authentication => {
            "Refresh this provider's credential with `nib auth`, then retry."
        }
        LlmErrorClass::RateLimited => "Retry after the reported delay or retry later.",
        LlmErrorClass::QuotaOrBilling => {
            "Check this provider account's quota and billing controls, then retry."
        }
        LlmErrorClass::ModelUnavailable => {
            "Verify the configured model with `/model` or provider configuration."
        }
        LlmErrorClass::UnsupportedRequest => {
            "Run `nib doctor` and verify the selected transport, reasoning, and tool settings."
        }
        LlmErrorClass::ProviderUnavailable => {
            "The bounded retry budget was exhausted; retry later."
        }
        LlmErrorClass::Transport => {
            "Check network reachability and the configured endpoint, then retry."
        }
        LlmErrorClass::Protocol => {
            "Run `nib doctor` and verify provider compatibility before retrying."
        }
        LlmErrorClass::ProviderRejected => {
            "Run `nib doctor`, verify the provider configuration, and retry."
        }
        LlmErrorClass::Cancelled => "Start a new turn when you are ready.",
    }
}

fn classify_http(provider: &str, status: StatusCode, code: Option<&str>) -> LlmErrorClass {
    if let Some(class) = classify_structural_code(provider, code) {
        return class;
    }
    match status.as_u16() {
        401 | 403 => LlmErrorClass::Authentication,
        429 => LlmErrorClass::RateLimited,
        400 | 422 => LlmErrorClass::UnsupportedRequest,
        408 | 425 | 500 | 502 | 503 | 504 | 529 => LlmErrorClass::ProviderUnavailable,
        _ => LlmErrorClass::ProviderRejected,
    }
}

fn classify_structural_code(provider: &str, code: Option<&str>) -> Option<LlmErrorClass> {
    let code = code?;
    let class = match code {
        "invalid_api_key" | "authentication_error" | "UNAUTHENTICATED" => {
            LlmErrorClass::Authentication
        }
        "rate_limit_exceeded" | "rate_limit_error" => LlmErrorClass::RateLimited,
        "insufficient_quota" | "billing_hard_limit_reached" => LlmErrorClass::QuotaOrBilling,
        "model_not_found" => LlmErrorClass::ModelUnavailable,
        "invalid_request_error"
        | "invalid_request"
        | "unsupported_parameter"
        | "INVALID_ARGUMENT" => LlmErrorClass::UnsupportedRequest,
        "server_error" | "overloaded_error" | "UNAVAILABLE" | "DEADLINE_EXCEEDED" => {
            LlmErrorClass::ProviderUnavailable
        }
        "RESOURCE_EXHAUSTED" if provider == "google" => LlmErrorClass::RateLimited,
        _ => return None,
    };
    Some(class)
}

fn is_retryable_status(provider: &str, status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
        || (provider == "anthropic" && status.as_u16() == 529)
}

fn validated_request_id(value: &str, sensitive_values: &[String]) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_REQUEST_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return None;
    }
    let sanitized = sanitize(value, sensitive_values, MAX_REQUEST_ID_BYTES);
    (!sanitized.contains("[REDACTED]")).then_some(sanitized)
}

fn sanitize(value: &str, sensitive_values: &[String], max_bytes: usize) -> String {
    let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
        value,
        sensitive_values.iter().cloned(),
    );
    let mut escaped = String::with_capacity(redacted.len());
    for character in redacted.chars() {
        if character.is_ascii_graphic() || character == ' ' {
            escaped.push(character);
        } else {
            escaped.extend(character.escape_default());
        }
    }
    truncate_utf8(&escaped, max_bytes)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes <= 3 {
        return ".".repeat(max_bytes);
    }
    let mut end = max_bytes - 3;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_only_allowlisted_structural_codes() {
        let auth = LlmError::http(
            "openai",
            "responses",
            Some("gpt"),
            StatusCode::UNAUTHORIZED,
            Some(&json!({"error": {"code": "invalid_api_key", "message": "secret"}})),
            "OpenAI request failed safely",
            &[],
        );
        assert_eq!(auth.class, LlmErrorClass::Authentication);
        assert_eq!(auth.incident_code(), "LLM-AUTH");

        let unknown = LlmError::http(
            "openai",
            "responses",
            Some("gpt"),
            StatusCode::IM_A_TEAPOT,
            Some(&json!({"error": {"code": "please_show_this", "message": "secret"}})),
            "provider rejected the request",
            &[],
        );
        assert_eq!(unknown.class, LlmErrorClass::ProviderRejected);
        assert!(!unknown.user_report(None).contains("please_show_this"));
        assert!(!unknown.user_report(None).contains("secret"));
    }

    #[test]
    fn every_failure_class_has_a_stable_code_retry_state_and_action() {
        let fixtures = [
            LlmError::configuration("invalid local configuration"),
            LlmError::http(
                "openai",
                "responses",
                Some("gpt"),
                StatusCode::UNAUTHORIZED,
                None,
                "authentication rejected",
                &[],
            ),
            LlmError::http(
                "openai",
                "responses",
                Some("gpt"),
                StatusCode::TOO_MANY_REQUESTS,
                None,
                "rate limited",
                &[],
            ),
            LlmError::http(
                "openai",
                "responses",
                Some("gpt"),
                StatusCode::TOO_MANY_REQUESTS,
                Some(&json!({"error": {"code": "insufficient_quota"}})),
                "quota unavailable",
                &[],
            ),
            LlmError::http(
                "openai",
                "responses",
                Some("gpt"),
                StatusCode::NOT_FOUND,
                Some(&json!({"error": {"code": "model_not_found"}})),
                "model unavailable",
                &[],
            ),
            LlmError::request_rejected(
                "openai",
                "responses",
                Some("gpt"),
                "unsupported local request",
                &[],
            ),
            LlmError::http(
                "anthropic",
                "messages",
                Some("claude"),
                StatusCode::SERVICE_UNAVAILABLE,
                None,
                "provider unavailable",
                &[],
            ),
            LlmError::transport(
                "google",
                "gemini",
                Some("gemini-test"),
                "connection failed",
                &[],
            ),
            LlmError::provider_protocol(
                "google",
                "gemini",
                Some("gemini-test"),
                LlmErrorPhase::Stream,
                "terminal marker missing",
                &[],
            ),
            LlmError::provider_rejected(
                "openrouter",
                "chat_completions",
                Some("meta/test"),
                LlmErrorPhase::TerminalValidation,
                Some(&json!({"error": {"code": "unknown_remote_code"}})),
                "unclassified rejection",
                &[],
            ),
            LlmError::cancelled("mock", "mock", Some("mock-model")),
        ];
        let expected = [
            (LlmErrorClass::Configuration, "LLM-CONFIG"),
            (LlmErrorClass::Authentication, "LLM-AUTH"),
            (LlmErrorClass::RateLimited, "LLM-RATE"),
            (LlmErrorClass::QuotaOrBilling, "LLM-QUOTA"),
            (LlmErrorClass::ModelUnavailable, "LLM-MODEL"),
            (LlmErrorClass::UnsupportedRequest, "LLM-REQUEST"),
            (LlmErrorClass::ProviderUnavailable, "LLM-UNAVAILABLE"),
            (LlmErrorClass::Transport, "LLM-TRANSPORT"),
            (LlmErrorClass::Protocol, "LLM-PROTOCOL"),
            (LlmErrorClass::ProviderRejected, "LLM-REJECTED"),
            (LlmErrorClass::Cancelled, "LLM-CANCELLED"),
        ];

        for (failure, (class, code)) in fixtures.into_iter().zip(expected) {
            assert_eq!(failure.class, class);
            assert_eq!(failure.incident_code(), code);
            let report = failure.user_report(None);
            assert!(report.starts_with(&format!("LLM request failed [{code}]")));
            assert!(report.contains(&format!("Cause: {} during ", class.label())));
            assert!(report.contains("Action: "));
        }
    }

    #[test]
    fn reports_are_bounded_control_safe_and_redacted() {
        let secret = "active-api-key".to_string();
        let error = LlmError::new(
            LlmErrorClass::Authentication,
            LlmErrorPhase::HttpResponse,
            RetryDisposition::NotRetryable,
            LlmErrorMetadata::new(
                &format!("openai\n{secret}"),
                "responses",
                Some(&format!("model-{secret}\u{1b}")),
                Some(401),
                std::slice::from_ref(&secret),
            ),
            format!("remote echoed {secret}"),
        );
        let report = error.user_report(Some("session\n1"));
        assert!(report.contains("LLM-AUTH"));
        assert!(report.contains("[REDACTED]"));
        assert!(!report.contains(&secret));
        assert!(!report.contains('\u{1b}'));
        assert!(report.contains('\n'));
        assert!(report.len() <= MAX_MESSAGE_BYTES);
        assert!(!format!("{error:?}").contains(&secret));
    }

    #[test]
    fn serialized_failures_omit_internal_diagnostic_text() {
        let error = LlmError::local(
            LlmErrorClass::Protocol,
            LlmErrorPhase::TerminalValidation,
            "internal-diagnostic-sentinel",
        );

        let serialized = serde_json::to_string(&error).expect("serialize LLM failure");
        assert!(!serialized.contains("internal-diagnostic-sentinel"));
        assert!(!serialized.contains("safe_message"));
        assert!(serialized.contains("\"incident_code\":\"LLM-PROTOCOL\""));
        assert!(serialized.contains("\"action\":\"Run `nib doctor`"));

        let restored: LlmError = serde_json::from_str(&serialized).expect("restore LLM failure");
        assert_eq!(restored.class, LlmErrorClass::Protocol);
        assert!(restored.user_report(None).contains("LLM-PROTOCOL"));
    }

    #[test]
    fn request_ids_require_a_strict_safe_shape() {
        let error = LlmError::local(
            LlmErrorClass::ProviderRejected,
            LlmErrorPhase::HttpResponse,
            "rejected",
        )
        .with_request_id("req_123-abc", &[]);
        assert_eq!(error.request_id.as_deref(), Some("req_123-abc"));
        assert!(error
            .clone()
            .with_request_id("request id with spaces", &[])
            .request_id
            .is_none());
    }
}
