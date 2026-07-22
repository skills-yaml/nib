//! LLM response types.

use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRequest {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCallRequest>>,
    pub finish_reason: String,
}

impl LlmResponse {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            tool_calls: None,
            finish_reason: "stop".to_string(),
        }
    }

    pub fn with_tools(calls: Vec<ToolCallRequest>) -> Self {
        Self {
            content: None,
            tool_calls: Some(calls),
            finish_reason: "tool_calls".to_string(),
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
        self.calls
            .into_values()
            .map(|pending| {
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
                Ok(ToolCallRequest {
                    name: pending.name,
                    arguments,
                })
            })
            .collect()
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

        assert_eq!(
            accumulator.finish().unwrap(),
            vec![
                ToolCallRequest {
                    name: "first".to_string(),
                    arguments: serde_json::json!({"a": 1}),
                },
                ToolCallRequest {
                    name: "second".to_string(),
                    arguments: serde_json::json!({"b": 2}),
                },
            ]
        );
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
}
