//! T043 split.

use super::*;

impl Plan {
    pub fn new(goal: &str, mut steps: Vec<PlanStep>) -> Self {
        let id = format!("plan-{}", Uuid::new_v4().simple());
        for (step_index, step) in steps.iter_mut().enumerate() {
            for obligation in &mut step.verification_obligations {
                obligation.plan_id.clone_from(&id);
                obligation.step_index = Some(step_index);
            }
        }
        Self {
            id,
            goal: normalize_plan_goal(goal),
            steps,
            current_step_index: 0,
            approved: false,
            approved_at: None,
            outcome: None,
        }
    }

    pub fn has_identity(&self) -> bool {
        !self.id.trim().is_empty() && !self.goal.trim().is_empty()
    }

    pub fn matches_goal(&self, goal: &str) -> bool {
        self.has_identity() && self.goal == normalize_plan_goal(goal)
    }

    pub fn is_resumable_for(&self, goal: &str) -> bool {
        self.is_structured() && !self.is_complete() && self.matches_goal(goal)
    }

    pub fn is_structured(&self) -> bool {
        if !self.has_identity()
            || self.steps.is_empty()
            || self.current_step_index > self.steps.len()
            || self
                .steps
                .iter()
                .any(|step| step.description.trim().is_empty())
            || self.validate_verification_metadata().is_err()
            || self.steps[..self.current_step_index].iter().any(|step| {
                step.status != "Completed"
                    || step
                        .verification_obligations
                        .iter()
                        .any(VerificationObligation::is_unresolved_required)
            })
        {
            return false;
        }
        if self.current_step_index == self.steps.len() {
            return self.steps.iter().all(|step| step.status == "Completed");
        }

        let current_status = self.steps[self.current_step_index].status.as_str();
        let current_is_valid = if self.approved {
            matches!(current_status, "InProgress" | "Blocked")
        } else {
            current_status == "Pending"
        };
        current_is_valid
            && self.steps[self.current_step_index + 1..]
                .iter()
                .all(|step| step.status == "Pending")
    }

    pub fn is_complete(&self) -> bool {
        self.current_step_index >= self.steps.len()
            && self.steps.iter().all(|step| step.status == "Completed")
    }

    pub fn validate_verification_metadata(&self) -> Result<(), String> {
        let mut obligation_ids = HashSet::new();
        for (step_index, step) in self.steps.iter().enumerate() {
            for obligation in &step.verification_obligations {
                if obligation.id.trim().is_empty() || obligation.id.len() > 128 {
                    return Err(format!(
                        "plan step {step_index} has an invalid verification obligation id"
                    ));
                }
                if obligation.description.trim().is_empty() {
                    return Err(format!(
                        "verification obligation {:?} has an empty description",
                        obligation.id
                    ));
                }
                if !obligation_ids.insert(obligation.id.clone()) {
                    return Err(format!(
                        "duplicate verification obligation id {:?}",
                        obligation.id
                    ));
                }
                if obligation.plan_id != self.id || obligation.step_index != Some(step_index) {
                    return Err(format!(
                        "verification obligation {:?} is not bound to its plan step",
                        obligation.id
                    ));
                }
                let Some(expected) = obligation.expected_invocation.as_ref() else {
                    return Err(format!(
                        "verification obligation {:?} has no independently persisted invocation contract",
                        obligation.id
                    ));
                };
                if expected.tool_name.trim().is_empty()
                    || normalize_verification_arguments(&expected.arguments)? != expected.arguments
                {
                    return Err(format!(
                        "verification obligation {:?} has an invalid invocation contract",
                        obligation.id
                    ));
                }
                if obligation.status == VerificationStatus::Waived
                    && obligation.waiver_source_message_index.is_none()
                {
                    return Err(format!(
                        "waived verification obligation {:?} has no human source message",
                        obligation.id
                    ));
                }
                if matches!(
                    obligation.status,
                    VerificationStatus::Running
                        | VerificationStatus::Passed
                        | VerificationStatus::Failed
                        | VerificationStatus::Cancelled
                        | VerificationStatus::Stale
                ) && obligation.invocation_id.is_none()
                {
                    return Err(format!(
                        "verification obligation {:?} has status {:?} without an invocation binding",
                        obligation.id, obligation.status
                    ));
                }
                if obligation.content_generation > step.content_generation {
                    return Err(format!(
                        "verification obligation {:?} refers to future content generation {}",
                        obligation.id, obligation.content_generation
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn approve(&mut self) {
        self.approved = true;
        self.approved_at = Some(Utc::now());
        if let Some(step) = self.steps.get_mut(self.current_step_index) {
            step.status = "InProgress".to_string();
            step.attempts = step.attempts.saturating_add(1);
            step.updated_at = Some(Utc::now());
        }
    }

    pub fn reject(&mut self, reason: impl Into<String>) {
        self.approved = false;
        self.outcome = Some(reason.into());
    }

    pub fn record_tool_outcome(&mut self, success: bool, outcome: impl Into<String>) {
        let Some(step) = self.steps.get_mut(self.current_step_index) else {
            return;
        };
        let outcome = outcome.into();
        let unresolved = step
            .verification_obligations
            .iter()
            .any(VerificationObligation::is_unresolved_required);
        step.status = if success && !unresolved {
            "InProgress"
        } else {
            "Blocked"
        }
        .to_string();
        step.outcome = Some(if success && unresolved {
            format!("{outcome}; required verification remains unresolved")
        } else {
            outcome
        });
        step.updated_at = Some(Utc::now());
    }

    pub fn begin_verification(
        &mut self,
        obligation_id: &str,
        invocation_id: crate::tools::ToolInvocationId,
        tool_name: &str,
        arguments: &serde_json::Value,
        worktree_identity: Option<String>,
    ) -> Result<(), String> {
        let step_index = self.current_step_index;
        let step = self
            .steps
            .get_mut(step_index)
            .ok_or_else(|| "verification has no active plan step".to_string())?;
        let declared = declared_obligation_ids(step);
        let obligation = step
            .verification_obligations
            .iter_mut()
            .find(|obligation| obligation.id == obligation_id)
            .ok_or_else(|| {
                format!(
                    "verification obligation {obligation_id:?} is not declared on active step {step_index}; declared: {declared}"
                )
            })?;
        if obligation.plan_id != self.id || obligation.step_index != Some(step_index) {
            return Err(format!(
                "verification obligation {obligation_id:?} is not bound to the active plan step"
            ));
        }
        let actual_arguments = normalize_verification_arguments(arguments)?;
        let expected = obligation.expected_invocation.as_ref().ok_or_else(|| {
            format!("verification obligation {obligation_id:?} has no trusted invocation contract")
        })?;
        if expected.tool_name != tool_name || expected.arguments != actual_arguments {
            return Err(format!(
                "verification obligation {obligation_id:?} does not match the persisted tool invocation contract"
            ));
        }
        obligation.status = VerificationStatus::Running;
        obligation.invocation_id = Some(invocation_id);
        obligation.worktree_identity = worktree_identity;
        obligation.content_identity = None;
        obligation.content_generation = step.content_generation;
        obligation.reason = None;
        obligation.updated_at = Some(Utc::now());
        step.status = "InProgress".to_string();
        step.updated_at = Some(Utc::now());
        Ok(())
    }

    pub fn finish_verification(
        &mut self,
        obligation_id: &str,
        invocation_id: crate::tools::ToolInvocationId,
        worktree_identity: Option<&str>,
        content_identity: Option<String>,
        success: bool,
        reason: Option<String>,
    ) -> Result<(), String> {
        let step_index = self.current_step_index;
        let step = self
            .steps
            .get_mut(step_index)
            .ok_or_else(|| "verification has no active plan step".to_string())?;
        let declared = declared_obligation_ids(step);
        let obligation = step
            .verification_obligations
            .iter_mut()
            .find(|obligation| obligation.id == obligation_id)
            .ok_or_else(|| {
                format!(
                    "verification obligation {obligation_id:?} is not declared on active step {step_index}; declared: {declared}"
                )
            })?;
        if obligation.invocation_id != Some(invocation_id)
            || obligation.status != VerificationStatus::Running
            || obligation
                .worktree_identity
                .as_deref()
                .is_some_and(|expected| Some(expected) != worktree_identity)
        {
            return Err(format!(
                "verification obligation {obligation_id:?} is not bound to invocation {invocation_id} in the active worktree"
            ));
        }
        if success && worktree_identity.is_none() {
            return Err(format!(
                "successful verification obligation {obligation_id:?} has no audited worktree identity"
            ));
        }
        if obligation.worktree_identity.is_none() {
            obligation.worktree_identity = worktree_identity.map(str::to_string);
        }
        obligation.content_identity = content_identity.clone();
        obligation.status = if success {
            VerificationStatus::Passed
        } else {
            VerificationStatus::Failed
        };
        obligation.content_generation = step.content_generation;
        obligation.reason = reason;
        obligation.updated_at = Some(Utc::now());
        obligation.attempts.push(VerificationAttempt {
            invocation_id,
            status: obligation.status,
            worktree_identity: obligation.worktree_identity.clone(),
            content_identity,
            reason: obligation.reason.clone(),
            recorded_at: Utc::now(),
        });
        if !success {
            step.status = "Blocked".to_string();
        }
        step.updated_at = Some(Utc::now());
        Ok(())
    }

    pub fn invalidate_verification_after_mutation(
        &mut self,
        invocation_id: crate::tools::ToolInvocationId,
    ) -> Vec<String> {
        let Some(step) = self.steps.get_mut(self.current_step_index) else {
            return Vec::new();
        };
        step.content_generation = step.content_generation.saturating_add(1);
        let mut invalidated = Vec::new();
        for obligation in &mut step.verification_obligations {
            if obligation.status == VerificationStatus::Passed
                && obligation.invocation_id != Some(invocation_id)
            {
                obligation.status = VerificationStatus::Stale;
                obligation.reason = Some("relevant worktree content changed".to_string());
                obligation.updated_at = Some(Utc::now());
                invalidated.push(obligation.id.clone());
            }
        }
        if !invalidated.is_empty() {
            step.status = "Blocked".to_string();
            step.outcome = Some("passed verification became stale after mutation".to_string());
            step.updated_at = Some(Utc::now());
        }
        invalidated
    }

    pub fn cancel_running_verifications(&mut self, reason: &str) -> Vec<String> {
        let Some(step) = self.steps.get_mut(self.current_step_index) else {
            return Vec::new();
        };
        let mut cancelled = Vec::new();
        for obligation in &mut step.verification_obligations {
            if obligation.status == VerificationStatus::Running {
                obligation.status = VerificationStatus::Cancelled;
                obligation.reason = Some(reason.to_string());
                obligation.updated_at = Some(Utc::now());
                cancelled.push(obligation.id.clone());
            }
        }
        cancelled
    }

    pub fn waive_verification(
        &mut self,
        expected_plan_id: &str,
        obligation_id: &str,
        source_message_index: usize,
        reason: impl Into<String>,
    ) -> Result<(), String> {
        if self.id != expected_plan_id {
            return Err("verification waiver does not match the active plan".to_string());
        }
        let step_index = self.current_step_index;
        let step = self
            .steps
            .get_mut(step_index)
            .ok_or_else(|| "verification waiver has no active plan step".to_string())?;
        let declared = declared_obligation_ids(step);
        let obligation = step
            .verification_obligations
            .iter_mut()
            .find(|obligation| obligation.id == obligation_id)
            .ok_or_else(|| {
                format!(
                    "verification obligation {obligation_id:?} is not declared on active step {step_index}; declared: {declared}"
                )
            })?;
        if obligation.authority == VerificationAuthority::Project {
            return Err(format!(
                "project verification obligation {obligation_id:?} cannot be waived by a task scope change"
            ));
        }
        if matches!(
            obligation.status,
            VerificationStatus::Running | VerificationStatus::Passed | VerificationStatus::Failed
        ) {
            return Err(format!(
                "verification obligation {obligation_id:?} with recorded execution evidence cannot be waived"
            ));
        }
        obligation.status = VerificationStatus::Waived;
        obligation.reason = Some(reason.into());
        obligation.waiver_source_message_index = Some(source_message_index);
        obligation.updated_at = Some(Utc::now());
        Ok(())
    }

    pub fn unresolved_verification_ids(&self) -> Vec<String> {
        self.steps
            .get(self.current_step_index)
            .map(|step| {
                step.verification_obligations
                    .iter()
                    .filter(|obligation| obligation.is_unresolved_required())
                    .map(|obligation| obligation.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn complete_current_step(&mut self, outcome: impl Into<String>) {
        let Some(step) = self.steps.get_mut(self.current_step_index) else {
            return;
        };
        if step
            .verification_obligations
            .iter()
            .any(VerificationObligation::is_unresolved_required)
        {
            step.status = "Blocked".to_string();
            step.outcome = Some("required verification remains unresolved".to_string());
            step.updated_at = Some(Utc::now());
            self.outcome = Some("verification_unresolved".to_string());
            return;
        }
        step.status = "Completed".to_string();
        step.outcome = Some(outcome.into());
        step.updated_at = Some(Utc::now());
        self.current_step_index += 1;
        if let Some(next) = self.steps.get_mut(self.current_step_index) {
            next.status = "InProgress".to_string();
            next.attempts = next.attempts.saturating_add(1);
            next.updated_at = Some(Utc::now());
        } else {
            self.outcome = Some("completed".to_string());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionEvent {
    #[serde(default)]
    pub index: usize,
    pub kind: String,
    #[serde(default)]
    pub details: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillUsageRecord {
    pub skill_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    #[serde(default, skip_serializing_if = "revision_is_zero")]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub messages: Vec<SessionMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub message_provenance: Vec<MessageProvenance>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub human_intent: Vec<HumanIntentRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clarifications: Vec<ClarificationRecord>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub summary_index: usize,
    #[serde(default)]
    pub events: Vec<SessionEvent>,
    #[serde(default)]
    pub active_skills: Vec<String>,
    #[serde(default)]
    pub skill_usage: Vec<SkillUsageRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_follow_ups: Vec<QueuedFollowUp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// Session permission mode chosen with Shift+Tab or `/mode` (`ask`,
    /// `accept-edits`, `plan`, `auto`, `policy`); overrides the configured
    /// `approvals.mode` for this session's runs (T080).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedFollowUp {
    pub id: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
    pub source: String,
}

pub(crate) fn revision_is_zero(revision: &u64) -> bool {
    *revision == 0
}

pub(crate) fn session_mismatch_field(expected: &Session, published: &Session) -> &'static str {
    if expected.id != published.id {
        return "id";
    }
    if expected.revision != published.revision {
        return "revision";
    }
    if expected.started_at != published.started_at {
        return "started_at";
    }
    if expected.messages != published.messages {
        return "messages";
    }
    if expected.message_provenance != published.message_provenance {
        return "message_provenance";
    }
    if expected.human_intent != published.human_intent {
        return "human_intent";
    }
    if expected.clarifications != published.clarifications {
        return "clarifications";
    }
    if expected.tool_calls.len() != published.tool_calls.len() {
        return "tool_calls.length";
    }
    for (expected, published) in expected.tool_calls.iter().zip(&published.tool_calls) {
        if expected.invocation_id != published.invocation_id {
            return "tool_calls.invocation_id";
        }
        if expected.id != published.id {
            return "tool_calls.id";
        }
        if expected.session_id != published.session_id {
            return "tool_calls.session_id";
        }
        if expected.tool_name != published.tool_name {
            return "tool_calls.tool_name";
        }
        if expected.arguments != published.arguments {
            return "tool_calls.arguments";
        }
        if expected.result != published.result {
            return "tool_calls.result";
        }
        if expected.error != published.error {
            return "tool_calls.error";
        }
        if expected.duration_seconds != published.duration_seconds {
            return "tool_calls.duration_seconds";
        }
        if expected.worktree_path != published.worktree_path {
            return "tool_calls.worktree_path";
        }
        if expected.timestamp != published.timestamp {
            return "tool_calls.timestamp";
        }
        if expected.provider != published.provider {
            return "tool_calls.provider";
        }
        if expected.sandbox_profile != published.sandbox_profile {
            return "tool_calls.sandbox_profile";
        }
        if expected.bwrap_args != published.bwrap_args {
            return "tool_calls.bwrap_args";
        }
        if expected.boundaries != published.boundaries {
            return "tool_calls.boundaries";
        }
        if expected.plan_id != published.plan_id {
            return "tool_calls.plan_id";
        }
    }
    if expected.plan != published.plan {
        return "plan";
    }
    if expected.summary != published.summary {
        return "summary";
    }
    if expected.summary_index != published.summary_index {
        return "summary_index";
    }
    if expected.events != published.events {
        return "events";
    }
    if expected.active_skills != published.active_skills {
        return "active_skills";
    }
    if expected.skill_usage != published.skill_usage {
        return "skill_usage";
    }
    if expected.queued_follow_ups != published.queued_follow_ups {
        return "queued_follow_ups";
    }
    if expected.display_name != published.display_name {
        return "display_name";
    }
    if expected.forked_from != published.forked_from {
        return "forked_from";
    }
    "unknown field"
}

impl Session {
    pub(crate) fn new(id: String) -> Self {
        Self {
            id,
            revision: 0,
            started_at: Some(Utc::now()),
            messages: vec![],
            message_provenance: vec![],
            human_intent: vec![],
            clarifications: vec![],
            tool_calls: vec![],
            plan: None,
            summary: None,
            summary_index: 0,
            events: vec![],
            active_skills: vec![],
            skill_usage: vec![],
            queued_follow_ups: vec![],
            display_name: None,
            forked_from: None,
            permission_mode: None,
        }
    }

    pub fn validate_message_sequence(&self) -> Result<(), SessionError> {
        let mut previous: Option<&str> = None;
        for (expected_index, message) in self.messages.iter().enumerate() {
            if message.index != expected_index {
                return Err(SessionError::InvalidMessageIndex {
                    expected: expected_index,
                    actual: message.index,
                });
            }
            validate_role_transition(previous, &message.role)?;
            previous = Some(&message.role);
        }
        Ok(())
    }

    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
    pub fn validate(&self) -> Result<(), SessionError> {
        validate_session_id(&self.id)?;
        self.validate_message_sequence()?;
        if let Some(plan) = &self.plan {
            plan.validate_verification_metadata()
                .map_err(SessionError::InvalidMutation)?;
        }
        for (expected_index, event) in self.events.iter().enumerate() {
            if event.index != expected_index {
                return Err(SessionError::InvalidEventIndex {
                    expected: expected_index,
                    actual: event.index,
                });
            }
        }
        if self.summary_index > self.messages.len() {
            return Err(SessionError::InvalidSummaryIndex {
                summary_index: self.summary_index,
                message_count: self.messages.len(),
            });
        }
        let mut previous = None;
        for provenance in &self.message_provenance {
            if provenance.message_index >= self.messages.len()
                || previous.is_some_and(|index| index >= provenance.message_index)
            {
                return Err(SessionError::InvalidMutation(
                    "message provenance must be uniquely ordered and reference an existing message"
                        .to_string(),
                ));
            }
            validate_message_origin_role(
                &self.messages[provenance.message_index].role,
                provenance.origin,
            )?;
            previous = Some(provenance.message_index);
        }
        for intent in &self.human_intent {
            if intent.text.trim().is_empty()
                || intent
                    .source_message_index
                    .is_some_and(|index| index >= self.messages.len())
                || intent
                    .source_event_index
                    .is_some_and(|index| index >= self.events.len())
                || (intent.source_message_index.is_none() && intent.source_event_index.is_none())
            {
                return Err(SessionError::InvalidMutation(
                    "human intent must have bounded content and an existing source".to_string(),
                ));
            }
            if let Some(index) = intent.source_message_index {
                if !self.message_origin(index).is_human() {
                    return Err(SessionError::InvalidMutation(
                        "human intent message source does not have human provenance".to_string(),
                    ));
                }
            }
            if let Some(index) = intent.source_event_index {
                if !matches!(
                    self.events[index].kind.as_str(),
                    "steering_input"
                        | "human_question_answer_received"
                        | "human_question_discussion_received"
                        | "plan_continue_requested"
                ) {
                    return Err(SessionError::InvalidMutation(
                        "human intent event source is not a trusted human-input boundary"
                            .to_string(),
                    ));
                }
            }
        }
        super::question_forms::validate_form_records(self)?;
        for clarification in &self.clarifications {
            let answer_source_is_human = clarification
                .answer_message_index
                .is_some_and(|index| self.message_origin(index).is_human())
                || clarification.answer_event_index.is_some_and(|index| {
                    self.events
                        .get(index)
                        .is_some_and(|event| event.kind == "human_question_answer_received")
                });
            if clarification.question.trim().is_empty()
                || clarification.question_event_index >= self.events.len()
                || clarification
                    .answer_message_index
                    .is_some_and(|index| index >= self.messages.len())
                || clarification
                    .answer_event_index
                    .is_some_and(|index| index >= self.events.len())
                || clarification.dependent_paths.len() > 32
                || clarification
                    .dependent_paths
                    .iter()
                    .any(|path| !valid_bounded_relative_path(path))
                || (clarification.status == ClarificationStatus::Answered
                    && (clarification.answer.as_deref().is_none_or(str::is_empty)
                        || !answer_source_is_human))
            {
                return Err(SessionError::InvalidMutation(
                    "clarification provenance is incomplete or inconsistent".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn message_origin(&self, index: usize) -> MessageOrigin {
        self.message_provenance
            .binary_search_by_key(&index, |provenance| provenance.message_index)
            .ok()
            .map(|position| self.message_provenance[position].origin)
            .unwrap_or(MessageOrigin::Unknown)
    }

    pub fn has_unresolved_clarification(&self, plan_id: Option<&str>) -> bool {
        self.clarifications.iter().any(|clarification| {
            matches!(
                clarification.status,
                ClarificationStatus::Pending
                    | ClarificationStatus::Unresolved
                    | ClarificationStatus::Cancelled
                    | ClarificationStatus::Discussed
            ) && clarification.plan_id.as_deref() == plan_id
        })
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse session JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid session message role: {0}")]
    InvalidRole(String),
    #[error("invalid session role transition: {previous:?} -> {next}")]
    RoleViolation {
        previous: Option<String>,
        next: String,
    },
    #[error("invalid session message index: expected {expected}, got {actual}")]
    InvalidMessageIndex { expected: usize, actual: usize },
    #[error("invalid session event index: expected {expected}, got {actual}")]
    InvalidEventIndex { expected: usize, actual: usize },
    #[error(
        "invalid session summary index: {summary_index} exceeds message count {message_count}"
    )]
    InvalidSummaryIndex {
        summary_index: usize,
        message_count: usize,
    },
    #[error("session file for {expected} contains session {actual}")]
    SessionIdMismatch { expected: String, actual: String },
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session lock was poisoned for {0}")]
    LockPoisoned(String),
    #[error("session already has an active agent run: {0}")]
    RunLeaseHeld(String),
    #[error("invalid session mutation: {0}")]
    InvalidMutation(String),
    #[error("invalid session id: {0}")]
    InvalidSessionId(String),
    #[error("session identifier conflicts with configured sensitive data")]
    SensitiveSessionId,
    #[error("session file {path} is {size} bytes; maximum is {max} bytes")]
    FileTooLarge { path: String, size: u64, max: u64 },
}

pub(crate) fn validate_role_transition(
    previous: Option<&str>,
    next: &str,
) -> Result<(), SessionError> {
    if !matches!(next, "user" | "assistant" | "tool") {
        return Err(SessionError::InvalidRole(next.to_string()));
    }
    let allowed = matches!(
        (previous, next),
        (None, "user")
            | (Some("user"), "user")
            | (Some("user"), "assistant")
            | (Some("assistant"), "user")
            | (Some("assistant"), "tool")
            | (Some("tool"), "assistant")
    );
    if allowed {
        Ok(())
    } else {
        Err(SessionError::RoleViolation {
            previous: previous.map(str::to_string),
            next: next.to_string(),
        })
    }
}

pub(crate) fn validate_message_origin_role(
    role: &str,
    origin: MessageOrigin,
) -> Result<(), SessionError> {
    let valid = match origin {
        MessageOrigin::Unknown => true,
        MessageOrigin::HumanRequest
        | MessageOrigin::HumanSteering
        | MessageOrigin::HumanQuestionAnswer
        | MessageOrigin::RuntimeContinuation => role == "user",
        MessageOrigin::ModelOutput => role == "assistant",
        // A subagent-directed message uses a provider `user` role, but its
        // persisted origin remains non-human. Ordinary tool observations keep
        // the provider `tool` role.
        MessageOrigin::ToolOutput => matches!(role, "user" | "tool"),
    };
    if valid {
        Ok(())
    } else {
        Err(SessionError::InvalidMutation(format!(
            "message origin {origin:?} is incompatible with provider role {role}"
        )))
    }
}

pub(crate) fn valid_bounded_relative_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

pub(crate) fn validate_session_id(id: &str) -> Result<(), SessionError> {
    if id.is_empty()
        || id.len() > 128
        || matches!(id, "." | "..")
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(SessionError::InvalidSessionId(id.to_string()));
    }
    Ok(())
}

pub(crate) type SessionMutex = Mutex<()>;
pub(crate) type SessionLockRegistry = Mutex<HashMap<PathBuf, Weak<SessionMutex>>>;

pub(crate) fn lock_session_mutex<'a, T>(
    mutex: &'a Mutex<T>,
    path: &Path,
    deadline: Option<Instant>,
) -> Result<std::sync::MutexGuard<'a, T>, SessionError> {
    let Some(deadline) = deadline else {
        return mutex
            .lock()
            .map_err(|_| SessionError::LockPoisoned(path.display().to_string()));
    };

    loop {
        ensure_session_lock_deadline(Some(deadline), path)?;
        match mutex.try_lock() {
            Ok(guard) => {
                ensure_session_lock_deadline(Some(deadline), path)?;
                return Ok(guard);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(SessionError::LockPoisoned(path.display().to_string()));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(SessionError::InvalidMutation(format!(
                        "timed out acquiring session lock: {}",
                        path.display()
                    )));
                }
                std::thread::sleep(SESSION_LOCK_POLL_INTERVAL.min(deadline - now));
            }
        }
    }
}

pub(crate) fn ensure_session_lock_deadline(
    deadline: Option<Instant>,
    path: &Path,
) -> Result<(), SessionError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(SessionError::InvalidMutation(format!(
            "timed out acquiring session lock: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) static SESSION_LOCKS: OnceLock<SessionLockRegistry> = OnceLock::new();
pub(crate) static SESSION_PREPARATION_LOCKS: OnceLock<SessionLockRegistry> = OnceLock::new();

pub(crate) fn session_preparation_mutex(
    path: &Path,
    deadline: Option<Instant>,
) -> Result<Arc<SessionMutex>, String> {
    let registry = SESSION_PREPARATION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry =
        lock_session_mutex(registry, path, deadline).map_err(|error| error.to_string())?;
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}
pub(crate) const MAX_SESSION_JSON_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_LISTED_SESSIONS: usize = 10_000;
pub(crate) const MAX_SESSION_DIRECTORY_ENTRIES: usize =
    MAX_LISTED_SESSIONS + SESSION_LOCK_STRIPES + 16;
pub(crate) const MAX_SESSION_DIRECTORY_NAME_BYTES: usize = MAX_SESSION_DIRECTORY_ENTRIES * 256;
pub(crate) const SESSION_LOCK_STRIPES: usize = 64;
pub(crate) const SESSION_DIRECTORY_IDENTITY_FILE: &str = ".session-directory.identity";
pub(crate) const SKILL_USAGE_LOCK_FILE: &str = ".skill-usage.lock";
pub(crate) const OPERATION_ERROR_SENTINEL: &str = "session operation failed under stable lock";
pub(crate) const SESSION_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy)]
pub(crate) struct SessionLockPolicy {
    pub(crate) timeout: Duration,
    pub(crate) offload_waits: bool,
}

tokio::task_local! {
    pub(crate) static SESSION_LOCK_POLICY: SessionLockPolicy;
}

thread_local! {
    pub(crate) static SESSION_LOCK_OFFLOAD_DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub(crate) struct SessionLockOffloadGuard;

impl SessionLockOffloadGuard {
    pub(crate) fn enter() -> Self {
        SESSION_LOCK_OFFLOAD_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for SessionLockOffloadGuard {
    fn drop(&mut self) {
        SESSION_LOCK_OFFLOAD_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    pub(crate) sessions_dir: PathBuf,
    pub(crate) directory: Option<Arc<crate::daemons::state::StableDirectory>>,
    pub(crate) parent_directory: Option<Arc<crate::daemons::state::StableDirectory>>,
    pub(crate) directory_identity_file: Option<Arc<File>>,
    pub(crate) initialization_error: Option<String>,
    pub(crate) lock_timeout: Option<Duration>,
    pub(crate) lock_deadline: Option<Instant>,
    pub(crate) sensitive_values: Arc<Vec<String>>,
}

pub(crate) struct SessionRunLease {
    pub(crate) session_id: String,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) lock: crate::daemons::state::HeldFileLock,
}

impl SessionRunLease {
    pub(crate) fn verify(&self) -> Result<(), SessionError> {
        self.lock.verify().map_err(|error| {
            SessionError::InvalidMutation(format!(
                "active run lease for {} became unsafe: {error}",
                self.session_id
            ))
        })
    }

    pub(crate) fn verify_for(
        &self,
        session_id: &str,
        sessions_dir: &Path,
    ) -> Result<(), SessionError> {
        if self.session_id != session_id {
            return Err(SessionError::InvalidMutation(format!(
                "active run lease for {} cannot authorize session {session_id}",
                self.session_id
            )));
        }
        if !crate::fs_security::canonical_paths_match(&self.sessions_dir, sessions_dir) {
            return Err(SessionError::InvalidMutation(format!(
                "active run lease for {} cannot authorize session directory {}",
                self.session_id,
                sessions_dir.display()
            )));
        }
        self.verify()
    }
}

#[derive(Debug)]
pub(crate) struct OpenedSession {
    pub(crate) session: Session,
    pub(crate) file: File,
    pub(crate) metadata: fs::Metadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDeleteOutcome {
    Missing,
    Retained,
    Deleted,
}

/// Lists a step's declared verification obligation ids for error messages, so
/// a model that cites an unknown id can correct itself (T081).
fn declared_obligation_ids(step: &PlanStep) -> String {
    if step.verification_obligations.is_empty() {
        return "none".to_string();
    }
    let ids: Vec<String> = step
        .verification_obligations
        .iter()
        .map(|obligation| format!("{:?}", obligation.id))
        .collect();
    format!("[{}]", ids.join(", "))
}
