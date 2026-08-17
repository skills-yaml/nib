//! Anthropic Messages API client.

use crate::llm::types::{
    LlmRequest, LlmRequestScope, LlmResponse, LlmTerminalStatus, ProviderCallId,
    ProviderContinuation, StreamEvent, ToolCallRequest,
};
use crate::tools::ToolInvocationId;
use async_trait::async_trait;
use reqwest::{Client, Response, Url};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::{mpsc, oneshot};

use super::{LlmClient, LlmStream};

const ANTHROPIC_TRANSPORT: &str = "anthropic_messages";

struct AnthropicTurnState {
    assistant_content: Vec<Value>,
    calls: Vec<(ToolInvocationId, ProviderCallId)>,
}

fn anthropic_continuation(
    model: &str,
    scope: Option<LlmRequestScope>,
    assistant_content: Vec<Value>,
    calls: &[ToolCallRequest],
) -> Result<ProviderContinuation, String> {
    let calls = calls
        .iter()
        .map(|call| {
            call.call_id
                .clone()
                .map(|provider_call_id| (call.invocation_id, provider_call_id))
                .ok_or_else(|| "Anthropic tool call is missing its provider call ID".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let encoded_bytes = serde_json::to_vec(&assistant_content)
        .map_err(|error| format!("failed to measure Anthropic continuation: {error}"))?
        .len();
    ProviderContinuation::new(
        "anthropic",
        model,
        ANTHROPIC_TRANSPORT,
        scope,
        calls
            .iter()
            .map(|(invocation_id, _)| *invocation_id)
            .collect(),
        assistant_content.len(),
        encoded_bytes,
        AnthropicTurnState {
            assistant_content,
            calls,
        },
    )
}

fn into_anthropic_messages(
    continuation: ProviderContinuation,
    model: &str,
    scope: Option<&LlmRequestScope>,
) -> Result<Vec<Value>, String> {
    let (state, outputs): (AnthropicTurnState, BTreeMap<ToolInvocationId, String>) =
        continuation.consume("anthropic", model, ANTHROPIC_TRANSPORT, scope)?;
    let mut results = Vec::with_capacity(state.calls.len());
    for (invocation_id, call_id) in state.calls {
        let output = outputs
            .get(&invocation_id)
            .ok_or_else(|| "Anthropic continuation is missing a tool output".to_string())?;
        let is_error = serde_json::from_str::<Value>(output)
            .ok()
            .and_then(|value| value.get("success").and_then(Value::as_bool))
            == Some(false);
        results.push(json!({
            "type": "tool_result",
            "tool_use_id": call_id.as_str(),
            "content": output,
            "is_error": is_error,
        }));
    }
    Ok(vec![
        json!({"role": "assistant", "content": state.assistant_content}),
        json!({"role": "user", "content": results}),
    ])
}

pub struct AnthropicClient {
    client: Client,
    model: String,
    api_keys: Vec<String>,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(model: String, api_keys: Vec<String>) -> Self {
        Self::with_base_url(model, api_keys, "https://api.anthropic.com/v1/messages")
            .expect("the built-in Anthropic endpoint must be valid")
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
            base_url: normalize_anthropic_endpoint(base_url.as_ref())?,
        })
    }

    fn request_body(&self, request: LlmRequest<'_>, stream: bool) -> Result<Value, String> {
        let LlmRequest {
            messages: request_messages,
            tools,
            temperature,
            max_output_tokens,
            reasoning_effort: _,
            scope,
            continuation,
        } = request;
        let system = request_messages
            .iter()
            .find(|message| message.get("role") == Some(&json!("system")))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("You are nib, a coding agent.");
        let mut messages = request_messages
            .iter()
            .filter(|message| message.get("role") != Some(&json!("system")))
            .map(|message| {
                json!({
                    "role": if message.get("role") == Some(&json!("assistant")) {
                        "assistant"
                    } else {
                        "user"
                    },
                    "content": message.get("content").unwrap_or(&json!("")),
                })
            })
            .collect::<Vec<_>>();
        if let Some(continuation) = continuation {
            messages.extend(into_anthropic_messages(
                continuation,
                &self.model,
                scope.as_ref(),
            )?);
        }
        let mut body = json!({
            "model": self.model,
            "max_tokens": max_output_tokens.unwrap_or(4096),
            "temperature": temperature,
            "system": system,
            "messages": messages,
        });
        if max_output_tokens == Some(0) {
            return Err("max_output_tokens must be greater than zero".to_string());
        }
        if stream {
            body["stream"] = json!(true);
        }
        if let Some(tools) = tools {
            body["tools"] = json!(anthropic_tool_definitions(tools)?);
        }
        Ok(body)
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, String> {
        validate_anthropic_request(&request)?;
        let scope = request.scope.clone();
        let body = self.request_body(request, false)?;

        let resp = crate::llm::send_with_retry_for(
            |credential_index| {
                self.client
                    .post(&self.base_url)
                    .header("x-api-key", &self.api_keys[credential_index])
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
            },
            self.api_keys.len(),
            crate::llm::is_anthropic_transient_status,
        )
        .await?;

        if !resp.status().is_success() {
            return Err(safe_anthropic_http_error(resp).await);
        }

        let data =
            crate::llm::read_bounded_json_response(resp, "Anthropic completion response").await?;
        let mut response = parse_anthropic_response(&data)?;
        if let Some(calls) = response
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        {
            let assistant_content = data
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| "Anthropic response is missing content".to_string())?;
            response.continuation = Some(anthropic_continuation(
                &self.model,
                scope,
                assistant_content,
                calls,
            )?);
        }
        Ok(response)
    }

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, String> {
        validate_anthropic_request(&request)?;
        let continuation_scope = request.scope.clone();
        let body = self.request_body(request, true)?;

        let resp = crate::llm::send_with_retry_for(
            |credential_index| {
                self.client
                    .post(&self.base_url)
                    .header("x-api-key", &self.api_keys[credential_index])
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
            },
            self.api_keys.len(),
            crate::llm::is_anthropic_transient_status,
        )
        .await?;

        if !resp.status().is_success() {
            return Err(safe_anthropic_http_error(resp).await);
        }
        crate::llm::ensure_response_content_length(
            &resp,
            crate::llm::MAX_LLM_STREAM_BYTES,
            "Anthropic stream response",
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
            let mut finish_reason: Option<String> = None;
            let mut parser = AnthropicStreamParser::default();
            let mut budget = crate::llm::ResponseByteBudget::new(
                crate::llm::MAX_LLM_STREAM_BYTES,
                "Anthropic stream response",
            );
            let bounded_bytes = resp.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|_| {
                    "Anthropic stream transport failed while reading a response".to_string()
                })?;
                budget.account(chunk.len())?;
                Ok::<_, String>(chunk)
            });
            let mut stream = bounded_bytes.eventsource();
            while let Some(event_res) =
                crate::llm::next_stream_item_or_closed(&public_tx, &mut stream).await
            {
                let event = match event_res {
                    Ok(event) => event,
                    Err(EventStreamError::Transport(error)) => {
                        fail_anthropic_stream(&public_tx, &mut completion_tx, error).await;
                        return;
                    }
                    Err(EventStreamError::Utf8(_) | EventStreamError::Parser(_)) => {
                        fail_anthropic_stream(
                            &public_tx,
                            &mut completion_tx,
                            "Anthropic stream contained malformed SSE data".to_string(),
                        )
                        .await;
                        return;
                    }
                };
                if let Err(error) =
                    crate::llm::ensure_stream_event_size(event.data.len(), "Anthropic stream event")
                {
                    fail_anthropic_stream(&public_tx, &mut completion_tx, error).await;
                    return;
                }

                let data = match serde_json::from_str::<Value>(&event.data) {
                    Ok(data) => data,
                    Err(_) => {
                        fail_anthropic_stream(
                            &public_tx,
                            &mut completion_tx,
                            "Anthropic stream contained malformed JSON".to_string(),
                        )
                        .await;
                        return;
                    }
                };

                if event.event == "message_stop" {
                    if is_anthropic_error_envelope(&event.event, &data) {
                        fail_anthropic_stream(
                            &public_tx,
                            &mut completion_tx,
                            "Anthropic stream reported a provider error".to_string(),
                        )
                        .await;
                        return;
                    }
                    let Some(reason) = finish_reason.take() else {
                        fail_anthropic_stream(
                            &public_tx,
                            &mut completion_tx,
                            "Anthropic stream ended without a stop reason".to_string(),
                        )
                        .await;
                        return;
                    };
                    let completion = match complete_anthropic_stream(
                        content,
                        tool_calls,
                        parser.take_call_ids(),
                        reason.clone(),
                        &model,
                        continuation_scope,
                    ) {
                        Ok(completion) => completion,
                        Err(error) => {
                            fail_anthropic_stream(&public_tx, &mut completion_tx, error).await;
                            return;
                        }
                    };
                    if !crate::llm::send_stream_event(&public_tx, Ok(StreamEvent::End(reason)))
                        .await
                    {
                        return;
                    }
                    if let Some(sender) = completion_tx.take() {
                        let _ = sender.send(Ok(completion));
                    }
                    return;
                }

                let events = match parser.parse_event(&event.event, &data) {
                    Ok(events) => events,
                    Err(error) => {
                        fail_anthropic_stream(&public_tx, &mut completion_tx, error).await;
                        return;
                    }
                };
                for parsed in events {
                    match parsed {
                        StreamEvent::End(reason) => {
                            if reason.trim().is_empty() || finish_reason.replace(reason).is_some() {
                                fail_anthropic_stream(
                                    &public_tx,
                                    &mut completion_tx,
                                    "Anthropic stream contained an invalid terminal reason"
                                        .to_string(),
                                )
                                .await;
                                return;
                            }
                        }
                        StreamEvent::Content(fragment) => {
                            if finish_reason.is_some() {
                                fail_anthropic_stream(
                                    &public_tx,
                                    &mut completion_tx,
                                    "Anthropic stream emitted content after its terminal reason"
                                        .to_string(),
                                )
                                .await;
                                return;
                            }
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
                            if finish_reason.is_some() {
                                fail_anthropic_stream(
                                    &public_tx,
                                    &mut completion_tx,
                                    "Anthropic stream emitted a tool call after its terminal reason"
                                        .to_string(),
                                )
                                .await;
                                return;
                            }
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
                fail_anthropic_stream(
                    &public_tx,
                    &mut completion_tx,
                    "Anthropic stream ended before a message_stop event".to_string(),
                )
                .await;
            }
        });

        Ok(LlmStream::with_private_completion(public_rx, completion_rx))
    }
}

fn validate_anthropic_request(request: &LlmRequest<'_>) -> Result<(), String> {
    if request.reasoning_effort.is_some() {
        return Err("Anthropic requests do not support reasoning_effort".to_string());
    }
    Ok(())
}

const MAX_NATIVE_BASE_URL_BYTES: usize = 4 * 1024;
const ANTHROPIC_MESSAGES_SUFFIX: &str = "/v1/messages";

pub(crate) fn normalize_anthropic_endpoint(base_url: &str) -> Result<String, String> {
    let configured = base_url.trim();
    if configured.is_empty()
        || configured.len() > MAX_NATIVE_BASE_URL_BYTES
        || configured.contains('\0')
    {
        return Err(format!(
            "Anthropic base_url must be between 1 and {MAX_NATIVE_BASE_URL_BYTES} bytes and contain no NUL"
        ));
    }
    let parsed = Url::parse(configured)
        .map_err(|_| "Anthropic base_url must be a valid absolute HTTP(S) URL".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("Anthropic base_url must be an absolute HTTP(S) URL".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Anthropic base_url must not contain embedded credentials".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("Anthropic base_url must not contain a query string or fragment".to_string());
    }

    let path = parsed.path().trim_end_matches('/');
    let exact_endpoint = path.ends_with(ANTHROPIC_MESSAGES_SUFFIX);
    if exact_endpoint {
        let prefix = path
            .strip_suffix(ANTHROPIC_MESSAGES_SUFFIX)
            .expect("matching Anthropic endpoint suffix");
        if prefix
            .trim_end_matches('/')
            .ends_with(ANTHROPIC_MESSAGES_SUFFIX)
        {
            return Err("Anthropic base_url contains a doubled API suffix".to_string());
        }
    }
    Ok(if exact_endpoint {
        parsed.as_str().trim_end_matches('/').to_string()
    } else {
        format!(
            "{}{ANTHROPIC_MESSAGES_SUFFIX}",
            parsed.as_str().trim_end_matches('/')
        )
    })
}

fn anthropic_tool_definitions(tools: &[Value]) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let tool = tool
                .as_object()
                .ok_or_else(|| "Anthropic tool definition must be an object".to_string())?;
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err("Anthropic tool definition type must be 'function'".to_string());
            }
            let function = tool
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| "Anthropic tool definition is missing function".to_string())?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| "Anthropic tool definition is missing a name".to_string())?;
            let description = match function.get("description") {
                Some(description) => description.as_str().ok_or_else(|| {
                    "Anthropic tool definition description must be a string".to_string()
                })?,
                None => "",
            };
            let input_schema = function
                .get("parameters")
                .filter(|schema| schema.is_object())
                .ok_or_else(|| {
                    "Anthropic tool definition parameters must be an object".to_string()
                })?;
            Ok(json!({
                "name": name,
                "description": description,
                "input_schema": input_schema,
            }))
        })
        .collect()
}

async fn safe_anthropic_http_error(response: Response) -> String {
    let status = response.status().as_u16();
    let body_read =
        crate::llm::read_bounded_error_response(response, "Anthropic API error response").await;
    if body_read.is_err() {
        format!(
            "Anthropic API request failed with HTTP {status}; the error response could not be read safely"
        )
    } else {
        format!("Anthropic API request failed with HTTP {status}")
    }
}

fn anthropic_terminal_status(
    reason: &str,
    has_tool_calls: bool,
) -> Result<LlmTerminalStatus, String> {
    match reason {
        "end_turn" | "stop_sequence" if !has_tool_calls => Ok(LlmTerminalStatus::Completed),
        "tool_use" if has_tool_calls => Ok(LlmTerminalStatus::Completed),
        "refusal" if !has_tool_calls => Ok(LlmTerminalStatus::Refused),
        "end_turn" | "stop_sequence" => {
            Err("Anthropic terminal reason is inconsistent with tool use".to_string())
        }
        "tool_use" => Err("Anthropic tool-use terminal reason has no tool calls".to_string()),
        "refusal" => Err("Anthropic refusal contained executable tool calls".to_string()),
        "max_tokens" => Err("Anthropic response was truncated before completion".to_string()),
        "pause_turn" => Err("Anthropic response paused before completion".to_string()),
        _ => Err("Anthropic response contained an unsupported terminal reason".to_string()),
    }
}

fn complete_anthropic_stream(
    content: String,
    tool_calls: crate::llm::ToolCallAccumulator,
    call_ids: BTreeMap<usize, ProviderCallId>,
    finish_reason: String,
    model: &str,
    scope: Option<LlmRequestScope>,
) -> Result<LlmResponse, String> {
    let tool_calls = tool_calls.finish_with_call_ids(call_ids)?;
    let terminal_status = anthropic_terminal_status(&finish_reason, !tool_calls.is_empty())?;
    let completed_content = (!content.trim().is_empty()).then_some(content);
    let continuation = if tool_calls.is_empty() {
        None
    } else {
        let mut assistant_content = Vec::new();
        if let Some(content) = completed_content.as_ref() {
            assistant_content.push(json!({"type": "text", "text": content}));
        }
        for call in &tool_calls {
            let call_id = call
                .call_id
                .as_ref()
                .ok_or_else(|| "Anthropic tool call is missing its provider call ID".to_string())?;
            assistant_content.push(json!({
                "type": "tool_use",
                "id": call_id.as_str(),
                "name": call.name,
                "input": call.arguments,
            }));
        }
        Some(anthropic_continuation(
            model,
            scope,
            assistant_content,
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

async fn fail_anthropic_stream(
    public_tx: &mpsc::Sender<Result<StreamEvent, String>>,
    completion_tx: &mut Option<oneshot::Sender<Result<LlmResponse, String>>>,
    error: String,
) {
    let _ = crate::llm::send_stream_event(public_tx, Err(error.clone())).await;
    if let Some(sender) = completion_tx.take() {
        let _ = sender.send(Err(error));
    }
}

fn is_anthropic_error_envelope(event_type: &str, data: &Value) -> bool {
    event_type == "error" || data.get("type").and_then(Value::as_str) == Some("error")
}

#[derive(Default)]
struct AnthropicStreamParser {
    call_ids: BTreeMap<usize, ProviderCallId>,
    native_call_ids: BTreeSet<String>,
}

impl AnthropicStreamParser {
    fn parse_event(&mut self, event_type: &str, data: &Value) -> Result<Vec<StreamEvent>, String> {
        if is_anthropic_error_envelope(event_type, data) {
            return Err("Anthropic stream reported a provider error".to_string());
        }
        let mut events = Vec::new();
        match event_type {
            "content_block_start" => {
                let block = data
                    .get("content_block")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        "Anthropic content block start is missing content_block".to_string()
                    })?;
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let index = data
                            .get("index")
                            .and_then(Value::as_u64)
                            .and_then(|index| usize::try_from(index).ok())
                            .ok_or_else(|| "Anthropic tool block is missing index".to_string())?;
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .filter(|name| !name.trim().is_empty())
                            .ok_or_else(|| "Anthropic tool block is missing name".to_string())?;
                        let native_call_id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.trim().is_empty())
                            .ok_or_else(|| {
                                "Anthropic tool block is missing its call ID".to_string()
                            })?;
                        if self.call_ids.contains_key(&index)
                            || !self.native_call_ids.insert(native_call_id.to_string())
                        {
                            return Err("Anthropic stream contains a duplicate tool call identity"
                                .to_string());
                        }
                        self.call_ids
                            .insert(index, ProviderCallId::new(native_call_id)?);
                        events.push(StreamEvent::ToolCallChunk {
                            index,
                            name: Some(name.to_string()),
                            arguments: Some(String::new()),
                        });
                    }
                    Some("text") => {}
                    Some(_) => {
                        return Err(
                            "Anthropic stream contained an unsupported content block".to_string()
                        )
                    }
                    None => return Err("Anthropic content block is missing its type".to_string()),
                }
            }
            "content_block_delta" => {
                let delta = data
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(|| "Anthropic content delta is missing delta".to_string())?;
                let index = data
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| "Anthropic content delta is missing index".to_string())?;
                if let Some(text) = delta.get("text") {
                    let text = text.as_str().ok_or_else(|| {
                        "Anthropic content delta text must be a string".to_string()
                    })?;
                    events.push(StreamEvent::Content(text.to_string()));
                }
                if let Some(arguments) = delta.get("partial_json") {
                    let arguments = arguments.as_str().ok_or_else(|| {
                        "Anthropic tool argument delta must be a string".to_string()
                    })?;
                    if !self.call_ids.contains_key(&index) {
                        return Err(
                            "Anthropic tool argument delta has no matching tool call".to_string()
                        );
                    }
                    events.push(StreamEvent::ToolCallChunk {
                        index,
                        name: None,
                        arguments: Some(arguments.to_string()),
                    });
                }
            }
            "message_delta" => {
                let delta = data
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(|| "Anthropic message delta is missing delta".to_string())?;
                if let Some(reason) = delta.get("stop_reason") {
                    let reason = reason
                        .as_str()
                        .filter(|reason| !reason.trim().is_empty())
                        .ok_or_else(|| {
                            "Anthropic message delta has an invalid stop reason".to_string()
                        })?;
                    events.push(StreamEvent::End(reason.to_string()));
                }
            }
            _ => {}
        }
        Ok(events)
    }

    fn take_call_ids(&mut self) -> BTreeMap<usize, ProviderCallId> {
        std::mem::take(&mut self.call_ids)
    }
}

pub fn parse_anthropic_stream_event(
    event_type: &str,
    data: &Value,
) -> Result<Vec<StreamEvent>, String> {
    AnthropicStreamParser::default().parse_event(event_type, data)
}

pub fn parse_anthropic_response(data: &Value) -> Result<LlmResponse, String> {
    if data.get("type").and_then(Value::as_str) == Some("error") {
        return Err("Anthropic completion reported a provider error".to_string());
    }
    let content_blocks = data
        .get("content")
        .and_then(Value::as_array)
        .ok_or("Anthropic response is missing content")?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut native_call_ids = BTreeSet::new();

    for block in content_blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let value = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("Anthropic text block is missing text")?;
                text.push_str(value);
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or("Anthropic tool use is missing a name")?;
                let native_call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .ok_or("Anthropic tool use is missing its call ID")?;
                if !native_call_ids.insert(native_call_id.to_string()) {
                    return Err("Anthropic response contains duplicate tool call IDs".to_string());
                }
                let arguments = block
                    .get("input")
                    .filter(|input| input.is_object())
                    .cloned()
                    .ok_or("Anthropic tool use input must be an object")?;
                tool_calls.push(ToolCallRequest::with_provider_call(
                    ProviderCallId::new(native_call_id)?,
                    name,
                    arguments,
                ));
            }
            Some(_) => {
                return Err("Anthropic response contains an unsupported content block".to_string())
            }
            None => return Err("Anthropic content block is missing its type".to_string()),
        }
    }

    let finish_reason = data
        .get("stop_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .ok_or("Anthropic response is missing a stop reason")?
        .to_string();
    let terminal_status = anthropic_terminal_status(&finish_reason, !tool_calls.is_empty())?;
    Ok(LlmResponse {
        terminal_status,
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        finish_reason,
        continuation: None,
    })
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

    fn test_client(base_url: String) -> AnthropicClient {
        AnthropicClient::with_base_url(
            "claude-test".to_string(),
            vec!["anthropic-test-key".to_string()],
            base_url,
        )
        .expect("test Anthropic endpoint")
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
    fn parses_streamed_tool_start_and_arguments() {
        let mut parser = AnthropicStreamParser::default();
        let start = parser
            .parse_event(
                "content_block_start",
                &json!({
                    "index": 2,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_test",
                        "name": "grep"
                    }
                }),
            )
            .unwrap();
        let delta = parser
            .parse_event(
                "content_block_delta",
                &json!({"index": 2, "delta": {"partial_json": "{\"pattern\":\"x\"}"}}),
            )
            .unwrap();
        assert!(matches!(
            &start[0],
            StreamEvent::ToolCallChunk { index: 2, name: Some(name), .. } if name == "grep"
        ));
        assert!(matches!(
            &delta[0],
            StreamEvent::ToolCallChunk { index: 2, arguments: Some(arguments), .. }
                if arguments.contains("pattern")
        ));
    }

    #[test]
    fn normalizes_and_validates_custom_anthropic_endpoints() {
        let root = AnthropicClient::with_base_url(
            "claude-test".to_string(),
            vec!["test-key".to_string()],
            "https://gateway.example/proxy",
        )
        .expect("custom root");
        assert_eq!(root.base_url, "https://gateway.example/proxy/v1/messages");

        let exact = AnthropicClient::with_base_url(
            "claude-test".to_string(),
            vec!["test-key".to_string()],
            "https://gateway.example/proxy/v1/messages/",
        )
        .expect("exact endpoint");
        assert_eq!(exact.base_url, "https://gateway.example/proxy/v1/messages");

        for invalid in [
            "https://user:secret@gateway.example/proxy",
            "https://gateway.example/proxy?token=secret",
            "https://gateway.example/proxy#fragment",
            "https://gateway.example/v1/messages/v1/messages",
        ] {
            assert!(AnthropicClient::with_base_url(
                "claude-test".to_string(),
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
        assert_eq!(error, "Anthropic requests do not support reasoning_effort");
        let error = client
            .stream(reasoning_request())
            .await
            .expect_err("stream must reject unsupported reasoning");
        assert_eq!(error, "Anthropic requests do not support reasoning_effort");
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
        assert_eq!(
            error,
            "Anthropic tool definition parameters must be an object"
        );
        let error = client
            .stream(LlmRequest::new(&messages, Some(&malformed_tools), 0.0))
            .await
            .expect_err("stream must reject a malformed tool");
        assert_eq!(
            error,
            "Anthropic tool definition parameters must be an object"
        );
    }

    #[test]
    fn complete_parser_rejects_missing_unsafe_and_inconsistent_terminal_states() {
        let text = json!({"content": [{"type": "text", "text": "partial"}]});
        assert!(parse_anthropic_response(&text)
            .expect_err("missing stop reason")
            .contains("missing a stop reason"));

        let truncated = json!({
            "content": [{"type": "text", "text": "partial"}],
            "stop_reason": "max_tokens"
        });
        assert!(parse_anthropic_response(&truncated)
            .expect_err("truncation")
            .contains("truncated"));

        let inconsistent = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_test",
                "name": "read_file",
                "input": {"path": "README.md"}
            }],
            "stop_reason": "end_turn"
        });
        assert!(parse_anthropic_response(&inconsistent)
            .expect_err("inconsistent tool terminal")
            .contains("inconsistent"));

        let unknown = json!({"content": [], "stop_reason": "remote_future_value"});
        let error = parse_anthropic_response(&unknown).expect_err("unknown terminal reason");
        assert_eq!(
            error,
            "Anthropic response contained an unsupported terminal reason"
        );
        assert!(!error.contains("remote_future_value"));
    }

    #[test]
    fn complete_parser_maps_refusal_without_tool_authority() {
        let response = parse_anthropic_response(&json!({
            "content": [{"type": "text", "text": "cannot comply"}],
            "stop_reason": "refusal"
        }))
        .expect("provider refusal");
        assert_eq!(response.terminal_status, LlmTerminalStatus::Refused);
        assert!(response.tool_calls.is_none());
    }

    #[tokio::test]
    async fn complete_posts_anthropic_payload_and_parses_mixed_content() {
        let (base_url, request_rx) = serve_once(
            "200 OK",
            "application/json",
            json!({
                "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "toolu_grep", "name": "grep", "input": {"pattern": "needle"}}
                ],
                "stop_reason": "tool_use"
            })
            .to_string(),
        );
        let response = test_client(base_url)
            .complete(
                LlmRequest::new(
                    &[
                        json!({"role": "system", "content": "follow project rules"}),
                        json!({"role": "user", "content": "search"}),
                    ],
                    Some(&[json!({
                        "type": "function",
                        "function": {
                            "name": "grep",
                            "description": "search files",
                            "parameters": {"type": "object", "required": ["pattern"]}
                        }
                    })]),
                    0.3,
                )
                .with_max_output_tokens(43)
                .with_scope(LlmRequestScope::new("test-session", "test-run").unwrap()),
            )
            .await
            .expect("Anthropic completion");

        assert_eq!(response.content.as_deref(), Some("checking"));
        assert_eq!(response.finish_reason, "tool_use");
        let call = &response.tool_calls.expect("tool use")[0];
        assert_eq!(call.name, "grep");
        assert_eq!(call.arguments["pattern"], "needle");
        assert_eq!(call.call_id.as_ref().unwrap().as_str(), "toolu_grep");

        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured Anthropic request");
        assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
        assert!(request
            .to_ascii_lowercase()
            .contains("x-api-key: anthropic-test-key"));
        assert!(request.contains("anthropic-version: 2023-06-01"));
        assert!(request.contains("\"max_tokens\":43"));
        assert!(request.contains("\"system\":\"follow project rules\""));
        assert!(request.contains("\"input_schema\""));
        assert!(!request.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn stream_consumes_named_sse_events_and_partial_tool_json() {
        let body = concat!(
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"text\":\"work\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_read\",\"name\":\"read_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"partial_json\":\"\\\"README.md\\\"}\"}}\n\n",
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\n",
            "data: {}\n\n"
        );
        let (base_url, request_rx) = serve_once("200 OK", "text/event-stream", body);
        let mut stream = test_client(base_url)
            .stream(
                LlmRequest::new(&[json!({"role": "user", "content": "read"})], None, 0.0)
                    .with_scope(LlmRequestScope::new("test-session", "test-run").unwrap()),
            )
            .await
            .expect("Anthropic stream");
        let mut content = String::new();
        let mut accumulator = crate::llm::ToolCallAccumulator::default();
        let mut finish = None;
        while let Some(event) = stream.recv().await {
            let event = event.expect("valid Anthropic event");
            if let StreamEvent::Content(fragment) = &event {
                content.push_str(fragment);
            }
            if let StreamEvent::End(reason) = &event {
                finish = Some(reason.clone());
            }
            accumulator.push(&event);
        }

        assert_eq!(content, "work");
        assert_eq!(finish.as_deref(), Some("tool_use"));
        let calls = accumulator.finish().expect("streamed tool call");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "README.md");
        let completion = stream.finish().await.expect("private Anthropic completion");
        assert_eq!(completion.content.as_deref(), Some("work"));
        assert_eq!(completion.finish_reason, "tool_use");
        let private_calls = completion.tool_calls.expect("private tool authority");
        assert_eq!(private_calls[0].name, "read_file");
        assert_eq!(private_calls[0].arguments["path"], "README.md");
        assert_eq!(
            private_calls[0].call_id.as_ref().unwrap().as_str(),
            "toolu_read"
        );
        let request = request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured Anthropic stream request");
        assert!(request.contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn dropping_stream_receiver_closes_the_open_http_response() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"text\":\"first\"}}\n\n"
        ));
        let mut stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "cancel"})],
                None,
                0.0,
            ))
            .await
            .expect("open Anthropic stream");
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
        assert!(disconnected, "Anthropic response connection remained open");
    }

    #[tokio::test]
    async fn message_stop_terminates_an_open_response_without_post_terminal_events() {
        let (base_url, _request_rx, disconnect_rx) = serve_open_stream(concat!(
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\n",
            "data: {}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"text\":\"late\"}}\n\n"
        ));
        let mut stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "finish"})],
                None,
                0.0,
            ))
            .await
            .expect("open Anthropic stream");

        let terminal = tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("terminal event timeout")
            .expect("terminal event")
            .expect("valid terminal event");
        assert_eq!(terminal, StreamEvent::End("end_turn".to_string()));
        assert!(tokio::time::timeout(Duration::from_secs(1), stream.recv())
            .await
            .expect("producer closure timeout")
            .is_none());

        let disconnected =
            tokio::task::spawn_blocking(move || disconnect_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .expect("disconnect observer")
                .expect("disconnect signal");
        assert!(disconnected, "Anthropic terminal event left response open");
    }

    #[tokio::test]
    async fn message_stop_without_delta_fails_closed() {
        let (base_url, _) = serve_once(
            "200 OK",
            "text/event-stream",
            "event: message_stop\ndata: {}\n\n",
        );
        let stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "finish"})],
                None,
                0.0,
            ))
            .await
            .expect("Anthropic stream");
        let error = stream
            .finish()
            .await
            .expect_err("missing stop reason must fail closed");
        assert!(error.contains("without a stop reason"));
    }

    #[tokio::test]
    async fn partial_tool_stream_eof_cannot_produce_private_authority() {
        let body = concat!(
            "event: content_block_start\n",
            "data: {\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_read\",\"name\":\"read_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n"
        );
        let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
        let stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "read"})],
                None,
                0.0,
            ))
            .await
            .expect("Anthropic stream");

        let error = stream
            .finish()
            .await
            .expect_err("EOF before message_stop must fail closed");
        assert!(error.contains("ended before a message_stop event"));
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
        assert_eq!(error, "Anthropic API request failed with HTTP 401");
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
        assert_eq!(error, "Anthropic API request failed with HTTP 403");
        assert!(!error.contains(SENTINEL));
    }

    #[tokio::test]
    async fn stream_error_event_hides_remote_message() {
        const SENTINEL: &str = "remote-stream-secret";
        let (base_url, _) = serve_once(
            "200 OK",
            "text/event-stream",
            format!(
                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"message\":\"{SENTINEL}\"}}}}\n\n"
            ),
        );
        let stream = test_client(base_url)
            .stream(LlmRequest::new(
                &[json!({"role": "user", "content": "x"})],
                None,
                0.0,
            ))
            .await
            .expect("Anthropic stream response");
        let error = stream.finish().await.expect_err("provider stream error");
        assert_eq!(error, "Anthropic stream reported a provider error");
        assert!(!error.contains(SENTINEL));
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
