//! Tool permission models and call/result types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    ReadOnly,
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
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub arguments: Value,
    pub session_id: Option<String>,
    pub project_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
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
