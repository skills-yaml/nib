use reqwest::StatusCode;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fmt;

const MAX_PROVIDER_BYTES: usize = 64;
const MAX_TRANSPORT_BYTES: usize = 64;
const MAX_MODEL_BYTES: usize = 512;
const MAX_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;

/// Copies a response request-ID header only after bounding the remote value.
///
/// Provider adapters remain responsible for selecting their exact documented
/// header. The returned value must still pass through `LlmError::with_request_id`
/// for syntax validation and sensitive-value redaction.
pub(crate) fn bounded_request_id_header(
    value: Option<&reqwest::header::HeaderValue>,
) -> Option<String> {
    let value = value?;
    if value.as_bytes().len() > MAX_REQUEST_ID_BYTES {
        return None;
    }
    value.to_str().ok().map(str::to_owned)
}

#[derive(Clone)]
pub(crate) struct LlmErrorContext {
    provider: String,
    transport: String,
    model: Option<String>,
    sensitive_values: Vec<String>,
    retry_attempts: crate::llm::RetryAttemptMetadata,
}

impl fmt::Debug for LlmErrorContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmErrorContext")
            .field("provider_bytes", &self.provider.len())
            .field("transport_bytes", &self.transport.len())
            .field("has_model", &self.model.is_some())
            .field(
                "model_bytes",
                &self.model.as_ref().map_or(0, std::string::String::len),
            )
            .field("sensitive_value_count", &self.sensitive_values.len())
            .field("retry_attempts", &self.retry_attempts)
            .finish()
    }
}

impl LlmErrorContext {
    pub(crate) fn new(
        provider: impl Into<String>,
        transport: impl Into<String>,
        model: Option<String>,
        sensitive_values: Vec<String>,
        retry_attempts: crate::llm::RetryAttemptMetadata,
    ) -> Self {
        let provider = provider.into();
        let transport = transport.into();
        Self {
            provider: sanitize(&provider, &sensitive_values, MAX_PROVIDER_BYTES),
            transport: sanitize(&transport, &sensitive_values, MAX_TRANSPORT_BYTES),
            model: model.map(|model| sanitize(&model, &sensitive_values, MAX_MODEL_BYTES)),
            sensitive_values,
            retry_attempts,
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
        .with_retry_attempts(self.retry_attempts)
    }

    pub(crate) fn attach_retry_attempts(&self, error: LlmError) -> LlmError {
        error.with_retry_attempts(self.retry_attempts)
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
    ExhaustedAfterCredentialRotation,
    Cancelled,
}

/// A provider-owned, documented structural fact that is safe for local control flow.
///
/// This value is intentionally not serialized. A stored or externally supplied error
/// record cannot manufacture provider capability evidence; only the active adapter
/// that decoded the provider's documented envelope can attach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProviderErrorDiscriminator {
    DocumentedTransportIncompatibility,
}

impl RetryDisposition {
    pub fn label(self) -> &'static str {
        match self {
            Self::NotAttempted => "not attempted",
            Self::NotRetryable => "not retryable",
            Self::Exhausted => "exhausted",
            Self::ExhaustedAfterCredentialRotation => "exhausted after credential rotation",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Provider-neutral, redaction-safe LLM failure evidence.
///
/// `safe_message` exists for compatibility with lower-level diagnostics and is already
/// bounded, escaped, and redacted. User-facing surfaces should use `user_report` rather
/// than presenting the compatibility message directly.
#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct LlmError {
    pub class: LlmErrorClass,
    pub phase: LlmErrorPhase,
    pub retry: RetryDisposition,
    #[serde(default)]
    pub attempts: crate::llm::RetryAttemptMetadata,
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
    #[serde(skip, default)]
    provider_discriminator: Option<LlmProviderErrorDiscriminator>,
}

impl fmt::Debug for LlmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmError")
            .field("class", &self.class)
            .field("phase", &self.phase)
            .field("retry", &self.retry)
            .field("attempts", &self.attempts)
            .field("provider_bytes", &self.provider.len())
            .field("transport_bytes", &self.transport.len())
            .field("has_model", &self.model.is_some())
            .field(
                "model_bytes",
                &self.model.as_ref().map_or(0, std::string::String::len),
            )
            .field("http_status", &self.http_status)
            .field("retry_after_seconds", &self.retry_after_seconds)
            .field("has_request_id", &self.request_id.is_some())
            .field(
                "request_id_bytes",
                &self.request_id.as_ref().map_or(0, std::string::String::len),
            )
            .field("incident_code", &self.incident_code())
            .field("provider_discriminator", &self.provider_discriminator)
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
#[derive(Clone, Copy)]
pub struct LlmErrorMetadata<'a> {
    provider: &'a str,
    transport: &'a str,
    model: Option<&'a str>,
    http_status: Option<u16>,
    sensitive_values: &'a [String],
}

impl fmt::Debug for LlmErrorMetadata<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmErrorMetadata")
            .field("provider_bytes", &self.provider.len())
            .field("transport_bytes", &self.transport.len())
            .field("has_model", &self.model.is_some())
            .field("model_bytes", &self.model.map_or(0, str::len))
            .field("http_status", &self.http_status)
            .field("sensitive_value_count", &self.sensitive_values.len())
            .finish()
    }
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
            attempts: crate::llm::RetryAttemptMetadata::no_network_attempt(),
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
            provider_discriminator: None,
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
            RetryDisposition::NotAttempted,
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
        let class = classify_http(provider, transport, status, structured_error);
        Self::new(
            class,
            LlmErrorPhase::HttpResponse,
            RetryDisposition::NotAttempted,
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
        let class = classify_structural_error(provider, transport, structured_error)
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
        redacted.attempts = self.attempts;
        redacted.class = normalize_retry_backed_class(redacted.class, redacted.attempts);
        redacted.code = incident_code_for(redacted.class).to_string();
        redacted.operator_action = action_for(redacted.class).to_string();
        redacted.request_id = self
            .request_id
            .as_deref()
            .and_then(|request_id| validated_request_id(request_id, sensitive_values));
        redacted.provider_discriminator = self.provider_discriminator;
        redacted
    }

    pub fn with_retry_after_seconds(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = (seconds <= 30).then_some(seconds);
        self
    }

    pub fn with_retry_attempts(mut self, attempts: crate::llm::RetryAttemptMetadata) -> Self {
        self.class = normalize_retry_backed_class(self.class, attempts);
        self.code = incident_code_for(self.class).to_string();
        self.operator_action = action_for(self.class).to_string();
        self.retry = if self.class == LlmErrorClass::Cancelled {
            RetryDisposition::Cancelled
        } else {
            attempts.error_disposition()
        };
        self.retry_after_seconds = attempts.final_retry_after_seconds();
        self.attempts = attempts;
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

    /// Attaches provider-owned evidence that an exact documented structural response
    /// proves the selected transport is incompatible.
    ///
    /// Provider adapters must call this only after matching their own transport's
    /// documented envelope path and discriminator. Numeric HTTP status, a generic
    /// invalid-request class, or provider prose is not sufficient evidence.
    pub fn with_documented_transport_incompatibility(mut self) -> Self {
        self.provider_discriminator =
            Some(LlmProviderErrorDiscriminator::DocumentedTransportIncompatibility);
        self
    }

    pub fn provider_discriminator(&self) -> Option<LlmProviderErrorDiscriminator> {
        self.provider_discriminator
    }

    pub fn incident_code(&self) -> &'static str {
        let code = incident_code_for(self.class);
        debug_assert!(self.code.is_empty() || self.code == code);
        code
    }

    pub fn action(&self) -> &'static str {
        let action = action_for(normalize_retry_backed_class(self.class, self.attempts));
        debug_assert!(
            self.operator_action.is_empty() || self.operator_action == action_for(self.class)
        );
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

#[derive(Deserialize)]
struct LlmErrorWire {
    class: LlmErrorClass,
    phase: LlmErrorPhase,
    #[serde(rename = "retry")]
    _retry: RetryDisposition,
    #[serde(default)]
    attempts: crate::llm::RetryAttemptMetadata,
    provider: String,
    transport: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    http_status: Option<u16>,
    #[serde(default)]
    #[serde(rename = "retry_after_seconds")]
    _retry_after_seconds: Option<u64>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default, rename = "incident_code")]
    _incident_code: String,
    #[serde(default, rename = "action")]
    _operator_action: String,
}

impl<'de> Deserialize<'de> for LlmError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LlmErrorWire::deserialize(deserializer)?;
        let (provider, transport) = normalize_stored_context(&wire.provider, &wire.transport);
        let model = wire
            .model
            .as_deref()
            .filter(|model| !model.is_empty())
            .map(|_| "[stored model redacted]".to_string());
        let request_id = wire.request_id.as_deref().and_then(|request_id| {
            if matches!(
                (provider.as_str(), transport.as_str()),
                ("openai", "responses" | "chat_completions") | ("anthropic", "anthropic_messages")
            ) && request_id.starts_with("req_")
            {
                validated_request_id(request_id, &[])
            } else {
                None
            }
        });
        let retry = if wire.class == LlmErrorClass::Cancelled {
            RetryDisposition::Cancelled
        } else {
            wire.attempts.error_disposition()
        };

        let class = normalize_retry_backed_class(wire.class, wire.attempts);
        Ok(Self {
            class,
            phase: wire.phase,
            retry,
            attempts: wire.attempts,
            provider,
            transport,
            model,
            http_status: wire
                .http_status
                .filter(|status| (100..=599).contains(status)),
            retry_after_seconds: wire.attempts.final_retry_after_seconds(),
            request_id,
            code: incident_code_for(class).to_string(),
            operator_action: action_for(class).to_string(),
            safe_message: String::new(),
            provider_discriminator: None,
        })
    }
}

fn normalize_retry_backed_class(
    class: LlmErrorClass,
    attempts: crate::llm::RetryAttemptMetadata,
) -> LlmErrorClass {
    if class == LlmErrorClass::ProviderUnavailable && !attempts.retry_exhausted() {
        LlmErrorClass::ProviderRejected
    } else {
        class
    }
}

fn normalize_stored_provider(value: &str) -> String {
    let value = sanitize(value, &[], MAX_PROVIDER_BYTES);
    match value.as_str() {
        "nib" | "unknown" | "mock" | "openai" | "anthropic" | "google" | "grok" | "openrouter"
        | "meta" => value,
        _ => "unknown".to_string(),
    }
}

fn normalize_stored_transport(value: &str) -> String {
    let value = sanitize(value, &[], MAX_TRANSPORT_BYTES);
    match value.as_str() {
        "local"
        | "legacy"
        | "mock"
        | "responses"
        | "chat_completions"
        | "anthropic_messages"
        | "gemini_generate_content" => value,
        _ => "legacy".to_string(),
    }
}

fn normalize_stored_context(provider: &str, transport: &str) -> (String, String) {
    let provider = normalize_stored_provider(provider);
    let transport = normalize_stored_transport(transport);
    if matches!(
        (provider.as_str(), transport.as_str()),
        ("nib", "local")
            | ("unknown", "legacy")
            | ("mock", "mock")
            | (
                "openai" | "grok" | "openrouter" | "meta",
                "responses" | "chat_completions"
            )
            | ("anthropic", "anthropic_messages")
            | ("google", "gemini_generate_content")
    ) {
        (provider, transport)
    } else {
        ("unknown".to_string(), "legacy".to_string())
    }
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

fn classify_http(
    provider: &str,
    transport: &str,
    status: StatusCode,
    structured_error: Option<&Value>,
) -> LlmErrorClass {
    if let Some(class) = classify_structural_error(provider, transport, structured_error) {
        return class;
    }
    match status.as_u16() {
        401 | 403 => LlmErrorClass::Authentication,
        429 => LlmErrorClass::RateLimited,
        408 | 425 | 500 | 502 | 503 | 504 | 529 => LlmErrorClass::ProviderUnavailable,
        _ => LlmErrorClass::ProviderRejected,
    }
}

fn classify_structural_error(
    provider: &str,
    transport: &str,
    value: Option<&Value>,
) -> Option<LlmErrorClass> {
    let value = value?;
    match (provider, transport) {
        ("openai", "chat_completions") => {
            classify_openai_error_object(value.get("error")?.as_object()?)
        }
        ("openai", "responses") => classify_openai_responses_error(value),
        ("anthropic", "anthropic_messages") => classify_anthropic_error(value),
        ("google", "gemini_generate_content") => classify_gemini_error(value),
        // Compatible transports own separate provider error contracts. Numeric HTTP
        // status remains usable, but an OpenAI/Anthropic/Gemini code name is not
        // inherited merely because a wire codec is shared.
        ("grok" | "openrouter" | "meta", "chat_completions" | "responses") => None,
        _ => None,
    }
}

fn classify_openai_responses_error(value: &Value) -> Option<LlmErrorClass> {
    if let Some(error) = value.get("error").and_then(Value::as_object) {
        return classify_openai_error_object(error);
    }
    if value.get("type").and_then(Value::as_str) == Some("response.failed")
        && value.pointer("/response/status").and_then(Value::as_str) == Some("failed")
    {
        return classify_openai_error_object(value.pointer("/response/error")?.as_object()?);
    }
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return classify_consistent_codes(
            [value.get("code").and_then(Value::as_str)],
            classify_openai_code,
        );
    }
    None
}

fn classify_openai_error_object(error: &serde_json::Map<String, Value>) -> Option<LlmErrorClass> {
    if error
        .get("code")
        .is_some_and(|value| !value.is_null() && !value.is_string())
        || error
            .get("type")
            .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return None;
    }
    classify_consistent_codes(
        [
            error.get("code").and_then(Value::as_str),
            error.get("type").and_then(Value::as_str),
        ],
        classify_openai_code,
    )
}

fn classify_openai_code(code: &str) -> Option<LlmErrorClass> {
    match code {
        "invalid_api_key" | "authentication_error" => Some(LlmErrorClass::Authentication),
        "rate_limit_exceeded" | "rate_limit_error" => Some(LlmErrorClass::RateLimited),
        "insufficient_quota" | "billing_hard_limit_reached" => Some(LlmErrorClass::QuotaOrBilling),
        "model_not_found" => Some(LlmErrorClass::ModelUnavailable),
        "invalid_request_error" | "invalid_request" | "unsupported_parameter" => {
            Some(LlmErrorClass::UnsupportedRequest)
        }
        "server_error" => Some(LlmErrorClass::ProviderUnavailable),
        _ => None,
    }
}

fn classify_anthropic_error(value: &Value) -> Option<LlmErrorClass> {
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let error = value.get("error")?.as_object()?;
    if error.get("type").is_some_and(|value| !value.is_string()) {
        return None;
    }
    match error.get("type")?.as_str()? {
        "authentication_error" | "permission_error" => Some(LlmErrorClass::Authentication),
        "rate_limit_error" => Some(LlmErrorClass::RateLimited),
        "billing_error" => Some(LlmErrorClass::QuotaOrBilling),
        "not_found_error" => Some(LlmErrorClass::ModelUnavailable),
        "invalid_request_error" | "request_too_large" => Some(LlmErrorClass::UnsupportedRequest),
        "api_error" | "overloaded_error" => Some(LlmErrorClass::ProviderUnavailable),
        _ => None,
    }
}

fn classify_gemini_error(value: &Value) -> Option<LlmErrorClass> {
    let error = value.get("error")?.as_object()?;
    if error.get("status").is_some_and(|value| !value.is_string()) {
        return None;
    }
    match error.get("status")?.as_str()? {
        "UNAUTHENTICATED" | "PERMISSION_DENIED" => Some(LlmErrorClass::Authentication),
        "RESOURCE_EXHAUSTED" => Some(LlmErrorClass::RateLimited),
        "NOT_FOUND" => Some(LlmErrorClass::ModelUnavailable),
        "INVALID_ARGUMENT" => Some(LlmErrorClass::UnsupportedRequest),
        "UNAVAILABLE" | "DEADLINE_EXCEEDED" => Some(LlmErrorClass::ProviderUnavailable),
        _ => None,
    }
}

fn classify_consistent_codes<const N: usize>(
    codes: [Option<&str>; N],
    classify: impl Fn(&str) -> Option<LlmErrorClass>,
) -> Option<LlmErrorClass> {
    let mut classification = None;
    for class in codes.into_iter().flatten().filter_map(classify) {
        if classification.is_some_and(|current| current != class) {
            return None;
        }
        classification = Some(class);
    }
    classification
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
    fn structural_classification_is_provider_transport_and_envelope_specific() {
        let valid = [
            (
                "openai",
                "responses",
                json!({"error": {"code": "insufficient_quota"}}),
                LlmErrorClass::QuotaOrBilling,
            ),
            (
                "anthropic",
                "anthropic_messages",
                json!({"type": "error", "error": {"type": "overloaded_error"}}),
                LlmErrorClass::ProviderUnavailable,
            ),
            (
                "google",
                "gemini_generate_content",
                json!({"error": {"status": "RESOURCE_EXHAUSTED"}}),
                LlmErrorClass::RateLimited,
            ),
        ];
        for (provider, transport, envelope, expected) in valid {
            let error = LlmError::http(
                provider,
                transport,
                Some("model"),
                StatusCode::IM_A_TEAPOT,
                Some(&envelope),
                "safe",
                &[],
            );
            assert_eq!(error.class, expected, "{provider}/{transport}");
        }

        let provider_negative = [
            (
                "anthropic",
                "anthropic_messages",
                json!({"error": {"code": "invalid_api_key"}}),
            ),
            (
                "google",
                "gemini_generate_content",
                json!({"error": {"type": "overloaded_error"}}),
            ),
            (
                "openrouter",
                "responses",
                json!({"error": {"code": "insufficient_quota"}}),
            ),
            (
                "openai",
                "anthropic_messages",
                json!({"type": "error", "error": {"type": "authentication_error"}}),
            ),
            (
                "openai",
                "responses",
                json!({"error": {"code": "invalid_api_key", "type": 42}}),
            ),
        ];
        for (provider, transport, envelope) in provider_negative {
            let error = LlmError::http(
                provider,
                transport,
                None,
                StatusCode::IM_A_TEAPOT,
                Some(&envelope),
                "safe",
                &[],
            );
            assert_eq!(
                error.class,
                LlmErrorClass::ProviderRejected,
                "{provider}/{transport}: {envelope}"
            );
        }
    }

    #[test]
    fn generic_bad_request_statuses_are_conservative() {
        for status in [StatusCode::BAD_REQUEST, StatusCode::UNPROCESSABLE_ENTITY] {
            for provider in [
                "openai",
                "grok",
                "openrouter",
                "meta",
                "anthropic",
                "google",
            ] {
                let transport = match provider {
                    "anthropic" => "anthropic_messages",
                    "google" => "gemini_generate_content",
                    _ => "chat_completions",
                };
                let error = LlmError::http(
                    provider,
                    transport,
                    Some("model"),
                    status,
                    None,
                    "safe",
                    &[],
                );
                assert_eq!(error.class, LlmErrorClass::ProviderRejected, "{provider}");
                assert_eq!(error.provider_discriminator(), None);
            }
        }
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

        let json_secret = "active/credential".to_string();
        let json_escaped = r#"active\/credential"#;
        let escaped_error = LlmError::new(
            LlmErrorClass::Authentication,
            LlmErrorPhase::HttpResponse,
            RetryDisposition::NotRetryable,
            LlmErrorMetadata::new(
                "openai",
                "responses",
                Some(json_escaped),
                Some(401),
                std::slice::from_ref(&json_secret),
            ),
            "safe",
        );
        let escaped_report = escaped_error.user_report(None);
        assert!(escaped_report.contains("model: [REDACTED]"));
        assert!(!escaped_report.contains(json_escaped));
    }

    #[test]
    fn debug_views_never_render_configured_labels_or_encoded_sensitive_values() {
        let active = "active/credential-123".to_string();
        let inactive = "inactive/credential-456".to_string();
        let sentinels = [
            active.as_str(),
            inactive.as_str(),
            "active%2Fcredential-123",
            "inactive%2Fcredential-456",
            r#"active\/credential-123"#,
            r#"inactive\/credential-456"#,
            "YWN0aXZlL2NyZWRlbnRpYWwtMTIz",
            "aW5hY3RpdmUvY3JlZGVudGlhbC00NTY=",
            "control\u{1b}sentinel",
        ];
        let joined = sentinels.join("|");
        let sensitive_values = vec![active.clone(), inactive.clone()];
        let context = LlmErrorContext::new(
            format!("openai-{joined}"),
            format!("responses-{joined}"),
            Some(format!("model-{joined}")),
            sensitive_values.clone(),
            crate::llm::RetryAttemptMetadata::no_network_attempt(),
        );
        let error = LlmError::new(
            LlmErrorClass::ProviderRejected,
            LlmErrorPhase::HttpResponse,
            RetryDisposition::NotRetryable,
            LlmErrorMetadata::new(
                &format!("openai-{joined}"),
                &format!("responses-{joined}"),
                Some(&format!("model-{joined}")),
                Some(400),
                &sensitive_values,
            ),
            "safe",
        );

        for debug in [format!("{context:?}"), format!("{error:?}")] {
            for sentinel in sentinels {
                assert!(
                    !debug.contains(sentinel),
                    "Debug leaked {sentinel}: {debug}"
                );
            }
            assert!(!debug.contains('\u{1b}'));
            assert!(debug.len() < 1_024);
        }
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
    fn deserialization_normalizes_hostile_legacy_fields_and_preserves_retry_evidence() {
        let attempts = crate::llm::RetryAttemptMetadata::new(
            3,
            true,
            true,
            Some(std::time::Duration::from_secs(12)),
        );
        let error = LlmError::new(
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::HttpResponse,
            RetryDisposition::NotRetryable,
            LlmErrorMetadata::new("openai", "responses", Some("gpt-safe"), Some(422), &[]),
            "safe",
        )
        .with_retry_attempts(attempts)
        .with_request_id("req_safe-123", &[])
        .with_documented_transport_incompatibility();
        let encoded = serde_json::to_string(&error).expect("serialize bounded error");
        assert!(!encoded.contains("provider_discriminator"));
        let restored: LlmError = serde_json::from_str(&encoded).expect("restore bounded error");
        assert_eq!(restored.attempts, attempts);
        assert_eq!(restored.request_id.as_deref(), Some("req_safe-123"));
        assert_eq!(restored.retry_after_seconds, Some(12));
        assert_eq!(restored.provider_discriminator(), None);

        let raw = "legacy/credential-123";
        let variants = [
            raw,
            "legacy%2Fcredential-123",
            r#"legacy\/credential-123"#,
            "bGVnYWN5L2NyZWRlbnRpYWwtMTIz",
            "legacy\u{1b}credential-123",
        ];
        let hostile = json!({
            "class": "authentication",
            "phase": "http_response",
            "retry": "exhausted",
            "attempts": {
                "attempts": 0,
                "credential_rotation_occurred": false,
                "retry_exhausted": false,
                "final_retry_after_seconds": null
            },
            "provider": format!("{}{}", variants.join("|"), "x".repeat(8_192)),
            "transport": variants.join("|"),
            "model": format!("{}{}", variants.join("|"), "m".repeat(32_768)),
            "http_status": 999,
            "retry_after_seconds": 9_999,
            "request_id": "bGVnYWN5L2NyZWRlbnRpYWwtMTIz",
            "incident_code": format!("{}{}", variants.join("|"), "c".repeat(16_384)),
            "action": format!("{}{}", variants.join("|"), "a".repeat(16_384))
        });
        let restored: LlmError =
            serde_json::from_value(hostile).expect("hostile legacy error remains readable");
        assert_eq!(restored.provider, "unknown");
        assert_eq!(restored.transport, "legacy");
        assert_eq!(restored.model.as_deref(), Some("[stored model redacted]"));
        assert_eq!(restored.http_status, None);
        assert_eq!(restored.retry, RetryDisposition::NotAttempted);
        assert_eq!(restored.retry_after_seconds, None);
        assert_eq!(restored.request_id, None);
        assert_eq!(restored.incident_code(), "LLM-AUTH");

        let wrong_header_owner = json!({
            "class": "provider_rejected",
            "phase": "http_response",
            "retry": "not_attempted",
            "attempts": {
                "attempts": 0,
                "credential_rotation_occurred": false,
                "retry_exhausted": false,
                "final_retry_after_seconds": null
            },
            "provider": "openai",
            "transport": "anthropic_messages",
            "http_status": 400,
            "request_id": "req_wrong-transport"
        });
        let restored: LlmError = serde_json::from_value(wrong_header_owner)
            .expect("cross-transport legacy error remains readable");
        assert_eq!(restored.provider, "unknown");
        assert_eq!(restored.transport, "legacy");
        assert_eq!(restored.request_id, None);
        let outputs = [
            restored.user_report(None),
            format!("{restored:?}"),
            serde_json::to_string(&restored).expect("serialize normalized error"),
        ];
        for output in outputs {
            assert!(output.len() < MAX_MESSAGE_BYTES + 1_024);
            assert!(!output.contains('\u{1b}'));
            for variant in variants {
                assert!(
                    !output.contains(variant),
                    "restored output leaked {variant}"
                );
            }
        }
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

        let short_secret = "foo".to_string();
        assert!(error
            .with_request_id("Zm9v", std::slice::from_ref(&short_secret))
            .request_id
            .is_none());
    }
}
