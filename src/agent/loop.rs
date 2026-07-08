//! Core agent loop — LLM reasoning + gated tool execution.

use crate::context::assemble_context;
use crate::llm::{create_client, LlmClient, LlmResponse};
use crate::session::SessionStore;
use crate::tools::{ToolCall, ToolExecutor};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct AgentLoopConfig {
    pub max_steps: u32,
    pub mode: String,
    pub provider: Option<String>,
    pub auto_approve: bool,
    pub approval_handler: Option<Arc<dyn crate::tools::executor::ApprovalHandler>>,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 15,
            mode: "execute".to_string(),
            provider: None,
            auto_approve: false,
            approval_handler: None,
        }
    }
}

pub struct AgentRunSummary {
    pub session_id: String,
    pub steps_taken: u32,
    pub last_message: Option<String>,
    pub tool_call_count: usize,
}

pub async fn run_agent_loop(
    project_root: PathBuf,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    let nib_cfg = crate::config::load_nib_config(&project_root);
    let llm: Arc<dyn LlmClient> = create_client(&nib_cfg.llm, cfg.provider.as_deref());
    let store = SessionStore::new(&project_root);
    let mcp_manager = if !nib_cfg.mcp.servers.is_empty() {
        crate::integrations::mcp::McpManager::new(&nib_cfg.mcp.servers)
            .await
            .ok()
            .map(Arc::new)
    } else {
        None
    };

    let mut executor = ToolExecutor::new(project_root.clone(), nib_cfg.execution.clone())
        .with_auto_approve(cfg.auto_approve);

    if let Some(mcp) = mcp_manager {
        executor = executor.with_mcp_manager(mcp);
    }

    if let Some(handler) = cfg.approval_handler.clone() {
        executor = executor.with_approval_handler(handler);
    }

    if store.load(session_id).is_none() {
        let mut s = store.create_session();
        if s.id != session_id {
            s.id = session_id.to_string();
            store.save(&s).map_err(|e| e.to_string())?;
        }
    }

    store.append_message(session_id, "user", goal);

    let tools_schema = executor.get_tools_schema().await;
    let context_block = assemble_context(&project_root, Some(goal));

    let mut filtered_tools = Vec::new();
    let mut use_tools_ref: Option<&[Value]> = None;

    if cfg.mode == "execute" {
        use_tools_ref = Some(tools_schema.as_slice());
    } else {
        for t in &tools_schema {
            if let Some(f) = t.get("function") {
                if let Some(name) = f.get("name").and_then(|n| n.as_str()) {
                    let level = crate::tools::registry::get_tool_metadata(name)
                        .map(|m| m.permission_level)
                        .unwrap_or(crate::tools::models::PermissionLevel::Destructive);
                    if level == crate::tools::models::PermissionLevel::ReadOnly || level == crate::tools::models::PermissionLevel::Plan {
                        filtered_tools.push(t.clone());
                    }
                }
            }
        }
        if !filtered_tools.is_empty() {
            use_tools_ref = Some(filtered_tools.as_slice());
        }
    }

    if nib_cfg.daemons.cron_enabled && nib_cfg.daemons.curator_enabled {
        crate::daemons::cron::Cron::run_maintenance(&project_root, nib_cfg.daemons.retention_days);
    }

    let mut steps = 0u32;
    for step in 0..cfg.max_steps {
        steps = step + 1;

        let _ = crate::context::compression::maybe_compress_session(
            &store,
            session_id,
            &llm,
            &nib_cfg,
        )
        .await;

        let system_prompt = build_system_prompt(
            &context_block,
            use_tools_ref.unwrap_or(&[]),
            &cfg.mode,
            &project_root,
        );
        let mut messages = vec![json!({"role": "system", "content": system_prompt})];

        if let Some(session) = store.load(session_id) {
            let mut last_role: Option<String> = None;
            for msg in session.messages {
                if let Some(ref lr) = last_role {
                    if lr == &msg.role {
                        if let Some(last_msg) = messages.last_mut() {
                            let old_content = last_msg["content"].as_str().unwrap_or("");
                            let new_content = format!("{}\n\n{}", old_content, msg.content);
                            *last_msg = json!({"role": msg.role, "content": new_content});
                        }
                        continue;
                    }
                }
                last_role = Some(msg.role.clone());
                messages.push(json!({"role": msg.role, "content": msg.content}));
            }
        }

        let response = llm.complete(&messages, use_tools_ref, 0.7).await?;

        if let Some(content) = &response.content {
            store.append_message(session_id, "assistant", content);
        }

        if let Some(calls) = &response.tool_calls {
            for tc in calls {
                let call = ToolCall {
                    tool_name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                    session_id: Some(session_id.to_string()),
                    project_root: Some(project_root.clone()),
                };
                let result = executor.execute(call, Some(session_id)).await;
                let obs = json!({
                    "tool": tc.name,
                    "success": result.success,
                    "output": result.output,
                    "error": result.error,
                });
                store.append_message(session_id, "tool", &obs.to_string());
            }
        } else if is_final(&response, &cfg.mode) {
            break;
        }
    }

    let final_session = store.load(session_id);
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        steps_taken: steps,
        last_message: final_session
            .as_ref()
            .and_then(|s| s.messages.last())
            .map(|m| m.content.clone()),
        tool_call_count: final_session.map(|s| s.tool_calls.len()).unwrap_or(0),
    })
}

fn build_system_prompt(
    context_block: &str,
    tools_schema: &[Value],
    mode: &str,
    project_root: &Path,
) -> String {
    let tool_list: String = tools_schema
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(format!(
                "- {}: {}",
                f.get("name")?.as_str()?,
                f.get("description").and_then(|d| d.as_str()).unwrap_or("")
            ))
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"You are nib, a trustworthy local-first coding agent.

{context_block}

Project root: {}
Current mode: {}

Available tools:
{}

Think step-by-step. Use tools when needed. When done, give a clear final answer."#,
        project_root.display(),
        mode,
        if tool_list.is_empty() {
            "No tools (plan mode)".to_string()
        } else {
            tool_list
        }
    )
}

fn is_final(response: &LlmResponse, mode: &str) -> bool {
    if mode == "plan" {
        return true;
    }
    if let Some(c) = &response.content {
        let lower = c.to_lowercase();
        return lower.contains("final answer") || lower.contains("task complete") || c.len() > 200;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{save_config, LlmConfig};
    use tempfile::tempdir;

    #[tokio::test]
    async fn agent_loop_with_mock() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let session = store.create_session();
        let mut cfg = LlmConfig::default();
        cfg.active_provider = Some("mock".to_string());
        save_config(dir.path(), &cfg).unwrap();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "explore the project",
            AgentLoopConfig {
                max_steps: 4,
                auto_approve: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(summary.steps_taken >= 1);
        let loaded = store.load(&session.id).unwrap();
        assert!(!loaded.messages.is_empty());
    }
}
