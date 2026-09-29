//! Google Gemini Generative Language API.

use crate::llm::types::{
    LlmDelta, LlmFinishReason, LlmMessage, LlmRequest, LlmRequestScope, LlmResponse,
    LlmStreamEvent, LlmTerminalStatus, LlmUsage, ProviderContinuation, ToolCallRequest,
    ToolDefinition, ToolResult,
};
use crate::tools::ToolInvocationId;
use async_trait::async_trait;
use reqwest::{Client, Response, Url};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::{mpsc, oneshot};

use super::{LlmClient, LlmStream};

const GEMINI_TRANSPORT: &str = "gemini_generate_content";

struct GeminiTurnState {
    model_content: Value,
    calls: Vec<(ToolInvocationId, String)>,
}

fn gemini_continuation(
    model: &str,
    scope: Option<LlmRequestScope>,
    model_content: Value,
    calls: &[ToolCallRequest],
) -> Result<ProviderContinuation, String> {
    let retained_function_parts = model_content
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("functionCall").is_some())
        .collect::<Vec<_>>();
    for part in &retained_function_parts {
        if part.get("thoughtSignature").is_some_and(|signature| {
            signature.as_str().is_none_or(|signature| {
                signature.is_empty() || signature.chars().any(char::is_control)
            })
        }) {
            return Err(
                "Gemini continuation contains invalid thought-signature metadata".to_string(),
            );
        }
    }
    let retained_function_names = retained_function_parts
        .iter()
        .filter_map(|part| {
            part.get("functionCall")
                .and_then(|call| call.get("name"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>();
    if retained_function_names.len() != calls.len()
        || retained_function_names
            .iter()
            .zip(calls)
            .any(|(retained, call)| *retained != call.name)
    {
        return Err(
            "Gemini continuation function-call order does not match validated calls".to_string(),
        );
    }
    let calls = calls
        .iter()
        .map(|call| (call.invocation_id, call.name.clone()))
        .collect::<Vec<_>>();
    let encoded_bytes = serde_json::to_vec(&model_content)
        .map_err(|error| format!("failed to measure Gemini continuation: {error}"))?
        .len();
    ProviderContinuation::new_ordered(
        "google",
        model,
        GEMINI_TRANSPORT,
        scope,
        calls
            .iter()
            .map(|(invocation_id, _)| *invocation_id)
            .collect(),
        1,
        encoded_bytes,
        GeminiTurnState {
            model_content,
            calls,
        },
    )
}

fn into_gemini_contents(
    continuation: ProviderContinuation,
    model: &str,
    scope: Option<&LlmRequestScope>,
) -> Result<Vec<Value>, String> {
    let (state, outputs): (GeminiTurnState, BTreeMap<ToolInvocationId, ToolResult>) =
        continuation.consume("google", model, GEMINI_TRANSPORT, scope)?;
    let mut parts = Vec::with_capacity(state.calls.len());
    for (invocation_id, name) in state.calls {
        let output = outputs
            .get(&invocation_id)
            .ok_or_else(|| "Gemini continuation is missing a tool output".to_string())?;
        let mut response = output.output().clone();
        if !response.is_object() {
            response = json!({"result": response});
        }
        parts.push(json!({
            "functionResponse": {
                "name": name,
                "response": response,
            }
        }));
    }
    Ok(vec![
        state.model_content,
        json!({"role": "user", "parts": parts}),
    ])
}

pub struct GeminiClient {
    client: Client,
    model: String,
    api_keys: Vec<String>,
    diagnostic_secrets: Vec<String>,
    base_url: String,
}

impl GeminiClient {
    pub fn new(model: String, api_keys: Vec<String>) -> Self {
        Self::with_base_url(
            model,
            api_keys,
            "https://generativelanguage.googleapis.com/v1beta",
        )
        .expect("the built-in Gemini endpoint must be valid")
    }

    pub fn with_base_url(
        model: String,
        api_keys: Vec<String>,
        base_url: impl AsRef<str>,
    ) -> Result<Self, String> {
        let diagnostic_secrets = api_keys.clone();
        Self::configured_with_diagnostic_secrets(model, api_keys, diagnostic_secrets, base_url)
    }

    pub(crate) fn configured_with_diagnostic_secrets(
        model: String,
        api_keys: Vec<String>,
        diagnostic_secrets: Vec<String>,
        base_url: impl AsRef<str>,
    ) -> Result<Self, String> {
        Ok(Self {
            client: Client::new(),
            model,
            api_keys,
            diagnostic_secrets,
            base_url: normalize_gemini_api_root(base_url.as_ref())?,
        })
    }

    pub(crate) fn request_body(&self, request: LlmRequest<'_>) -> Result<Value, String> {
        let LlmRequest {
            messages,
            tools,
            options,
            max_output_tokens,
            tool_choice,
            scope,
            continuation,
        } = request;
        let mut contents = gemini_contents(messages);
        if let Some(continuation) = continuation {
            contents.extend(into_gemini_contents(
                continuation,
                &self.model,
                scope.as_ref(),
            )?);
        }
        let mut generation_config = serde_json::Map::new();
        if let Some(temperature) = options.temperature() {
            generation_config.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(max_output_tokens) = max_output_tokens {
            if max_output_tokens == 0 {
                return Err("max_output_tokens must be greater than zero".to_string());
            }
            generation_config.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
        }
        let mut body = json!({
            "contents": contents,
        });
        if !generation_config.is_empty() {
            body["generationConfig"] = Value::Object(generation_config);
        }
        if let Some(system) = messages
            .iter()
            .find(|message| message.role == crate::llm::LlmMessageRole::System)
        {
            body["systemInstruction"] = json!({"parts": [{"text": system.content}]});
        }
        if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
            let declarations = tools
                .iter()
                .map(ToolDefinition::to_gemini_declaration)
                .collect::<Vec<_>>();
            body["tools"] = json!([{"functionDeclarations": declarations}]);
            if let Some(mode) = tool_choice.as_gemini_mode() {
                body["toolConfig"] = json!({
                    "functionCallingConfig": {"mode": mode}
                });
            }
        }
        Ok(body)
    }

    fn error_context(
        &self,
        retry_attempts: crate::llm::RetryAttemptMetadata,
    ) -> crate::llm::error::LlmErrorContext {
        crate::llm::error::LlmErrorContext::new(
            "google",
            GEMINI_TRANSPORT,
            Some(self.model.clone()),
            self.diagnostic_secrets.clone(),
            retry_attempts,
        )
    }

    fn request_error(&self, message: impl AsRef<str>) -> crate::llm::LlmError {
        crate::llm::LlmError::request_rejected(
            "google",
            GEMINI_TRANSPORT,
            Some(&self.model),
            message,
            &self.diagnostic_secrets,
        )
    }

    fn protocol_error(&self, message: impl AsRef<str>) -> crate::llm::LlmError {
        crate::llm::LlmError::provider_protocol(
            "google",
            GEMINI_TRANSPORT,
            Some(&self.model),
            crate::llm::LlmErrorPhase::TerminalValidation,
            message,
            &self.diagnostic_secrets,
        )
    }
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
#[async_trait]
impl LlmClient for GeminiClient {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, crate::llm::LlmError> {
        crate::llm::conformance::validate_request_capabilities(
            &request,
            "google",
            crate::llm::registry::ProviderTransport::GeminiGenerateContent,
            crate::llm::conformance::ProviderOperation::Complete,
        )
        .map_err(|error| self.request_error(error))?;
        validate_gemini_request(&request).map_err(|error| self.request_error(error))?;
        let scope = request.scope.clone();
        let body = self
            .request_body(request)
            .map_err(|error| self.request_error(error))?;
        let retry_capabilities = crate::llm::registry::retry_capabilities(
            "google",
            crate::llm::registry::ProviderTransport::GeminiGenerateContent,
        );
        let outcome = crate::llm::send_with_retry(
            |credential_index| {
                let url = format!(
                    "{}/models/{}:generateContent",
                    self.base_url.trim_end_matches('/'),
                    self.model
                );
                self.client
                    .post(url)
                    .header("x-goog-api-key", &self.api_keys[credential_index])
                    .json(&body)
            },
            self.api_keys.len(),
            retry_capabilities,
        )
        .await
        .map_err(|failure| {
            crate::llm::LlmError::transport(
                "google",
                GEMINI_TRANSPORT,
                Some(&self.model),
                "Gemini request failed before a valid HTTP response",
                &self.diagnostic_secrets,
            )
            .with_retry_attempts(failure.attempts)
        })?;
        let retry_attempts = outcome.attempts;
        let response = outcome.value;
        if !response.status().is_success() {
            return Err(safe_gemini_http_error(
                response,
                &self.model,
                &self.diagnostic_secrets,
                retry_attempts,
            )
            .await);
        }
        let data = crate::llm::read_bounded_json_response(response, "Gemini completion response")
            .await
            .map_err(|error| {
                self.protocol_error(error)
                    .with_retry_attempts(retry_attempts)
            })?;
        let mut completed = parse_gemini_response(&data).map_err(|error| {
            crate::llm::LlmError::provider_rejected(
                "google",
                GEMINI_TRANSPORT,
                Some(&self.model),
                crate::llm::LlmErrorPhase::TerminalValidation,
                Some(&data),
                error,
                &self.diagnostic_secrets,
            )
            .with_retry_attempts(retry_attempts)
        })?;
        if let Some(calls) = completed
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        {
            let model_content = data["candidates"][0]
                .get("content")
                .cloned()
                .ok_or_else(|| {
                    self.protocol_error("Gemini response function calls are missing content")
                        .with_retry_attempts(retry_attempts)
                })?;
            completed.continuation = Some(
                gemini_continuation(&self.model, scope, model_content, calls).map_err(|error| {
                    self.protocol_error(error)
                        .with_retry_attempts(retry_attempts)
                })?,
            );
        }
        completed.attempts = retry_attempts;
        Ok(completed)
    }

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, crate::llm::LlmError> {
        crate::llm::conformance::validate_request_capabilities(
            &request,
            "google",
            crate::llm::registry::ProviderTransport::GeminiGenerateContent,
            crate::llm::conformance::ProviderOperation::Stream,
        )
        .map_err(|error| self.request_error(error))?;
        validate_gemini_request(&request).map_err(|error| self.request_error(error))?;
        let continuation_scope = request.scope.clone();
        let body = self
            .request_body(request)
            .map_err(|error| self.request_error(error))?;
        let retry_capabilities = crate::llm::registry::retry_capabilities(
            "google",
            crate::llm::registry::ProviderTransport::GeminiGenerateContent,
        );
        let outcome = crate::llm::send_with_retry(
            |credential_index| {
                let url = format!(
                    "{}/models/{}:streamGenerateContent?alt=sse",
                    self.base_url.trim_end_matches('/'),
                    self.model
                );
                self.client
                    .post(url)
                    .header("x-goog-api-key", &self.api_keys[credential_index])
                    .json(&body)
            },
            self.api_keys.len(),
            retry_capabilities,
        )
        .await
        .map_err(|failure| {
            crate::llm::LlmError::transport(
                "google",
                GEMINI_TRANSPORT,
                Some(&self.model),
                "Gemini request failed before a valid HTTP response",
                &self.diagnostic_secrets,
            )
            .with_retry_attempts(failure.attempts)
        })?;
        let retry_attempts = outcome.attempts;
        let response = outcome.value;
        if !response.status().is_success() {
            return Err(safe_gemini_http_error(
                response,
                &self.model,
                &self.diagnostic_secrets,
                retry_attempts,
            )
            .await);
        }
        crate::llm::ensure_response_content_length(
            &response,
            crate::llm::MAX_LLM_STREAM_BYTES,
            "Gemini stream response",
        )
        .map_err(|error| {
            self.protocol_error(error)
                .with_retry_attempts(retry_attempts)
        })?;

        let (public_tx, public_rx) = mpsc::channel(100);
        let (completion_tx, completion_rx) = oneshot::channel();
        let model = self.model.clone();
        let sensitive_values = self.diagnostic_secrets.clone();
        tokio::spawn(async move {
            use eventsource_stream::{EventStreamError, Eventsource};
            use futures_util::StreamExt;

            let mut completion_tx = Some(completion_tx);
            let mut content = String::new();
            let mut tool_calls = crate::llm::ToolCallAccumulator::default();
            let mut budget = crate::llm::ResponseByteBudget::new(
                crate::llm::MAX_LLM_STREAM_BYTES,
                "Gemini stream response",
            );
            let bounded_bytes = response.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|_| {
                    "Gemini stream transport failed while reading a response".to_string()
                })?;
                budget.account(chunk.len())?;
                Ok::<_, String>(chunk)
            });
            let mut stream = bounded_bytes.eventsource();
            let mut item_budget = crate::llm::ResponseItemBudget::new(
                crate::llm::MAX_LLM_STREAM_EVENTS,
                "Gemini stream events",
            );
            let mut parser = GeminiStreamParser::default();
            let mut retained_parts = Vec::new();
            let mut usage = None;
            while let Some(event_result) =
                crate::llm::next_stream_item_or_closed(&public_tx, &mut stream).await
            {
                let event = match event_result {
                    Ok(event) => event,
                    Err(EventStreamError::Transport(error)) => {
                        fail_gemini_stream(
                            &public_tx,
                            &mut completion_tx,
                            crate::llm::LlmError::transport(
                                "google",
                                GEMINI_TRANSPORT,
                                Some(&model),
                                error,
                                &sensitive_values,
                            )
                            .with_phase(crate::llm::LlmErrorPhase::Stream),
                        )
                        .await;
                        return;
                    }
                    Err(EventStreamError::Utf8(_) | EventStreamError::Parser(_)) => {
                        fail_gemini_stream(
                            &public_tx,
                            &mut completion_tx,
                            "Gemini stream contained malformed SSE data".to_string(),
                        )
                        .await;
                        return;
                    }
                };
                if let Err(error) = item_budget.account(1) {
                    fail_gemini_stream(&public_tx, &mut completion_tx, error).await;
                    return;
                }
                if let Err(error) =
                    crate::llm::ensure_stream_event_size(event.data.len(), "Gemini stream event")
                {
                    fail_gemini_stream(&public_tx, &mut completion_tx, error).await;
                    return;
                }
                let data = match serde_json::from_str::<Value>(&event.data) {
                    Ok(data) => data,
                    Err(_) => {
                        fail_gemini_stream(
                            &public_tx,
                            &mut completion_tx,
                            "Gemini stream contained malformed JSON".to_string(),
                        )
                        .await;
                        return;
                    }
                };
                if data.get("error").is_some() {
                    fail_gemini_stream(
                        &public_tx,
                        &mut completion_tx,
                        crate::llm::LlmError::provider_rejected(
                            "google",
                            GEMINI_TRANSPORT,
                            Some(&model),
                            crate::llm::LlmErrorPhase::Stream,
                            Some(&data),
                            "Gemini stream reported a provider error",
                            &sensitive_values,
                        ),
                    )
                    .await;
                    return;
                }
                let chunk_usage = match parse_gemini_usage(&data) {
                    Ok(usage) => usage,
                    Err(error) => {
                        fail_gemini_stream(&public_tx, &mut completion_tx, error).await;
                        return;
                    }
                };
                if let Err(error) = merge_gemini_stream_usage(&mut usage, chunk_usage) {
                    fail_gemini_stream(&public_tx, &mut completion_tx, error).await;
                    return;
                }
                if let Some(parts) = data
                    .get("candidates")
                    .and_then(Value::as_array)
                    .and_then(|candidates| candidates.first())
                    .and_then(|candidate| candidate.get("content"))
                    .and_then(|content| content.get("parts"))
                    .and_then(Value::as_array)
                {
                    retained_parts.extend(parts.iter().cloned());
                }
                let events = match parser.parse_chunk(&data) {
                    Ok(events) => events,
                    Err(error) => {
                        fail_gemini_stream(
                            &public_tx,
                            &mut completion_tx,
                            gemini_stream_rejection(&model, Some(&data), error, &sensitive_values),
                        )
                        .await;
                        return;
                    }
                };
                for parsed in events {
                    match parsed {
                        LlmStreamEvent::Terminal(reason) => {
                            let completion = match complete_gemini_stream(
                                content,
                                tool_calls,
                                reason,
                                &model,
                                continuation_scope,
                                retained_parts,
                                usage,
                            ) {
                                Ok(completion) => completion.with_retry_attempts(retry_attempts),
                                Err(error) => {
                                    fail_gemini_stream(
                                        &public_tx,
                                        &mut completion_tx,
                                        gemini_stream_rejection(
                                            &model,
                                            Some(&data),
                                            error,
                                            &sensitive_values,
                                        ),
                                    )
                                    .await;
                                    return;
                                }
                            };
                            if !crate::llm::send_stream_event(
                                &public_tx,
                                Ok(LlmStreamEvent::Terminal(reason)),
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
                        LlmStreamEvent::Delta(LlmDelta::Content(fragment)) => {
                            content.push_str(&fragment);
                            if !crate::llm::send_stream_event(
                                &public_tx,
                                Ok(LlmStreamEvent::Delta(LlmDelta::Content(fragment))),
                            )
                            .await
                            {
                                return;
                            }
                        }
                        LlmStreamEvent::Delta(delta @ LlmDelta::ToolCallChunk { .. }) => {
                            tool_calls.push(&delta);
                            if !crate::llm::send_stream_event(
                                &public_tx,
                                Ok(LlmStreamEvent::Delta(delta)),
                            )
                            .await
                            {
                                return;
                            }
                        }
                    }
                }
            }
            if !public_tx.is_closed() {
                fail_gemini_stream(
                    &public_tx,
                    &mut completion_tx,
                    gemini_stream_rejection(
                        &model,
                        None,
                        "Gemini stream ended before an explicit finishReason",
                        &sensitive_values,
                    ),
                )
                .await;
            }
        });
        Ok(LlmStream::with_private_completion(public_rx, completion_rx)
            .with_error_context(self.error_context(retry_attempts)))
    }
}

fn validate_gemini_request(request: &LlmRequest<'_>) -> Result<(), String> {
    crate::llm::conformance::reject_unsupported_reasoning(request, "Gemini")
}

const MAX_NATIVE_BASE_URL_BYTES: usize = 4 * 1024;
const GEMINI_API_SUFFIX: &str = "/v1beta";

pub(crate) fn normalize_gemini_api_root(base_url: &str) -> Result<String, String> {
    let configured = base_url.trim();
    if configured.is_empty()
        || configured.len() > MAX_NATIVE_BASE_URL_BYTES
        || configured.contains('\0')
    {
        return Err(format!(
            "Gemini base_url must be between 1 and {MAX_NATIVE_BASE_URL_BYTES} bytes and contain no NUL"
        ));
    }
    let parsed = Url::parse(configured)
        .map_err(|_| "Gemini base_url must be a valid absolute HTTP(S) URL".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("Gemini base_url must be an absolute HTTP(S) URL".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Gemini base_url must not contain embedded credentials".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("Gemini base_url must not contain a query string or fragment".to_string());
    }

    let path = parsed.path().trim_end_matches('/');
    if path.ends_with(":generateContent") || path.ends_with(":streamGenerateContent") {
        return Err(
            "Gemini base_url must be an API root, not a single operation endpoint".to_string(),
        );
    }
    let exact_root = path.ends_with(GEMINI_API_SUFFIX);
    if exact_root {
        let prefix = path
            .strip_suffix(GEMINI_API_SUFFIX)
            .expect("matching Gemini API suffix");
        if prefix.trim_end_matches('/').ends_with(GEMINI_API_SUFFIX) {
            return Err("Gemini base_url contains a doubled API suffix".to_string());
        }
    }
    Ok(if exact_root {
        parsed.as_str().trim_end_matches('/').to_string()
    } else {
        format!(
            "{}{GEMINI_API_SUFFIX}",
            parsed.as_str().trim_end_matches('/')
        )
    })
}

async fn safe_gemini_http_error(
    response: Response,
    model: &str,
    sensitive_values: &[String],
    retry_attempts: crate::llm::RetryAttemptMetadata,
) -> crate::llm::LlmError {
    let status = response.status();
    let body_read =
        crate::llm::read_bounded_error_response(response, "Gemini API error response").await;
    let structured = body_read
        .as_ref()
        .ok()
        .and_then(|body| serde_json::from_str::<Value>(body).ok());
    let safe_message = if body_read.is_err() {
        format!(
            "Gemini API request failed with HTTP {}; the error response could not be read safely",
            status.as_u16()
        )
    } else {
        format!("Gemini API request failed with HTTP {}", status.as_u16())
    };
    crate::llm::LlmError::http(
        "google",
        GEMINI_TRANSPORT,
        Some(model),
        status,
        structured.as_ref(),
        safe_message,
        sensitive_values,
    )
    .with_retry_attempts(retry_attempts)
}

fn is_gemini_refusal_reason(reason: &str) -> bool {
    matches!(
        reason,
        "SAFETY"
            | "RECITATION"
            | "LANGUAGE"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "IMAGE_SAFETY"
            | "IMAGE_PROHIBITED_CONTENT"
            | "NO_IMAGE"
            | "IMAGE_RECITATION"
            | "IMAGE_OTHER"
    )
}

fn normalize_gemini_finish_reason(
    reason: &str,
    has_tool_calls: bool,
) -> Result<LlmFinishReason, String> {
    match reason {
        "STOP" if has_tool_calls => Ok(LlmFinishReason::ToolCalls),
        "STOP" => Ok(LlmFinishReason::Complete),
        reason if is_gemini_refusal_reason(reason) && !has_tool_calls => {
            Ok(LlmFinishReason::Refusal)
        }
        reason if is_gemini_refusal_reason(reason) => {
            Err("Gemini refusal contained executable function calls".to_string())
        }
        "MAX_TOKENS" => Err("Gemini response was truncated before completion".to_string()),
        "MALFORMED_FUNCTION_CALL" => Err("Gemini reported a malformed function call".to_string()),
        "UNEXPECTED_TOOL_CALL" => Err("Gemini reported an unexpected tool call".to_string()),
        "TOO_MANY_TOOL_CALLS" => Err("Gemini reported too many tool calls".to_string()),
        "MISSING_THOUGHT_SIGNATURE" => {
            Err("Gemini response was missing required continuation metadata".to_string())
        }
        "OTHER" | "FINISH_REASON_UNSPECIFIED" => {
            Err("Gemini response did not complete successfully".to_string())
        }
        _ => Err("Gemini response contained an unsupported terminal reason".to_string()),
    }
}

fn gemini_terminal_status(
    reason: LlmFinishReason,
    has_tool_calls: bool,
) -> Result<LlmTerminalStatus, String> {
    match (reason, has_tool_calls) {
        (LlmFinishReason::Complete | LlmFinishReason::Refusal, false)
        | (LlmFinishReason::ToolCalls, true) => Ok(reason.terminal_status()),
        (LlmFinishReason::Complete, true) => {
            Err("Gemini completion terminal contained executable function calls".to_string())
        }
        (LlmFinishReason::ToolCalls, false) => {
            Err("Gemini tool terminal contained no function calls".to_string())
        }
        (LlmFinishReason::Refusal, true) => {
            Err("Gemini refusal contained executable function calls".to_string())
        }
    }
}

fn normalized_gemini_prompt_block(data: &Value) -> Result<Option<LlmFinishReason>, String> {
    let Some(reason) = data
        .get("promptFeedback")
        .and_then(|feedback| feedback.get("blockReason"))
    else {
        return Ok(None);
    };
    let reason = reason
        .as_str()
        .filter(|reason| !reason.trim().is_empty())
        .ok_or_else(|| "Gemini prompt block reason is malformed".to_string())?;
    if matches!(
        reason,
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "OTHER"
    ) {
        Ok(Some(LlmFinishReason::Refusal))
    } else {
        Err("Gemini response contained an unsupported prompt block reason".to_string())
    }
}

fn complete_gemini_stream(
    content: String,
    tool_calls: crate::llm::ToolCallAccumulator,
    finish_reason: LlmFinishReason,
    model: &str,
    scope: Option<LlmRequestScope>,
    retained_parts: Vec<Value>,
    usage: Option<LlmUsage>,
) -> Result<LlmResponse, String> {
    let tool_calls = tool_calls.finish()?;
    let terminal_status = gemini_terminal_status(finish_reason, !tool_calls.is_empty())?;
    let completed_content = (!content.trim().is_empty()).then_some(content);
    let continuation = if tool_calls.is_empty() {
        None
    } else {
        Some(gemini_continuation(
            model,
            scope,
            json!({"role": "model", "parts": retained_parts}),
            &tool_calls,
        )?)
    };
    Ok(LlmResponse {
        terminal_status,
        content: completed_content,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        finish_reason,
        continuation,
        usage,
        attempts: crate::llm::RetryAttemptMetadata::no_network_attempt(),
    })
}

async fn fail_gemini_stream(
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

fn gemini_stream_rejection(
    model: &str,
    data: Option<&Value>,
    error: impl AsRef<str>,
    sensitive_values: &[String],
) -> crate::llm::LlmError {
    crate::llm::LlmError::provider_rejected(
        "google",
        GEMINI_TRANSPORT,
        Some(model),
        crate::llm::LlmErrorPhase::Stream,
        data,
        error,
        sensitive_values,
    )
}

fn gemini_contents(messages: &[LlmMessage]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message.role != crate::llm::LlmMessageRole::System)
        .map(|message| {
            let role = if message.role == crate::llm::LlmMessageRole::Assistant {
                "model"
            } else {
                "user"
            };
            json!({
                "role": role,
                "parts": [{"text": message.content}]
            })
        })
        .collect()
}

pub fn gemini_function_declarations(tools: &[Value]) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let tool = tool
                .as_object()
                .ok_or_else(|| "Gemini tool definition must be an object".to_string())?;
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err("Gemini tool definition type must be 'function'".to_string());
            }
            let function = tool
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| "Gemini tool definition is missing function".to_string())?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| "Gemini tool definition is missing a name".to_string())?;
            let description = match function.get("description") {
                Some(description) => description.as_str().ok_or_else(|| {
                    "Gemini tool definition description must be a string".to_string()
                })?,
                None => "",
            };
            let parameters = function
                .get("parameters")
                .filter(|schema| schema.is_object())
                .ok_or_else(|| "Gemini tool definition parameters must be an object".to_string())?;
            Ok(json!({
                "name": name,
                "description": description,
                "parameters": crate::llm::types::gemini_compatible_schema(parameters),
            }))
        })
        .collect()
}

fn required_gemini_usage_count(
    usage: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<u64, String> {
    usage
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("Gemini usage {field} must be a non-negative integer"))
}

fn optional_gemini_usage_count(
    usage: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<u64>, String> {
    match usage.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("Gemini usage {field} must be a non-negative integer")),
    }
}

fn parse_gemini_usage(data: &Value) -> Result<Option<LlmUsage>, String> {
    let Some(usage) = data.get("usageMetadata").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let usage = usage
        .as_object()
        .ok_or_else(|| "Gemini usageMetadata must be an object".to_string())?;
    let input = required_gemini_usage_count(usage, "promptTokenCount")?;
    let candidates = optional_gemini_usage_count(usage, "candidatesTokenCount")?.unwrap_or(0);
    let reasoning = optional_gemini_usage_count(usage, "thoughtsTokenCount")?;
    // Gemini reports thought tokens outside candidatesTokenCount but includes them in
    // totalTokenCount. Normalize output to the complete billed output before applying
    // the provider-neutral exact-total invariant.
    let output = candidates
        .checked_add(reasoning.unwrap_or(0))
        .ok_or_else(|| {
            "Gemini output usage overflowed while including thought tokens".to_string()
        })?;
    LlmUsage::new(
        input,
        output,
        required_gemini_usage_count(usage, "totalTokenCount")?,
        optional_gemini_usage_count(usage, "cachedContentTokenCount")?,
        reasoning,
    )
    .map(Some)
    .map_err(|error| format!("Gemini usage is invalid: {error}"))
}

fn merge_gemini_stream_usage(
    current: &mut Option<LlmUsage>,
    next: Option<LlmUsage>,
) -> Result<(), String> {
    let Some(next) = next else {
        return Ok(());
    };
    if let Some(prior) = current {
        if next.input_tokens != prior.input_tokens
            || next.output_tokens < prior.output_tokens
            || next.total_tokens < prior.total_tokens
            || next.cached_input_tokens != prior.cached_input_tokens
            || (prior.reasoning_output_tokens.is_some() && next.reasoning_output_tokens.is_none())
            || next
                .reasoning_output_tokens
                .zip(prior.reasoning_output_tokens)
                .is_some_and(|(next, prior)| next < prior)
        {
            return Err("Gemini cumulative stream usage was inconsistent".to_string());
        }
    }
    *current = Some(next);
    Ok(())
}

pub fn parse_gemini_response(data: &Value) -> Result<LlmResponse, String> {
    if data.get("error").is_some() {
        return Err("Gemini completion reported a provider error".to_string());
    }
    if let Some(finish_reason) = normalized_gemini_prompt_block(data)? {
        return Ok(LlmResponse {
            terminal_status: LlmTerminalStatus::Refused,
            content: None,
            tool_calls: None,
            finish_reason,
            continuation: None,
            usage: parse_gemini_usage(data)?,
            attempts: crate::llm::RetryAttemptMetadata::no_network_attempt(),
        });
    }
    let candidates = data
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| "Gemini response is missing candidates".to_string())?;
    if candidates.len() != 1 {
        return Err("Gemini response must contain exactly one candidate".to_string());
    }
    let candidate = &candidates[0];
    let native_finish_reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .ok_or_else(|| "Gemini response is missing a finish reason".to_string())?;
    let parts = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array);
    if let Some(parts) = parts {
        crate::llm::ensure_response_item_count(parts.len(), "Gemini content parts")?;
    }
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for part in parts.into_iter().flatten() {
        let mut recognized = false;
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            recognized = true;
            content.push_str(text);
        } else if part.get("text").is_some() {
            return Err("Gemini response text part is malformed".to_string());
        }
        if let Some(function) = part.get("functionCall") {
            recognized = true;
            let function = function
                .as_object()
                .ok_or_else(|| "Gemini function call is malformed".to_string())?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "Gemini function call is missing a name".to_string())?;
            let arguments = function
                .get("args")
                .filter(|arguments| arguments.is_object())
                .cloned()
                .ok_or_else(|| "Gemini function call arguments must be an object".to_string())?;
            tool_calls.push(ToolCallRequest::new(name, arguments));
        }
        if !recognized {
            return Err("Gemini response contains an unsupported content part".to_string());
        }
    }
    let finish_reason =
        normalize_gemini_finish_reason(native_finish_reason, !tool_calls.is_empty())?;
    let terminal_status = gemini_terminal_status(finish_reason, !tool_calls.is_empty())?;
    let usage = parse_gemini_usage(data)?;
    Ok(LlmResponse {
        terminal_status,
        content: (!content.is_empty()).then_some(content),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        finish_reason,
        continuation: None,
        usage,
        attempts: crate::llm::RetryAttemptMetadata::no_network_attempt(),
    })
}

#[derive(Default)]
struct GeminiStreamParser {
    next_call_index: usize,
    provider_call_indexes: BTreeMap<String, usize>,
}

impl GeminiStreamParser {
    fn parse_chunk(&mut self, data: &Value) -> Result<Vec<LlmStreamEvent>, String> {
        if data.get("error").is_some() {
            return Err("Gemini stream reported a provider error".to_string());
        }
        if let Some(reason) = normalized_gemini_prompt_block(data)? {
            return Ok(vec![LlmStreamEvent::Terminal(reason)]);
        }
        let candidates = data
            .get("candidates")
            .and_then(Value::as_array)
            .ok_or_else(|| "Gemini stream chunk is missing candidates".to_string())?;
        if candidates.len() != 1 {
            return Err("Gemini stream chunk must contain exactly one candidate".to_string());
        }
        let candidate = &candidates[0];
        let mut events = Vec::new();
        if let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            crate::llm::ensure_response_item_count(parts.len(), "Gemini streamed content parts")?;
            for part in parts {
                let mut recognized = false;
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    recognized = true;
                    events.push(LlmStreamEvent::Delta(LlmDelta::Content(text.to_string())));
                } else if part.get("text").is_some() {
                    return Err("Gemini stream text part is malformed".to_string());
                }
                if let Some(function) = part.get("functionCall") {
                    recognized = true;
                    let function = function
                        .as_object()
                        .ok_or_else(|| "Gemini stream function call is malformed".to_string())?;
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| "Gemini function call is missing a name".to_string())?;
                    let arguments = function
                        .get("args")
                        .filter(|arguments| arguments.is_object())
                        .ok_or_else(|| {
                            "Gemini function call arguments must be an object".to_string()
                        })?;
                    let index = self.call_index(function)?;
                    events.push(LlmStreamEvent::Delta(LlmDelta::ToolCallChunk {
                        index,
                        name: Some(name.to_string()),
                        arguments: Some(arguments.to_string()),
                    }));
                }
                if !recognized {
                    return Err("Gemini stream contains an unsupported content part".to_string());
                }
            }
        }
        if let Some(reason) = candidate.get("finishReason") {
            let reason = reason
                .as_str()
                .filter(|reason| !reason.trim().is_empty() && *reason != "null")
                .ok_or_else(|| "Gemini stream contains an invalid finish reason".to_string())?;
            events.push(LlmStreamEvent::Terminal(normalize_gemini_finish_reason(
                reason,
                self.next_call_index > 0,
            )?));
        }
        Ok(events)
    }

    fn call_index(&mut self, function: &serde_json::Map<String, Value>) -> Result<usize, String> {
        let provider_id = function
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        if let Some(index) = provider_id.and_then(|id| self.provider_call_indexes.get(id)) {
            return Ok(*index);
        }

        let index = self.next_call_index;
        self.next_call_index = self
            .next_call_index
            .checked_add(1)
            .ok_or_else(|| "Gemini stream contains too many function calls".to_string())?;
        if let Some(provider_id) = provider_id {
            self.provider_call_indexes
                .insert(provider_id.to_string(), index);
        }
        Ok(index)
    }
}

pub fn parse_gemini_stream_chunk(data: &Value) -> Result<Vec<LlmStreamEvent>, String> {
    GeminiStreamParser::default().parse_chunk(data)
}

#[cfg(test)]
#[path = "gemini_tests.rs"]
mod tests;
