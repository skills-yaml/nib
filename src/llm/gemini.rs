//! Google Gemini Generative Language API.

use crate::llm::types::{
    LlmRequest, LlmRequestScope, LlmResponse, LlmTerminalStatus, ProviderContinuation, StreamEvent,
    ToolCallRequest,
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
    let retained_function_names = model_content
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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
    let (state, outputs): (GeminiTurnState, BTreeMap<ToolInvocationId, String>) =
        continuation.consume("google", model, GEMINI_TRANSPORT, scope)?;
    let mut parts = Vec::with_capacity(state.calls.len());
    for (invocation_id, name) in state.calls {
        let output = outputs
            .get(&invocation_id)
            .ok_or_else(|| "Gemini continuation is missing a tool output".to_string())?;
        let mut response = serde_json::from_str::<Value>(output)
            .map_err(|_| "Gemini continuation contains an invalid tool output".to_string())?;
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
        Ok(Self {
            client: Client::new(),
            model,
            api_keys,
            base_url: normalize_gemini_api_root(base_url.as_ref())?,
        })
    }

    fn request_body(&self, request: LlmRequest<'_>) -> Result<Value, String> {
        let LlmRequest {
            messages,
            tools,
            temperature,
            max_output_tokens,
            reasoning_effort: _,
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
        let mut body = json!({
            "contents": contents,
            "generationConfig": {"temperature": temperature},
        });
        if let Some(max_output_tokens) = max_output_tokens {
            if max_output_tokens == 0 {
                return Err("max_output_tokens must be greater than zero".to_string());
            }
            body["generationConfig"]["maxOutputTokens"] = json!(max_output_tokens);
        }
        if let Some(system) = messages
            .iter()
            .find(|message| message.get("role") == Some(&json!("system")))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
        {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }
        if let Some(declarations) = tools.map(gemini_function_declarations).transpose()? {
            if !declarations.is_empty() {
                body["tools"] = json!([{"functionDeclarations": declarations}]);
            }
        }
        Ok(body)
    }
}

#[async_trait]
impl LlmClient for GeminiClient {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, String> {
        validate_gemini_request(&request)?;
        let scope = request.scope.clone();
        let body = self.request_body(request)?;
        let response = crate::llm::send_with_retry(
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
        )
        .await?;
        if !response.status().is_success() {
            return Err(safe_gemini_http_error(response).await);
        }
        let data =
            crate::llm::read_bounded_json_response(response, "Gemini completion response").await?;
        let mut completed = parse_gemini_response(&data)?;
        if let Some(calls) = completed
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        {
            let model_content = data["candidates"][0]
                .get("content")
                .cloned()
                .ok_or_else(|| "Gemini response function calls are missing content".to_string())?;
            completed.continuation = Some(gemini_continuation(
                &self.model,
                scope,
                model_content,
                calls,
            )?);
        }
        Ok(completed)
    }

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, String> {
        validate_gemini_request(&request)?;
        let continuation_scope = request.scope.clone();
        let body = self.request_body(request)?;
        let response = crate::llm::send_with_retry(
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
        )
        .await?;
        if !response.status().is_success() {
            return Err(safe_gemini_http_error(response).await);
        }
        crate::llm::ensure_response_content_length(
            &response,
            crate::llm::MAX_LLM_STREAM_BYTES,
            "Gemini stream response",
        )?;

        let (public_tx, public_rx) = mpsc::channel(100);
        let (completion_tx, completion_rx) = oneshot::channel();
        let model = self.model.clone();
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
            let mut parser = GeminiStreamParser::default();
            let mut retained_parts = Vec::new();
            while let Some(event_result) =
                crate::llm::next_stream_item_or_closed(&public_tx, &mut stream).await
            {
                let event = match event_result {
                    Ok(event) => event,
                    Err(EventStreamError::Transport(error)) => {
                        fail_gemini_stream(&public_tx, &mut completion_tx, error).await;
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
                        fail_gemini_stream(&public_tx, &mut completion_tx, error).await;
                        return;
                    }
                };
                for parsed in events {
                    match parsed {
                        StreamEvent::End(reason) => {
                            if reason.trim().is_empty() {
                                fail_gemini_stream(
                                    &public_tx,
                                    &mut completion_tx,
                                    "Gemini stream contained an invalid terminal reason"
                                        .to_string(),
                                )
                                .await;
                                return;
                            }
                            let completion = match complete_gemini_stream(
                                content,
                                tool_calls,
                                reason.clone(),
                                &model,
                                continuation_scope,
                                retained_parts,
                            ) {
                                Ok(completion) => completion,
                                Err(error) => {
                                    fail_gemini_stream(&public_tx, &mut completion_tx, error).await;
                                    return;
                                }
                            };
                            if !crate::llm::send_stream_event(
                                &public_tx,
                                Ok(StreamEvent::End(reason)),
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
                        StreamEvent::Content(fragment) => {
                            content.push_str(&fragment);
                            if !crate::llm::send_stream_event(
                                &public_tx,
                                Ok(StreamEvent::Content(fragment)),
                            )
                            .await
                            {
                                return;
                            }
                        }
                        event @ StreamEvent::ToolCallChunk { .. } => {
                            tool_calls.push(&event);
                            if !crate::llm::send_stream_event(&public_tx, Ok(event)).await {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !public_tx.is_closed() {
                fail_gemini_stream(
                    &public_tx,
                    &mut completion_tx,
                    "Gemini stream ended before an explicit finishReason".to_string(),
                )
                .await;
            }
        });
        Ok(LlmStream::with_private_completion(public_rx, completion_rx))
    }
}

fn validate_gemini_request(request: &LlmRequest<'_>) -> Result<(), String> {
    if request.reasoning_effort.is_some() {
        return Err("Gemini requests do not support reasoning_effort".to_string());
    }
    Ok(())
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

async fn safe_gemini_http_error(response: Response) -> String {
    let status = response.status().as_u16();
    let body_read =
        crate::llm::read_bounded_error_response(response, "Gemini API error response").await;
    if body_read.is_err() {
        format!(
            "Gemini API request failed with HTTP {status}; the error response could not be read safely"
        )
    } else {
        format!("Gemini API request failed with HTTP {status}")
    }
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

fn gemini_terminal_status(reason: &str, has_tool_calls: bool) -> Result<LlmTerminalStatus, String> {
    match reason {
        "STOP" => Ok(LlmTerminalStatus::Completed),
        reason if is_gemini_refusal_reason(reason) && !has_tool_calls => {
            Ok(LlmTerminalStatus::Refused)
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

fn normalized_gemini_prompt_block(data: &Value) -> Result<Option<String>, String> {
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
        Ok(Some("SAFETY".to_string()))
    } else {
        Err("Gemini response contained an unsupported prompt block reason".to_string())
    }
}

fn complete_gemini_stream(
    content: String,
    tool_calls: crate::llm::ToolCallAccumulator,
    finish_reason: String,
    model: &str,
    scope: Option<LlmRequestScope>,
    retained_parts: Vec<Value>,
) -> Result<LlmResponse, String> {
    let tool_calls = tool_calls.finish()?;
    let terminal_status = gemini_terminal_status(&finish_reason, !tool_calls.is_empty())?;
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
    })
}

async fn fail_gemini_stream(
    public_tx: &mpsc::Sender<Result<StreamEvent, String>>,
    completion_tx: &mut Option<oneshot::Sender<Result<LlmResponse, String>>>,
    error: String,
) {
    let _ = crate::llm::send_stream_event(public_tx, Err(error.clone())).await;
    if let Some(sender) = completion_tx.take() {
        let _ = sender.send(Err(error));
    }
}

fn gemini_contents(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message.get("role") != Some(&json!("system")))
        .map(|message| {
            let role = if message.get("role") == Some(&json!("assistant")) {
                "model"
            } else {
                "user"
            };
            json!({
                "role": role,
                "parts": [{"text": message.get("content").and_then(Value::as_str).unwrap_or_default()}]
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
                "parameters": parameters,
            }))
        })
        .collect()
}

pub fn parse_gemini_response(data: &Value) -> Result<LlmResponse, String> {
    if data.get("error").is_some() {
        return Err("Gemini completion reported a provider error".to_string());
    }
    if let Some(reason) = normalized_gemini_prompt_block(data)? {
        return Ok(LlmResponse {
            terminal_status: LlmTerminalStatus::Refused,
            content: None,
            tool_calls: None,
            finish_reason: reason,
            continuation: None,
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
    let finish_reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .ok_or_else(|| "Gemini response is missing a finish reason".to_string())?
        .to_string();
    let parts = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array);
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
    let terminal_status = gemini_terminal_status(&finish_reason, !tool_calls.is_empty())?;
    Ok(LlmResponse {
        terminal_status,
        content: (!content.is_empty()).then_some(content),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        finish_reason,
        continuation: None,
    })
}

#[derive(Default)]
struct GeminiStreamParser {
    next_call_index: usize,
    provider_call_indexes: BTreeMap<String, usize>,
}

impl GeminiStreamParser {
    fn parse_chunk(&mut self, data: &Value) -> Result<Vec<StreamEvent>, String> {
        if data.get("error").is_some() {
            return Err("Gemini stream reported a provider error".to_string());
        }
        if let Some(reason) = normalized_gemini_prompt_block(data)? {
            return Ok(vec![StreamEvent::End(reason)]);
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
            for part in parts {
                let mut recognized = false;
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    recognized = true;
                    events.push(StreamEvent::Content(text.to_string()));
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
                    events.push(StreamEvent::ToolCallChunk {
                        index,
                        name: Some(name.to_string()),
                        arguments: Some(arguments.to_string()),
                    });
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
            gemini_terminal_status(reason, false)?;
            events.push(StreamEvent::End(reason.to_string()));
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

pub fn parse_gemini_stream_chunk(data: &Value) -> Result<Vec<StreamEvent>, String> {
    GeminiStreamParser::default().parse_chunk(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ReasoningEffort;
    use crate::llm::test_support::{
        serve_once, serve_once_with_declared_length, serve_open_stream,
    };
    use crate::llm::types::{LlmRequestScope, ProviderContinuation};
    use std::time::Duration;

    fn test_client(base_url: String) -> GeminiClient {
        GeminiClient::with_base_url(
            "gemini-test".to_string(),
            vec!["gemini-test-key".to_string()],
            base_url,
        )
        .expect("test Gemini endpoint")
    }

    fn request_with_continuation(messages: &[Value]) -> LlmRequest<'_> {
        let scope = LlmRequestScope::new("test-session", "test-run").unwrap();
        let continuation = ProviderContinuation::new(
            "openai",
            "test-model",
            "responses",
            Some(scope.clone()),
            vec![crate::tools::ToolInvocationId::new()],
            1,
            0,
            (),
        )
        .unwrap();
        LlmRequest::new(messages, None, 0.0)
            .with_scope(scope)
            .with_continuation(Some(continuation))
    }

    #[test]
    fn converts_tools_to_gemini_function_declarations() {
        let declarations = gemini_function_declarations(&[json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {"type": "object", "required": ["path"]}
            }
        })])
        .expect("valid declarations");
        assert_eq!(declarations[0]["name"], "read_file");
        assert_eq!(declarations[0]["parameters"]["required"][0], "path");
    }

    #[test]
    fn normalizes_and_validates_custom_gemini_api_roots() {
        let root = GeminiClient::with_base_url(
            "gemini-test".to_string(),
            vec!["test-key".to_string()],
            "https://gateway.example/proxy",
        )
        .expect("custom root");
        assert_eq!(root.base_url, "https://gateway.example/proxy/v1beta");

        let exact = GeminiClient::with_base_url(
            "gemini-test".to_string(),
            vec!["test-key".to_string()],
            "https://gateway.example/proxy/v1beta/",
        )
        .expect("exact API root");
        assert_eq!(exact.base_url, "https://gateway.example/proxy/v1beta");

        for invalid in [
            "https://user:secret@gateway.example/proxy",
            "https://gateway.example/proxy?token=secret",
            "https://gateway.example/proxy#fragment",
            "https://gateway.example/v1beta/v1beta",
            "https://gateway.example/v1beta/models/gemini-test:generateContent",
        ] {
            assert!(GeminiClient::with_base_url(
                "gemini-test".to_string(),
                vec!["test-key".to_string()],
                invalid,
            )
            .is_err());
        }
    }

    #[tokio::test]
    async fn complete_and_stream_reject_foreign_continuations_and_unsupported_reasoning() {
        let client = test_client("http://127.0.0.1:9".to_string());
        let messages = [json!({"role": "user", "content": "x"})];

        let error = client
            .complete(request_with_continuation(&messages))
            .await
            .expect_err("completion must reject a foreign provider continuation");
        assert_eq!(
            error,
            "provider continuation does not match the provider, model, API mode, session, or run"
        );
        let error = client
            .stream(request_with_continuation(&messages))
            .await
            .expect_err("stream must reject a foreign provider continuation");
        assert_eq!(
            error,
            "provider continuation does not match the provider, model, API mode, session, or run"
        );

        let reasoning_request = || {
            LlmRequest::new(&messages, None, 0.0)
                .with_reasoning_effort(Some(ReasoningEffort::Medium))
        };
        let error = client
            .complete(reasoning_request())
            .await
            .expect_err("completion must reject unsupported reasoning");
        assert_eq!(error, "Gemini requests do not support reasoning_effort");
        let error = client
            .stream(reasoning_request())
            .await
            .expect_err("stream must reject unsupported reasoning");
        assert_eq!(error, "Gemini requests do not support reasoning_effort");
    }

    #[tokio::test]
    async fn complete_and_stream_reject_malformed_tools_before_io() {
        let client = test_client("http://127.0.0.1:9".to_string());
        let messages = [json!({"role": "user", "content": "x"})];
        let malformed_tools = [json!({"type": "function", "function": {"name": "read"}})];

        let error = client
            .complete(LlmRequest::new(&messages, Some(&malformed_tools), 0.0))
            .await
            .expect_err("completion must reject a malformed tool");
        assert_eq!(error, "Gemini tool definition parameters must be an object");
        let error = client
            .stream(LlmRequest::new(&messages, Some(&malformed_tools), 0.0))
            .await
            .expect_err("stream must reject a malformed tool");
        assert_eq!(error, "Gemini tool definition parameters must be an object");
    }

    #[test]
    fn complete_parser_rejects_missing_unsafe_and_malformed_terminal_states() {
        let missing = json!({
            "candidates": [{"content": {"parts": [{"text": "partial"}]}}]
        });
        assert!(parse_gemini_response(&missing)
            .expect_err("missing finish reason")
            .contains("missing a finish reason"));

        let truncated = json!({
            "candidates": [{
                "content": {"parts": [{"text": "partial"}]},
                "finishReason": "MAX_TOKENS"
            }]
        });
        assert!(parse_gemini_response(&truncated)
            .expect_err("truncation")
            .contains("truncated"));

        let malformed_call = json!({
            "candidates": [{
                "content": {"parts": [{"functionCall": {"name": "read_file"}}]},
                "finishReason": "STOP"
            }]
        });
        assert!(parse_gemini_response(&malformed_call)
            .expect_err("missing function arguments")
            .contains("arguments must be an object"));

        let unknown = json!({
            "candidates": [{"content": {"parts": []}, "finishReason": "REMOTE_FUTURE_VALUE"}]
        });
        let error = parse_gemini_response(&unknown).expect_err("unknown finish reason");
        assert_eq!(
            error,
            "Gemini response contained an unsupported terminal reason"
        );
        assert!(!error.contains("REMOTE_FUTURE_VALUE"));
    }

    #[test]
    fn complete_parser_maps_prompt_and_candidate_safety_blocks_to_refusal() {
        let prompt = parse_gemini_response(&json!({
            "promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}
        }))
        .expect("prompt refusal");
        assert_eq!(prompt.terminal_status, LlmTerminalStatus::Refused);
        assert_eq!(prompt.finish_reason, "SAFETY");
        assert!(prompt.tool_calls.is_none());

        let candidate = parse_gemini_response(&json!({
            "candidates": [{"finishReason": "SAFETY"}]
        }))
        .expect("candidate refusal");
        assert_eq!(candidate.terminal_status, LlmTerminalStatus::Refused);
        assert!(candidate.tool_calls.is_none());
    }

    #[test]
    fn parses_gemini_streamed_function_call() {
        let events = parse_gemini_stream_chunk(&json!({
            "candidates": [{
                "content": {"parts": [{
                    "functionCall": {"name": "grep", "args": {"pattern": "needle"}}
                }]},
                "finishReason": "STOP"
            }]
        }))
        .unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::ToolCallChunk { name: Some(name), arguments: Some(arguments), .. }
                if name == "grep" && arguments.contains("needle")
        ));
        assert_eq!(events[1], StreamEvent::End("STOP".to_string()));
    }

    #[test]
    fn streamed_function_calls_keep_distinct_provider_identities_across_chunks() {
        let mut parser = GeminiStreamParser::default();
        let first = parser
            .parse_chunk(&json!({
                "candidates": [{
                    "content": {"parts": [{
                        "functionCall": {
                            "id": "provider-call-one",
                            "name": "read_file",
                            "args": {"path": "README.md"}
                        }
                    }]}
                }]
            }))
            .expect("first Gemini chunk");
        let second = parser
            .parse_chunk(&json!({
                "candidates": [{
                    "content": {"parts": [{
                        "functionCall": {
                            "id": "provider-call-two",
                            "name": "grep",
                            "args": {"pattern": "needle"}
                        }
                    }]},
                    "finishReason": "STOP"
                }]
            }))
            .expect("second Gemini chunk");

        assert_eq!(parser.provider_call_indexes["provider-call-one"], 0);
        assert_eq!(parser.provider_call_indexes["provider-call-two"], 1);
        let mut accumulator = crate::llm::ToolCallAccumulator::default();
        for event in first.iter().chain(&second) {
            accumulator.push(event);
        }
        let calls = accumulator.finish().expect("two Gemini function calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "README.md");
        assert_eq!(calls[1].name, "grep");
        assert_eq!(calls[1].arguments["pattern"], "needle");
    }

    #[test]
    fn idless_function_calls_from_separate_chunks_get_distinct_indexes() {
        let mut parser = GeminiStreamParser::default();
        let first = parser
            .parse_chunk(&json!({
                "candidates": [{"content": {"parts": [{
                    "functionCall": {"name": "first", "args": {}}
                }]}}]
            }))
            .expect("first Gemini chunk");
        let second = parser
            .parse_chunk(&json!({
                "candidates": [{"content": {"parts": [{
                    "functionCall": {"name": "second", "args": {}}
                }]}}]
            }))
            .expect("second Gemini chunk");

        assert!(matches!(
            first[0],
            StreamEvent::ToolCallChunk { index: 0, .. }
        ));
        assert!(matches!(
            second[0],
            StreamEvent::ToolCallChunk { index: 1, .. }
        ));
    }

    #[tokio::test]
    async fn complete_posts_gemini_payload_and_parses_text_and_function_calls() {
        let (base_url, request_rx) = serve_once(
            "200 OK",
            "application/json",
            json!({
                "candidates": [{
                    "content": {"parts": [
                        {"text": "checking"},
                        {"functionCall": {"name": "grep", "args": {"pattern": "needle"}}}
                    ]},
                    "finishReason": "STOP"
                }]
            })
            .to_string(),
        );
        let response = test_client(base_url)
            .complete(
                LlmRequest::new(
                    &[
                        json!({"role": "system", "content": "follow project rules"}),
                        json!({"role": "user", "content": "search"}),
                        json!({"role": "assistant", "content": "working"}),
                    ],
                    Some(&[json!({
                        "type": "function",
                        "function": {
                            "name": "grep",
                            "description": "search files",
                            "parameters": {"type": "object", "required": ["pattern"]}
                        }
                    })]),
                    0.4,
                )
                .with_max_output_tokens(47)
                .with_scope(LlmRequestScope::new("test-session", "test-run").unwrap()),
            )
            .await
            .expect("Gemini completion");

        assert_eq!(response.content.as_deref(), Some("checking"));
        assert_eq!(response.finish_reason, "STOP");
        let call = &response.tool_calls.expect("function call")[0];
        assert_eq!(call.name, "grep");
        assert_eq!(call.arguments["pattern"], "needle");

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured Gemini request");
        assert!(request.starts_with("POST /v1beta/models/gemini-test:generateContent HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-goog-api-key: gemini-test-key"));
        assert!(request.contains("\"maxOutputTokens\":47"));
        assert!(request.contains("\"systemInstruction\""));
        assert!(request.contains("\"functionDeclarations\""));
        assert!(request.contains("\"role\":\"model\""));
    }

    #[tokio::test]
    async fn stream_consumes_sse_text_and_function_calls() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"work\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}}}]},\"finishReason\":\"STOP\"}]}\n\n"
        );
        let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", body);
        let mut stream = test_client(base_url)
            .stream(
                LlmRequest::new(&[json!({"role": "user", "content": "read"})], None, 0.0)
                    .with_scope(LlmRequestScope::new("test-session", "test-run").unwrap()),
            )
            .await
            .expect("Gemini stream");
        let mut content = String::new();
        let mut accumulator = crate::llm::ToolCallAccumulator::default();
        let mut finish = None;
        while let Some(event) = stream.recv().await {
            let event = event.expect("valid Gemini event");
            if let StreamEvent::Content(fragment) = &event {
                content.push_str(fragment);
            }
            if let StreamEvent::End(reason) = &event {
                finish = Some(reason.clone());
            }
            accumulator.push(&event);
        }

        assert_eq!(content, "work");
        assert_eq!(finish.as_deref(), Some("STOP"));
        let calls = accumulator.finish().expect("streamed Gemini call");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "README.md");
        let completion = stream.finish().await.expect("private Gemini completion");
        assert_eq!(completion.content.as_deref(), Some("work"));
        assert_eq!(completion.finish_reason, "STOP");
        let private_calls = completion.tool_calls.expect("private tool authority");
        assert_eq!(private_calls[0].name, "read_file");
        assert_eq!(private_calls[0].arguments["path"], "README.md");
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured Gemini stream request");
        assert!(request
            .starts_with("POST /v1beta/models/gemini-test:streamGenerateContent?alt=sse HTTP/1.1"));
    }

    #[tokio::test]
    async fn dropping_stream_receiver_closes_the_open_http_response() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"first\"}]}}]}\n\n",
        );
        let mut stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "cancel"})],
                None,
                0.0,
            ))
            .await
            .expect("open Gemini stream");
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
        assert!(disconnected, "Gemini response connection remained open");
    }

    #[tokio::test]
    async fn terminal_chunk_terminates_an_open_response_without_post_terminal_events() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"late\"}]}}]}\n\n"
        ));
        let mut stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "finish"})],
                None,
                0.0,
            ))
            .await
            .expect("open Gemini stream");

        let terminal = tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("terminal event timeout")
            .expect("terminal event")
            .expect("valid terminal event");
        assert_eq!(terminal, StreamEvent::End("STOP".to_string()));
        assert!(tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("producer closure timeout")
            .is_none());

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "Gemini terminal event left response open");
    }

    #[tokio::test]
    async fn tool_chunk_followed_by_eof_cannot_produce_private_authority() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{",
            "\"name\":\"read_file\",\"args\":{\"path\":\"README.md\"}}}]}}]}\n\n"
        );
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "read"})],
                None,
                0.0,
            ))
            .await
            .expect("Gemini stream");

        let error = stream
            .finish()
            .await
            .expect_err("EOF before finishReason must fail closed");
        assert!(error.contains("ended before an explicit finishReason"));
    }

    #[tokio::test]
    async fn complete_and_stream_hide_http_error_bodies() {
        const SENTINEL: &str = "remote-secret-prompt-and-key";
        let (base_url, _) = serve_once("401 Unauthorized", "text/plain", SENTINEL);
        let error = test_client(base_url)
            .complete(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect_err("completion must reject HTTP error");
        assert_eq!(error, "Gemini API request failed with HTTP 401");
        assert!(!error.contains(SENTINEL));

        let (base_url, _) = serve_once("403 Forbidden", "text/plain", SENTINEL);
        let error = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect_err("stream must reject HTTP error");
        assert_eq!(error, "Gemini API request failed with HTTP 403");
        assert!(!error.contains(SENTINEL));
    }

    #[tokio::test]
    async fn stream_in_band_error_hides_remote_message() {
        const SENTINEL: &str = "remote-stream-secret";
        let (base_url, _) = serve_once(
            "200 OK",
            "text/event-stream",
            format!("data: {{\"error\":{{\"message\":\"{SENTINEL}\"}}}}\n\n"),
        );
        let stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect("Gemini stream response");
        let error = stream.finish().await.expect_err("provider stream error");
        assert_eq!(error, "Gemini stream reported a provider error");
        assert!(!error.contains(SENTINEL));
    }

    #[tokio::test]
    async fn stream_rejects_truncation_before_private_completion() {
        let (base_url, _) = serve_once(
            "200 OK",
            "text/event-stream",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"finishReason\":\"MAX_TOKENS\"}]}\n\n",
        );
        let stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect("Gemini stream response");
        let error = stream.finish().await.expect_err("truncated stream");
        assert!(error.contains("truncated"));
    }

    #[tokio::test]
    async fn oversized_declared_response_lengths_are_rejected() {
        let (base_url, _) = serve_once_with_declared_length(
            "200 OK",
            "application/json",
            "{}",
            Some(crate::llm::MAX_LLM_COMPLETE_RESPONSE_BYTES + 1),
        );
        let error = test_client(base_url)
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
        let error = test_client(base_url)
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
