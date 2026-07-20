//! Core agent loop: planned, approved, bounded LLM reasoning and tool execution.

use crate::agent::state::AgentState;
use crate::context::budget::{build_bounded_runtime_input, RuntimePromptRequest};
use crate::context::skills::{Skill, SkillPolicyEffect};
use crate::context::{
    assemble_runtime_context_sections, select_profile_skills, RuntimeContextSection,
};
use crate::llm::{create_client, LlmClient, StreamEvent, ToolCallAccumulator, ToolCallRequest};
use crate::session::{
    normalize_plan_goal, Session, SessionEvent, SessionMessage, SessionRunLease, SessionStore,
    ToolCallRecord,
};
use crate::tools::executor::{ApprovalHandler, StdinApprovalHandler};
use crate::tools::models::{AfterToolHook, PermissionLevel, PolicyEffect, PolicyRule, ToolCall};
use crate::tools::ToolExecutor;
use chrono::Utc;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc::Sender, Notify};

#[derive(Clone, Default)]
pub struct CancellationSignal {
    state: Arc<CancellationState>,
}

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

#[derive(Default)]
struct PreparedTaskBatch {
    tasks: Vec<PreparedTask>,
}

struct PreparedTask {
    id: String,
    tool_name: String,
}

impl PreparedTaskBatch {
    fn track(&mut self, id: String, tool_name: String) {
        self.tasks.push(PreparedTask { id, tool_name });
    }

    fn start_all(&mut self) -> Vec<(PreparedTask, String)> {
        std::mem::take(&mut self.tasks)
            .into_iter()
            .filter_map(|task| {
                crate::daemons::task::TASK_MANAGER
                    .start_task(&task.id)
                    .err()
                    .map(|error| {
                        let error = match crate::daemons::task::TASK_MANAGER
                            .compensate_prepared_task(&task.id, error.clone())
                        {
                            Ok(()) => error,
                            Err(compensation_error) => format!(
                                "{error}; failed to compensate prepared task: {compensation_error}"
                            ),
                        };
                        (task, error)
                    })
            })
            .collect()
    }
}

impl Drop for PreparedTaskBatch {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            let _ = crate::daemons::task::TASK_MANAGER.compensate_prepared_task(
                &task.id,
                "prepared task was abandoned before its tool observation was persisted".to_string(),
            );
        }
    }
}

impl CancellationSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) -> bool {
        let requested = !self.state.cancelled.swap(true, Ordering::AcqRel);
        if requested {
            self.state.notify.notify_one();
        }
        requested
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        while !self.is_cancelled() {
            let notified = self.state.notify.notified();
            if self.is_cancelled() {
                break;
            }
            notified.await;
        }
    }
}

#[async_trait::async_trait]
pub trait QuestionHandler: Send + Sync {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String>;
}

pub struct AgentLoopConfig {
    /// A non-zero value overrides `agent.max_turns` for this run.
    pub max_steps: u32,
    pub mode: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub auto_approve: bool,
    pub approval_handler: Option<Arc<dyn ApprovalHandler>>,
    pub question_handler: Option<Arc<dyn QuestionHandler>>,
    pub stream_tx: Option<Sender<StreamEvent>>,
    pub cancellation: Option<CancellationSignal>,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 0,
            mode: "execute".to_string(),
            provider: None,
            model: None,
            auto_approve: false,
            approval_handler: None,
            question_handler: None,
            stream_tx: None,
            cancellation: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AgentRunSummary {
    pub session_id: String,
    pub steps_taken: u32,
    pub last_message: Option<String>,
    pub tool_call_count: usize,
    pub final_state: AgentState,
    pub outcome: String,
    pub bound_reached: bool,
    pub trace: Vec<String>,
}

pub async fn run_agent_loop(
    project_root: PathBuf,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    let runtime = prepare_agent_loop_runtime(
        &project_root,
        None,
        None,
        cfg.provider.as_deref(),
        cfg.model.as_deref(),
    )?;
    let run_lease = runtime
        .session_store
        .try_acquire_run_lease(session_id)
        .map_err(|error| error.to_string())?;
    run_agent_loop_with_runtime(runtime, session_id, goal, cfg, run_lease).await
}

pub(crate) async fn run_agent_loop_for_profile_with_lease(
    project_root: PathBuf,
    profile_id: &str,
    sessions_dir: &Path,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
    run_lease: SessionRunLease,
) -> Result<AgentRunSummary, String> {
    let runtime = prepare_agent_loop_runtime(
        &project_root,
        Some(profile_id),
        Some(sessions_dir),
        cfg.provider.as_deref(),
        cfg.model.as_deref(),
    )?;
    run_agent_loop_with_runtime(runtime, session_id, goal, cfg, run_lease).await
}

struct AgentLoopRuntime {
    nib_cfg: crate::config::NibConfig,
    profile: crate::profile::Profile,
    session_store: SessionStore,
}

fn prepare_agent_loop_runtime(
    project_root: &Path,
    profile_id: Option<&str>,
    expected_sessions_dir: Option<&Path>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<AgentLoopRuntime, String> {
    let mut nib_cfg =
        crate::config::load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    apply_model_override(&mut nib_cfg, provider, model)?;
    let profiles = crate::profile::ProfileRegistry::load(project_root, &nib_cfg.profiles)
        .map_err(|error| error.to_string())?;
    let profile = match profile_id {
        Some(profile_id) => profiles
            .get(profile_id)
            .ok_or_else(|| format!("agent profile no longer exists: {profile_id}"))?,
        None => profiles
            .for_workspace(project_root)
            .unwrap_or_else(|| profiles.default_profile()),
    }
    .clone();
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;
    let session_store = SessionStore::at_dir(profile.sessions_dir().to_path_buf());
    if let Some(expected_sessions_dir) = expected_sessions_dir {
        let expected_sessions_dir = expected_sessions_dir
            .canonicalize()
            .map_err(|error| format!("failed to resolve agent session scope: {error}"))?;
        if !crate::fs_security::canonical_paths_match(
            session_store.sessions_dir(),
            &expected_sessions_dir,
        ) {
            return Err(format!(
                "agent profile session scope changed: expected {}, got {}",
                expected_sessions_dir.display(),
                session_store.sessions_dir().display()
            ));
        }
    }
    Ok(AgentLoopRuntime {
        nib_cfg,
        profile,
        session_store,
    })
}

async fn run_agent_loop_with_runtime(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
    run_lease: SessionRunLease,
) -> Result<AgentRunSummary, String> {
    let sessions_dir = runtime.session_store.sessions_dir().to_path_buf();
    run_lease
        .verify_for(session_id, &sessions_dir)
        .map_err(|error| error.to_string())?;
    let cancellation = cfg.cancellation.clone();
    let stream_tx = cfg.stream_tx.clone();
    let cancellation_store = runtime.session_store.clone();
    let run_result = if let Some(cancellation) = cancellation {
        if cancellation.is_cancelled() {
            reconcile_cancelled_run(&cancellation_store, session_id, &stream_tx).await
        } else {
            let mut running = Box::pin(run_agent_loop_inner(runtime, session_id, goal, cfg));
            tokio::select! {
                biased;
                result = &mut running => result,
                _ = cancellation.cancelled() => {
                    drop(running);
                    reconcile_cancelled_run(&cancellation_store, session_id, &stream_tx).await
                }
            }
        }
    } else {
        run_agent_loop_inner(runtime, session_id, goal, cfg).await
    };
    let result = match (run_result, run_lease.verify_for(session_id, &sessions_dir)) {
        (Ok(summary), Ok(())) => Ok(summary),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.to_string()),
        (Err(run_error), Err(lease_error)) => Err(format!(
            "{run_error}; active run lease verification failed: {lease_error}"
        )),
    };
    if let Ok(summary) = &result {
        emit(&stream_tx, StreamEvent::End(summary.outcome.clone())).await;
    }
    result
}

async fn run_agent_loop_inner(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    let normalized_goal = normalize_plan_goal(goal);
    if normalized_goal.is_empty() {
        return Err("agent goal cannot be empty".to_string());
    }
    if !matches!(cfg.mode.as_str(), "execute" | "plan") {
        return Err(format!("unsupported agent mode: {}", cfg.mode));
    }

    let AgentLoopRuntime {
        nib_cfg,
        profile,
        session_store: store,
    } = runtime;
    let project_root = profile.root_path().to_path_buf();
    let max_turns = if cfg.max_steps == 0 {
        nib_cfg.agent.max_turns.max(1)
    } else {
        cfg.max_steps
    };
    let max_transitions = max_turns.saturating_mul(10).saturating_add(10);
    let llm: Arc<dyn LlmClient> = create_client(&nib_cfg.llm, cfg.provider.as_deref())?;
    let active_skills = select_profile_skills(&project_root, &nib_cfg, &profile, goal)?;
    let policy_rules = skill_policy_rules(&active_skills);
    let after_tool_hooks = skill_after_tool_hooks(&active_skills);
    let sensitive_values = nib_cfg.sensitive_values();

    let mcp_manager = if nib_cfg.mcp.client_enabled && !nib_cfg.mcp.servers.is_empty() {
        Some(Arc::new(
            crate::integrations::mcp::McpManager::new(&nib_cfg.mcp.servers, &sensitive_values)
                .await
                .map_err(|error| format!("failed to initialize MCP clients: {error}"))?,
        ))
    } else {
        None
    };
    let mut executor = ToolExecutor::new(project_root.clone(), nib_cfg.execution.clone())
        .with_auto_approve(cfg.auto_approve)
        .with_terminal_config(&nib_cfg.terminal)
        .with_approvals_config(&nib_cfg.approvals)
        .with_session_store(store.clone())
        .with_environment(profile.custom_env())
        .with_sensitive_values(sensitive_values)
        .with_deferred_background_start(true)
        .with_policy_rules(policy_rules)
        .with_after_tool_hooks(after_tool_hooks);
    if let Some(stream_tx) = cfg.stream_tx.clone() {
        let (terminal_tx, mut terminal_rx) = tokio::sync::mpsc::channel(64);
        executor = executor.with_terminal_output_sender(terminal_tx);
        tokio::spawn(async move {
            while let Some(event) = terminal_rx.recv().await {
                let stream = match event.stream {
                    crate::tools::core::TerminalOutputStream::Stdout => "stdout",
                    crate::tools::core::TerminalOutputStream::Stderr => "stderr",
                };
                let _ = stream_tx
                    .send(StreamEvent::TerminalOutput {
                        tool_name: event.tool_name,
                        stream: stream.to_string(),
                        chunk: String::from_utf8_lossy(&event.chunk).into_owned(),
                        background_task_id: event.background_task_id,
                    })
                    .await;
            }
        });
    }
    if let Some(mcp) = mcp_manager {
        executor = executor.with_mcp_manager(mcp);
    }
    if let Some(handler) = cfg.approval_handler.clone() {
        executor = executor.with_approval_handler(handler);
    }

    if store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .is_none()
    {
        store
            .try_create_session_with_id(session_id.to_string())
            .map_err(|error| format!("failed to create session {session_id}: {error}"))?;
    }
    invalidate_nonresumable_plan(&store, session_id, &normalized_goal)?;
    prepare_user_turn(&store, session_id, goal)?;
    for skill in &active_skills {
        let reason = if profile
            .active_skills()
            .iter()
            .any(|active| active.eq_ignore_ascii_case(&skill.frontmatter.name))
        {
            "profile active skill"
        } else {
            "matched current goal"
        };
        store
            .record_skill_usage(
                session_id,
                &skill.frontmatter.name,
                Some(reason.to_string()),
            )
            .map_err(|error| error.to_string())?;
    }

    if nib_cfg.daemons.cron_enabled && nib_cfg.daemons.curator_enabled {
        let maintenance_started = Instant::now();
        let maintenance = crate::daemons::cron::Cron::run_profile_maintenance_due(
            &profile,
            nib_cfg.daemons.interval_seconds,
            nib_cfg.daemons.retention_days,
            crate::daemons::curator::CuratorPolicy {
                allow_destructive_cleanup: nib_cfg.daemons.allow_destructive_cleanup,
            },
            Utc::now(),
        );
        match maintenance {
            Ok(Some(report)) => record_curator_tool_call(
                &store,
                session_id,
                profile.id(),
                &nib_cfg,
                Some(&report),
                None,
                maintenance_started.elapsed().as_secs_f64(),
            )?,
            Ok(None) => {}
            Err(error) => {
                record_curator_tool_call(
                    &store,
                    session_id,
                    profile.id(),
                    &nib_cfg,
                    None,
                    Some(&error),
                    maintenance_started.elapsed().as_secs_f64(),
                )?;
                return Err(error);
            }
        }
    }

    let tools_schema = executor.get_tools_schema().await;
    let memory = if nib_cfg.memory.enabled {
        profile.memory_store().load_result()?
    } else {
        crate::session::memory::MemoryStoreData::default()
    };
    let mut context_sections =
        assemble_runtime_context_sections(&project_root, goal, &active_skills, &memory);
    let workload_store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        profile.daemon_dir().to_path_buf(),
    )?;
    context_sections.workload = workload_context_sections(&workload_store.list()?);
    let tools_ref = (cfg.mode == "execute").then_some(tools_schema.as_slice());

    let mut state = AgentState::Idle;
    let mut trace = vec![state.as_str().to_string()];
    let mut transition_count = 0u32;
    let mut llm_turns = 0u32;
    let mut tool_call_count = 0usize;
    let mut messages = Vec::new();
    let mut llm_tools: Option<Vec<Value>> = None;
    let mut response_content: Option<String> = None;
    let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
    let mut reconciliation_reason: Option<String> = None;
    let mut pending_question: Option<ToolCallRequest> = None;
    let mut pending_observations: Vec<Value> = Vec::new();
    let mut pending_batch_success = true;
    let mut outcome = "running".to_string();
    let mut bound_reached = false;
    let mut active_plan_id: Option<String> = None;

    store
        .record_event(
            session_id,
            "state_transition",
            json!({"from": Value::Null, "to": state.as_str()}),
        )
        .map_err(|error| error.to_string())?;
    emit(
        &cfg.stream_tx,
        StreamEvent::StateTransition {
            state: state.as_str().to_string(),
        },
    )
    .await;

    while state != AgentState::Done {
        if transition_count >= max_transitions && state != AgentState::Reconciliation {
            bound_reached = true;
            reconciliation_reason = Some("transition_limit_reached".to_string());
            state = transition_state(
                &store,
                session_id,
                state,
                AgentState::Reconciliation,
                &mut trace,
                &mut transition_count,
                &cfg.stream_tx,
            )
            .await?;
            continue;
        }

        state = match state {
            AgentState::Idle => {
                let (next, plan_id) = route_idle_plan(&store, session_id, &normalized_goal)?;
                active_plan_id = plan_id;
                transition_state(
                    &store,
                    session_id,
                    state,
                    next,
                    &mut trace,
                    &mut transition_count,
                    &cfg.stream_tx,
                )
                .await?
            }
            AgentState::Planning => {
                if llm_turns >= max_turns {
                    bound_reached = true;
                    reconciliation_reason = Some("turn_limit_reached".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    llm_turns += 1;
                    let planning_session = store
                        .load_result(session_id)
                        .map_err(|error| {
                            format!("failed to load session context for planning: {error}")
                        })?
                        .ok_or_else(|| "session disappeared before planning".to_string())?;
                    match crate::agent::planner::generate_plan_with_context_events_bounded(
                        &llm,
                        goal,
                        &context_sections,
                        Some(&planning_session),
                        cfg.stream_tx.as_ref(),
                        nib_cfg.llm.context_length,
                    )
                    .await
                    {
                        Ok(plan) => {
                            let step_count = plan.steps.len();
                            let plan_id = plan.id.clone();
                            let plan_goal = plan.goal.clone();
                            let stored = store
                                .update_session(session_id, |session| {
                                    if let Some(current) = session.plan.as_ref() {
                                        let current_plan_id = current.id.clone();
                                        let current_goal = current.goal.clone();
                                        append_session_event(
                                            session,
                                            "plan_generation_conflict",
                                            json!({
                                                "generated_plan_id": plan_id,
                                                "generated_goal": plan_goal,
                                                "current_plan_id": current_plan_id,
                                                "current_goal": current_goal,
                                            }),
                                        );
                                        return Ok(false);
                                    }
                                    session.plan = Some(plan);
                                    session.events.push(SessionEvent {
                                        index: session.events.len(),
                                        kind: "plan_generated".to_string(),
                                        details: json!({
                                            "plan_id": plan_id,
                                            "goal": plan_goal,
                                            "step_count": step_count,
                                        }),
                                        timestamp: Some(Utc::now()),
                                    });
                                    Ok(true)
                                })
                                .map_err(|error| error.to_string())?;
                            if stored {
                                active_plan_id = Some(plan_id);
                                emit(&cfg.stream_tx, StreamEvent::PlanGenerated { step_count })
                                    .await;
                                if cfg.mode == "plan" {
                                    reconciliation_reason = Some("plan_ready".to_string());
                                    transition_state(
                                        &store,
                                        session_id,
                                        state,
                                        AgentState::Reconciliation,
                                        &mut trace,
                                        &mut transition_count,
                                        &cfg.stream_tx,
                                    )
                                    .await?
                                } else {
                                    transition_state(
                                        &store,
                                        session_id,
                                        state,
                                        AgentState::PlanApproval,
                                        &mut trace,
                                        &mut transition_count,
                                        &cfg.stream_tx,
                                    )
                                    .await?
                                }
                            } else {
                                reconciliation_reason =
                                    Some("plan_changed_during_generation".to_string());
                                transition_state(
                                    &store,
                                    session_id,
                                    state,
                                    AgentState::Reconciliation,
                                    &mut trace,
                                    &mut transition_count,
                                    &cfg.stream_tx,
                                )
                                .await?
                            }
                        }
                        Err(error) => {
                            reconciliation_reason = Some(format!("planning_failed: {error}"));
                            transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::Reconciliation,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?
                        }
                    }
                }
            }
            AgentState::PlanApproval => {
                let expected_plan_id = active_plan_id
                    .clone()
                    .ok_or_else(|| "plan approval state has no bound plan".to_string())?;
                let session = store
                    .load_result(session_id)
                    .map_err(|error| {
                        format!("failed to load session before plan approval: {error}")
                    })?
                    .ok_or_else(|| "session disappeared before plan approval".to_string())?;
                let plan = session
                    .plan
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| "plan approval state has no plan".to_string())?;
                if plan.id != expected_plan_id
                    || !plan.is_structured()
                    || !plan.matches_goal(&normalized_goal)
                {
                    record_plan_binding_conflict(
                        &store,
                        session_id,
                        &expected_plan_id,
                        &normalized_goal,
                        "plan_approval",
                    )?;
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else if plan.approved {
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::BuildContext,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    let arguments = json!({
                        "plan_id": plan.id,
                        "goal": plan.goal,
                        "steps": plan.steps.iter().map(|step| &step.description).collect::<Vec<_>>(),
                    });
                    emit(
                        &cfg.stream_tx,
                        StreamEvent::ApprovalRequired {
                            tool_name: "approve_plan".to_string(),
                            arguments: arguments.clone(),
                        },
                    )
                    .await;
                    store
                        .record_event(
                            session_id,
                            "approval_required",
                            json!({"kind": "plan", "arguments": arguments}),
                        )
                        .map_err(|error| error.to_string())?;

                    let approved = if cfg.auto_approve {
                        true
                    } else {
                        let handler: Arc<dyn ApprovalHandler> = cfg
                            .approval_handler
                            .clone()
                            .unwrap_or_else(|| Arc::new(StdinApprovalHandler));
                        handler
                            .handle_approval(
                                &ToolCall {
                                    tool_name: "approve_plan".to_string(),
                                    arguments: arguments.clone(),
                                    session_id: Some(session_id.to_string()),
                                    project_root: Some(project_root.clone()),
                                },
                                PermissionLevel::Plan,
                            )
                            .await
                            .granted
                    };
                    let decision_applied = store
                        .update_session(session_id, |session| {
                            if session.plan.as_ref() != Some(&plan) {
                                let current_plan_id =
                                    session.plan.as_ref().map(|current| current.id.clone());
                                let current_goal =
                                    session.plan.as_ref().map(|current| current.goal.clone());
                                append_session_event(
                                    session,
                                    "stale_plan_approval_ignored",
                                    json!({
                                        "expected_plan_id": expected_plan_id,
                                        "expected_goal": normalized_goal,
                                        "current_plan_id": current_plan_id,
                                        "current_goal": current_goal,
                                        "approved": approved,
                                    }),
                                );
                                return Ok(false);
                            }
                            let current = session.plan.as_mut().ok_or_else(|| {
                                crate::session::SessionError::InvalidMutation(
                                    "plan disappeared while applying approval".to_string(),
                                )
                            })?;
                            if approved {
                                current.approve();
                            } else {
                                current.reject("plan approval denied");
                            }
                            let plan_id = current.id.clone();
                            let plan_goal = current.goal.clone();
                            append_session_event(
                                session,
                                "plan_approved",
                                json!({
                                    "plan_id": plan_id,
                                    "goal": plan_goal,
                                    "approved": approved,
                                }),
                            );
                            Ok(true)
                        })
                        .map_err(|error| error.to_string())?;
                    if !decision_applied {
                        reconciliation_reason = Some("plan_binding_changed".to_string());
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    } else if approved {
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::BuildContext,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    } else {
                        reconciliation_reason = Some("plan_approval_denied".to_string());
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    }
                }
            }
            AgentState::BuildContext => {
                if verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "build_context",
                )? {
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Compression,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                }
            }
            AgentState::Compression => {
                if llm_turns >= max_turns {
                    bound_reached = true;
                    reconciliation_reason = Some("turn_limit_reached".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    if let Some(report) = crate::context::compression::maybe_compress_session(
                        &store, session_id, &llm, &nib_cfg,
                    )
                    .await?
                    {
                        llm_turns += 1;
                        emit(
                            &cfg.stream_tx,
                            StreamEvent::Compression {
                                before_tokens: report.before_tokens,
                                after_tokens: report.after_tokens,
                                summarized_through: report.summarized_through,
                            },
                        )
                        .await;
                    }
                    if llm_turns >= max_turns {
                        bound_reached = true;
                        reconciliation_reason = Some("turn_limit_reached".to_string());
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    } else {
                        let session = store
                            .load_result(session_id)
                            .map_err(|error| {
                                format!("failed to load session while building context: {error}")
                            })?
                            .ok_or_else(|| {
                                "session disappeared while building context".to_string()
                            })?;
                        if !session.plan.as_ref().is_some_and(|plan| {
                            active_plan_id.as_deref() == Some(plan.id.as_str())
                                && plan.is_structured()
                                && plan.matches_goal(&normalized_goal)
                                && plan.approved
                        }) {
                            record_plan_binding_conflict(
                                &store,
                                session_id,
                                active_plan_id.as_deref().unwrap_or(""),
                                &normalized_goal,
                                "compression",
                            )?;
                            reconciliation_reason = Some("plan_binding_changed".to_string());
                            state = transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::Reconciliation,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?;
                            continue;
                        }
                        let current_step = session
                            .plan
                            .as_ref()
                            .and_then(|plan| plan.steps.get(plan.current_step_index))
                            .map(|step| format!("{}\nStatus: {}", step.description, step.status));
                        let bounded = build_bounded_runtime_input(RuntimePromptRequest {
                            context: &context_sections,
                            session: &session,
                            current_step: current_step.as_deref(),
                            tools: tools_ref,
                            mode: &cfg.mode,
                            project_root: &project_root,
                            tool_use_enforcement: nib_cfg.agent.tool_use_enforcement,
                            context_length: nib_cfg.llm.context_length,
                        })?;
                        store
                            .record_event(
                                session_id,
                                "context_bounded",
                                json!({
                                    "context_length": nib_cfg.llm.context_length,
                                    "approximate_input_tokens": bounded.approximate_tokens,
                                    "raw_message_count": bounded.raw_message_count,
                                    "raw_tool_count": bounded.raw_tool_count,
                                    "included_tool_count": bounded.included_tool_count,
                                }),
                            )
                            .map_err(|error| error.to_string())?;
                        messages = bounded.messages;
                        llm_tools = bounded.tools;
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::InspectLlm,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    }
                }
            }
            AgentState::InspectLlm => {
                llm_turns += 1;
                let stream_result = llm.stream(&messages, llm_tools.as_deref(), 0.7).await;
                let mut stream = match stream_result {
                    Ok(stream) => stream,
                    Err(error) => {
                        reconciliation_reason = Some(format!("llm_stream_failed: {error}"));
                        state = transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?;
                        continue;
                    }
                };
                let mut content = String::new();
                let mut accumulator = ToolCallAccumulator::default();
                let mut stream_error = None;
                while let Some(result) = stream.recv().await {
                    match result {
                        Ok(event) => {
                            accumulator.push(&event);
                            if let StreamEvent::Content(fragment) = &event {
                                content.push_str(fragment);
                            }
                            emit(&cfg.stream_tx, event).await;
                        }
                        Err(error) => {
                            stream_error = Some(error);
                            break;
                        }
                    }
                }
                if let Some(error) = stream_error {
                    reconciliation_reason = Some(format!("llm_stream_failed: {error}"));
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    response_content = (!content.trim().is_empty()).then_some(content);
                    match accumulator.finish() {
                        Ok(calls) => {
                            tool_calls = calls;
                            transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::UpdateMemory,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?
                        }
                        Err(error) => {
                            reconciliation_reason = Some(format!("invalid_tool_stream: {error}"));
                            transition_state(
                                &store,
                                session_id,
                                state,
                                AgentState::Reconciliation,
                                &mut trace,
                                &mut transition_count,
                                &cfg.stream_tx,
                            )
                            .await?
                        }
                    }
                }
            }
            AgentState::UpdateMemory => {
                if !verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "update_memory",
                )? {
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }
                if tool_calls.is_empty() {
                    if let Some(content) = response_content.as_deref() {
                        store
                            .try_append_message(session_id, "assistant", content)
                            .map_err(|error| error.to_string())?;
                        reconciliation_reason = Some("model_response".to_string());
                    } else {
                        reconciliation_reason = Some("empty_model_response".to_string());
                    }
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    let intent = json!({
                        "content": response_content,
                        "tool_calls": tool_calls.iter().map(|call| json!({
                            "name": call.name,
                            "arguments": call.arguments,
                        })).collect::<Vec<_>>(),
                    });
                    store
                        .try_append_message(session_id, "assistant", &intent.to_string())
                        .map_err(|error| error.to_string())?;
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::UserApproval,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                }
            }
            AgentState::UserApproval => {
                if !verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "user_approval",
                )? {
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }
                for call in &tool_calls {
                    let tool_call = ToolCall {
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        session_id: Some(session_id.to_string()),
                        project_root: Some(project_root.clone()),
                    };
                    if executor.requires_interactive_approval(&tool_call) {
                        emit(
                            &cfg.stream_tx,
                            StreamEvent::ApprovalRequired {
                                tool_name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        )
                        .await;
                        store
                            .record_event(
                                session_id,
                                "approval_required",
                                json!({"kind": "tool", "tool_name": call.name, "arguments": call.arguments}),
                            )
                            .map_err(|error| error.to_string())?;
                    }
                }
                transition_state(
                    &store,
                    session_id,
                    state,
                    AgentState::ToolExecute,
                    &mut trace,
                    &mut transition_count,
                    &cfg.stream_tx,
                )
                .await?
            }
            AgentState::ToolExecute => {
                if !verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "tool_execute",
                )? {
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }
                let question_count = tool_calls
                    .iter()
                    .filter(|request| request.name == "ask_question")
                    .count();
                if question_count > 0 && tool_calls.len() != 1 {
                    let error = "ask_question must be the only tool call in its batch";
                    let observations = tool_calls
                        .iter()
                        .map(|request| {
                            json!({
                                "tool": request.name,
                                "success": false,
                                "output": Value::Null,
                                "error": error,
                            })
                        })
                        .collect::<Vec<_>>();
                    for request in &tool_calls {
                        emit(
                            &cfg.stream_tx,
                            StreamEvent::ToolCompleted {
                                tool_name: request.name.clone(),
                                success: false,
                                output: None,
                                error: Some(error.to_string()),
                            },
                        )
                        .await;
                    }
                    store
                        .record_event(
                            session_id,
                            "tool_batch_rejected",
                            json!({
                                "reason": "mixed_question_batch",
                                "tool_names": tool_calls.iter().map(|call| &call.name).collect::<Vec<_>>(),
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                    store
                        .try_append_message(
                            session_id,
                            "tool",
                            &json!({"observations": observations}).to_string(),
                        )
                        .map_err(|error| error.to_string())?;
                    tool_calls.clear();
                    response_content = None;
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::BuildContext,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }

                let mut observations = Vec::new();
                let mut batch_success = true;
                let mut batch_user_denied = false;
                let mut prepared_tasks = PreparedTaskBatch::default();
                for request in &tool_calls {
                    emit(
                        &cfg.stream_tx,
                        StreamEvent::ToolStarted {
                            tool_name: request.name.clone(),
                            arguments: request.arguments.clone(),
                        },
                    )
                    .await;
                    store
                        .record_event(
                            session_id,
                            "tool_started",
                            json!({"tool_name": request.name, "arguments": request.arguments}),
                        )
                        .map_err(|error| error.to_string())?;
                    if request.name == "ask_question" {
                        if pending_question.is_none() {
                            pending_question = Some(request.clone());
                        } else {
                            let result = executor
                                .execute(
                                    ToolCall {
                                        tool_name: request.name.clone(),
                                        arguments: request.arguments.clone(),
                                        session_id: Some(session_id.to_string()),
                                        project_root: Some(project_root.clone()),
                                    },
                                    Some(session_id),
                                )
                                .await;
                            tool_call_count += 1;
                            batch_success = false;
                            let error = "only one question can be pending at a time".to_string();
                            let output = result.output;
                            emit(
                                &cfg.stream_tx,
                                StreamEvent::ToolCompleted {
                                    tool_name: request.name.clone(),
                                    success: false,
                                    output: output.clone(),
                                    error: Some(error.clone()),
                                },
                            )
                            .await;
                            store
                                .record_event(
                                    session_id,
                                    "tool_completed",
                                    json!({
                                        "tool_name": request.name,
                                        "success": false,
                                        "output": output.clone(),
                                        "error": error.clone(),
                                    }),
                                )
                                .map_err(|error| error.to_string())?;
                            observations.push(json!({
                                "tool": request.name,
                                "success": false,
                                "output": output,
                                "error": error,
                            }));
                        }
                        continue;
                    }
                    let result = executor
                        .execute(
                            ToolCall {
                                tool_name: request.name.clone(),
                                arguments: request.arguments.clone(),
                                session_id: Some(session_id.to_string()),
                                project_root: Some(project_root.clone()),
                            },
                            Some(session_id),
                        )
                        .await;
                    tool_call_count += 1;
                    batch_success &= result.success;
                    batch_user_denied |= !result.approval_granted
                        && result.approval_source.as_deref() == Some("denied");
                    let prepared_work = request.name == "schedule"
                        || (request.name == "run_terminal"
                            && request
                                .arguments
                                .get("background")
                                .and_then(Value::as_bool)
                                .unwrap_or(false));
                    if result.success && prepared_work {
                        if let Some(task_id) = result
                            .output
                            .as_ref()
                            .and_then(|output| output.get("task_id"))
                            .and_then(Value::as_str)
                        {
                            prepared_tasks.track(task_id.to_string(), request.name.clone());
                        }
                    }
                    let output = result.output.clone();
                    let error = result.error.clone();
                    emit(
                        &cfg.stream_tx,
                        StreamEvent::ToolCompleted {
                            tool_name: request.name.clone(),
                            success: result.success,
                            output: output.clone(),
                            error: error.clone(),
                        },
                    )
                    .await;
                    store
                        .record_event(
                            session_id,
                            "tool_completed",
                            json!({
                                "tool_name": request.name,
                                "success": result.success,
                                "output": output,
                                "error": error,
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                    observations.push(json!({
                        "tool": request.name,
                        "success": result.success,
                        "output": result.output,
                        "error": result.error,
                    }));
                }
                if pending_question.is_some() {
                    pending_observations = observations;
                    pending_batch_success = batch_success;
                    tool_calls.clear();
                    response_content = None;
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::WaitingForUserInput,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    store
                        .try_append_message(
                            session_id,
                            "tool",
                            &json!({"observations": observations}).to_string(),
                        )
                        .map_err(|error| error.to_string())?;
                    for (task, error) in prepared_tasks.start_all() {
                        store
                            .record_event(
                                session_id,
                                "prepared_task_start_failed",
                                json!({
                                    "task_id": task.id,
                                    "tool_name": task.tool_name,
                                    "error": error,
                                }),
                            )
                            .map_err(|error| error.to_string())?;
                        batch_success = false;
                    }
                    let tool_outcome = if batch_success {
                        "tool batch succeeded"
                    } else {
                        "one or more tools failed"
                    };
                    let plan_updated = update_plan_tool_outcome(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                        &normalized_goal,
                        batch_success,
                        tool_outcome,
                    )?;
                    tool_calls.clear();
                    response_content = None;
                    if !plan_updated {
                        reconciliation_reason = Some("plan_binding_changed".to_string());
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    } else if batch_user_denied {
                        reconciliation_reason = Some("tool_execution_failed".to_string());
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::Reconciliation,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    } else {
                        transition_state(
                            &store,
                            session_id,
                            state,
                            AgentState::BuildContext,
                            &mut trace,
                            &mut transition_count,
                            &cfg.stream_tx,
                        )
                        .await?
                    }
                }
            }
            AgentState::WaitingForUserInput => {
                if !verify_bound_plan(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    true,
                    "waiting_for_user_input",
                )? {
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }
                let request = pending_question
                    .take()
                    .ok_or_else(|| "waiting-for-input state has no pending question".to_string())?;
                let question = request
                    .arguments
                    .get("question")
                    .and_then(Value::as_str)
                    .filter(|question| !question.trim().is_empty())
                    .ok_or_else(|| "ask_question requires a non-empty question".to_string())?
                    .to_string();
                let options = request
                    .arguments
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                emit(
                    &cfg.stream_tx,
                    StreamEvent::QuestionRequired {
                        question: question.clone(),
                        options: options.clone(),
                    },
                )
                .await;
                store
                    .record_event(
                        session_id,
                        "question_required",
                        json!({"question": question, "options": options}),
                    )
                    .map_err(|error| error.to_string())?;

                let answer = match cfg.question_handler.as_ref() {
                    Some(handler) => handler.ask(&question, &options).await,
                    None => Err("no question handler configured".to_string()),
                }
                .and_then(|answer| {
                    if answer.trim().is_empty() {
                        Err("question handler returned an empty answer".to_string())
                    } else {
                        Ok(answer)
                    }
                });

                let mut arguments = request.arguments.clone();
                if let Some(object) = arguments.as_object_mut() {
                    match &answer {
                        Ok(answer) => {
                            object.insert("answer".to_string(), json!(answer));
                        }
                        Err(error) => {
                            object.insert("answer_error".to_string(), json!(error));
                        }
                    }
                }
                let result = executor
                    .execute(
                        ToolCall {
                            tool_name: request.name.clone(),
                            arguments,
                            session_id: Some(session_id.to_string()),
                            project_root: Some(project_root.clone()),
                        },
                        Some(session_id),
                    )
                    .await;
                tool_call_count += 1;
                let (question_success, question_output, question_error) = match answer {
                    Ok(answer) if result.success => (
                        true,
                        Some(json!({"question": question, "answer": answer})),
                        None,
                    ),
                    Ok(_) => (false, result.output, result.error),
                    Err(error) => (false, result.output, Some(error)),
                };
                emit(
                    &cfg.stream_tx,
                    StreamEvent::ToolCompleted {
                        tool_name: request.name.clone(),
                        success: question_success,
                        output: question_output.clone(),
                        error: question_error.clone(),
                    },
                )
                .await;
                store
                    .record_event(
                        session_id,
                        "tool_completed",
                        json!({
                            "tool_name": request.name,
                            "success": question_success,
                            "output": question_output,
                            "error": question_error,
                        }),
                    )
                    .map_err(|error| error.to_string())?;
                pending_observations.push(json!({
                    "tool": request.name,
                    "success": question_success,
                    "output": question_output,
                    "error": question_error,
                }));
                let batch_success = pending_batch_success && question_success;
                store
                    .try_append_message(
                        session_id,
                        "tool",
                        &json!({"observations": pending_observations}).to_string(),
                    )
                    .map_err(|error| error.to_string())?;
                let plan_updated = update_plan_tool_outcome(
                    &store,
                    session_id,
                    active_plan_id.as_deref(),
                    &normalized_goal,
                    batch_success,
                    if batch_success {
                        "question answered"
                    } else {
                        "question was not answered"
                    },
                )?;
                pending_observations.clear();
                pending_batch_success = true;
                if !plan_updated {
                    reconciliation_reason = Some("plan_binding_changed".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else if batch_success {
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::BuildContext,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                } else {
                    reconciliation_reason = Some("waiting_for_user_input".to_string());
                    transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Reconciliation,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?
                }
            }
            AgentState::Reconciliation => {
                let reason = reconciliation_reason
                    .take()
                    .unwrap_or_else(|| "reconciled".to_string());
                let mut continue_plan = false;
                outcome = match reason.as_str() {
                    "model_response" => {
                        let (binding_matches, should_continue, next_step) = store
                            .update_session(session_id, |session| {
                                let binding_matches = session.plan.as_ref().is_some_and(|plan| {
                                    active_plan_id.as_deref() == Some(plan.id.as_str())
                                        && plan.is_structured()
                                        && plan.matches_goal(&normalized_goal)
                                        && plan.approved
                                });
                                if !binding_matches {
                                    let current_plan_id =
                                        session.plan.as_ref().map(|plan| plan.id.clone());
                                    let current_goal =
                                        session.plan.as_ref().map(|plan| plan.goal.clone());
                                    append_session_event(
                                        session,
                                        "plan_binding_conflict",
                                        json!({
                                            "stage": "reconciliation",
                                            "expected_plan_id": active_plan_id,
                                            "expected_goal": normalized_goal,
                                            "current_plan_id": current_plan_id,
                                            "current_goal": current_goal,
                                        }),
                                    );
                                    return Ok((false, false, None));
                                }
                                let mut next_step = None;
                                let mut should_continue = false;
                                if let Some(plan) = session.plan.as_mut() {
                                    plan.complete_current_step(
                                        response_content
                                            .as_deref()
                                            .unwrap_or("model completed the plan step"),
                                    );
                                    should_continue = !plan.is_complete();
                                    next_step = plan
                                        .steps
                                        .get(plan.current_step_index)
                                        .map(|step| step.description.clone());
                                }
                                Ok((true, should_continue, next_step))
                            })
                            .map_err(|error| error.to_string())?;
                        if !binding_matches {
                            continue_plan = false;
                            "plan_binding_changed".to_string()
                        } else if should_continue {
                            continue_plan = true;
                            let next_step =
                                next_step.as_deref().unwrap_or("the next approved step");
                            store
                                .try_append_message(
                                    session_id,
                                    "user",
                                    &format!("Continue with approved plan step: {next_step}"),
                                )
                                .map_err(|error| error.to_string())?;
                            "step_completed".to_string()
                        } else {
                            continue_plan = false;
                            "completed".to_string()
                        }
                    }
                    "plan_ready" => {
                        append_assistant_if_allowed(
                            &store,
                            session_id,
                            "Structured plan generated and awaiting execution approval.",
                        )?;
                        "plan_ready".to_string()
                    }
                    "plan_approval_denied" => {
                        append_assistant_if_allowed(
                            &store,
                            session_id,
                            "Plan approval denied; no tools were executed.",
                        )?;
                        "plan_approval_denied".to_string()
                    }
                    other => {
                        append_assistant_if_allowed(
                            &store,
                            session_id,
                            &format!("Run reconciled with outcome: {other}"),
                        )?;
                        other.to_string()
                    }
                };
                store
                    .record_event(
                        session_id,
                        "reconciliation",
                        json!({"outcome": outcome, "continue": continue_plan}),
                    )
                    .map_err(|error| error.to_string())?;
                emit(
                    &cfg.stream_tx,
                    StreamEvent::Reconciled {
                        outcome: outcome.clone(),
                    },
                )
                .await;
                response_content = None;
                transition_state(
                    &store,
                    session_id,
                    state,
                    if continue_plan {
                        AgentState::BuildContext
                    } else {
                        AgentState::Done
                    },
                    &mut trace,
                    &mut transition_count,
                    &cfg.stream_tx,
                )
                .await?
            }
            AgentState::Done => AgentState::Done,
        };
    }

    let final_session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load final session: {error}"))?;
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        steps_taken: llm_turns,
        last_message: final_session
            .as_ref()
            .and_then(|session| session.messages.last())
            .map(|message| message.content.clone()),
        tool_call_count,
        final_state: state,
        outcome,
        bound_reached,
        trace,
    })
}

fn workload_context_sections(
    records: &[crate::daemons::workload::DurableTaskRecord],
) -> Vec<RuntimeContextSection> {
    let mut status_counts = std::collections::BTreeMap::<&str, usize>::new();
    for record in records {
        *status_counts.entry(record.status.as_str()).or_default() += 1;
    }
    let counts = status_counts
        .into_iter()
        .map(|(status, count)| format!("{status}={count}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sections = vec![RuntimeContextSection {
        label: "workload.snapshot".to_string(),
        content: format!(
            "authoritative durable workload: total={}; statuses=[{}]",
            records.len(),
            counts
        ),
    }];
    sections.extend(records.iter().map(|record| RuntimeContextSection {
        label: format!("workload.task.{}", record.id),
        content: format!(
            "kind={}; status={}; cancel_requested={}; occurrences={}/{}; next_run_at={}",
            record.kind,
            record.status,
            record.cancel_requested,
            record.completed_occurrences,
            record.total_occurrences,
            record
                .next_run_at
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "none".to_string())
        ),
    }));
    sections
}

async fn reconcile_cancelled_run(
    store: &SessionStore,
    session_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AgentRunSummary, String> {
    if store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        store
            .try_create_session_with_id(session_id.to_string())
            .map_err(|error| error.to_string())?;
    }
    let (transitioned_to_reconciliation, last_message, tool_call_count, trace) = store
        .update_session(session_id, |session| {
            let current_state = last_persisted_state(session);
            append_session_event(
                session,
                "cancel_requested",
                json!({"reason": "cancelled_by_user", "state": current_state.clone()}),
            );
            let transitioned_to_reconciliation =
                current_state.as_deref() != Some(AgentState::Reconciliation.as_str());
            if transitioned_to_reconciliation {
                append_session_event(
                    session,
                    "state_transition",
                    json!({
                        "from": current_state.clone(),
                        "to": AgentState::Reconciliation.as_str(),
                    }),
                );
            }

            if let Some(plan) = session.plan.as_mut().filter(|plan| !plan.is_complete()) {
                plan.outcome = Some("cancelled_by_user".to_string());
                if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                    if step.status != "Completed" {
                        step.status = "Cancelled".to_string();
                        step.outcome = Some("cancelled_by_user".to_string());
                        step.updated_at = Some(Utc::now());
                    }
                }
            }
            if matches!(
                session.messages.last().map(|message| message.role.as_str()),
                Some("user") | Some("tool")
            ) {
                session.messages.push(SessionMessage {
                    index: session.messages.len(),
                    role: "assistant".to_string(),
                    content: "Run cancelled by user.".to_string(),
                    timestamp: Some(Utc::now()),
                });
            }
            append_session_event(
                session,
                "reconciliation",
                json!({"outcome": "cancelled_by_user", "continue": false}),
            );
            append_session_event(
                session,
                "state_transition",
                json!({
                    "from": AgentState::Reconciliation.as_str(),
                    "to": AgentState::Done.as_str(),
                }),
            );
            let trace = session
                .events
                .iter()
                .filter(|event| event.kind == "state_transition")
                .filter_map(|event| event.details.get("to").and_then(Value::as_str))
                .map(str::to_string)
                .collect();
            Ok((
                transitioned_to_reconciliation,
                session
                    .messages
                    .last()
                    .map(|message| message.content.clone()),
                session.tool_calls.len(),
                trace,
            ))
        })
        .map_err(|error| format!("failed to reconcile cancelled session: {error}"))?;

    if transitioned_to_reconciliation {
        emit(
            stream_tx,
            StreamEvent::StateTransition {
                state: AgentState::Reconciliation.as_str().to_string(),
            },
        )
        .await;
    }
    emit(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: "cancelled_by_user".to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    )
    .await;

    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        steps_taken: 0,
        last_message,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: "cancelled_by_user".to_string(),
        bound_reached: false,
        trace,
    })
}

fn last_persisted_state(session: &Session) -> Option<String> {
    session
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "state_transition")
        .and_then(|event| event.details.get("to"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn append_session_event(session: &mut Session, kind: &str, details: Value) {
    session.events.push(SessionEvent {
        index: session.events.len(),
        kind: kind.to_string(),
        details,
        timestamp: Some(Utc::now()),
    });
}

fn record_curator_tool_call(
    store: &SessionStore,
    session_id: &str,
    profile_id: &str,
    config: &crate::config::NibConfig,
    report: Option<&crate::daemons::curator::CuratorReport>,
    error: Option<&str>,
    duration_seconds: f64,
) -> Result<(), String> {
    let policy_decision = if config.daemons.allow_destructive_cleanup {
        "destructive_cleanup_authorized_by_config"
    } else {
        "destructive_cleanup_not_authorized"
    };
    let result = match report {
        Some(report) => json!({
            "status": "completed",
            "scanned": report.scanned,
            "deleted": report.deleted,
            "pinned": report.pinned,
            "policy_skipped": report.policy_skipped,
            "retained": report.retained,
            "sessions_deleted": report.sessions_deleted,
            "memory_deleted": report.memory_deleted,
            "skills_deleted": report.skills_deleted,
            "errors": &report.errors,
        }),
        None => json!({
            "status": "error",
            "message": error.unwrap_or("curator maintenance failed"),
        }),
    };
    store
        .record_tool_call(ToolCallRecord {
            id: Some(format!("daemon-curator-{}", uuid::Uuid::new_v4())),
            session_id: Some(session_id.to_string()),
            tool_name: Some("daemon_curator".to_string()),
            arguments: json!({
                "profile_id": profile_id,
                "retention_days": config.daemons.retention_days,
                "interval_seconds": config.daemons.interval_seconds,
                "permission_level": "destructive",
                "policy_decision": policy_decision,
                "policy_source": "daemons.allow_destructive_cleanup",
            }),
            result: Some(result),
            error: error.map(str::to_string),
            duration_seconds: Some(duration_seconds),
            worktree_path: None,
            timestamp: Some(Utc::now()),
            provider: Some("internal-daemon".to_string()),
            sandbox_profile: None,
            bwrap_args: None,
            boundaries: Some(config.execution.boundaries.clone()),
            plan_id: None,
        })
        .map_err(|record_error| {
            let context = error
                .map(|maintenance_error| format!(" after maintenance error: {maintenance_error}"))
                .unwrap_or_default();
            format!("failed to record curator maintenance in session{context}: {record_error}")
        })
}

fn prepare_user_turn(store: &SessionStore, session_id: &str, content: &str) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if matches!(
                session.messages.last().map(|message| message.role.as_str()),
                Some("user") | Some("tool")
            ) {
                session.messages.push(SessionMessage {
                    index: session.messages.len(),
                    role: "assistant".to_string(),
                    content: "Previous run reconciled before accepting new user input.".to_string(),
                    timestamp: Some(Utc::now()),
                });
            }
            session.messages.push(SessionMessage {
                index: session.messages.len(),
                role: "user".to_string(),
                content: content.to_string(),
                timestamp: Some(Utc::now()),
            });
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn plan_invalidation_reason(
    plan: &crate::session::Plan,
    normalized_goal: &str,
) -> Option<&'static str> {
    if !plan.has_identity() {
        Some("legacy_plan")
    } else if !plan.is_structured() {
        Some("invalid_plan")
    } else if plan.is_complete() {
        Some("completed_plan")
    } else if !plan.matches_goal(normalized_goal) {
        Some("goal_mismatch")
    } else {
        None
    }
}

fn invalidate_plan_in_session(
    session: &mut Session,
    normalized_goal: &str,
) -> Option<&'static str> {
    let reason = session
        .plan
        .as_ref()
        .and_then(|plan| plan_invalidation_reason(plan, normalized_goal))?;
    let prior = session
        .plan
        .take()
        .expect("plan was present while determining invalidation");
    append_session_event(
        session,
        "plan_invalidated",
        json!({
            "reason": reason,
            "previous_plan_id": (!prior.id.trim().is_empty()).then_some(prior.id),
            "previous_goal": (!prior.goal.trim().is_empty()).then_some(prior.goal),
            "requested_goal": normalized_goal,
            "approved": prior.approved,
            "current_step_index": prior.current_step_index,
            "step_count": prior.steps.len(),
            "outcome": prior.outcome,
        }),
    );
    Some(reason)
}

fn route_idle_plan(
    store: &SessionStore,
    session_id: &str,
    normalized_goal: &str,
) -> Result<(AgentState, Option<String>), String> {
    store
        .update_session(session_id, |session| {
            invalidate_plan_in_session(session, normalized_goal);
            Ok(match session.plan.as_ref() {
                None => (AgentState::Planning, None),
                Some(plan) if !plan.approved => (AgentState::PlanApproval, Some(plan.id.clone())),
                Some(plan) => (AgentState::BuildContext, Some(plan.id.clone())),
            })
        })
        .map_err(|error| format!("failed to route the active session plan: {error}"))
}

fn append_plan_binding_conflict(
    session: &mut Session,
    expected_plan_id: &str,
    normalized_goal: &str,
    stage: &str,
) {
    let current_plan_id = session.plan.as_ref().map(|plan| plan.id.clone());
    let current_goal = session.plan.as_ref().map(|plan| plan.goal.clone());
    append_session_event(
        session,
        "plan_binding_conflict",
        json!({
            "stage": stage,
            "expected_plan_id": expected_plan_id,
            "expected_goal": normalized_goal,
            "current_plan_id": current_plan_id,
            "current_goal": current_goal,
        }),
    );
}

fn record_plan_binding_conflict(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: &str,
    normalized_goal: &str,
    stage: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            append_plan_binding_conflict(session, expected_plan_id, normalized_goal, stage);
            Ok(())
        })
        .map_err(|error| format!("failed to audit plan binding conflict: {error}"))
}

fn verify_bound_plan(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
    normalized_goal: &str,
    require_approved: bool,
    stage: &str,
) -> Result<bool, String> {
    store
        .update_session(session_id, |session| {
            let matches = session.plan.as_ref().is_some_and(|plan| {
                expected_plan_id == Some(plan.id.as_str())
                    && plan.is_structured()
                    && plan.matches_goal(normalized_goal)
                    && (!require_approved || plan.approved)
            });
            if !matches {
                append_plan_binding_conflict(
                    session,
                    expected_plan_id.unwrap_or(""),
                    normalized_goal,
                    stage,
                );
            }
            Ok(matches)
        })
        .map_err(|error| format!("failed to verify active plan binding: {error}"))
}

fn invalidate_nonresumable_plan(
    store: &SessionStore,
    session_id: &str,
    goal: &str,
) -> Result<(), String> {
    let normalized_goal = normalize_plan_goal(goal);
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to inspect existing plan: {error}"))?
        .ok_or_else(|| "session disappeared while inspecting existing plan".to_string())?;
    let Some(reason) = session
        .plan
        .as_ref()
        .and_then(|plan| plan_invalidation_reason(plan, &normalized_goal))
    else {
        return Ok(());
    };

    store
        .update_session(session_id, |session| {
            invalidate_plan_in_session(session, &normalized_goal);
            Ok(())
        })
        .map_err(|error| {
            format!("failed to invalidate {reason} before starting the requested goal: {error}")
        })
}

fn apply_model_override(
    config: &mut crate::config::NibConfig,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) -> Result<(), String> {
    let Some(model) = model_override
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return Ok(());
    };
    let provider = provider_override
        .map(str::to_string)
        .unwrap_or_else(|| config.llm.get_active_provider());
    let entry = config.llm.providers.get_mut(&provider).ok_or_else(|| {
        format!("cannot override model for unconfigured LLM provider: {provider}")
    })?;
    entry.model = model.to_string();
    Ok(())
}

fn append_assistant_if_allowed(
    store: &SessionStore,
    session_id: &str,
    content: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if matches!(
                session.messages.last().map(|message| message.role.as_str()),
                Some("user") | Some("tool")
            ) {
                session.messages.push(SessionMessage {
                    index: session.messages.len(),
                    role: "assistant".to_string(),
                    content: content.to_string(),
                    timestamp: Some(Utc::now()),
                });
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn update_plan_tool_outcome(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
    normalized_goal: &str,
    success: bool,
    outcome: &str,
) -> Result<bool, String> {
    store
        .update_session(session_id, |session| {
            let matches = session.plan.as_ref().is_some_and(|plan| {
                expected_plan_id == Some(plan.id.as_str())
                    && plan.is_structured()
                    && plan.matches_goal(normalized_goal)
                    && plan.approved
            });
            if !matches {
                let current_plan_id = session.plan.as_ref().map(|plan| plan.id.clone());
                let current_goal = session.plan.as_ref().map(|plan| plan.goal.clone());
                append_session_event(
                    session,
                    "plan_binding_conflict",
                    json!({
                        "stage": "tool_outcome",
                        "expected_plan_id": expected_plan_id,
                        "expected_goal": normalized_goal,
                        "current_plan_id": current_plan_id,
                        "current_goal": current_goal,
                    }),
                );
                return Ok(false);
            }
            let plan = session
                .plan
                .as_mut()
                .expect("plan presence was checked above");
            plan.record_tool_outcome(success, outcome);
            Ok(true)
        })
        .map_err(|error| error.to_string())
}

async fn transition_state(
    store: &SessionStore,
    session_id: &str,
    current: AgentState,
    next: AgentState,
    trace: &mut Vec<String>,
    transition_count: &mut u32,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AgentState, String> {
    if !current.can_transition_to(next) {
        return Err(format!("invalid agent transition: {current} -> {next}"));
    }
    *transition_count = transition_count.saturating_add(1);
    trace.push(next.as_str().to_string());
    store
        .record_event(
            session_id,
            "state_transition",
            json!({"from": current.as_str(), "to": next.as_str()}),
        )
        .map_err(|error| error.to_string())?;
    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: next.as_str().to_string(),
        },
    )
    .await;
    Ok(next)
}

async fn emit(stream_tx: &Option<Sender<StreamEvent>>, event: StreamEvent) {
    if let Some(sender) = stream_tx {
        let _ = sender.send(event).await;
    }
}

fn skill_policy_rules(skills: &[Skill]) -> Vec<PolicyRule> {
    crate::context::skills::policy_rules_for_skills(skills)
        .into_iter()
        .map(|rule| PolicyRule {
            effect: match rule.effect {
                SkillPolicyEffect::Deny => PolicyEffect::Deny,
                SkillPolicyEffect::RequireApproval => PolicyEffect::RequireApproval,
            },
            tool_name: rule.tool_name.unwrap_or_else(|| "*".to_string()),
            argument_contains: rule.argument_contains,
            reason: format!("skill '{}' constraint", rule.skill_name),
        })
        .collect()
}

fn skill_after_tool_hooks(skills: &[Skill]) -> Vec<AfterToolHook> {
    skills
        .iter()
        .flat_map(|skill| {
            skill
                .frontmatter
                .hooks
                .after_tool
                .iter()
                .filter(|hook| !hook.tool.trim().is_empty() && !hook.command.trim().is_empty())
                .map(|hook| AfterToolHook {
                    source: skill.frontmatter.name.clone(),
                    tool_name: hook.tool.clone(),
                    command: hook.command.clone(),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{save_config, LlmConfig, ProviderEntry};
    use crate::tools::models::ApprovalDecision;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    struct DenyApproval;

    struct AnswerQuestion;

    struct ApprovePlanOnly {
        calls: Arc<AtomicUsize>,
    }

    struct BlockingApproval {
        entered: Arc<tokio::sync::Notify>,
    }

    struct ControlledApproval {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        granted: bool,
    }

    #[async_trait::async_trait]
    impl QuestionHandler for AnswerQuestion {
        async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
            assert_eq!(question, "Which verification mode?");
            assert_eq!(options, ["fast", "full"]);
            Ok("full".to_string())
        }
    }

    fn mock_config() -> LlmConfig {
        LlmConfig {
            active_provider: Some("mock".to_string()),
            providers: HashMap::from([(
                "mock".to_string(),
                ProviderEntry {
                    model: "mock-model".to_string(),
                    api_key: None,
                    api_keys: Vec::new(),
                    base_url: None,
                },
            )]),
            ..Default::default()
        }
    }

    fn pending_plan(goal: &str, description: &str) -> crate::session::Plan {
        crate::session::Plan::new(
            goal,
            vec![crate::session::PlanStep {
                description: description.to_string(),
                status: "Pending".to_string(),
                outcome: None,
                attempts: 0,
                updated_at: None,
            }],
        )
    }

    #[test]
    fn plan_invalidation_is_audited_while_same_goal_resume_is_preserved() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());

        let mut resumed = store.create_session_with_id("resume");
        let mut resumable_plan = pending_plan("keep working", "continue the approved work");
        resumable_plan.approve();
        let resumable_id = resumable_plan.id.clone();
        resumed.plan = Some(resumable_plan);
        store.save(&mut resumed).expect("resumable plan");
        invalidate_nonresumable_plan(&store, "resume", "  keep\nworking ").expect("resume check");
        let resumed = store.load("resume").expect("resumed session");
        assert_eq!(resumed.plan.as_ref().unwrap().id, resumable_id);
        assert!(!resumed
            .events
            .iter()
            .any(|event| event.kind == "plan_invalidated"));

        let mut mismatched = store.create_session_with_id("mismatch");
        let mut mismatched_plan = pending_plan("old goal", "perform old work");
        mismatched_plan.approve();
        let mismatched_id = mismatched_plan.id.clone();
        mismatched.plan = Some(mismatched_plan);
        store.save(&mut mismatched).expect("mismatched plan");
        invalidate_nonresumable_plan(&store, "mismatch", "new goal")
            .expect("mismatch invalidation");
        let mismatched = store.load("mismatch").expect("mismatched session");
        assert!(mismatched.plan.is_none());
        let mismatch_event = mismatched
            .events
            .iter()
            .find(|event| event.kind == "plan_invalidated")
            .expect("mismatch audit");
        assert_eq!(mismatch_event.details["reason"], "goal_mismatch");
        assert_eq!(mismatch_event.details["previous_plan_id"], mismatched_id);
        assert_eq!(mismatch_event.details["previous_goal"], "old goal");
        assert_eq!(mismatch_event.details["requested_goal"], "new goal");

        let mut completed = store.create_session_with_id("completed");
        let mut completed_plan = pending_plan("same goal", "finish old run");
        completed_plan.approve();
        completed_plan.complete_current_step("done");
        completed.plan = Some(completed_plan);
        store.save(&mut completed).expect("completed plan");
        invalidate_nonresumable_plan(&store, "completed", "same goal")
            .expect("completed invalidation");
        let completed = store.load("completed").expect("completed session");
        assert!(completed.plan.is_none());
        assert_eq!(
            completed
                .events
                .iter()
                .find(|event| event.kind == "plan_invalidated")
                .unwrap()
                .details["reason"],
            "completed_plan"
        );

        let mut legacy = store.create_session_with_id("legacy");
        let mut legacy_plan = pending_plan("legacy goal", "legacy step");
        legacy_plan.id.clear();
        legacy_plan.goal.clear();
        legacy.plan = Some(legacy_plan);
        store.save(&mut legacy).expect("legacy plan");
        invalidate_nonresumable_plan(&store, "legacy", "legacy goal").expect("legacy invalidation");
        let legacy = store.load("legacy").expect("legacy session");
        assert!(legacy.plan.is_none());
        assert_eq!(
            legacy
                .events
                .iter()
                .find(|event| event.kind == "plan_invalidated")
                .unwrap()
                .details["reason"],
            "legacy_plan"
        );

        let mut malformed = store.create_session_with_id("malformed");
        let mut malformed_plan = pending_plan("same goal", "invalidated before execution");
        malformed_plan.approve();
        malformed_plan.steps[0].status = "Completed".to_string();
        malformed.plan = Some(malformed_plan);
        store.save(&mut malformed).expect("malformed plan");
        invalidate_nonresumable_plan(&store, "malformed", "same goal")
            .expect("malformed invalidation");
        let malformed = store.load("malformed").expect("malformed session");
        assert!(malformed.plan.is_none());
        assert_eq!(
            malformed
                .events
                .iter()
                .find(|event| event.kind == "plan_invalidated")
                .unwrap()
                .details["reason"],
            "invalid_plan"
        );
    }

    #[async_trait::async_trait]
    impl ApprovalHandler for DenyApproval {
        async fn handle_approval(
            &self,
            _call: &ToolCall,
            _level: PermissionLevel,
        ) -> ApprovalDecision {
            ApprovalDecision::denied()
        }
    }

    #[async_trait::async_trait]
    impl ApprovalHandler for ApprovePlanOnly {
        async fn handle_approval(
            &self,
            call: &ToolCall,
            _level: PermissionLevel,
        ) -> ApprovalDecision {
            assert_eq!(call.tool_name, "approve_plan");
            self.calls.fetch_add(1, Ordering::SeqCst);
            ApprovalDecision::granted_user()
        }
    }

    #[async_trait::async_trait]
    impl ApprovalHandler for BlockingApproval {
        async fn handle_approval(
            &self,
            _call: &ToolCall,
            _level: PermissionLevel,
        ) -> ApprovalDecision {
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    #[async_trait::async_trait]
    impl ApprovalHandler for ControlledApproval {
        async fn handle_approval(
            &self,
            call: &ToolCall,
            _level: PermissionLevel,
        ) -> ApprovalDecision {
            assert_eq!(call.tool_name, "approve_plan");
            self.entered.notify_one();
            self.release.notified().await;
            if self.granted {
                ApprovalDecision::granted_user()
            } else {
                ApprovalDecision::denied()
            }
        }
    }

    fn initialize_git_repository(path: &Path) {
        std::fs::write(path.join("README.md"), "test project\n").expect("seed file");
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "nib-tests@example.invalid"],
            vec!["config", "user.name", "nib tests"],
            vec!["add", "README.md"],
            vec!["commit", "--quiet", "-m", "initial"],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .expect("git command");
            assert!(status.success());
        }
    }

    #[test]
    fn runtime_model_override_targets_selected_provider() {
        let mut config = crate::config::NibConfig {
            llm: mock_config(),
            ..Default::default()
        };

        apply_model_override(&mut config, Some("mock"), Some("override-model"))
            .expect("model override");

        assert_eq!(config.llm.providers["mock"].model, "override-model");
        assert!(apply_model_override(&mut config, Some("missing"), Some("model")).is_err());
    }

    #[test]
    fn dropping_a_prepared_task_batch_fails_every_unstarted_task() {
        let task_id = format!("prepared-drop-{}", uuid::Uuid::new_v4());
        crate::daemons::task::TASK_MANAGER.register_subagent(task_id.clone());
        {
            let mut prepared = PreparedTaskBatch::default();
            prepared.track(task_id.clone(), "schedule".to_string());
        }

        let task = crate::daemons::task::TASK_MANAGER
            .get_task(&task_id)
            .expect("failed prepared task");
        assert_eq!(task["status"], "failed");
        assert!(task["error"]
            .as_str()
            .is_some_and(|error| error.contains("before its tool observation was persisted")));
    }

    #[test]
    fn dropping_a_durable_prepared_batch_persists_compensation_failure_audit() {
        let directory = tempdir().expect("tempdir");
        let sessions_dir = directory.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            directory.path().join("state/daemons"),
        )
        .expect("durable store");
        let id = format!("batch-compensation-{}", uuid::Uuid::new_v4().simple());
        store
            .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: id.clone(),
                command: "printf ok".to_string(),
                cwd: directory.path().to_path_buf(),
                project_root: directory.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir,
                session_id: "origin".to_string(),
                execution: crate::config::ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1024,
            })
            .expect("prepare task");
        crate::daemons::task::TASK_MANAGER
            .register_durable_task(id.clone(), store.clone())
            .expect("register durable task");
        let record_path = store.daemon_dir().join("tasks").join(format!("{id}.json"));
        std::fs::remove_file(&record_path).expect("remove record");
        std::fs::create_dir(&record_path).expect("inject compensation failure");

        {
            let mut prepared = PreparedTaskBatch::default();
            prepared.track(id.clone(), "run_terminal".to_string());
        }

        let records =
            crate::daemons::task::DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
                .read_all()
                .expect("compensation audit");
        assert!(records.iter().any(|record| {
            record.action == "prepared_task_compensation"
                && record.target.as_deref() == Some(id.as_str())
                && record.outcome == "compensation_failed"
        }));
    }

    #[tokio::test]
    async fn agent_loop_with_mock_has_audited_lifecycle() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "explore the project",
            AgentLoopConfig {
                max_steps: 6,
                auto_approve: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.final_state, AgentState::Done);
        assert_eq!(summary.outcome, "completed");
        assert!(summary.tool_call_count >= 1);
        assert!(summary.trace.contains(&"plan_approval".to_string()));
        assert!(summary.trace.contains(&"tool_execute".to_string()));
        let loaded = store.load(&session.id).unwrap();
        loaded.validate_message_sequence().unwrap();
        assert!(loaded.plan.unwrap().is_complete());
    }

    #[tokio::test]
    async fn same_goal_incomplete_plan_resumes_without_replanning_or_reapproval() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let mut session = store.create_session_with_id("resume-same-goal");
        let mut plan = pending_plan("explore the project", "explore the project");
        plan.approve();
        let plan_id = plan.id.clone();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved resumable plan");

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "  explore\n the project ",
            AgentLoopConfig {
                max_steps: 4,
                auto_approve: true,
                ..Default::default()
            },
        )
        .await
        .expect("same-goal resume");

        assert_eq!(summary.outcome, "completed");
        assert!(!summary.trace.contains(&"planning".to_string()));
        assert!(!summary.trace.contains(&"plan_approval".to_string()));
        let persisted = store.load(&session.id).expect("resumed session");
        assert_eq!(persisted.plan.as_ref().unwrap().id, plan_id);
        assert!(persisted.plan.as_ref().unwrap().is_complete());
        assert!(!persisted
            .events
            .iter()
            .any(|event| event.kind == "plan_invalidated"));
    }

    #[tokio::test]
    async fn different_goal_invalidates_approved_plan_before_generating_a_new_one() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let mut session = store.create_session_with_id("replace-plan-goal");
        let mut plan = pending_plan("old goal", "perform old work");
        plan.approve();
        let old_plan_id = plan.id.clone();
        session.plan = Some(plan);
        store.save(&mut session).expect("old approved plan");

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "explore the project",
            AgentLoopConfig {
                max_steps: 6,
                auto_approve: true,
                ..Default::default()
            },
        )
        .await
        .expect("replacement plan run");

        assert_eq!(summary.outcome, "completed");
        assert!(summary.trace.contains(&"planning".to_string()));
        assert!(summary.trace.contains(&"plan_approval".to_string()));
        let persisted = store.load(&session.id).expect("replanned session");
        let replacement = persisted.plan.as_ref().expect("replacement plan");
        assert_ne!(replacement.id, old_plan_id);
        assert_eq!(replacement.goal, "explore the project");
        let invalidated = persisted
            .events
            .iter()
            .find(|event| event.kind == "plan_invalidated")
            .expect("plan invalidation audit");
        assert_eq!(invalidated.details["reason"], "goal_mismatch");
        assert_eq!(invalidated.details["previous_plan_id"], old_plan_id);
    }

    #[tokio::test]
    async fn denied_plan_has_no_tool_side_effects() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "explore the project",
            AgentLoopConfig {
                max_steps: 4,
                approval_handler: Some(Arc::new(DenyApproval)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "plan_approval_denied");
        assert_eq!(summary.tool_call_count, 0);
        assert!(store
            .load(&session.id)
            .unwrap()
            .tool_calls
            .iter()
            .all(|call| call.tool_name.as_deref() == Some("daemon_curator")));
    }

    #[tokio::test]
    async fn due_profile_maintenance_is_mirrored_into_originating_session() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "explore the project",
            AgentLoopConfig {
                max_steps: 1,
                auto_approve: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.tool_call_count, 0);
        let loaded = store.load(&session.id).expect("originating session");
        let daemon_calls = loaded
            .tool_calls
            .iter()
            .filter(|call| call.tool_name.as_deref() == Some("daemon_curator"))
            .collect::<Vec<_>>();
        assert_eq!(daemon_calls.len(), 1);
        let call = daemon_calls[0];
        assert_eq!(call.session_id.as_deref(), Some(session.id.as_str()));
        assert_eq!(call.provider.as_deref(), Some("internal-daemon"));
        assert_eq!(call.arguments["profile_id"], "default");
        assert_eq!(
            call.arguments["policy_decision"],
            "destructive_cleanup_not_authorized"
        );
        assert_eq!(
            call.arguments["policy_source"],
            "daemons.allow_destructive_cleanup"
        );
        assert_eq!(call.result.as_ref().unwrap()["status"], "completed");
        assert_eq!(call.result.as_ref().unwrap()["deleted"], 0);
        assert!(call.error.is_none());
        assert!(call
            .duration_seconds
            .is_some_and(|duration| duration >= 0.0));
    }

    #[tokio::test]
    async fn configured_turn_bound_reconciles_before_execution() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "explore the project",
            AgentLoopConfig {
                max_steps: 1,
                auto_approve: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(summary.bound_reached);
        assert_eq!(summary.outcome, "turn_limit_reached");
        assert_eq!(summary.tool_call_count, 0);
        assert!(!summary.trace.contains(&"tool_execute".to_string()));
    }

    #[tokio::test]
    async fn question_handler_resumes_the_same_running_loop() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "ask a question before continuing",
            AgentLoopConfig {
                max_steps: 5,
                auto_approve: true,
                question_handler: Some(Arc::new(AnswerQuestion)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "completed");
        assert!(summary
            .trace
            .contains(&"waiting_for_user_input".to_string()));
        let loaded = store.load(&session.id).unwrap();
        assert!(loaded.messages.iter().any(|message| {
            message.role == "tool" && message.content.contains("\"answer\":\"full\"")
        }));
        loaded.validate_message_sequence().unwrap();
    }

    #[tokio::test]
    async fn mixed_question_batch_is_rejected_before_any_side_effect() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "mixed question batch",
            AgentLoopConfig {
                max_steps: 5,
                auto_approve: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "completed");
        assert_eq!(summary.tool_call_count, 0);
        assert!(!dir.path().join("mixed-side-effect.txt").exists());
        let loaded = store.load(&session.id).unwrap();
        assert!(loaded
            .events
            .iter()
            .any(|event| event.kind == "tool_batch_rejected"));
        assert!(loaded
            .tool_calls
            .iter()
            .all(|call| call.tool_name.as_deref() != Some("run_terminal")));
        loaded.validate_message_sequence().unwrap();
    }

    #[tokio::test]
    async fn failed_terminal_observation_is_available_for_bounded_self_correction() {
        let dir = tempdir().unwrap();
        initialize_git_repository(dir.path());
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "recover from terminal failure",
            AgentLoopConfig {
                max_steps: 5,
                auto_approve: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "completed");
        assert_eq!(summary.tool_call_count, 1);
        assert!(summary
            .trace
            .windows(2)
            .any(|states| states == ["tool_execute", "build_context"]));
        let loaded = store.load(&session.id).unwrap();
        assert!(loaded.messages.iter().any(|message| {
            message.role == "tool" && message.content.contains("recoverable stderr")
        }));
        let terminal = loaded
            .tool_calls
            .iter()
            .find(|call| call.tool_name.as_deref() == Some("run_terminal"))
            .expect("terminal audit");
        assert!(terminal.error.as_deref().is_some_and(|error| {
            error.contains("recoverable stderr") && error.contains("command exited with 7")
        }));
        loaded.validate_message_sequence().unwrap();
    }

    #[tokio::test]
    async fn classifier_safe_terminal_does_not_claim_tool_approval_is_pending() {
        let dir = tempdir().unwrap();
        initialize_git_repository(dir.path());
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        let approval_calls = Arc::new(AtomicUsize::new(0));
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "safe terminal approval",
            AgentLoopConfig {
                max_steps: 5,
                approval_handler: Some(Arc::new(ApprovePlanOnly {
                    calls: approval_calls.clone(),
                })),
                stream_tx: Some(stream_tx),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "completed");
        assert_eq!(approval_calls.load(Ordering::SeqCst), 1);
        let loaded = store.load(&session.id).unwrap();
        assert!(!loaded
            .events
            .iter()
            .any(|event| { event.kind == "approval_required" && event.details["kind"] == "tool" }));
        let terminal = loaded
            .tool_calls
            .iter()
            .find(|call| call.tool_name.as_deref() == Some("run_terminal"))
            .expect("terminal audit");
        assert_eq!(
            terminal.result.as_ref().unwrap()["approval"]["source"],
            "classifier"
        );
        let mut terminal_output = String::new();
        let mut saw_normal_end = false;
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(100), stream_rx.recv())
                .await
            {
                Ok(Some(StreamEvent::TerminalOutput { chunk, .. })) => {
                    terminal_output.push_str(&chunk);
                }
                Ok(Some(StreamEvent::End(reason))) if reason == "completed" => {
                    saw_normal_end = true;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert!(terminal_output.contains("ok"), "{terminal_output:?}");
        assert!(saw_normal_end, "normal completion must end the run stream");
    }

    #[tokio::test]
    async fn concurrent_same_session_run_is_rejected_until_the_owner_releases_its_lease() {
        let directory = tempdir().unwrap();
        save_config(directory.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(directory.path()).unwrap();
        let session = store.create_session();
        let session_id = session.id.clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let run_root = directory.path().to_path_buf();
        let run_session_id = session_id.clone();
        let run_entered = entered.clone();
        let run_release = release.clone();
        let first = tokio::spawn(async move {
            run_agent_loop(
                run_root,
                &run_session_id,
                "hold the session lease",
                AgentLoopConfig {
                    max_steps: 6,
                    approval_handler: Some(Arc::new(ControlledApproval {
                        entered: run_entered,
                        release: run_release,
                        granted: false,
                    })),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .expect("first run reached plan approval");
        let before_conflict = store.load(&session_id).expect("blocked session");
        let conflict = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_agent_loop(
                directory.path().to_path_buf(),
                &session_id,
                "replace the active run",
                AgentLoopConfig {
                    max_steps: 2,
                    auto_approve: true,
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("concurrent run failed closed promptly")
        .expect_err("concurrent run must not acquire the lease");
        assert!(
            conflict.contains("active agent run"),
            "unexpected conflict: {conflict}"
        );
        let after_conflict = store.load(&session_id).expect("unchanged blocked session");
        assert_eq!(after_conflict.messages, before_conflict.messages);
        assert_eq!(after_conflict.plan, before_conflict.plan);
        assert_eq!(after_conflict.events, before_conflict.events);
        assert_eq!(after_conflict.tool_calls, before_conflict.tool_calls);

        release.notify_one();
        let first_summary = tokio::time::timeout(std::time::Duration::from_secs(2), first)
            .await
            .expect("first run completed after release")
            .expect("first run joined")
            .expect("first run reconciled");
        assert_eq!(first_summary.outcome, "plan_approval_denied");

        let lease = store
            .try_acquire_run_lease(&session_id)
            .expect("lease is available after the owner finishes");
        lease.verify().expect("released lease is valid");
    }

    #[tokio::test]
    async fn stale_plan_approval_cannot_approve_a_replacement_plan() {
        let directory = tempdir().unwrap();
        save_config(directory.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(directory.path()).unwrap();
        let session = store.create_session();
        let session_id = session.id.clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let run_root = directory.path().to_path_buf();
        let run_session_id = session_id.clone();
        let run_entered = entered.clone();
        let run_release = release.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                run_root,
                &run_session_id,
                "approve only the generated plan",
                AgentLoopConfig {
                    max_steps: 6,
                    approval_handler: Some(Arc::new(ControlledApproval {
                        entered: run_entered,
                        release: run_release,
                        granted: true,
                    })),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .expect("run reached plan approval");
        let shown_plan_id = store
            .load(&session_id)
            .expect("approval session")
            .plan
            .expect("generated plan")
            .id;
        let replacement = pending_plan("replacement goal", "do not approve this plan");
        let replacement_id = replacement.id.clone();
        store
            .update_session(&session_id, |session| {
                session.plan = Some(replacement);
                Ok(())
            })
            .expect("replace plan while approval is pending");
        release.notify_one();

        let summary = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("stale approval run completed")
            .expect("stale approval run joined")
            .expect("stale approval run reconciled");
        assert_eq!(summary.outcome, "plan_binding_changed");
        assert_eq!(summary.tool_call_count, 0);
        let persisted = store.load(&session_id).expect("replacement plan session");
        let current = persisted.plan.as_ref().expect("replacement plan remains");
        assert_eq!(current.id, replacement_id);
        assert_eq!(current.goal, "replacement goal");
        assert!(!current.approved);
        let stale = persisted
            .events
            .iter()
            .find(|event| event.kind == "stale_plan_approval_ignored")
            .expect("stale approval audit");
        assert_eq!(stale.details["expected_plan_id"], shown_plan_id);
        assert_eq!(stale.details["current_plan_id"], replacement_id);
        assert!(persisted.events.iter().all(|event| {
            event.kind != "plan_approved" || event.details["plan_id"] != replacement_id
        }));
    }

    #[tokio::test]
    async fn cancellation_interrupts_blocked_approval_and_reconciles_the_session() {
        let directory = tempdir().unwrap();
        save_config(directory.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(directory.path()).unwrap();
        let session = store.create_session();
        let session_id = session.id.clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let cancellation = CancellationSignal::new();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);
        let run_root = directory.path().to_path_buf();
        let run_session_id = session_id.clone();
        let run_entered = entered.clone();
        let run_cancellation = cancellation.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                run_root,
                &run_session_id,
                "wait for plan approval",
                AgentLoopConfig {
                    max_steps: 6,
                    approval_handler: Some(Arc::new(BlockingApproval {
                        entered: run_entered,
                    })),
                    stream_tx: Some(stream_tx),
                    cancellation: Some(run_cancellation),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .expect("agent reached blocked approval");
        assert!(cancellation.cancel());
        let summary = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("cancelled run stopped promptly")
            .expect("agent task joined")
            .expect("cancelled run reconciled");

        assert_eq!(summary.outcome, "cancelled_by_user");
        assert_eq!(summary.final_state, AgentState::Done);
        assert!(summary.trace.ends_with(&[
            AgentState::Reconciliation.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ]));
        let persisted = store.load(&session_id).expect("cancelled session");
        let plan = persisted.plan.as_ref().expect("generated plan");
        assert_eq!(plan.outcome.as_deref(), Some("cancelled_by_user"));
        assert_eq!(plan.steps[plan.current_step_index].status, "Cancelled");
        assert_eq!(
            plan.steps[plan.current_step_index].outcome.as_deref(),
            Some("cancelled_by_user")
        );
        persisted.validate_message_sequence().unwrap();
        let cancellation_event = persisted
            .events
            .iter()
            .position(|event| event.kind == "cancel_requested")
            .expect("cancellation audit");
        let reconciliation = persisted
            .events
            .iter()
            .position(|event| {
                event.kind == "reconciliation" && event.details["outcome"] == "cancelled_by_user"
            })
            .expect("cancellation reconciliation");
        let done = persisted
            .events
            .iter()
            .position(|event| event.kind == "state_transition" && event.details["to"] == "done")
            .expect("terminal transition");
        assert!(cancellation_event < reconciliation && reconciliation < done);

        let mut streamed = Vec::new();
        while let Ok(event) = stream_rx.try_recv() {
            streamed.push(event);
        }
        let reconciled = streamed
            .iter()
            .position(|event| {
                matches!(
                    event,
                    StreamEvent::Reconciled { outcome } if outcome == "cancelled_by_user"
                )
            })
            .expect("reconciled stream event");
        let ended = streamed
            .iter()
            .position(
                |event| matches!(event, StreamEvent::End(reason) if reason == "cancelled_by_user"),
            )
            .expect("terminal stream event");
        assert!(reconciled < ended);
    }
}
