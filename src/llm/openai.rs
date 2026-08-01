//! OpenAI-compatible chat completions (OpenAI, Grok, OpenRouter).

use crate::config::ReasoningEffort;
use crate::llm::types::{
    LlmRequest, LlmRequestScope, LlmResponse, LlmTerminalStatus, ProviderCallId,
    ProviderContinuation, StreamEvent, ToolCallAccumulator, ToolCallRequest,
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
    let (state, outputs): (ChatTurnState, BTreeMap<ToolInvocationId, String>) =
        continuation.consume(provider, model, CHAT_TRANSPORT, scope)?;
    let mut messages = vec![state.assistant_message];
    for (invocation_id, call_id) in state.calls {
        let output = outputs
            .get(&invocation_id)
            .ok_or_else(|| "Chat continuation is missing a tool output".to_string())?;
        messages.push(json!({
            "role": "tool",
            "tool_call_id": call_id.as_str(),
            "content": output,
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
        mut diagnostic_secrets: Vec<String>,
        base_url: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        diagnostic_secrets.extend(api_keys.iter().cloned());
        diagnostic_secrets
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        diagnostic_secrets.dedup();
        Self {
            client: Client::new(),
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

    fn request_body(&self, request: LlmRequest<'_>, stream: bool) -> Result<Value, String> {
        let continuation_messages = request
            .continuation
            .map(|continuation| {
                into_chat_messages(
                    continuation,
                    &self.provider,
                    &self.model,
                    request.scope.as_ref(),
                )
            })
            .transpose()?
            .unwrap_or_default();
        let mut messages = request.messages.to_vec();
        messages.extend(continuation_messages);

        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "temperature": request.temperature,
        });
        if stream {
            body["stream"] = json!(true);
        }
        if let Some(tools) = request.tools {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
        if let Some(effort) = request.reasoning_effort.or(self.reasoning_effort) {
            body["reasoning_effort"] = json!(effort.as_str());
        }
        Ok(body)
    }

    fn requests_tools_with_reasoning(&self, request: &LlmRequest<'_>) -> bool {
        request.tools.is_some_and(|tools| !tools.is_empty())
            && request
                .reasoning_effort
                .or(self.reasoning_effort)
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

    fn request_error(&self, error: &str) -> String {
        let safe_credential_error = error == "provider request requires at least one credential"
            || (error.starts_with("all ") && error.contains("credential(s) exhausted"));
        let detail = if safe_credential_error {
            error
        } else {
            "request failed before a valid HTTP response"
        };
        self.contextual_error("request failed", detail)
    }

    async fn http_error(
        &self,
        status: StatusCode,
        response: reqwest::Response,
        requests_tools_with_reasoning: bool,
    ) -> String {
        let body = crate::llm::read_bounded_error_response(
            response,
            "OpenAI-compatible API error response",
        )
        .await;
        let detail = match body {
            Ok(body) => structured_chat_error_detail(&body),
            Err(error) if error.contains("byte limit") => error,
            Err(_) => "provider error response could not be read".to_string(),
        };
        let guidance = (requests_tools_with_reasoning
            && matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
            ))
        .then_some(CHAT_REASONING_TOOL_GUIDANCE)
        .unwrap_or_default();
        self.contextual_error(
            &format!("error HTTP {}", status.as_u16()),
            &format!("{detail}{guidance}"),
        )
    }

    fn completion_read_error(&self, error: &str) -> String {
        let detail = if error.contains("byte limit") || error.starts_with("invalid ") {
            error
        } else {
            "completion response could not be read"
        };
        self.contextual_error("protocol failure", detail)
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
                .any(|refusal| {
                    !refusal.is_null()
                        && refusal
                            .as_str()
                            .is_none_or(|refusal| !refusal.trim().is_empty())
                })
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

#[async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, String> {
        let requests_tools_with_reasoning = self.requests_tools_with_reasoning(&request);
        let scope = request.scope.clone();
        let body = self
            .request_body(request, false)
            .map_err(|error| self.contextual_error("request rejected", &error))?;
        let url = self.endpoint();

        let resp = crate::llm::send_with_retry(
            |credential_index| {
                let mut request = self.client.post(&url).json(&body);
                request = request.bearer_auth(&self.api_keys[credential_index]);
                request
            },
            self.api_keys.len(),
        )
        .await
        .map_err(|error| self.request_error(&error))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(self
                .http_error(status, resp, requests_tools_with_reasoning)
                .await);
        }

        let data =
            crate::llm::read_bounded_json_response(resp, "OpenAI-compatible completion response")
                .await
                .map_err(|error| self.completion_read_error(&error))?;
        if let Some(failure) = chat_protocol_failure(&data) {
            return Err(self.contextual_error(
                failure.context_kind(),
                &failure.detail(requests_tools_with_reasoning),
            ));
        }
        let mut response = parse_openai_response(&data).map_err(|_| {
            self.contextual_error(
                "protocol failure",
                "completion response did not satisfy the Chat Completions schema",
            )
        })?;
        attach_chat_continuation(&mut response, &data, &self.provider, &self.model, scope)
            .map_err(|error| self.contextual_error("protocol failure", &error))?;
        Ok(response)
    }

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, String> {
        let requests_tools_with_reasoning = self.requests_tools_with_reasoning(&request);
        let scope = request.scope.clone();
        let body = self
            .request_body(request, true)
            .map_err(|error| self.contextual_error("request rejected", &error))?;
        let url = self.endpoint();

        let resp = crate::llm::send_with_retry(
            |credential_index| {
                let mut request = self.client.post(&url).json(&body);
                request = request.bearer_auth(&self.api_keys[credential_index]);
                request
            },
            self.api_keys.len(),
        )
        .await
        .map_err(|error| self.request_error(&error))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(self
                .http_error(status, resp, requests_tools_with_reasoning)
                .await);
        }
        crate::llm::ensure_response_content_length(
            &resp,
            crate::llm::MAX_LLM_STREAM_BYTES,
            "OpenAI-compatible stream response",
        )
        .map_err(|error| self.contextual_error("protocol failure", &error))?;

        let provider = self.provider.clone();
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
            while let Some(event_res) =
                crate::llm::next_stream_item_or_closed(&tx, &mut stream).await
            {
                match event_res {
                    Ok(event) => {
                        if let Err(error) = crate::llm::ensure_stream_event_size(
                            event.data.len(),
                            "OpenAI-compatible stream event",
                        ) {
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
                        if event.event == "error" {
                            let failure = serde_json::from_str::<Value>(&event.data)
                                .ok()
                                .and_then(|data| chat_protocol_failure(&data))
                                .unwrap_or_else(ChatProtocolFailure::in_band_error);
                            fail_chat_stream(
                                &tx,
                                &mut completion_tx,
                                contextual_chat_error(
                                    &provider,
                                    &model,
                                    failure.context_kind(),
                                    &failure.detail(requests_tools_with_reasoning),
                                    &diagnostic_secrets,
                                ),
                            )
                            .await;
                            return;
                        }
                        if event.data == "[DONE]" {
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
                            return;
                        }

                        match serde_json::from_str::<Value>(&event.data) {
                            Ok(data) => {
                                if let Some(failure) = chat_protocol_failure(&data) {
                                    fail_chat_stream(
                                        &tx,
                                        &mut completion_tx,
                                        contextual_chat_error(
                                            &provider,
                                            &model,
                                            failure.context_kind(),
                                            &failure.detail(requests_tools_with_reasoning),
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
                                for parsed in parsed_events {
                                    match &parsed {
                                        StreamEvent::Content(fragment) => {
                                            content.push_str(fragment)
                                        }
                                        StreamEvent::ToolCallChunk { .. } => {
                                            tool_calls.push(&parsed)
                                        }
                                        StreamEvent::End(reason) => {
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
                                            if (!calls.is_empty() && reason != "tool_calls")
                                                || (calls.is_empty() && reason == "tool_calls")
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
                                                    continuation_scope,
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
                                                finish_reason: reason.clone(),
                                                continuation,
                                            };
                                            if !crate::llm::send_stream_event(&tx, Ok(parsed)).await
                                            {
                                                return;
                                            }
                                            if let Some(sender) = completion_tx.take() {
                                                let _ = sender.send(Ok(completion));
                                            }
                                            return;
                                        }
                                        _ => {}
                                    }
                                    if !crate::llm::send_stream_event(&tx, Ok(parsed)).await {
                                        return;
                                    }
                                }
                            }
                            Err(_) => {
                                fail_chat_stream(
                                    &tx,
                                    &mut completion_tx,
                                    contextual_chat_error(
                                        &provider,
                                        &model,
                                        "protocol failure",
                                        "stream contained malformed JSON",
                                        &diagnostic_secrets,
                                    ),
                                )
                                .await;
                                return;
                            }
                        }
                    }
                    Err(EventStreamError::Transport(error)) => {
                        fail_chat_stream(
                            &tx,
                            &mut completion_tx,
                            contextual_chat_error(
                                &provider,
                                &model,
                                "stream transport failed",
                                &error,
                                &diagnostic_secrets,
                            ),
                        )
                        .await;
                        return;
                    }
                    Err(EventStreamError::Utf8(_) | EventStreamError::Parser(_)) => {
                        fail_chat_stream(
                            &tx,
                            &mut completion_tx,
                            contextual_chat_error(
                                &provider,
                                &model,
                                "protocol failure",
                                "stream contained malformed SSE data",
                                &diagnostic_secrets,
                            ),
                        )
                        .await;
                        return;
                    }
                }
            }
            if !tx.is_closed() {
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
        });

        Ok(LlmStream::with_private_completion(rx, completion_rx))
    }
}

async fn fail_chat_stream(
    public_tx: &mpsc::Sender<Result<StreamEvent, String>>,
    completion_tx: &mut Option<oneshot::Sender<Result<LlmResponse, String>>>,
    error: String,
) {
    let _ = crate::llm::send_stream_event(public_tx, Err(error.clone())).await;
    if let Some(sender) = completion_tx.take() {
        let _ = sender.send(Err(error));
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

pub fn parse_openai_stream_chunk(data: &Value) -> Result<Vec<StreamEvent>, String> {
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
    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        events.push(StreamEvent::Content(content.to_string()));
    }
    if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| "OpenAI tool-call delta is missing index".to_string())?
                as usize;
            let function = call.get("function").unwrap_or(&Value::Null);
            events.push(StreamEvent::ToolCallChunk {
                index,
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                arguments: function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
        if !finish.is_empty() && finish != "null" {
            events.push(StreamEvent::End(finish.to_string()));
        }
    }
    Ok(events)
}

pub fn parse_openai_response(data: &Value) -> Result<LlmResponse, String> {
    if let Some(failure) = chat_protocol_failure(data) {
        return Err(failure.detail(false));
    }
    let choice = &data["choices"][0];
    let message = &choice["message"];
    let content = message
        .get("content")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());

    let mut tool_calls = Vec::new();
    if let Some(tcs) = message.get("tool_calls").and_then(|v| v.as_array()) {
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

    let finish = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .filter(|reason| !reason.is_empty() && *reason != "null")
        .ok_or_else(|| "OpenAI response is missing finish_reason".to_string())?
        .to_string();
    if (!tool_calls.is_empty() && finish != "tool_calls")
        || (tool_calls.is_empty() && finish == "tool_calls")
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
        finish_reason: finish,
        continuation: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::{
        serve_once, serve_once_with_declared_length, serve_open_stream,
    };
    use std::time::Duration;

    #[test]
    fn parses_streamed_text_and_tool_fragments() {
        let events = parse_openai_stream_chunk(&json!({
            "choices": [{
                "delta": {
                    "content": "working",
                    "tool_calls": [{
                        "index": 0,
                        "function": {"name": "read_file", "arguments": "{\"path\":"}
                    }]
                },
                "finish_reason": null
            }]
        }))
        .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::Content(value) if value == "working"));
        assert!(matches!(
            &events[1],
            StreamEvent::ToolCallChunk { index: 0, name: Some(name), .. } if name == "read_file"
        ));
    }

    #[test]
    fn complete_parser_rejects_unsafe_and_unknown_terminal_states() {
        for (finish_reason, expected_detail) in [
            ("length", "truncated"),
            ("content_filter", "safety controls"),
            ("function_call", "unsupported finish_reason"),
            ("remote-secret-terminal", "unsupported finish_reason"),
            ("error", "in-band error"),
        ] {
            let error = parse_openai_response(&json!({
                "choices": [{
                    "message": {"content": "not authorized"},
                    "finish_reason": finish_reason
                }]
            }))
            .expect_err("unsafe terminal state must fail");
            assert!(error.contains(expected_detail));
            assert!(!error.contains("remote-secret-terminal"));
        }

        let missing = parse_openai_response(&json!({
            "choices": [{"message": {"content": "not authorized"}}]
        }))
        .expect_err("missing terminal state must fail");
        assert!(missing.contains("missing finish_reason"));
    }

    #[test]
    fn complete_and_stream_parsers_reject_provider_errors_and_refusals() {
        for response in [
            json!({
                "error": {"message": "remote-secret-error"},
                "choices": []
            }),
            json!({
                "choices": [{
                    "error": {"message": "remote-secret-error"},
                    "message": {"content": "not authorized"},
                    "finish_reason": "stop"
                }]
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": null,
                        "refusal": "remote-secret-refusal"
                    },
                    "finish_reason": "stop"
                }]
            }),
        ] {
            let complete_error = parse_openai_response(&response)
                .expect_err("provider failure must not become a completed response");
            let stream_error = parse_openai_stream_chunk(&response)
                .expect_err("provider failure must not become a stream event");
            for error in [complete_error, stream_error] {
                assert!(!error.contains("remote-secret"));
                assert!(error.contains("in-band error") || error.contains("refusal"));
            }
        }
    }

    #[test]
    fn chat_request_construction_is_provider_and_model_name_agnostic() {
        let messages = [json!({"role": "user", "content": "inspect"})];
        let tools = [json!({
            "type": "function",
            "function": {"name": "read_file"}
        })];
        for (provider, model, base_url, expected_endpoint) in [
            (
                "openai",
                "gpt-3.5-legacy",
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "openai",
                "gateway-unknown-model",
                "https://gateway.example/compatible/v1",
                "https://gateway.example/compatible/v1/chat/completions",
            ),
            (
                "openrouter",
                "unrecognized/model",
                "https://openrouter.ai/api/v1",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
        ] {
            let client = OpenAiCompatClient::configured(
                provider.to_string(),
                model.to_string(),
                vec!["fixture-key".to_string()],
                base_url,
                None,
            );
            assert_eq!(client.endpoint(), expected_endpoint);
            let body = client
                .request_body(LlmRequest::new(&messages, Some(&tools), 0.2), false)
                .expect("valid Chat request");
            assert_eq!(body["model"], model);
            assert!(body.get("messages").is_some());
            assert!(body.get("tools").is_some());
            assert!(body.get("input").is_none());
            assert!(body.get("store").is_none());
            assert!(body.get("include").is_none());
        }
    }

    #[tokio::test]
    async fn complete_posts_chat_payload_and_parses_tool_calls() {
        let (base_url, request_rx) = serve_once(
            "200 OK",
            "application/json",
            json!({
                "choices": [{
                    "message": {
                        "content": "inspected",
                        "tool_calls": [{
                            "id": "call_read_1",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\":\"README.md\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            })
            .to_string(),
        );
        let client = OpenAiCompatClient::new(
            "test-model".to_string(),
            vec!["test-key".to_string()],
            base_url,
        );
        let response = client
            .complete(LlmRequest::new(
                &[json!({"role": "user", "content": "inspect"})],
                Some(&[json!({"type": "function", "function": {"name": "read_file"}})]),
                0.2,
            ))
            .await
            .expect("OpenAI completion");

        assert_eq!(response.content.as_deref(), Some("inspected"));
        assert_eq!(response.finish_reason, "tool_calls");
        let call = &response.tool_calls.expect("tool call")[0];
        assert_eq!(call.call_id.as_ref().unwrap().as_str(), "call_read_1");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments["path"], "README.md");

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        assert!(request.starts_with("POST /chat/completions HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key"));
        assert!(request.contains("\"model\":\"test-model\""));
        assert!(request.contains("\"tool_choice\":\"auto\""));
    }

    #[tokio::test]
    async fn http_200_error_envelope_is_safe_and_keeps_reasoning_tool_guidance() {
        let remote_secret = "remote-provider-detail-secret";
        let (base_url, _) = serve_once(
            "200 OK",
            "application/json",
            json!({
                "error": {
                    "message": format!(
                        "Function tools with reasoning_effort failed: {remote_secret}"
                    ),
                    "param": "reasoning_effort"
                }
            })
            .to_string(),
        );
        let client = OpenAiCompatClient::configured(
            "openrouter".to_string(),
            "provider/model".to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            Some(ReasoningEffort::Medium),
        );
        let messages = [json!({"role": "user", "content": "plan"})];
        let tools = [json!({
            "type": "function",
            "function": {"name": "submit_plan", "parameters": {"type": "object"}}
        })];

        let error = client
            .complete(LlmRequest::new(&messages, Some(&tools), 0.0))
            .await
            .expect_err("HTTP-200 provider error must fail");

        assert!(error.contains("provider failure"));
        assert!(error.contains("in-band error"));
        assert!(error.contains("api = \"responses\""));
        assert!(error.contains("reasoning_effort = \"none\""));
        assert!(!error.contains(remote_secret));
    }

    #[tokio::test]
    async fn chat_reasoning_tool_rejection_is_actionable_redacted_and_not_retried() {
        let (base_url, request_rx) = serve_once(
            "400 Bad Request",
            "application/json",
            json!({
                "error": {
                    "message": "Function tools with reasoning_effort are not supported for gpt-5.6-luna in /v1/chat/completions; credential incident-secret",
                    "type": "invalid_request_error",
                    "param": "reasoning_effort"
                }
            })
            .to_string(),
        );
        let client = OpenAiCompatClient::configured(
            "openai".to_string(),
            "gpt-5.6-luna".to_string(),
            vec!["incident-secret".to_string()],
            format!("{base_url}/v1"),
            Some(ReasoningEffort::Medium),
        );
        let messages = [json!({"role": "user", "content": "plan"})];
        let tools = [json!({
            "type": "function",
            "function": {"name": "submit_plan", "parameters": {"type": "object"}}
        })];

        let error = client
            .complete(LlmRequest::new(&messages, Some(&tools), 0.3))
            .await
            .expect_err("reported Chat tuple must fail");

        assert!(error.contains("openai Chat Completions API error"));
        assert!(error.contains("model gpt-5.6-luna"));
        assert!(error.contains("api = \"responses\""));
        assert!(error.contains("reasoning_effort = \"none\""));
        assert!(!error.contains("incident-secret"));
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("single captured request");
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(request.contains("\"reasoning_effort\":\"medium\""));
        assert!(request.contains("\"tools\""));
        assert!(!request.contains("\"input\""));
        assert!(!request.contains("\"store\""));
    }

    #[tokio::test]
    async fn stream_posts_stream_flag_and_accumulates_text_and_tools() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"work\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_private_chat\",\"function\":{\"name\":\"grep\",\"arguments\":\"{\\\"pattern\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"needle\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", body);
        let client = OpenAiCompatClient::new(
            "stream-model".to_string(),
            vec!["stream-key".to_string()],
            base_url,
        );
        let mut stream = client
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "search"})],
                None,
                0.1,
            ))
            .await
            .expect("OpenAI stream");
        let mut content = String::new();
        let mut accumulator = crate::llm::ToolCallAccumulator::default();
        let mut finish = None;
        while let Some(event) = stream.recv().await {
            let event = event.expect("valid stream event");
            if let StreamEvent::Content(fragment) = &event {
                content.push_str(fragment);
            }
            if let StreamEvent::End(reason) = &event {
                finish = Some(reason.clone());
            }
            accumulator.push(&event);
        }

        assert_eq!(content, "work");
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        let calls = accumulator.finish().expect("complete tool call");
        assert!(
            calls[0].call_id.is_none(),
            "public projection exposed a call ID"
        );
        assert_eq!(calls[0].name, "grep");
        assert_eq!(calls[0].arguments["pattern"], "needle");
        let private = stream.finish().await.expect("private terminal completion");
        let private_call = &private.tool_calls.expect("private tool call")[0];
        assert_eq!(
            private_call.call_id.as_ref().unwrap().as_str(),
            "call_private_chat"
        );
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured stream request");
        assert!(request.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn streaming_in_band_errors_fail_without_remote_detail() {
        let remote_secret = "remote-stream-error-secret";
        for body in [
            format!(
                "data: {{\"error\":{{\"message\":\"{remote_secret}\"}}}}\n\n"
            ),
            format!(
                "data: {{\"choices\":[{{\"error\":{{\"message\":\"{remote_secret}\"}},\"delta\":{{}},\"finish_reason\":null}}]}}\n\n"
            ),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"error\"}]}\n\n"
                .to_string(),
            format!("event: error\ndata: {{\"message\":\"{remote_secret}\"}}\n\n"),
        ] {
            let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
            let client = OpenAiCompatClient::configured(
                "openrouter".to_string(),
                "provider/model".to_string(),
                vec!["fixture-key".to_string()],
                base_url,
                None,
            );
            let messages = [json!({"role": "user", "content": "stream"})];
            let error = client
                .stream(LlmRequest::new(&messages, None, 0.0))
                .await
                .expect("HTTP stream response")
                .finish()
                .await
                .expect_err("in-band stream error must fail");
            assert!(error.contains("in-band error"));
            assert!(!error.contains(remote_secret));
        }
    }

    #[tokio::test]
    async fn streaming_reasoning_tool_error_keeps_only_local_guidance() {
        let remote_secret = "remote-stream-reasoning-secret";
        let body = format!(
            "data: {{\"error\":{{\"message\":\"Function tools with reasoning_effort failed: {remote_secret}\",\"param\":\"reasoning_effort\"}}}}\n\n"
        );
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let client = OpenAiCompatClient::configured(
            "openrouter".to_string(),
            "provider/model".to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            Some(ReasoningEffort::Medium),
        );
        let messages = [json!({"role": "user", "content": "stream"})];
        let tools = [json!({
            "type": "function",
            "function": {"name": "submit_plan", "parameters": {"type": "object"}}
        })];
        let error = client
            .stream(LlmRequest::new(&messages, Some(&tools), 0.0))
            .await
            .expect("HTTP stream response")
            .finish()
            .await
            .expect_err("reasoning/tool error must fail");

        assert!(error.contains("api = \"responses\""));
        assert!(error.contains("reasoning_effort = \"none\""));
        assert!(!error.contains(remote_secret));
    }

    #[tokio::test]
    async fn streaming_unsafe_and_unknown_terminal_states_fail_closed() {
        for (finish_reason, expected_detail) in [
            ("length", "truncated"),
            ("content_filter", "safety controls"),
            ("remote-secret-terminal", "unsupported finish_reason"),
        ] {
            let body = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"partial\"}},\"finish_reason\":\"{finish_reason}\"}}]}}\n\n"
            );
            let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
            let client = OpenAiCompatClient::new(
                "stream-model".to_string(),
                vec!["stream-key".to_string()],
                base_url,
            );
            let messages = [json!({"role": "user", "content": "stream"})];
            let error = client
                .stream(LlmRequest::new(&messages, None, 0.0))
                .await
                .expect("HTTP stream response")
                .finish()
                .await
                .expect_err("unsafe terminal state must fail");
            assert!(error.contains(expected_detail));
            assert!(!error.contains("remote-secret-terminal"));
        }
    }

    #[tokio::test]
    async fn dropping_stream_receiver_closes_the_open_http_response() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(
            "data: {\"choices\":[{\"delta\":{\"content\":\"first\"},\"finish_reason\":null}]}\n\n",
        );
        let client = OpenAiCompatClient::new(
            "stream-model".to_string(),
            vec!["stream-key".to_string()],
            base_url,
        );
        let mut stream = client
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "cancel"})],
                None,
                0.0,
            ))
            .await
            .expect("open OpenAI stream");
        let first = tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("first event timeout")
            .expect("first event")
            .expect("valid first event");
        assert_eq!(first, StreamEvent::Content("first".to_string()));
        drop(stream);

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "OpenAI response connection remained open");
    }

    #[tokio::test]
    async fn done_sentinel_without_finish_reason_fails_and_closes_open_response() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"late\"},\"finish_reason\":null}]}\n\n"
        ));
        let client = OpenAiCompatClient::new(
            "stream-model".to_string(),
            vec!["stream-key".to_string()],
            base_url,
        );
        let mut stream = client
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "finish"})],
                None,
                0.0,
            ))
            .await
            .expect("open OpenAI stream");

        let error = tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("terminal event timeout")
            .expect("terminal event")
            .expect_err("DONE without finish_reason must fail");
        assert!(error.contains("explicit finish_reason"));
        assert!(tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("producer closure timeout")
            .is_none());

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "OpenAI terminal event left response open");
    }

    #[tokio::test]
    async fn done_sentinel_without_finish_reason_cannot_authorize_completion() {
        let (base_url, _) = serve_once("200 OK", "text/event-stream", "data: [DONE]\n\n");
        let client = OpenAiCompatClient::new(
            "stream-model".to_string(),
            vec!["stream-key".to_string()],
            base_url,
        );
        let stream = client
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "finish"})],
                None,
                0.0,
            ))
            .await
            .expect("OpenAI stream");
        let error = stream
            .finish()
            .await
            .expect_err("typed finish reason required");
        assert!(error.contains("explicit finish_reason"));
    }

    #[tokio::test]
    async fn partial_tool_delta_followed_by_eof_cannot_authorize_a_tool() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_partial\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"unsafe.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n"
        );
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let client = OpenAiCompatClient::new(
            "stream-model".to_string(),
            vec!["stream-key".to_string()],
            base_url,
        );
        let messages = [json!({"role": "user", "content": "write"})];
        let stream = client
            .stream(LlmRequest::new(&messages, None, 0.0))
            .await
            .expect("OpenAI stream");
        let error = stream
            .finish()
            .await
            .expect_err("partial public tool delta must not authorize execution");
        assert!(error.contains("explicit finish_reason"));
    }

    #[tokio::test]
    async fn complete_and_stream_surface_http_errors() {
        let (base_url, _) = serve_once("401 Unauthorized", "text/plain", "bad key");
        let client =
            OpenAiCompatClient::new("model".to_string(), vec!["bad".to_string()], base_url);
        let error = client
            .complete(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect_err("completion must reject HTTP errors");
        assert!(error.contains("Chat Completions API error HTTP 401"));
        assert!(!error.contains("bad key"));

        let (base_url, _) = serve_once("403 Forbidden", "text/plain", "forbidden");
        let client =
            OpenAiCompatClient::new("model".to_string(), vec!["bad".to_string()], base_url);
        let error = client
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect_err("stream must reject HTTP errors");
        assert!(error.contains("Chat Completions API error HTTP 403"));
        assert!(!error.contains("forbidden"));
    }

    #[tokio::test]
    async fn http_errors_omit_prompt_echo_and_redact_full_context() {
        let secret = "env-only-context-secret";
        let prompt_echo = "user-prompt-that-must-not-be-diagnostic";
        let (base_url, _) = serve_once(
            "400 Bad Request",
            "application/json",
            json!({
                "error": {
                    "type": prompt_echo,
                    "code": prompt_echo,
                    "param": format!("reasoning_effort-{prompt_echo}"),
                    "message": format!("Function tools with reasoning_effort failed: {prompt_echo}\n\u{1b}[31m")
                }
            })
            .to_string(),
        );
        let client = OpenAiCompatClient::configured(
            format!("gateway-{secret}\n"),
            format!("model-{secret}\u{1b}"),
            vec![secret.to_string()],
            format!("{base_url}/{secret}"),
            Some(ReasoningEffort::Medium),
        );
        let messages = [json!({"role": "user", "content": prompt_echo})];
        let tools = [json!({
            "type": "function",
            "function": {"name": "submit_plan", "parameters": {"type": "object"}}
        })];
        let error = client
            .complete(LlmRequest::new(&messages, Some(&tools), 0.0))
            .await
            .expect_err("HTTP error");
        assert!(error.contains("[REDACTED]"));
        assert!(error.contains("api = \"responses\""));
        assert!(!error.contains(secret));
        assert!(!error.contains(prompt_echo));
        assert!(!error.contains('\n'));
        assert!(!error.contains('\u{1b}'));
        assert!(error.len() <= MAX_DIAGNOSTIC_BYTES);
    }

    #[tokio::test]
    async fn oversized_http_errors_keep_chat_context() {
        let (base_url, _) = serve_once(
            "400 Bad Request",
            "text/plain",
            "x".repeat(crate::llm::MAX_LLM_ERROR_RESPONSE_BYTES + 1),
        );
        let client = OpenAiCompatClient::configured(
            "openai".to_string(),
            "gpt-test".to_string(),
            vec!["key".to_string()],
            base_url,
            None,
        );
        let messages = [json!({"role": "user", "content": "inspect"})];
        let error = client
            .complete(LlmRequest::new(&messages, None, 0.0))
            .await
            .expect_err("oversized provider error");
        assert!(error.contains("openai Chat Completions API error HTTP 400"));
        assert!(error.contains("65536-byte limit"));
        assert!(error.len() <= MAX_DIAGNOSTIC_BYTES);
    }

    #[tokio::test]
    async fn oversized_declared_response_lengths_are_rejected() {
        let (base_url, _) = serve_once_with_declared_length(
            "200 OK",
            "application/json",
            "{}",
            Some(crate::llm::MAX_LLM_COMPLETE_RESPONSE_BYTES + 1),
        );
        let client =
            OpenAiCompatClient::new("model".to_string(), vec!["key".to_string()], base_url);
        let error = client
            .complete(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect_err("oversized completion must be rejected");
        assert!(error.contains("4194304-byte limit"));

        let (base_url, _) = serve_once_with_declared_length(
            "200 OK",
            "text/event-stream",
            "",
            Some(crate::llm::MAX_LLM_STREAM_BYTES + 1),
        );
        let client =
            OpenAiCompatClient::new("model".to_string(), vec!["key".to_string()], base_url);
        let error = client
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect_err("oversized stream must be rejected");
        assert!(error.contains("16777216-byte limit"));
    }
}
