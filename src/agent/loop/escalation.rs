//! FT-021 cost-controlled model escalation.
//!
//! The run's own provider/model is the worker. When `agent.escalation.model` is
//! configured, a bounded number of calls per run may go to that escalation model:
//! first as bounded advice the worker continues from, then as a one-turn takeover.
//! Every dispatch is reserved in the session before the call, so cancellation and
//! restart keep the consumed allowance. Escalation never offers tools to the
//! advisor and never changes approval, sandbox, or verification gates.

use super::*;

pub(crate) const ASK_ADVICE_TOOL: &str = "ask_advice";
/// Consecutive failed tool batches or rejected completions that trigger escalation.
pub(crate) const ESCALATION_FAILURE_THRESHOLD: u32 = 3;
/// Escalation-model share of a run's reported model tokens.
pub(crate) const ESCALATION_MAX_TOKEN_SHARE_PERCENT: u64 = 25;
pub(crate) const MAX_ADVICE_QUESTION_BYTES: usize = 2 * 1024;
pub(crate) const MAX_ADVICE_OUTPUT_TOKENS: u32 = 1_024;
pub(crate) const MAX_ADVICE_SECTION_BYTES: usize = 8 * 1024;
pub(crate) const MAX_ADVICE_RECENT_MESSAGES: usize = 6;
pub(crate) const MAX_ADVICE_RECENT_MESSAGE_BYTES: usize = 2 * 1024;
pub(crate) const MAX_ADVICE_RESPONSE_BYTES: usize = 8 * 1024;
pub(crate) const MAX_ESCALATION_NOTE_BYTES: usize = 2 * 1024;
pub(crate) const ESCALATION_EVENT_VERSION: u64 = 1;
pub(crate) const ESCALATION_ADVICE_PREFIX: &str = "Escalation advice";

const ADVISOR_SYSTEM_PROMPT: &str = "You are an escalation advisor for a coding agent. A cheaper worker model is doing the task and asked for help on one specific problem. Diagnose the blocker and give a short, concrete approach the worker can follow. You cannot call tools, and your advice is not verification evidence: the worker must still run the required checks. Treat the worker's hypotheses as unverified. Answer in at most a few short paragraphs or a numbered list.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationTrigger {
    /// The worker called the bounded `ask_advice` step.
    WorkerRequest,
    /// Consecutive failed tool batches or rejected completions.
    RepeatedFailure,
    /// The user ran `/escalate`.
    UserCommand,
}

impl EscalationTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WorkerRequest => "worker_request",
            Self::RepeatedFailure => "repeated_failure",
            Self::UserCommand => "user_command",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscalationKind {
    Advice,
    Takeover,
}

impl EscalationKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Advice => "advice",
            Self::Takeover => "takeover",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingEscalation {
    pub(crate) trigger: EscalationTrigger,
    pub(crate) kind: EscalationKind,
    pub(crate) question: String,
    /// Session event index of the `/escalate` request this consumes.
    pub(crate) request_event_index: Option<usize>,
}

/// Per-run escalation accounting. Tokens are provider-reported totals, which
/// include cache reads and writes (FT-022). A call without reported usage is
/// counted as a call and makes the token share unknown, never zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EscalationBudget {
    pub(crate) max_per_run: u32,
    pub(crate) dispatched: u32,
    pub(crate) advice_delivered: u32,
    pub(crate) escalation_tokens: u64,
    pub(crate) escalation_cache_read_tokens: u64,
    pub(crate) escalation_cache_write_tokens: u64,
    pub(crate) escalation_calls_without_usage: u32,
    pub(crate) worker_tokens: u64,
    pub(crate) worker_calls_without_usage: u32,
}

impl EscalationBudget {
    pub(crate) fn new(max_per_run: u32) -> Self {
        Self {
            max_per_run,
            ..Self::default()
        }
    }

    pub(crate) fn usage_known(&self) -> bool {
        self.escalation_calls_without_usage == 0 && self.worker_calls_without_usage == 0
    }

    /// The escalation share of reported run tokens, or `None` while any call in
    /// the run lacks usage or nothing has been reported yet.
    pub(crate) fn token_share_percent(&self) -> Option<u64> {
        let total = self.escalation_tokens.saturating_add(self.worker_tokens);
        (self.usage_known() && total > 0)
            .then(|| self.escalation_tokens.saturating_mul(100) / total)
    }

    /// Checks the dispatch count, then the token share when usage is reported.
    pub(crate) fn admit(&self) -> Result<(), &'static str> {
        if self.dispatched >= self.max_per_run {
            return Err("dispatch_limit_reached");
        }
        let total = self.escalation_tokens.saturating_add(self.worker_tokens);
        if self.usage_known()
            && self.escalation_tokens > 0
            && self.escalation_tokens.saturating_mul(100)
                >= ESCALATION_MAX_TOKEN_SHARE_PERCENT.saturating_mul(total)
        {
            return Err("token_share_exceeded");
        }
        Ok(())
    }

    pub(crate) fn record_worker_usage(&mut self, usage: Option<&crate::llm::LlmUsage>) {
        match usage {
            Some(usage) => {
                self.worker_tokens = self.worker_tokens.saturating_add(usage.total_tokens)
            }
            None => {
                self.worker_calls_without_usage = self.worker_calls_without_usage.saturating_add(1)
            }
        }
    }

    pub(crate) fn record_escalation_usage(&mut self, usage: Option<&crate::llm::LlmUsage>) {
        match usage {
            Some(usage) => {
                self.escalation_tokens = self.escalation_tokens.saturating_add(usage.total_tokens);
                self.escalation_cache_read_tokens = self
                    .escalation_cache_read_tokens
                    .saturating_add(usage.cached_input_tokens.unwrap_or(0));
                self.escalation_cache_write_tokens = self
                    .escalation_cache_write_tokens
                    .saturating_add(usage.cache_write_input_tokens.unwrap_or(0));
            }
            None => {
                self.escalation_calls_without_usage =
                    self.escalation_calls_without_usage.saturating_add(1);
            }
        }
    }

    pub(crate) fn snapshot(&self) -> Value {
        json!({
            "max_per_run": self.max_per_run,
            "dispatched": self.dispatched,
            "remaining": self.max_per_run.saturating_sub(self.dispatched),
            "usage_basis": if self.usage_known() { "tokens" } else { "calls" },
            "escalation_tokens": self.escalation_tokens,
            "escalation_cache_read_tokens": self.escalation_cache_read_tokens,
            "escalation_cache_write_tokens": self.escalation_cache_write_tokens,
            "escalation_calls_without_usage": self.escalation_calls_without_usage,
            "worker_tokens": self.worker_tokens,
            "worker_calls_without_usage": self.worker_calls_without_usage,
            "token_share_percent": self.token_share_percent(),
            "max_token_share_percent": ESCALATION_MAX_TOKEN_SHARE_PERCENT,
        })
    }

    /// Rebuilds the run's consumed allowance from its durable session records, so
    /// a resumed run cannot spend the same allowance twice.
    pub(crate) fn from_session(max_per_run: u32, session: &Session, run_id: &str) -> Self {
        let mut budget = Self::new(max_per_run);
        for event in &session.events {
            if event.details.get("run_id").and_then(Value::as_str) != Some(run_id) {
                continue;
            }
            match event.kind.as_str() {
                "escalation_dispatched" => budget.dispatched = budget.dispatched.saturating_add(1),
                "escalation_completed" => {
                    if event.details["outcome"] == "advice_delivered" {
                        budget.advice_delivered = budget.advice_delivered.saturating_add(1);
                    }
                    budget.record_escalation_usage(
                        usage_from_details(&event.details["usage"]).as_ref(),
                    );
                }
                // Escalation calls carry a model role; worker turns do not.
                "context_usage" if event.details.get("model_role").is_none() => {
                    let usage = (event.details["unknown"] == false)
                        .then(|| usage_from_details(&event.details))
                        .flatten();
                    budget.record_worker_usage(usage.as_ref());
                }
                _ => {}
            }
        }
        // A dispatch without its completion record (cancellation, crash) keeps its
        // reservation and has no reported usage.
        let completed = session
            .events
            .iter()
            .filter(|event| {
                event.kind == "escalation_completed"
                    && event.details.get("run_id").and_then(Value::as_str) == Some(run_id)
            })
            .count();
        let interrupted = (budget.dispatched as usize).saturating_sub(completed);
        budget.escalation_calls_without_usage = budget
            .escalation_calls_without_usage
            .saturating_add(u32::try_from(interrupted).unwrap_or(u32::MAX));
        budget
    }
}

fn usage_from_details(details: &Value) -> Option<crate::llm::LlmUsage> {
    let input = details.get("input_tokens")?.as_u64()?;
    let output = details.get("output_tokens")?.as_u64()?;
    let total = details.get("total_tokens")?.as_u64()?;
    crate::llm::LlmUsage::new(
        input,
        output,
        total,
        details.get("cached_input_tokens").and_then(Value::as_u64),
        details
            .get("reasoning_output_tokens")
            .and_then(Value::as_u64),
    )
    .and_then(|usage| {
        usage.with_cache_write_input_tokens(
            details
                .get("cache_write_input_tokens")
                .and_then(Value::as_u64),
        )
    })
    .ok()
}

fn usage_details(usage: Option<&crate::llm::LlmUsage>) -> Value {
    usage.map_or(Value::Null, |usage| {
        json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "cache_write_input_tokens": usage.cache_write_input_tokens,
            "reasoning_output_tokens": usage.reasoning_output_tokens,
        })
    })
}

/// Live escalation state for one run.
pub(crate) struct EscalationRuntime {
    pub(crate) client: Arc<dyn LlmClient>,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) budget: EscalationBudget,
    pub(crate) consecutive_failures: u32,
    pub(crate) pending: Option<PendingEscalation>,
}

impl EscalationRuntime {
    pub(crate) fn label(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }

    /// Advice comes first; once advice was delivered in this run, a further
    /// failure or user escalation becomes a one-turn takeover.
    pub(crate) fn next_kind(&self, trigger: EscalationTrigger) -> EscalationKind {
        if trigger != EscalationTrigger::WorkerRequest && self.budget.advice_delivered > 0 {
            EscalationKind::Takeover
        } else {
            EscalationKind::Advice
        }
    }

    /// Counts a failed tool batch or rejected completion. Returns true when the
    /// streak reaches the threshold; the streak then restarts.
    pub(crate) fn observe_failure(&mut self) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures >= ESCALATION_FAILURE_THRESHOLD {
            self.consecutive_failures = 0;
            true
        } else {
            false
        }
    }

    pub(crate) fn observe_success(&mut self) {
        self.consecutive_failures = 0;
    }
}

/// Resolves the escalation model for this run. `Ok(None)` means escalation is
/// off: not configured, disallowed for this run, or the run pinned a model.
pub(crate) fn prepare_escalation_runtime(
    nib_cfg: &crate::config::NibConfig,
    cfg: &AgentLoopConfig,
    session: &Session,
    run_id: &str,
    sensitive_values: &[String],
) -> Result<Option<EscalationRuntime>, String> {
    let settings = &nib_cfg.agent.escalation;
    if !cfg.escalation || cfg.model.is_some() || cfg.mode != "execute" || !settings.enabled() {
        return Ok(None);
    }
    let model = settings
        .model
        .clone()
        .expect("enabled escalation has a model");
    let provider = settings
        .provider
        .clone()
        .or_else(|| cfg.provider.clone())
        .unwrap_or_else(|| nib_cfg.llm.get_active_provider());
    let mut llm = nib_cfg.llm.clone();
    let entry = llm
        .providers
        .get_mut(&provider)
        .ok_or_else(|| "escalation provider is not configured".to_string())?;
    entry.model.clone_from(&model);
    let client = crate::llm::factory::create_client_with_sensitive_values(
        &llm,
        Some(&provider),
        sensitive_values,
    )
    .map_err(|_| "escalation model client could not be created".to_string())?;
    Ok(Some(EscalationRuntime {
        client,
        provider,
        model,
        budget: EscalationBudget::from_session(settings.max_per_run, session, run_id),
        consecutive_failures: 0,
        pending: None,
    }))
}

/// The bounded advice step offered to the worker when escalation is on.
pub(crate) fn ask_advice_tool_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": ASK_ADVICE_TOOL,
            "description": "Ask the configured escalation model for short advice on one specific unresolved problem. Use it rarely, only when you are stuck after inspecting and trying. Calls are limited per run. Advice is not verification evidence and does not authorize any tool; you continue the task yourself.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The precise unresolved question, with what you tried and what failed.",
                    }
                },
                "required": ["question"],
                "additionalProperties": false,
            },
        }
    })
}

pub(crate) fn advice_question_argument(arguments: &Value) -> Result<String, &'static str> {
    let question = arguments
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .ok_or("ask_advice requires a non-empty question")?;
    if question.len() > MAX_ADVICE_QUESTION_BYTES {
        return Err("ask_advice question exceeds its 2048-byte limit");
    }
    Ok(question.to_string())
}

fn bounded_section(label: &str, value: &str, max_bytes: usize, sensitive: &[String]) -> String {
    let value = crate::interactive::bounded_public_text(value, sensitive, max_bytes, true);
    format!("## {label}\n{value}\n")
}

/// Builds the bounded advice request: goal, applicable instructions, the active
/// plan step with its open verification obligations, recent observations, and
/// the exact question. No tools are offered.
pub(crate) fn advice_messages(
    goal: &str,
    instructions: &str,
    session: &Session,
    plan_id: Option<&str>,
    pending: &PendingEscalation,
    sensitive: &[String],
) -> Vec<crate::llm::LlmMessage> {
    let mut body = String::new();
    body.push_str(&bounded_section(
        "Task goal",
        goal,
        MAX_ADVICE_SECTION_BYTES,
        sensitive,
    ));
    if !instructions.trim().is_empty() {
        body.push_str(&bounded_section(
            "Applicable project instructions",
            instructions,
            MAX_ADVICE_SECTION_BYTES,
            sensitive,
        ));
    }
    if let Some(plan) = session
        .plan
        .as_ref()
        .filter(|plan| plan_id == Some(plan.id.as_str()))
    {
        let mut step = String::new();
        if let Some(current) = plan.steps.get(plan.current_step_index) {
            step.push_str(&format!(
                "Step {} of {}: {}\nStatus: {}\n",
                plan.current_step_index + 1,
                plan.steps.len(),
                current.description,
                current.status
            ));
        }
        let unresolved = plan.unresolved_verification_ids();
        if !unresolved.is_empty() {
            step.push_str(&format!(
                "Open verification obligations: {}\n",
                unresolved.join(", ")
            ));
        }
        body.push_str(&bounded_section(
            "Current plan step",
            &step,
            MAX_ADVICE_SECTION_BYTES,
            sensitive,
        ));
    }
    let recent = session
        .messages
        .iter()
        .rev()
        .filter(|message| message.role != "system")
        .take(MAX_ADVICE_RECENT_MESSAGES)
        .collect::<Vec<_>>();
    if !recent.is_empty() {
        let mut observations = String::new();
        for message in recent.into_iter().rev() {
            let label = match session.message_origin(message.index) {
                origin if origin.is_human() => "human",
                MessageOrigin::ModelOutput => "worker (unverified)",
                MessageOrigin::ToolOutput => "tool observation",
                MessageOrigin::RuntimeContinuation => "runtime",
                _ => message.role.as_str(),
            };
            let content = crate::interactive::bounded_public_text(
                &message.content,
                sensitive,
                MAX_ADVICE_RECENT_MESSAGE_BYTES,
                true,
            );
            observations.push_str(&format!("[{label}] {content}\n"));
        }
        body.push_str(&bounded_section(
            "Recent observations",
            &observations,
            MAX_ADVICE_SECTION_BYTES,
            sensitive,
        ));
    }
    body.push_str(&format!(
        "## Escalation trigger\n{}\n",
        match pending.trigger {
            EscalationTrigger::WorkerRequest => "The worker asked for advice.",
            EscalationTrigger::RepeatedFailure =>
                "The worker had repeated consecutive failed tool calls or verifications.",
            EscalationTrigger::UserCommand => "The user requested escalation.",
        }
    ));
    body.push_str(&bounded_section(
        "Question",
        &pending.question,
        MAX_ADVICE_QUESTION_BYTES,
        sensitive,
    ));
    vec![
        crate::llm::LlmMessage::system(ADVISOR_SYSTEM_PROMPT),
        crate::llm::LlmMessage::user(body),
    ]
}

pub(crate) fn default_escalation_question(trigger: EscalationTrigger) -> &'static str {
    match trigger {
        EscalationTrigger::RepeatedFailure => {
            "The last attempts failed repeatedly. What is the likely cause, and what should the worker do next?"
        }
        _ => "What is the most likely blocker in the current step, and what should the worker do next?",
    }
}

/// Finds the newest `/escalate` request not yet consumed in this session.
pub(crate) fn pending_user_escalation_request(session: &Session) -> Option<(usize, String)> {
    let consumed = session
        .events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "escalation_dispatched" | "escalation_skipped"
            )
        })
        .filter_map(|event| {
            event
                .details
                .get("request_event_index")
                .and_then(Value::as_u64)
        })
        .max();
    let request = session
        .events
        .iter()
        .rev()
        .find(|event| event.kind == "escalation_requested")?;
    if consumed.is_some_and(|consumed| consumed >= request.index as u64) {
        return None;
    }
    let note = request
        .details
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((request.index, note))
}

/// Records a `/escalate` request for the session's current or next run.
pub(crate) fn record_user_escalation_request(
    store: &SessionStore,
    session_id: &str,
    note: Option<&str>,
) -> Result<(), String> {
    let note = note
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(|note| {
            crate::interactive::bounded_public_text(
                note,
                store.public_sensitive_values(),
                MAX_ESCALATION_NOTE_BYTES,
                false,
            )
        });
    store
        .record_event(
            session_id,
            "escalation_requested",
            json!({
                "version": ESCALATION_EVENT_VERSION,
                "trigger": EscalationTrigger::UserCommand.as_str(),
                "note": note,
            }),
        )
        .map(|_| ())
        .map_err(|error| format!("failed to record the escalation request: {error}"))
}

/// Admits a trigger against the budget. A denial is recorded and returns `None`.
pub(crate) async fn admit_escalation(
    escalation: &EscalationRuntime,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    trigger: EscalationTrigger,
    request_event_index: Option<usize>,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<Option<EscalationKind>, String> {
    let kind = escalation.next_kind(trigger);
    match escalation.budget.admit() {
        Ok(()) => Ok(Some(kind)),
        Err(reason) => {
            store
                .record_event(
                    session_id,
                    "escalation_skipped",
                    json!({
                        "version": ESCALATION_EVENT_VERSION,
                        "run_id": run_id,
                        "kind": kind.as_str(),
                        "trigger": trigger.as_str(),
                        "reason": reason,
                        "request_event_index": request_event_index,
                        "budget": escalation.budget.snapshot(),
                    }),
                )
                .map_err(|error| error.to_string())?;
            emit(
                stream_tx,
                StreamEvent::Escalation {
                    kind: kind.as_str().to_string(),
                    trigger: trigger.as_str().to_string(),
                    model: escalation.label(),
                    status: "skipped".to_string(),
                    reason: Some(reason.to_string()),
                },
            )
            .await;
            Ok(None)
        }
    }
}

/// Reserves one escalation in the session before the provider call.
pub(crate) async fn record_escalation_dispatch(
    escalation: &mut EscalationRuntime,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    plan_id: Option<&str>,
    pending: &PendingEscalation,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<u32, String> {
    escalation.budget.dispatched = escalation.budget.dispatched.saturating_add(1);
    let sequence = escalation.budget.dispatched;
    let step_index = store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .and_then(|session| session.plan)
        .filter(|plan| plan_id == Some(plan.id.as_str()))
        .map(|plan| plan.current_step_index);
    store
        .record_event(
            session_id,
            "escalation_dispatched",
            json!({
                "version": ESCALATION_EVENT_VERSION,
                "run_id": run_id,
                "sequence": sequence,
                "kind": pending.kind.as_str(),
                "trigger": pending.trigger.as_str(),
                "provider": escalation.provider,
                "model": escalation.model,
                "plan_id": plan_id,
                "step_index": step_index,
                "request_event_index": pending.request_event_index,
                "reserved": {
                    "escalations": 1,
                    "max_output_tokens": (pending.kind == EscalationKind::Advice)
                        .then_some(MAX_ADVICE_OUTPUT_TOKENS),
                },
                "budget": escalation.budget.snapshot(),
            }),
        )
        .map_err(|error| error.to_string())?;
    emit(
        stream_tx,
        StreamEvent::Escalation {
            kind: pending.kind.as_str().to_string(),
            trigger: pending.trigger.as_str().to_string(),
            model: escalation.label(),
            status: "started".to_string(),
            reason: None,
        },
    )
    .await;
    Ok(sequence)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_escalation_completion(
    escalation: &mut EscalationRuntime,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    sequence: u32,
    kind: EscalationKind,
    trigger: EscalationTrigger,
    outcome: &str,
    usage: Option<&crate::llm::LlmUsage>,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<(), String> {
    escalation.budget.record_escalation_usage(usage);
    if outcome == "advice_delivered" {
        escalation.budget.advice_delivered = escalation.budget.advice_delivered.saturating_add(1);
    }
    store
        .record_event(
            session_id,
            "escalation_completed",
            json!({
                "version": ESCALATION_EVENT_VERSION,
                "run_id": run_id,
                "sequence": sequence,
                "kind": kind.as_str(),
                "trigger": trigger.as_str(),
                "provider": escalation.provider,
                "model": escalation.model,
                "outcome": outcome,
                "usage_available": usage.is_some(),
                "usage": usage_details(usage),
                "budget": escalation.budget.snapshot(),
            }),
        )
        .map_err(|error| error.to_string())?;
    emit(
        stream_tx,
        StreamEvent::Escalation {
            kind: kind.as_str().to_string(),
            trigger: trigger.as_str().to_string(),
            model: escalation.label(),
            status: outcome.to_string(),
            reason: None,
        },
    )
    .await;
    Ok(())
}

/// Runs one bounded advice call. Returns the sanitized advice text, or `None`
/// when the call failed; both outcomes are recorded and counted.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_escalation_advice(
    escalation: &mut EscalationRuntime,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    goal: &str,
    instructions: &str,
    plan_id: Option<&str>,
    pending: &PendingEscalation,
    request_scope: &LlmRequestScope,
    sensitive: &[String],
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<Option<String>, String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "session disappeared before escalation advice".to_string())?;
    let messages = advice_messages(goal, instructions, &session, plan_id, pending, sensitive);
    let sequence = record_escalation_dispatch(
        escalation, store, session_id, run_id, plan_id, pending, stream_tx,
    )
    .await?;
    // A fresh request: no worker continuation, no tools, bounded output.
    let request = LlmRequest::new(&messages, None)
        .with_scope(request_scope.clone())
        .with_max_output_tokens(MAX_ADVICE_OUTPUT_TOKENS);
    let result = match escalation.client.stream(request).await {
        Ok(stream) => finish_private_provider_stream(stream, sensitive)
            .await
            .map(|(response, _)| response),
        Err(error) => Err(error),
    };
    let (advice, usage, outcome) = match result {
        Ok(response) if response.terminal_status != LlmTerminalStatus::Refused => {
            let advice = response
                .content
                .as_deref()
                .map(str::trim)
                .filter(|content| !content.is_empty())
                .map(|content| {
                    crate::interactive::bounded_public_text(
                        content,
                        sensitive,
                        MAX_ADVICE_RESPONSE_BYTES,
                        true,
                    )
                });
            let outcome = if advice.is_some() {
                "advice_delivered"
            } else {
                "advice_empty"
            };
            (advice, response.usage, outcome)
        }
        Ok(response) => (None, response.usage, "advice_refused"),
        Err(_) => (None, None, "advice_failed"),
    };
    // Session usage totals include advice calls; the model role keeps them out of
    // the worker's share and distinct from the run's worker request sequence.
    let mut usage_event =
        crate::context::snapshot::usage_event_details(run_id, u64::from(sequence), usage.as_ref());
    usage_event["model_role"] = json!("escalation_advice");
    store
        .record_event(session_id, "context_usage", usage_event)
        .map_err(|error| error.to_string())?;
    record_escalation_completion(
        escalation,
        store,
        session_id,
        run_id,
        sequence,
        EscalationKind::Advice,
        pending.trigger,
        outcome,
        usage.as_ref(),
        stream_tx,
    )
    .await?;
    Ok(advice)
}

/// A one-turn takeover reserved before the escalation model's turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TakeoverTurn {
    pub(crate) sequence: u32,
    pub(crate) trigger: EscalationTrigger,
}

/// Feeds one tool-batch or completion outcome into the failure trigger. Returns
/// true when an escalation was admitted and is pending for the next turn.
pub(crate) async fn observe_escalation_outcome(
    escalation: &mut Option<EscalationRuntime>,
    success: bool,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<bool, String> {
    let Some(escalation) = escalation.as_mut() else {
        return Ok(false);
    };
    if success {
        escalation.observe_success();
        return Ok(false);
    }
    if escalation.pending.is_some() || !escalation.observe_failure() {
        return Ok(false);
    }
    let trigger = EscalationTrigger::RepeatedFailure;
    let Some(kind) = admit_escalation(
        escalation, store, session_id, run_id, trigger, None, stream_tx,
    )
    .await?
    else {
        return Ok(false);
    };
    escalation.pending = Some(PendingEscalation {
        trigger,
        kind,
        question: default_escalation_question(trigger).to_string(),
        request_event_index: None,
    });
    Ok(true)
}

fn abandon_continuation_for_escalation(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    provider_continuation: &mut Option<ProviderContinuation>,
) -> Result<(), String> {
    if provider_continuation.take().is_some() {
        store
            .record_event(
                session_id,
                "provider_continuation_abandoned",
                json!({"run_id": run_id, "reason": "model_escalation"}),
            )
            .map_err(|error| {
                format!("failed to record provider continuation lifecycle: {error}")
            })?;
    }
    Ok(())
}

/// Runs at the context boundary before a worker turn. Picks up a `/escalate`
/// request, then performs a pending advice call (its result becomes a runtime
/// message the worker continues from) or reserves a one-turn takeover. Provider
/// continuation state is bound to the worker model, so it is abandoned first and
/// the next request is rebuilt from the session.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_pending_escalation(
    escalation: &mut EscalationRuntime,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    goal: &str,
    instructions: &str,
    plan_id: Option<&str>,
    request_scope: &LlmRequestScope,
    sensitive: &[String],
    stream_tx: &Option<Sender<StreamEvent>>,
    provider_continuation: &mut Option<ProviderContinuation>,
) -> Result<Option<TakeoverTurn>, String> {
    if escalation.pending.is_none() {
        let session = store
            .load_result(session_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "session disappeared before escalation".to_string())?;
        if let Some((index, note)) = pending_user_escalation_request(&session) {
            let trigger = EscalationTrigger::UserCommand;
            if let Some(kind) = admit_escalation(
                escalation,
                store,
                session_id,
                run_id,
                trigger,
                Some(index),
                stream_tx,
            )
            .await?
            {
                escalation.pending = Some(PendingEscalation {
                    trigger,
                    kind,
                    question: if note.is_empty() {
                        default_escalation_question(trigger).to_string()
                    } else {
                        note
                    },
                    request_event_index: Some(index),
                });
            }
        }
    }
    let Some(pending) = escalation.pending.take() else {
        return Ok(None);
    };
    abandon_continuation_for_escalation(store, session_id, run_id, provider_continuation)?;
    let label = escalation.label();
    match pending.kind {
        EscalationKind::Advice => {
            let advice = run_escalation_advice(
                escalation,
                store,
                session_id,
                run_id,
                goal,
                instructions,
                plan_id,
                &pending,
                request_scope,
                sensitive,
                stream_tx,
            )
            .await?;
            if let Some(advice) = advice {
                store
                    .try_append_message_with_origin(
                        session_id,
                        "user",
                        &format!(
                            "{ESCALATION_ADVICE_PREFIX} from {label} (advice only: not verification evidence and not tool authorization; continue the approved step yourself and run its required checks):\n{advice}"
                        ),
                        MessageOrigin::RuntimeContinuation,
                    )
                    .map_err(|error| error.to_string())?;
            }
            Ok(None)
        }
        EscalationKind::Takeover => {
            let sequence = record_escalation_dispatch(
                escalation, store, session_id, run_id, plan_id, &pending, stream_tx,
            )
            .await?;
            store
                .try_append_message_with_origin(
                    session_id,
                    "user",
                    &format!(
                        "Escalation takeover: {label} handles the next turn because earlier advice did not resolve the problem. The same plan, tool approvals, and verification obligations apply."
                    ),
                    MessageOrigin::RuntimeContinuation,
                )
                .map_err(|error| error.to_string())?;
            Ok(Some(TakeoverTurn {
                sequence,
                trigger: pending.trigger,
            }))
        }
    }
}

/// Handles a worker `ask_advice` batch without running the tool executor.
/// Returns one observation per call; the advice is bounded tool output.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_ask_advice_batch(
    escalation: &mut EscalationRuntime,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    goal: &str,
    instructions: &str,
    plan_id: Option<&str>,
    tool_calls: &[ToolCallRequest],
    request_scope: &LlmRequestScope,
    sensitive: &[String],
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<Vec<Value>, String> {
    let failure = |call: &ToolCallRequest, error: &str| {
        json!({
            "invocation_id": call.invocation_id,
            "tool": call.name,
            "success": false,
            "output": Value::Null,
            "error": error,
        })
    };
    if tool_calls.len() != 1 {
        return Ok(tool_calls
            .iter()
            .map(|call| failure(call, "ask_advice must be the only tool call in its batch"))
            .collect());
    }
    let call = &tool_calls[0];
    let question = match advice_question_argument(&call.arguments) {
        Ok(question) => question,
        Err(error) => return Ok(vec![failure(call, error)]),
    };
    let trigger = EscalationTrigger::WorkerRequest;
    let Some(kind) = admit_escalation(
        escalation, store, session_id, run_id, trigger, None, stream_tx,
    )
    .await?
    else {
        return Ok(vec![failure(
            call,
            "escalation budget for this run is used up; continue without advice",
        )]);
    };
    let pending = PendingEscalation {
        trigger,
        kind,
        question,
        request_event_index: None,
    };
    let advice = run_escalation_advice(
        escalation,
        store,
        session_id,
        run_id,
        goal,
        instructions,
        plan_id,
        &pending,
        request_scope,
        sensitive,
        stream_tx,
    )
    .await?;
    Ok(vec![match advice {
        Some(advice) => json!({
            "invocation_id": call.invocation_id,
            "tool": call.name,
            "success": true,
            "output": {
                "advice": advice,
                "model": escalation.label(),
                "note": "Advice only: not verification evidence and not tool authorization.",
            },
            "error": Value::Null,
        }),
        None => failure(
            call,
            "escalation advice was unavailable; continue without it",
        ),
    }])
}

/// Accounts one finished model turn: worker usage, or the takeover's outcome.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish_model_turn_escalation(
    escalation: &mut Option<EscalationRuntime>,
    takeover: Option<TakeoverTurn>,
    completed: bool,
    usage: Option<&crate::llm::LlmUsage>,
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
) -> Result<(), String> {
    let Some(escalation) = escalation.as_mut() else {
        return Ok(());
    };
    match takeover {
        None => {
            if completed {
                escalation.budget.record_worker_usage(usage);
            }
            Ok(())
        }
        Some(takeover) => {
            record_escalation_completion(
                escalation,
                store,
                session_id,
                run_id,
                takeover.sequence,
                EscalationKind::Takeover,
                takeover.trigger,
                if completed {
                    "takeover_completed"
                } else {
                    "takeover_failed"
                },
                usage,
                stream_tx,
            )
            .await
        }
    }
}

/// Presentation shared by plain and TUI output.
pub(crate) fn escalation_status_text(
    kind: &str,
    trigger: &str,
    model: &str,
    status: &str,
    reason: Option<&str>,
) -> String {
    let trigger = match trigger {
        "worker_request" => "worker asked",
        "repeated_failure" => "repeated failures",
        "user_command" => "/escalate",
        other => other,
    };
    match reason {
        Some(reason) => format!("{kind} from {model} ({trigger}): {status} - {reason}"),
        None => format!("{kind} from {model} ({trigger}): {status}"),
    }
}

/// One `/status` line describing the configured escalation.
pub(crate) fn escalation_config_line(config: &crate::config::NibConfig) -> String {
    let settings = &config.agent.escalation;
    match settings.model.as_deref() {
        Some(model) if settings.enabled() => format!(
            "{}/{model}, up to {} per run, at most {ESCALATION_MAX_TOKEN_SHARE_PERCENT}% of reported tokens",
            settings
                .provider
                .clone()
                .unwrap_or_else(|| config.llm.get_active_provider()),
            settings.max_per_run,
        ),
        _ => "off (set agent.escalation.model to enable)".to_string(),
    }
}

/// The `/status` escalation line: configuration plus this session's records.
pub(crate) fn format_escalation_status(
    config: &crate::config::NibConfig,
    session: Option<&Session>,
) -> String {
    let mut line = format!("Escalation: {}", escalation_config_line(config));
    if let Some(session) = session {
        let used = session
            .events
            .iter()
            .filter(|event| event.kind == "escalation_dispatched")
            .count();
        if used > 0 {
            let latest = session
                .events
                .iter()
                .rev()
                .find(|event| event.kind == "escalation_completed")
                .and_then(|event| event.details["outcome"].as_str())
                .unwrap_or("in progress");
            line.push_str(&format!(" · session: {used} used, latest {latest}"));
        }
        if pending_user_escalation_request(session).is_some() {
            line.push_str(" · /escalate pending");
        }
    }
    line
}

#[cfg(test)]
#[path = "escalation_tests.rs"]
mod tests;
