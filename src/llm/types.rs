//! LLM response types.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Content(String),
    ToolCallChunk {
        index: usize,
        name: Option<String>,
        arguments: Option<String>,
    },
    End(String),
}
