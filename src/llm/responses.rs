//! OpenAI-compatible Responses API transport.

use crate::config::ReasoningEffort;
use crate::llm::types::{
    LlmDelta, LlmFinishReason, LlmMessage, LlmRequest, LlmRequestScope, LlmResponse,
    LlmStreamEvent, LlmTerminalStatus, LlmUsage, ProviderCallId, ProviderContinuation,
    ToolCallRequest, ToolDefinition, ToolResult, MAX_CONTINUATION_BYTES, MAX_CONTINUATION_ITEMS,
};
use crate::llm::{
    ensure_response_content_length, ensure_stream_event_size, next_stream_item_or_closed,
    read_bounded_error_response, read_bounded_json_response, send_stream_event, send_with_retry,
    LlmClient, LlmStream, ResponseByteBudget, MAX_LLM_STREAM_BYTES,
};
use crate::tools::ToolInvocationId;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::{mpsc, oneshot};

const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const RESPONSES_TRANSPORT: &str = "responses";

struct ResponsesTurnState {
    output_items: Vec<Value>,
    calls: Vec<(ToolInvocationId, ProviderCallId)>,
}

fn responses_continuation(
    provider: &str,
    model: &str,
    scope: Option<LlmRequestScope>,
    output_items: Vec<Value>,
    calls: Vec<(ToolInvocationId, ProviderCallId)>,
) -> Result<ProviderContinuation, String> {
    let continuation_binding = calls
        .first()
        .and_then(|(_, call_id)| call_id.continuation_binding())
        .ok_or_else(|| {
            "Responses continuation requires provenance-bound function call IDs".to_string()
        })?;
    if calls
        .iter()
        .any(|(_, call_id)| call_id.continuation_binding() != Some(continuation_binding))
    {
        return Err(
            "Responses continuation function call IDs have inconsistent provenance".to_string(),
        );
    }
    let unique_call_ids = calls
        .iter()
        .map(|(_, call_id)| call_id.as_str())
        .collect::<BTreeSet<_>>();
    if unique_call_ids.len() != calls.len() {
        return Err("Responses continuation contains duplicate function call IDs".to_string());
    }
    let encoded_bytes = serde_json::to_vec(&output_items)
        .map_err(|error| format!("failed to measure Responses continuation: {error}"))?
        .len();
    let pending_invocations = calls
        .iter()
        .map(|(invocation_id, _)| *invocation_id)
        .collect();
    ProviderContinuation::new(
        provider,
        model,
        RESPONSES_TRANSPORT,
        scope,
        pending_invocations,
        output_items.len(),
        encoded_bytes,
        ResponsesTurnState {
            output_items,
            calls,
        },
    )
}

fn into_responses_input(
    continuation: ProviderContinuation,
    provider: &str,
    model: &str,
    scope: Option<&LlmRequestScope>,
) -> Result<Vec<Value>, String> {
    let (state, outputs): (ResponsesTurnState, BTreeMap<ToolInvocationId, ToolResult>) =
        continuation.consume(provider, model, RESPONSES_TRANSPORT, scope)?;
    let mut input = state.output_items;
    for (invocation_id, provider_call_id) in state.calls {
        let output = outputs
            .get(&invocation_id)
            .ok_or_else(|| "provider continuation is missing a tool output".to_string())?;
        input.push(json!({
            "type": "function_call_output",
            "call_id": provider_call_id.as_str(),
            "output": output.encoded_output(),
        }));
    }
    if input.len() > MAX_CONTINUATION_ITEMS {
        return Err(format!(
            "Responses continuation exceeds the {MAX_CONTINUATION_ITEMS}-item limit"
        ));
    }
    let encoded_bytes = serde_json::to_vec(&input)
        .map_err(|error| format!("failed to measure Responses continuation: {error}"))?
        .len();
    if encoded_bytes > MAX_CONTINUATION_BYTES {
        return Err(format!(
            "Responses continuation exceeds the {MAX_CONTINUATION_BYTES}-byte limit"
        ));
    }
    Ok(input)
}

fn encode_responses_tools(tools: &[ToolDefinition], provider: &str) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut encoded = tool.to_responses_tool();
            if !matches!(provider, "openai" | "grok") {
                if let Some(object) = encoded.as_object_mut() {
                    object.remove("strict");
                }
            }
            encoded
        })
        .collect()
}

pub struct OpenAiResponsesClient {
    client: Client,
    provider: String,
    model: String,
    api_keys: Vec<String>,
    diagnostic_secrets: Vec<String>,
    endpoint: String,
    reasoning_effort: Option<ReasoningEffort>,
}

impl OpenAiResponsesClient {
    /// Creates a Responses client for an exact endpoint URL.
    pub fn new(
        provider: impl Into<String>,
        model: String,
        api_keys: Vec<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        Self::configured(provider, model, api_keys, endpoint, None)
    }

    /// Creates a Responses client with a provider-level reasoning default.
    pub fn configured(
        provider: impl Into<String>,
        model: String,
        api_keys: Vec<String>,
        endpoint: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        let diagnostic_secrets = api_keys.clone();
        Self::configured_with_diagnostic_secrets(
            provider,
            model,
            api_keys,
            diagnostic_secrets,
            endpoint,
            reasoning_effort,
        )
    }

    pub(crate) fn configured_with_diagnostic_secrets(
        provider: impl Into<String>,
        model: String,
        api_keys: Vec<String>,
        diagnostic_secrets: Vec<String>,
        endpoint: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        Self::configured_with_diagnostic_secrets_and_client(
            provider,
            model,
            api_keys,
            diagnostic_secrets,
            endpoint,
            reasoning_effort,
            Client::new(),
        )
    }

    pub(crate) fn configured_with_diagnostic_secrets_and_client(
        provider: impl Into<String>,
        model: String,
        api_keys: Vec<String>,
        mut diagnostic_secrets: Vec<String>,
        endpoint: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
        client: Client,
    ) -> Self {
        diagnostic_secrets.extend(api_keys.iter().cloned());
        diagnostic_secrets
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        diagnostic_secrets.dedup();
        Self {
            client,
            provider: provider.into(),
            model,
            api_keys,
            diagnostic_secrets,
            endpoint: endpoint.into(),
            reasoning_effort,
        }
    }

    pub(crate) fn request_body(
        &self,
        request: LlmRequest<'_>,
        stream: bool,
    ) -> Result<(Value, Option<LlmRequestScope>, Vec<Value>), String> {
        crate::llm::conformance::reject_explicit_temperature_for_responses(&request)
            .map_err(|error| self.contextual_error("request rejected", &error))?;
        let LlmRequest {
            messages,
            tools,
            options,
            max_output_tokens,
            tool_choice,
            scope,
            continuation,
        } = request;
        let has_continuation = continuation.is_some();

        let replay_tail = continuation
            .map(|continuation| {
                into_responses_input(continuation, &self.provider, &self.model, scope.as_ref())
                    .map_err(|error| self.contextual_error("continuation rejected", &error))
            })
            .transpose()?
            .unwrap_or_default();
        let mut input = messages
            .iter()
            .map(LlmMessage::to_openai_chat)
            .collect::<Vec<_>>();
        input.extend(replay_tail.iter().cloned());

        let mut body = json!({
            "model": self.model,
            "input": input,
            "store": false,
            "stream": stream,
        });
        if let Some(max_output_tokens) = max_output_tokens {
            if max_output_tokens == 0 {
                return Err(self.contextual_error(
                    "request rejected",
                    "max_output_tokens must be greater than zero",
                ));
            }
            body["max_output_tokens"] = json!(max_output_tokens);
        }
        if let Some(effort) = options.resolved_reasoning(self.reasoning_effort) {
            body["reasoning"] = json!({"effort": effort.as_str()});
        }
        let has_tools = tools.is_some_and(|tools| !tools.is_empty());
        if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
            body["tools"] = Value::Array(encode_responses_tools(tools, &self.provider));
            if !(self.provider == "meta"
                || (self.provider == "openrouter" && self.model.starts_with("anthropic/")))
            {
                body["tool_choice"] = tool_choice.as_openai_value();
            }
        }
        if has_tools || has_continuation {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }

        Ok((body, scope, replay_tail))
    }

    fn contextual_error(&self, kind: &str, detail: &str) -> String {
        bounded_redacted(
            format!(
                "{} Responses API {kind} (model {}): {detail}",
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
            RESPONSES_TRANSPORT,
            Some(self.model.clone()),
            self.diagnostic_secrets.clone(),
            retry_attempts,
        )
    }

    fn protocol_error(&self, detail: impl AsRef<str>) -> crate::llm::LlmError {
        crate::llm::LlmError::provider_protocol(
            &self.provider,
            RESPONSES_TRANSPORT,
            Some(&self.model),
            crate::llm::LlmErrorPhase::TerminalValidation,
            detail,
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
            RESPONSES_TRANSPORT,
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
        retry_attempts: crate::llm::RetryAttemptMetadata,
    ) -> crate::llm::LlmError {
        let request_id = if self.provider == "openai" {
            crate::llm::error::bounded_request_id_header(response.headers().get("x-request-id"))
        } else {
            None
        };
        let (structured, detail) =
            match read_bounded_error_response(response, "Responses API error response").await {
                Ok(body) => {
                    let structured = serde_json::from_str::<Value>(&body).ok();
                    let detail = structured
                        .as_ref()
                        .map(structured_error_detail)
                        .unwrap_or_else(|| {
                            "provider returned a non-JSON error response".to_string()
                        });
                    (structured, detail)
                }
                Err(error) if error.contains("byte limit") => (None, error),
                Err(_) => (
                    None,
                    "provider error response could not be read".to_string(),
                ),
            };
        let error = crate::llm::LlmError::http(
            &self.provider,
            RESPONSES_TRANSPORT,
            Some(&self.model),
            status,
            structured.as_ref(),
            self.contextual_error(&format!("HTTP {}", status.as_u16()), &detail),
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
        self.protocol_error(self.contextual_error("protocol failure", detail))
    }
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
#[async_trait]
impl LlmClient for OpenAiResponsesClient {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, crate::llm::LlmError> {
        crate::llm::conformance::validate_request_capabilities(
            &request,
            &self.provider,
            crate::llm::registry::ProviderTransport::Responses,
            crate::llm::conformance::ProviderOperation::Complete,
        )
        .map_err(|error| {
            crate::llm::LlmError::request_rejected(
                &self.provider,
                RESPONSES_TRANSPORT,
                Some(&self.model),
                error,
                &self.diagnostic_secrets,
            )
        })?;
        let (body, scope, replay_tail) = self.request_body(request, false).map_err(|error| {
            crate::llm::LlmError::request_rejected(
                &self.provider,
                RESPONSES_TRANSPORT,
                Some(&self.model),
                error,
                &self.diagnostic_secrets,
            )
        })?;
        let retry_capabilities = crate::llm::registry::retry_capabilities(
            &self.provider,
            crate::llm::registry::ProviderTransport::Responses,
        );
        let outcome = send_with_retry(
            |credential_index| {
                self.client
                    .post(&self.endpoint)
                    .bearer_auth(&self.api_keys[credential_index])
                    .json(&body)
            },
            self.api_keys.len(),
            retry_capabilities,
        )
        .await
        .map_err(|failure| self.request_error(&failure.error, failure.attempts))?;
        let retry_attempts = outcome.attempts;
        let response = outcome.value;

        let status = response.status();
        if !status.is_success() {
            return Err(self.http_error(status, response, retry_attempts).await);
        }
        let data = read_bounded_json_response(response, "Responses completion response")
            .await
            .map_err(|error| {
                self.completion_read_error(&error)
                    .with_retry_attempts(retry_attempts)
            })?;
        parse_terminal_response(
            &data,
            &self.provider,
            &self.model,
            scope,
            replay_tail,
            &self.diagnostic_secrets,
        )
        .map_err(|error| {
            crate::llm::LlmError::provider_rejected(
                &self.provider,
                RESPONSES_TRANSPORT,
                Some(&self.model),
                crate::llm::LlmErrorPhase::TerminalValidation,
                Some(&data),
                error,
                &self.diagnostic_secrets,
            )
            .with_retry_attempts(retry_attempts)
        })
        .map(|response| response.with_retry_attempts(retry_attempts))
    }

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, crate::llm::LlmError> {
        crate::llm::conformance::validate_request_capabilities(
            &request,
            &self.provider,
            crate::llm::registry::ProviderTransport::Responses,
            crate::llm::conformance::ProviderOperation::Stream,
        )
        .map_err(|error| {
            crate::llm::LlmError::request_rejected(
                &self.provider,
                RESPONSES_TRANSPORT,
                Some(&self.model),
                error,
                &self.diagnostic_secrets,
            )
        })?;
        let (body, scope, replay_tail) = self.request_body(request, true).map_err(|error| {
            crate::llm::LlmError::request_rejected(
                &self.provider,
                RESPONSES_TRANSPORT,
                Some(&self.model),
                error,
                &self.diagnostic_secrets,
            )
        })?;
        let retry_capabilities = crate::llm::registry::retry_capabilities(
            &self.provider,
            crate::llm::registry::ProviderTransport::Responses,
        );
        let outcome = send_with_retry(
            |credential_index| {
                self.client
                    .post(&self.endpoint)
                    .bearer_auth(&self.api_keys[credential_index])
                    .json(&body)
            },
            self.api_keys.len(),
            retry_capabilities,
        )
        .await
        .map_err(|failure| self.request_error(&failure.error, failure.attempts))?;
        let retry_attempts = outcome.attempts;
        let response = outcome.value;

        let status = response.status();
        if !status.is_success() {
            return Err(self.http_error(status, response, retry_attempts).await);
        }
        ensure_response_content_length(
            &response,
            MAX_LLM_STREAM_BYTES,
            "Responses stream response",
        )
        .map_err(|error| {
            self.protocol_error(self.contextual_error("protocol failure", &error))
                .with_retry_attempts(retry_attempts)
        })?;

        let provider = self.provider.clone();
        let model = self.model.clone();
        let diagnostic_secrets = self.diagnostic_secrets.clone();
        let (public_tx, public_rx) = mpsc::channel(100);
        let (completion_tx, completion_rx) = oneshot::channel();

        tokio::spawn(async move {
            use eventsource_stream::{EventStreamError, Eventsource};

            let mut completion_tx = Some(completion_tx);
            let mut budget =
                ResponseByteBudget::new(MAX_LLM_STREAM_BYTES, "Responses stream response");
            let bounded_bytes = response.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|_| {
                    "Responses stream transport failed while reading a response".to_string()
                })?;
                budget.account(chunk.len())?;
                Ok::<_, String>(chunk)
            });
            let mut events = bounded_bytes.eventsource();
            let mut item_budget = crate::llm::ResponseItemBudget::new(
                crate::llm::MAX_LLM_STREAM_EVENTS,
                "Responses stream events",
            );

            while let Some(event) = next_stream_item_or_closed(&public_tx, &mut events).await {
                let event = match event {
                    Ok(event) => event,
                    Err(EventStreamError::Transport(error)) => {
                        fail_stream(
                            &public_tx,
                            &mut completion_tx,
                            crate::llm::LlmError::transport(
                                &provider,
                                RESPONSES_TRANSPORT,
                                Some(&model),
                                contextual_error(
                                    &provider,
                                    &model,
                                    "stream transport failed",
                                    &error,
                                    &diagnostic_secrets,
                                ),
                                &diagnostic_secrets,
                            )
                            .with_phase(crate::llm::LlmErrorPhase::Stream),
                        )
                        .await;
                        return;
                    }
                    Err(EventStreamError::Utf8(_) | EventStreamError::Parser(_)) => {
                        fail_stream(
                            &public_tx,
                            &mut completion_tx,
                            contextual_error(
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
                };
                if let Err(error) = item_budget.account(1) {
                    fail_stream(&public_tx, &mut completion_tx, error).await;
                    return;
                }

                if let Err(error) =
                    ensure_stream_event_size(event.data.len(), "Responses stream event")
                {
                    fail_stream(
                        &public_tx,
                        &mut completion_tx,
                        contextual_error(
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
                if event.data == "[DONE]" {
                    fail_stream(
                        &public_tx,
                        &mut completion_tx,
                        crate::llm::LlmError::provider_rejected(
                            &provider,
                            RESPONSES_TRANSPORT,
                            Some(&model),
                            crate::llm::LlmErrorPhase::Stream,
                            None,
                            contextual_error(
                                &provider,
                                &model,
                                "protocol failure",
                                "stream ended before a typed response.completed event",
                                &diagnostic_secrets,
                            ),
                            &diagnostic_secrets,
                        ),
                    )
                    .await;
                    return;
                }

                let data = match serde_json::from_str::<Value>(&event.data) {
                    Ok(data) => data,
                    Err(_) => {
                        fail_stream(
                            &public_tx,
                            &mut completion_tx,
                            contextual_error(
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
                };

                match parse_stream_event(
                    &data,
                    &provider,
                    &model,
                    scope.as_ref(),
                    &replay_tail,
                    &diagnostic_secrets,
                ) {
                    Ok(StreamAction::Ignore) => {}
                    Ok(StreamAction::Public(projected)) => {
                        for projected in projected {
                            if !send_stream_event(&public_tx, Ok(projected)).await {
                                return;
                            }
                        }
                    }
                    Ok(StreamAction::Completed(completion)) => {
                        let completion = (*completion).with_retry_attempts(retry_attempts);
                        let finish_reason = completion.finish_reason;
                        if !send_stream_event(
                            &public_tx,
                            Ok(LlmStreamEvent::Terminal(finish_reason)),
                        )
                        .await
                        {
                            return;
                        }
                        if let Some(sender) = completion_tx.take() {
                            let _ = sender.send(Ok(completion));
                        }
                        return;
                    }
                    Err(error) => {
                        fail_stream(
                            &public_tx,
                            &mut completion_tx,
                            crate::llm::LlmError::provider_rejected(
                                &provider,
                                RESPONSES_TRANSPORT,
                                Some(&model),
                                crate::llm::LlmErrorPhase::Stream,
                                Some(&data),
                                error,
                                &diagnostic_secrets,
                            ),
                        )
                        .await;
                        return;
                    }
                }
            }

            if !public_tx.is_closed() {
                fail_stream(
                    &public_tx,
                    &mut completion_tx,
                    crate::llm::LlmError::provider_rejected(
                        &provider,
                        RESPONSES_TRANSPORT,
                        Some(&model),
                        crate::llm::LlmErrorPhase::Stream,
                        None,
                        contextual_error(
                            &provider,
                            &model,
                            "protocol failure",
                            "stream ended before a typed response.completed event",
                            &diagnostic_secrets,
                        ),
                        &diagnostic_secrets,
                    ),
                )
                .await;
            }
        });

        Ok(LlmStream::with_private_completion(public_rx, completion_rx)
            .with_error_context(self.error_context(retry_attempts)))
    }
}

async fn fail_stream(
    public_tx: &mpsc::Sender<Result<LlmStreamEvent, crate::llm::LlmStreamFailure>>,
    completion_tx: &mut Option<oneshot::Sender<Result<LlmResponse, crate::llm::LlmStreamFailure>>>,
    error: impl Into<crate::llm::LlmStreamFailure>,
) {
    let error = error.into();
    let _ = send_stream_event(public_tx, Err(error.clone())).await;
    if let Some(sender) = completion_tx.take() {
        let _ = sender.send(Err(error));
    }
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn parse_terminal_response(
    data: &Value,
    provider: &str,
    model: &str,
    scope: Option<LlmRequestScope>,
    replay_tail: Vec<Value>,
    secrets: &[String],
) -> Result<LlmResponse, String> {
    let object = data.as_object().ok_or_else(|| {
        contextual_error(
            provider,
            model,
            "protocol failure",
            "terminal response was not an object",
            secrets,
        )
    })?;
    let status = object.get("status").and_then(Value::as_str);
    if !matches!(status, Some("completed") | Some("complete")) {
        let detail = match status {
            Some("incomplete") => incomplete_detail(data),
            Some("failed") => structured_error_detail(data),
            _ => "terminal response status was not completed".to_string(),
        };
        return Err(contextual_error(
            provider,
            model,
            "did not complete",
            &detail,
            secrets,
        ));
    }
    if object.get("error").is_some_and(|error| !error.is_null()) {
        return Err(contextual_error(
            provider,
            model,
            "failed",
            &structured_error_detail(data),
            secrets,
        ));
    }
    let usage = parse_responses_usage(data)
        .map_err(|error| contextual_error(provider, model, "protocol failure", &error, secrets))?;

    let output = object
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            contextual_error(
                provider,
                model,
                "protocol failure",
                "completed response was missing its output array",
                secrets,
            )
        })?;
    crate::llm::ensure_response_item_count(output.len(), "Responses output items")
        .map_err(|error| contextual_error(provider, model, "protocol failure", &error, secrets))?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut call_bindings = Vec::new();
    let mut refused = false;
    let continuation_binding = uuid::Uuid::new_v4();

    for item in output {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            return Err(contextual_error(
                provider,
                model,
                "protocol failure",
                "response output item was missing its type",
                secrets,
            ));
        };
        match item_type {
            "message" => {
                ensure_completed_item(item, provider, model, "message", secrets)?;
                match item.get("content") {
                    Some(Value::String(text)) => content.push_str(text),
                    Some(Value::Array(parts)) => {
                        crate::llm::ensure_response_item_count(
                            parts.len(),
                            "Responses content parts",
                        )
                        .map_err(|error| {
                            contextual_error(provider, model, "protocol failure", &error, secrets)
                        })?;
                        for part in parts {
                            match part.get("type").and_then(Value::as_str) {
                                Some("output_text") | Some("text") => {
                                    let text = part
                                        .get("text")
                                        .and_then(Value::as_str)
                                        .ok_or_else(|| {
                                            contextual_error(
                                                provider,
                                                model,
                                                "protocol failure",
                                                "output_text part was missing text",
                                                secrets,
                                            )
                                        })?;
                                    content.push_str(text);
                                }
                                Some("refusal") => {
                                    refused = true;
                                }
                                Some(_) => {}
                                None => {
                                    if let Some(text) = part.as_str() {
                                        content.push_str(text);
                                    } else {
                                        return Err(contextual_error(
                                            provider,
                                            model,
                                            "protocol failure",
                                            "response content part was missing its type",
                                            secrets,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        return Err(contextual_error(
                            provider,
                            model,
                            "protocol failure",
                            "response message was missing its content array",
                            secrets,
                        ));
                    }
                }
            }
            "function_call" => {
                ensure_completed_item(item, provider, model, "function call", secrets)?;
                let call_id =
                    required_string(item, "call_id", "function call", provider, model, secrets)?;
                let name = required_nonempty_string(
                    item,
                    "name",
                    "function call",
                    provider,
                    model,
                    secrets,
                )?;
                let arguments =
                    required_string(item, "arguments", "function call", provider, model, secrets)?;
                let arguments = serde_json::from_str(arguments).map_err(|_| {
                    contextual_error(
                        provider,
                        model,
                        "protocol failure",
                        "function call contained invalid JSON arguments",
                        secrets,
                    )
                })?;
                let call_id =
                    ProviderCallId::for_responses(call_id.to_string(), continuation_binding)
                        .map_err(|_| {
                            contextual_error(
                                provider,
                                model,
                                "protocol failure",
                                "function call contained an invalid provider call ID",
                                secrets,
                            )
                        })?;
                let tool_call = ToolCallRequest::with_provider_call(call_id, name, arguments);
                call_bindings.push((
                    tool_call.invocation_id,
                    tool_call
                        .call_id
                        .as_ref()
                        .expect("provider call was attached")
                        .clone(),
                ));
                tool_calls.push(tool_call);
            }
            "refusal" => {
                refused = true;
            }
            "error" => {
                return Err(contextual_error(
                    provider,
                    model,
                    "failed",
                    &structured_error_detail(item),
                    secrets,
                ));
            }
            _ => {}
        }
    }

    if refused && !tool_calls.is_empty() {
        return Err(contextual_error(
            provider,
            model,
            "protocol failure",
            "completed response mixed a refusal with executable function calls",
            secrets,
        ));
    }
    if refused {
        return Ok(LlmResponse {
            terminal_status: LlmTerminalStatus::Refused,
            content: None,
            tool_calls: None,
            finish_reason: LlmFinishReason::Refusal,
            continuation: None,
            usage,
            attempts: crate::llm::RetryAttemptMetadata::no_network_attempt(),
        });
    }

    let continuation = if call_bindings.is_empty() {
        None
    } else {
        let mut continuation_items = replay_tail;
        continuation_items.extend(output.iter().cloned());
        Some(
            responses_continuation(provider, model, scope, continuation_items, call_bindings)
                .map_err(|error| {
                    contextual_error(provider, model, "protocol failure", &error, secrets)
                })?,
        )
    };
    let has_tools = !tool_calls.is_empty();

    Ok(LlmResponse {
        terminal_status: LlmTerminalStatus::Completed,
        content: (!content.trim().is_empty()).then_some(content),
        tool_calls: has_tools.then_some(tool_calls),
        finish_reason: if has_tools {
            LlmFinishReason::ToolCalls
        } else {
            LlmFinishReason::Complete
        },
        continuation,
        usage,
        attempts: crate::llm::RetryAttemptMetadata::no_network_attempt(),
    })
}

fn required_responses_usage_count(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<u64, String> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("Responses usage {field} must be a non-negative integer"))
}

fn optional_responses_usage_detail(
    usage: &serde_json::Map<String, Value>,
    details_field: &str,
    count_field: &str,
) -> Result<Option<u64>, String> {
    let Some(details) = usage.get(details_field).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let details = details
        .as_object()
        .ok_or_else(|| format!("Responses usage {details_field} must be an object"))?;
    match details.get(count_field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            format!("Responses usage {details_field}.{count_field} must be a non-negative integer")
        }),
    }
}

fn parse_responses_usage(data: &Value) -> Result<Option<LlmUsage>, String> {
    let Some(usage) = data.get("usage").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let usage = usage
        .as_object()
        .ok_or_else(|| "Responses usage must be an object".to_string())?;
    LlmUsage::new(
        required_responses_usage_count(usage, "input_tokens")?,
        required_responses_usage_count(usage, "output_tokens")?,
        required_responses_usage_count(usage, "total_tokens")?,
        optional_responses_usage_detail(usage, "input_tokens_details", "cached_tokens")?,
        optional_responses_usage_detail(usage, "output_tokens_details", "reasoning_tokens")?,
    )
    .map(Some)
    .map_err(|error| format!("Responses usage is invalid: {error}"))
}

fn ensure_completed_item(
    item: &Value,
    provider: &str,
    model: &str,
    label: &str,
    secrets: &[String],
) -> Result<(), String> {
    match item.get("status") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(status)) if matches!(status.as_str(), "completed" | "complete") => {
            Ok(())
        }
        _ => Err(contextual_error(
            provider,
            model,
            "protocol failure",
            &format!("terminal {label} item was not completed"),
            secrets,
        )),
    }
}

fn required_string<'a>(
    item: &'a Value,
    field: &str,
    label: &str,
    provider: &str,
    model: &str,
    secrets: &[String],
) -> Result<&'a str, String> {
    item.get(field).and_then(Value::as_str).ok_or_else(|| {
        contextual_error(
            provider,
            model,
            "protocol failure",
            &format!("{label} was missing {field}"),
            secrets,
        )
    })
}

fn required_nonempty_string<'a>(
    item: &'a Value,
    field: &str,
    label: &str,
    provider: &str,
    model: &str,
    secrets: &[String],
) -> Result<&'a str, String> {
    required_string(item, field, label, provider, model, secrets).and_then(|value| {
        if value.trim().is_empty() {
            Err(contextual_error(
                provider,
                model,
                "protocol failure",
                &format!("{label} was missing {field}"),
                secrets,
            ))
        } else {
            Ok(value)
        }
    })
}

#[derive(Debug)]
enum StreamAction {
    Ignore,
    Public(Vec<LlmStreamEvent>),
    Completed(Box<LlmResponse>),
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn parse_stream_event(
    data: &Value,
    provider: &str,
    model: &str,
    scope: Option<&LlmRequestScope>,
    replay_tail: &[Value],
    secrets: &[String],
) -> Result<StreamAction, String> {
    let event_type = data.get("type").and_then(Value::as_str).ok_or_else(|| {
        contextual_error(
            provider,
            model,
            "protocol failure",
            "stream event was missing its type",
            secrets,
        )
    })?;

    match event_type {
        "response.output_text.delta" => {
            let delta =
                required_string(data, "delta", "output text delta", provider, model, secrets)?;
            Ok(StreamAction::Public(vec![LlmStreamEvent::Delta(
                LlmDelta::Content(delta.to_string()),
            )]))
        }
        "response.output_item.added"
            if data
                .get("item")
                .and_then(|item| item.get("type"))
                .and_then(Value::as_str)
                == Some("function_call") =>
        {
            let index = stream_output_index(data, provider, model, secrets)?;
            let item = data.get("item").expect("guarded above");
            let name = required_nonempty_string(
                item,
                "name",
                "function call item",
                provider,
                model,
                secrets,
            )?;
            Ok(StreamAction::Public(vec![LlmStreamEvent::Delta(
                LlmDelta::ToolCallChunk {
                    index,
                    name: Some(name.to_string()),
                    arguments: None,
                },
            )]))
        }
        "response.function_call_arguments.delta" => {
            let index = stream_output_index(data, provider, model, secrets)?;
            let delta = required_string(
                data,
                "delta",
                "function arguments delta",
                provider,
                model,
                secrets,
            )?;
            Ok(StreamAction::Public(vec![LlmStreamEvent::Delta(
                LlmDelta::ToolCallChunk {
                    index,
                    name: None,
                    arguments: Some(delta.to_string()),
                },
            )]))
        }
        "response.completed" => {
            let response = data.get("response").ok_or_else(|| {
                contextual_error(
                    provider,
                    model,
                    "protocol failure",
                    "response.completed event was missing its response envelope",
                    secrets,
                )
            })?;
            parse_terminal_response(
                response,
                provider,
                model,
                scope.cloned(),
                replay_tail.to_vec(),
                secrets,
            )
            .map(Box::new)
            .map(StreamAction::Completed)
        }
        "response.failed" => Err(contextual_error(
            provider,
            model,
            "failed",
            &structured_error_detail(data.get("response").unwrap_or(data)),
            secrets,
        )),
        "response.incomplete" => Err(contextual_error(
            provider,
            model,
            "did not complete",
            &incomplete_detail(data.get("response").unwrap_or(data)),
            secrets,
        )),
        "error" => Err(contextual_error(
            provider,
            model,
            "stream failed",
            &structured_error_detail(data),
            secrets,
        )),
        _ => Ok(StreamAction::Ignore),
    }
}

fn stream_output_index(
    data: &Value,
    provider: &str,
    model: &str,
    secrets: &[String],
) -> Result<usize, String> {
    let index = data
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok());
    index.ok_or_else(|| {
        contextual_error(
            provider,
            model,
            "protocol failure",
            "streamed function event was missing a valid output_index",
            secrets,
        )
    })
}

fn structured_error_detail(_data: &Value) -> String {
    "provider returned an error; provider-supplied detail omitted".to_string()
}

fn incomplete_detail(_data: &Value) -> String {
    "provider returned an incomplete response; provider-supplied detail omitted".to_string()
}

fn contextual_error(
    provider: &str,
    model: &str,
    kind: &str,
    detail: &str,
    secrets: &[String],
) -> String {
    bounded_redacted(
        format!("{provider} Responses API {kind} (model {model}): {detail}"),
        secrets,
    )
}

fn bounded_redacted(message: String, secrets: &[String]) -> String {
    let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
        &message,
        secrets.iter().cloned(),
    );
    truncate_utf8(escape_diagnostic_controls(&redacted), MAX_DIAGNOSTIC_BYTES)
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

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
    value.push_str("...");
    value
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod tests;
