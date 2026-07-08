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
    let use_tools = cfg.mode == "execute";
    let context_block = assemble_context(&project_root, Some(goal));

    let mut steps = 0u32;
    for step in 0..cfg.max_steps {
        steps = step + 1;
        let prompt = build_prompt(
            &store,
            session_id,
            goal,
            &context_block,
            &tools_schema,
            &cfg.mode,
            &project_root,
        );
        let messages = vec![json!({"role": "user", "content": prompt})];
        let tools_ref = if use_tools {
            Some(tools_schema.as_slice())
        } else {
            None
        };

        let response = llm.complete(&messages, tools_ref, 0.7).await?;

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

fn build_prompt(
    store: &SessionStore,
    session_id: &str,
    goal: &str,
    context_block: &str,
    tools_schema: &[Value],
    mode: &str,
    project_root: &Path,
) -> String {
    let history: Vec<String> = store
        .load(session_id)
        .map(|s| {
            s.messages
                .iter()
                .rev()
                .take(8)
                .rev()
                .map(|m| format!("{}: {}", m.role.to_uppercase(), m.content))
                .collect()
        })
        .unwrap_or_default();

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

Current goal: {}

Recent conversation:
{}

Think step-by-step. Use tools when needed. When done, give a clear final answer."#,
        project_root.display(),
        mode,
        if tool_list.is_empty() {
            "No tools (plan mode)".to_string()
        } else {
            tool_list
        },
        goal,
        if history.is_empty() {
            "(none)".to_string()
        } else {
            history.join("\n")
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
