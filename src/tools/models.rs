//! Tool permission models and call/result types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

/// Provider-neutral identity for one nib tool invocation.
///
/// Provider correlation handles are deliberately separate and must never be
/// used as this durable workload identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolInvocationId(Uuid);

impl ToolInvocationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ToolInvocationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ToolInvocationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    ReadOnly,
    Plan,
    Safe,
    Destructive,
    Network,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    #[default]
    Manual,
    Smart,
    Policy,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetadata {
    pub name: String,
    pub description: String,
    pub permission_level: PermissionLevel,
    #[serde(default = "default_true")]
    pub requires_approval: bool,
    #[serde(default)]
    pub requires_worktree: bool,
    #[serde(default = "default_true")]
    pub mcp_exposable: bool,
    #[serde(default)]
    pub input_schema: Value,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub invocation_id: ToolInvocationId,
    pub tool_name: String,
    pub arguments: Value,
    pub session_id: Option<String>,
    pub project_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub invocation_id: ToolInvocationId,
    pub tool_name: String,
    pub success: bool,
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    pub duration_seconds: f64,
    #[serde(default)]
    pub approval_granted: bool,
    #[serde(default)]
    pub approval_source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApprovalDecision {
    pub granted: bool,
    pub source: String,
    pub note: Option<String>,
}

impl ApprovalDecision {
    pub fn granted_policy() -> Self {
        Self {
            granted: true,
            source: "policy".to_string(),
            note: Some("read-only".to_string()),
        }
    }

    pub fn denied() -> Self {
        Self {
            granted: false,
            source: "denied".to_string(),
            note: Some("User denied".to_string()),
        }
    }

    pub fn denied_by_policy(note: impl Into<String>) -> Self {
        Self {
            granted: false,
            source: "policy".to_string(),
            note: Some(note.into()),
        }
    }

    pub fn granted_classifier() -> Self {
        Self {
            granted: true,
            source: "classifier".to_string(),
            note: Some("Smart classifier approved".to_string()),
        }
    }

    pub fn granted_user() -> Self {
        Self {
            granted: true,
            source: "user".to_string(),
            note: Some("CLI confirmation".to_string()),
        }
    }

    pub fn granted_yolo() -> Self {
        Self {
            granted: true,
            source: "yolo".to_string(),
            note: Some("YOLO mode".to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyEffect {
    Allow,
    RequireApproval,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRule {
    pub effect: PolicyEffect,
    pub tool_name: String,
    pub argument_contains: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterToolHook {
    pub source: String,
    pub tool_name: String,
    pub command: String,
}

impl PolicyRule {
    pub fn matches(&self, call: &ToolCall) -> bool {
        if self.tool_name != "*" && self.tool_name != call.tool_name {
            return false;
        }

        self.argument_contains.as_ref().is_none_or(|needle| {
            call.arguments
                .to_string()
                .to_lowercase()
                .contains(&needle.to_lowercase())
        })
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionRecord {
    pub id: String,
    pub session_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub result: ToolResult,
    pub approval: ApprovalDecision,
    pub worktree_path: Option<String>,
    pub timestamp: DateTime<Utc>,
}
