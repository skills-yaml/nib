//! Core agent loop: planned, approved, bounded LLM reasoning and tool execution.

use crate::agent::state::AgentState;
use crate::context::budget::{build_bounded_runtime_input, RuntimePromptRequest};
use crate::context::skills::{Skill, SkillPolicyEffect};
use crate::context::{
    assemble_runtime_context_sections, attachment_context_sections, select_profile_skills,
    RuntimeContextSection,
};
use crate::llm::{
    LlmClient, LlmError, LlmErrorClass, LlmErrorPhase, LlmRequest, LlmRequestScope, LlmResponse,
    LlmStream, LlmTerminalStatus, ProviderContinuation, StreamEvent, ToolCallRequest,
    ToolResult as ProviderToolResult, ToolResultClass,
};
use crate::session::{
    normalize_plan_goal, Session, SessionEvent, SessionMessage, SessionRunLease, SessionStore,
    ToolCallRecord,
};
use crate::tools::classifier::ToolRisk;
use crate::tools::executor::{ApprovalHandler, StdinApprovalHandler};
use crate::tools::models::{AfterToolHook, PermissionLevel, PolicyEffect, PolicyRule, ToolCall};
use crate::tools::ToolExecutor;
use chrono::Utc;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, mpsc::Sender, Notify};

const MAX_QUESTION_BYTES: usize = 20_000;
const MAX_QUESTION_OPTION_BYTES: usize = 1_000;
const MAX_PUBLIC_PROVIDER_CONTENT_BYTES: usize = 64 * 1024;
const MAX_PUBLIC_PROVIDER_TOOL_CHUNK_BYTES: usize = 8 * 1024;
const MAX_PERSISTED_PROVIDER_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_STEERING_INPUT_BYTES: usize = 8 * 1024;
const MAX_STEERING_INPUTS_PER_RUN: usize = 32;
const MAX_STEERING_TOTAL_BYTES_PER_RUN: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SteeringInstruction {
    sequence: usize,
    text: String,
}

#[derive(Clone)]
pub struct ExactRunSteeringHandle {
    store: SessionStore,
    session_id: String,
    run_id: String,
    channel_id: String,
    source: String,
    sender: mpsc::UnboundedSender<SteeringInstruction>,
    submission_lock: Arc<std::sync::Mutex<()>>,
}

pub struct ExactRunSteeringReceiver {
    store: SessionStore,
    session_id: String,
    run_id: String,
    channel_id: String,
    receiver: mpsc::UnboundedReceiver<SteeringInstruction>,
}

impl std::fmt::Debug for ExactRunSteeringHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExactRunSteeringHandle")
            .field("session_id", &"<redacted>")
            .field("run_id", &"<redacted>")
            .field("channel_id", &"<redacted>")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ExactRunSteeringReceiver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExactRunSteeringReceiver")
            .field("session_id", &"<redacted>")
            .field("run_id", &"<redacted>")
            .field("channel_id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

pub fn exact_run_steering_channel(
    store: SessionStore,
    session_id: impl Into<String>,
    run_id: impl Into<String>,
    source: &str,
) -> Result<(ExactRunSteeringHandle, ExactRunSteeringReceiver), String> {
    let session_id = session_id.into();
    crate::session::validate_session_id(&session_id).map_err(|error| error.to_string())?;
    let run_id = resolve_agent_run_id(Some(run_id.into()))?;
    if !matches!(source, "plain" | "tui") {
        return Err("steering source must be plain or tui".to_string());
    }
    let channel_id = uuid::Uuid::new_v4().simple().to_string();
    let (sender, receiver) = mpsc::unbounded_channel();
    Ok((
        ExactRunSteeringHandle {
            store: store.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            channel_id: channel_id.clone(),
            source: source.to_string(),
            sender,
            submission_lock: Arc::new(std::sync::Mutex::new(())),
        },
        ExactRunSteeringReceiver {
            store,
            session_id,
            run_id,
            channel_id,
            receiver,
        },
    ))
}

impl ExactRunSteeringHandle {
    pub fn submit(&self, text: &str) -> Result<usize, String> {
        let text = normalize_steering_input(text)?;
        let _submission_guard = self
            .submission_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run_id = self.run_id.clone();
        let source = self.source.clone();
        let sequence =
            self.store
                .update_session(&self.session_id, |session| {
                    let start_index = session
                        .events
                        .iter()
                        .rposition(|event| {
                            event.kind == "run_started" && event.details["run_id"] == run_id
                        })
                        .ok_or_else(|| {
                            crate::session::SessionError::InvalidMutation(
                                "steering run is not active".to_string(),
                            )
                        })?;
                    if session.events.iter().skip(start_index + 1).any(|event| {
                        event.kind == "run_started" && event.details["run_id"] != run_id
                    }) {
                        return Err(crate::session::SessionError::InvalidMutation(
                            "steering handle is stale for the active run".to_string(),
                        ));
                    }
                    let active_events = &session.events[start_index + 1..];
                    let bound_channel = active_events.iter().rev().find_map(|event| {
                        (event.kind == "steering_channel_bound"
                            && event.details["run_id"] == run_id)
                            .then(|| event.details["channel_id"].as_str())
                            .flatten()
                    });
                    if bound_channel != Some(self.channel_id.as_str()) {
                        return Err(crate::session::SessionError::InvalidMutation(
                            "steering handle is not installed on the active run".to_string(),
                        ));
                    }
                    let current_state = active_events.iter().rev().find_map(|event| {
                        (event.kind == "state_transition")
                            .then(|| event.details["to"].as_str())
                            .flatten()
                    });
                    if active_events.iter().any(|event| {
                        event.kind == "run_terminal" && event.details["run_id"] == run_id
                    }) || matches!(
                        current_state,
                        Some(state)
                            if state == AgentState::Reconciliation.as_str()
                                || state == AgentState::Done.as_str()
                    ) {
                        return Err(crate::session::SessionError::InvalidMutation(
                            "steering run is reconciling or terminal".to_string(),
                        ));
                    }
                    if active_events.iter().rev().find_map(|event| {
                        (event.kind == "steering_admission" && event.details["run_id"] == run_id)
                            .then(|| event.details["open"].as_bool())
                            .flatten()
                    }) != Some(true)
                    {
                        return Err(crate::session::SessionError::InvalidMutation(
                            "steering is not accepted at the current run boundary".to_string(),
                        ));
                    }
                    let accepted = active_events
                        .iter()
                        .filter(|event| {
                            event.kind == "steering_input"
                                && event.details["run_id"] == run_id
                                && event.details["channel_id"] == self.channel_id
                        })
                        .collect::<Vec<_>>();
                    if accepted.len() >= MAX_STEERING_INPUTS_PER_RUN {
                        return Err(crate::session::SessionError::InvalidMutation(
                            "steering input count limit reached for this run".to_string(),
                        ));
                    }
                    let mut total_bytes = 0usize;
                    for (index, event) in accepted.iter().enumerate() {
                        if event.details["sequence"].as_u64() != Some((index + 1) as u64) {
                            return Err(crate::session::SessionError::InvalidMutation(
                                "persisted steering sequence is invalid".to_string(),
                            ));
                        }
                        let prior = event.details["text"].as_str().ok_or_else(|| {
                            crate::session::SessionError::InvalidMutation(
                                "persisted steering input is invalid".to_string(),
                            )
                        })?;
                        total_bytes = total_bytes.checked_add(prior.len()).ok_or_else(|| {
                            crate::session::SessionError::InvalidMutation(
                                "persisted steering input size overflowed".to_string(),
                            )
                        })?;
                    }
                    if total_bytes.saturating_add(text.len()) > MAX_STEERING_TOTAL_BYTES_PER_RUN {
                        return Err(crate::session::SessionError::InvalidMutation(
                            "steering input byte limit reached for this run".to_string(),
                        ));
                    }
                    let sequence = accepted.len() + 1;
                    append_session_event(
                        session,
                        "steering_input",
                        json!({
                            "run_id": run_id,
                            "channel_id": self.channel_id,
                            "sequence": sequence,
                            "source": source,
                            "text": text,
                        }),
                    );
                    Ok(sequence)
                })
                .map_err(|error| error.to_string())?;

        if self
            .sender
            .send(SteeringInstruction {
                sequence,
                text: text.clone(),
            })
            .is_err()
        {
            record_steering_delivery_failure(
                &self.store,
                &self.session_id,
                &self.run_id,
                &[sequence],
                "receiver_closed",
            )?;
            return Err(
                "steering was persisted but the active run stopped before intake".to_string(),
            );
        }
        Ok(sequence)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) fn fail_unaccounted(&self, reason: &str) -> Result<(), String> {
        let _submission_guard = self
            .submission_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = self
            .store
            .load_result(&self.session_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "steering session is unavailable".to_string())?;
        let sequences = session
            .events
            .iter()
            .filter(|event| {
                event.kind == "steering_input"
                    && event.details["run_id"] == self.run_id
                    && event.details["channel_id"] == self.channel_id
            })
            .filter_map(|event| event.details["sequence"].as_u64())
            .map(|sequence| sequence as usize)
            .filter(|sequence| !steering_sequence_accounted(&session, &self.run_id, *sequence))
            .collect::<Vec<_>>();
        record_steering_delivery_failure(
            &self.store,
            &self.session_id,
            &self.run_id,
            &sequences,
            reason,
        )
    }
}

impl Drop for ExactRunSteeringReceiver {
    fn drop(&mut self) {
        self.receiver.close();
        let sequences = std::iter::from_fn(|| self.receiver.try_recv().ok())
            .map(|instruction| instruction.sequence)
            .collect::<Vec<_>>();
        if !sequences.is_empty() {
            let _ = record_steering_delivery_failure(
                &self.store,
                &self.session_id,
                &self.run_id,
                &sequences,
                "run_ended_before_intake",
            );
        }
    }
}

fn normalize_steering_input(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("steering input cannot be empty".to_string());
    }
    if text.len() > MAX_STEERING_INPUT_BYTES {
        return Err(format!(
            "steering input exceeds the {MAX_STEERING_INPUT_BYTES}-byte limit"
        ));
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err("steering input contains unsupported control characters".to_string());
    }
    Ok(text.to_string())
}

fn record_steering_delivery_failure(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    sequences: &[usize],
    reason: &str,
) -> Result<(), String> {
    if sequences.is_empty() {
        return Ok(());
    }
    store
        .update_session(session_id, |session| {
            for sequence in sequences {
                if steering_sequence_accounted(session, run_id, *sequence) {
                    continue;
                }
                append_session_event(
                    session,
                    "steering_delivery_failed",
                    json!({"run_id": run_id, "sequence": sequence, "reason": reason}),
                );
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
}

fn steering_sequence_accounted(session: &Session, run_id: &str, sequence: usize) -> bool {
    session.events.iter().any(|event| {
        matches!(
            event.kind.as_str(),
            "steering_intake" | "steering_delivery_failed"
        ) && event.details["run_id"] == run_id
            && event.details["sequence"] == sequence
    })
}

pub(crate) fn bind_exact_run_steering_receiver(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    receiver: &ExactRunSteeringReceiver,
) -> Result<(), String> {
    receiver.verify_binding(store, session_id, run_id)?;
    store
        .update_session(session_id, |session| {
            let start_index = session
                .events
                .iter()
                .rposition(|event| event.kind == "run_started" && event.details["run_id"] == run_id)
                .ok_or_else(|| {
                    crate::session::SessionError::InvalidMutation(
                        "cannot install steering before the exact run starts".to_string(),
                    )
                })?;
            let active_events = &session.events[start_index + 1..];
            if active_events
                .iter()
                .any(|event| event.kind == "run_started" && event.details["run_id"] != run_id)
            {
                return Err(crate::session::SessionError::InvalidMutation(
                    "cannot install a stale steering receiver".to_string(),
                ));
            }
            if active_events
                .iter()
                .any(|event| event.kind == "run_terminal" && event.details["run_id"] == run_id)
            {
                return Err(crate::session::SessionError::InvalidMutation(
                    "cannot install steering on a terminal run".to_string(),
                ));
            }
            if active_events.iter().any(|event| {
                event.kind == "steering_channel_bound" && event.details["run_id"] == run_id
            }) {
                return Err(crate::session::SessionError::InvalidMutation(
                    "the exact run already has an installed steering channel".to_string(),
                ));
            }
            append_session_event(
                session,
                "steering_channel_bound",
                json!({"run_id": run_id, "channel_id": receiver.channel_id}),
            );
            append_session_event(
                session,
                "steering_admission",
                json!({"run_id": run_id, "open": true, "phase": "run_started"}),
            );
            Ok(())
        })
        .map_err(|error| error.to_string())
}

fn set_steering_admission(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    phase: &str,
    open: bool,
) -> Result<bool, String> {
    store
        .update_session(session_id, |session| {
            let has_channel = session.events.iter().any(|event| {
                event.kind == "steering_channel_bound" && event.details["run_id"] == run_id
            });
            if !has_channel {
                return Ok(true);
            }
            if !open {
                let pending_input = session.events.iter().any(|event| {
                    event.kind == "steering_input"
                        && event.details["run_id"] == run_id
                        && event.details["sequence"].as_u64().is_some_and(|sequence| {
                            !steering_sequence_accounted(session, run_id, sequence as usize)
                        })
                });
                if pending_input {
                    return Ok(false);
                }
            }
            append_session_event(
                session,
                "steering_admission",
                json!({"run_id": run_id, "open": open, "phase": phase}),
            );
            Ok(true)
        })
        .map_err(|error| error.to_string())
}

fn close_steering_admission(
    enabled: bool,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    phase: &str,
) -> Result<bool, String> {
    if !enabled {
        return Ok(true);
    }
    set_steering_admission(store, session_id, run_id, phase, false)
}

fn open_steering_admission(
    enabled: bool,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    phase: &str,
) -> Result<(), String> {
    if !enabled {
        return Ok(());
    }
    set_steering_admission(store, session_id, run_id, phase, true).map(|_| ())
}

fn record_tool_started_and_open_steering(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    request: &ToolCallRequest,
    allow_steering: bool,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "tool_started",
                json!({
                    "invocation_id": request.invocation_id,
                    "tool_name": request.name,
                }),
            );
            if allow_steering
                && session.events.iter().any(|event| {
                    event.kind == "steering_channel_bound" && event.details["run_id"] == run_id
                })
            {
                append_session_event(
                    session,
                    "steering_admission",
                    json!({"run_id": run_id, "open": true, "phase": "tool_started"}),
                );
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
}

impl ExactRunSteeringReceiver {
    fn verify_binding(
        &self,
        store: &SessionStore,
        session_id: &str,
        run_id: &str,
    ) -> Result<(), String> {
        if self.session_id != session_id
            || self.run_id != run_id
            || self.store.sessions_dir() != store.sessions_dir()
        {
            return Err("steering receiver is not bound to the exact active run".to_string());
        }
        Ok(())
    }

    fn drain(&mut self) -> Vec<SteeringInstruction> {
        let mut instructions =
            std::iter::from_fn(|| self.receiver.try_recv().ok()).collect::<Vec<_>>();
        instructions.sort_unstable_by_key(|instruction| instruction.sequence);
        instructions
    }
}

fn record_steering_intake(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    channel_id: &str,
    instructions: &[SteeringInstruction],
) -> Result<(), String> {
    if instructions.is_empty() {
        return Ok(());
    }
    store
        .update_session(session_id, |session| {
            let bound_channel = session.events.iter().rev().find_map(|event| {
                (event.kind == "steering_channel_bound" && event.details["run_id"] == run_id)
                    .then(|| event.details["channel_id"].as_str())
                    .flatten()
            });
            if bound_channel != Some(channel_id) {
                return Err(crate::session::SessionError::InvalidMutation(
                    "steering intake did not originate from the installed exact-run channel"
                        .to_string(),
                ));
            }
            for instruction in instructions {
                let persisted = session.events.iter().any(|event| {
                    event.kind == "steering_input"
                        && event.details["run_id"] == run_id
                        && event.details["channel_id"] == channel_id
                        && event.details["sequence"] == instruction.sequence
                        && event.details["text"] == instruction.text
                });
                if !persisted {
                    return Err(crate::session::SessionError::InvalidMutation(
                        "steering delivery has no matching persisted exact-run input".to_string(),
                    ));
                }
                if steering_sequence_accounted(session, run_id, instruction.sequence) {
                    return Err(crate::session::SessionError::InvalidMutation(
                        "steering input was delivered more than once".to_string(),
                    ));
                }
                if (1..instruction.sequence)
                    .any(|prior| !steering_sequence_accounted(session, run_id, prior))
                {
                    return Err(crate::session::SessionError::InvalidMutation(
                        "steering intake would skip an earlier exact-run input".to_string(),
                    ));
                }
                append_session_event(
                    session,
                    "steering_intake",
                    json!({
                        "run_id": run_id,
                        "channel_id": channel_id,
                        "sequence": instruction.sequence,
                    }),
                );
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
}

fn append_steering_context(
    context: &mut crate::context::RuntimeContextSections,
    instructions: &[SteeringInstruction],
) {
    if instructions.is_empty() {
        return;
    }
    context
        .task
        .push_str("\n\nExact-run steering accepted after the original submission:");
    for instruction in instructions {
        let encoded = serde_json::to_string(&instruction.text)
            .expect("validated steering input must serialize as JSON");
        context
            .task
            .push_str(&format!("\n{}. {encoded}", instruction.sequence));
    }
}

fn supersede_unapproved_plan_for_steering(
    store: &SessionStore,
    session_id: &str,
    expected_plan_id: Option<&str>,
    instructions: &[SteeringInstruction],
) -> Result<bool, String> {
    store
        .update_session(session_id, |session| {
            let Some(plan) = session.plan.as_ref() else {
                return Ok(false);
            };
            if plan.approved || expected_plan_id != Some(plan.id.as_str()) {
                return Ok(false);
            }
            let prior = session.plan.take().expect("unapproved plan was present");
            append_session_event(
                session,
                "plan_superseded_by_steering",
                json!({
                    "previous_plan_id": prior.id,
                    "step_count": prior.steps.len(),
                    "first_sequence": instructions.first().map(|instruction| instruction.sequence),
                    "last_sequence": instructions.last().map(|instruction| instruction.sequence),
                }),
            );
            Ok(true)
        })
        .map_err(|error| error.to_string())
}

fn safe_question_arguments(arguments: &Value, sensitive_values: &[String]) -> Value {
    let question = arguments
        .get("question")
        .and_then(Value::as_str)
        .map(|question| {
            crate::interactive::bounded_public_text(
                question,
                sensitive_values,
                MAX_QUESTION_BYTES,
                false,
            )
        });
    let options = arguments
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(Value::as_str)
                .take(20)
                .map(|option| {
                    crate::interactive::bounded_public_text(
                        option,
                        sensitive_values,
                        MAX_QUESTION_OPTION_BYTES,
                        false,
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({"question": question, "options": options})
}

fn safe_question_execution_arguments(
    arguments: &Value,
    answer: &Result<String, String>,
    sensitive_values: &[String],
) -> Value {
    let mut arguments = safe_question_arguments(arguments, sensitive_values);
    let object = arguments
        .as_object_mut()
        .expect("safe question arguments are always an object");
    match answer {
        Ok(answer) => {
            object.insert(
                "answer".to_string(),
                json!(crate::interactive::bounded_public_text(
                    answer,
                    sensitive_values,
                    MAX_QUESTION_BYTES,
                    false,
                )),
            );
        }
        Err(error) => {
            object.insert(
                "answer_error".to_string(),
                json!(crate::interactive::bounded_public_text(
                    error,
                    sensitive_values,
                    MAX_QUESTION_BYTES,
                    false,
                )),
            );
        }
    }
    arguments
}

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
    pub run_id: Option<String>,
    pub steering: Option<ExactRunSteeringReceiver>,
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
            run_id: None,
            steering: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct AgentRunSummary {
    pub session_id: String,
    #[serde(default)]
    pub run_id: String,
    pub steps_taken: u32,
    pub last_message: Option<String>,
    pub tool_call_count: usize,
    pub final_state: AgentState,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<LlmError>,
    pub bound_reached: bool,
    pub trace: Vec<String>,
}

impl AgentRunSummary {
    pub fn is_failure(&self) -> bool {
        is_agent_failure_outcome(&self.outcome)
    }

    pub fn user_failure_report(&self) -> Option<String> {
        self.failure.as_ref().map_or_else(
            || {
                is_llm_failure_outcome(&self.outcome).then(|| {
                    LlmError::new(
                        LlmErrorClass::ProviderRejected,
                        LlmErrorPhase::TerminalValidation,
                        crate::llm::RetryDisposition::NotAttempted,
                        crate::llm::LlmErrorMetadata::new("unknown", "legacy", None, None, &[]),
                        "legacy run did not persist structured LLM failure evidence",
                    )
                    .user_report(Some(&self.session_id))
                })
            },
            |failure| Some(failure.user_report(Some(&self.session_id))),
        )
    }
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
    runtime.nib_cfg.validate_public_session_id(session_id)?;
    let run_lease = runtime
        .session_store
        .try_acquire_run_lease(session_id)
        .map_err(|error| error.to_string())?;
    run_agent_loop_with_runtime(runtime, session_id, goal, cfg, run_lease).await
}

/// Run against the profile and session directory captured when an interactive UI
/// started. Reloaded configuration may tighten or remove that profile, but it may
/// never redirect the turn into another profile with a coincident session ID.
pub async fn run_agent_loop_for_profile(
    project_root: PathBuf,
    profile_id: &str,
    sessions_dir: &Path,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    crate::config::load_nib_config_full(&project_root)
        .map_err(|error| error.to_string())?
        .validate_public_session_id(session_id)?;
    let store = SessionStore::at_dir(sessions_dir.to_path_buf());
    let run_lease = store
        .try_acquire_run_lease(session_id)
        .map_err(|error| error.to_string())?;
    run_agent_loop_for_profile_with_lease(
        project_root,
        profile_id,
        sessions_dir,
        session_id,
        goal,
        cfg,
        run_lease,
    )
    .await
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
    runtime.nib_cfg.validate_public_session_id(session_id)?;
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
    run_agent_loop_with_runtime_and_recovery(
        runtime,
        session_id,
        goal,
        cfg,
        run_lease,
        reconcile_interrupted_provider_continuation,
    )
    .await
}

async fn run_agent_loop_with_runtime_and_recovery(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    mut cfg: AgentLoopConfig,
    run_lease: SessionRunLease,
    recover_interrupted_continuation: fn(&SessionStore, &str) -> Result<bool, String>,
) -> Result<AgentRunSummary, String> {
    let sessions_dir = runtime.session_store.sessions_dir().to_path_buf();
    run_lease
        .verify_for(session_id, &sessions_dir)
        .map_err(|error| error.to_string())?;
    let run_id = resolve_agent_run_id(cfg.run_id.clone())?;
    if runtime
        .session_store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        runtime
            .session_store
            .try_create_session_with_id(session_id.to_string())
            .map_err(|error| error.to_string())?;
    }
    runtime
        .session_store
        .update_session(session_id, |session| {
            if session.events.iter().any(|event| {
                event.kind == "run_started"
                    && event.details.get("run_id").and_then(Value::as_str) == Some(&run_id)
            }) {
                return Err(crate::session::SessionError::InvalidMutation(
                    "duplicate or replayed run_id".to_string(),
                ));
            }
            append_session_event(session, "run_started", json!({"run_id": run_id.clone()}));
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    let unsupported_steering_mode = cfg.steering.is_some()
        && !matches!(cfg.mode.as_str(), "execute" | "plan")
        && cfg.steering.take().is_some();
    let steering_binding = if unsupported_steering_mode {
        Err(format!(
            "exact-run steering is not supported for agent mode: {}",
            cfg.mode
        ))
    } else {
        cfg.steering.as_ref().map_or(Ok(()), |steering| {
            bind_exact_run_steering_receiver(&runtime.session_store, session_id, &run_id, steering)
        })
    };
    // Explicit compression is a local maintenance operation, not a chat turn. It
    // must not reconcile an older provider continuation because that recovery may
    // append an assistant boundary. The next ordinary run remains responsible for
    // that pre-existing continuation.
    let recovery_result = match steering_binding {
        Err(error) => Err(error),
        Ok(()) if cfg.mode == "compact" => Ok(false),
        Ok(()) => recover_interrupted_continuation(&runtime.session_store, session_id),
    };
    cfg.run_id = Some(run_id.clone());
    let explicit_compaction = cfg.mode == "compact";
    let cancellation = cfg.cancellation.clone();
    let stream_tx = cfg.stream_tx.clone();
    let cancellation_store = runtime.session_store.clone();
    let run_result = match recovery_result {
        Err(error) => Err(error),
        Ok(_) => {
            if let Some(cancellation) = cancellation {
                if cancellation.is_cancelled() {
                    reconcile_cancelled_run(&cancellation_store, session_id, &stream_tx).await
                } else {
                    let mut running = Box::pin(run_agent_operation(runtime, session_id, goal, cfg));
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            if explicit_compaction
                                && explicit_compaction_terminal_committed(
                                    &cancellation_store,
                                    session_id,
                                    &run_id,
                                )?
                            {
                                running.await
                            } else {
                                drop(running);
                                reconcile_cancelled_run(&cancellation_store, session_id, &stream_tx).await
                            }
                        },
                        result = &mut running => result,
                    }
                }
            } else {
                Box::pin(run_agent_operation(runtime, session_id, goal, cfg)).await
            }
        }
    };
    let result = match (run_result, run_lease.verify_for(session_id, &sessions_dir)) {
        (Ok(mut summary), Ok(())) => {
            summary.run_id = run_id.clone();
            let outcome = summary.outcome.clone();
            runtime_terminal_event(&cancellation_store, session_id, &run_id, &outcome)?;
            Ok(summary)
        }
        (Err(error), Ok(())) => {
            runtime_terminal_event(&cancellation_store, session_id, &run_id, "local_error")?;
            Err(error)
        }
        (Ok(_), Err(error)) => Err(error.to_string()),
        (Err(run_error), Err(lease_error)) => Err(format!(
            "{run_error}; active run lease verification failed: {lease_error}"
        )),
    };
    if let Ok(summary) = &result {
        if let Some(failure) = &summary.failure {
            emit_nonblocking(
                &stream_tx,
                StreamEvent::Failure {
                    failure: failure.clone(),
                    session_id: Some(summary.session_id.clone()),
                },
            );
        }
        emit_nonblocking(&stream_tx, StreamEvent::End(summary.outcome.clone()));
    }
    result
}

fn explicit_compaction_terminal_committed(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
) -> Result<bool, String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to inspect explicit compression terminal: {error}"))?
        .ok_or_else(|| "session disappeared while inspecting explicit compression".to_string())?;
    Ok(session.events.iter().any(|event| {
        event.kind == "compression_request_terminal" && event.details["run_id"] == run_id
    }))
}

async fn run_agent_operation(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    if cfg.mode == "compact" {
        Box::pin(run_explicit_compaction(runtime, session_id, cfg)).await
    } else {
        Box::pin(run_agent_loop_inner(runtime, session_id, goal, cfg)).await
    }
}

async fn run_explicit_compaction(
    runtime: AgentLoopRuntime,
    session_id: &str,
    cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    let AgentLoopRuntime {
        nib_cfg,
        session_store: store,
        ..
    } = runtime;
    let run_id = cfg
        .run_id
        .as_deref()
        .ok_or_else(|| "agent run identity was not initialized".to_string())?;
    store
        .record_event(
            session_id,
            "compression_requested",
            json!({"run_id": run_id, "source": "interactive"}),
        )
        .map_err(|error| error.to_string())?;
    emit(
        &cfg.stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Compression.as_str().to_string(),
        },
    )
    .await;

    if !nib_cfg.compression.enabled {
        return finish_explicit_compaction(
            &store,
            session_id,
            run_id,
            &cfg.stream_tx,
            "compression_disabled",
            0,
        );
    }
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session for explicit compression: {error}"))?
        .ok_or_else(|| "session disappeared before explicit compression".to_string())?;
    if session.summary_index >= session.messages.len() {
        return finish_explicit_compaction(
            &store,
            session_id,
            run_id,
            &cfg.stream_tx,
            "context_unchanged",
            0,
        );
    }

    let sensitive_values = nib_cfg.sensitive_values();
    let llm: Arc<dyn LlmClient> = match crate::llm::factory::create_client_with_sensitive_values(
        &nib_cfg.llm,
        cfg.provider.as_deref(),
        &sensitive_values,
    ) {
        Ok(llm) => llm,
        Err(error) => {
            let failure = llm_configuration_failure(
                &nib_cfg,
                cfg.provider.as_deref(),
                &error,
                &sensitive_values,
            );
            return reconcile_explicit_compression_failure(
                &store,
                session_id,
                &cfg.stream_tx,
                failure,
                "configuration_failed",
            );
        }
    };
    match crate::context::compression::explicitly_compress_session(
        &store, session_id, &llm, &nib_cfg,
    )
    .await
    {
        Ok(Some(report)) => {
            emit_nonblocking(
                &cfg.stream_tx,
                StreamEvent::Compression {
                    before_tokens: report.before_tokens,
                    after_tokens: report.after_tokens,
                    summarized_through: report.summarized_through,
                },
            );
            finish_explicit_compaction(
                &store,
                session_id,
                run_id,
                &cfg.stream_tx,
                "context_compacted",
                1,
            )
        }
        Ok(None) => finish_explicit_compaction(
            &store,
            session_id,
            run_id,
            &cfg.stream_tx,
            "context_unchanged",
            0,
        ),
        Err(error) => {
            let failure = redact_provider_failure(&nib_cfg, error);
            reconcile_explicit_compression_failure(
                &store,
                session_id,
                &cfg.stream_tx,
                failure,
                "compression_failed",
            )
        }
    }
}

fn finish_explicit_compaction(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
    outcome: &str,
    steps_taken: u32,
) -> Result<AgentRunSummary, String> {
    let tool_call_count = store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "compression_request_terminal",
                json!({"run_id": run_id, "outcome": outcome}),
            );
            Ok(session.tool_calls.len())
        })
        .map_err(|error| format!("failed to reconcile explicit compression: {error}"))?;
    emit_nonblocking(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: outcome.to_string(),
        },
    );
    emit_nonblocking(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    );
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        steps_taken,
        last_message: None,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: outcome.to_string(),
        failure: None,
        bound_reached: false,
        trace: vec![
            AgentState::Compression.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

fn reconcile_explicit_compression_failure(
    store: &SessionStore,
    session_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
    failure: LlmError,
    outcome: &str,
) -> Result<AgentRunSummary, String> {
    let persisted_failure = failure.clone();
    let tool_call_count = store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "reconciliation",
                json!({
                    "outcome": outcome,
                    "continue": false,
                    "failure": persisted_failure,
                }),
            );
            Ok(session.tool_calls.len())
        })
        .map_err(|error| format!("failed to reconcile explicit compression failure: {error}"))?;
    emit_nonblocking(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: outcome.to_string(),
        },
    );
    emit_nonblocking(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    );
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: String::new(),
        steps_taken: 0,
        last_message: None,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: outcome.to_string(),
        failure: Some(failure),
        bound_reached: false,
        trace: vec![
            AgentState::Compression.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

fn resolve_agent_run_id(run_id: Option<String>) -> Result<String, String> {
    let run_id = run_id.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    if run_id.len() != 32
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("agent run_id must be exactly 32 lowercase hexadecimal characters".to_string());
    }
    Ok(run_id)
}

fn request_scope_for_run(session_id: &str, run_id: &str) -> Result<LlmRequestScope, String> {
    LlmRequestScope::new(session_id, run_id)
}

async fn run_agent_loop_inner(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    mut cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    let AgentLoopRuntime {
        nib_cfg,
        profile,
        session_store: store,
    } = runtime;
    if store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session {session_id}: {error}"))?
        .is_none()
    {
        store
            .try_create_session_with_id(session_id.to_string())
            .map_err(|error| format!("failed to create session {session_id}: {error}"))?;
    }
    let normalized_goal = normalize_plan_goal(goal);
    if normalized_goal.is_empty() {
        return Err("agent goal cannot be empty".to_string());
    }
    if !matches!(cfg.mode.as_str(), "execute" | "plan") {
        return Err(format!("unsupported agent mode: {}", cfg.mode));
    }
    let run_id = cfg
        .run_id
        .clone()
        .ok_or_else(|| "agent run identity was not initialized".to_string())?;
    let request_scope = request_scope_for_run(session_id, &run_id)?;
    if let Some(steering) = cfg.steering.as_ref() {
        steering.verify_binding(&store, session_id, &run_id)?;
    }
    let steering_enabled = cfg.steering.is_some();
    invalidate_nonresumable_plan(&store, session_id, &normalized_goal)?;

    let project_root = profile.root_path().to_path_buf();
    let max_turns = if cfg.max_steps == 0 {
        nib_cfg.agent.max_turns.max(1)
    } else {
        cfg.max_steps
    };
    let max_transitions = max_turns.saturating_mul(10).saturating_add(10);
    let sensitive_values = nib_cfg.sensitive_values();
    let public_output_sensitive_values = nib_cfg.public_session_sensitive_values();
    let llm: Arc<dyn LlmClient> = match crate::llm::factory::create_client_with_sensitive_values(
        &nib_cfg.llm,
        cfg.provider.as_deref(),
        &sensitive_values,
    ) {
        Ok(llm) => llm,
        Err(error) => {
            let failure = llm_configuration_failure(
                &nib_cfg,
                cfg.provider.as_deref(),
                &error,
                &sensitive_values,
            );
            return reconcile_preflight_llm_failure(
                &store,
                session_id,
                &normalized_goal,
                failure,
                &cfg.stream_tx,
            )
            .await;
        }
    };
    let active_skills = select_profile_skills(&project_root, &nib_cfg, &profile, goal)?;
    let policy_rules = skill_policy_rules(&active_skills);
    let after_tool_hooks = skill_after_tool_hooks(&active_skills);
    let mcp_manager = if nib_cfg.mcp.client_enabled && !nib_cfg.mcp.servers.is_empty() {
        Some(Arc::new(
            crate::integrations::mcp::McpManager::new(
                &nib_cfg.mcp.servers,
                &public_output_sensitive_values,
            )
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
        .with_sensitive_values(public_output_sensitive_values.clone())
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

    prepare_user_turn(&store, session_id, goal, profile.root_path())?;
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
    if let Ok(Some(session)) = store.load_result(session_id) {
        let attachments = session
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.attachments.as_slice())
            .unwrap_or(&[]);
        context_sections.attachments = attachment_context_sections(&project_root, attachments);
    }
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
    let mut reconciliation_failure: Option<LlmError> = None;
    let mut pending_question: Option<ToolCallRequest> = None;
    let mut pending_observations: Vec<Value> = Vec::new();
    let mut pending_batch_success = true;
    let mut outcome = "running".to_string();
    let mut bound_reached = false;
    let mut active_plan_id: Option<String> = None;
    let mut provider_continuation: Option<ProviderContinuation> = None;

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
        let steering = cfg
            .steering
            .as_mut()
            .map(ExactRunSteeringReceiver::drain)
            .unwrap_or_default();
        if !steering.is_empty() {
            let can_replan_at_reconciliation = state == AgentState::Reconciliation
                && cfg.mode == "plan"
                && reconciliation_failure.is_none()
                && !bound_reached
                && llm_turns < max_turns
                && reconciliation_reason.as_deref() == Some("plan_ready");
            let cannot_apply = state == AgentState::WaitingForUserInput
                || (state == AgentState::Reconciliation && !can_replan_at_reconciliation);
            if cannot_apply {
                record_steering_delivery_failure(
                    &store,
                    session_id,
                    &run_id,
                    &steering
                        .iter()
                        .map(|instruction| instruction.sequence)
                        .collect::<Vec<_>>(),
                    "run_reconciled_before_safe_boundary",
                )?;
            } else {
                let channel_id = cfg
                    .steering
                    .as_ref()
                    .map(|receiver| receiver.channel_id.as_str())
                    .ok_or_else(|| "steering intake has no installed receiver".to_string())?;
                record_steering_intake(&store, session_id, &run_id, channel_id, &steering)?;
                append_steering_context(&mut context_sections, &steering);
                if provider_continuation.take().is_some() {
                    record_provider_continuation_lifecycle(
                        &store,
                        session_id,
                        "provider_continuation_abandoned_by_steering",
                        &run_id,
                    )?;
                }

                if state == AgentState::PlanApproval
                    && supersede_unapproved_plan_for_steering(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                        &steering,
                    )?
                {
                    active_plan_id = None;
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Planning,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }

                if state == AgentState::InspectLlm {
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

                if state == AgentState::UpdateMemory {
                    response_content = None;
                    tool_calls.clear();
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

                if matches!(state, AgentState::UserApproval | AgentState::ToolExecute) {
                    response_content = None;
                    tool_calls.clear();
                    store
                        .record_event(
                            session_id,
                            "tool_proposal_superseded_by_steering",
                            json!({
                                "first_sequence": steering.first().map(|instruction| instruction.sequence),
                                "last_sequence": steering.last().map(|instruction| instruction.sequence),
                            }),
                        )
                        .map_err(|error| error.to_string())?;
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

                if state == AgentState::Reconciliation {
                    reconciliation_reason = None;
                    response_content = None;
                    tool_calls.clear();
                    if !supersede_unapproved_plan_for_steering(
                        &store,
                        session_id,
                        active_plan_id.as_deref(),
                        &steering,
                    )? {
                        return Err(
                            "plan steering reached reconciliation without its bound unapproved plan"
                                .to_string(),
                        );
                    }
                    active_plan_id = None;
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        AgentState::Planning,
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    continue;
                }
            }
        }

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
                    if llm_turns.saturating_add(1) >= max_turns
                        && !close_steering_admission(
                            steering_enabled,
                            &store,
                            session_id,
                            &run_id,
                            "final_planning_request",
                        )?
                    {
                        continue;
                    }
                    llm_turns += 1;
                    let planning_session = store
                        .load_result(session_id)
                        .map_err(|error| {
                            format!("failed to load session context for planning: {error}")
                        })?
                        .ok_or_else(|| "session disappeared before planning".to_string())?;
                    match crate::agent::planner::generate_plan_with_context_events_bounded_scoped(
                        &llm,
                        goal,
                        &context_sections,
                        Some(&planning_session),
                        cfg.stream_tx.as_ref(),
                        nib_cfg.llm.context_length,
                        Some(request_scope.clone()),
                    )
                    .await
                    {
                        Ok(mut plan) => {
                            sanitize_provider_plan(&mut plan, &public_output_sensitive_values);
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
                            reconciliation_failure = Some(redact_provider_failure(
                                &nib_cfg,
                                error.with_phase(LlmErrorPhase::Planning),
                            ));
                            reconciliation_reason = Some("planning_failed".to_string());
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
                    if llm_turns < max_turns {
                        open_steering_admission(
                            steering_enabled,
                            &store,
                            session_id,
                            &run_id,
                            "approved_plan_build_context",
                        )?;
                    }
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
                    if !close_steering_admission(
                        steering_enabled,
                        &store,
                        session_id,
                        &run_id,
                        "plan_approval",
                    )? {
                        continue;
                    }
                    let arguments = json!({
                        "plan_id": plan.id,
                        "goal": plan.goal,
                        "steps": plan.steps.iter().map(|step| &step.description).collect::<Vec<_>>(),
                    });
                    emit(
                        &cfg.stream_tx,
                        StreamEvent::ApprovalRequired {
                            tool_name: "approve_plan".to_string(),
                        },
                    )
                    .await;
                    store
                        .record_event(
                            session_id,
                            "approval_required",
                            json!({
                                "kind": "plan",
                                "plan_id": plan.id.clone(),
                                "step_count": plan.steps.len(),
                            }),
                        )
                        .map_err(|error| error.to_string())?;

                    let approved = if cfg.auto_approve {
                        true
                    } else {
                        let handler: Arc<dyn ApprovalHandler> = cfg
                            .approval_handler
                            .clone()
                            .unwrap_or_else(|| Arc::new(StdinApprovalHandler));
                        let approval_call = ToolCall {
                            invocation_id: crate::tools::ToolInvocationId::new(),
                            tool_name: "approve_plan".to_string(),
                            arguments: arguments.clone(),
                            session_id: Some(session_id.to_string()),
                            project_root: Some(project_root.clone()),
                        };
                        let context = executor.approval_context(
                            &approval_call,
                            PermissionLevel::Plan,
                            ToolRisk::RequiresApproval,
                            &project_root,
                            &executor.execution_config,
                            false,
                            Some(session_id),
                            "approve the persisted structured plan for later gated execution",
                        );
                        handler
                            .handle_approval_with_context(
                                &approval_call,
                                PermissionLevel::Plan,
                                &context,
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
                        if llm_turns < max_turns {
                            open_steering_admission(
                                steering_enabled,
                                &store,
                                session_id,
                                &run_id,
                                "approved_plan_build_context",
                            )?;
                        }
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
                } else if provider_continuation.is_some() {
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
                } else {
                    // Automatic compression is optional context maintenance. Keep the
                    // final remaining model turn for the task request so an accepted
                    // exact-run steer can never be consumed by a summary request that
                    // does not contain steering context.
                    let compression = if llm_turns.saturating_add(1) >= max_turns {
                        None
                    } else {
                        match crate::context::compression::maybe_compress_session(
                            &store, session_id, &llm, &nib_cfg,
                        )
                        .await
                        {
                            Ok(compression) => compression,
                            Err(error) => {
                                reconciliation_failure =
                                    Some(redact_provider_failure(&nib_cfg, error));
                                reconciliation_reason = Some("compression_failed".to_string());
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
                        }
                    };
                    if let Some(report) = compression {
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
                if llm_turns.saturating_add(1) >= max_turns
                    && !close_steering_admission(
                        steering_enabled,
                        &store,
                        session_id,
                        &run_id,
                        "final_provider_request",
                    )?
                {
                    continue;
                }
                llm_turns += 1;
                let continued_turn = provider_continuation.is_some();
                let typed_messages = crate::llm::LlmMessage::from_openai_values(&messages)?;
                let typed_tools =
                    crate::llm::ToolDefinition::from_openai_values_opt(llm_tools.as_deref())?;
                let request = LlmRequest::new(&typed_messages, typed_tools.as_deref())
                    .with_scope(request_scope.clone())
                    .with_continuation(provider_continuation.take());
                let stream_result = llm.stream(request).await;
                let stream = match stream_result {
                    Ok(stream) => stream,
                    Err(error) => {
                        if continued_turn {
                            record_provider_continuation_lifecycle(
                                &store,
                                session_id,
                                "provider_continuation_abandoned",
                                &run_id,
                            )?;
                        }
                        reconciliation_failure = Some(redact_provider_failure(&nib_cfg, error));
                        reconciliation_reason = Some("llm_stream_failed".to_string());
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
                match finish_private_provider_stream(stream, &public_output_sensitive_values).await
                {
                    Ok((response, pending_stream_events)) => {
                        if continued_turn {
                            record_provider_continuation_lifecycle(
                                &store,
                                session_id,
                                "provider_continuation_closed",
                                &run_id,
                            )?;
                        }
                        if response.terminal_status == LlmTerminalStatus::Refused {
                            response_content = None;
                            tool_calls.clear();
                            provider_continuation = None;
                            reconciliation_reason = Some("model_refusal".to_string());
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
                            for event in pending_stream_events {
                                emit(&cfg.stream_tx, event).await;
                            }
                            response_content = response.content;
                            tool_calls = response.tool_calls.unwrap_or_default();
                            provider_continuation = response.continuation;
                            if provider_continuation.is_some() {
                                record_provider_continuation_lifecycle(
                                    &store,
                                    session_id,
                                    "provider_continuation_opened",
                                    &run_id,
                                )?;
                            }
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
                    }
                    Err(error) => {
                        if continued_turn {
                            record_provider_continuation_lifecycle(
                                &store,
                                session_id,
                                "provider_continuation_abandoned",
                                &run_id,
                            )?;
                        }
                        reconciliation_failure = Some(redact_provider_failure(&nib_cfg, error));
                        reconciliation_reason = Some("llm_stream_failed".to_string());
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
                if !close_steering_admission(
                    steering_enabled,
                    &store,
                    session_id,
                    &run_id,
                    if tool_calls.is_empty() {
                        "assistant_response_commit"
                    } else {
                        "tool_proposal_commit"
                    },
                )? {
                    continue;
                }
                if tool_calls.is_empty() {
                    if let Some(content) = response_content.as_deref() {
                        let content = safe_persisted_provider_message(
                            content,
                            &public_output_sensitive_values,
                            true,
                        );
                        store
                            .try_append_message(session_id, "assistant", &content)
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
                            "invocation_id": call.invocation_id,
                            "name": call.name,
                            "arguments": if call.name == "ask_question" {
                                match crate::tools::executor::validate_registered_tool_arguments(
                                    &call.name,
                                    &call.arguments,
                                ) {
                                    Ok(()) => safe_question_arguments(
                                        &call.arguments,
                                        &public_output_sensitive_values,
                                    ),
                                    Err(_) => json!({"validation": "rejected"}),
                                }
                            } else {
                                call.arguments.clone()
                            },
                        })).collect::<Vec<_>>(),
                    });
                    let persisted_intent = safe_persisted_provider_message(
                        &intent.to_string(),
                        &public_output_sensitive_values,
                        false,
                    );
                    store
                        .try_append_message(session_id, "assistant", &persisted_intent)
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
                        invocation_id: call.invocation_id,
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
                            },
                        )
                        .await;
                        store
                            .record_event(
                                session_id,
                                "approval_required",
                                json!({"kind": "tool", "invocation_id": call.invocation_id, "tool_name": call.name}),
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
                                "invocation_id": request.invocation_id,
                                "tool": request.name,
                                "success": false,
                                "output": Value::Null,
                                "error": error,
                            })
                        })
                        .collect::<Vec<_>>();
                    let classifications = vec![ToolResultClass::Error; tool_calls.len()];
                    let continuation_failure = record_provider_tool_outputs(
                        &mut provider_continuation,
                        &tool_calls,
                        &observations,
                        &classifications,
                    )
                    .err();
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
                                "tool_calls": tool_calls.iter().map(|call| json!({
                                    "invocation_id": call.invocation_id,
                                    "name": call.name,
                                })).collect::<Vec<_>>(),
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
                    if continuation_failure.is_none() && llm_turns < max_turns {
                        open_steering_admission(
                            steering_enabled,
                            &store,
                            session_id,
                            &run_id,
                            "rejected_tool_batch_build_context",
                        )?;
                    }
                    state = transition_state(
                        &store,
                        session_id,
                        state,
                        if continuation_failure.is_some() {
                            AgentState::Reconciliation
                        } else {
                            AgentState::BuildContext
                        },
                        &mut trace,
                        &mut transition_count,
                        &cfg.stream_tx,
                    )
                    .await?;
                    if let Some(error) = continuation_failure {
                        reconciliation_failure = Some(redact_provider_failure(
                            &nib_cfg,
                            LlmError::local(
                                LlmErrorClass::Protocol,
                                LlmErrorPhase::Continuation,
                                error,
                            ),
                        ));
                        reconciliation_reason = Some("provider_continuation_failed".to_string());
                    }
                    continue;
                }

                let mut observations = Vec::new();
                let mut classifications = Vec::new();
                let mut batch_success = true;
                let mut batch_user_denied = false;
                let mut prepared_tasks = PreparedTaskBatch::default();
                for request in &tool_calls {
                    if request.name != "ask_question" {
                        record_tool_started_and_open_steering(
                            &store,
                            session_id,
                            &run_id,
                            request,
                            llm_turns < max_turns,
                        )?;
                    } else {
                        store
                            .record_event(
                                session_id,
                                "tool_started",
                                json!({"invocation_id": request.invocation_id, "tool_name": request.name}),
                            )
                            .map_err(|error| error.to_string())?;
                    }
                    emit(
                        &cfg.stream_tx,
                        StreamEvent::ToolStarted {
                            tool_name: request.name.clone(),
                        },
                    )
                    .await;
                    if request.name == "ask_question"
                        && crate::tools::executor::validate_registered_tool_arguments(
                            &request.name,
                            &request.arguments,
                        )
                        .is_ok()
                    {
                        pending_question = Some(request.clone());
                        continue;
                    }
                    let result = executor
                        .execute(
                            ToolCall {
                                invocation_id: request.invocation_id,
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
                                "invocation_id": request.invocation_id,
                                "tool_name": request.name,
                                "success": result.success,
                                "output": output,
                                "error": error,
                            }),
                        )
                        .map_err(|error| error.to_string())?;
                    observations.push(json!({
                        "invocation_id": request.invocation_id,
                        "tool": request.name,
                        "success": result.success,
                        "output": result.output,
                        "error": result.error,
                    }));
                    classifications.push(ToolResultClass::from_success(result.success));
                }
                let continuation_failure = if pending_question.is_none() {
                    record_provider_tool_outputs(
                        &mut provider_continuation,
                        &tool_calls,
                        &observations,
                        &classifications,
                    )
                    .err()
                } else {
                    None
                };
                if let Some(error) = continuation_failure {
                    store
                        .try_append_message(
                            session_id,
                            "tool",
                            &json!({"observations": observations}).to_string(),
                        )
                        .map_err(|error| error.to_string())?;
                    tool_calls.clear();
                    response_content = None;
                    reconciliation_failure = Some(redact_provider_failure(
                        &nib_cfg,
                        LlmError::local(
                            LlmErrorClass::Protocol,
                            LlmErrorPhase::Continuation,
                            error,
                        ),
                    ));
                    reconciliation_reason = Some("provider_continuation_failed".to_string());
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
                } else if pending_question.is_some() {
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
                        if llm_turns < max_turns {
                            open_steering_admission(
                                steering_enabled,
                                &store,
                                session_id,
                                &run_id,
                                "completed_tool_batch_build_context",
                            )?;
                        }
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
                crate::tools::executor::validate_registered_tool_arguments(
                    &request.name,
                    &request.arguments,
                )?;
                let raw_question = request
                    .arguments
                    .get("question")
                    .and_then(Value::as_str)
                    .filter(|question| !question.trim().is_empty())
                    .ok_or_else(|| "ask_question requires a non-empty question".to_string())?;
                let question = crate::interactive::bounded_public_text(
                    raw_question,
                    &public_output_sensitive_values,
                    MAX_QUESTION_BYTES,
                    false,
                );
                let options = request
                    .arguments
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|option| {
                                crate::interactive::bounded_public_text(
                                    option,
                                    &public_output_sensitive_values,
                                    MAX_QUESTION_OPTION_BYTES,
                                    false,
                                )
                            })
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
                        json!({"invocation_id": request.invocation_id, "question": question, "options": options}),
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
                        Ok(crate::interactive::bounded_public_text(
                            &answer,
                            &public_output_sensitive_values,
                            MAX_QUESTION_BYTES,
                            false,
                        ))
                    }
                })
                .map_err(|error| {
                    crate::interactive::bounded_public_text(
                        &error,
                        &public_output_sensitive_values,
                        MAX_QUESTION_BYTES,
                        false,
                    )
                });

                let arguments = safe_question_execution_arguments(
                    &request.arguments,
                    &answer,
                    &public_output_sensitive_values,
                );
                let result = executor
                    .execute(
                        ToolCall {
                            invocation_id: request.invocation_id,
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
                            "invocation_id": request.invocation_id,
                            "tool_name": request.name,
                            "success": question_success,
                            "output": question_output,
                            "error": question_error,
                        }),
                    )
                    .map_err(|error| error.to_string())?;
                let question_observation = json!({
                    "invocation_id": request.invocation_id,
                    "tool": request.name,
                    "success": question_success,
                    "output": question_output,
                    "error": question_error,
                });
                let continuation_failure = record_provider_tool_output(
                    &mut provider_continuation,
                    &request,
                    &question_observation,
                    ToolResultClass::from_success(question_success),
                )
                .err();
                pending_observations.push(question_observation);
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
                if let Some(error) = continuation_failure {
                    reconciliation_failure = Some(redact_provider_failure(
                        &nib_cfg,
                        LlmError::local(
                            LlmErrorClass::Protocol,
                            LlmErrorPhase::Continuation,
                            error,
                        ),
                    ));
                    reconciliation_reason = Some("provider_continuation_failed".to_string());
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
                } else if !plan_updated {
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
                    if llm_turns < max_turns {
                        open_steering_admission(
                            steering_enabled,
                            &store,
                            session_id,
                            &run_id,
                            "answered_question_build_context",
                        )?;
                    }
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
                if provider_continuation.take().is_some() {
                    record_provider_continuation_lifecycle(
                        &store,
                        session_id,
                        "provider_continuation_abandoned",
                        &run_id,
                    )?;
                }
                let reason = reconciliation_reason
                    .take()
                    .unwrap_or_else(|| "reconciled".to_string());
                if reason == "plan_ready"
                    && !close_steering_admission(
                        steering_enabled,
                        &store,
                        session_id,
                        &run_id,
                        "plan_ready_commit",
                    )?
                {
                    reconciliation_reason = Some(reason);
                    continue;
                }
                let mut continue_plan = false;
                outcome = match reason.as_str() {
                    "model_response" => {
                        let safe_plan_outcome = safe_provider_plan_outcome(
                            response_content.as_deref(),
                            &public_output_sensitive_values,
                        );
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
                                    plan.complete_current_step(&safe_plan_outcome);
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
                        if is_agent_failure_outcome(other) {
                            block_active_plan_for_failure(
                                &store,
                                session_id,
                                active_plan_id.as_deref(),
                                &normalized_goal,
                                other,
                            )?;
                        }
                        if reconciliation_failure.is_none() {
                            append_assistant_if_allowed(
                                &store,
                                session_id,
                                &format!("Run reconciled with outcome: {other}"),
                            )?;
                        }
                        other.to_string()
                    }
                };
                let failure_details = reconciliation_failure.clone();
                store
                    .record_event(
                        session_id,
                        "reconciliation",
                        json!({
                            "outcome": outcome,
                            "continue": continue_plan,
                            "failure": failure_details,
                        }),
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
                if continue_plan && llm_turns < max_turns {
                    open_steering_admission(
                        steering_enabled,
                        &store,
                        session_id,
                        &run_id,
                        "continued_plan_build_context",
                    )?;
                }
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
        run_id: run_id.clone(),
        steps_taken: llm_turns,
        last_message: final_session
            .as_ref()
            .and_then(|session| session.messages.last())
            .map(|message| message.content.clone()),
        tool_call_count,
        final_state: state,
        outcome,
        failure: reconciliation_failure,
        bound_reached,
        trace,
    })
}

fn runtime_terminal_event(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    outcome: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            if !session.events.iter().any(|event| {
                event.kind == "run_terminal"
                    && event.details.get("run_id").and_then(Value::as_str) == Some(run_id)
            }) {
                append_session_event(
                    session,
                    "run_terminal",
                    json!({"run_id": run_id, "outcome": outcome}),
                );
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
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
    let (transitioned_to_reconciliation, tool_call_count, trace) = store
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
        run_id: String::new(),
        steps_taken: 0,
        // Cancellation is local lifecycle state, never model-authored output. In
        // particular, do not echo the user's interrupted prompt as a gateway reply.
        last_message: None,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: "cancelled_by_user".to_string(),
        failure: None,
        bound_reached: false,
        trace,
    })
}

fn llm_configuration_failure(
    config: &crate::config::NibConfig,
    provider_override: Option<&str>,
    safe_message: &str,
    sensitive_values: &[String],
) -> LlmError {
    let provider = provider_override
        .or(config.llm.active_provider.as_deref())
        .unwrap_or("unconfigured");
    let entry = config.llm.providers.get(provider);
    let transport = crate::llm::registry::provider_descriptor(provider)
        .map(|descriptor| descriptor.configured_transport(entry).as_str())
        .unwrap_or("unknown");
    LlmError::new(
        LlmErrorClass::Configuration,
        LlmErrorPhase::Configuration,
        crate::llm::RetryDisposition::NotAttempted,
        crate::llm::LlmErrorMetadata::new(
            provider,
            transport,
            entry.map(|entry| entry.model.as_str()),
            None,
            &crate::llm::factory::provider_error_sensitive_values(sensitive_values.to_vec()),
        ),
        safe_message,
    )
}

async fn reconcile_preflight_llm_failure(
    store: &SessionStore,
    session_id: &str,
    normalized_goal: &str,
    failure: LlmError,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<AgentRunSummary, String> {
    let persisted_failure = failure.clone();
    let (last_message, tool_call_count) = store
        .update_session(session_id, |session| {
            if let Some(plan) = session
                .plan
                .as_mut()
                .filter(|plan| plan.matches_goal(normalized_goal) && !plan.is_complete())
            {
                plan.outcome = Some("configuration_failed".to_string());
                if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                    if step.status != "Completed" {
                        step.status = "Blocked".to_string();
                        step.outcome = Some("configuration_failed".to_string());
                        step.updated_at = Some(Utc::now());
                    }
                }
            }
            let previous_state = last_persisted_state(session);
            append_session_event(
                session,
                "state_transition",
                json!({
                    "from": previous_state,
                    "to": AgentState::Reconciliation.as_str(),
                }),
            );
            append_session_event(
                session,
                "reconciliation",
                json!({
                    "outcome": "configuration_failed",
                    "continue": false,
                    "failure": persisted_failure.clone(),
                }),
            );
            append_session_event(
                session,
                "state_transition",
                json!({
                    "from": AgentState::Reconciliation.as_str(),
                    "to": AgentState::Done.as_str(),
                }),
            );
            Ok((
                session
                    .messages
                    .last()
                    .map(|message| message.content.clone()),
                session.tool_calls.len(),
            ))
        })
        .map_err(|error| format!("failed to reconcile LLM configuration failure: {error}"))?;

    emit(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Reconciliation.as_str().to_string(),
        },
    )
    .await;
    emit(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: "configuration_failed".to_string(),
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
        run_id: String::new(),
        steps_taken: 0,
        last_message,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: "configuration_failed".to_string(),
        failure: Some(failure),
        bound_reached: false,
        trace: vec![
            AgentState::Reconciliation.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
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

fn record_provider_continuation_lifecycle(
    store: &SessionStore,
    session_id: &str,
    kind: &str,
    run_id: &str,
) -> Result<(), String> {
    store
        .record_event(session_id, kind, json!({"run_id": run_id}))
        .map(|_| ())
        .map_err(|error| format!("failed to record provider continuation lifecycle: {error}"))
}

fn record_provider_tool_output(
    continuation: &mut Option<ProviderContinuation>,
    request: &ToolCallRequest,
    observation: &Value,
    classification: ToolResultClass,
) -> Result<(), String> {
    match continuation.as_mut() {
        Some(continuation) => continuation.record_tool_result(ProviderToolResult::new(
            request.invocation_id,
            observation.clone(),
            classification,
        )?),
        None => Ok(()),
    }
}

fn record_provider_tool_outputs(
    continuation: &mut Option<ProviderContinuation>,
    requests: &[ToolCallRequest],
    observations: &[Value],
    classifications: &[ToolResultClass],
) -> Result<(), String> {
    if continuation.is_none() {
        return Ok(());
    }
    if requests.len() != observations.len() || requests.len() != classifications.len() {
        return Err("provider continuation tool/output counts do not match".to_string());
    }
    for ((request, observation), classification) in
        requests.iter().zip(observations).zip(classifications)
    {
        record_provider_tool_output(continuation, request, observation, *classification)?;
    }
    Ok(())
}

fn reconcile_interrupted_provider_continuation(
    store: &SessionStore,
    session_id: &str,
) -> Result<bool, String> {
    store
        .update_session(session_id, |session| {
            let latest_lifecycle = session.events.iter().rev().find(|event| {
                matches!(
                    event.kind.as_str(),
                    "provider_continuation_opened"
                        | "provider_continuation_closed"
                        | "provider_continuation_abandoned"
                        | "provider_continuation_interrupted"
                        | "reconciliation"
                )
            });
            let Some(opened) =
                latest_lifecycle.filter(|event| event.kind == "provider_continuation_opened")
            else {
                return Ok(false);
            };
            let prior_run_id = opened
                .details
                .get("run_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let current_state = last_persisted_state(session);
            append_session_event(
                session,
                "provider_continuation_interrupted",
                json!({
                    "prior_run_id": prior_run_id,
                    "reason": "opaque continuation was discarded after process interruption",
                }),
            );
            if !matches!(
                current_state.as_deref(),
                Some("Reconciliation") | Some("Done")
            ) {
                append_session_event(
                    session,
                    "state_transition",
                    json!({
                        "from": current_state,
                        "to": AgentState::Reconciliation.as_str(),
                    }),
                );
            }
            if let Some(plan) = session.plan.as_mut().filter(|plan| !plan.is_complete()) {
                plan.outcome = Some("provider_continuation_interrupted".to_string());
                if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                    if step.status != "Completed" {
                        step.status = "Blocked".to_string();
                        step.outcome = Some("provider_continuation_interrupted".to_string());
                        step.updated_at = Some(Utc::now());
                    }
                }
            }
            if session
                .messages
                .last()
                .is_some_and(|message| message.role == "tool")
            {
                session.messages.push(SessionMessage {
                    index: session.messages.len(),
                    role: "assistant".to_string(),
                    content: json!({
                        "type": "provider_continuation_boundary",
                        "outcome": "provider_continuation_interrupted",
                    })
                    .to_string(),
                    timestamp: Some(Utc::now()),
                    attachments: Vec::new(),
                });
            }
            append_session_event(
                session,
                "reconciliation",
                json!({
                    "outcome": "provider_continuation_interrupted",
                    "continue": false,
                }),
            );
            if current_state.as_deref() != Some(AgentState::Done.as_str()) {
                append_session_event(
                    session,
                    "state_transition",
                    json!({
                        "from": AgentState::Reconciliation.as_str(),
                        "to": AgentState::Done.as_str(),
                    }),
                );
            }
            Ok(true)
        })
        .map_err(|error| format!("failed to reconcile interrupted provider turn: {error}"))
}

fn block_active_plan_for_failure(
    store: &SessionStore,
    session_id: &str,
    active_plan_id: Option<&str>,
    normalized_goal: &str,
    reason: &str,
) -> Result<(), String> {
    store
        .update_session(session_id, |session| {
            let Some(plan) = session.plan.as_mut() else {
                return Ok(());
            };
            if active_plan_id != Some(plan.id.as_str())
                || !plan.matches_goal(normalized_goal)
                || plan.is_complete()
            {
                return Ok(());
            }
            plan.outcome = Some(reason.to_string());
            if let Some(step) = plan.steps.get_mut(plan.current_step_index) {
                if step.status != "Completed" {
                    step.status = "Blocked".to_string();
                    step.outcome = Some(reason.to_string());
                    step.updated_at = Some(Utc::now());
                }
            }
            Ok(())
        })
        .map_err(|error| format!("failed to block plan after provider failure: {error}"))
}

fn is_llm_failure_outcome(outcome: &str) -> bool {
    [
        "planning_failed:",
        "llm_stream_failed:",
        "invalid_tool_stream:",
        "provider_continuation_failed:",
        "compression_failed:",
        "configuration_failed:",
    ]
    .iter()
    .any(|prefix| outcome.starts_with(prefix))
        || matches!(
            outcome,
            "planning_failed"
                | "llm_stream_failed"
                | "invalid_tool_stream"
                | "provider_continuation_failed"
                | "compression_failed"
                | "configuration_failed"
        )
}

fn is_agent_failure_outcome(outcome: &str) -> bool {
    is_llm_failure_outcome(outcome)
        || matches!(
            outcome,
            "model_refusal"
                | "empty_model_response"
                | "tool_execution_failed"
                | "transition_limit_reached"
                | "turn_limit_reached"
                | "provider_continuation_interrupted"
        )
}

fn redact_provider_failure(config: &crate::config::NibConfig, error: LlmError) -> LlmError {
    error.redacted_with(&crate::llm::factory::provider_error_sensitive_values(
        config.sensitive_values(),
    ))
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
            invocation_id: Some(crate::tools::ToolInvocationId::new()),
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

fn prepare_user_turn(
    store: &SessionStore,
    session_id: &str,
    content: &str,
    project_root: &Path,
) -> Result<(), String> {
    let (content, attachments) =
        crate::interactive::resolve_path_attachments(project_root, content)?;
    store
        .update_session(session_id, |session| {
            session.messages.push(SessionMessage {
                index: session.messages.len(),
                role: "user".to_string(),
                content,
                timestamp: Some(Utc::now()),
                attachments,
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
    } else if plan.outcome.as_deref() == Some("provider_continuation_interrupted") {
        Some("provider_continuation_interrupted")
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
                    attachments: Vec::new(),
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

fn emit_nonblocking(stream_tx: &Option<Sender<StreamEvent>>, event: StreamEvent) {
    if let Some(sender) = stream_tx {
        let _ = sender.try_send(event);
    }
}

/// Holds provider deltas behind the private terminal-authority boundary.
///
/// Network adapters bound event count and bytes before constructing `LlmStream`. Returning
/// projections only beside a validated, non-refusal response makes it impossible for callers to
/// accidentally publish a partial stream that later fails terminal validation.
// Preserve the canonical typed error, including its retry and redaction-safe failure metadata,
// across this private terminal-authority boundary rather than introducing a boxed error API.
#[allow(clippy::result_large_err)]
async fn finish_private_provider_stream(
    stream: LlmStream,
    sensitive_values: &[String],
) -> Result<(LlmResponse, Vec<StreamEvent>), LlmError> {
    let response = stream.finish().await?;
    let pending = project_validated_llm_response(&response, sensitive_values);
    Ok((response, pending))
}

fn project_validated_llm_response(
    response: &LlmResponse,
    sensitive_values: &[String],
) -> Vec<StreamEvent> {
    if response.terminal_status == LlmTerminalStatus::Refused {
        return Vec::new();
    }
    let mut events = Vec::new();
    if let Some(content) = response.content.as_ref() {
        events.push(StreamEvent::Content(
            crate::interactive::bounded_public_text(
                content,
                sensitive_values,
                MAX_PUBLIC_PROVIDER_CONTENT_BYTES,
                true,
            ),
        ));
    }
    if let Some(tool_calls) = response.tool_calls.as_ref() {
        events.extend(tool_calls.iter().enumerate().map(|(index, call)| {
            StreamEvent::ToolCallChunk {
                index,
                name: Some(crate::interactive::bounded_public_text(
                    &call.name,
                    sensitive_values,
                    MAX_PUBLIC_PROVIDER_TOOL_CHUNK_BYTES,
                    false,
                )),
                arguments: Some(crate::interactive::bounded_public_text(
                    &call.arguments.to_string(),
                    sensitive_values,
                    MAX_PUBLIC_PROVIDER_TOOL_CHUNK_BYTES,
                    false,
                )),
            }
        }));
    }
    events
}

fn safe_persisted_provider_message(
    value: &str,
    sensitive_values: &[String],
    preserve_layout: bool,
) -> String {
    crate::interactive::bounded_public_text(
        value,
        sensitive_values,
        MAX_PERSISTED_PROVIDER_MESSAGE_BYTES,
        preserve_layout,
    )
}

fn safe_provider_plan_outcome(content: Option<&str>, sensitive_values: &[String]) -> String {
    content
        .map(|content| safe_persisted_provider_message(content, sensitive_values, true))
        .unwrap_or_else(|| "model completed the plan step".to_string())
}

fn sanitize_provider_plan(plan: &mut crate::session::Plan, sensitive_values: &[String]) {
    for step in &mut plan.steps {
        step.description = crate::interactive::bounded_public_text(
            &step.description,
            sensitive_values,
            MAX_PUBLIC_PROVIDER_TOOL_CHUNK_BYTES,
            false,
        );
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
    use crate::config::{save_config, save_nib_config_full, LlmConfig, NibConfig, ProviderEntry};
    use crate::tools::models::ApprovalDecision;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    #[test]
    fn provider_stream_projection_uses_only_the_validated_response() {
        assert_eq!(
            project_validated_llm_response(&LlmResponse::text("safe model text"), &[]),
            vec![StreamEvent::Content("safe model text".to_string())]
        );
        let mut refused = LlmResponse::text("private refusal detail");
        refused.terminal_status = LlmTerminalStatus::Refused;
        refused.finish_reason = crate::llm::LlmFinishReason::Refusal;
        assert!(project_validated_llm_response(&refused, &[]).is_empty());

        let secret = "stream/output-secret".to_string();
        let projected = project_validated_llm_response(
            &LlmResponse::text(format!(
                "{secret} stream\\/output-secret c3RyZWFtL291dHB1dC1zZWNyZXQ= \u{1b}[2J"
            )),
            std::slice::from_ref(&secret),
        );
        let StreamEvent::Content(content) = &projected[0] else {
            panic!("validated text response must project as content")
        };
        assert!(!content.contains(&secret));
        assert!(!content.contains(r"stream\/output-secret"));
        assert!(!content.contains("c3RyZWFtL291dHB1dC1zZWNyZXQ"));
        assert!(!content.contains('\u{1b}'));
    }

    #[test]
    fn validated_provider_content_is_sanitized_before_session_persistence() {
        let directory = tempdir().expect("session directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let secret = "persist/provider-secret".to_string();
        for raw in [
            format!(
                "{secret} persist\\/provider-secret cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ= \u{1b}[2J"
            ),
            json!({
                "content": secret.clone(),
                "tool_calls": [{
                    "name": "read_file",
                    "arguments": {"path": "persist\\/provider-secret"}
                }]
            })
            .to_string(),
        ] {
            let session = store.create_session();
            store
                .try_append_message(&session.id, "user", "safe request")
                .expect("user message");
            let projected =
                safe_persisted_provider_message(&raw, std::slice::from_ref(&secret), true);
            store
                .try_append_message(&session.id, "assistant", &projected)
                .expect("safe assistant message");
            let persisted = store.load(&session.id).expect("persisted session");
            let assistant = &persisted
                .messages
                .last()
                .expect("assistant message")
                .content;
            assert!(!assistant.contains(&secret));
            assert!(!assistant.contains(r"persist\/provider-secret"));
            assert!(!assistant.contains("cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ="));
            assert!(!assistant
                .chars()
                .any(|character| { character.is_control() && !matches!(character, '\n' | '\t') }));
            assert!(assistant.len() <= MAX_PERSISTED_PROVIDER_MESSAGE_BYTES);
        }

        let mut plan = crate::session::Plan::new(
            "safe goal",
            vec![crate::session::PlanStep {
                description: format!(
                    "{secret} persist\\/provider-secret cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ= \u{1b}[2J"
                ),
                status: "Pending".to_string(),
                outcome: None,
                attempts: 0,
                updated_at: None,
            }],
        );
        sanitize_provider_plan(&mut plan, std::slice::from_ref(&secret));
        let description = &plan.steps[0].description;
        assert!(!description.contains(&secret));
        assert!(!description.contains(r"persist\/provider-secret"));
        assert!(!description.contains("cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ="));
        assert!(!description.contains('\u{1b}'));

        let raw_outcome = format!(
            "{secret} persist\\/provider-secret cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ= \u{1b}[2J {} OUTCOME_PRIVATE_TAIL",
            "o".repeat(MAX_PERSISTED_PROVIDER_MESSAGE_BYTES + 512),
        );
        let outcome = safe_provider_plan_outcome(Some(&raw_outcome), std::slice::from_ref(&secret));
        plan.complete_current_step(&outcome);
        let outcome = plan.steps[0].outcome.as_deref().expect("plan outcome");
        assert!(!outcome.contains(&secret));
        assert!(!outcome.contains(r"persist\/provider-secret"));
        assert!(!outcome.contains("cGVyc2lzdC9wcm92aWRlci1zZWNyZXQ="));
        assert!(!outcome.contains('\u{1b}'));
        assert!(!outcome.contains("OUTCOME_PRIVATE_TAIL"));
        assert!(outcome.len() <= MAX_PERSISTED_PROVIDER_MESSAGE_BYTES);
    }

    #[tokio::test]
    async fn provider_deltas_remain_private_until_terminal_validation() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(Ok(crate::llm::LlmStreamEvent::Delta(
            crate::llm::LlmDelta::Content("private-before-late-error\u{1b}[31m".to_string()),
        )))
        .await
        .expect("delta");
        tx.send(Err(crate::llm::LlmStreamFailure::from(
            "late provider rejection",
        )))
        .await
        .expect("error");
        drop(tx);

        let error = finish_private_provider_stream(LlmStream::from_public_receiver(rx), &[])
            .await
            .expect_err("late error rejects the private stream");
        assert_eq!(error.class, LlmErrorClass::Protocol);

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(Ok(crate::llm::LlmStreamEvent::Delta(
            crate::llm::LlmDelta::Content("refusal-private-content".to_string()),
        )))
        .await
        .expect("refusal delta");
        tx.send(Ok(crate::llm::LlmStreamEvent::Terminal(
            crate::llm::LlmFinishReason::Refusal,
        )))
        .await
        .expect("refusal terminal");
        drop(tx);

        let (response, projected) =
            finish_private_provider_stream(LlmStream::from_public_receiver(rx), &[])
                .await
                .expect("valid private refusal");
        assert_eq!(response.terminal_status, LlmTerminalStatus::Refused);
        assert!(projected.is_empty(), "refusal deltas must stay private");
    }

    struct EnvironmentGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvironmentGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

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
                    ..ProviderEntry::default()
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
    fn interrupted_provider_turn_is_reconciled_once_and_never_replays_tools() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let mut session = store.create_session_with_id("interrupted-provider-turn");
        let mut plan = pending_plan("inspect safely", "read the requested file");
        plan.approve();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved plan");
        store
            .try_append_message(&session.id, "user", "inspect safely")
            .unwrap();
        store
            .try_append_message(&session.id, "assistant", "normalized tool intent")
            .unwrap();
        record_provider_continuation_lifecycle(
            &store,
            &session.id,
            "provider_continuation_opened",
            "prior-run",
        )
        .unwrap();
        store
            .record_event(
                &session.id,
                "tool_completed",
                json!({"tool_name": "read_file", "success": true}),
            )
            .unwrap();
        store
            .try_append_message(
                &session.id,
                "tool",
                &json!({"observations": [{"tool": "read_file", "success": true}]}).to_string(),
            )
            .unwrap();
        let messages_before_reconciliation = store
            .load(&session.id)
            .expect("session before reconciliation")
            .messages;

        assert!(reconcile_interrupted_provider_continuation(&store, &session.id).unwrap());
        let reconciled = store.load(&session.id).expect("reconciled session");
        assert_eq!(
            reconciled.messages[..messages_before_reconciliation.len()],
            messages_before_reconciliation
        );
        let boundary = reconciled.messages.last().expect("reconciliation boundary");
        assert_eq!(boundary.role, "assistant");
        assert_eq!(
            serde_json::from_str::<Value>(&boundary.content).unwrap(),
            json!({
                "type": "provider_continuation_boundary",
                "outcome": "provider_continuation_interrupted",
            })
        );
        let plan = reconciled.plan.as_ref().expect("blocked plan");
        assert_eq!(
            plan.outcome.as_deref(),
            Some("provider_continuation_interrupted")
        );
        assert_eq!(plan.steps[plan.current_step_index].status, "Blocked");
        assert_eq!(
            reconciled
                .events
                .iter()
                .filter(|event| event.kind == "tool_completed")
                .count(),
            1
        );
        assert!(reconciled.events.iter().any(|event| {
            event.kind == "reconciliation"
                && event.details["outcome"] == "provider_continuation_interrupted"
                && event.details["continue"] == false
        }));

        invalidate_nonresumable_plan(&store, &session.id, "inspect safely").unwrap();
        assert!(store.load(&session.id).unwrap().plan.is_none());
        assert!(!reconcile_interrupted_provider_continuation(&store, &session.id).unwrap());
        assert_eq!(
            store
                .load(&session.id)
                .unwrap()
                .events
                .iter()
                .filter(|event| event.kind == "tool_completed")
                .count(),
            1
        );
    }

    #[test]
    fn exact_run_identity_is_canonical_private_and_exactly_scoped() {
        for invalid in [
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
        ] {
            let error = resolve_agent_run_id(Some(invalid.to_string()))
                .expect_err("non-canonical run ID must fail closed");
            assert!(error.contains("32 lowercase hexadecimal"));
        }

        let first = resolve_agent_run_id(None).expect("generated run ID");
        let second = resolve_agent_run_id(None).expect("second generated run ID");
        assert_ne!(first, second);
        for generated in [&first, &second] {
            assert_eq!(generated.len(), 32);
            assert!(generated
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        }

        let supplied = "0123456789abcdef0123456789abcdef";
        let scope = request_scope_for_run("exact-scope-session", supplied).expect("request scope");
        assert_eq!(scope.session_id, "exact-scope-session");
        assert_eq!(scope.run_id, supplied);

        let summary = AgentRunSummary {
            session_id: "legacy-summary".to_string(),
            run_id: supplied.to_string(),
            steps_taken: 0,
            last_message: None,
            tool_call_count: 0,
            final_state: AgentState::Done,
            outcome: "completed".to_string(),
            failure: None,
            bound_reached: false,
            trace: Vec::new(),
        };
        let mut legacy = serde_json::to_value(summary).expect("serialize summary");
        legacy
            .as_object_mut()
            .expect("summary object")
            .remove("run_id");
        let decoded: AgentRunSummary =
            serde_json::from_value(legacy).expect("legacy summary remains readable");
        assert!(decoded.run_id.is_empty());
    }

    #[test]
    fn exact_run_steering_is_persisted_ordered_and_bound_before_intake() {
        let directory = tempdir().expect("steering store");
        let store = SessionStore::new(directory.path());
        let session = store.create_session_with_id("steering-order");
        let run_id = "0123456789abcdef0123456789abcdef";
        let (handle, mut receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("steering channel");

        assert!(handle.submit("before admission").is_err());
        store
            .record_event(&session.id, "run_started", json!({"run_id": run_id}))
            .expect("run admission");
        bind_exact_run_steering_receiver(&store, &session.id, run_id, &receiver)
            .expect("install exact receiver");
        assert_eq!(handle.submit("first instruction").expect("first"), 1);
        assert_eq!(handle.submit("second instruction").expect("second"), 2);

        let persisted_before_intake = store.load(&session.id).expect("persisted steering");
        let inputs = persisted_before_intake
            .events
            .iter()
            .filter(|event| event.kind == "steering_input")
            .collect::<Vec<_>>();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].details["sequence"], 1);
        assert_eq!(inputs[0].details["source"], "plain");
        assert_eq!(inputs[0].details["text"], "first instruction");
        assert_eq!(inputs[1].details["sequence"], 2);
        assert!(!persisted_before_intake
            .events
            .iter()
            .any(|event| event.kind == "steering_intake"));

        let instructions = receiver.drain();
        assert_eq!(
            instructions
                .iter()
                .map(|instruction| (instruction.sequence, instruction.text.as_str()))
                .collect::<Vec<_>>(),
            [(1, "first instruction"), (2, "second instruction")]
        );
        assert!(record_steering_intake(
            &store,
            &session.id,
            run_id,
            &receiver.channel_id,
            &instructions[1..],
        )
        .expect_err("out-of-order intake must fail closed")
        .contains("skip an earlier"));
        record_steering_intake(
            &store,
            &session.id,
            run_id,
            &receiver.channel_id,
            &instructions,
        )
        .expect("durable intake");
        assert!(record_steering_intake(
            &store,
            &session.id,
            run_id,
            &receiver.channel_id,
            &instructions,
        )
        .is_err());

        let other_directory = tempdir().expect("other steering store");
        let other_store = SessionStore::new(other_directory.path());
        let other_session = other_store.create_session_with_id("other-steering-session");
        assert!(receiver
            .verify_binding(&other_store, &other_session.id, run_id)
            .is_err());
        assert!(receiver
            .verify_binding(&store, &session.id, "abcdef0123456789abcdef0123456789")
            .is_err());
    }

    #[test]
    fn exact_run_steering_installs_one_channel_and_linearizes_action_admission() {
        let directory = tempdir().expect("steering store");
        let store = SessionStore::new(directory.path());
        let session = store.create_session_with_id("steering-channel-owner");
        let run_id = "0123456789abcdef0123456789abcdef";
        let (first, mut first_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("first steering channel");
        let (second, second_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
                .expect("second steering channel");
        store
            .record_event(&session.id, "run_started", json!({"run_id": run_id}))
            .expect("run start");
        bind_exact_run_steering_receiver(&store, &session.id, run_id, &first_receiver)
            .expect("install first channel");
        assert!(
            bind_exact_run_steering_receiver(&store, &session.id, run_id, &second_receiver,)
                .expect_err("second channel must not install")
                .contains("already has")
        );
        assert!(second
            .submit("must not enter the first channel")
            .expect_err("uninstalled handle")
            .contains("not installed"));

        assert_eq!(first.submit("wins before the action fence").unwrap(), 1);
        assert!(
            !close_steering_admission(true, &store, &session.id, run_id, "test_action_commit",)
                .expect("pending instruction blocks action")
        );
        let instructions = first_receiver.drain();
        record_steering_intake(
            &store,
            &session.id,
            run_id,
            &first_receiver.channel_id,
            &instructions,
        )
        .expect("account pending instruction");
        assert!(
            close_steering_admission(true, &store, &session.id, run_id, "test_action_commit",)
                .expect("action fence closes after intake")
        );
        assert!(first
            .submit("must lose after the action fence")
            .expect_err("closed admission")
            .contains("current run boundary"));
    }

    #[test]
    fn exact_run_steering_survives_historical_reconciliation_for_later_active_steps() {
        let directory = tempdir().expect("steering store");
        let store = SessionStore::new(directory.path());
        let session = store.create_session_with_id("steering-after-reconciliation");
        let run_id = "0123456789abcdef0123456789abcdef";
        let (handle, receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("steering channel");
        store
            .record_event(&session.id, "run_started", json!({"run_id": run_id}))
            .expect("run start");
        bind_exact_run_steering_receiver(&store, &session.id, run_id, &receiver)
            .expect("install exact receiver");
        store
            .record_event(
                &session.id,
                "state_transition",
                json!({"from": "UpdateMemory", "to": "Reconciliation"}),
            )
            .expect("historical reconciliation");
        open_steering_admission(
            true,
            &store,
            &session.id,
            run_id,
            "continued_plan_build_context",
        )
        .expect("reopen admission");
        store
            .record_event(
                &session.id,
                "state_transition",
                json!({"from": "Reconciliation", "to": "BuildContext"}),
            )
            .expect("active continuation");
        assert_eq!(
            handle
                .submit("steer the later approved plan step")
                .expect("later active steering"),
            1
        );
    }

    #[test]
    fn disabled_steering_admission_is_a_persistence_noop() {
        let directory = tempdir().expect("steering store");
        let store = SessionStore::new(directory.path());
        let session = store.create_session_with_id("steering-disabled-noop");
        let revision = store.load(&session.id).expect("persisted session").revision;

        assert!(close_steering_admission(
            false,
            &store,
            &session.id,
            "0123456789abcdef0123456789abcdef",
            "disabled_close",
        )
        .expect("disabled close is a no-op"));
        open_steering_admission(
            false,
            &store,
            &session.id,
            "0123456789abcdef0123456789abcdef",
            "disabled_open",
        )
        .expect("disabled open is a no-op");

        assert_eq!(
            store
                .load(&session.id)
                .expect("unchanged persisted session")
                .revision,
            revision
        );
    }

    #[test]
    fn exact_run_steering_rejects_stale_terminal_and_unbounded_input() {
        let directory = tempdir().expect("steering store");
        let store = SessionStore::new(directory.path());
        let session = store.create_session_with_id("steering-bounds");
        let first_run = "0123456789abcdef0123456789abcdef";
        let second_run = "abcdef0123456789abcdef0123456789";
        let (first, first_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), first_run, "tui")
                .expect("first steering channel");
        store
            .record_event(&session.id, "run_started", json!({"run_id": first_run}))
            .expect("first run");
        bind_exact_run_steering_receiver(&store, &session.id, first_run, &first_receiver)
            .expect("install first receiver");
        assert!(first.submit("\u{0007}").is_err());
        assert!(first
            .submit(&"x".repeat(MAX_STEERING_INPUT_BYTES + 1))
            .is_err());

        store
            .record_event(&session.id, "run_started", json!({"run_id": second_run}))
            .expect("replacement run");
        assert!(first
            .submit("must not reach the replacement run")
            .expect_err("stale handle")
            .contains("stale"));

        let bounded = store.create_session_with_id("steering-total-bound");
        let (bounded_handle, bounded_receiver) =
            exact_run_steering_channel(store.clone(), bounded.id.clone(), first_run, "plain")
                .expect("bounded channel");
        store
            .record_event(&bounded.id, "run_started", json!({"run_id": first_run}))
            .expect("bounded run");
        bind_exact_run_steering_receiver(&store, &bounded.id, first_run, &bounded_receiver)
            .expect("install bounded receiver");
        for _ in 0..4 {
            bounded_handle
                .submit(&"x".repeat(MAX_STEERING_INPUT_BYTES))
                .expect("within total bound");
        }
        assert!(bounded_handle
            .submit("one byte too many")
            .expect_err("total bound")
            .contains("byte limit"));
        store
            .record_event(
                &bounded.id,
                "run_terminal",
                json!({"run_id": first_run, "outcome": "completed"}),
            )
            .expect("terminal");
        assert!(bounded_handle.submit("too late").is_err());
    }

    #[test]
    fn exact_run_steering_channel_loss_is_explicit_after_persistence() {
        let directory = tempdir().expect("steering store");
        let store = SessionStore::new(directory.path());
        let session = store.create_session_with_id("steering-channel-loss");
        let run_id = "0123456789abcdef0123456789abcdef";
        let (handle, receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("steering channel");
        store
            .record_event(&session.id, "run_started", json!({"run_id": run_id}))
            .expect("run admission");
        bind_exact_run_steering_receiver(&store, &session.id, run_id, &receiver)
            .expect("install exact receiver");
        drop(receiver);

        assert!(handle
            .submit("persist even when delivery loses the race")
            .expect_err("closed channel")
            .contains("persisted"));
        let persisted = store.load(&session.id).expect("delivery failure evidence");
        let input_index = persisted
            .events
            .iter()
            .position(|event| event.kind == "steering_input")
            .expect("steering input");
        let failure_index = persisted
            .events
            .iter()
            .position(|event| event.kind == "steering_delivery_failed")
            .expect("delivery failure");
        assert!(input_index < failure_index);
        assert_eq!(
            persisted.events[failure_index].details["reason"],
            "receiver_closed"
        );
    }

    #[test]
    fn provider_failures_block_the_bound_plan_and_classify_the_run_as_failed() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let mut session = store.create_session_with_id("provider-failure");
        let mut plan = pending_plan("continue", "call the model");
        plan.approve();
        let plan_id = plan.id.clone();
        session.plan = Some(plan);
        store.save(&mut session).unwrap();

        let failure = "llm_stream_failed";
        for failure in [failure, "compression_failed"] {
            block_active_plan_for_failure(&store, &session.id, Some(&plan_id), "continue", failure)
                .unwrap();
            let plan = store.load(&session.id).unwrap().plan.unwrap();
            assert_eq!(plan.steps[plan.current_step_index].status, "Blocked");
            assert_eq!(plan.outcome.as_deref(), Some(failure));
            assert!(is_agent_failure_outcome(failure));
        }

        let summary = AgentRunSummary {
            session_id: session.id,
            run_id: String::new(),
            steps_taken: 1,
            last_message: None,
            tool_call_count: 0,
            final_state: AgentState::Done,
            outcome: failure.to_string(),
            failure: None,
            bound_reached: false,
            trace: Vec::new(),
        };
        assert!(summary.is_failure());
        assert!(!AgentRunSummary {
            outcome: "completed".to_string(),
            ..summary
        }
        .is_failure());

        let mut config = crate::config::NibConfig::default();
        config.llm.providers.insert(
            "anthropic".to_string(),
            ProviderEntry {
                model: "fixture-model".to_string(),
                api_key: Some("inactive-provider-secret".to_string()),
                ..ProviderEntry::default()
            },
        );
        let redacted = redact_provider_failure(
            &config,
            LlmError::local(
                LlmErrorClass::Protocol,
                LlmErrorPhase::Stream,
                format!("inactive-provider-secret {}", "x".repeat(16 * 1024)),
            ),
        );
        assert!(!redacted.contains("inactive-provider-secret"));
        assert!(redacted.contains("[REDACTED]"));
        assert!(redacted.to_string().ends_with("..."));
        assert!(redacted.len() <= 8 * 1024);
    }

    #[test]
    fn next_user_turn_after_llm_failure_does_not_create_assistant_content() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session_with_id("provider-failure-next-turn");
        store
            .try_append_message(&session.id, "user", "first request")
            .expect("first user turn");
        store
            .record_event(
                &session.id,
                "reconciliation",
                json!({
                    "outcome": "llm_stream_failed",
                    "continue": false,
                    "failure": {
                        "class": "transport",
                        "phase": "stream",
                        "retry": "retryable",
                    },
                }),
            )
            .expect("persist structured failure");

        prepare_user_turn(
            &store,
            &session.id,
            "second request",
            std::path::Path::new("."),
        )
        .expect("accept the next user turn");

        let persisted = store.load(&session.id).expect("persisted session");
        assert_eq!(
            persisted
                .messages
                .iter()
                .map(|message| (message.role.as_str(), message.content.as_str()))
                .collect::<Vec<_>>(),
            [("user", "first request"), ("user", "second request"),]
        );
        assert!(persisted.messages.iter().all(|message| {
            !message.content.contains("Previous run reconciled")
                && !message.content.contains("LLM-")
        }));
    }

    #[test]
    fn next_user_turn_after_local_reconciliation_does_not_create_assistant_content() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session_with_id("local-reconciliation-next-turn");
        store
            .try_append_message(&session.id, "user", "interrupted request")
            .expect("first user turn");
        store
            .record_event(
                &session.id,
                "reconciliation",
                json!({"outcome": "cancelled_by_user", "continue": false}),
            )
            .expect("persist local reconciliation");

        prepare_user_turn(
            &store,
            &session.id,
            "replacement request",
            std::path::Path::new("."),
        )
        .expect("accept replacement turn");

        let persisted = store.load(&session.id).expect("persisted session");
        assert_eq!(
            persisted
                .messages
                .iter()
                .map(|message| (message.role.as_str(), message.content.as_str()))
                .collect::<Vec<_>>(),
            [
                ("user", "interrupted request"),
                ("user", "replacement request"),
            ]
        );
    }

    #[test]
    fn prepare_user_turn_stores_structured_path_attachments() {
        let dir = tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/lib.rs"), "fn attached() {}").expect("file");
        let store = SessionStore::new(dir.path());
        let session = store.create_session();
        prepare_user_turn(&store, &session.id, "inspect @src/lib.rs", dir.path()).expect("attach");
        let persisted = store.load(&session.id).expect("session");
        let user = persisted
            .messages
            .iter()
            .find(|message| message.role == "user")
            .expect("user turn");
        assert_eq!(user.content, "inspect @src/lib.rs");
        assert!(!user.content.contains("fn attached"));
        assert_eq!(user.attachments.len(), 1);
        assert_eq!(user.attachments[0].path, "src/lib.rs");
    }

    #[test]
    #[serial_test::serial]
    fn provider_failure_persistence_redacts_inactive_environment_credentials() {
        const SECRET: &str = "inactive/env-provider-secret";
        let _environment = EnvironmentGuard::set("ANTHROPIC_API_KEY", SECRET);
        let config = crate::config::NibConfig::default();
        let error = LlmError::local(
            LlmErrorClass::Protocol,
            LlmErrorPhase::Stream,
            "request failed for model-inactive%2Fenv-provider-secret",
        );

        let redacted = redact_provider_failure(&config, error);

        assert_eq!(redacted, "request failed for model-[REDACTED]");
        assert!(!redacted.contains(SECRET));
        assert!(!redacted.contains("inactive%2Fenv-provider-secret"));
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
    async fn explicit_compaction_uses_exact_run_identity_without_synthetic_messages() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        store
            .try_append_message(&session.id, "user", "retain this compact fact")
            .unwrap();
        store
            .try_append_message(&session.id, "assistant", "retained answer")
            .unwrap();
        let before = store.load(&session.id).unwrap().messages;
        let run_id = "cdef0123456789abcdef0123456789ab";
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(16);

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "explicit context compression",
            AgentLoopConfig {
                mode: "compact".to_string(),
                run_id: Some(run_id.to_string()),
                stream_tx: Some(stream_tx),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "context_compacted");
        assert_eq!(summary.run_id, run_id);
        assert_eq!(summary.last_message, None);
        let after = store.load(&session.id).unwrap();
        assert_eq!(after.messages, before);
        assert!(after.summary.is_some());
        assert_eq!(
            after
                .events
                .iter()
                .filter(|event| event.kind == "run_started" && event.details["run_id"] == run_id)
                .count(),
            1
        );
        assert_eq!(
            after
                .events
                .iter()
                .filter(|event| event.kind == "run_terminal" && event.details["run_id"] == run_id)
                .count(),
            1
        );
        assert_eq!(
            after
                .events
                .iter()
                .filter(|event| event.kind == "compression")
                .count(),
            1
        );
        let events = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(events
            .iter()
            .any(|event| matches!(event, StreamEvent::Compression { .. })));
        assert!(events.iter().any(
            |event| matches!(event, StreamEvent::End(reason) if reason == "context_compacted")
        ));
    }

    #[tokio::test]
    async fn exact_run_steering_is_rejected_for_explicit_compaction() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        let run_id = "bcdef0123456789abcdef0123456789a";
        let (steering, receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("create uninstalled steering channel");

        let error = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "",
            AgentLoopConfig {
                mode: "compact".to_string(),
                run_id: Some(run_id.to_string()),
                steering: Some(receiver),
                ..Default::default()
            },
        )
        .await
        .expect_err("compact mode must reject exact-run steering");

        assert!(error.contains("not supported for agent mode: compact"));
        assert!(steering
            .submit("must never be accepted by maintenance")
            .expect_err("uninstalled compact steering")
            .contains("not installed"));
        let persisted = store.load(&session.id).expect("terminal compact rejection");
        assert!(persisted.events.iter().any(|event| {
            event.kind == "run_terminal"
                && event.details["run_id"] == run_id
                && event.details["outcome"] == "local_error"
        }));
        assert!(!persisted.events.iter().any(|event| {
            matches!(
                event.kind.as_str(),
                "steering_channel_bound" | "steering_admission" | "steering_input" | "compression"
            )
        }));
    }

    #[tokio::test]
    async fn committed_compaction_cannot_be_reclassified_by_blocked_presentation_cancellation() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        store
            .try_append_message(&session.id, "user", "retain committed compact fact")
            .unwrap();
        store
            .try_append_message(&session.id, "assistant", "retain committed answer")
            .unwrap();
        let cancellation = CancellationSignal::new();
        let run_id = "def0123456789abcdef0123456789abc";
        // The initial Compression state fills this channel. No receiver is drained,
        // reproducing the presentation backpressure that formerly opened a window
        // between summary commit and operation terminalization.
        let (stream_tx, _stream_rx) = tokio::sync::mpsc::channel(1);
        let project_root = dir.path().to_path_buf();
        let session_id = session.id.clone();
        let run_cancellation = cancellation.clone();
        let handle = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                "",
                AgentLoopConfig {
                    mode: "compact".to_string(),
                    run_id: Some(run_id.to_string()),
                    stream_tx: Some(stream_tx),
                    cancellation: Some(run_cancellation),
                    ..Default::default()
                },
            )
            .await
        });

        loop {
            let persisted = store.load(&session.id).unwrap();
            if persisted
                .events
                .iter()
                .any(|event| event.kind == "compression")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
        let summary = handle.await.unwrap().unwrap();
        assert_eq!(summary.outcome, "context_compacted");
        let persisted = store.load(&session.id).unwrap();
        assert!(persisted.events.iter().any(|event| {
            event.kind == "compression_request_terminal"
                && event.details["run_id"] == run_id
                && event.details["outcome"] == "context_compacted"
        }));
        assert!(!persisted.events.iter().any(|event| {
            event.kind == "run_terminal"
                && event.details["run_id"] == run_id
                && event.details["outcome"] == "cancelled_by_user"
        }));
    }

    #[tokio::test]
    async fn interactive_profile_binding_prevents_compaction_drift_after_default_changes() {
        let dir = tempdir().unwrap();
        let mut config = crate::config::NibConfig {
            llm: mock_config(),
            ..Default::default()
        };
        config.profiles.default = "profile-a".to_string();
        config.profiles.active = vec![
            crate::config::ProfileConfig {
                id: "profile-a".to_string(),
                root: PathBuf::from("."),
                state_dir: Some(PathBuf::from(".nib/profiles/profile-a")),
                ..Default::default()
            },
            crate::config::ProfileConfig {
                id: "profile-b".to_string(),
                root: PathBuf::from("."),
                state_dir: Some(PathBuf::from(".nib/profiles/profile-b")),
                ..Default::default()
            },
        ];
        crate::config::save_nib_config_full(dir.path(), &mut config).unwrap();
        let profile_a = crate::interactive::resolve_interactive_profile_scope(dir.path()).unwrap();
        let profile_a_id = profile_a.profile_id().to_string();
        let store_a = profile_a.into_session_store();
        let session_id = "coincident-compaction-session";
        store_a.create_session_with_id(session_id);
        store_a
            .try_append_message(session_id, "user", "profile A private context")
            .unwrap();
        store_a
            .try_append_message(session_id, "assistant", "profile A private answer")
            .unwrap();

        config.profiles.default = "profile-b".to_string();
        crate::config::save_nib_config_full(dir.path(), &mut config).unwrap();
        let store_b = SessionStore::for_project(dir.path()).unwrap();
        store_b.create_session_with_id(session_id);
        let run_id = "ef0123456789abcdef0123456789abcd";

        let summary = run_agent_loop_for_profile(
            dir.path().to_path_buf(),
            &profile_a_id,
            store_a.sessions_dir(),
            session_id,
            "",
            AgentLoopConfig {
                mode: "compact".to_string(),
                run_id: Some(run_id.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "context_compacted");
        assert!(store_a.load(session_id).unwrap().summary.is_some());
        let untouched = store_b.load(session_id).unwrap();
        assert!(untouched.messages.is_empty());
        assert!(untouched.summary.is_none());
        assert!(!untouched
            .events
            .iter()
            .any(|event| event.details["run_id"] == run_id));
    }

    #[tokio::test]
    async fn explicit_compaction_does_not_recover_or_render_an_interrupted_chat_turn() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        store
            .try_append_message(&session.id, "user", "inspect safely")
            .unwrap();
        store
            .try_append_message(&session.id, "assistant", "normalized tool intent")
            .unwrap();
        record_provider_continuation_lifecycle(
            &store,
            &session.id,
            "provider_continuation_opened",
            "prior-run",
        )
        .unwrap();
        store
            .try_append_message(
                &session.id,
                "tool",
                &json!({"observations": [{"tool": "read_file", "success": true}]}).to_string(),
            )
            .unwrap();
        let before = store.load(&session.id).unwrap();

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "",
            AgentLoopConfig {
                mode: "compact".to_string(),
                run_id: Some("1234567890abcdef1234567890abcdef".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            summary.outcome.as_str(),
            "context_compacted" | "context_unchanged"
        ));
        let after = store.load(&session.id).unwrap();
        assert_eq!(after.messages, before.messages);
        assert_eq!(
            after
                .events
                .iter()
                .filter(|event| event.kind == "provider_continuation_interrupted")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn pre_cancelled_explicit_compaction_reconciles_without_provider_or_chat_mutation() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        store
            .try_append_message(&session.id, "user", "must remain raw")
            .unwrap();
        let before = store.load(&session.id).unwrap().messages;
        let cancellation = CancellationSignal::new();
        cancellation.cancel();
        let run_id = "234567890abcdef1234567890abcdef1";

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "",
            AgentLoopConfig {
                mode: "compact".to_string(),
                run_id: Some(run_id.to_string()),
                cancellation: Some(cancellation),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "cancelled_by_user");
        let after = store.load(&session.id).unwrap();
        assert_eq!(after.messages, before);
        assert!(after.summary.is_none());
        assert!(!after.events.iter().any(|event| {
            matches!(event.kind.as_str(), "compression" | "compression_requested")
        }));
        assert_eq!(
            after
                .events
                .iter()
                .filter(|event| event.kind == "run_terminal"
                    && event.details["run_id"] == run_id
                    && event.details["outcome"] == "cancelled_by_user")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn explicit_compaction_configuration_failure_is_safe_and_terminal() {
        let dir = tempdir().unwrap();
        let config = LlmConfig {
            active_provider: Some("meta".to_string()),
            providers: HashMap::from([(
                "meta".to_string(),
                ProviderEntry {
                    model: "meta-test-model".to_string(),
                    api_key: Some("private-test-key".to_string()),
                    ..ProviderEntry::default()
                },
            )]),
            ..Default::default()
        };
        save_config(dir.path(), &config).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        store
            .try_append_message(
                &session.id,
                "user",
                "compress without a configured endpoint",
            )
            .unwrap();
        let before = store.load(&session.id).unwrap().messages;
        let run_id = "34567890abcdef1234567890abcdef12";

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "",
            AgentLoopConfig {
                mode: "compact".to_string(),
                run_id: Some(run_id.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "configuration_failed");
        let report = summary.user_failure_report().expect("safe failure report");
        assert!(!report.contains("private-test-key"));
        assert!(summary.failure.is_some());
        let after = store.load(&session.id).unwrap();
        assert_eq!(after.messages, before);
        assert!(after.summary.is_none());
        assert_eq!(
            after
                .events
                .iter()
                .filter(|event| event.kind == "run_terminal"
                    && event.details["run_id"] == run_id
                    && event.details["outcome"] == "configuration_failed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn explicit_compaction_provider_rejection_is_safe_and_terminal() {
        let dir = tempdir().unwrap();
        let remote_secret = "private-explicit-compression-provider-detail";
        let private_key = "private-explicit-compression-key";
        let (base_url, request_rx) = crate::llm::test_support::serve_once(
            "400 Bad Request",
            "application/json",
            json!({"error": {"message": remote_secret}}).to_string(),
        );
        let config = LlmConfig {
            active_provider: Some("openai".to_string()),
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: "fixture-model".to_string(),
                    api_key: Some(private_key.to_string()),
                    base_url: Some(base_url),
                    api: Some(crate::config::LlmApiMode::ChatCompletions),
                    ..ProviderEntry::default()
                },
            )]),
            ..Default::default()
        };
        save_config(dir.path(), &config).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        store
            .try_append_message(&session.id, "user", "compress against the fixture")
            .unwrap();
        store
            .try_append_message(&session.id, "assistant", "raw answer remains")
            .unwrap();
        let before = store.load(&session.id).unwrap().messages;
        let run_id = "4567890abcdef1234567890abcdef123";

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "",
            AgentLoopConfig {
                mode: "compact".to_string(),
                run_id: Some(run_id.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let request = request_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("provider request");
        assert!(request.starts_with("POST /chat/completions "));
        assert_eq!(summary.outcome, "compression_failed");
        let report = summary.user_failure_report().expect("safe provider report");
        assert!(!report.contains(remote_secret));
        assert!(!report.contains(private_key));
        let after = store.load(&session.id).unwrap();
        assert_eq!(after.messages, before);
        assert!(after.summary.is_none());
        let persisted = serde_json::to_string(&after).unwrap();
        assert!(!persisted.contains(remote_secret));
        assert!(!persisted.contains(private_key));
        assert_eq!(
            after
                .events
                .iter()
                .filter(|event| event.kind == "run_terminal"
                    && event.details["run_id"] == run_id
                    && event.details["outcome"] == "compression_failed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn exact_run_identity_has_audited_mock_lifecycle_and_replay_fails_closed() {
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
                run_id: Some("0123456789abcdef0123456789abcdef".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.final_state, AgentState::Done);
        assert_eq!(summary.run_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(summary.outcome, "completed");
        assert!(summary.tool_call_count >= 1);
        assert!(summary.trace.contains(&"plan_approval".to_string()));
        assert!(summary.trace.contains(&"tool_execute".to_string()));
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(
            loaded
                .events
                .iter()
                .filter(|event| event.kind == "run_started")
                .count(),
            1
        );
        for kind in [
            "run_started",
            "provider_continuation_opened",
            "provider_continuation_closed",
            "run_terminal",
        ] {
            assert!(loaded
                .events
                .iter()
                .filter(|event| event.kind == kind)
                .all(|event| event.details["run_id"] == "0123456789abcdef0123456789abcdef"));
        }
        assert_eq!(
            loaded
                .events
                .iter()
                .filter(|event| event.kind == "run_terminal")
                .count(),
            1
        );
        loaded.validate_message_sequence().unwrap();
        assert!(loaded
            .events
            .iter()
            .filter(|event| matches!(event.kind.as_str(), "approval_required" | "tool_started"))
            .all(|event| event.details.get("arguments").is_none()));
        assert!(loaded.plan.unwrap().is_complete());

        let before_replay = store.load(&session.id).unwrap();
        let replay = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "must not execute",
            AgentLoopConfig {
                auto_approve: true,
                run_id: Some("0123456789abcdef0123456789abcdef".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("replayed run ID must fail closed");
        assert!(replay.contains("duplicate or replayed run_id"));
        let after_replay = store.load(&session.id).unwrap();
        assert_eq!(before_replay.messages, after_replay.messages);
        assert_eq!(before_replay.tool_calls, after_replay.tool_calls);
        assert_eq!(before_replay.events, after_replay.events);
    }

    #[tokio::test]
    async fn exact_run_identity_pre_cancel_records_one_matching_terminal() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        let cancellation = CancellationSignal::new();
        assert!(cancellation.cancel());
        let run_id = "fedcba9876543210fedcba9876543210";

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "must not reach the provider",
            AgentLoopConfig {
                cancellation: Some(cancellation),
                run_id: Some(run_id.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("pre-cancelled run reconciles");

        assert_eq!(summary.run_id, run_id);
        assert_eq!(summary.outcome, "cancelled_by_user");
        let persisted = store.load(&session.id).expect("cancelled session");
        assert!(persisted.messages.is_empty());
        let starts = persisted
            .events
            .iter()
            .filter(|event| event.kind == "run_started")
            .collect::<Vec<_>>();
        let terminals = persisted
            .events
            .iter()
            .filter(|event| event.kind == "run_terminal")
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 1);
        assert_eq!(terminals.len(), 1);
        assert_eq!(starts[0].details["run_id"], run_id);
        assert_eq!(terminals[0].details["run_id"], run_id);
        assert_eq!(terminals[0].details["outcome"], "cancelled_by_user");
        assert!(starts[0].index < terminals[0].index);
    }

    #[tokio::test]
    async fn exact_run_steering_binding_failure_terminalizes_the_admitted_run_once() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let target = store.create_session_with_id("steering-binding-target");
        let foreign = store.create_session_with_id("steering-binding-foreign");
        let run_id = "fedcba9876543210fedcba9876543210";
        let (_handle, foreign_receiver) =
            exact_run_steering_channel(store.clone(), foreign.id.clone(), run_id, "plain")
                .expect("foreign steering receiver");

        let error = run_agent_loop(
            dir.path().to_path_buf(),
            &target.id,
            "must not reach provider execution",
            AgentLoopConfig {
                run_id: Some(run_id.to_string()),
                steering: Some(foreign_receiver),
                ..Default::default()
            },
        )
        .await
        .expect_err("wrong-session receiver must fail the admitted run");
        assert!(error.contains("not bound to the exact active run"));

        let persisted = store.load(&target.id).expect("terminalized target session");
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| {
                    event.kind == "run_started" && event.details["run_id"] == run_id
                })
                .count(),
            1
        );
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| {
                    event.kind == "run_terminal"
                        && event.details["run_id"] == run_id
                        && event.details["outcome"] == "local_error"
                })
                .count(),
            1
        );
        assert!(!persisted
            .events
            .iter()
            .any(|event| event.kind == "steering_channel_bound"));
        assert!(persisted.messages.is_empty());
        assert!(persisted.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn pre_cancelled_restart_reconciles_open_provider_continuation_before_cancellation() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        store
            .try_append_message(&session.id, "user", "inspect safely")
            .unwrap();
        store
            .try_append_message(&session.id, "assistant", "normalized tool intent")
            .unwrap();
        record_provider_continuation_lifecycle(
            &store,
            &session.id,
            "provider_continuation_opened",
            "prior-run",
        )
        .unwrap();
        store
            .record_event(
                &session.id,
                "tool_completed",
                json!({"tool_name": "read_file", "success": true}),
            )
            .unwrap();
        store
            .try_append_message(
                &session.id,
                "tool",
                &json!({"observations": [{"tool": "read_file", "success": true}]}).to_string(),
            )
            .unwrap();
        let cancellation = CancellationSignal::new();
        assert!(cancellation.cancel());

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "must not reach the provider",
            AgentLoopConfig {
                cancellation: Some(cancellation),
                run_id: Some("abcdef0123456789abcdef0123456789".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("pre-cancelled restart reconciles");

        assert_eq!(summary.outcome, "cancelled_by_user");
        let persisted = store.load(&session.id).expect("reconciled session");
        persisted.validate_message_sequence().unwrap();
        let boundary = persisted.messages.last().expect("continuation boundary");
        assert_eq!(boundary.role, "assistant");
        assert_eq!(
            serde_json::from_str::<Value>(&boundary.content).unwrap(),
            json!({
                "type": "provider_continuation_boundary",
                "outcome": "provider_continuation_interrupted",
            })
        );
        let interrupted = persisted
            .events
            .iter()
            .find(|event| event.kind == "provider_continuation_interrupted")
            .expect("interrupted continuation event");
        let cancel_requested = persisted
            .events
            .iter()
            .find(|event| event.kind == "cancel_requested")
            .expect("cancel request event");
        assert!(interrupted.index < cancel_requested.index);
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| event.kind == "provider_continuation_interrupted")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn continuation_recovery_failure_terminalizes_the_admitted_run_once() {
        fn fail_recovery(_store: &SessionStore, _session_id: &str) -> Result<bool, String> {
            Err("injected continuation recovery persistence failure".to_string())
        }

        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        let runtime = prepare_agent_loop_runtime(dir.path(), None, None, None, None).unwrap();
        let lease = runtime
            .session_store
            .try_acquire_run_lease(&session.id)
            .unwrap();
        let cancellation = CancellationSignal::new();
        assert!(cancellation.cancel());
        let run_id = "0123abcdef4567890123abcdef456789";

        let error = run_agent_loop_with_runtime_and_recovery(
            runtime,
            &session.id,
            "must not reach cancellation or provider execution",
            AgentLoopConfig {
                cancellation: Some(cancellation),
                run_id: Some(run_id.to_string()),
                ..Default::default()
            },
            lease,
            fail_recovery,
        )
        .await
        .expect_err("injected recovery failure must fail the run");
        assert!(error.contains("injected continuation recovery persistence failure"));

        let persisted = store.load(&session.id).expect("terminalized session");
        let starts = persisted
            .events
            .iter()
            .filter(|event| event.kind == "run_started" && event.details["run_id"] == run_id)
            .count();
        let terminals = persisted
            .events
            .iter()
            .filter(|event| {
                event.kind == "run_terminal"
                    && event.details["run_id"] == run_id
                    && event.details["outcome"] == "local_error"
            })
            .count();
        assert_eq!(starts, 1);
        assert_eq!(terminals, 1);
        assert!(persisted.messages.is_empty());
        assert!(persisted.tool_calls.is_empty());
        assert!(persisted.events.iter().all(|event| {
            event.kind != "cancel_requested" && !event.kind.starts_with("provider_continuation_")
        }));
    }

    #[tokio::test]
    async fn llm_configuration_failure_reconciles_as_typed_non_network_evidence() {
        let dir = tempdir().unwrap();
        save_config(dir.path(), &mock_config()).unwrap();
        let store = SessionStore::for_project(dir.path()).unwrap();
        let session = store.create_session();
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(16);

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "inspect configuration",
            AgentLoopConfig {
                provider: Some("missing-provider".to_string()),
                stream_tx: Some(stream_tx),
                ..Default::default()
            },
        )
        .await
        .expect("configuration failures reconcile safely");

        assert_eq!(summary.outcome, "configuration_failed");
        let failure = summary
            .failure
            .as_ref()
            .expect("typed configuration failure");
        assert_eq!(failure.class, LlmErrorClass::Configuration);
        assert_eq!(failure.phase, LlmErrorPhase::Configuration);
        assert_eq!(failure.retry, crate::llm::RetryDisposition::NotAttempted);
        assert_eq!(failure.provider, "missing-provider");
        assert!(summary
            .user_failure_report()
            .expect("configuration report")
            .contains("LLM-CONFIG"));

        let persisted = store.load(&session.id).expect("reconciled session");
        assert!(persisted.messages.is_empty());
        let reconciliation = persisted
            .events
            .iter()
            .find(|event| event.kind == "reconciliation")
            .expect("reconciliation event");
        assert_eq!(reconciliation.details["outcome"], "configuration_failed");
        assert_eq!(reconciliation.details["failure"]["class"], "configuration");

        let events = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(
            event,
            StreamEvent::Failure {
                failure,
                session_id: Some(id),
            } if failure.class == LlmErrorClass::Configuration && id == &session.id
        )));
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
    #[serial_test::serial]
    async fn exact_run_steering_during_a_provider_response_suppresses_its_uncommitted_tool_proposal(
    ) {
        let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
        let directory = tempdir().expect("project");
        save_config(directory.path(), &mock_config()).expect("mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let goal = "exact run steering response smoke";
        let mut session = store.create_session_with_id("steering-response-suppression");
        let mut plan = pending_plan(goal, "do not execute the obsolete proposal");
        plan.approve();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved plan");
        let run_id = "0123456789abcdef0123456789abcdef";
        let (steering, steering_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("steering channel");
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);
        let project_root = directory.path().to_path_buf();
        let session_id = session.id.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                goal,
                AgentLoopConfig {
                    max_steps: 5,
                    auto_approve: true,
                    run_id: Some(run_id.to_string()),
                    steering: Some(steering_receiver),
                    stream_tx: Some(stream_tx),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    stream_rx.recv().await,
                    Some(StreamEvent::StateTransition { state }) if state == AgentState::InspectLlm.as_str()
                ) {
                    break;
                }
            }
        })
        .await
        .expect("run reached the provider request");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            steering
                .submit("replacement steering marker; answer without tools")
                .expect("durable steering"),
            1
        );

        let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("steered run completed")
            .expect("agent task joined")
            .expect("agent run succeeded");
        assert_eq!(summary.outcome, "completed");
        assert_eq!(summary.tool_call_count, 0);
        assert_eq!(
            summary.last_message.as_deref(),
            Some("Final answer: replacement steering marker observed.")
        );

        let persisted = store.load(&session.id).expect("steered session");
        assert!(persisted
            .tool_calls
            .iter()
            .all(|record| record.tool_name.as_deref() != Some("list_directory")));
        assert!(!persisted.events.iter().any(|event| matches!(
            event.kind.as_str(),
            "tool_requested" | "tool_started" | "tool_completed"
        )));
        assert!(persisted.events.iter().any(|event| {
            event.kind == "provider_continuation_abandoned_by_steering"
                && event.details["run_id"] == run_id
        }));
        let input_index = persisted
            .events
            .iter()
            .position(|event| event.kind == "steering_input")
            .expect("persisted steering input");
        let intake_index = persisted
            .events
            .iter()
            .position(|event| event.kind == "steering_intake")
            .expect("persisted steering intake");
        assert!(input_index < intake_index);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exact_run_steering_rejects_during_a_final_provider_response_without_replacement_budget(
    ) {
        let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
        let directory = tempdir().expect("project");
        save_config(directory.path(), &mock_config()).expect("mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let goal = "exact run steering response smoke final response";
        let mut session = store.create_session_with_id("steering-final-response");
        let mut plan = pending_plan(goal, "complete the only provider turn");
        plan.approve();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved plan");
        let run_id = "11111111111111111111111111111111";
        let (steering, steering_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("steering channel");
        let project_root = directory.path().to_path_buf();
        let session_id = session.id.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                goal,
                AgentLoopConfig {
                    max_steps: 1,
                    auto_approve: true,
                    run_id: Some(run_id.to_string()),
                    steering: Some(steering_receiver),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if store
                    .load(&session.id)
                    .expect("request state")
                    .events
                    .iter()
                    .any(|event| {
                        event.kind == "steering_admission"
                            && event.details["phase"] == "final_provider_request"
                            && event.details["open"] == false
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("final provider request closes steering admission");
        assert!(steering
            .submit("cannot promise a replacement request")
            .expect_err("final response steering must fail synchronously")
            .contains("current run boundary"));

        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("final provider run completed")
            .expect("agent task joined")
            .expect("agent run succeeded");
        let persisted = store.load(&session.id).expect("terminal session");
        assert!(!persisted
            .events
            .iter()
            .any(|event| event.kind == "steering_input"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exact_run_steering_rejects_during_final_planning_without_replacement_budget() {
        let _interactive_smoke = EnvironmentGuard::set("NIB_ENABLE_INTERACTIVE_SMOKE", "1");
        let directory = tempdir().expect("project");
        save_config(directory.path(), &mock_config()).expect("mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let session = store.create_session_with_id("steering-final-planning");
        let goal = "interactive queue smoke final planning turn";
        let run_id = "22222222222222222222222222222222";
        let (steering, steering_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
                .expect("steering channel");
        let project_root = directory.path().to_path_buf();
        let session_id = session.id.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                goal,
                AgentLoopConfig {
                    max_steps: 1,
                    approval_handler: Some(Arc::new(DenyApproval)),
                    run_id: Some(run_id.to_string()),
                    steering: Some(steering_receiver),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if store
                    .load(&session.id)
                    .expect("planning state")
                    .events
                    .iter()
                    .any(|event| {
                        event.kind == "steering_admission"
                            && event.details["phase"] == "final_planning_request"
                            && event.details["open"] == false
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("final planning request closes steering admission");
        assert!(steering
            .submit("cannot promise replanning")
            .expect_err("final planning steering must fail synchronously")
            .contains("current run boundary"));

        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("final planning run completed")
            .expect("agent task joined")
            .expect("agent run succeeded");
        let persisted = store.load(&session.id).expect("terminal session");
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| event.kind == "plan_generated")
                .count(),
            1
        );
        assert!(!persisted
            .events
            .iter()
            .any(|event| event.kind == "steering_input"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exact_run_steering_stays_closed_when_the_final_provider_turn_starts_a_tool() {
        let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
        let directory = tempdir().expect("project");
        save_config(directory.path(), &mock_config()).expect("mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let goal = "exact run steering tool smoke final turn";
        let mut session = store.create_session_with_id("steering-final-tool");
        let mut plan = pending_plan(goal, "execute the final-turn tool proposal");
        plan.approve();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved plan");
        let run_id = "33333333333333333333333333333333";
        let (steering, steering_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("steering channel");
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);
        let project_root = directory.path().to_path_buf();
        let session_id = session.id.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                goal,
                AgentLoopConfig {
                    max_steps: 1,
                    auto_approve: true,
                    run_id: Some(run_id.to_string()),
                    steering: Some(steering_receiver),
                    stream_tx: Some(stream_tx),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    stream_rx.recv().await,
                    Some(StreamEvent::ToolStarted { tool_name }) if tool_name == "run_terminal"
                ) {
                    break;
                }
            }
        })
        .await
        .expect("final-turn tool started");
        assert!(steering
            .submit("cannot be applied after the final-turn tool")
            .expect_err("post-tool steering needs replacement budget")
            .contains("current run boundary"));

        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("final tool run completed")
            .expect("agent task joined")
            .expect("agent run succeeded");
        let persisted = store.load(&session.id).expect("terminal session");
        assert!(!persisted
            .events
            .iter()
            .any(|event| event.kind == "steering_input"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exact_run_steering_after_tool_start_applies_before_the_next_provider_request() {
        let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
        let directory = tempdir().expect("project");
        save_config(directory.path(), &mock_config()).expect("mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let goal = "exact run steering tool smoke";
        let mut session = store.create_session_with_id("steering-after-tool-start");
        let mut plan = pending_plan(goal, goal);
        plan.approve();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved plan");
        let run_id = "fedcba9876543210fedcba9876543210";
        let (steering, steering_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
                .expect("steering channel");
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);
        let project_root = directory.path().to_path_buf();
        let session_id = session.id.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                goal,
                AgentLoopConfig {
                    max_steps: 5,
                    auto_approve: true,
                    run_id: Some(run_id.to_string()),
                    steering: Some(steering_receiver),
                    stream_tx: Some(stream_tx),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    stream_rx.recv().await,
                    Some(StreamEvent::ToolStarted { tool_name }) if tool_name == "run_terminal"
                ) {
                    loop {
                        if store
                            .load(&session.id)
                            .expect("tool lifecycle")
                            .events
                            .iter()
                            .any(|event| event.kind == "tool_started")
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                    break;
                }
            }
        })
        .await
        .expect("tool started");
        steering
            .submit("replacement steering marker after the started tool")
            .expect("durable post-tool steering");

        let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("post-tool steered run completed")
            .expect("agent task joined")
            .expect("agent run succeeded");
        assert_eq!(summary.outcome, "completed");
        assert_eq!(summary.tool_call_count, 1);
        assert_eq!(
            summary.last_message.as_deref(),
            Some("Final answer: replacement steering marker observed.")
        );
        let persisted = store.load(&session.id).expect("post-tool steering state");
        assert_eq!(
            persisted
                .tool_calls
                .iter()
                .filter(|record| record.tool_name.as_deref() == Some("run_terminal"))
                .count(),
            1
        );
        let event_index = |kind: &str| {
            persisted
                .events
                .iter()
                .position(|event| event.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind} event"))
        };
        assert!(event_index("tool_started") < event_index("steering_input"));
        assert!(event_index("steering_input") < event_index("tool_completed"));
        assert!(event_index("tool_completed") < event_index("steering_intake"));
        assert!(persisted.events.iter().any(|event| {
            event.kind == "provider_continuation_abandoned_by_steering"
                && event.details["run_id"] == run_id
        }));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exact_run_steering_drained_in_compression_abandons_the_tool_continuation() {
        #[cfg(windows)]
        const HOSTED_PROGRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
        #[cfg(not(windows))]
        const HOSTED_PROGRESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
        let directory = tempdir().expect("project");
        save_config(directory.path(), &mock_config()).expect("mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let goal = "exact run steering tool smoke compression race";
        let mut session = store.create_session_with_id("steering-compression-race");
        let mut plan = pending_plan(goal, goal);
        plan.approve();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved plan");
        let run_id = "44444444444444444444444444444444";
        let (steering, steering_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("steering channel");
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(1);
        let project_root = directory.path().to_path_buf();
        let session_id = session.id.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                goal,
                AgentLoopConfig {
                    max_steps: 5,
                    auto_approve: true,
                    run_id: Some(run_id.to_string()),
                    steering: Some(steering_receiver),
                    stream_tx: Some(stream_tx),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(HOSTED_PROGRESS_TIMEOUT, async {
            loop {
                match stream_rx.recv().await {
                    Some(StreamEvent::ToolCompleted { tool_name, .. })
                        if tool_name == "run_terminal" =>
                    {
                        break
                    }
                    None => panic!("stream closed before the first terminal tool completed"),
                    _ => {}
                }
            }
        })
        .await
        .expect("first tool completed");
        tokio::time::timeout(HOSTED_PROGRESS_TIMEOUT, async {
            loop {
                if store
                    .load(&session.id)
                    .expect("compression state")
                    .events
                    .iter()
                    .any(|event| {
                        event.kind == "state_transition"
                            && event.details["to"] == AgentState::Compression.as_str()
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("agent is blocked after BuildContext drain at Compression transition");
        steering
            .submit("replacement steering marker after BuildContext drain")
            .expect("late pre-request steering is durable");
        let drain = tokio::spawn(async move { while stream_rx.recv().await.is_some() {} });

        let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("compression-race run completed")
            .expect("agent task joined")
            .expect("agent run succeeded");
        drain.await.expect("stream drain joined");
        assert_eq!(
            summary.last_message.as_deref(),
            Some("Final answer: replacement steering marker observed.")
        );
        let persisted = store.load(&session.id).expect("steered session");
        assert!(persisted.events.iter().any(|event| {
            event.kind == "provider_continuation_abandoned_by_steering"
                && event.details["run_id"] == run_id
        }));
        assert!(persisted
            .events
            .iter()
            .any(|event| event.kind == "steering_intake"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exact_run_steering_reserves_the_final_turn_instead_of_automatic_compression() {
        let _steering_smoke = EnvironmentGuard::set("NIB_ENABLE_EXACT_STEERING_SMOKE", "1");
        let directory = tempdir().expect("project");
        let mut config = NibConfig {
            llm: mock_config(),
            ..NibConfig::default()
        };
        config.compression.enabled = true;
        config.compression.threshold = 0.000_1;
        config.compression.target_ratio = 0.000_05;
        save_nib_config_full(directory.path(), &mut config).expect("compression config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let goal = "exact run steering final turn smoke compression threshold";
        let mut session = store.create_session_with_id("steering-final-turn-compression");
        let mut plan = pending_plan(goal, "use the final model turn for the task");
        plan.approve();
        session.plan = Some(plan);
        store.save(&mut session).expect("approved plan");
        for (role, text) in [
            (
                "user",
                "historic user context that exceeds the tiny threshold",
            ),
            (
                "assistant",
                "historic assistant response retained for compression",
            ),
            (
                "user",
                "second historic user context for an eligible summary",
            ),
            (
                "assistant",
                "second historic assistant response before the active turn",
            ),
        ] {
            store
                .try_append_message(&session.id, role, text)
                .expect("historic message");
        }
        let run_id = "55555555555555555555555555555555";
        let (steering, steering_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "plain")
                .expect("steering channel");
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(1);
        let project_root = directory.path().to_path_buf();
        let session_id = session.id.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                goal,
                AgentLoopConfig {
                    max_steps: 1,
                    auto_approve: true,
                    run_id: Some(run_id.to_string()),
                    steering: Some(steering_receiver),
                    stream_tx: Some(stream_tx),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if store
                    .load(&session.id)
                    .expect("build state")
                    .events
                    .iter()
                    .any(|event| {
                        event.kind == "state_transition"
                            && event.details["to"] == AgentState::BuildContext.as_str()
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run blocks before the BuildContext steering drain");
        steering
            .submit("replacement steering marker on the reserved final turn")
            .expect("pre-request steering accepted");
        let drain = tokio::spawn(async move { while stream_rx.recv().await.is_some() {} });

        let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("reserved final turn completed")
            .expect("agent task joined")
            .expect("agent run succeeded");
        drain.await.expect("stream drain joined");
        assert_eq!(
            summary.last_message.as_deref(),
            Some("Final answer: replacement steering marker observed.")
        );
        let persisted = store.load(&session.id).expect("terminal session");
        assert!(persisted
            .events
            .iter()
            .any(|event| event.kind == "steering_intake"));
        assert!(!persisted
            .events
            .iter()
            .any(|event| event.kind == "compression"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exact_run_steering_supersedes_an_unapproved_plan_before_prompting() {
        let _interactive_smoke = EnvironmentGuard::set("NIB_ENABLE_INTERACTIVE_SMOKE", "1");
        let directory = tempdir().expect("project");
        save_config(directory.path(), &mock_config()).expect("mock config");
        let store = SessionStore::for_project(directory.path()).expect("session store");
        let session = store.create_session_with_id("steering-plan-supersession");
        let goal = "interactive queue smoke with a steerable plan";
        let run_id = "abcdef0123456789abcdef0123456789";
        let (steering, steering_receiver) =
            exact_run_steering_channel(store.clone(), session.id.clone(), run_id, "tui")
                .expect("steering channel");
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(64);
        let project_root = directory.path().to_path_buf();
        let session_id = session.id.clone();
        let run = tokio::spawn(async move {
            run_agent_loop(
                project_root,
                &session_id,
                goal,
                AgentLoopConfig {
                    max_steps: 5,
                    approval_handler: Some(Arc::new(DenyApproval)),
                    run_id: Some(run_id.to_string()),
                    steering: Some(steering_receiver),
                    stream_tx: Some(stream_tx),
                    ..Default::default()
                },
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    stream_rx.recv().await,
                    Some(StreamEvent::StateTransition { state }) if state == AgentState::Planning.as_str()
                ) {
                    break;
                }
            }
        })
        .await
        .expect("planner request started");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        steering
            .submit("regenerate the plan with the new verification constraint")
            .expect("durable plan steering");

        let summary = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("replanned run completed")
            .expect("agent task joined")
            .expect("agent run succeeded");
        assert_eq!(summary.outcome, "plan_approval_denied");
        let persisted = store.load(&session.id).expect("replanned session");
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| event.kind == "plan_generated")
                .count(),
            2
        );
        assert_eq!(
            persisted
                .events
                .iter()
                .filter(|event| event.kind == "approval_required")
                .count(),
            1,
            "the obsolete plan must never reach approval"
        );
        assert!(persisted
            .events
            .iter()
            .any(|event| event.kind == "plan_superseded_by_steering"));
        assert!(persisted
            .events
            .iter()
            .any(|event| event.kind == "steering_intake"));
        assert!(persisted
            .tool_calls
            .iter()
            .all(|record| { !matches!(record.tool_name.as_deref(), Some("list_directory")) }));
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
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(32);

        let summary = run_agent_loop(
            dir.path().to_path_buf(),
            &session.id,
            "explore RAW_APPROVAL_LIFECYCLE_SENTINEL",
            AgentLoopConfig {
                max_steps: 4,
                approval_handler: Some(Arc::new(DenyApproval)),
                stream_tx: Some(stream_tx),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome, "plan_approval_denied");
        assert_eq!(summary.tool_call_count, 0);
        let persisted = store.load(&session.id).unwrap();
        assert!(persisted
            .tool_calls
            .iter()
            .all(|call| call.tool_name.as_deref() == Some("daemon_curator")));
        let approval_event = persisted
            .events
            .iter()
            .find(|event| event.kind == "approval_required")
            .expect("approval event");
        assert!(approval_event.details.get("arguments").is_none());
        assert!(!approval_event
            .details
            .to_string()
            .contains("RAW_APPROVAL_LIFECYCLE_SENTINEL"));
        let stream = std::iter::from_fn(|| stream_rx.try_recv().ok()).collect::<Vec<_>>();
        assert!(stream
            .iter()
            .any(|event| matches!(event, StreamEvent::ApprovalRequired { tool_name } if tool_name == "approve_plan")));
        assert!(!format!("{stream:?}").contains("RAW_APPROVAL_LIFECYCLE_SENTINEL"));
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
        let messages_before_cancel = store
            .load(&session_id)
            .expect("session before cancellation")
            .messages;
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
        assert!(summary.last_message.is_none());
        assert_eq!(persisted.messages, messages_before_cancel);
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

    #[test]
    fn question_projection_requires_registry_schema_and_is_control_safe_and_bounded() {
        let invalid = json!({
            "question": "choose",
            "options": (0..21).map(|index| format!("option-{index}")).collect::<Vec<_>>()
        });
        assert!(crate::tools::executor::validate_registered_tool_arguments(
            "ask_question",
            &invalid,
        )
        .is_err());

        let secret = "active/credential".to_string();
        let valid = json!({
            "question": format!("mode {secret} active\\/credential\n\u{1b}[31m?"),
            "options": ["YWN0aXZlL2NyZWRlbnRpYWw=\r", "execute\t"]
        });
        crate::tools::executor::validate_registered_tool_arguments("ask_question", &valid)
            .expect("registry-valid question");
        let projected = safe_question_arguments(&valid, std::slice::from_ref(&secret));
        let encoded = serde_json::to_string(&projected).expect("safe question projection");
        assert!(!projected["question"]
            .as_str()
            .expect("question")
            .chars()
            .any(char::is_control));
        assert!(!encoded.contains(&secret));
        assert!(!encoded.contains(r"active\/credential"));
        assert!(!encoded.contains("YWN0aXZlL2NyZWRlbnRpYWw="));
        assert!(!encoded.contains("\\u001b"));
        assert!(encoded.len() < MAX_QUESTION_BYTES + 2 * MAX_QUESTION_OPTION_BYTES);
    }

    #[tokio::test]
    async fn question_execution_and_audit_persist_only_the_public_projection() {
        let directory = tempdir().expect("question audit directory");
        let store = SessionStore::at_dir(directory.path().join("sessions"));
        let session = store.create_session_with_id("question-audit");
        let secret = "active/credential".to_string();
        let raw_arguments = json!({
            "question": format!(
                "{secret} active\\/credential YWN0aXZlL2NyZWRlbnRpYWw= [2J {} QUESTION_PRIVATE_TAIL",
                "q".repeat(MAX_QUESTION_BYTES + 512),
            ),
            "options": [
                "active\\/credential\r",
                "YWN0aXZlL2NyZWRlbnRpYWw=\t",
            ],
        });
        let answer = Err(format!(
            "handler failed with {secret} active\\/credential YWN0aXZlL2NyZWRlbnRpYWw= [31m {} ANSWER_PRIVATE_TAIL",
            "e".repeat(MAX_QUESTION_BYTES + 512),
        ));
        let arguments = safe_question_execution_arguments(
            &raw_arguments,
            &answer,
            std::slice::from_ref(&secret),
        );
        let mut executor = ToolExecutor::new(
            directory.path().to_path_buf(),
            crate::config::ExecutionConfig::default(),
        )
        .with_session_store(store.clone())
        .with_sensitive_values([secret.clone()])
        .with_auto_approve(true);

        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "ask_question".to_string(),
                    arguments,
                    session_id: Some(session.id.clone()),
                    project_root: Some(directory.path().to_path_buf()),
                },
                Some(&session.id),
            )
            .await;

        assert!(!result.success);
        let persisted = store.load(&session.id).expect("question audit session");
        assert!(persisted
            .events
            .iter()
            .any(|event| event.kind == "tool_attempted"));
        assert_eq!(persisted.tool_calls.len(), 1);
        let audited_arguments = &persisted.tool_calls[0].arguments;
        assert!(audited_arguments["question"]
            .as_str()
            .is_some_and(|value| value.len() <= MAX_QUESTION_BYTES));
        assert!(audited_arguments["answer_error"]
            .as_str()
            .is_some_and(|value| value.len() <= MAX_QUESTION_BYTES));
        let public_value = json!({
            "output": result.output,
            "error": result.error,
            "session": persisted,
        });
        let public_surface =
            serde_json::to_string(&public_value).expect("serialize public question surfaces");
        for forbidden in [
            secret.as_str(),
            r"active\/credential",
            "YWN0aXZlL2NyZWRlbnRpYWw=",
            "QUESTION_PRIVATE_TAIL",
            "ANSWER_PRIVATE_TAIL",
            r"\u001b",
        ] {
            assert!(
                !public_surface.contains(forbidden),
                "public question surface contained {forbidden:?}"
            );
        }
        fn assert_public_strings_are_bounded(value: &Value) {
            match value {
                Value::Object(object) => {
                    for value in object.values() {
                        assert_public_strings_are_bounded(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        assert_public_strings_are_bounded(value);
                    }
                }
                Value::String(value) => {
                    assert!(value.len() <= MAX_PERSISTED_PROVIDER_MESSAGE_BYTES)
                }
                _ => {}
            }
        }
        assert_public_strings_are_bounded(&public_value);
    }
}
