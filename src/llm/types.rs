//! LLM response types.

use crate::config::ReasoningEffort;
use crate::tools::ToolInvocationId;
use serde_json::Value;
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

pub struct LlmRequest<'a> {
    pub messages: &'a [Value],
    pub tools: Option<&'a [Value]>,
    pub temperature: f64,
    pub max_output_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub scope: Option<LlmRequestScope>,
    pub continuation: Option<ProviderContinuation>,
}

impl<'a> LlmRequest<'a> {
    pub fn new(messages: &'a [Value], tools: Option<&'a [Value]>, temperature: f64) -> Self {
        Self {
            messages,
            tools,
            temperature,
            max_output_tokens: None,
            reasoning_effort: None,
            scope: None,
            continuation: None,
        }
    }

    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
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
            .field("tool_count", &self.tools.map_or(0, <[Value]>::len))
            .field("temperature", &self.temperature)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
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
}
