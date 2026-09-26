//! OpenAI-compatible chat completions (OpenAI, Grok, OpenRouter).

use crate::config::ReasoningEffort;
use crate::llm::types::{
    LlmDelta, LlmFinishReason, LlmMessage, LlmRequest, LlmRequestScope, LlmResponse,
    LlmStreamEvent, LlmTerminalStatus, LlmUsage, ProviderCallId, ProviderContinuation,
    ToolCallAccumulator, ToolCallRequest, ToolChoice, ToolDefinition, ToolResult,
};
use crate::tools::ToolInvocationId;
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::{mpsc, oneshot};

use super::{LlmClient, LlmStream};

const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const CHAT_TRANSPORT: &str = "chat_completions";
const CHAT_REASONING_TOOL_GUIDANCE: &str = " Configure this provider with api = \"responses\", or set reasoning_effort = \"none\" if the model supports Chat tool calls without reasoning.";

struct ChatTurnState {
    assistant_message: Value,
    calls: Vec<(ToolInvocationId, ProviderCallId)>,
}

fn chat_continuation(
    provider: &str,
    model: &str,
    scope: Option<LlmRequestScope>,
    assistant_message: Value,
    calls: &[ToolCallRequest],
) -> Result<ProviderContinuation, String> {
    let calls = calls
        .iter()
        .map(|call| {
            call.call_id
                .clone()
                .map(|provider_call_id| (call.invocation_id, provider_call_id))
                .ok_or_else(|| "Chat tool call is missing its provider call ID".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let unique = calls
        .iter()
        .map(|(_, call_id)| call_id.as_str())
        .collect::<BTreeSet<_>>();
    if unique.len() != calls.len() {
        return Err("Chat continuation contains duplicate provider call IDs".to_string());
    }
    let encoded_bytes = serde_json::to_vec(&assistant_message)
        .map_err(|error| format!("failed to measure Chat continuation: {error}"))?
        .len();
    ProviderContinuation::new(
        provider,
        model,
        CHAT_TRANSPORT,
        scope,
        calls
            .iter()
            .map(|(invocation_id, _)| *invocation_id)
            .collect(),
        1,
        encoded_bytes,
        ChatTurnState {
            assistant_message,
            calls,
        },
    )
}

fn into_chat_messages(
    continuation: ProviderContinuation,
    provider: &str,
    model: &str,
    scope: Option<&LlmRequestScope>,
) -> Result<Vec<Value>, String> {
    let (state, outputs): (ChatTurnState, BTreeMap<ToolInvocationId, ToolResult>) =
        continuation.consume(provider, model, CHAT_TRANSPORT, scope)?;
    let mut messages = vec![state.assistant_message];
    for (invocation_id, call_id) in state.calls {
        let output = outputs
            .get(&invocation_id)
            .ok_or_else(|| "Chat continuation is missing a tool output".to_string())?;
        messages.push(json!({
            "role": "tool",
            "tool_call_id": call_id.as_str(),
            "content": output.encoded_output(),
        }));
    }
    Ok(messages)
}

pub struct OpenAiCompatClient {
    client: Client,
    provider: String,
    model: String,
    api_keys: Vec<String>,
    diagnostic_secrets: Vec<String>,
    base_url: String,
    reasoning_effort: Option<ReasoningEffort>,
}

impl OpenAiCompatClient {
    pub fn new(model: String, api_keys: Vec<String>, base_url: impl Into<String>) -> Self {
        Self::configured(
            "openai-compatible".to_string(),
            model,
            api_keys,
            base_url,
            None,
        )
    }

    pub fn configured(
        provider: String,
        model: String,
        api_keys: Vec<String>,
        base_url: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        let diagnostic_secrets = api_keys.clone();
        Self::configured_with_diagnostic_secrets(
            provider,
            model,
            api_keys,
            diagnostic_secrets,
            base_url,
            reasoning_effort,
        )
    }

    pub(crate) fn configured_with_diagnostic_secrets(
        provider: String,
        model: String,
        api_keys: Vec<String>,
        diagnostic_secrets: Vec<String>,
        base_url: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        Self::configured_with_diagnostic_secrets_and_client(
            provider,
            model,
            api_keys,
            diagnostic_secrets,
            base_url,
            reasoning_effort,
            Client::new(),
        )
    }

    pub(crate) fn configured_with_diagnostic_secrets_and_client(
        provider: String,
        model: String,
        api_keys: Vec<String>,
        mut diagnostic_secrets: Vec<String>,
        base_url: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
        client: Client,
    ) -> Self {
        diagnostic_secrets.extend(api_keys.iter().cloned());
        diagnostic_secrets
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        diagnostic_secrets.dedup();
        Self {
            client,
            provider,
            model,
            api_keys,
            diagnostic_secrets,
            base_url: base_url.into(),
            reasoning_effort,
        }
    }

    pub fn openai(model: String, api_keys: Vec<String>) -> Self {
        Self::new(
            model,
            api_keys,
            "https://api.openai.com/v1/chat/completions",
        )
    }

    pub fn xai(model: String, api_keys: Vec<String>) -> Self {
        Self::new(model, api_keys, "https://api.x.ai/v1/chat/completions")
    }

    pub fn openrouter(model: String, api_keys: Vec<String>) -> Self {
        Self::new(
            model,
            api_keys,
            "https://openrouter.ai/api/v1/chat/completions",
        )
    }

    pub fn meta(model: String, api_keys: Vec<String>) -> Self {
        Self::new(model, api_keys, "https://api.meta.com/v1/chat/completions")
    }

    fn endpoint(&self) -> String {
        if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
        }
    }

    pub(crate) fn request_body(
        &self,
        request: LlmRequest<'_>,
        stream: bool,
    ) -> Result<Value, String> {
        let LlmRequest {
            messages,
            tools,
            options,
            max_output_tokens,
            tool_choice,
            scope,
            continuation,
        } = request;
        let continuation_messages = continuation
            .map(|continuation| {
                into_chat_messages(continuation, &self.provider, &self.model, scope.as_ref())
            })
            .transpose()?
            .unwrap_or_default();
        let mut encoded_messages = messages
            .iter()
            .map(LlmMessage::to_openai_chat)
            .collect::<Vec<_>>();
        encoded_messages.extend(continuation_messages);

        let mut body = json!({
            "model": self.model,
            "messages": encoded_messages,
        });
        if let Some(temperature) = options.temperature() {
            body["temperature"] = json!(temperature);
        }
        if let Some(max_output_tokens) = max_output_tokens {
            if max_output_tokens == 0 {
                return Err("max_output_tokens must be greater than zero".to_string());
            }
            let field = if uses_max_completion_tokens(&self.provider) {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            body[field] = json!(max_output_tokens);
        }
        if stream {
            body["stream"] = json!(true);
            // Only canonical OpenAI documents this option. Compatible providers keep
            // their existing request shape unless their own contract proves support.
            if self.provider == "openai" {
                body["stream_options"] = json!({"include_usage": true});
            }
        }
        if let Some(tools) = tools {
            body["tools"] = json!(encode_chat_tools(tools, &self.provider));
            if let Some(choice) = chat_tool_choice(&self.provider, &self.model, tool_choice) {
                body["tool_choice"] = choice;
            }
        }
        if let Some(effort) = options.resolved_reasoning(self.reasoning_effort) {
            body["reasoning_effort"] = json!(effort.as_str());
        }
        Ok(body)
    }

    fn requests_tools_with_reasoning(&self, request: &LlmRequest<'_>) -> bool {
        request.tools.is_some_and(|tools| !tools.is_empty())
            && crate::llm::conformance::resolved_reasoning(request, self.reasoning_effort)
                .is_some_and(|effort| effort != ReasoningEffort::None)
    }

    fn contextual_error(&self, kind: &str, detail: &str) -> String {
        bounded_redacted(
            format!(
                "{} Chat Completions API {kind} (model {}): {detail}",
                self.provider, self.model
            ),
            &self.diagnostic_secrets,
        )
    }

    fn error_context(
        &self,
        retry_attempts: crate::llm::RetryAttemptMetadata,
    ) -> crate::llm::error::LlmErrorContext {
        crate::llm::error::LlmErrorContext::new(
            self.provider.clone(),
            CHAT_TRANSPORT,
            Some(self.model.clone()),
            self.diagnostic_secrets.clone(),
            retry_attempts,
        )
    }

    fn protocol_error(&self, kind: &str, detail: &str) -> crate::llm::LlmError {
        crate::llm::LlmError::provider_protocol(
            &self.provider,
            CHAT_TRANSPORT,
            Some(&self.model),
            crate::llm::LlmErrorPhase::TerminalValidation,
            self.contextual_error(kind, detail),
            &self.diagnostic_secrets,
        )
    }

    fn rejected_request(&self, detail: &str) -> crate::llm::LlmError {
        crate::llm::LlmError::request_rejected(
            &self.provider,
            CHAT_TRANSPORT,
            Some(&self.model),
            self.contextual_error("request rejected", detail),
            &self.diagnostic_secrets,
        )
    }

    fn request_error(
        &self,
        error: &str,
        retry_attempts: crate::llm::RetryAttemptMetadata,
    ) -> crate::llm::LlmError {
        let safe_credential_error = error == "provider request requires at least one credential"
            || (error.starts_with("all ") && error.contains("credential(s) exhausted"));
        let detail = if safe_credential_error {
            error
        } else {
            "request failed before a valid HTTP response"
        };
        crate::llm::LlmError::transport(
            &self.provider,
            CHAT_TRANSPORT,
            Some(&self.model),
            self.contextual_error("request failed", detail),
            &self.diagnostic_secrets,
        )
        .with_retry_attempts(retry_attempts)
    }

    async fn http_error(
        &self,
        status: StatusCode,
        response: reqwest::Response,
        requests_tools_with_reasoning: bool,
        retry_attempts: crate::llm::RetryAttemptMetadata,
    ) -> crate::llm::LlmError {
        let request_id = if self.provider == "openai" {
            crate::llm::error::bounded_request_id_header(response.headers().get("x-request-id"))
        } else {
            None
        };
        let body = crate::llm::read_bounded_error_response(
            response,
            "OpenAI-compatible API error response",
        )
        .await;
        let (structured, detail) = match body {
            Ok(body) => {
                let structured = serde_json::from_str::<Value>(&body).ok();
                let detail = structured_chat_error_detail(&body);
                (structured, detail)
            }
            Err(error) if error.contains("byte limit") => (None, error),
            Err(_) => (
                None,
                "provider error response could not be read".to_string(),
            ),
        };
        let guidance = if requests_tools_with_reasoning
            && matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
            ) {
            CHAT_REASONING_TOOL_GUIDANCE
        } else {
            ""
        };
        let error = crate::llm::LlmError::http(
            &self.provider,
            CHAT_TRANSPORT,
            Some(&self.model),
            status,
            structured.as_ref(),
            self.contextual_error(
                &format!("error HTTP {}", status.as_u16()),
                &format!("{detail}{guidance}"),
            ),
            &self.diagnostic_secrets,
        )
        .with_retry_attempts(retry_attempts);
        match request_id {
            Some(request_id) => error.with_request_id(&request_id, &self.diagnostic_secrets),
            None => error,
        }
    }

    fn completion_read_error(&self, error: &str) -> crate::llm::LlmError {
        let detail = if error.contains("byte limit") || error.starts_with("invalid ") {
            error
        } else {
            "completion response could not be read"
        };
        self.protocol_error("protocol failure", detail)
    }
}

fn uses_max_completion_tokens(provider: &str) -> bool {
    matches!(provider, "openai" | "grok")
}

fn encode_chat_tools(tools: &[ToolDefinition], provider: &str) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut encoded = tool.to_openai_tool();
            if !chat_includes_strict(provider) {
                if let Some(function) = encoded.get_mut("function").and_then(Value::as_object_mut) {
                    function.remove("strict");
                    if matches!(provider, "openrouter" | "meta") {
                        if let Some(parameters) = function.get("parameters").cloned() {
                            function.insert(
                                "parameters".to_string(),
                                crate::llm::types::gemini_compatible_schema(&parameters),
                            );
                        }
                    }
                }
            }
            encoded
        })
        .collect()
}

fn chat_includes_strict(provider: &str) -> bool {
    matches!(provider, "openai" | "grok")
}

fn chat_tool_choice(provider: &str, model: &str, tool_choice: ToolChoice) -> Option<Value> {
    if provider == "meta" || (provider == "openrouter" && model.starts_with("anthropic/")) {
        None
    } else {
        Some(tool_choice.as_openai_value())
    }
}

fn is_chat_refusal_value(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Bool(true) => true,
        _ => true,
    }
}

fn chat_text_content(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                if let Some(fragment) = part.as_str() {
                    text.push_str(fragment);
                } else if let Some(fragment) = part.get("text").and_then(Value::as_str) {
                    text.push_str(fragment);
                }
            }
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn structured_chat_error_detail(body: &str) -> String {
    let Some(_error) = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error").cloned())
    else {
        return "provider returned a non-JSON error response".to_string();
    };
    "provider returned a structured error; provider-supplied detail omitted".to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatProtocolFailureKind {
    InBandError,
    Truncated,
    SafetyBlocked,
    Refused,
    UnsupportedTerminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChatProtocolFailure {
    kind: ChatProtocolFailureKind,
}

impl ChatProtocolFailure {
    fn new(kind: ChatProtocolFailureKind) -> Self {
        Self { kind }
    }

    fn in_band_error() -> Self {
        Self::new(ChatProtocolFailureKind::InBandError)
    }

    fn context_kind(self) -> &'static str {
        match self.kind {
            ChatProtocolFailureKind::InBandError => "provider failure",
            ChatProtocolFailureKind::Truncated
            | ChatProtocolFailureKind::SafetyBlocked
            | ChatProtocolFailureKind::Refused
            | ChatProtocolFailureKind::UnsupportedTerminal => "unsafe terminal state",
        }
    }

    fn detail(self, requests_tools_with_reasoning: bool) -> String {
        let detail = match self.kind {
            ChatProtocolFailureKind::InBandError => {
                "provider returned an in-band error; provider-supplied detail omitted"
            }
            ChatProtocolFailureKind::Truncated => {
                "completion was truncated before a safe terminal state"
            }
            ChatProtocolFailureKind::SafetyBlocked => {
                "completion was blocked by provider safety controls"
            }
            ChatProtocolFailureKind::Refused => {
                "completion returned a refusal and cannot be authorized as successful"
            }
            ChatProtocolFailureKind::UnsupportedTerminal => {
                "completion returned an unsupported finish_reason"
            }
        };
        if requests_tools_with_reasoning && self.kind == ChatProtocolFailureKind::InBandError {
            format!("{detail}{CHAT_REASONING_TOOL_GUIDANCE}")
        } else {
            detail.to_string()
        }
    }
}

fn chat_protocol_failure(data: &Value) -> Option<ChatProtocolFailure> {
    let choices = data.get("choices").and_then(Value::as_array);
    let mut errors = Vec::new();
    if let Some(error) = data.get("error").filter(|error| !error.is_null()) {
        errors.push(error);
    }
    if let Some(choices) = choices {
        for choice in choices {
            for error in [
                choice.get("error"),
                choice
                    .get("message")
                    .and_then(|message| message.get("error")),
                choice.get("delta").and_then(|delta| delta.get("error")),
            ]
            .into_iter()
            .flatten()
            .filter(|error| !error.is_null())
            {
                errors.push(error);
            }
        }
    }

    let has_error_finish = choices.is_some_and(|choices| {
        choices
            .iter()
            .any(|choice| choice.get("finish_reason").and_then(Value::as_str) == Some("error"))
    });
    if !errors.is_empty() || has_error_finish {
        return Some(ChatProtocolFailure::in_band_error());
    }

    if choices.is_some_and(|choices| {
        choices.iter().any(|choice| {
            [choice.get("message"), choice.get("delta")]
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("refusal"))
                .any(is_chat_refusal_value)
        })
    }) {
        return Some(ChatProtocolFailure::new(ChatProtocolFailureKind::Refused));
    }

    choices.and_then(|choices| {
        choices.iter().find_map(|choice| {
            let reason = choice.get("finish_reason").and_then(Value::as_str)?;
            match reason {
                "" | "null" | "stop" | "tool_calls" => None,
                "length" => Some(ChatProtocolFailure::new(ChatProtocolFailureKind::Truncated)),
                "content_filter" => Some(ChatProtocolFailure::new(
                    ChatProtocolFailureKind::SafetyBlocked,
                )),
                _ => Some(ChatProtocolFailure::new(
                    ChatProtocolFailureKind::UnsupportedTerminal,
                )),
            }
        })
    })
}

fn chat_assistant_message(
    content: Option<&str>,
    calls: &[ToolCallRequest],
) -> Result<Value, String> {
    let tool_calls = calls
        .iter()
        .map(|call| {
            let call_id = call
                .call_id
                .as_ref()
                .ok_or_else(|| "Chat tool call is missing its provider call ID".to_string())?;
            let arguments = serde_json::to_string(&call.arguments)
                .map_err(|error| format!("failed to encode Chat tool arguments: {error}"))?;
            Ok(json!({
                "id": call_id.as_str(),
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": arguments,
                }
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(json!({
        "role": "assistant",
        "content": content,
        "tool_calls": tool_calls,
    }))
}

fn attach_chat_continuation(
    response: &mut LlmResponse,
    _data: &Value,
    provider: &str,
    model: &str,
    scope: Option<LlmRequestScope>,
) -> Result<(), String> {
    let Some(calls) = response
        .tool_calls
        .as_ref()
        .filter(|calls| !calls.is_empty())
    else {
        return Ok(());
    };
    let assistant_message = chat_assistant_message(response.content.as_deref(), calls)?;
    response.continuation = Some(chat_continuation(
        provider,
        model,
        scope,
        assistant_message,
        calls,
    )?);
    Ok(())
}

fn bounded_redacted(message: String, secrets: &[String]) -> String {
    let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
        &message,
        secrets.iter().cloned(),
    );
    truncate_diagnostic(&escape_diagnostic_controls(&redacted), MAX_DIAGNOSTIC_BYTES)
}

fn escape_diagnostic_controls(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_graphic() || character == ' ' {
            escaped.push(character);
        } else {
            escaped.extend(character.escape_default());
        }
    }
    escaped
}

fn truncate_diagnostic(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &value[..end])
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, crate::llm::LlmError> {
        crate::llm::conformance::validate_request_capabilities(
            &request,
            &self.provider,
            crate::llm::registry::ProviderTransport::ChatCompletions,
            crate::llm::conformance::ProviderOperation::Complete,
        )
        .map_err(|error| self.rejected_request(&error))?;
        let requests_tools_with_reasoning = self.requests_tools_with_reasoning(&request);
        let scope = request.scope.clone();
        let body = self
            .request_body(request, false)
            .map_err(|error| self.rejected_request(&error))?;
        let url = self.endpoint();

        let retry_capabilities = crate::llm::registry::retry_capabilities(
            &self.provider,
            crate::llm::registry::ProviderTransport::ChatCompletions,
        );
        let outcome = crate::llm::send_with_retry(
            |credential_index| {
                let mut request = self.client.post(&url).json(&body);
                request = request.bearer_auth(&self.api_keys[credential_index]);
                request
            },
            self.api_keys.len(),
            retry_capabilities,
        )
        .await
        .map_err(|failure| self.request_error(&failure.error, failure.attempts))?;
        let retry_attempts = outcome.attempts;
        let resp = outcome.value;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(self
                .http_error(status, resp, requests_tools_with_reasoning, retry_attempts)
                .await);
        }

        let data =
            crate::llm::read_bounded_json_response(resp, "OpenAI-compatible completion response")
                .await
                .map_err(|error| {
                    self.completion_read_error(&error)
                        .with_retry_attempts(retry_attempts)
                })?;
        if let Some(failure) = chat_protocol_failure(&data) {
            return Err(crate::llm::LlmError::provider_rejected(
                &self.provider,
                CHAT_TRANSPORT,
                Some(&self.model),
                crate::llm::LlmErrorPhase::TerminalValidation,
                Some(&data),
                self.contextual_error(
                    failure.context_kind(),
                    &failure.detail(requests_tools_with_reasoning),
                ),
                &self.diagnostic_secrets,
            )
            .with_retry_attempts(retry_attempts));
        }
        let mut response = parse_openai_response(&data).map_err(|_| {
            self.protocol_error(
                "protocol failure",
                "completion response did not satisfy the Chat Completions schema",
            )
            .with_retry_attempts(retry_attempts)
        })?;
        attach_chat_continuation(&mut response, &data, &self.provider, &self.model, scope)
            .map_err(|error| {
                self.protocol_error("protocol failure", &error)
                    .with_retry_attempts(retry_attempts)
            })?;
        response.attempts = retry_attempts;
        Ok(response)
    }

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, crate::llm::LlmError> {
        crate::llm::conformance::validate_request_capabilities(
            &request,
            &self.provider,
            crate::llm::registry::ProviderTransport::ChatCompletions,
            crate::llm::conformance::ProviderOperation::Stream,
        )
        .map_err(|error| self.rejected_request(&error))?;
        let requests_tools_with_reasoning = self.requests_tools_with_reasoning(&request);
        let scope = request.scope.clone();
        let body = self
            .request_body(request, true)
            .map_err(|error| self.rejected_request(&error))?;
        let url = self.endpoint();

        let retry_capabilities = crate::llm::registry::retry_capabilities(
            &self.provider,
            crate::llm::registry::ProviderTransport::ChatCompletions,
        );
        let outcome = crate::llm::send_with_retry(
            |credential_index| {
                let mut request = self.client.post(&url).json(&body);
                request = request.bearer_auth(&self.api_keys[credential_index]);
                request
            },
            self.api_keys.len(),
            retry_capabilities,
        )
        .await
        .map_err(|failure| self.request_error(&failure.error, failure.attempts))?;
        let retry_attempts = outcome.attempts;
        let resp = outcome.value;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(self
                .http_error(status, resp, requests_tools_with_reasoning, retry_attempts)
                .await);
        }
        crate::llm::ensure_response_content_length(
            &resp,
            crate::llm::MAX_LLM_STREAM_BYTES,
            "OpenAI-compatible stream response",
        )
        .map_err(|error| {
            self.protocol_error("protocol failure", &error)
                .with_retry_attempts(retry_attempts)
        })?;

        let provider = self.provider.clone();
        let canonical_openai = provider == "openai";
        let model = self.model.clone();
        let continuation_scope = scope;
        let diagnostic_secrets = self.diagnostic_secrets.clone();
        let (tx, rx) = mpsc::channel(100);
        let (completion_tx, completion_rx) = oneshot::channel();

        tokio::spawn(async move {
            use eventsource_stream::{EventStreamError, Eventsource};
            use futures_util::StreamExt;

            let mut completion_tx = Some(completion_tx);
            let mut content = String::new();
            let mut tool_calls = ToolCallAccumulator::default();
            let mut call_ids = BTreeMap::new();
            let mut pending_completion: Option<LlmResponse> = None;
            let mut budget = crate::llm::ResponseByteBudget::new(
                crate::llm::MAX_LLM_STREAM_BYTES,
                "OpenAI-compatible stream response",
            );
            let bounded_bytes = resp.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|_| {
                    "Chat Completions stream transport failed while reading a response".to_string()
                })?;
                budget.account(chunk.len())?;
                Ok::<_, String>(chunk)
            });
            let mut stream = bounded_bytes.eventsource();
            let mut item_budget = crate::llm::ResponseItemBudget::new(
                crate::llm::MAX_LLM_STREAM_EVENTS,
                "OpenAI-compatible stream events",
            );
            'events: while let Some(event_res) =
                crate::llm::next_stream_item_or_closed(&tx, &mut stream).await
            {
                match event_res {
                    Ok(event) => {
                        if let Err(error) = item_budget.account(1) {
                            fail_chat_stream_conditionally(
                                &tx,
                                &mut completion_tx,
                                contextual_chat_error(
                                    &provider,
                                    &model,
                                    "protocol failure",
                                    &error,
                                    &diagnostic_secrets,
                                ),
                                pending_completion.is_some(),
                            )
                            .await;
                            return;
                        }
                        if let Err(error) = crate::llm::ensure_stream_event_size(
                            event.data.len(),
                            "OpenAI-compatible stream event",
                        ) {
                            fail_chat_stream_conditionally(
                                &tx,
                                &mut completion_tx,
                                contextual_chat_error(
                                    &provider,
                                    &model,
                                    "protocol failure",
                                    &error,
                                    &diagnostic_secrets,
                                ),
                                pending_completion.is_some(),
                            )
                            .await;
                            return;
                        }
                        if event.event == "error" {
                            let structured = serde_json::from_str::<Value>(&event.data).ok();
                            let failure = structured
                                .as_ref()
                                .and_then(chat_protocol_failure)
                                .unwrap_or_else(ChatProtocolFailure::in_band_error);
                            fail_chat_stream_conditionally(
                                &tx,
                                &mut completion_tx,
                                crate::llm::LlmError::provider_rejected(
                                    &provider,
                                    CHAT_TRANSPORT,
                                    Some(&model),
                                    crate::llm::LlmErrorPhase::Stream,
                                    structured.as_ref(),
                                    contextual_chat_error(
                                        &provider,
                                        &model,
                                        failure.context_kind(),
                                        &failure.detail(requests_tools_with_reasoning),
                                        &diagnostic_secrets,
                                    ),
                                    &diagnostic_secrets,
                                ),
                                pending_completion.is_some(),
                            )
                            .await;
                            return;
                        }
                        if event.data == "[DONE]" {
                            if let Some(completion) = pending_completion.take() {
                                if let Some(sender) = completion_tx.take() {
                                    let _ = sender.send(Ok(completion));
                                }
                            } else {
                                fail_chat_stream(
                                    &tx,
                                    &mut completion_tx,
                                    contextual_chat_error(
                                        &provider,
                                        &model,
                                        "protocol failure",
                                        "stream ended before an explicit finish_reason",
                                        &diagnostic_secrets,
                                    ),
                                )
                                .await;
                            }
                            return;
                        }

                        match serde_json::from_str::<Value>(&event.data) {
                            Ok(data) => {
                                let chunk_usage = match parse_openai_usage(&data) {
                                    Ok(usage) => usage,
                                    Err(error) => {
                                        fail_chat_stream_conditionally(
                                            &tx,
                                            &mut completion_tx,
                                            contextual_chat_error(
                                                &provider,
                                                &model,
                                                "protocol failure",
                                                &error,
                                                &diagnostic_secrets,
                                            ),
                                            pending_completion.is_some(),
                                        )
                                        .await;
                                        return;
                                    }
                                };
                                if let Some(completion) = pending_completion.as_mut() {
                                    let usage_only = data
                                        .get("choices")
                                        .and_then(Value::as_array)
                                        .is_some_and(Vec::is_empty)
                                        && chunk_usage.is_some();
                                    if !usage_only || completion.usage.is_some() {
                                        fail_chat_private_completion(
                                            &mut completion_tx,
                                            contextual_chat_error(
                                                &provider,
                                                &model,
                                                "protocol failure",
                                                "stream emitted invalid or duplicate data after its terminal event",
                                                &diagnostic_secrets,
                                            ),
                                        );
                                        return;
                                    }
                                    completion.usage = chunk_usage;
                                    continue;
                                }
                                if let Some(failure) = chat_protocol_failure(&data) {
                                    fail_chat_stream(
                                        &tx,
                                        &mut completion_tx,
                                        crate::llm::LlmError::provider_rejected(
                                            &provider,
                                            CHAT_TRANSPORT,
                                            Some(&model),
                                            crate::llm::LlmErrorPhase::Stream,
                                            Some(&data),
                                            contextual_chat_error(
                                                &provider,
                                                &model,
                                                failure.context_kind(),
                                                &failure.detail(requests_tools_with_reasoning),
                                                &diagnostic_secrets,
                                            ),
                                            &diagnostic_secrets,
                                        ),
                                    )
                                    .await;
                                    return;
                                }
                                let ids = match chat_stream_call_ids(&data) {
                                    Ok(ids) => ids,
                                    Err(_) => {
                                        fail_chat_stream(
                                            &tx,
                                            &mut completion_tx,
                                            contextual_chat_error(
                                                &provider,
                                                &model,
                                                "protocol failure",
                                                "stream contained an invalid provider call ID",
                                                &diagnostic_secrets,
                                            ),
                                        )
                                        .await;
                                        return;
                                    }
                                };
                                for (index, call_id) in ids {
                                    if call_ids
                                        .insert(index, call_id.clone())
                                        .is_some_and(|existing| existing != call_id)
                                    {
                                        fail_chat_stream(
                                            &tx,
                                            &mut completion_tx,
                                            contextual_chat_error(
                                                &provider,
                                                &model,
                                                "protocol failure",
                                                "stream changed a provider call ID",
                                                &diagnostic_secrets,
                                            ),
                                        )
                                        .await;
                                        return;
                                    }
                                }
                                let parsed_events = match parse_openai_stream_chunk(&data) {
                                    Ok(events) => events,
                                    Err(_) => {
                                        fail_chat_stream(
                                            &tx,
                                            &mut completion_tx,
                                            contextual_chat_error(
                                                &provider,
                                                &model,
                                                "protocol failure",
                                                "stream contained an invalid Chat Completions delta",
                                                &diagnostic_secrets,
                                            ),
                                        )
                                        .await;
                                        return;
                                    }
                                };
                                if chunk_usage.is_some()
                                    && !parsed_events
                                        .iter()
                                        .any(|event| matches!(event, LlmStreamEvent::Terminal(_)))
                                {
                                    fail_chat_stream(
                                        &tx,
                                        &mut completion_tx,
                                        contextual_chat_error(
                                            &provider,
                                            &model,
                                            "protocol failure",
                                            "stream returned usage before its terminal event",
                                            &diagnostic_secrets,
                                        ),
                                    )
                                    .await;
                                    return;
                                }
                                for parsed in parsed_events {
                                    match &parsed {
                                        LlmStreamEvent::Delta(LlmDelta::Content(fragment)) => {
                                            content.push_str(fragment)
                                        }
                                        LlmStreamEvent::Delta(
                                            delta @ LlmDelta::ToolCallChunk { .. },
                                        ) => tool_calls.push(delta),
                                        LlmStreamEvent::Terminal(reason) => {
                                            let calls = match std::mem::take(&mut tool_calls)
                                                .finish_with_call_ids(std::mem::take(&mut call_ids))
                                            {
                                                Ok(calls) => calls,
                                                Err(_) => {
                                                    fail_chat_stream(
                                                        &tx,
                                                        &mut completion_tx,
                                                        contextual_chat_error(
                                                            &provider,
                                                            &model,
                                                            "protocol failure",
                                                            "stream ended with an incomplete tool call",
                                                            &diagnostic_secrets,
                                                        ),
                                                    )
                                                    .await;
                                                    return;
                                                }
                                            };
                                            if (!calls.is_empty()
                                                && *reason != LlmFinishReason::ToolCalls)
                                                || (calls.is_empty()
                                                    && *reason == LlmFinishReason::ToolCalls)
                                            {
                                                fail_chat_stream(
                                                    &tx,
                                                    &mut completion_tx,
                                                    contextual_chat_error(
                                                        &provider,
                                                        &model,
                                                        "protocol failure",
                                                        "finish_reason did not match the completed tool-call set",
                                                        &diagnostic_secrets,
                                                    ),
                                                )
                                                .await;
                                                return;
                                            }
                                            let completed_content = (!content.trim().is_empty())
                                                .then_some(std::mem::take(&mut content));
                                            let continuation = if calls.is_empty() {
                                                None
                                            } else {
                                                let assistant_message = match chat_assistant_message(
                                                    completed_content.as_deref(),
                                                    &calls,
                                                ) {
                                                    Ok(message) => message,
                                                    Err(error) => {
                                                        fail_chat_stream(
                                                            &tx,
                                                            &mut completion_tx,
                                                            contextual_chat_error(
                                                                &provider,
                                                                &model,
                                                                "protocol failure",
                                                                &error,
                                                                &diagnostic_secrets,
                                                            ),
                                                        )
                                                        .await;
                                                        return;
                                                    }
                                                };
                                                match chat_continuation(
                                                    &provider,
                                                    &model,
                                                    continuation_scope.clone(),
                                                    assistant_message,
                                                    &calls,
                                                ) {
                                                    Ok(continuation) => Some(continuation),
                                                    Err(error) => {
                                                        fail_chat_stream(
                                                            &tx,
                                                            &mut completion_tx,
                                                            contextual_chat_error(
                                                                &provider,
                                                                &model,
                                                                "protocol failure",
                                                                &error,
                                                                &diagnostic_secrets,
                                                            ),
                                                        )
                                                        .await;
                                                        return;
                                                    }
                                                }
                                            };
                                            let completion = LlmResponse {
                                                terminal_status: LlmTerminalStatus::Completed,
                                                content: completed_content,
                                                tool_calls: (!calls.is_empty()).then_some(calls),
                                                finish_reason: *reason,
                                                continuation,
                                                usage: chunk_usage,
                                                attempts: retry_attempts,
                                            };
                                            if !crate::llm::send_stream_event(&tx, Ok(parsed)).await
                                            {
                                                return;
                                            }
                                            if canonical_openai {
                                                pending_completion = Some(completion);
                                                continue 'events;
                                            }
                                            if let Some(sender) = completion_tx.take() {
                                                let _ = sender.send(Ok(completion));
                                            }
                                            return;
                                        }
                                    }
                                    if !crate::llm::send_stream_event(&tx, Ok(parsed)).await {
                                        return;
                                    }
                                }
                            }
                            Err(_) => {
                                fail_chat_stream_conditionally(
                                    &tx,
                                    &mut completion_tx,
                                    contextual_chat_error(
                                        &provider,
                                        &model,
                                        "protocol failure",
                                        "stream contained malformed JSON",
                                        &diagnostic_secrets,
                                    ),
                                    pending_completion.is_some(),
                                )
                                .await;
                                return;
                            }
                        }
                    }
                    Err(EventStreamError::Transport(error)) => {
                        fail_chat_stream_conditionally(
                            &tx,
                            &mut completion_tx,
                            crate::llm::LlmError::transport(
                                &provider,
                                CHAT_TRANSPORT,
                                Some(&model),
                                contextual_chat_error(
                                    &provider,
                                    &model,
                                    "stream transport failed",
                                    &error,
                                    &diagnostic_secrets,
                                ),
                                &diagnostic_secrets,
                            )
                            .with_phase(crate::llm::LlmErrorPhase::Stream),
                            pending_completion.is_some(),
                        )
                        .await;
                        return;
                    }
                    Err(EventStreamError::Utf8(_) | EventStreamError::Parser(_)) => {
                        fail_chat_stream_conditionally(
                            &tx,
                            &mut completion_tx,
                            contextual_chat_error(
                                &provider,
                                &model,
                                "protocol failure",
                                "stream contained malformed SSE data",
                                &diagnostic_secrets,
                            ),
                            pending_completion.is_some(),
                        )
                        .await;
                        return;
                    }
                }
            }
            if !tx.is_closed() {
                let detail = if pending_completion.is_some() {
                    "canonical OpenAI stream ended before its [DONE] marker"
                } else {
                    "stream ended before an explicit finish_reason"
                };
                fail_chat_stream_conditionally(
                    &tx,
                    &mut completion_tx,
                    contextual_chat_error(
                        &provider,
                        &model,
                        "protocol failure",
                        detail,
                        &diagnostic_secrets,
                    ),
                    pending_completion.is_some(),
                )
                .await;
            }
        });

        Ok(LlmStream::with_private_completion(rx, completion_rx)
            .with_error_context(self.error_context(retry_attempts)))
    }
}

async fn fail_chat_stream(
    public_tx: &mpsc::Sender<Result<LlmStreamEvent, crate::llm::LlmStreamFailure>>,
    completion_tx: &mut Option<oneshot::Sender<Result<LlmResponse, crate::llm::LlmStreamFailure>>>,
    error: impl Into<crate::llm::LlmStreamFailure>,
) {
    let error = error.into();
    let _ = crate::llm::send_stream_event(public_tx, Err(error.clone())).await;
    if let Some(sender) = completion_tx.take() {
        let _ = sender.send(Err(error));
    }
}

fn fail_chat_private_completion(
    completion_tx: &mut Option<oneshot::Sender<Result<LlmResponse, crate::llm::LlmStreamFailure>>>,
    error: impl Into<crate::llm::LlmStreamFailure>,
) {
    if let Some(sender) = completion_tx.take() {
        let _ = sender.send(Err(error.into()));
    }
}

async fn fail_chat_stream_conditionally(
    public_tx: &mpsc::Sender<Result<LlmStreamEvent, crate::llm::LlmStreamFailure>>,
    completion_tx: &mut Option<oneshot::Sender<Result<LlmResponse, crate::llm::LlmStreamFailure>>>,
    error: impl Into<crate::llm::LlmStreamFailure>,
    public_terminal_emitted: bool,
) {
    if public_terminal_emitted {
        fail_chat_private_completion(completion_tx, error);
    } else {
        fail_chat_stream(public_tx, completion_tx, error).await;
    }
}

fn contextual_chat_error(
    provider: &str,
    model: &str,
    kind: &str,
    detail: &str,
    secrets: &[String],
) -> String {
    bounded_redacted(
        format!("{provider} Chat Completions API {kind} (model {model}): {detail}"),
        secrets,
    )
}

fn chat_stream_call_ids(data: &Value) -> Result<Vec<(usize, ProviderCallId)>, String> {
    let Some(tool_calls) = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("tool_calls"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    tool_calls
        .iter()
        .filter_map(|call| call.get("id").and_then(Value::as_str).map(|id| (call, id)))
        .map(|(call, id)| {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .ok_or_else(|| "streamed provider call ID is missing an index".to_string())?;
            ProviderCallId::new(id.to_string()).map(|id| (index, id))
        })
        .collect()
}

fn normalize_chat_finish_reason(reason: &str) -> Result<LlmFinishReason, String> {
    match reason {
        "stop" => Ok(LlmFinishReason::Complete),
        "tool_calls" => Ok(LlmFinishReason::ToolCalls),
        _ => Err("OpenAI-compatible response contained an unsupported finish_reason".to_string()),
    }
}

pub fn parse_openai_stream_chunk(data: &Value) -> Result<Vec<LlmStreamEvent>, String> {
    if let Some(failure) = chat_protocol_failure(data) {
        return Err(failure.detail(false));
    }
    let Some(choice) = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
    else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(content) = chat_text_content(delta.get("content")).or_else(|| {
        choice
            .get("message")
            .and_then(|message| chat_text_content(message.get("content")))
    }) {
        events.push(LlmStreamEvent::Delta(LlmDelta::Content(content)));
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        crate::llm::ensure_response_item_count(tool_calls.len(), "OpenAI streamed tool calls")?;
        for call in tool_calls {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| "OpenAI tool-call delta is missing index".to_string())?
                as usize;
            let function = call.get("function").unwrap_or(&Value::Null);
            events.push(LlmStreamEvent::Delta(LlmDelta::ToolCallChunk {
                index,
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                arguments: function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }));
        }
    }
    if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
        if !finish.is_empty() && finish != "null" {
            events.push(LlmStreamEvent::Terminal(normalize_chat_finish_reason(
                finish,
            )?));
        }
    }
    Ok(events)
}

fn required_chat_usage_count(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<u64, String> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("OpenAI usage {field} must be a non-negative integer"))
}

fn optional_chat_usage_detail(
    usage: &serde_json::Map<String, Value>,
    details_field: &str,
    count_field: &str,
) -> Result<Option<u64>, String> {
    let Some(details) = usage.get(details_field).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let details = details
        .as_object()
        .ok_or_else(|| format!("OpenAI usage {details_field} must be an object"))?;
    match details.get(count_field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            format!("OpenAI usage {details_field}.{count_field} must be a non-negative integer")
        }),
    }
}

fn parse_openai_usage(data: &Value) -> Result<Option<LlmUsage>, String> {
    let Some(usage) = data.get("usage").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let usage = usage
        .as_object()
        .ok_or_else(|| "OpenAI usage must be an object".to_string())?;
    let input = required_chat_usage_count(usage, "prompt_tokens")
        .or_else(|_| required_chat_usage_count(usage, "input_tokens"))?;
    let visible_output = required_chat_usage_count(usage, "completion_tokens")
        .or_else(|_| required_chat_usage_count(usage, "output_tokens"))?;
    let total = required_chat_usage_count(usage, "total_tokens")?;
    let cached = match optional_chat_usage_detail(usage, "prompt_tokens_details", "cached_tokens")?
    {
        Some(cached) => Some(cached),
        None => optional_chat_usage_detail(usage, "input_tokens_details", "cached_tokens")?,
    };
    let reasoning =
        match optional_chat_usage_detail(usage, "completion_tokens_details", "reasoning_tokens")? {
            Some(reasoning) => Some(reasoning),
            None => optional_chat_usage_detail(usage, "output_tokens_details", "reasoning_tokens")?,
        };
    let output = match input.checked_add(visible_output) {
        Some(sum) if sum == total => visible_output,
        Some(sum) => match reasoning {
            Some(reasoning) if sum.checked_add(reasoning) == Some(total) => visible_output
                .checked_add(reasoning)
                .ok_or_else(|| "OpenAI usage overflowed".to_string())?,
            _ => {
                return Err("OpenAI usage total does not match input plus output".to_string());
            }
        },
        None => return Err("OpenAI usage overflowed".to_string()),
    };
    LlmUsage::new(input, output, total, cached, reasoning)
        .map(Some)
        .map_err(|error| format!("OpenAI usage is invalid: {error}"))
}

pub fn parse_openai_response(data: &Value) -> Result<LlmResponse, String> {
    if let Some(failure) = chat_protocol_failure(data) {
        return Err(failure.detail(false));
    }
    let choice = &data["choices"][0];
    let message = &choice["message"];
    let content = chat_text_content(message.get("content"));

    let mut tool_calls = Vec::new();
    if let Some(tcs) = message.get("tool_calls").and_then(|v| v.as_array()) {
        crate::llm::ensure_response_item_count(tcs.len(), "OpenAI tool calls")?;
        for tc in tcs {
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
            if name.is_empty() {
                return Err("OpenAI tool call is missing a function name".to_string());
            }
            let arguments: Value = serde_json::from_str(args_str)
                .map_err(|error| format!("invalid OpenAI tool arguments: {error}"))?;
            let call_id = tc
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| "OpenAI tool call is missing its call ID".to_string())?;
            tool_calls.push(ToolCallRequest::with_provider_call(
                ProviderCallId::new(call_id)?,
                name,
                arguments,
            ));
        }
    }

    let native_finish = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .filter(|reason| !reason.is_empty() && *reason != "null")
        .ok_or_else(|| "OpenAI response is missing finish_reason".to_string())?;
    let finish_reason = normalize_chat_finish_reason(native_finish)?;
    if (!tool_calls.is_empty() && finish_reason != LlmFinishReason::ToolCalls)
        || (tool_calls.is_empty() && finish_reason == LlmFinishReason::ToolCalls)
    {
        return Err("OpenAI finish_reason did not match the completed tool-call set".to_string());
    }

    Ok(LlmResponse {
        terminal_status: LlmTerminalStatus::Completed,
        content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        finish_reason,
        continuation: None,
        usage: parse_openai_usage(data)?,
        attempts: crate::llm::RetryAttemptMetadata::no_network_attempt(),
    })
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
