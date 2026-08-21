//! LLM response types.

use crate::config::ReasoningEffort;
use crate::tools::ToolInvocationId;
use serde_json::{json, Value};
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub(crate) const MAX_CONTINUATION_ITEMS: usize = 256;
pub(crate) const MAX_CONTINUATION_BYTES: usize = 4 * 1024 * 1024;
const MAX_CALL_ID_BYTES: usize = 512;
const MAX_TOOL_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderCallId {
    value: String,
    continuation_binding: Option<uuid::Uuid>,
}

impl ProviderCallId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err("provider call ID must not be empty".to_string());
        }
        if value.len() > MAX_CALL_ID_BYTES || value.contains('\0') {
            return Err(format!(
                "provider call ID must be at most {MAX_CALL_ID_BYTES} bytes and contain no NUL"
            ));
        }
        Ok(Self {
            value,
            continuation_binding: None,
        })
    }

    pub(crate) fn for_responses(
        value: impl Into<String>,
        continuation_binding: uuid::Uuid,
    ) -> Result<Self, String> {
        let mut call_id = Self::new(value)?;
        call_id.continuation_binding = Some(continuation_binding);
        Ok(call_id)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.value
    }

    pub(crate) fn continuation_binding(&self) -> Option<uuid::Uuid> {
        self.continuation_binding
    }
}

impl fmt::Debug for ProviderCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderCallId(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequestScope {
    pub session_id: String,
    pub run_id: String,
}

impl LlmRequestScope {
    pub fn new(session_id: impl Into<String>, run_id: impl Into<String>) -> Result<Self, String> {
        let scope = Self {
            session_id: session_id.into(),
            run_id: run_id.into(),
        };
        if scope.session_id.trim().is_empty() || scope.run_id.trim().is_empty() {
            return Err("LLM request scope requires non-empty session and run IDs".to_string());
        }
        if scope.session_id.len() > 512 || scope.run_id.len() > 512 {
            return Err("LLM request scope identifiers must be at most 512 bytes".to_string());
        }
        Ok(scope)
    }
}

const MAX_TOOL_NAME_BYTES: usize = 256;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 8 * 1024;
const MAX_TOOL_SCHEMA_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmMessageRole {
    System,
    User,
    Assistant,
}

impl LlmMessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            other => Err(format!(
                "LLM message role '{other}' is not a provider-neutral system, user, or assistant message"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmMessage {
    pub role: LlmMessageRole,
    pub content: String,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: LlmMessageRole::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: LlmMessageRole::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: LlmMessageRole::Assistant,
            content: content.into(),
        }
    }

    pub fn from_openai_value(value: &Value) -> Result<Self, String> {
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| "LLM message is missing a role".to_string())?;
        let content = value
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| "LLM message is missing string content".to_string())?;
        if value.get("tool_call_id").is_some() || role == "tool" {
            return Err(
                "tool results are continuation data and cannot be supplied as LLM messages"
                    .to_string(),
            );
        }
        Ok(Self {
            role: LlmMessageRole::parse(role)?,
            content: content.to_string(),
        })
    }

    pub fn from_openai_values(values: &[Value]) -> Result<Vec<Self>, String> {
        values.iter().map(Self::from_openai_value).collect()
    }

    pub fn to_openai_chat(&self) -> Value {
        json!({
            "role": self.role.as_str(),
            "content": self.content,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    name: String,
    description: String,
    parameters: Value,
    strict: bool,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Result<Self, String> {
        let name = name.into();
        let description = description.into();
        if name.trim().is_empty() || name.len() > MAX_TOOL_NAME_BYTES || name.contains('\0') {
            return Err(format!(
                "tool name must be 1..={MAX_TOOL_NAME_BYTES} bytes and contain no NUL"
            ));
        }
        if description.len() > MAX_TOOL_DESCRIPTION_BYTES || description.contains('\0') {
            return Err(format!(
                "tool description must be at most {MAX_TOOL_DESCRIPTION_BYTES} bytes and contain no NUL"
            ));
        }
        if !parameters.is_object() {
            return Err("tool parameters must be a JSON object schema".to_string());
        }
        let encoded = serde_json::to_vec(&parameters)
            .map_err(|error| format!("tool parameters could not be encoded: {error}"))?;
        if encoded.len() > MAX_TOOL_SCHEMA_BYTES {
            return Err(format!(
                "tool parameters must be at most {MAX_TOOL_SCHEMA_BYTES} bytes"
            ));
        }
        Ok(Self {
            name,
            description,
            parameters,
            strict: false,
        })
    }

    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    pub fn function(name: impl Into<String>) -> Self {
        Self::new(name, "", json!({"type": "object"}))
            .expect("non-empty function tool names are valid")
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn parameters(&self) -> &Value {
        &self.parameters
    }

    pub fn from_openai_value(value: &Value) -> Result<Self, String> {
        let tool = value
            .as_object()
            .ok_or_else(|| "tool definition must be an object".to_string())?;
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            return Err("tool definition type must be 'function'".to_string());
        }
        let function = tool
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| "tool definition is missing function".to_string())?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool definition is missing a name".to_string())?;
        let description = match function.get("description") {
            Some(description) => description
                .as_str()
                .ok_or_else(|| "tool description must be a string".to_string())?,
            None => "",
        };
        let parameters = function
            .get("parameters")
            .cloned()
            .ok_or_else(|| "tool definition parameters must be an object".to_string())?;
        let mut tool = Self::new(name, description, parameters)?;
        match function.get("strict") {
            None => {}
            Some(Value::Bool(strict)) => tool.strict = *strict,
            Some(_) => return Err("function tool strict value must be boolean".to_string()),
        }
        Ok(tool)
    }

    pub fn from_openai_values(values: &[Value]) -> Result<Vec<Self>, String> {
        values.iter().map(Self::from_openai_value).collect()
    }

    pub fn from_openai_values_opt(values: Option<&[Value]>) -> Result<Option<Vec<Self>>, String> {
        values.map(Self::from_openai_values).transpose()
    }

    pub fn to_openai_tool(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
                "strict": self.strict,
            }
        })
    }

    pub fn to_anthropic_tool(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.parameters,
        })
    }

    pub fn to_gemini_declaration(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
        })
    }

    pub fn to_responses_tool(&self) -> Value {
        json!({
            "type": "function",
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
            "strict": self.strict,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningOption {
    ProviderDefault,
    Disabled,
    Effort(ReasoningEffort),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GenerationOptions {
    temperature: Option<f64>,
    reasoning: ReasoningOption,
}

impl Default for GenerationOptions {
    fn default() -> Self {
        Self::provider_default()
    }
}

impl GenerationOptions {
    pub fn provider_default() -> Self {
        Self {
            temperature: None,
            reasoning: ReasoningOption::ProviderDefault,
        }
    }

    pub fn temperature(&self) -> Option<f64> {
        self.temperature
    }

    pub fn reasoning(&self) -> ReasoningOption {
        self.reasoning
    }

    pub fn with_temperature(mut self, value: f64) -> Result<Self, String> {
        self.set_temperature(value)?;
        Ok(self)
    }

    pub fn with_reasoning(mut self, reasoning: ReasoningOption) -> Self {
        self.reasoning = reasoning;
        self
    }

    fn set_temperature(&mut self, value: f64) -> Result<(), String> {
        if !value.is_finite() {
            return Err("temperature must be a finite number".to_string());
        }
        if !(0.0..=2.0).contains(&value) {
            return Err("temperature must be in 0.0..=2.0".to_string());
        }
        self.temperature = Some(value);
        Ok(())
    }

    pub fn resolved_reasoning(
        self,
        configured: Option<ReasoningEffort>,
    ) -> Option<ReasoningEffort> {
        match self.reasoning {
            ReasoningOption::ProviderDefault => configured,
            ReasoningOption::Disabled => Some(ReasoningEffort::None),
            ReasoningOption::Effort(effort) => Some(effort),
        }
    }
}

pub struct LlmRequest<'a> {
    pub messages: &'a [LlmMessage],
    pub tools: Option<&'a [ToolDefinition]>,
    pub options: GenerationOptions,
    pub max_output_tokens: Option<u32>,
    pub scope: Option<LlmRequestScope>,
    pub continuation: Option<ProviderContinuation>,
}

impl<'a> LlmRequest<'a> {
    pub fn new(messages: &'a [LlmMessage], tools: Option<&'a [ToolDefinition]>) -> Self {
        Self {
            messages,
            tools,
            options: GenerationOptions::provider_default(),
            max_output_tokens: None,
            scope: None,
            continuation: None,
        }
    }

    pub fn with_options(mut self, options: GenerationOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_temperature(mut self, value: f64) -> Result<Self, String> {
        self.options.set_temperature(value)?;
        Ok(self)
    }

    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.options.reasoning = match effort {
            None => ReasoningOption::ProviderDefault,
            Some(ReasoningEffort::None) => ReasoningOption::Disabled,
            Some(effort) => ReasoningOption::Effort(effort),
        };
        self
    }

    /// Applies a hard provider request-side output ceiling.
    ///
    /// Adapters serialize this through their native field and reject zero before I/O.
    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    pub fn with_scope(mut self, scope: LlmRequestScope) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn with_continuation(mut self, continuation: Option<ProviderContinuation>) -> Self {
        self.continuation = continuation;
        self
    }
}

impl fmt::Debug for LlmRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmRequest")
            .field("message_count", &self.messages.len())
            .field("tool_count", &self.tools.map_or(0, <[ToolDefinition]>::len))
            .field("temperature", &self.options.temperature())
            .field("max_output_tokens", &self.max_output_tokens)
            .field("reasoning", &self.options.reasoning())
            .field("scope", &self.scope)
            .field("continuation", &self.continuation)
            .finish()
    }
}

pub struct ProviderContinuation {
    provider: String,
    model: String,
    transport: &'static str,
    scope: Option<LlmRequestScope>,
    pending_invocations: Vec<ToolInvocationId>,
    tool_outputs: BTreeMap<ToolInvocationId, String>,
    encoded_bytes: usize,
    require_ordered_results: bool,
    state: Box<dyn Any + Send>,
}

impl fmt::Debug for ProviderContinuation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderContinuation")
            .field("value", &"<redacted>")
            .finish()
    }
}

impl ProviderContinuation {
    // These fields are separate security boundaries: provider identity, request scope,
    // continuation limits, and opaque state must not be collapsed or inferred.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new<T: Any + Send>(
        provider: &str,
        model: &str,
        transport: &'static str,
        scope: Option<LlmRequestScope>,
        pending_invocations: Vec<ToolInvocationId>,
        item_count: usize,
        encoded_bytes: usize,
        state: T,
    ) -> Result<Self, String> {
        Self::build(
            provider,
            model,
            transport,
            scope,
            pending_invocations,
            item_count,
            encoded_bytes,
            false,
            state,
        )
    }

    // Ordered continuations intentionally expose the same validated boundaries.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_ordered<T: Any + Send>(
        provider: &str,
        model: &str,
        transport: &'static str,
        scope: Option<LlmRequestScope>,
        pending_invocations: Vec<ToolInvocationId>,
        item_count: usize,
        encoded_bytes: usize,
        state: T,
    ) -> Result<Self, String> {
        Self::build(
            provider,
            model,
            transport,
            scope,
            pending_invocations,
            item_count,
            encoded_bytes,
            true,
            state,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build<T: Any + Send>(
        provider: &str,
        model: &str,
        transport: &'static str,
        scope: Option<LlmRequestScope>,
        pending_invocations: Vec<ToolInvocationId>,
        item_count: usize,
        encoded_bytes: usize,
        require_ordered_results: bool,
        state: T,
    ) -> Result<Self, String> {
        if item_count.saturating_add(pending_invocations.len()) > MAX_CONTINUATION_ITEMS {
            return Err(format!(
                "provider continuation exceeds the {MAX_CONTINUATION_ITEMS}-item limit"
            ));
        }
        if pending_invocations.is_empty() {
            return Err("provider continuation requires at least one tool call".to_string());
        }
        let scope = scope.ok_or_else(|| {
            "provider continuation requires a bound session and run scope".to_string()
        })?;
        let unique_invocations = pending_invocations.iter().copied().collect::<BTreeSet<_>>();
        if unique_invocations.len() != pending_invocations.len() {
            return Err("provider continuation contains duplicate tool invocation IDs".to_string());
        }
        if encoded_bytes > MAX_CONTINUATION_BYTES {
            return Err(format!(
                "provider continuation exceeds the {MAX_CONTINUATION_BYTES}-byte limit"
            ));
        }
        Ok(Self {
            provider: provider.to_string(),
            model: model.to_string(),
            transport,
            scope: Some(scope),
            pending_invocations,
            tool_outputs: BTreeMap::new(),
            encoded_bytes,
            require_ordered_results,
            state: Box::new(state),
        })
    }

    pub fn record_tool_output(
        &mut self,
        invocation_id: ToolInvocationId,
        output: &Value,
    ) -> Result<(), String> {
        if !self.pending_invocations.contains(&invocation_id) {
            return Err("tool invocation ID does not belong to this continuation".to_string());
        }
        if self.tool_outputs.contains_key(&invocation_id) {
            return Err("tool invocation ID was already completed".to_string());
        }
        if self.require_ordered_results
            && self
                .pending_invocations
                .get(self.tool_outputs.len())
                .is_some_and(|expected| *expected != invocation_id)
        {
            return Err("provider continuation tool outputs are out of order".to_string());
        }
        let output = serde_json::to_string(output)
            .map_err(|error| format!("failed to encode tool output: {error}"))?;
        if output.len() > MAX_TOOL_OUTPUT_BYTES {
            return Err(format!(
                "tool output exceeds the {MAX_TOOL_OUTPUT_BYTES}-byte continuation limit"
            ));
        }
        let next_bytes = self
            .encoded_bytes
            .checked_add(output.len())
            .ok_or_else(|| "provider continuation byte count overflowed".to_string())?;
        if next_bytes > MAX_CONTINUATION_BYTES {
            return Err(format!(
                "provider continuation exceeds the {MAX_CONTINUATION_BYTES}-byte limit"
            ));
        }
        self.encoded_bytes = next_bytes;
        self.tool_outputs.insert(invocation_id, output);
        Ok(())
    }

    pub(crate) fn consume<T: Any + Send>(
        self,
        provider: &str,
        model: &str,
        transport: &'static str,
        scope: Option<&LlmRequestScope>,
    ) -> Result<(T, BTreeMap<ToolInvocationId, String>), String> {
        if self.provider != provider
            || self.model != model
            || self.transport != transport
            || self.scope.as_ref() != scope
        {
            return Err(
                "provider continuation does not match the provider, model, API mode, session, or run"
                    .to_string(),
            );
        }
        if self.tool_outputs.len() != self.pending_invocations.len() {
            return Err("provider continuation is missing one or more tool outputs".to_string());
        }
        let state = self.state.downcast::<T>().map_err(|_| {
            "provider continuation state does not match the selected adapter".to_string()
        })?;
        Ok((*state, self.tool_outputs))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRequest {
    pub invocation_id: ToolInvocationId,
    pub(crate) call_id: Option<ProviderCallId>,
    pub name: String,
    pub arguments: Value,
}

impl ToolCallRequest {
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            invocation_id: ToolInvocationId::new(),
            call_id: None,
            name: name.into(),
            arguments,
        }
    }

    pub(crate) fn with_provider_call(
        call_id: ProviderCallId,
        name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self {
            invocation_id: ToolInvocationId::new(),
            call_id: Some(call_id),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmTerminalStatus {
    Completed,
    Refused,
}

#[derive(Debug)]
pub struct LlmResponse {
    pub terminal_status: LlmTerminalStatus,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCallRequest>>,
    pub finish_reason: String,
    pub continuation: Option<ProviderContinuation>,
}

impl LlmResponse {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            terminal_status: LlmTerminalStatus::Completed,
            content: Some(content.into()),
            tool_calls: None,
            finish_reason: "stop".to_string(),
            continuation: None,
        }
    }

    pub fn with_tools(calls: Vec<ToolCallRequest>) -> Self {
        Self {
            terminal_status: LlmTerminalStatus::Completed,
            content: None,
            tool_calls: Some(calls),
            finish_reason: "tool_calls".to_string(),
            continuation: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Content(String),
    ToolCallChunk {
        index: usize,
        name: Option<String>,
        arguments: Option<String>,
    },
    StateTransition {
        state: String,
    },
    PlanGenerated {
        step_count: usize,
    },
    ApprovalRequired {
        tool_name: String,
        arguments: Value,
    },
    QuestionRequired {
        question: String,
        options: Vec<String>,
    },
    ToolStarted {
        tool_name: String,
        arguments: Value,
    },
    TerminalOutput {
        tool_name: String,
        stream: String,
        chunk: String,
        background_task_id: Option<String>,
    },
    ToolCompleted {
        tool_name: String,
        success: bool,
        output: Option<Value>,
        error: Option<String>,
    },
    Compression {
        before_tokens: usize,
        after_tokens: usize,
        summarized_through: usize,
    },
    Reconciled {
        outcome: String,
    },
    Failure {
        failure: crate::llm::LlmError,
        session_id: Option<String>,
    },
    End(String),
}

#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<usize, PendingToolCall>,
}

#[derive(Debug, Default)]
struct PendingToolCall {
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    pub fn push(&mut self, event: &StreamEvent) {
        let StreamEvent::ToolCallChunk {
            index,
            name,
            arguments,
        } = event
        else {
            return;
        };
        let pending = self.calls.entry(*index).or_default();
        if let Some(name) = name {
            pending.name.push_str(name);
        }
        if let Some(arguments) = arguments {
            pending.arguments.push_str(arguments);
        }
    }

    pub fn finish(self) -> Result<Vec<ToolCallRequest>, String> {
        self.finish_with_call_ids(BTreeMap::new())
    }

    pub(crate) fn finish_with_call_ids(
        self,
        mut call_ids: BTreeMap<usize, ProviderCallId>,
    ) -> Result<Vec<ToolCallRequest>, String> {
        let calls = self
            .calls
            .into_iter()
            .map(|(index, pending)| {
                if pending.name.trim().is_empty() {
                    return Err("streamed tool call is missing a name".to_string());
                }
                let arguments = if pending.arguments.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&pending.arguments).map_err(|error| {
                        format!(
                            "invalid streamed arguments for tool '{}': {error}",
                            pending.name
                        )
                    })?
                };
                Ok(match call_ids.remove(&index) {
                    Some(call_id) => {
                        ToolCallRequest::with_provider_call(call_id, pending.name, arguments)
                    }
                    None => ToolCallRequest::new(pending.name, arguments),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        if !call_ids.is_empty() {
            return Err("streamed provider call ID did not match a tool call".to_string());
        }
        Ok(calls)
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_interleaved_tool_chunks_deterministically() {
        let mut accumulator = ToolCallAccumulator::default();
        accumulator.push(&StreamEvent::ToolCallChunk {
            index: 1,
            name: Some("second".to_string()),
            arguments: Some("{\"b\":".to_string()),
        });
        accumulator.push(&StreamEvent::ToolCallChunk {
            index: 0,
            name: Some("first".to_string()),
            arguments: Some("{\"a\":1}".to_string()),
        });
        accumulator.push(&StreamEvent::ToolCallChunk {
            index: 1,
            name: None,
            arguments: Some("2}".to_string()),
        });

        let calls = accumulator.finish().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "first");
        assert_eq!(calls[0].arguments, serde_json::json!({"a": 1}));
        assert_eq!(calls[1].name, "second");
        assert_eq!(calls[1].arguments, serde_json::json!({"b": 2}));
        assert_ne!(calls[0].invocation_id, calls[1].invocation_id);
    }

    #[test]
    fn rejects_malformed_streamed_arguments() {
        let mut accumulator = ToolCallAccumulator::default();
        accumulator.push(&StreamEvent::ToolCallChunk {
            index: 0,
            name: Some("broken".to_string()),
            arguments: Some("{".to_string()),
        });
        assert!(accumulator
            .finish()
            .unwrap_err()
            .contains("invalid streamed"));
    }

    #[test]
    fn privately_attaches_provider_call_ids_by_stream_index() {
        let mut accumulator = ToolCallAccumulator::default();
        accumulator.push(&StreamEvent::ToolCallChunk {
            index: 2,
            name: Some("inspect".to_string()),
            arguments: Some("{}".to_string()),
        });
        let call_id = ProviderCallId::new("private-call").unwrap();
        let calls = accumulator
            .finish_with_call_ids(BTreeMap::from([(2, call_id)]))
            .unwrap();
        assert_eq!(calls[0].call_id.as_ref().unwrap().as_str(), "private-call");
    }

    #[test]
    fn responses_continuation_limits_are_enforced_before_replay() {
        let scope = LlmRequestScope::new("session", "run").unwrap();
        let invocation_id = ToolInvocationId::new();

        let item_error = ProviderContinuation::new(
            "openai",
            "gpt",
            "responses",
            Some(scope.clone()),
            vec![invocation_id],
            MAX_CONTINUATION_ITEMS,
            0,
            (),
        )
        .unwrap_err();
        assert!(item_error.contains("item limit"));

        let byte_error = ProviderContinuation::new(
            "openai",
            "gpt",
            "responses",
            Some(scope.clone()),
            vec![invocation_id],
            1,
            MAX_CONTINUATION_BYTES + 1,
            (),
        )
        .unwrap_err();
        assert!(byte_error.contains("byte limit"));

        let mut continuation = ProviderContinuation::new(
            "openai",
            "gpt",
            "responses",
            Some(scope),
            vec![invocation_id],
            1,
            0,
            (),
        )
        .unwrap();
        let output_error = continuation
            .record_tool_output(
                invocation_id,
                &serde_json::json!("x".repeat(MAX_TOOL_OUTPUT_BYTES)),
            )
            .unwrap_err();
        assert!(output_error.contains("tool output exceeds"));
    }

    #[test]
    fn generation_options_accept_finite_range_and_reject_invalid_temperature() {
        let options = GenerationOptions::provider_default()
            .with_temperature(0.5)
            .expect("valid temperature");
        assert_eq!(options.temperature(), Some(0.5));
        assert_eq!(options.reasoning(), ReasoningOption::ProviderDefault);

        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 2.01] {
            let error = GenerationOptions::provider_default()
                .with_temperature(invalid)
                .expect_err("invalid temperature");
            assert!(
                error.contains("finite") || error.contains("0.0..=2.0"),
                "{error}"
            );
        }
    }

    #[test]
    fn typed_messages_and_tools_reject_wire_protocol_shapes() {
        let message = LlmMessage::from_openai_value(&json!({
            "role": "user",
            "content": "inspect"
        }))
        .expect("user message");
        assert_eq!(message, LlmMessage::user("inspect"));
        assert!(LlmMessage::from_openai_value(&json!({
            "role": "tool",
            "content": "result",
            "tool_call_id": "call_1"
        }))
        .unwrap_err()
        .contains("continuation"));

        let tool = ToolDefinition::from_openai_value(&json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {"type": "object"}
            }
        }))
        .expect("function tool");
        assert_eq!(tool.name(), "read_file");
        assert_eq!(
            ToolDefinition::from_openai_value(&json!({
                "type": "function",
                "function": {"name": "read"}
            }))
            .unwrap_err(),
            "tool definition parameters must be an object"
        );
    }
}
