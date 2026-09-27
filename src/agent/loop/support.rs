//! Agent loop internals.

use super::*;

pub(crate) const MAX_QUESTION_BYTES: usize = 20_000;
pub(crate) const MAX_QUESTION_OPTION_BYTES: usize = 1_000;
pub(crate) const MAX_PUBLIC_PROVIDER_CONTENT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_PUBLIC_PROVIDER_TOOL_CHUNK_BYTES: usize = 8 * 1024;
pub(crate) const MAX_PERSISTED_PROVIDER_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_STEERING_INPUT_BYTES: usize = 8 * 1024;
pub(crate) const MAX_STEERING_INPUTS_PER_RUN: usize = 32;
pub(crate) const MAX_STEERING_TOTAL_BYTES_PER_RUN: usize = 32 * 1024;
pub(crate) const MAX_IDENTICAL_FAILED_TOOL_BATCHES: u8 = 3;

/// Per-run resource evidence is deliberately provider-neutral and contains only
/// bounded counters. Raw prompts, model output, and question text remain in their
/// existing private/session boundaries.
#[derive(Clone, Default)]
pub(crate) struct AgentResourceTracker {
    pub(crate) counters: Arc<AgentResourceCounters>,
}

#[derive(Default)]
pub(crate) struct AgentResourceCounters {
    pub(crate) generation_requests: AtomicU64,
    pub(crate) tool_attempts: AtomicU64,
    pub(crate) approximate_context_tokens_total: AtomicU64,
    pub(crate) approximate_context_tokens_max: AtomicU64,
    pub(crate) compression_requests: AtomicU64,
    pub(crate) repeated_questions: AtomicU64,
    pub(crate) question_fingerprints: std::sync::Mutex<std::collections::BTreeSet<[u8; 32]>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AgentResourceSnapshot {
    pub(crate) generation_requests: u64,
    pub(crate) tool_attempts: u64,
    pub(crate) approximate_context_tokens_total: u64,
    pub(crate) approximate_context_tokens_max: u64,
    pub(crate) compression_requests: u64,
    pub(crate) repeated_questions: u64,
}

impl AgentResourceTracker {
    pub(crate) fn from_session(session: &Session) -> Self {
        let tracker = Self::default();
        {
            let mut fingerprints = tracker
                .counters
                .question_fingerprints
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            fingerprints.extend(session.events.iter().filter_map(|event| {
                (event.kind == "question_required")
                    .then(|| event.details.get("question").and_then(Value::as_str))
                    .flatten()
                    .map(question_fingerprint)
            }));
        }
        tracker
    }

    pub(crate) fn record_generation_request(&self, request: &LlmRequest<'_>) {
        self.counters
            .generation_requests
            .fetch_add(1, Ordering::Relaxed);
        let messages = request
            .messages
            .iter()
            .map(crate::llm::LlmMessage::to_openai_chat)
            .collect::<Vec<_>>();
        let tools = request.tools.map(|tools| {
            tools
                .iter()
                .map(crate::llm::ToolDefinition::to_openai_tool)
                .collect::<Vec<_>>()
        });
        let approximate = u64::try_from(crate::context::budget::approximate_llm_input_tokens(
            &messages,
            tools.as_deref(),
        ))
        .unwrap_or(u64::MAX);
        self.counters
            .approximate_context_tokens_total
            .fetch_add(approximate, Ordering::Relaxed);
        self.counters
            .approximate_context_tokens_max
            .fetch_max(approximate, Ordering::Relaxed);
    }

    pub(crate) fn record_tool_attempt(&self) {
        self.counters.tool_attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn generation_requests(&self) -> u64 {
        self.counters.generation_requests.load(Ordering::Relaxed)
    }

    pub(crate) fn record_compression_requests_since(&self, previous_generation_requests: u64) {
        let requested = self
            .generation_requests()
            .saturating_sub(previous_generation_requests);
        self.counters
            .compression_requests
            .fetch_add(requested, Ordering::Relaxed);
    }

    pub(crate) fn observe_question(&self, question: &str) {
        let repeated = !self
            .counters
            .question_fingerprints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(question_fingerprint(question));
        if repeated {
            self.counters
                .repeated_questions
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self) -> AgentResourceSnapshot {
        AgentResourceSnapshot {
            generation_requests: self.generation_requests(),
            tool_attempts: self.counters.tool_attempts.load(Ordering::Relaxed),
            approximate_context_tokens_total: self
                .counters
                .approximate_context_tokens_total
                .load(Ordering::Relaxed),
            approximate_context_tokens_max: self
                .counters
                .approximate_context_tokens_max
                .load(Ordering::Relaxed),
            compression_requests: self.counters.compression_requests.load(Ordering::Relaxed),
            repeated_questions: self.counters.repeated_questions.load(Ordering::Relaxed),
        }
    }
}

pub(crate) fn question_fingerprint(question: &str) -> [u8; 32] {
    let normalized = question
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ");
    Sha256::digest(normalized.as_bytes()).into()
}

pub(crate) struct ResourceTrackingLlm {
    pub(crate) inner: Arc<dyn LlmClient>,
    pub(crate) resources: AgentResourceTracker,
}

#[async_trait::async_trait]
impl LlmClient for ResourceTrackingLlm {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, LlmError> {
        self.resources.record_generation_request(&request);
        self.inner.complete(request).await
    }

    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        self.resources.record_generation_request(&request);
        self.inner.stream(request).await
    }
}

pub(crate) fn persist_resource_evidence(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    outcome: &str,
    resources: &AgentResourceTracker,
) -> Result<(), String> {
    let snapshot = resources.snapshot();
    store
        .record_event(
            session_id,
            "agent_resource_usage",
            json!({
                "version": 1,
                "run_id": run_id,
                "outcome": outcome,
                "generation_requests": snapshot.generation_requests,
                "tool_attempts": snapshot.tool_attempts,
                "approximate_context_tokens_total": snapshot.approximate_context_tokens_total,
                "approximate_context_tokens_max": snapshot.approximate_context_tokens_max,
                "compression_requests": snapshot.compression_requests,
                "repeated_questions": snapshot.repeated_questions,
            }),
        )
        .map(|_| ())
        .map_err(|error| format!("failed to persist run resource evidence: {error}"))
}

/// Retains bounded comparison state, never raw requests or tool output. Invocation IDs
/// identify executions rather than progress, so they are deliberately excluded.
#[derive(Default)]
pub(crate) struct FailedToolBatchGuard {
    pub(crate) previous: Option<[u8; 32]>,
    pub(crate) consecutive_failures: u8,
}

impl FailedToolBatchGuard {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn observe(
        &mut self,
        requests: &[ToolCallRequest],
        observations: &[Value],
        batch_success: bool,
    ) -> bool {
        if batch_success
            || observations.is_empty()
            || observations.iter().any(|item| item["success"] == true)
        {
            self.reset();
            return false;
        }
        // Executor observations are already sanitized. Compare their complete semantic
        // result without truncating changes late in a large output or error string.
        let batch = json!({
            "requests": requests.iter().map(|request| json!({
                "name": request.name,
                "arguments": request.arguments,
            })).collect::<Vec<_>>(),
            "observations": observations.iter().map(|observation| {
                let mut output = observation["output"].clone();
                // Foreground terminal duration varies even when the command makes no
                // progress. Preserve all command output and other semantic metadata.
                if observation["tool"] == "run_terminal" {
                    if let Some(output) = output.as_object_mut() {
                        output.remove("duration");
                    }
                }
                json!({
                    "tool": observation["tool"],
                    "success": observation["success"],
                    "output": output,
                    "error": observation["error"],
                })
            }).collect::<Vec<_>>(),
        });
        let fingerprint: [u8; 32] = Sha256::digest(batch.to_string().as_bytes()).into();
        self.consecutive_failures = if self.previous == Some(fingerprint) {
            self.consecutive_failures.saturating_add(1)
        } else {
            1
        };
        self.previous = Some(fingerprint);
        self.consecutive_failures >= MAX_IDENTICAL_FAILED_TOOL_BATCHES
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SteeringInstruction {
    pub(crate) sequence: usize,
    pub(crate) text: String,
}

#[derive(Clone)]
pub struct ExactRunSteeringHandle {
    pub(crate) store: SessionStore,
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    pub(crate) channel_id: String,
    pub(crate) source: String,
    pub(crate) sender: mpsc::UnboundedSender<SteeringInstruction>,
    pub(crate) submission_lock: Arc<std::sync::Mutex<()>>,
}

pub struct ExactRunSteeringReceiver {
    pub(crate) store: SessionStore,
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    pub(crate) channel_id: String,
    pub(crate) receiver: mpsc::UnboundedReceiver<SteeringInstruction>,
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
    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
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
                    let source_event_index = session.events.len();
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
                    session.human_intent.push(HumanIntentRecord {
                        kind: HumanIntentKind::Steering,
                        text: text.clone(),
                        source_message_index: None,
                        source_event_index: Some(source_event_index),
                    });
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

pub(crate) fn normalize_steering_input(text: &str) -> Result<String, String> {
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

pub(crate) fn record_steering_delivery_failure(
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

pub(crate) fn steering_sequence_accounted(
    session: &Session,
    run_id: &str,
    sequence: usize,
) -> bool {
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

pub(crate) fn set_steering_admission(
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

pub(crate) fn close_steering_admission(
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

pub(crate) fn open_steering_admission(
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

pub(crate) fn record_tool_started_and_open_steering(
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
    pub(crate) fn verify_binding(
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

    pub(crate) fn drain(&mut self) -> Vec<SteeringInstruction> {
        let mut instructions =
            std::iter::from_fn(|| self.receiver.try_recv().ok()).collect::<Vec<_>>();
        instructions.sort_unstable_by_key(|instruction| instruction.sequence);
        instructions
    }
}

pub(crate) fn record_steering_intake(
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

pub(crate) fn append_steering_context(
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

pub(crate) fn supersede_unapproved_plan_for_steering(
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

pub(crate) fn safe_question_arguments(arguments: &Value, sensitive_values: &[String]) -> Value {
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
    let proposed_answer = arguments
        .get("proposed_answer")
        .and_then(Value::as_str)
        .filter(|answer| !answer.trim().is_empty())
        .map(|answer| {
            crate::interactive::bounded_public_text(
                answer,
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
    let dependent_paths = arguments
        .get("dependent_paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .take(32)
                .map(|path| {
                    crate::interactive::bounded_public_text(path, sensitive_values, 4_096, false)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut safe =
        json!({"question": question, "options": options, "dependent_paths": dependent_paths});
    if let Some(proposed_answer) = proposed_answer {
        safe["proposed_answer"] = json!(proposed_answer);
    }
    safe
}

pub(crate) fn safe_question_execution_arguments(
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReusedClarificationAnswer {
    pub(crate) answer: String,
    pub(crate) answer_message_index: Option<usize>,
    pub(crate) answer_event_index: Option<usize>,
}

pub(crate) struct ClarificationPrompt<'a> {
    pub(crate) question: &'a str,
    pub(crate) proposed_answer: Option<&'a str>,
    pub(crate) options: &'a [String],
    pub(crate) dependent_paths: &'a [String],
}

pub(crate) fn persist_question_required(
    store: &SessionStore,
    session_id: &str,
    plan_id: Option<&str>,
    invocation_id: crate::tools::ToolInvocationId,
    prompt: ClarificationPrompt<'_>,
) -> Result<Option<ReusedClarificationAnswer>, String> {
    let ClarificationPrompt {
        question,
        proposed_answer,
        options,
        dependent_paths,
    } = prompt;
    store
        .update_session(session_id, |session| {
            let prior = session
                .clarifications
                .iter()
                .rev()
                .find_map(|clarification| {
                    (clarification.plan_id.as_deref() == plan_id
                        && clarification.question == question
                        && clarification.proposed_answer.as_deref() == proposed_answer
                        && clarification.options == options
                        && clarification.status == ClarificationStatus::Answered)
                        .then(|| {
                            Some(ReusedClarificationAnswer {
                                answer: clarification.answer.clone()?,
                                answer_message_index: clarification.answer_message_index,
                                answer_event_index: clarification.answer_event_index,
                            })
                        })
                        .flatten()
                });
            let event_index = session.events.len();
            append_session_event(
                session,
                "question_required",
                json!({
                    "invocation_id": invocation_id,
                    "question": question,
                    "proposed_answer": proposed_answer,
                    "options": options,
                    "answer_reused": prior.is_some(),
                }),
            );
            session.clarifications.push(ClarificationRecord {
                invocation_id,
                plan_id: plan_id.map(str::to_string),
                question: question.to_string(),
                proposed_answer: proposed_answer.map(str::to_string),
                options: options.to_vec(),
                dependent_paths: dependent_paths.to_vec(),
                status: if prior.is_some() {
                    ClarificationStatus::Answered
                } else {
                    ClarificationStatus::Pending
                },
                answer: prior.as_ref().map(|prior| prior.answer.clone()),
                question_event_index: event_index,
                answer_message_index: prior.as_ref().and_then(|prior| prior.answer_message_index),
                answer_event_index: prior.as_ref().and_then(|prior| prior.answer_event_index),
                reason: prior
                    .as_ref()
                    .map(|_| "reused exact answered question in the same plan".to_string()),
                outcome: prior.as_ref().map(|_| "answered".to_string()),
            });
            Ok(prior)
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn persist_question_observation(
    store: &SessionStore,
    session_id: &str,
    invocation_id: crate::tools::ToolInvocationId,
    observations: &[Value],
    outcome: &QuestionOutcome,
    reused_answer: bool,
) -> Result<(), String> {
    store
        .try_append_message_with_origin(
            session_id,
            "tool",
            &json!({"observations": observations}).to_string(),
            MessageOrigin::ToolOutput,
        )
        .map_err(|error| error.to_string())?;
    store
        .update_session(session_id, |session| {
            let target = session
                .clarifications
                .iter()
                .rev()
                .find(|clarification| clarification.invocation_id == invocation_id)
                .ok_or_else(|| {
                    crate::session::SessionError::InvalidMutation(
                        "question outcome has no persisted clarification".to_string(),
                    )
                })?
                .clone();
            match outcome {
                QuestionOutcome::Answered(answer) | QuestionOutcome::ApprovedProposal(answer) => {
                    let (answer_message_index, answer_event_index) = if reused_answer {
                        (target.answer_message_index, target.answer_event_index)
                    } else {
                        let event_index = session.events.len();
                        append_session_event(
                            session,
                            "human_question_answer_received",
                            json!({
                                "invocation_id": invocation_id,
                                "question_event_index": target.question_event_index,
                                "answer": answer,
                                "decision": outcome.reason(),
                            }),
                        );
                        session.human_intent.push(HumanIntentRecord {
                            kind: HumanIntentKind::QuestionAnswer,
                            text: answer.clone(),
                            source_message_index: None,
                            source_event_index: Some(event_index),
                        });
                        (None, Some(event_index))
                    };
                    for clarification in session.clarifications.iter_mut().filter(|candidate| {
                        candidate.plan_id == target.plan_id
                            && candidate.question == target.question
                            && candidate.proposed_answer == target.proposed_answer
                            && candidate.options == target.options
                            && candidate.status != ClarificationStatus::Answered
                    }) {
                        clarification.status = ClarificationStatus::Answered;
                        clarification.answer = Some(answer.clone());
                        clarification.answer_message_index = answer_message_index;
                        clarification.answer_event_index = answer_event_index;
                        clarification.outcome = Some(outcome.reason().to_string());
                        clarification.reason = (clarification.invocation_id != invocation_id)
                            .then(|| {
                                "resolved by a later valid answer to the same question and option set"
                                    .to_string()
                            });
                    }
                }
                other => {
                    let clarification = session
                        .clarifications
                        .iter_mut()
                        .rev()
                        .find(|clarification| clarification.invocation_id == invocation_id)
                        .expect("target clarification was cloned above");
                    clarification.status = other.clarification_status();
                    clarification.outcome = Some(other.reason().to_string());
                    clarification.reason = Some(match other {
                        QuestionOutcome::InputUnavailable(message) => message.clone(),
                        _ => other.reason().to_string(),
                    });
                }
            }
            Ok(())
        })
        .map_err(|error| error.to_string())
}

pub(crate) fn bounded_clarification_dependency_paths(
    arguments: &Value,
) -> Result<Vec<String>, String> {
    let Some(values) = arguments.get("dependent_paths") else {
        return Ok(Vec::new());
    };
    let values = values
        .as_array()
        .ok_or_else(|| "ask_question dependent_paths must be an array".to_string())?;
    if values.len() > 32 {
        return Err("ask_question dependent_paths exceeds the 32-path limit".to_string());
    }
    let mut paths = values
        .iter()
        .map(|value| {
            let path = value
                .as_str()
                .ok_or_else(|| "ask_question dependent path must be a string".to_string())?;
            let parsed = Path::new(path);
            if path.is_empty()
                || parsed.is_absolute()
                || parsed.components().any(|component| {
                    !matches!(
                        component,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
            {
                return Err(format!(
                    "ask_question dependent path is not a bounded worktree-relative path: {path}"
                ));
            }
            Ok(path.to_string())
        })
        .collect::<Result<Vec<_>, String>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(crate) fn scopes_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

pub(crate) fn unresolved_clarification_dependencies(
    session: &Session,
    plan_id: Option<&str>,
    instruction_root: &Path,
    tool_calls: &[ToolCallRequest],
) -> Result<Vec<crate::tools::ToolInvocationId>, String> {
    let unresolved = session
        .clarifications
        .iter()
        .filter(|clarification| {
            clarification.plan_id.as_deref() == plan_id
                && matches!(
                    clarification.status,
                    ClarificationStatus::Pending
                        | ClarificationStatus::Unresolved
                        | ClarificationStatus::Cancelled
                )
        })
        .collect::<Vec<_>>();
    let mut blockers = Vec::new();
    for clarification in unresolved {
        let dependencies = clarification
            .dependent_paths
            .iter()
            .map(|path| instruction_root.join(path))
            .collect::<Vec<_>>();
        let blocked = tool_calls
            .iter()
            .filter(|call| call.name != "ask_question")
            .try_fold(false, |blocked, call| -> Result<bool, String> {
                if blocked || dependencies.is_empty() {
                    return Ok(true);
                }
                let scopes =
                    tool_instruction_scopes(instruction_root, &call.name, &call.arguments)?;
                Ok(scopes.iter().any(|scope| {
                    dependencies
                        .iter()
                        .any(|dependency| scopes_overlap(scope, dependency))
                }))
            })?;
        if blocked {
            blockers.push(clarification.invocation_id);
        }
    }
    Ok(blockers)
}

#[derive(Clone, Default)]
pub struct CancellationSignal {
    pub(crate) state: Arc<CancellationState>,
}

#[derive(Default)]
pub(crate) struct CancellationState {
    pub(crate) cancelled: AtomicBool,
    pub(crate) notify: Notify,
}

#[derive(Default)]
pub(crate) struct PreparedTaskBatch {
    pub(crate) tasks: Vec<PreparedTask>,
}

pub(crate) struct PreparedTask {
    pub(crate) id: String,
    pub(crate) tool_name: String,
}

impl PreparedTaskBatch {
    pub(crate) fn track(&mut self, id: String, tool_name: String) {
        self.tasks.push(PreparedTask { id, tool_name });
    }

    pub(crate) fn start_all(&mut self) -> Vec<(PreparedTask, String)> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionOutcome {
    Answered(String),
    ApprovedProposal(String),
    LeftUnanswered,
    Cancelled,
    InputClosed,
    InputUnavailable(String),
}

impl QuestionOutcome {
    pub fn answered_text(&self) -> Option<&str> {
        match self {
            Self::Answered(answer) | Self::ApprovedProposal(answer) => Some(answer.as_str()),
            _ => None,
        }
    }

    pub(crate) fn clarification_status(&self) -> ClarificationStatus {
        match self {
            Self::Answered(_) | Self::ApprovedProposal(_) => ClarificationStatus::Answered,
            Self::Cancelled => ClarificationStatus::Cancelled,
            Self::LeftUnanswered | Self::InputClosed | Self::InputUnavailable(_) => {
                ClarificationStatus::Unresolved
            }
        }
    }

    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Answered(_) => "answered",
            Self::ApprovedProposal(_) => "approved_proposal",
            Self::LeftUnanswered => "left_unanswered",
            Self::Cancelled => "cancelled",
            Self::InputClosed => "input_closed",
            Self::InputUnavailable(_) => "input_unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QuestionRequestContext<'a> {
    pub invocation_id: crate::tools::ToolInvocationId,
    pub question: &'a str,
    pub proposed_answer: Option<&'a str>,
    pub options: &'a [String],
}

#[async_trait::async_trait]
pub trait QuestionHandler: Send + Sync {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String>;

    async fn ask_with_context(&self, context: QuestionRequestContext<'_>) -> QuestionOutcome {
        match self.ask(context.question, context.options).await {
            Ok(answer) if !answer.trim().is_empty() => QuestionOutcome::Answered(answer),
            Ok(_) => QuestionOutcome::InputUnavailable(
                "question handler returned an empty answer".to_string(),
            ),
            Err(_) => {
                QuestionOutcome::InputUnavailable("question handler was unavailable".to_string())
            }
        }
    }
}

pub struct AgentLoopConfig {
    /// A non-zero value overrides `agent.max_turns` for this run.
    pub max_steps: u32,
    pub mode: String,
    /// True only for a newly submitted plain/TUI request. Non-interactive,
    /// delegated, gateway, and durable callers retain mandatory planning.
    pub interactive_request: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub auto_approve: bool,
    pub approval_handler: Option<Arc<dyn ApprovalHandler>>,
    pub question_handler: Option<Arc<dyn QuestionHandler>>,
    pub stream_tx: Option<Sender<StreamEvent>>,
    pub cancellation: Option<CancellationSignal>,
    pub run_id: Option<String>,
    pub steering: Option<ExactRunSteeringReceiver>,
    pub continuation_plan_id: Option<String>,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_steps: 0,
            mode: "execute".to_string(),
            interactive_request: false,
            provider: None,
            model: None,
            auto_approve: false,
            approval_handler: None,
            question_handler: None,
            stream_tx: None,
            cancellation: None,
            run_id: None,
            steering: None,
            continuation_plan_id: None,
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
        if self.outcome == "instruction_context_missing" {
            let body = self
                .last_message
                .as_deref()
                .filter(|message| message.contains("Project instructions could not be loaded"))
                .map(str::to_string)
                .unwrap_or_else(|| {
                    instruction_context_user_message(
                        "required project instructions are unavailable",
                    )
                });
            return Some(format!("{body}\nSession: {}", self.session_id));
        }
        self.failure.as_ref().map_or_else(
            || {
                if is_llm_failure_outcome(&self.outcome) {
                    Some(
                        LlmError::new(
                            LlmErrorClass::ProviderRejected,
                            LlmErrorPhase::TerminalValidation,
                            crate::llm::RetryDisposition::NotAttempted,
                            crate::llm::LlmErrorMetadata::new("unknown", "legacy", None, None, &[]),
                            "legacy run did not persist structured LLM failure evidence",
                        )
                        .user_report(Some(&self.session_id)),
                    )
                } else if is_agent_failure_outcome(&self.outcome)
                    || matches!(
                        self.outcome.as_str(),
                        "local_error" | "unresponsive_worker_shutdown"
                    )
                {
                    Some(crate::interactive::user_visible_stop_report(
                        &self.outcome,
                        &self.session_id,
                    ))
                } else {
                    None
                }
            },
            |failure| Some(failure.user_report(Some(&self.session_id))),
        )
    }
}

pub(crate) fn instruction_context_user_message(error: &str) -> String {
    format!(
        "Project instructions could not be loaded, so this run stopped before dependent work.\n{error}\nRestore readable project instructions within the size limit, or increase llm.context_length, then retry the same plan."
    )
}
