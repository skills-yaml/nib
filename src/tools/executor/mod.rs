//! Central ToolExecutor: scope, policy, approval, isolation, dispatch, and audit.

use crate::config::{
    boundary_profile_tightening_error, ApprovalsConfig, ExecutionConfig, TerminalConfig,
};
use crate::integrations::mcp::McpManager;
use crate::integrations::worktree::WorktreeManager;
use crate::session::{SessionStore, ToolCallRecord};
use crate::tools::classifier::{classify_tool_call, safe_command_requires_isolation, ToolRisk};
use crate::tools::core;
use crate::tools::models::{
    AfterToolHook, ApprovalDecision, ApprovalMode, PermissionLevel, PolicyEffect, PolicyRule,
    ToolCall, ToolResult,
};
use crate::tools::registry::get_tool_metadata;
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;
use tokio::io::AsyncBufReadExt;
use uuid::Uuid;

mod skills;
mod split_00;
mod split_01;
mod split_02;

pub(crate) use split_00::*;
pub(crate) use split_01::*;
pub(crate) use split_02::*;

pub use split_00::{
    ApprovalContext, ApprovalHandler, EffectiveExecutionPosture, InstructionExecutionPosture,
    ToolPolicyHook,
};
pub use split_00::{StdinApprovalHandler, ToolExecutor};
pub use split_01::tools_json_schema;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
