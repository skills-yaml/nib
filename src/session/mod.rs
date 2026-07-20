//! File-based session persistence under the active profile's sessions directory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

pub mod memory;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMessage {
    #[serde(default)]
    pub index: usize,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ToolCallRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub arguments: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bwrap_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<crate::config::BoundaryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub current_step_index: usize,
    #[serde(default)]
    pub approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

pub fn normalize_plan_goal(goal: &str) -> String {
    goal.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Plan {
    pub fn new(goal: &str, steps: Vec<PlanStep>) -> Self {
        Self {
            id: format!("plan-{}", Uuid::new_v4().simple()),
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
            || self.steps[..self.current_step_index]
                .iter()
                .any(|step| step.status != "Completed")
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
        step.status = if success { "InProgress" } else { "Blocked" }.to_string();
        step.outcome = Some(outcome.into());
        step.updated_at = Some(Utc::now());
    }

    pub fn complete_current_step(&mut self, outcome: impl Into<String>) {
        let Some(step) = self.steps.get_mut(self.current_step_index) else {
            return;
        };
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
}

fn revision_is_zero(revision: &u64) -> bool {
    *revision == 0
}

fn session_mismatch_field(expected: &Session, published: &Session) -> &'static str {
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
    if expected.tool_calls.len() != published.tool_calls.len() {
        return "tool_calls.length";
    }
    for (expected, published) in expected.tool_calls.iter().zip(&published.tool_calls) {
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
    "unknown field"
}

impl Session {
    fn new(id: String) -> Self {
        Self {
            id,
            revision: 0,
            started_at: Some(Utc::now()),
            messages: vec![],
            tool_calls: vec![],
            plan: None,
            summary: None,
            summary_index: 0,
            events: vec![],
            active_skills: vec![],
            skill_usage: vec![],
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

    pub fn validate(&self) -> Result<(), SessionError> {
        validate_session_id(&self.id)?;
        self.validate_message_sequence()?;
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
        Ok(())
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
    #[error("session file {path} is {size} bytes; maximum is {max} bytes")]
    FileTooLarge { path: String, size: u64, max: u64 },
}

fn validate_role_transition(previous: Option<&str>, next: &str) -> Result<(), SessionError> {
    if !matches!(next, "user" | "assistant" | "tool") {
        return Err(SessionError::InvalidRole(next.to_string()));
    }
    let allowed = matches!(
        (previous, next),
        (None, "user")
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

type SessionMutex = Mutex<()>;
type SessionLockRegistry = Mutex<HashMap<PathBuf, Weak<SessionMutex>>>;

fn lock_session_mutex<'a>(
    mutex: &'a SessionMutex,
    path: &Path,
    deadline: Option<Instant>,
) -> Result<std::sync::MutexGuard<'a, ()>, SessionError> {
    let Some(deadline) = deadline else {
        return mutex
            .lock()
            .map_err(|_| SessionError::LockPoisoned(path.display().to_string()));
    };

    loop {
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
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

static SESSION_LOCKS: OnceLock<SessionLockRegistry> = OnceLock::new();
const MAX_SESSION_JSON_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LISTED_SESSIONS: usize = 10_000;
const MAX_SESSION_DIRECTORY_ENTRIES: usize = MAX_LISTED_SESSIONS + SESSION_LOCK_STRIPES + 16;
const MAX_SESSION_DIRECTORY_NAME_BYTES: usize = MAX_SESSION_DIRECTORY_ENTRIES * 256;
const SESSION_LOCK_STRIPES: usize = 64;
const SESSION_DIRECTORY_IDENTITY_FILE: &str = ".session-directory.identity";
const SKILL_USAGE_LOCK_FILE: &str = ".skill-usage.lock";
const OPERATION_ERROR_SENTINEL: &str = "session operation failed under stable lock";
const SESSION_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy)]
pub(crate) struct SessionLockPolicy {
    timeout: Duration,
    offload_waits: bool,
}

tokio::task_local! {
    static SESSION_LOCK_POLICY: SessionLockPolicy;
}

thread_local! {
    static SESSION_LOCK_OFFLOAD_DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct SessionLockOffloadGuard;

impl SessionLockOffloadGuard {
    fn enter() -> Self {
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
    sessions_dir: PathBuf,
    directory: Option<Arc<crate::daemons::state::StableDirectory>>,
    parent_directory: Option<Arc<crate::daemons::state::StableDirectory>>,
    directory_identity_file: Option<Arc<File>>,
    initialization_error: Option<String>,
    lock_timeout: Option<Duration>,
}

pub(crate) struct SessionRunLease {
    session_id: String,
    lock: crate::daemons::state::HeldFileLock,
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
}

#[derive(Debug)]
struct OpenedSession {
    session: Session,
    file: File,
    metadata: fs::Metadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDeleteOutcome {
    Missing,
    Retained,
    Deleted,
}

impl SessionStore {
    pub(crate) async fn with_lock_policy<F>(timeout: Duration, future: F) -> F::Output
    where
        F: Future,
    {
        SESSION_LOCK_POLICY
            .scope(
                SessionLockPolicy {
                    timeout,
                    offload_waits: true,
                },
                future,
            )
            .await
    }

    pub(crate) fn current_lock_policy() -> Option<SessionLockPolicy> {
        SESSION_LOCK_POLICY.try_with(|policy| *policy).ok()
    }

    pub(crate) async fn with_optional_lock_policy<F>(
        policy: Option<SessionLockPolicy>,
        future: F,
    ) -> F::Output
    where
        F: Future,
    {
        match policy {
            Some(policy) => SESSION_LOCK_POLICY.scope(policy, future).await,
            None => future.await,
        }
    }

    pub fn new(project_root: &Path) -> Self {
        let nib = project_root.join(".nib");
        let sessions_dir = nib.join("sessions");
        Self::at_dir(sessions_dir)
    }

    pub fn for_project(project_root: &Path) -> Result<Self, String> {
        let config =
            crate::config::load_nib_config_full(project_root).map_err(|error| error.to_string())?;
        let profiles = crate::profile::ProfileRegistry::load(project_root, &config.profiles)
            .map_err(|error| error.to_string())?;
        let profile = profiles
            .for_workspace(project_root)
            .unwrap_or_else(|| profiles.default_profile());
        profile
            .ensure_state_dirs()
            .map_err(|error| error.to_string())?;
        Ok(Self::at_dir(profile.sessions_dir().to_path_buf()))
    }

    pub fn at_dir(sessions_dir: PathBuf) -> Self {
        let requested = crate::fs_security::absolute_path(&sessions_dir)
            .unwrap_or_else(|_| sessions_dir.clone());
        match open_session_directory(&requested) {
            Ok((sessions_dir, directory, parent_directory, identity_file)) => Self {
                sessions_dir,
                directory: Some(Arc::new(directory)),
                parent_directory: Some(Arc::new(parent_directory)),
                directory_identity_file: Some(Arc::new(identity_file)),
                initialization_error: None,
                lock_timeout: None,
            },
            Err(error) => Self {
                sessions_dir: requested,
                directory: None,
                parent_directory: None,
                directory_identity_file: None,
                initialization_error: Some(format!(
                    "session directory is unsafe or unavailable: {error}"
                )),
                lock_timeout: None,
            },
        }
    }

    pub(crate) fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = Some(timeout);
        self
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub(crate) fn try_acquire_run_lease(&self, id: &str) -> Result<SessionRunLease, SessionError> {
        validate_session_id(id)?;
        self.verify_directory_binding()?;
        let lock_path = self.sessions_dir.join(format!(".session-run-{id}.lock"));
        let lock = crate::daemons::state::try_acquire_file_lock_in(&lock_path, &self.sessions_dir)
            .map_err(|error| {
                if error.contains("already held by another owner") {
                    SessionError::RunLeaseHeld(id.to_string())
                } else {
                    SessionError::InvalidMutation(format!(
                        "failed to acquire active run lease for {id}: {error}"
                    ))
                }
            })?;
        self.verify_directory_binding()?;
        Ok(SessionRunLease {
            session_id: id.to_string(),
            lock,
        })
    }

    fn path(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{id}.json"))
    }

    fn lock_path(&self, id: &str) -> Result<PathBuf, SessionError> {
        validate_session_id(id)?;
        let stripe = session_lock_stripe(id);
        Ok(self
            .sessions_dir
            .join(format!(".session-lock-{stripe:02}.lock")))
    }

    fn process_lock(&self, path: &Path) -> Result<Arc<SessionMutex>, SessionError> {
        let registry = SESSION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut registry = registry
            .lock()
            .map_err(|_| SessionError::LockPoisoned(path.display().to_string()))?;
        registry.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
        Ok(lock)
    }

    fn directory(&self) -> Result<&crate::daemons::state::StableDirectory, SessionError> {
        if let Some(error) = &self.initialization_error {
            return Err(SessionError::InvalidMutation(error.clone()));
        }
        self.directory.as_deref().ok_or_else(|| {
            SessionError::InvalidMutation("session directory is not initialized".to_string())
        })
    }

    fn verify_directory_binding(&self) -> Result<(), SessionError> {
        let directory = self.directory()?;
        let parent = self.parent_directory.as_deref().ok_or_else(|| {
            SessionError::InvalidMutation("session parent directory is not initialized".to_string())
        })?;
        let identity_file = self.directory_identity_file.as_deref().ok_or_else(|| {
            SessionError::InvalidMutation(
                "session directory identity is not initialized".to_string(),
            )
        })?;
        let visible_marker = self.sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
        let anchor = session_directory_identity_anchor(&visible_marker)
            .map_err(SessionError::InvalidMutation)?;

        directory
            .verify_visible()
            .and_then(|_| parent.verify_visible())
            .and_then(|_| directory.verify_file_identity(&visible_marker, identity_file))
            .and_then(|_| parent.verify_file_identity(&anchor, identity_file))
            .map_err(SessionError::InvalidMutation)
    }

    fn with_anchored_lock<T>(
        &self,
        lock_path: PathBuf,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_deadline(lock_path, self.lock_deadline(), operation)
    }

    fn lock_deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        self.lock_timeout
            .or_else(|| SESSION_LOCK_POLICY.try_with(|policy| policy.timeout).ok())
            .map(|timeout| now.checked_add(timeout).unwrap_or(now))
    }

    fn with_anchored_lock_until<T>(
        &self,
        lock_path: PathBuf,
        deadline: Instant,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_deadline(lock_path, Some(deadline), operation)
    }

    fn with_anchored_lock_deadline<T>(
        &self,
        lock_path: PathBuf,
        deadline: Option<Instant>,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let should_offload = SESSION_LOCK_POLICY
            .try_with(|policy| policy.offload_waits)
            .unwrap_or(false)
            && SESSION_LOCK_OFFLOAD_DEPTH.with(|depth| depth.get() == 0)
            && tokio::runtime::Handle::try_current().is_ok_and(|handle| {
                matches!(
                    handle.runtime_flavor(),
                    tokio::runtime::RuntimeFlavor::MultiThread
                )
            });
        if should_offload {
            return tokio::task::block_in_place(|| {
                let _guard = SessionLockOffloadGuard::enter();
                self.with_anchored_lock_deadline_inner(lock_path, deadline, operation)
            });
        }
        self.with_anchored_lock_deadline_inner(lock_path, deadline, operation)
    }

    fn with_anchored_lock_deadline_inner<T>(
        &self,
        lock_path: PathBuf,
        deadline: Option<Instant>,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.verify_directory_binding()?;
        let process_lock = self.process_lock(&lock_path)?;
        let _guard = lock_session_mutex(&process_lock, &lock_path, deadline)?;
        self.verify_directory_binding()?;

        let directory = self.directory()?;
        let mut outcome = None;
        let lock_operation = |_current_directory: &crate::daemons::state::StableDirectory| {
            self.verify_directory_binding()
                .map_err(|error| error.to_string())?;
            match operation(directory) {
                Ok(value) => {
                    self.verify_directory_binding()
                        .map_err(|error| error.to_string())?;
                    outcome = Some(Ok(value));
                    Ok(())
                }
                Err(error) => {
                    outcome = Some(Err(error));
                    Err(OPERATION_ERROR_SENTINEL.to_string())
                }
            }
        };
        let lock_result = match deadline {
            Some(deadline) => crate::daemons::state::with_file_lock_in_until(
                &lock_path,
                &self.sessions_dir,
                deadline,
                lock_operation,
            ),
            None => crate::daemons::state::with_file_lock_in(
                &lock_path,
                &self.sessions_dir,
                lock_operation,
            ),
        };

        match lock_result {
            Ok(()) => outcome.unwrap_or_else(|| {
                Err(SessionError::InvalidMutation(
                    "stable session lock returned without an operation result".to_string(),
                ))
            }),
            Err(error) if error == OPERATION_ERROR_SENTINEL => {
                outcome.unwrap_or_else(|| Err(SessionError::InvalidMutation(error)))
            }
            Err(error) => Err(SessionError::InvalidMutation(error)),
        }
    }

    fn with_session_lock<T>(
        &self,
        id: &str,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock(self.lock_path(id)?, operation)
    }

    fn with_session_lock_until<T>(
        &self,
        id: &str,
        deadline: Instant,
        operation: impl FnOnce(&crate::daemons::state::StableDirectory) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_until(self.lock_path(id)?, deadline, operation)
    }

    /// Serializes writes and destructive maintenance that depend on skill usage.
    /// Session JSON remains authoritative; the curator holds this lock while it
    /// rebuilds its cross-session aggregate and decides whether a skill is stale.
    pub(crate) fn with_skill_usage_lock<T>(
        &self,
        operation: impl FnOnce() -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock(self.sessions_dir.join(SKILL_USAGE_LOCK_FILE), |_| {
            operation()
        })
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn with_skill_usage_lock_for_testing<T>(
        &self,
        operation: impl FnOnce() -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_skill_usage_lock(operation)
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn with_session_lock_for_testing<T>(
        &self,
        id: &str,
        operation: impl FnOnce() -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_session_lock(id, |_| operation())
    }

    fn with_skill_usage_lock_until<T>(
        &self,
        deadline: Instant,
        operation: impl FnOnce() -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_anchored_lock_until(
            self.sessions_dir.join(SKILL_USAGE_LOCK_FILE),
            deadline,
            |_| operation(),
        )
    }

    fn load_opened_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        id: &str,
    ) -> Result<Option<OpenedSession>, SessionError> {
        self.load_opened_unlocked_with_hook(directory, id, || Ok(()))
    }

    fn load_opened_unlocked_with_hook(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        id: &str,
        after_read: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<Option<OpenedSession>, SessionError> {
        let path = self.path(id);
        if !directory
            .path_exists(&path)
            .map_err(SessionError::InvalidMutation)?
        {
            return Ok(None);
        }
        let file = directory
            .open_read(&path)
            .map_err(SessionError::InvalidMutation)?;
        let metadata = file.metadata()?;
        if metadata.len() > MAX_SESSION_JSON_BYTES {
            return Err(SessionError::FileTooLarge {
                path: path.display().to_string(),
                size: metadata.len(),
                max: MAX_SESSION_JSON_BYTES,
            });
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        let mut reader = (&file).take(MAX_SESSION_JSON_BYTES + 1);
        reader.read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SESSION_JSON_BYTES {
            return Err(SessionError::FileTooLarge {
                path: path.display().to_string(),
                size: bytes.len() as u64,
                max: MAX_SESSION_JSON_BYTES,
            });
        }
        after_read()?;
        directory
            .verify_file_identity(&path, &file)
            .map_err(SessionError::InvalidMutation)?;
        let session: Session = serde_json::from_slice(&bytes)?;
        if session.id != id {
            return Err(SessionError::SessionIdMismatch {
                expected: id.to_string(),
                actual: session.id,
            });
        }
        session.validate()?;
        Ok(Some(OpenedSession {
            session,
            file,
            metadata,
        }))
    }

    /// Strictly loads a session. Missing files are distinct from unreadable, corrupt,
    /// or invariant-violating files so callers cannot accidentally recreate over them.
    pub fn load_result(&self, id: &str) -> Result<Option<Session>, SessionError> {
        self.with_session_lock(id, |directory| {
            Ok(self
                .load_opened_unlocked(directory, id)?
                .map(|opened| opened.session))
        })
    }

    pub(crate) fn load_result_with_deadline(
        &self,
        id: &str,
        deadline: Instant,
    ) -> Result<Option<Session>, SessionError> {
        self.with_session_lock_until(id, deadline, |directory| {
            Ok(self
                .load_opened_unlocked(directory, id)?
                .map(|opened| opened.session))
        })
    }

    pub(crate) fn load_result_with_metadata(
        &self,
        id: &str,
    ) -> Result<Option<(Session, fs::Metadata)>, SessionError> {
        self.with_session_lock(id, |directory| {
            Ok(self
                .load_opened_unlocked(directory, id)?
                .map(|opened| (opened.session, opened.metadata)))
        })
    }

    /// Compatibility wrapper for read-only callers. Corruption fails loudly instead of
    /// being reported as a missing session; new code should prefer [`Self::load_result`].
    pub fn load(&self, id: &str) -> Option<Session> {
        self.load_result(id)
            .unwrap_or_else(|error| panic!("failed to load session {id}: {error}"))
    }

    pub fn try_create_session(&self) -> Result<Session, SessionError> {
        self.try_create_session_with_id(Uuid::new_v4().to_string())
    }

    pub fn create_session(&self) -> Session {
        self.try_create_session()
            .unwrap_or_else(|error| panic!("failed to create session: {error}"))
    }

    pub fn try_create_session_with_id(
        &self,
        id: impl Into<String>,
    ) -> Result<Session, SessionError> {
        let id = id.into();
        self.with_session_lock(&id, |directory| {
            if let Some(opened) = self.load_opened_unlocked(directory, &id)? {
                return Ok(opened.session);
            }
            let session = Session::new(id.clone());
            self.save_unlocked(directory, &session, None)?;
            Ok(session)
        })
    }

    pub fn create_session_with_id(&self, id: impl Into<String>) -> Session {
        let id = id.into();
        self.try_create_session_with_id(id.clone())
            .unwrap_or_else(|error| panic!("failed to create session {id}: {error}"))
    }

    fn save_unlocked(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        session: &Session,
        expected: Option<&File>,
    ) -> Result<(), SessionError> {
        self.save_unlocked_with_commit_check(directory, session, expected, || Ok(()))
    }

    fn save_unlocked_with_commit_check(
        &self,
        directory: &crate::daemons::state::StableDirectory,
        session: &Session,
        expected: Option<&File>,
        before_commit: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<(), SessionError> {
        session.validate()?;
        let path = self.path(&session.id);
        let data = serde_json::to_vec_pretty(session)?;
        if data.len() as u64 > MAX_SESSION_JSON_BYTES {
            return Err(SessionError::FileTooLarge {
                path: path.display().to_string(),
                size: data.len() as u64,
                max: MAX_SESSION_JSON_BYTES,
            });
        }
        let expected = expected.map_or(
            crate::daemons::state::FileExpectation::Missing,
            crate::daemons::state::FileExpectation::Present,
        );
        directory
            .save_bytes_atomically_expected_with_hook(
                &path,
                &data,
                ".nib-session-",
                true,
                expected,
                || {
                    self.verify_directory_binding()
                        .map_err(|error| error.to_string())?;
                    before_commit().map_err(|error| error.to_string())?;
                    self.verify_directory_binding()
                        .map_err(|error| error.to_string())
                },
            )
            .map_err(SessionError::InvalidMutation)?;
        self.verify_directory_binding()?;
        let published = self
            .load_opened_unlocked(directory, &session.id)?
            .ok_or_else(|| SessionError::NotFound(session.id.clone()))?;
        if published.session != *session {
            return Err(SessionError::InvalidMutation(format!(
                "published session did not retain the requested {}: {}",
                session_mismatch_field(session, &published.session),
                path.display()
            )));
        }
        Ok(())
    }

    pub fn save(&self, session: &mut Session) -> Result<(), SessionError> {
        match self.lock_deadline() {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, || {
                self.save_under_skill_usage_lock_with_deadline(session, Some(deadline))
            }),
            None => self.with_skill_usage_lock(|| self.save_under_skill_usage_lock(session)),
        }
    }

    fn save_under_skill_usage_lock(&self, session: &mut Session) -> Result<(), SessionError> {
        self.save_under_skill_usage_lock_with_deadline(session, None)
    }

    fn save_under_skill_usage_lock_with_deadline(
        &self,
        session: &mut Session,
        deadline: Option<Instant>,
    ) -> Result<(), SessionError> {
        let committed_revision = session.revision.checked_add(1).ok_or_else(|| {
            SessionError::InvalidMutation("session revision overflowed".to_string())
        })?;
        let operation = |directory: &crate::daemons::state::StableDirectory| {
            // Refuse to replace an existing session that cannot itself be loaded and
            // validated. Recovery must be an explicit operation, never an incidental save.
            let opened = self.load_opened_unlocked(directory, &session.id)?;
            let mut next = session.clone();
            if let Some(opened) = opened.as_ref() {
                if session.revision != opened.session.revision {
                    return Err(SessionError::InvalidMutation(format!(
                        "stale session revision for {}: snapshot={}, current={}",
                        session.id, session.revision, opened.session.revision
                    )));
                }
            } else if session.revision != 0 {
                return Err(SessionError::InvalidMutation(format!(
                    "session {} is missing but snapshot revision is {}",
                    session.id, session.revision
                )));
            }
            next.revision = committed_revision;
            self.save_unlocked(directory, &next, opened.as_ref().map(|opened| &opened.file))
        };
        match deadline {
            Some(deadline) => self.with_session_lock_until(&session.id, deadline, operation),
            None => self.with_session_lock(&session.id, operation),
        }?;
        session.revision = committed_revision;
        Ok(())
    }

    pub fn update_session<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        match self.lock_deadline() {
            Some(deadline) => self.update_session_with_deadline(id, deadline, update),
            None => self
                .with_skill_usage_lock(|| self.update_session_under_skill_usage_lock(id, update)),
        }
    }

    pub(crate) fn update_session_with_deadline<T>(
        &self,
        id: &str,
        deadline: Instant,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_skill_usage_lock_until(deadline, || {
            self.update_session_under_skill_usage_lock_with_deadline(id, Some(deadline), update)
        })
    }

    fn update_session_under_skill_usage_lock<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.update_session_under_skill_usage_lock_with_deadline(id, None, update)
    }

    fn update_session_under_skill_usage_lock_with_deadline<T>(
        &self,
        id: &str,
        deadline: Option<Instant>,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let operation = |directory: &crate::daemons::state::StableDirectory| {
            let mut opened = self
                .load_opened_unlocked(directory, id)?
                .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
            let revision = opened.session.revision;
            let result = update(&mut opened.session)?;
            opened.session.revision = revision.checked_add(1).ok_or_else(|| {
                SessionError::InvalidMutation("session revision overflowed".to_string())
            })?;
            self.save_unlocked(directory, &opened.session, Some(&opened.file))?;
            Ok(result)
        };
        match deadline {
            Some(deadline) => self.with_session_lock_until(id, deadline, operation),
            None => self.with_session_lock(id, operation),
        }
    }

    pub fn update_or_create_session<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        match self.lock_deadline() {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, || {
                self.update_or_create_session_under_skill_usage_lock_with_deadline(
                    id,
                    Some(deadline),
                    update,
                )
            }),
            None => self.with_skill_usage_lock(|| {
                self.update_or_create_session_under_skill_usage_lock(id, update)
            }),
        }
    }

    pub(crate) fn update_or_create_session_with_deadline<T>(
        &self,
        id: &str,
        deadline: Instant,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.with_skill_usage_lock_until(deadline, || {
            self.update_or_create_session_under_skill_usage_lock_with_deadline(
                id,
                Some(deadline),
                update,
            )
        })
    }

    fn update_or_create_session_under_skill_usage_lock<T>(
        &self,
        id: &str,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        self.update_or_create_session_under_skill_usage_lock_with_deadline(id, None, update)
    }

    fn update_or_create_session_under_skill_usage_lock_with_deadline<T>(
        &self,
        id: &str,
        deadline: Option<Instant>,
        update: impl FnOnce(&mut Session) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let operation = |directory: &crate::daemons::state::StableDirectory| {
            let opened = self.load_opened_unlocked(directory, id)?;
            let mut session = opened
                .as_ref()
                .map(|opened| opened.session.clone())
                .unwrap_or_else(|| Session::new(id.to_string()));
            let revision = session.revision;
            let result = update(&mut session)?;
            session.revision = revision.checked_add(1).ok_or_else(|| {
                SessionError::InvalidMutation("session revision overflowed".to_string())
            })?;
            self.save_unlocked(
                directory,
                &session,
                opened.as_ref().map(|opened| &opened.file),
            )?;
            Ok(result)
        };
        match deadline {
            Some(deadline) => self.with_session_lock_until(id, deadline, operation),
            None => self.with_session_lock(id, operation),
        }
    }

    /// Atomically reloads and conditionally deletes a session while holding the
    /// same process and file locks used by every session mutation.
    pub fn delete_if(
        &self,
        id: &str,
        should_delete: impl FnOnce(&Session, &fs::Metadata) -> bool,
    ) -> Result<SessionDeleteOutcome, SessionError> {
        self.delete_if_with_commit_check(id, should_delete, || Ok(()))
    }

    pub(crate) fn delete_if_with_commit_check(
        &self,
        id: &str,
        should_delete: impl FnOnce(&Session, &fs::Metadata) -> bool,
        before_delete: impl FnOnce() -> Result<(), SessionError>,
    ) -> Result<SessionDeleteOutcome, SessionError> {
        let deadline = self.lock_deadline();
        let operation = || {
            let session_operation = |directory: &crate::daemons::state::StableDirectory| {
                let path = self.path(id);
                directory
                    .recover_quarantined_file(&path, ".nib-session-delete-")
                    .map_err(SessionError::InvalidMutation)?;
                let Some(opened) = self.load_opened_unlocked(directory, id)? else {
                    return Ok(SessionDeleteOutcome::Missing);
                };
                if !should_delete(&opened.session, &opened.metadata) {
                    return Ok(SessionDeleteOutcome::Retained);
                }
                directory
                    .remove_file_if_matches_with_hook(
                        &path,
                        &opened.file,
                        ".nib-session-delete-",
                        || before_delete().map_err(|error| error.to_string()),
                    )
                    .map_err(SessionError::InvalidMutation)?;
                self.verify_directory_binding()?;
                Ok(SessionDeleteOutcome::Deleted)
            };
            match deadline {
                Some(deadline) => self.with_session_lock_until(id, deadline, session_operation),
                None => self.with_session_lock(id, session_operation),
            }
        };
        match deadline {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, operation),
            None => self.with_skill_usage_lock(operation),
        }
    }

    pub fn append_message(&self, id: &str, role: &str, content: &str) -> Session {
        match self.try_append_message(id, role, content) {
            Ok(session) => session,
            Err(error) => {
                if !matches!(
                    &error,
                    SessionError::InvalidRole(_) | SessionError::RoleViolation { .. }
                ) {
                    panic!("failed to append to session {id}: {error}");
                }
                let error_message = error.to_string();
                self.record_event(
                    id,
                    "role_violation",
                    serde_json::json!({
                        "role": role,
                        "content": content,
                        "error": error_message,
                    }),
                )
                .unwrap_or_else(|audit_error| {
                    panic!("failed to audit role violation for session {id}: {audit_error}")
                });
                self.load_result(id)
                    .unwrap_or_else(|load_error| {
                        panic!("failed to reload session {id}: {load_error}")
                    })
                    .unwrap_or_else(|| panic!("session {id} disappeared after role violation"))
            }
        }
    }

    pub fn try_append_message(
        &self,
        id: &str,
        role: &str,
        content: &str,
    ) -> Result<Session, SessionError> {
        self.update_or_create_session(id, |session| {
            validate_role_transition(session.messages.last().map(|m| m.role.as_str()), role)?;
            let index = session.messages.len();
            session.messages.push(SessionMessage {
                index,
                role: role.to_string(),
                content: content.to_string(),
                timestamp: Some(Utc::now()),
            });
            Ok(session.clone())
        })
    }

    pub fn record_event(
        &self,
        id: &str,
        kind: impl Into<String>,
        details: serde_json::Value,
    ) -> Result<SessionEvent, SessionError> {
        let kind = kind.into();
        self.update_or_create_session(id, |session| {
            let event = SessionEvent {
                index: session.events.len(),
                kind,
                details,
                timestamp: Some(Utc::now()),
            };
            session.events.push(event.clone());
            Ok(event)
        })
    }

    pub fn record_skill_usage(
        &self,
        id: &str,
        skill_name: impl Into<String>,
        reason: Option<String>,
    ) -> Result<SkillUsageRecord, SessionError> {
        let skill_name = skill_name.into();
        crate::context::skills::canonical_skill_id(&skill_name).map_err(|error| {
            SessionError::InvalidMutation(format!("invalid skill name: {error}"))
        })?;
        let deadline = self.lock_deadline();
        let operation = || {
            self.update_or_create_session_under_skill_usage_lock_with_deadline(
                id,
                deadline,
                |session| {
                    if !session.active_skills.iter().any(|name| name == &skill_name) {
                        session.active_skills.push(skill_name.clone());
                    }
                    let usage = SkillUsageRecord {
                        skill_name,
                        reason,
                        timestamp: Some(Utc::now()),
                    };
                    session.skill_usage.push(usage.clone());
                    Ok(usage)
                },
            )
        };
        match deadline {
            Some(deadline) => self.with_skill_usage_lock_until(deadline, operation),
            None => self.with_skill_usage_lock(operation),
        }
    }

    fn list_entries_result(
        &self,
        max_sessions: usize,
        max_total_bytes: Option<u64>,
        validate_contents: bool,
    ) -> Result<Vec<String>, SessionError> {
        self.verify_directory_binding()?;
        let directory = self.directory()?;
        directory
            .recover_stale_temporary_files_strict(
                ".nib-session-",
                MAX_SESSION_DIRECTORY_ENTRIES.saturating_mul(4),
                MAX_SESSION_DIRECTORY_NAME_BYTES.saturating_mul(4),
            )
            .map_err(SessionError::InvalidMutation)?;
        let mut ids = Vec::new();
        let mut total_bytes = 0_u64;
        directory
            .for_each_entry_bounded(
                MAX_SESSION_DIRECTORY_ENTRIES,
                MAX_SESSION_DIRECTORY_NAME_BYTES,
                |name| {
                    if crate::daemons::state::StableDirectory::is_atomic_transaction_artifact_name(
                        &name,
                        ".nib-session-",
                    ) {
                        return Ok(());
                    }
                    let name_path = PathBuf::from(&name);
                    if name_path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        != Some("json")
                    {
                        return Ok(());
                    }
                    let id = name_path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .ok_or_else(|| {
                            format!(
                                "session path is not valid UTF-8: {}",
                                self.sessions_dir.join(&name).display()
                            )
                        })?
                        .to_string();
                    validate_session_id(&id).map_err(|error| error.to_string())?;
                    if ids.len() >= max_sessions {
                        return Err(format!(
                            "session directory {} exceeds the {max_sessions}-session limit",
                            self.sessions_dir.display()
                        ));
                    }
                    let path = self.sessions_dir.join(&name);
                    let file = directory.open_read(&path)?;
                    let metadata = file.metadata().map_err(|error| error.to_string())?;
                    if !metadata.is_file() {
                        return Err(format!(
                            "session path is not a regular file: {}",
                            path.display()
                        ));
                    }
                    if metadata.len() > MAX_SESSION_JSON_BYTES {
                        return Err(format!(
                            "session file {} exceeds the {MAX_SESSION_JSON_BYTES}-byte limit",
                            path.display()
                        ));
                    }
                    directory.verify_file_identity(&path, &file)?;
                    total_bytes = total_bytes.checked_add(metadata.len()).ok_or_else(|| {
                        "profile session byte count overflowed during enumeration".to_string()
                    })?;
                    if max_total_bytes.is_some_and(|maximum| total_bytes > maximum) {
                        return Err(format!(
                            "profile sessions exceed the {}-byte skill usage aggregation limit",
                            max_total_bytes.expect("checked maximum")
                        ));
                    }
                    ids.push(id);
                    Ok(())
                },
            )
            .map_err(SessionError::InvalidMutation)?;
        ids.sort();
        if validate_contents {
            for id in &ids {
                let path = self.path(id);
                self.with_session_lock(id, |locked_directory| {
                    self.load_opened_unlocked(locked_directory, id)?
                        .map(|_| ())
                        .ok_or_else(|| {
                            SessionError::InvalidMutation(format!(
                                "session disappeared during enumeration: {}",
                                path.display()
                            ))
                        })
                })?;
            }
        }
        self.verify_directory_binding()?;
        Ok(ids)
    }

    pub fn list_result(&self) -> Result<Vec<String>, SessionError> {
        self.list_entries_result(MAX_LISTED_SESSIONS, None, true)
    }

    pub(crate) fn list_for_skill_usage(
        &self,
        max_sessions: usize,
        max_total_bytes: u64,
    ) -> Result<Vec<String>, SessionError> {
        // This is a metadata preflight; the curator strictly loads every returned ID.
        self.list_entries_result(max_sessions, Some(max_total_bytes), false)
    }

    pub fn list(&self) -> Vec<String> {
        self.list_result()
            .unwrap_or_else(|error| panic!("failed to list sessions: {error}"))
    }

    pub fn record_tool_call(&self, record: ToolCallRecord) -> Result<(), SessionError> {
        let sid = record
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.update_or_create_session(&sid, |session| {
            session.tool_calls.push(record);
            Ok(())
        })
    }

    pub fn get_latest_id(&self) -> Option<String> {
        self.list().pop()
    }
}

fn open_session_directory(
    requested: &Path,
) -> Result<
    (
        PathBuf,
        crate::daemons::state::StableDirectory,
        crate::daemons::state::StableDirectory,
        File,
    ),
    String,
> {
    let sessions_dir = crate::fs_security::ensure_directory_without_symlinks(requested)
        .map_err(|error| error.to_string())?;
    let parent_path = sessions_dir.parent().ok_or_else(|| {
        format!(
            "session directory has no persistent identity parent: {}",
            sessions_dir.display()
        )
    })?;
    let parent = crate::daemons::state::StableDirectory::open(parent_path)?;
    let directory = parent.open_child(&sessions_dir)?;
    let identity_file = initialize_session_directory_identity(&directory, &parent, &sessions_dir)?;
    directory.recover_stale_temporary_files(
        ".nib-session-",
        MAX_SESSION_DIRECTORY_ENTRIES.saturating_mul(4),
        MAX_SESSION_DIRECTORY_NAME_BYTES.saturating_mul(4),
    )?;
    Ok((sessions_dir, directory, parent, identity_file))
}

fn initialize_session_directory_identity(
    directory: &crate::daemons::state::StableDirectory,
    parent: &crate::daemons::state::StableDirectory,
    sessions_dir: &Path,
) -> Result<File, String> {
    let visible = sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
    let anchor = session_directory_identity_anchor(&visible)?;
    let visible_exists = directory.path_exists(&visible)?;
    let anchor_exists = parent.path_exists(&anchor)?;

    match (visible_exists, anchor_exists) {
        (false, false) => {
            drop(directory.open_read_write_create(&visible)?);
            directory.hard_link_to(&visible, parent, &anchor)?;
            directory.sync_directory()?;
            parent.sync_directory()?;
        }
        (true, false) => {
            directory.hard_link_to(&visible, parent, &anchor)?;
            parent.sync_directory()?;
        }
        (false, true) => {
            return Err(format!(
                "session directory identity marker is missing while its persistent anchor remains: {}",
                visible.display()
            ));
        }
        (true, true) => {}
    }

    let identity_file = directory.open_read(&visible)?;
    parent.verify_file_identity(&anchor, &identity_file)?;
    directory.verify_visible()?;
    parent.verify_visible()?;
    Ok(identity_file)
}

fn session_directory_identity_anchor(visible_marker: &Path) -> Result<PathBuf, String> {
    crate::daemons::state::daemon_lock_anchor_path(visible_marker)
}

fn session_lock_stripe(id: &str) -> usize {
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    (hash as usize) % SESSION_LOCK_STRIPES
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_ROOT: &str = "NIB_SESSION_COMMIT_CHILD_ROOT";
    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_ID: &str = "NIB_SESSION_COMMIT_CHILD_ID";
    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_MODE: &str = "NIB_SESSION_COMMIT_CHILD_MODE";
    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_READY: &str = "NIB_SESSION_COMMIT_CHILD_READY";
    #[cfg(unix)]
    const SESSION_COMMIT_CHILD_RELEASE: &str = "NIB_SESSION_COMMIT_CHILD_RELEASE";

    fn plan_step(description: &str) -> PlanStep {
        PlanStep {
            description: description.to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        }
    }

    #[test]
    fn plan_identity_is_unique_and_goal_matching_is_exact_after_normalization() {
        let first = Plan::new(
            "  Implement\tplan identity\nand resume  ",
            vec![plan_step("implement")],
        );
        let second = Plan::new(
            "Implement plan identity and resume",
            vec![plan_step("implement")],
        );

        assert_ne!(first.id, second.id);
        assert_eq!(first.goal, "Implement plan identity and resume");
        assert!(first.matches_goal("Implement   plan identity and resume"));
        assert!(!first.matches_goal("implement plan identity and resume"));
        assert!(first.is_resumable_for("Implement plan identity and resume"));

        let mut malformed = Plan::new(
            "Implement plan identity and resume",
            vec![plan_step("implement")],
        );
        malformed.steps[0].description.clear();
        malformed.approve();
        assert!(!malformed.is_structured());
        assert!(!malformed.is_resumable_for("Implement plan identity and resume"));

        let mut stale_cursor = Plan::new(
            "Implement plan identity and resume",
            vec![plan_step("implement"), plan_step("verify")],
        );
        stale_cursor.approve();
        stale_cursor.steps[0].status = "Completed".to_string();
        assert!(!stale_cursor.is_structured());
        assert!(!stale_cursor.is_resumable_for("Implement plan identity and resume"));

        let mut advanced_future = Plan::new(
            "Implement plan identity and resume",
            vec![plan_step("implement"), plan_step("verify")],
        );
        advanced_future.approve();
        advanced_future.steps[1].status = "InProgress".to_string();
        assert!(!advanced_future.is_structured());
    }

    #[test]
    fn legacy_plan_metadata_defaults_empty_for_runtime_invalidation() {
        let legacy: Plan = serde_json::from_value(serde_json::json!({
            "steps": [{
                "description": "legacy step",
                "status": "Pending"
            }],
            "current_step_index": 0
        }))
        .expect("legacy plan");

        assert!(!legacy.has_identity());
        assert!(!legacy.is_structured());
        assert!(!legacy.is_resumable_for("legacy goal"));
    }

    #[cfg(windows)]
    fn create_directory_junction(junction: &Path, target: &Path) {
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(junction)
            .arg(target)
            .output()
            .expect("create directory junction");
        assert!(
            output.status.success(),
            "mklink failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn session_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session();
        store.append_message(&session.id, "user", "hello");

        let loaded = store.load(&session.id).expect("load session");
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].role, "user");
        assert_eq!(loaded.messages[0].content, "hello");
        assert!(loaded.messages[0].timestamp.is_some());

        let raw = fs::read_to_string(store.path(&session.id)).expect("read file");
        let reparsed: Session = serde_json::from_str(&raw).expect("parse");
        assert_eq!(reparsed, loaded);
    }

    #[test]
    fn session_audit_floats_roundtrip_without_losing_a_ulp() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session();
        let duration = 1.551_978_736_000_000_1_f64;

        store
            .record_tool_call(ToolCallRecord {
                session_id: Some(session.id.clone()),
                tool_name: Some("roundtrip_probe".to_string()),
                arguments: serde_json::json!({"nested_duration": duration}),
                duration_seconds: Some(duration),
                ..ToolCallRecord::default()
            })
            .expect("persist exact audit float");

        let loaded = store.load(&session.id).expect("load session");
        let call = loaded.tool_calls.last().expect("audit call");
        assert_eq!(
            call.duration_seconds.expect("duration").to_bits(),
            duration.to_bits()
        );
        assert_eq!(
            call.arguments["nested_duration"]
                .as_f64()
                .expect("nested duration")
                .to_bits(),
            duration.to_bits()
        );
    }

    #[test]
    fn loads_legacy_session_without_timestamps() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let legacy = r#"{
  "id": "legacy-session",
  "messages": [
    {"role": "user", "content": "hi"}
  ],
  "tool_calls": []
}"#;
        fs::write(store.path("legacy-session"), legacy).expect("write legacy");

        let loaded = store.load("legacy-session").expect("load legacy");
        assert_eq!(loaded.messages.len(), 1);
        assert!(loaded.messages[0].timestamp.is_none());
    }

    #[test]
    fn oversized_sparse_session_is_rejected_before_reading() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let path = store.path("oversized");
        fs::File::create(&path)
            .and_then(|file| file.set_len(MAX_SESSION_JSON_BYTES + 1))
            .expect("create sparse session");

        let error = store
            .load_result("oversized")
            .expect_err("oversized session must fail closed");

        assert!(matches!(error, SessionError::FileTooLarge { .. }));
        assert_eq!(
            fs::metadata(path).unwrap().len(),
            MAX_SESSION_JSON_BYTES + 1
        );
    }

    #[test]
    fn list_result_rejects_valid_named_corrupt_or_mismatched_sessions() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let path = store.path("corrupt-session");
        fs::write(&path, b"{not valid json").expect("write corrupt session");

        let corrupt_error = store
            .list_result()
            .expect_err("corrupt session must fail strict enumeration");
        assert!(
            corrupt_error.to_string().contains("parse session JSON"),
            "{corrupt_error}"
        );

        fs::write(&path, r#"{"id":"different-session"}"#).expect("write mismatched session");
        let mismatch_error = store
            .list_result()
            .expect_err("mismatched session must fail strict enumeration");
        assert!(
            mismatch_error
                .to_string()
                .contains("contains session different-session"),
            "{mismatch_error}"
        );
    }

    #[test]
    fn record_skill_usage_rejects_noncanonicalizable_name_without_persisting() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session();

        let error = store
            .record_skill_usage(&session.id, "!!!", Some("invalid".to_string()))
            .expect_err("invalid skill name must be rejected");

        assert!(matches!(error, SessionError::InvalidMutation(_)));
        let persisted = store.load(&session.id).expect("load session");
        assert!(persisted.active_skills.is_empty());
        assert!(persisted.skill_usage.is_empty());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_file_replacement_during_read_is_rejected() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-during-read");
        let path = store.path(&session.id);
        let displaced = store.sessions_dir().join("displaced-read-session.json");
        let mut replacement = Session::new(session.id.clone());
        replacement.summary = Some("replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

        let error = store
            .with_session_lock(&session.id, |directory| {
                store.load_opened_unlocked_with_hook(directory, &session.id, || {
                    fs::rename(&path, &displaced)?;
                    fs::write(&path, &replacement_bytes)?;
                    Ok(())
                })
            })
            .expect_err("replacement during read must fail");

        assert!(error.to_string().contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(&path).expect("replacement bytes"),
            replacement_bytes
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_file_replacement_during_update_is_not_overwritten() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-file");
        let path = store.path(&session.id);
        let displaced = store.sessions_dir().join("displaced-session.json");
        let mut replacement = Session::new(session.id.clone());
        replacement.summary = Some("replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

        let error = store
            .update_session(&session.id, |current| {
                fs::rename(&path, &displaced)?;
                fs::write(&path, &replacement_bytes)?;
                current.summary = Some("must-not-publish".to_string());
                Ok(())
            })
            .expect_err("replaced session identity must fail");

        assert!(error.to_string().contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(&path).expect("replacement bytes"),
            replacement_bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_child_session_commit_barrier_and_fsync_crash_recovery() {
        if let Some(root) = std::env::var_os(SESSION_COMMIT_CHILD_ROOT) {
            run_session_commit_child(Path::new(&root));
            return;
        }

        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let replacement_session = store.create_session_with_id("child-session-commit-substitution");
        let replacement_path = store.path(&replacement_session.id);
        let displaced_path = store.sessions_dir().join("child-session-displaced");
        let ready = root.path().join("session-replacement.ready");
        let release = root.path().join("session-replacement.release");
        let mut child = spawn_session_commit_child(
            root.path(),
            &replacement_session.id,
            "replace",
            &ready,
            Some(&release),
        );
        wait_for_session_commit_child(&mut child, &ready);

        let mut replacement = replacement_session.clone();
        replacement.revision = 1;
        replacement.summary = Some("authoritative replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");
        fs::rename(&replacement_path, &displaced_path).expect("displace expected session");
        fs::write(&replacement_path, &replacement_bytes).expect("install replacement session");
        fs::write(&release, b"release").expect("release replacement child");
        let status = child.wait().expect("wait for replacement child");
        assert!(status.success(), "replacement child failed: {status}");
        assert_eq!(
            fs::read(&replacement_path).expect("replacement session bytes"),
            replacement_bytes
        );
        assert_eq!(
            fs::read(&displaced_path).expect("displaced session bytes"),
            serde_json::to_vec_pretty(&replacement_session).expect("serialize original")
        );

        let crash_session = store.create_session_with_id("child-session-fsync-crash");
        let crash_path = store.path(&crash_session.id);
        let crash_before = fs::read(&crash_path).expect("session before crash");
        let crash_ready = root.path().join("session-crash.ready");
        let mut crash_child =
            spawn_session_commit_child(root.path(), &crash_session.id, "kill", &crash_ready, None);
        wait_for_session_commit_child(&mut crash_child, &crash_ready);
        let temporary = session_temporary_paths(store.sessions_dir());
        assert_eq!(temporary.len(), 1, "expected one fsynced session temp");
        crash_child.kill().expect("kill session writer");
        crash_child.wait().expect("reap session writer");
        assert!(
            temporary[0].exists(),
            "killed writer temp disappeared early"
        );

        drop(store);
        let recovered = SessionStore::new(root.path());
        assert_eq!(
            fs::read(&crash_path).expect("session after recovery"),
            crash_before
        );
        assert!(
            session_temporary_paths(recovered.sessions_dir()).is_empty(),
            "fresh session store left the killed writer temp"
        );
        assert_eq!(
            recovered
                .load_result(&crash_session.id)
                .expect("load recovered session")
                .expect("recovered session"),
            crash_session
        );
    }

    #[test]
    fn stale_session_snapshot_cannot_overwrite_a_newer_revision() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("session-revision-cas");
        let mut first = store.load(&session.id).expect("first snapshot");
        let mut stale = first.clone();
        first.summary = Some("first writer".to_string());
        stale.summary = Some("stale writer".to_string());

        store.save(&mut first).expect("first revision commit");
        let error = store
            .save(&mut stale)
            .expect_err("stale snapshot revision must be rejected");

        assert!(
            error.to_string().contains("stale session revision"),
            "{error}"
        );
        assert_eq!(
            store
                .load(&session.id)
                .expect("authoritative session")
                .summary
                .as_deref(),
            Some("first writer")
        );
    }

    #[test]
    fn consecutive_session_saves_refresh_the_snapshot_revision() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let mut session = store.create_session_with_id("session-consecutive-save");
        assert_eq!(session.revision, 0);

        session.summary = Some("first".to_string());
        store.save(&mut session).expect("first save");
        assert_eq!(session.revision, 1);
        session.summary = Some("second".to_string());
        store.save(&mut session).expect("second save");
        assert_eq!(session.revision, 2);

        let persisted = store.load(&session.id).expect("persisted session");
        assert_eq!(persisted.revision, 2);
        assert_eq!(persisted.summary.as_deref(), Some("second"));
    }

    #[test]
    fn legacy_session_without_revision_defaults_to_zero() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("legacy-session-revision");
        let path = store.path(&session.id);
        let bytes = fs::read(&path).expect("legacy-compatible session bytes");
        assert!(!String::from_utf8_lossy(&bytes).contains("\"revision\""));
        assert_eq!(store.load(&session.id).expect("legacy session").revision, 0);
    }

    #[test]
    fn session_revision_overflow_preserves_disk_and_snapshot() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let mut session = store.create_session_with_id("session-revision-overflow");
        session.revision = u64::MAX;
        let path = store.path(&session.id);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&session).expect("overflow session JSON"),
        )
        .expect("write overflow session");
        let before = fs::read(&path).expect("session before failed save");

        let error = store
            .save(&mut session)
            .expect_err("revision overflow must fail closed");

        assert!(error.to_string().contains("revision overflowed"), "{error}");
        assert_eq!(session.revision, u64::MAX);
        assert_eq!(fs::read(path).expect("session after failed save"), before);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_delete_rejects_replacement_at_quarantine_boundary() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-delete");
        let path = store.path(&session.id);
        let displaced = store.sessions_dir().join("displaced-delete-session.json");
        let mut replacement = Session::new(session.id.clone());
        replacement.summary = Some("newer replacement".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

        let error = store
            .delete_if_with_commit_check(
                &session.id,
                |_, _| true,
                || {
                    fs::rename(&path, &displaced)?;
                    fs::write(&path, &replacement_bytes)?;
                    Ok(())
                },
            )
            .expect_err("replaced session must not be deleted");

        assert!(error.to_string().contains("identity changed"), "{error}");
        assert_eq!(
            fs::read(path).expect("replacement session"),
            replacement_bytes
        );
        assert!(displaced.exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_lock_replacement_while_held_is_rejected() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-lock");
        let lock_path = store.lock_path(&session.id).expect("lock path");
        let displaced = store.sessions_dir().join("displaced-session.lock");

        let error = store
            .update_session(&session.id, |current| {
                fs::rename(&lock_path, &displaced)?;
                fs::write(&lock_path, b"replacement-lock")?;
                current.summary = Some("operation-must-report-failure".to_string());
                Ok(())
            })
            .expect_err("replaced lock identity must fail");

        assert!(
            error.to_string().contains("different identities"),
            "{error}"
        );
        assert_eq!(
            fs::read(&lock_path).expect("replacement lock"),
            b"replacement-lock"
        );
        assert!(SessionStore::new(root.path())
            .load_result(&session.id)
            .is_err());
    }

    #[test]
    fn deadline_update_times_out_behind_process_lock_without_mutating() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("deadline-session-lock");
        let holder_store = store.clone();
        let holder_id = session.id.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_store
                .update_session(&holder_id, |_current| {
                    held_tx.send(()).expect("signal held session lock");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release held session lock");
                    Ok(())
                })
                .expect("holder session update");
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("session lock is held");

        let error = store
            .update_session_with_deadline(
                &session.id,
                Instant::now() + Duration::from_millis(100),
                |current| {
                    current.summary = Some("must not persist".to_string());
                    Ok(())
                },
            )
            .expect_err("deadline must bound the process lock wait");
        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );

        release_tx.send(()).expect("release session lock");
        holder.join().expect("session lock holder");
        assert_eq!(
            store.load(&session.id).expect("session remains").summary,
            None
        );
    }

    #[test]
    fn configured_lock_timeout_bounds_default_session_updates() {
        let root = tempdir().expect("project");
        let unbounded_store = SessionStore::new(root.path());
        let session = unbounded_store.create_session_with_id("configured-deadline-session");
        let holder_store = unbounded_store.clone();
        let holder_id = session.id.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_store
                .update_session(&holder_id, |_current| {
                    held_tx.send(()).expect("signal held session lock");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release held session lock");
                    Ok(())
                })
                .expect("holder session update");
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("session lock is held");

        let bounded_store = unbounded_store
            .clone()
            .with_lock_timeout(Duration::from_millis(100));
        let started = Instant::now();
        let error = bounded_store
            .update_session(&session.id, |current| {
                current.summary = Some("must not persist".to_string());
                Ok(())
            })
            .expect_err("configured deadline must bound the default update API");
        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        release_tx.send(()).expect("release session lock");
        holder.join().expect("session lock holder");
        assert_eq!(
            unbounded_store
                .load(&session.id)
                .expect("session remains")
                .summary,
            None
        );
    }

    #[test]
    fn configured_lock_timeout_is_shared_across_nested_locks() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("shared-deadline-session");

        let session_holder_store = store.clone();
        let session_holder_id = session.id.clone();
        let (session_held_tx, session_held_rx) = std::sync::mpsc::channel();
        let (release_session_tx, release_session_rx) = std::sync::mpsc::channel();
        let session_holder = std::thread::spawn(move || {
            session_holder_store
                .with_session_lock(&session_holder_id, |_| {
                    session_held_tx.send(()).expect("signal session lock held");
                    release_session_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release session lock");
                    Ok(())
                })
                .expect("hold session lock");
        });
        session_held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("session lock is held");

        let skill_holder_store = store.clone();
        let (skill_held_tx, skill_held_rx) = std::sync::mpsc::channel();
        let (release_skill_tx, release_skill_rx) = std::sync::mpsc::channel();
        let skill_holder = std::thread::spawn(move || {
            skill_holder_store
                .with_skill_usage_lock(|| {
                    skill_held_tx.send(()).expect("signal skill lock held");
                    release_skill_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release skill lock");
                    Ok(())
                })
                .expect("hold skill usage lock");
        });
        skill_held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("skill usage lock is held");

        let delayed_release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(180));
            release_skill_tx.send(()).expect("release skill usage lock");
        });
        let bounded_store = store.clone().with_lock_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let error = bounded_store
            .update_session(&session.id, |_current| Ok(()))
            .expect_err("one deadline must cover both nested lock waits");
        let elapsed = started.elapsed();
        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );
        assert!(
            elapsed < Duration::from_millis(420),
            "nested locks received separate timeout budgets: {elapsed:?}"
        );

        delayed_release.join().expect("delayed skill lock release");
        skill_holder.join().expect("skill lock holder");
        release_session_tx.send(()).expect("release session lock");
        session_holder.join().expect("session lock holder");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn scoped_lock_policy_propagates_to_spawned_store_without_starving_runtime() {
        let root = tempdir().expect("project");
        let holder_store = SessionStore::new(root.path());
        let session = holder_store.create_session_with_id("scoped-deadline-session");
        let holder_id = session.id.clone();
        let operation_store = SessionStore::new(root.path());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            holder_store
                .update_session(&holder_id, |_current| {
                    held_tx.send(()).expect("signal held session lock");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release held session lock");
                    Ok(())
                })
                .expect("holder session update");
        });
        held_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("session lock is held");

        let operation_id = session.id.clone();
        let operation = tokio::spawn(SessionStore::with_lock_policy(
            Duration::from_millis(200),
            async move {
                let policy = SessionStore::current_lock_policy();
                tokio::spawn(SessionStore::with_optional_lock_policy(
                    policy,
                    async move { operation_store.update_session(&operation_id, |_current| Ok(())) },
                ))
                .await
                .expect("spawned session operation")
            },
        ));
        tokio::time::timeout(Duration::from_millis(100), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        })
        .await
        .expect("the sole async worker remains responsive during a session lock wait");

        let error = operation
            .await
            .expect("scoped operation task")
            .expect_err("scoped lock policy must time out");
        assert!(
            error
                .to_string()
                .contains("timed out acquiring session lock"),
            "{error}"
        );

        release_tx.send(()).expect("release session lock");
        holder.join().expect("session lock holder");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn skill_usage_lock_replacement_while_held_is_rejected() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let lock_path = store.sessions_dir().join(SKILL_USAGE_LOCK_FILE);
        let displaced = store.sessions_dir().join("displaced-skill-usage.lock");

        let error = store
            .with_skill_usage_lock(|| {
                fs::rename(&lock_path, &displaced)?;
                fs::write(&lock_path, b"replacement-lock")?;
                Ok(())
            })
            .expect_err("replaced skill usage lock identity must fail");

        assert!(
            error.to_string().contains("different identities"),
            "{error}"
        );
        assert_eq!(
            fs::read(&lock_path).expect("replacement lock"),
            b"replacement-lock"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn whole_session_directory_replacement_is_rejected() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("replace-session-directory");
        let sessions_dir = store.sessions_dir().to_path_buf();
        let displaced = root.path().join("displaced-sessions");
        #[cfg(unix)]
        let (replacement_path, replacement_bytes) = {
            let replacement_path = sessions_dir.join(format!("{}.json", session.id));
            let mut replacement = Session::new(session.id.clone());
            replacement.summary = Some("replacement-directory".to_string());
            let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");
            (replacement_path, replacement_bytes)
        };

        let error = store
            .update_session(&session.id, |current| {
                #[cfg(unix)]
                {
                    fs::rename(&sessions_dir, &displaced)?;
                    fs::create_dir(&sessions_dir)?;
                    fs::write(&replacement_path, &replacement_bytes)?;
                    current.summary = Some("must-not-publish".to_string());
                    Ok(())
                }
                #[cfg(windows)]
                {
                    let _ = current;
                    let error = fs::rename(&sessions_dir, &displaced)
                        .expect_err("live Windows session lock pins the sessions directory");
                    Err::<(), SessionError>(SessionError::Io(error))
                }
            })
            .expect_err("detached directory must fail");

        #[cfg(unix)]
        {
            assert!(error.to_string().contains("directory"), "{error}");
            assert_eq!(
                fs::read(&replacement_path).expect("replacement session"),
                replacement_bytes
            );
            assert!(store.list_result().is_err());
            assert!(SessionStore::new(root.path())
                .load_result(&session.id)
                .is_err());
        }
        #[cfg(windows)]
        {
            assert!(!error.to_string().is_empty());
            assert!(sessions_dir.is_dir());
            assert!(!displaced.exists());
            assert_eq!(
                store
                    .load_result(&session.id)
                    .expect("original session remains readable")
                    .expect("original session remains present")
                    .summary,
                session.summary
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn session_lock_artifacts_are_bounded_by_fixed_stripes() {
        let root = tempdir().expect("project");
        let store = SessionStore::new(root.path());
        let ids = (0..256)
            .map(|index| format!("bounded-lock-{index}"))
            .collect::<Vec<_>>();
        for id in &ids {
            store
                .try_create_session_with_id(id)
                .expect("create striped session");
        }

        let lock_names = fs::read_dir(store.sessions_dir())
            .expect("list session locks")
            .map(|entry| entry.expect("directory entry").file_name())
            .filter(|name| {
                name.to_string_lossy().starts_with(".session-lock-")
                    && name.to_string_lossy().ends_with(".lock")
            })
            .collect::<Vec<_>>();
        assert!(!lock_names.is_empty());
        assert!(lock_names.len() <= SESSION_LOCK_STRIPES);
        for id in ids {
            assert!(!store.sessions_dir().join(format!(".{id}.lock")).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_store_rejects_symlinked_nib_without_outside_write() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        symlink(outside.path(), root.path().join(".nib")).expect("symlink .nib");
        let store = SessionStore::new(root.path());

        assert!(store.try_create_session_with_id("blocked").is_err());
        assert!(!outside.path().join("sessions").exists());
    }

    #[cfg(windows)]
    #[test]
    fn direct_store_rejects_junctioned_nib_without_outside_write() {
        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        create_directory_junction(&root.path().join(".nib"), outside.path());
        let store = SessionStore::new(root.path());

        assert!(store.try_create_session_with_id("blocked").is_err());
        assert!(!outside.path().join("sessions").exists());
    }

    #[cfg(unix)]
    #[test]
    fn mutation_rechecks_directory_after_constructor_race() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        let store = SessionStore::new(root.path());
        fs::rename(root.path().join(".nib"), root.path().join(".nib-displaced"))
            .expect("displace .nib");
        symlink(outside.path(), root.path().join(".nib")).expect("swap state ancestor");

        assert!(store.try_create_session_with_id("raced").is_err());
        assert!(!outside.path().join("sessions").exists());
    }

    #[cfg(unix)]
    fn run_session_commit_child(root: &Path) {
        let id = std::env::var(SESSION_COMMIT_CHILD_ID)
            .expect("session commit child id must be configured");
        let mode = std::env::var(SESSION_COMMIT_CHILD_MODE)
            .expect("session commit child mode must be configured");
        let ready = PathBuf::from(
            std::env::var_os(SESSION_COMMIT_CHILD_READY)
                .expect("session commit child ready path must be configured"),
        );
        let release = std::env::var_os(SESSION_COMMIT_CHILD_RELEASE).map(PathBuf::from);
        let store = SessionStore::new(root);
        let result = store.with_skill_usage_lock(|| {
            store.with_session_lock(&id, |directory| {
                let mut opened = store
                    .load_opened_unlocked(directory, &id)?
                    .ok_or_else(|| SessionError::NotFound(id.clone()))?;
                opened.session.summary = Some("child must not publish".to_string());
                opened.session.revision =
                    opened.session.revision.checked_add(1).ok_or_else(|| {
                        SessionError::InvalidMutation("session revision overflowed".to_string())
                    })?;
                store.save_unlocked_with_commit_check(
                    directory,
                    &opened.session,
                    Some(&opened.file),
                    || {
                        fs::write(&ready, b"ready")?;
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(30);
                        loop {
                            if release.as_ref().is_some_and(|path| path.exists()) {
                                return Ok(());
                            }
                            if std::time::Instant::now() >= deadline {
                                return Err(SessionError::InvalidMutation(
                                    "session commit child timed out".to_string(),
                                ));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    },
                )
            })
        });

        match mode.as_str() {
            "replace" => {
                let error = result.expect_err("commit-barrier replacement must fail closed");
                assert!(error.to_string().contains("identity changed"), "{error}");
            }
            "kill" => panic!("session crash child unexpectedly left its commit barrier"),
            value => panic!("unsupported session commit child mode: {value}"),
        }
    }

    #[cfg(unix)]
    fn spawn_session_commit_child(
        root: &Path,
        id: &str,
        mode: &str,
        ready: &Path,
        release: Option<&Path>,
    ) -> std::process::Child {
        let _ = fs::remove_file(ready);
        if let Some(release) = release {
            let _ = fs::remove_file(release);
        }
        let mut command = std::process::Command::new(
            std::env::current_exe().expect("current session test binary"),
        );
        command
            .args([
                "--exact",
                "session::tests::real_child_session_commit_barrier_and_fsync_crash_recovery",
                "--nocapture",
            ])
            .env(SESSION_COMMIT_CHILD_ROOT, root)
            .env(SESSION_COMMIT_CHILD_ID, id)
            .env(SESSION_COMMIT_CHILD_MODE, mode)
            .env(SESSION_COMMIT_CHILD_READY, ready);
        if let Some(release) = release {
            command.env(SESSION_COMMIT_CHILD_RELEASE, release);
        }
        command.spawn().expect("spawn session commit child")
    }

    #[cfg(unix)]
    fn wait_for_session_commit_child(child: &mut std::process::Child, ready: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect session commit child") {
                panic!("session commit child exited before readiness: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session commit child did not become ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn session_temporary_paths(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("list session directory")
            .map(|entry| entry.expect("session directory entry").path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".nib-session-") && name.ends_with(".tmp")
                })
            })
            .collect()
    }
}
