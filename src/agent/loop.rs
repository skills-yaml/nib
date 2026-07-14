//! Core agent loop — LLM reasoning + gated tool execution.

use crate::context::assemble_context;
use crate::llm::{create_client, LlmClient};
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
    pub stream_tx: Option<tokio::sync::mpsc::Sender<crate::llm::types::StreamEvent>>,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 15,
            mode: "execute".to_string(),
            provider: None,
            auto_approve: false,
            approval_handler: None,
            stream_tx: None,
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
                    if level == crate::tools::models::PermissionLevel::ReadOnly
                        || level == crate::tools::models::PermissionLevel::Plan
                    {
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
    let mut state = crate::agent::state::AgentState::Idle;
    let mut steps = 0u32;
    let mut messages = Vec::new();
    let mut response_content: Option<String> = None;
    let mut tool_calls = Vec::new();
    let mut tool_call_count = 0;

    while state != crate::agent::state::AgentState::Done && steps < cfg.max_steps {
        state = match state {
            crate::agent::state::AgentState::Idle => {
                steps += 1;
                let session = store.load(session_id).unwrap();
                if session.plan.is_none() {
                    crate::agent::state::AgentState::Planning
                } else {
                    crate::agent::state::AgentState::BuildContext
                }
            }
            crate::agent::state::AgentState::Planning => {
                if let Ok(plan) = crate::agent::planner::generate_plan(&llm, goal).await {
                    if let Some(mut session) = store.load(session_id) {
                        session.plan = Some(plan);
                        let _ = store.save(&session);
                    }
                }
                crate::agent::state::AgentState::BuildContext
            }
            crate::agent::state::AgentState::BuildContext => {
                // compression
                let _ = crate::context::compression::maybe_compress_session(
                    &store, session_id, &llm, &nib_cfg,
                )
                .await;

                let mut current_step_info = String::new();
                if let Some(session) = store.load(session_id) {
                    if let Some(plan) = &session.plan {
                        if plan.current_step_index < plan.steps.len() {
                            let step = &plan.steps[plan.current_step_index];
                            current_step_info = format!(
                                "\n\nCurrent Plan Step ({}/{}): {}\nStatus: {}",
                                plan.current_step_index + 1,
                                plan.steps.len(),
                                step.description,
                                step.status
                            );
                        } else {
                            current_step_info = "\n\nAll planned steps are completed.".to_string();
                        }
                    }
                }

                let system_prompt = format!(
                    "{}{}",
                    build_system_prompt(
                        &context_block,
                        use_tools_ref.unwrap_or(&[]),
                        &cfg.mode,
                        &project_root,
                    ),
                    current_step_info
                );
                messages.clear();
                messages.push(json!({"role": "system", "content": system_prompt}));

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
                crate::agent::state::AgentState::InspectLlm
            }
            crate::agent::state::AgentState::InspectLlm => {
                let mut rx = llm.stream(&messages, use_tools_ref, 0.7).await?;
                let mut accumulated_content = String::new();
                let mut current_tool_calls: std::collections::HashMap<
                    usize,
                    crate::llm::ToolCallRequest,
                > = std::collections::HashMap::new();

                while let Some(res) = rx.recv().await {
                    match res {
                        Ok(event) => {
                            if let Some(tx) = &cfg.stream_tx {
                                let _ = tx.send(event.clone()).await;
                            }
                            match event {
                                crate::llm::types::StreamEvent::Content(c) => {
                                    accumulated_content.push_str(&c);
                                }
                                crate::llm::types::StreamEvent::ToolCallChunk {
                                    index,
                                    name,
                                    arguments,
                                } => {
                                    let tc = current_tool_calls.entry(index).or_insert_with(|| {
                                        crate::llm::ToolCallRequest {
                                            name: String::new(),
                                            arguments: serde_json::json!(""),
                                        }
                                    });
                                    if let Some(n) = name {
                                        tc.name.push_str(&n);
                                    }
                                    if let Some(a) = arguments {
                                        let current = tc.arguments.as_str().unwrap_or("");
                                        tc.arguments =
                                            serde_json::Value::String(format!("{}{}", current, a));
                                    }
                                }
                                crate::llm::types::StreamEvent::End(_) => {}
                            }
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }

                response_content = if accumulated_content.is_empty() {
                    None
                } else {
                    Some(accumulated_content)
                };

                if current_tool_calls.is_empty() {
                    tool_calls.clear();
                } else {
                    let mut calls = Vec::new();
                    let mut indices: Vec<_> = current_tool_calls.keys().copied().collect();
                    indices.sort();
                    for idx in indices {
                        let mut tc = current_tool_calls.remove(&idx).unwrap();
                        if let Some(s) = tc.arguments.as_str() {
                            tc.arguments = serde_json::from_str(s).unwrap_or(serde_json::json!({}));
                        }
                        calls.push(tc);
                    }
                    tool_calls = calls;
                }

                crate::agent::state::AgentState::UpdateMemory
            }
            crate::agent::state::AgentState::UpdateMemory => {
                if let Some(content) = &response_content {
                    store.append_message(session_id, "assistant", content);
                }

                if !tool_calls.is_empty() {
                    crate::agent::state::AgentState::ToolExecute
                } else {
                    // Check if final
                    let mut is_fin = false;
                    if cfg.mode == "plan" {
                        is_fin = true;
                    } else if let Some(c) = &response_content {
                        let lower = c.to_lowercase();
                        if lower.contains("final answer")
                            || lower.contains("task complete")
                            || c.len() > 200
                        {
                            is_fin = true;
                        }
                    }
                    if is_fin {
                        if let Some(mut session) = store.load(session_id) {
                            if let Some(mut plan) = session.plan.take() {
                                if plan.current_step_index < plan.steps.len() {
                                    plan.steps[plan.current_step_index].status =
                                        "Completed".to_string();
                                    plan.current_step_index += 1;
                                }
                                session.plan = Some(plan.clone());
                                let _ = store.save(&session);

                                if plan.current_step_index >= plan.steps.len() {
                                    crate::agent::state::AgentState::Done
                                } else {
                                    crate::agent::state::AgentState::Idle
                                }
                            } else {
                                crate::agent::state::AgentState::Done
                            }
                        } else {
                            crate::agent::state::AgentState::Done
                        }
                    } else {
                        crate::agent::state::AgentState::Idle
                    }
                }
            }
            crate::agent::state::AgentState::ToolExecute => {
                let mut requires_user_input = false;
                for tc in &tool_calls {
                    if tc.name == "ask_question" {
                        requires_user_input = true;
                    }
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
                    tool_call_count += 1;
                }
                if requires_user_input {
                    crate::agent::state::AgentState::WaitingForUserInput
                } else {
                    crate::agent::state::AgentState::Idle
                }
            }
            crate::agent::state::AgentState::WaitingForUserInput => {
                // Return Done for now to break the loop (the outer UI will handle resuming)
                crate::agent::state::AgentState::Done
            }
            crate::agent::state::AgentState::Done => crate::agent::state::AgentState::Done,
        };
    }

    let final_session = store.load(session_id);
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        steps_taken: steps,
        last_message: final_session
            .as_ref()
            .and_then(|s| s.messages.last())
            .map(|m| m.content.clone()),
        tool_call_count,
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
        let cfg = LlmConfig {
            active_provider: Some("mock".to_string()),
            ..Default::default()
        };
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
