//! Tool registry, executor, and implementations.

pub mod classifier;
pub mod core;
pub mod delegation;
pub mod executor;
pub mod models;
pub mod registry;

pub use delegation::{merge_subagent_worktree, spawn_subagent};
pub use executor::{tools_json_schema, ToolExecutor};
pub use models::{ApprovalMode, PermissionLevel, ToolCall, ToolInvocationId, ToolResult};
pub use registry::{get_tool_metadata, list_tools, tool_names};
