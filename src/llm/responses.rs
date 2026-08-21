//! OpenAI-compatible Responses API transport.

use crate::config::ReasoningEffort;
use crate::llm::types::{
    LlmMessage, LlmRequest, LlmRequestScope, LlmResponse, LlmTerminalStatus, ProviderCallId,
    ProviderContinuation, StreamEvent, ToolCallRequest, ToolDefinition, MAX_CONTINUATION_BYTES,
    MAX_CONTINUATION_ITEMS,
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
    let (state, outputs): (ResponsesTurnState, BTreeMap<ToolInvocationId, String>) =
        continuation.consume(provider, model, RESPONSES_TRANSPORT, scope)?;
    let mut input = state.output_items;
    for (invocation_id, provider_call_id) in state.calls {
        let output = outputs
            .get(&invocation_id)
            .ok_or_else(|| "provider continuation is missing a tool output".to_string())?;
        input.push(json!({
            "type": "function_call_output",
            "call_id": provider_call_id.as_str(),
            "output": output,
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
        mut diagnostic_secrets: Vec<String>,
        endpoint: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Self {
        diagnostic_secrets.extend(api_keys.iter().cloned());
        diagnostic_secrets
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        diagnostic_secrets.dedup();
        Self {
            client: Client::new(),
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
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(ToolDefinition::to_responses_tool)
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
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

    fn error_context(&self) -> crate::llm::error::LlmErrorContext {
        crate::llm::error::LlmErrorContext::new(
            self.provider.clone(),
            RESPONSES_TRANSPORT,
            Some(self.model.clone()),
            self.diagnostic_secrets.clone(),
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

    fn request_error(&self, error: &str) -> crate::llm::LlmError {
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
    }

    async fn http_error(
        &self,
        status: StatusCode,
        response: reqwest::Response,
    ) -> crate::llm::LlmError {
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
        crate::llm::LlmError::http(
            &self.provider,
            RESPONSES_TRANSPORT,
            Some(&self.model),
            status,
            structured.as_ref(),
            self.contextual_error(&format!("HTTP {}", status.as_u16()), &detail),
            &self.diagnostic_secrets,
        )
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

#[async_trait]
impl LlmClient for OpenAiResponsesClient {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, crate::llm::LlmError> {
        let (body, scope, replay_tail) = self.request_body(request, false).map_err(|error| {
            crate::llm::LlmError::request_rejected(
                &self.provider,
                RESPONSES_TRANSPORT,
                Some(&self.model),
                error,
                &self.diagnostic_secrets,
            )
        })?;
        let response = send_with_retry(
            |credential_index| {
                self.client
                    .post(&self.endpoint)
                    .bearer_auth(&self.api_keys[credential_index])
                    .json(&body)
            },
            self.api_keys.len(),
        )
        .await
        .map_err(|error| self.request_error(&error))?;

        let status = response.status();
        if !status.is_success() {
            return Err(self.http_error(status, response).await);
        }
        let data = read_bounded_json_response(response, "Responses completion response")
            .await
            .map_err(|error| self.completion_read_error(&error))?;
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
        })
    }

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, crate::llm::LlmError> {
        let (body, scope, replay_tail) = self.request_body(request, true).map_err(|error| {
            crate::llm::LlmError::request_rejected(
                &self.provider,
                RESPONSES_TRANSPORT,
                Some(&self.model),
                error,
                &self.diagnostic_secrets,
            )
        })?;
        let response = send_with_retry(
            |credential_index| {
                self.client
                    .post(&self.endpoint)
                    .bearer_auth(&self.api_keys[credential_index])
                    .json(&body)
            },
            self.api_keys.len(),
        )
        .await
        .map_err(|error| self.request_error(&error))?;

        let status = response.status();
        if !status.is_success() {
            return Err(self.http_error(status, response).await);
        }
        ensure_response_content_length(
            &response,
            MAX_LLM_STREAM_BYTES,
            "Responses stream response",
        )
        .map_err(|error| self.protocol_error(self.contextual_error("protocol failure", &error)))?;

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
                        contextual_error(
                            &provider,
                            &model,
                            "protocol failure",
                            "stream ended before a typed response.completed event",
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
                        let completion = *completion;
                        let finish_reason = completion.finish_reason.clone();
                        if !send_stream_event(&public_tx, Ok(StreamEvent::End(finish_reason))).await
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
                    contextual_error(
                        &provider,
                        &model,
                        "protocol failure",
                        "stream ended before a typed response.completed event",
                        &diagnostic_secrets,
                    ),
                )
                .await;
            }
        });

        Ok(LlmStream::with_private_completion(public_rx, completion_rx)
            .with_error_context(self.error_context()))
    }
}

async fn fail_stream(
    public_tx: &mpsc::Sender<Result<StreamEvent, crate::llm::LlmStreamFailure>>,
    completion_tx: &mut Option<oneshot::Sender<Result<LlmResponse, crate::llm::LlmStreamFailure>>>,
    error: impl Into<crate::llm::LlmStreamFailure>,
) {
    let error = error.into();
    let _ = send_stream_event(public_tx, Err(error.clone())).await;
    if let Some(sender) = completion_tx.take() {
        let _ = sender.send(Err(error));
    }
}

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
    if status != Some("completed") {
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
                let parts = item
                    .get("content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        contextual_error(
                            provider,
                            model,
                            "protocol failure",
                            "response message was missing its content array",
                            secrets,
                        )
                    })?;
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            let text =
                                part.get("text").and_then(Value::as_str).ok_or_else(|| {
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
            finish_reason: "refusal".to_string(),
            continuation: None,
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
        finish_reason: if has_tools { "tool_calls" } else { "stop" }.to_string(),
        continuation,
    })
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
        Some(Value::String(status)) if status == "completed" => Ok(()),
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

enum StreamAction {
    Ignore,
    Public(Vec<StreamEvent>),
    Completed(Box<LlmResponse>),
}

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
            Ok(StreamAction::Public(vec![StreamEvent::Content(
                delta.to_string(),
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
            Ok(StreamAction::Public(vec![StreamEvent::ToolCallChunk {
                index,
                name: Some(name.to_string()),
                arguments: None,
            }]))
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
            Ok(StreamAction::Public(vec![StreamEvent::ToolCallChunk {
                index,
                name: None,
                arguments: Some(delta.to_string()),
            }]))
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
mod tests {
    use super::*;
    use crate::llm::test_support::serve_once;
    use std::time::Duration;

    fn completed_with_call() -> Value {
        json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "opaque-reasoning-secret"
                },
                {
                    "type": "message",
                    "status": "completed",
                    "content": [
                        {"type": "output_text", "text": "inspected"},
                        {"type": "output_text", "text": " successfully"}
                    ]
                },
                {
                    "type": "function_call",
                    "status": "completed",
                    "call_id": "call_private_123",
                    "name": "read_file",
                    "arguments": "{\"path\":\"README.md\"}"
                }
            ]
        })
    }

    fn request_json(request: &str) -> Value {
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("HTTP request body separator");
        serde_json::from_str(body).expect("JSON request body")
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {}\n\n", event))
            .collect()
    }

    #[tokio::test]
    async fn posts_responses_native_payload_and_preserves_private_continuation() {
        let terminal = completed_with_call();
        let (base_url, request_rx) = serve_once("200 OK", "application/json", terminal.to_string());
        let endpoint = format!("{base_url}/v1/responses?route=test");
        let client = OpenAiResponsesClient::configured(
            "openai",
            "gpt-test".to_string(),
            vec!["test-api-key".to_string()],
            endpoint,
            Some(ReasoningEffort::Low),
        );
        let messages = [LlmMessage::user("inspect")];
        let tools = [
            ToolDefinition::new("read_file", "Read a file", json!({"type": "object"}))
                .expect("read_file tool"),
        ];
        let scope = LlmRequestScope::new("session-1", "run-1").unwrap();
        let response = client
            .complete(
                LlmRequest::new(&messages, Some(&tools))
                    .with_reasoning_effort(Some(ReasoningEffort::Medium))
                    .with_max_output_tokens(41)
                    .with_scope(scope.clone()),
            )
            .await
            .expect("Responses completion");

        assert_eq!(response.content.as_deref(), Some("inspected successfully"));
        assert_eq!(response.terminal_status, LlmTerminalStatus::Completed);
        assert_eq!(response.finish_reason, "tool_calls");
        let call = &response.tool_calls.as_ref().expect("tool calls")[0];
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments["path"], "README.md");
        assert_eq!(
            call.call_id.as_ref().expect("provider call ID").as_str(),
            "call_private_123"
        );
        assert_eq!(
            format!("{:?}", response.continuation.as_ref().unwrap()),
            "ProviderContinuation { value: \"<redacted>\" }"
        );

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured request");
        assert!(request.starts_with("POST /v1/responses?route=test HTTP/1.1"));
        let body = request_json(&request);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_output_tokens"], 41);
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(
            body["include"],
            json!(["reasoning.encrypted_content"]),
            "stateless tool turns must request replayable encrypted reasoning"
        );
        assert!(body.get("temperature").is_none());
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(body["tools"][0].get("function").is_none());

        let invocation_id = call.invocation_id;
        let mut continuation = response.continuation.expect("continuation");
        continuation
            .record_tool_output(invocation_id, &json!({"ok": true}))
            .unwrap();
        let follow_up = [LlmMessage::user("continue")];
        let (continued_body, _, replay_tail) = client
            .request_body(
                LlmRequest::new(&follow_up, None)
                    .with_scope(scope.clone())
                    .with_continuation(Some(continuation)),
                false,
            )
            .expect("matching continuation");
        let input = continued_body["input"].as_array().unwrap();
        assert_eq!(
            continued_body["include"],
            json!(["reasoning.encrypted_content"])
        );
        assert_eq!(input.len(), 5);
        assert_eq!(input[1]["encrypted_content"], "opaque-reasoning-secret");
        assert_eq!(input[3]["call_id"], "call_private_123");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "call_private_123");
        assert_eq!(input[4]["output"], "{\"ok\":true}");

        let second_terminal = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_2", "encrypted_content": "second-private"},
                {"type": "function_call", "call_id": "call_private_456", "name": "write_file", "arguments": "{\"path\":\"out.txt\"}"}
            ]
        });
        let second_response = parse_terminal_response(
            &second_terminal,
            "openai",
            "gpt-test",
            Some(scope.clone()),
            replay_tail,
            &[],
        )
        .expect("second tool round");
        let second_invocation_id = second_response.tool_calls.as_ref().unwrap()[0].invocation_id;
        let mut second_continuation = second_response.continuation.unwrap();
        second_continuation
            .record_tool_output(second_invocation_id, &json!({"written": true}))
            .unwrap();
        let next_messages = [LlmMessage::user("latest runtime context")];
        let (third_body, _, _) = client
            .request_body(
                LlmRequest::new(&next_messages, None)
                    .with_scope(scope)
                    .with_continuation(Some(second_continuation)),
                false,
            )
            .expect("second continuation");
        let third_input = third_body["input"].as_array().unwrap();
        assert_eq!(third_input.len(), 8);
        assert_eq!(third_input[0]["content"], "latest runtime context");
        assert_eq!(third_input[1]["id"], "rs_1");
        assert_eq!(third_input[2]["type"], "message");
        assert_eq!(third_input[3]["call_id"], "call_private_123");
        assert_eq!(third_input[4]["type"], "function_call_output");
        assert_eq!(third_input[4]["call_id"], "call_private_123");
        assert_eq!(third_input[5]["id"], "rs_2");
        assert_eq!(third_input[6]["call_id"], "call_private_456");
        assert_eq!(third_input[7]["type"], "function_call_output");
        assert_eq!(third_input[7]["call_id"], "call_private_456");
    }

    #[test]
    fn rejects_mismatched_continuation_binding() {
        let response = parse_terminal_response(
            &completed_with_call(),
            "openai",
            "gpt-a",
            Some(LlmRequestScope::new("session", "run").unwrap()),
            Vec::new(),
            &[],
        )
        .unwrap();
        let mut continuation = response.continuation.unwrap();
        let invocation_id = response.tool_calls.unwrap()[0].invocation_id;
        continuation
            .record_tool_output(invocation_id, &json!("done"))
            .unwrap();
        let client = OpenAiResponsesClient::new(
            "openai",
            "gpt-b".to_string(),
            vec!["key".to_string()],
            "http://unused.invalid/v1/responses",
        );
        let messages = [LlmMessage::user("continue")];
        let error = client
            .request_body(
                LlmRequest::new(&messages, None)
                    .with_scope(LlmRequestScope::new("session", "run").unwrap())
                    .with_continuation(Some(continuation)),
                false,
            )
            .unwrap_err();
        assert!(error.contains("does not match"));
    }

    #[tokio::test]
    async fn concurrent_session_continuations_are_scope_bound_and_isolated() {
        use std::sync::Arc;

        fn terminal(call_id: &str, name: &str, opaque: &str) -> Value {
            json!({
                "status": "completed",
                "output": [
                    {"type": "reasoning", "encrypted_content": opaque},
                    {"type": "function_call", "call_id": call_id, "name": name, "arguments": "{}"}
                ]
            })
        }

        let client = Arc::new(OpenAiResponsesClient::new(
            "openai",
            "gpt-test".to_string(),
            vec!["key".to_string()],
            "http://unused.invalid/v1/responses",
        ));

        let cross_response = parse_terminal_response(
            &terminal("call_cross", "cross", "opaque-cross"),
            "openai",
            "gpt-test",
            Some(LlmRequestScope::new("session-a", "run-a").unwrap()),
            Vec::new(),
            &[],
        )
        .unwrap();
        let cross_invocation_id = cross_response.tool_calls.as_ref().unwrap()[0].invocation_id;
        let mut cross_continuation = cross_response.continuation.unwrap();
        cross_continuation
            .record_tool_output(cross_invocation_id, &json!({"ok": true}))
            .unwrap();
        let cross_messages = [LlmMessage::user("cross")];
        let error = client
            .request_body(
                LlmRequest::new(&cross_messages, None)
                    .with_scope(LlmRequestScope::new("session-b", "run-b").unwrap())
                    .with_continuation(Some(cross_continuation)),
                false,
            )
            .expect_err("another session must not consume a continuation");
        assert!(error.contains("does not match"));

        let build = |session: &'static str,
                     run: &'static str,
                     call_id: &'static str,
                     opaque: &'static str| {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                let scope = LlmRequestScope::new(session, run).unwrap();
                let response = parse_terminal_response(
                    &terminal(call_id, "inspect", opaque),
                    "openai",
                    "gpt-test",
                    Some(scope.clone()),
                    Vec::new(),
                    &[],
                )
                .unwrap();
                let invocation_id = response.tool_calls.as_ref().unwrap()[0].invocation_id;
                let mut continuation = response.continuation.unwrap();
                continuation
                    .record_tool_output(invocation_id, &json!({"session": session}))
                    .unwrap();
                let messages = [LlmMessage::user(session)];
                client
                    .request_body(
                        LlmRequest::new(&messages, None)
                            .with_scope(scope)
                            .with_continuation(Some(continuation)),
                        false,
                    )
                    .unwrap()
                    .0
            })
        };
        let (body_a, body_b) = tokio::join!(
            build("session-a", "run-a", "call_a", "opaque-a"),
            build("session-b", "run-b", "call_b", "opaque-b")
        );
        let body_a = body_a.unwrap().to_string();
        let body_b = body_b.unwrap().to_string();
        assert!(body_a.contains("call_a"));
        assert!(body_a.contains("opaque-a"));
        assert!(!body_a.contains("call_b"));
        assert!(!body_a.contains("opaque-b"));
        assert!(body_b.contains("call_b"));
        assert!(body_b.contains("opaque-b"));
        assert!(!body_b.contains("call_a"));
        assert!(!body_b.contains("opaque-a"));
    }

    #[test]
    fn same_raw_call_id_from_another_session_cannot_complete_a_continuation() {
        let response_a = parse_terminal_response(
            &completed_with_call(),
            "openai",
            "gpt-test",
            Some(LlmRequestScope::new("session-a", "run-a").unwrap()),
            Vec::new(),
            &[],
        )
        .unwrap();
        let response_b = parse_terminal_response(
            &completed_with_call(),
            "openai",
            "gpt-test",
            Some(LlmRequestScope::new("session-b", "run-b").unwrap()),
            Vec::new(),
            &[],
        )
        .unwrap();
        let call_a = response_a.tool_calls.as_ref().unwrap()[0]
            .call_id
            .clone()
            .unwrap();
        let call_b = response_b.tool_calls.as_ref().unwrap()[0]
            .call_id
            .clone()
            .unwrap();
        let invocation_a = response_a.tool_calls.as_ref().unwrap()[0].invocation_id;
        let invocation_b = response_b.tool_calls.as_ref().unwrap()[0].invocation_id;
        assert_eq!(call_a.as_str(), call_b.as_str());
        assert_ne!(
            call_a, call_b,
            "private continuation provenance was not bound"
        );

        let mut continuation_a = response_a.continuation.unwrap();
        let error = continuation_a
            .record_tool_output(invocation_b, &json!({"wrong": true}))
            .expect_err("another session's token must be rejected");
        assert!(error.contains("does not belong"));
        continuation_a
            .record_tool_output(invocation_a, &json!({"right": true}))
            .expect("originating token remains valid");
    }

    #[test]
    fn incomplete_and_failed_statuses_are_not_completed_results() {
        for terminal in [
            json!({
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": []
            }),
            json!({
                "status": "failed",
                "error": {"code": "server_error", "message": "failed"},
                "output": []
            }),
        ] {
            let error = parse_terminal_response(&terminal, "openai", "gpt", None, Vec::new(), &[])
                .expect_err("terminal must fail");
            assert!(error.contains("Responses API"));
        }
    }

    #[test]
    fn refusal_is_typed_and_redacted_but_mixed_tool_output_fails() {
        let refusal = json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "status": "completed",
                "content": [{"type": "refusal", "refusal": "private refusal text"}]
            }]
        });
        let response =
            parse_terminal_response(&refusal, "openai", "gpt", None, Vec::new(), &[]).unwrap();
        assert_eq!(response.terminal_status, LlmTerminalStatus::Refused);
        assert_eq!(response.finish_reason, "refusal");
        assert!(response.content.is_none());
        assert!(response.tool_calls.is_none());
        assert!(response.continuation.is_none());
        assert!(!format!("{response:?}").contains("private refusal text"));

        let mixed = json!({
            "status": "completed",
            "output": [
                {"type": "message", "content": [
                    {"type": "refusal", "refusal": "private refusal text"}
                ]},
                {"type": "function_call", "call_id": "private_call", "name": "unsafe", "arguments": "{}"}
            ]
        });
        let error = parse_terminal_response(&mixed, "openai", "gpt", None, Vec::new(), &[])
            .expect_err("mixed refusal and function call must fail");
        assert!(!error.contains("private refusal text"));
        assert!(!error.contains("private_call"));
    }

    #[tokio::test]
    async fn stream_projects_deltas_but_uses_completed_envelope_privately() {
        let body = sse(&[
            json!({"type": "response.output_text.delta", "delta": "inspected"}),
            json!({"type": "response.output_text.delta", "delta": " successfully"}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {
                "type": "function_call",
                "call_id": "call_private_123",
                "name": "read_file",
                "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"path\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2, "delta": "\"README.md\"}"}),
            json!({"type": "response.completed", "response": completed_with_call()}),
        ]);
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let client = OpenAiResponsesClient::new(
            "openai",
            "gpt-test".to_string(),
            vec!["key".to_string()],
            format!("{base_url}/v1/responses"),
        );
        let messages = [LlmMessage::user("inspect")];
        let mut stream = client
            .stream(
                LlmRequest::new(&messages, None)
                    .with_scope(LlmRequestScope::new("session", "run").unwrap()),
            )
            .await
            .expect("Responses stream");
        let mut public = Vec::new();
        while let Some(event) = stream.recv().await {
            public.push(event.expect("valid public event"));
        }

        assert!(matches!(&public[0], StreamEvent::Content(text) if text == "inspected"));
        assert!(matches!(&public[1], StreamEvent::Content(text) if text == " successfully"));
        assert!(matches!(
            &public[2],
            StreamEvent::ToolCallChunk { index: 2, name: Some(name), arguments: None }
                if name == "read_file"
        ));
        assert!(matches!(public.last(), Some(StreamEvent::End(reason)) if reason == "tool_calls"));
        assert!(!format!("{public:?}").contains("call_private_123"));

        let completion = stream.finish().await.expect("private completion");
        let call = &completion.tool_calls.unwrap()[0];
        assert_eq!(call.call_id.as_ref().unwrap().as_str(), "call_private_123");
        assert!(completion.continuation.is_some());
    }

    #[tokio::test]
    async fn stream_reconstructs_multiple_interleaved_fragmented_calls_privately() {
        let terminal = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "encrypted_content": "private-reasoning"},
                {"type": "function_call", "call_id": "call_alpha", "name": "alpha", "arguments": "{\"value\":1}"},
                {"type": "function_call", "call_id": "call_beta", "name": "beta", "arguments": "{\"value\":2}"}
            ]
        });
        let body = sse(&[
            json!({"type": "response.output_item.added", "output_index": 1, "item": {
                "type": "function_call", "call_id": "call_beta", "name": "beta", "arguments": ""
            }}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "call_id": "call_alpha", "name": "alpha", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"value\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"value\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "2}"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "1}"}),
            json!({"type": "response.completed", "response": terminal}),
        ]);
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let client = OpenAiResponsesClient::new(
            "openai",
            "gpt-test".to_string(),
            vec!["key".to_string()],
            format!("{base_url}/v1/responses"),
        );
        let messages = [LlmMessage::user("run both")];
        let mut stream = client
            .stream(
                LlmRequest::new(&messages, None)
                    .with_scope(LlmRequestScope::new("session", "run").unwrap()),
            )
            .await
            .expect("Responses stream");
        let mut public = Vec::new();
        while let Some(event) = stream.recv().await {
            public.push(event.expect("valid public event"));
        }
        assert!(!format!("{public:?}").contains("call_alpha"));
        assert!(!format!("{public:?}").contains("private-reasoning"));

        let completion = stream.finish().await.expect("private completion");
        let calls = completion.tool_calls.expect("multiple calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].call_id.as_ref().unwrap().as_str(), "call_alpha");
        assert_eq!(calls[0].arguments["value"], 1);
        assert_eq!(calls[1].call_id.as_ref().unwrap().as_str(), "call_beta");
        assert_eq!(calls[1].arguments["value"], 2);
    }

    #[test]
    fn terminal_parser_aggregates_messages_and_multiple_function_calls_in_order() {
        let terminal = json!({
            "status": "completed",
            "output": [
                {"type": "message", "content": [
                    {"type": "output_text", "text": "first "}
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "one", "arguments": "{\"n\":1}"},
                {"type": "reasoning", "encrypted_content": "private"},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "second"}
                ]},
                {"type": "function_call", "call_id": "call_2", "name": "two", "arguments": "{\"n\":2}"}
            ]
        });
        let response = parse_terminal_response(
            &terminal,
            "openai",
            "gpt",
            Some(LlmRequestScope::new("session", "run").unwrap()),
            Vec::new(),
            &[],
        )
        .unwrap();

        assert_eq!(response.content.as_deref(), Some("first second"));
        let calls = response.tool_calls.unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "one");
        assert_eq!(calls[0].call_id.as_ref().unwrap().as_str(), "call_1");
        assert_eq!(calls[1].name, "two");
        assert_eq!(calls[1].call_id.as_ref().unwrap().as_str(), "call_2");
        assert!(response.continuation.is_some());
    }

    #[test]
    fn whitespace_stream_deltas_are_preserved() {
        let StreamAction::Public(text) = parse_stream_event(
            &json!({"type": "response.output_text.delta", "delta": " "}),
            "openai",
            "gpt",
            None,
            &[],
            &[],
        )
        .unwrap() else {
            panic!("text projection");
        };
        assert_eq!(text, [StreamEvent::Content(" ".to_string())]);

        let StreamAction::Public(arguments) = parse_stream_event(
            &json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": " "
            }),
            "openai",
            "gpt",
            None,
            &[],
            &[],
        )
        .unwrap() else {
            panic!("arguments projection");
        };
        assert!(matches!(
            &arguments[0],
            StreamEvent::ToolCallChunk { arguments: Some(delta), .. } if delta == " "
        ));
    }

    #[tokio::test]
    async fn dropping_stream_closes_the_open_http_response() {
        let (base_url, _request_rx, disconnect_rx) =
            crate::llm::test_support::serve_open_stream(sse(&[json!({
                "type": "response.output_text.delta",
                "delta": "first"
            })]));
        let client = OpenAiResponsesClient::new(
            "openai",
            "gpt-test".to_string(),
            vec!["key".to_string()],
            format!("{base_url}/v1/responses"),
        );
        let messages = [LlmMessage::user("inspect")];
        let mut stream = client
            .stream(LlmRequest::new(&messages, None))
            .await
            .expect("open Responses stream");
        assert_eq!(
            stream.recv().await.unwrap().unwrap(),
            StreamEvent::Content("first".to_string())
        );
        drop(stream);

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .unwrap()
                .unwrap();
        assert!(disconnected, "Responses connection remained open");
    }

    #[tokio::test]
    async fn stream_rejects_eof_and_done_without_typed_completion() {
        for body in [
            sse(&[json!({
                "type": "response.output_text.delta",
                "delta": "partial"
            })]),
            "data: [DONE]\n\n".to_string(),
        ] {
            let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
            let client = OpenAiResponsesClient::new(
                "openai",
                "gpt-test".to_string(),
                vec!["key".to_string()],
                format!("{base_url}/v1/responses"),
            );
            let messages = [LlmMessage::user("inspect")];
            let stream = client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect("stream starts");
            let error = stream.finish().await.expect_err("terminal event required");
            assert!(error.contains("response.completed"));
        }
    }

    #[test]
    fn typed_stream_failures_are_terminal() {
        for event in [
            json!({"type": "response.failed", "response": {
                "status": "failed", "error": {"code": "server_error", "message": "failed"}
            }}),
            json!({"type": "response.incomplete", "response": {
                "status": "incomplete", "incomplete_details": {"reason": "content_filter"}
            }}),
            json!({"type": "error", "code": "bad_request", "message": "failed"}),
        ] {
            assert!(parse_stream_event(&event, "openai", "gpt", None, &[], &[]).is_err());
        }
    }

    #[tokio::test]
    async fn complete_and_stream_structural_errors_keep_their_typed_classification() {
        const SENTINEL: &str = "remote-provider-message";
        let terminal = json!({
            "id": "resp_failed",
            "status": "failed",
            "error": {"code": "server_error", "message": SENTINEL},
            "output": []
        });
        let (base_url, _) = serve_once("200 OK", "application/json", terminal.to_string());
        let client = OpenAiResponsesClient::new(
            "openai",
            "gpt-test".to_string(),
            vec!["key".to_string()],
            format!("{base_url}/v1/responses"),
        );
        let messages = [LlmMessage::user("inspect")];
        let complete_error = client
            .complete(LlmRequest::new(&messages, None))
            .await
            .expect_err("provider failure must reject completion");

        let body = sse(&[json!({
            "type": "response.failed",
            "response": {
                "status": "failed",
                "error": {"code": "server_error", "message": SENTINEL},
                "output": []
            }
        })]);
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let client = OpenAiResponsesClient::new(
            "openai",
            "gpt-test".to_string(),
            vec!["key".to_string()],
            format!("{base_url}/v1/responses"),
        );
        let stream_error = client
            .stream(LlmRequest::new(&messages, None))
            .await
            .expect("stream starts")
            .finish()
            .await
            .expect_err("provider failure must terminate the stream");

        assert_eq!(
            complete_error.class,
            crate::llm::LlmErrorClass::ProviderUnavailable
        );
        assert_eq!(
            stream_error.class,
            crate::llm::LlmErrorClass::ProviderUnavailable
        );
        assert_eq!(
            complete_error.phase,
            crate::llm::LlmErrorPhase::TerminalValidation
        );
        assert_eq!(stream_error.phase, crate::llm::LlmErrorPhase::Stream);
        assert_eq!(stream_error.provider, "openai");
        assert_eq!(stream_error.transport, RESPONSES_TRANSPORT);
        assert!(!complete_error.user_report(None).contains(SENTINEL));
        assert!(!stream_error.user_report(None).contains(SENTINEL));
    }

    #[tokio::test]
    async fn http_errors_are_bounded_and_redact_credentials() {
        let secret = "test-api-key-secret";
        let prompt_echo = "private-user-prompt-must-not-escape";
        let body = json!({
            "error": {
                "type": prompt_echo,
                "code": prompt_echo,
                "param": prompt_echo,
                "message": format!("credential {secret}: {prompt_echo}\n\u{1b}[31m{}", "x".repeat(10_000))
            }
        });
        let (base_url, _) = serve_once("400 Bad Request", "application/json", body.to_string());
        let client = OpenAiResponsesClient::new(
            format!("openai-{secret}"),
            format!("gpt-{secret}\n"),
            vec![secret.to_string()],
            format!("{base_url}/v1/responses"),
        );
        let messages = [LlmMessage::user("inspect")];
        let error = client
            .complete(LlmRequest::new(&messages, None))
            .await
            .expect_err("HTTP error");
        assert_eq!(error.class, crate::llm::LlmErrorClass::UnsupportedRequest);
        assert_eq!(error.phase, crate::llm::LlmErrorPhase::HttpResponse);
        assert_eq!(error.http_status, Some(400));
        assert!(error.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(error.contains("[REDACTED]"));
        assert!(!error.contains(secret));
        assert!(!error.contains(prompt_echo));
        assert!(!error.contains('\n'));
        assert!(!error.contains('\u{1b}'));
        assert!(!error.contains(&"x".repeat(5_000)));
    }

    #[test]
    fn strict_defaults_false_but_explicit_values_are_preserved() {
        let loose = ToolDefinition::function("loose").to_responses_tool();
        let strict = ToolDefinition::function("strict")
            .with_strict(true)
            .to_responses_tool();
        assert_eq!(loose["strict"], false);
        assert_eq!(strict["strict"], true);
    }
}
