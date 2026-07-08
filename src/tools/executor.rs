//! Central ToolExecutor — all tool usage must pass through here.

use crate::config::ExecutionConfig;
use crate::integrations::mcp::McpManager;
use crate::integrations::worktree::WorktreeManager;
use crate::session::{SessionStore, ToolCallRecord};
use crate::tools::core;
use crate::tools::models::{ApprovalDecision, ApprovalMode, PermissionLevel, ToolCall, ToolResult};
use crate::tools::registry::{get_permission_level, get_tool_metadata};
use chrono::Utc;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncBufReadExt;
use uuid::Uuid;

#[async_trait::async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision;
}

pub struct StdinApprovalHandler;

#[async_trait::async_trait]
impl ApprovalHandler for StdinApprovalHandler {
    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision {
        eprintln!("\nApproval required for {}", call.tool_name);
        eprintln!("Permission level: {:?}", level);
        eprintln!("Arguments: {}", call.arguments);
        eprint!("Approve? [y/N]: ");
        let _ = io::stderr().flush();

        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        let mut line = String::new();

        if reader.read_line(&mut line).await.is_ok() && line.trim().eq_ignore_ascii_case("y") {
            ApprovalDecision::granted_user()
        } else {
            ApprovalDecision::denied()
        }
    }
}

pub struct ToolExecutor {
    pub session_store: Option<SessionStore>,
    pub approval_mode: ApprovalMode,
    pub project_root: PathBuf,
    pub auto_approve: bool,
    pub execution_config: ExecutionConfig,
    pub approval_handler: Arc<dyn ApprovalHandler>,
    worktree_manager: Option<WorktreeManager>,
    pub mcp_manager: Option<Arc<McpManager>>,
}

impl ToolExecutor {
    pub fn new(project_root: PathBuf, execution_config: ExecutionConfig) -> Self {
        let store = SessionStore::new(&project_root);
        Self {
            session_store: Some(store),
            approval_mode: ApprovalMode::Manual,
            project_root,
            auto_approve: false,
            execution_config,
            approval_handler: Arc::new(StdinApprovalHandler),
            worktree_manager: None,
            mcp_manager: None,
        }
    }

    pub fn with_auto_approve(mut self, auto: bool) -> Self {
        self.auto_approve = auto;
        self
    }

    pub fn with_approval_handler(mut self, handler: Arc<dyn ApprovalHandler>) -> Self {
        self.approval_handler = handler;
        self
    }

    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    pub async fn get_tools_schema(&self) -> Vec<Value> {
        let mut schemas = tools_json_schema();
        if let Some(mcp) = &self.mcp_manager {
            if let Ok(mcp_tools) = mcp.list_tools().await {
                for t in mcp_tools {
                    schemas.push(json!({
                        "type": "function",
                        "function": t
                    }));
                }
            }
        }
        schemas
    }

    pub async fn execute(&mut self, call: ToolCall, session_id: Option<&str>) -> ToolResult {
        let start = Instant::now();
        let tool_name = call.tool_name.clone();
        let mut is_mcp_tool = false;
        let metadata_opt = get_tool_metadata(&tool_name);

        if metadata_opt.is_none() && tool_name.contains("::") {
            // It might be an MCP tool
            is_mcp_tool = true;
        } else if metadata_opt.is_none() {
            return ToolResult {
                tool_name,
                success: false,
                output: None,
                error: Some(format!("Unknown tool: {}", call.tool_name)),
                duration_seconds: start.elapsed().as_secs_f64(),
                approval_granted: false,
                approval_source: None,
            };
        }

        let effective_root = self.resolve_scope(&call);
        let worktree = if let Some(meta) = metadata_opt {
            self.ensure_worktree(&call, meta, &effective_root, session_id)
        } else {
            None
        };

        // Assume Destructive level for unknown/MCP tools to be safe
        let level = get_permission_level(&tool_name).unwrap_or(PermissionLevel::Destructive);
        let approval = self.handle_approval(&call, level, session_id).await;

        if !approval.granted {
            let result = ToolResult {
                tool_name: tool_name.clone(),
                success: false,
                output: None,
                error: Some("Approval denied".to_string()),
                duration_seconds: start.elapsed().as_secs_f64(),
                approval_granted: false,
                approval_source: Some(approval.source.clone()),
            };
            self.record(&call, &result, &approval, session_id, worktree.as_deref());
            return result;
        }

        let cwd = worktree.as_ref().unwrap_or(&effective_root);

        let exec_result = if is_mcp_tool {
            if let Some(mcp) = &self.mcp_manager {
                match mcp.call_tool(&tool_name, call.arguments.clone()).await {
                    Ok(res) => Ok(res),
                    Err(e) => Err(e.to_string()),
                }
            } else {
                Err("MCP tool called but manager not initialized".to_string())
            }
        } else {
            core::dispatch(&tool_name, &call.arguments, cwd, &self.execution_config).await
        };

        let (success, output, error) = match exec_result {
            Ok(v) => (true, Some(v), None),
            Err(e) => (false, None, Some(e)),
        };

        let result = ToolResult {
            tool_name: tool_name.clone(),
            success,
            output,
            error,
            duration_seconds: start.elapsed().as_secs_f64(),
            approval_granted: true,
            approval_source: Some(approval.source.clone()),
        };

        self.record(&call, &result, &approval, session_id, worktree.as_deref());
        result
    }

    fn resolve_scope(&self, call: &ToolCall) -> PathBuf {
        call.project_root
            .clone()
            .unwrap_or_else(|| self.project_root.clone())
            .canonicalize()
            .unwrap_or_else(|_| self.project_root.clone())
    }

    fn ensure_worktree(
        &mut self,
        call: &ToolCall,
        metadata: &crate::tools::models::ToolMetadata,
        effective_root: &Path,
        session_id: Option<&str>,
    ) -> Option<PathBuf> {
        let needs = metadata.requires_worktree
            || matches!(
                metadata.permission_level,
                PermissionLevel::Safe | PermissionLevel::Destructive
            );
        if !needs {
            return None;
        }
        let sid = session_id.or(call.session_id.as_deref())?;
        if self.worktree_manager.is_none() {
            self.worktree_manager = Some(WorktreeManager::new(effective_root.to_path_buf()));
        }
        if let Some(wm) = &mut self.worktree_manager {
            wm.create_for_session(sid).ok()
        } else {
            None
        }
    }

    async fn handle_approval(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        _session_id: Option<&str>,
    ) -> ApprovalDecision {
        if level == PermissionLevel::ReadOnly {
            return ApprovalDecision::granted_policy();
        }
        if self.approval_mode == ApprovalMode::Off {
            return ApprovalDecision::granted_yolo();
        }
        if self.auto_approve {
            return ApprovalDecision::granted_user();
        }

        self.approval_handler.handle_approval(call, level).await
    }

    fn record(
        &self,
        call: &ToolCall,
        result: &ToolResult,
        approval: &ApprovalDecision,
        session_id: Option<&str>,
        worktree: Option<&Path>,
    ) {
        let Some(sid) = session_id.or(call.session_id.as_deref()) else {
            return;
        };
        let Some(store) = &self.session_store else {
            return;
        };

        let record = ToolCallRecord {
            id: Some(format!("tool-{}", Uuid::new_v4())),
            session_id: Some(sid.to_string()),
            tool_name: Some(call.tool_name.clone()),
            arguments: call.arguments.clone(),
            result: Some(json!({
                "success": result.success,
                "output": result.output,
                "error": result.error,
                "approval": approval.source,
            })),
            error: result.error.clone(),
            duration_seconds: Some(result.duration_seconds),
            worktree_path: worktree.map(|p| p.to_string_lossy().to_string()),
            timestamp: Some(Utc::now()),
        };
        let _ = store.record_tool_call(record);
    }
}

pub fn tools_json_schema() -> Vec<Value> {
    crate::tools::registry::list_tools()
        .into_iter()
        .map(|meta| {
            json!({
                "type": "function",
                "function": {
                    "name": meta.name,
                    "description": meta.description,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "command": { "type": "string" },
                            "pattern": { "type": "string" },
                            "patch": { "type": "string" },
                        },
                        "required": []
                    }
                }
            })
        })
        .collect()
}
