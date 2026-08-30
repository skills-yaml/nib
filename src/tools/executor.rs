//! Central ToolExecutor: scope, policy, approval, isolation, dispatch, and audit.

use crate::config::{
    boundary_profile_tightening_error, ApprovalsConfig, ExecutionConfig, TerminalConfig,
};
use crate::integrations::mcp::McpManager;
use crate::integrations::worktree::WorktreeManager;
use crate::session::{SessionStore, ToolCallRecord};
use crate::tools::classifier::{classify_tool_call, safe_command_requires_isolation, ToolRisk};
use crate::tools::core;
use crate::tools::models::{
    AfterToolHook, ApprovalDecision, ApprovalMode, PermissionLevel, PolicyEffect, PolicyRule,
    ToolCall, ToolResult,
};
use crate::tools::registry::get_tool_metadata;
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;
use tokio::io::AsyncBufReadExt;
use uuid::Uuid;

const MAX_INSTRUCTION_POLICY_BYTES: u64 = 1_048_576;
const MAX_APPROVAL_FIELD_BYTES: usize = 240;
const MAX_APPROVAL_LINE_BYTES: usize = 320;
const MAX_APPROVAL_LINES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionExecutionPosture {
    Configured,
    Tightened,
    InvalidFailClosed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveExecutionPosture {
    pub configured_approval_preset: String,
    pub effective_approval_mode: &'static str,
    pub provider: String,
    pub profile: String,
    pub network: String,
    pub mutation_plan_gate: bool,
    pub mutation_owned_worktree_gate: bool,
    pub instruction_posture: InstructionExecutionPosture,
    pub sandbox_route: crate::sandbox::SandboxExecutionRoute,
    pub broad_or_off: bool,
}

struct ResolvedExecutionConfig {
    config: ExecutionConfig,
    policy_rules: Vec<PolicyRule>,
    instruction_posture: InstructionExecutionPosture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalContext {
    pub action: String,
    pub permission_and_risk: String,
    pub target_scope: String,
    pub network: String,
    pub worktree: String,
    pub reason: String,
    pub choices: String,
}

impl ApprovalContext {
    pub fn compatibility(call: &ToolCall, level: PermissionLevel) -> Self {
        Self {
            action: normalized_approval_action(call, &[]),
            permission_and_risk: format!("{} / not classified", permission_label(level)),
            target_scope: "not available to compatibility handler".to_string(),
            network: "not available to compatibility handler".to_string(),
            worktree: "not available to compatibility handler".to_string(),
            reason: "interactive approval requested".to_string(),
            choices: "approve once or deny".to_string(),
        }
    }

    pub fn lines(&self) -> Vec<String> {
        let lines = vec![
            format!("Action: {}", self.action),
            format!("Permission / risk: {}", self.permission_and_risk),
            format!("Target scope: {}", self.target_scope),
            format!("Network: {}", self.network),
            format!("Worktree: {}", self.worktree),
            format!("Reason: {} | Choices: {}", self.reason, self.choices),
        ]
        .into_iter()
        .map(|line| bounded_approval_field_with_limit(&line, &[], MAX_APPROVAL_LINE_BYTES))
        .collect::<Vec<_>>();
        debug_assert_eq!(lines.len(), MAX_APPROVAL_LINES);
        lines
    }

    pub fn render(&self) -> String {
        self.lines().join("\n")
    }
}

struct PreparedTaskGuard {
    task_id: Option<String>,
}

impl PreparedTaskGuard {
    fn from_output(prepared_work: bool, output: Option<&Value>) -> Self {
        let task_id = prepared_work
            .then(|| {
                output
                    .and_then(|output| output.get("task_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        Self { task_id }
    }

    fn is_armed(&self) -> bool {
        self.task_id.is_some()
    }

    fn fail(&mut self, error: impl Into<String>) -> Result<(), String> {
        if let Some(task_id) = self.task_id.take() {
            crate::daemons::task::TASK_MANAGER.compensate_prepared_task(&task_id, error.into())?;
        }
        Ok(())
    }

    fn release(&mut self) {
        self.task_id = None;
    }

    fn start(&mut self) -> Result<(), String> {
        let Some(task_id) = self.task_id.take() else {
            return Ok(());
        };
        if let Err(error) = crate::daemons::task::TASK_MANAGER.start_task(&task_id) {
            return match crate::daemons::task::TASK_MANAGER
                .compensate_prepared_task(&task_id, error.clone())
            {
                Ok(()) => Err(error),
                Err(compensation_error) => Err(format!(
                    "{error}; failed to compensate prepared task: {compensation_error}"
                )),
            };
        }
        Ok(())
    }
}

impl Drop for PreparedTaskGuard {
    fn drop(&mut self) {
        let _ = self.fail("prepared task was abandoned before executor reconciliation completed");
    }
}

const REDACTION_MARKER: &[u8] = b"[REDACTED]";
const GENERIC_SECRET_MIN_BODY_BYTES: usize = 7;
const GENERIC_SECRET_LOOKAHEAD_BYTES: usize = 4 + GENERIC_SECRET_MIN_BODY_BYTES;
const MAX_PERCENT_DECODE_PASSES: usize = 8;
const MAX_STREAM_REDACTION_PENDING_BYTES: usize = 1024 * 1024;
static GENERIC_SECRET_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?:sk|xai)-[A-Za-z0-9_-]{7,}").expect("static secret pattern is valid")
});

struct StreamingTerminalRedactor {
    stdout: TerminalStreamRedactor,
    stderr: TerminalStreamRedactor,
}

impl StreamingTerminalRedactor {
    fn new(secrets: &[String]) -> Self {
        Self {
            stdout: TerminalStreamRedactor::new(secrets),
            stderr: TerminalStreamRedactor::new(secrets),
        }
    }

    fn push(&mut self, stream: core::TerminalOutputStream, chunk: &[u8], eof: bool) -> Vec<u8> {
        match stream {
            core::TerminalOutputStream::Stdout => self.stdout.push(chunk, eof),
            core::TerminalOutputStream::Stderr => self.stderr.push(chunk, eof),
        }
    }
}

struct RedactedTerminalCapture {
    stdout: VecDeque<u8>,
    stderr: VecDeque<u8>,
    limit: usize,
}

impl RedactedTerminalCapture {
    fn new(limit: usize) -> Self {
        Self {
            stdout: VecDeque::with_capacity(limit.min(64 * 1024)),
            stderr: VecDeque::with_capacity(limit.min(64 * 1024)),
            limit,
        }
    }

    fn push(&mut self, stream: core::TerminalOutputStream, chunk: &[u8]) {
        let captured = match stream {
            core::TerminalOutputStream::Stdout => &mut self.stdout,
            core::TerminalOutputStream::Stderr => &mut self.stderr,
        };
        if chunk.len() >= self.limit {
            captured.clear();
            captured.extend(&chunk[chunk.len() - self.limit..]);
            return;
        }
        let overflow = captured
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.limit);
        if overflow > 0 {
            captured.drain(..overflow);
        }
        captured.extend(chunk);
    }

    fn snapshot(&self) -> (Vec<u8>, Vec<u8>) {
        (
            self.stdout.iter().copied().collect(),
            self.stderr.iter().copied().collect(),
        )
    }
}

struct RedactedTerminalProjection {
    redactor: StreamingTerminalRedactor,
    capture: Option<RedactedTerminalCapture>,
}

fn projected_terminal_stream(
    captured: Vec<u8>,
    limit: usize,
    raw_was_truncated: bool,
    has_sensitive_values: bool,
) -> Vec<u8> {
    if raw_was_truncated && has_sensitive_values {
        bounded_redaction_marker(limit)
    } else {
        captured
    }
}

fn bounded_redaction_marker(limit: usize) -> Vec<u8> {
    REDACTION_MARKER[..limit.min(REDACTION_MARKER.len())].to_vec()
}

fn projected_terminal_error(output: &Map<String, Value>) -> String {
    let exit_code = output
        .get("exit_code")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let stdout = output.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = output.get("stderr").and_then(Value::as_str).unwrap_or("");
    let stdout_bytes = output
        .get("stdout_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stderr_bytes = output
        .get("stderr_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stdout_retained = output
        .get("stdout_bytes_retained")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stderr_retained = output
        .get("stderr_bytes_retained")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stdout_truncated = output
        .get("stdout_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let stderr_truncated = output
        .get("stderr_truncated")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    format!(
        "command exited with {exit_code}\nstdout ({stdout_bytes} bytes, {stdout_retained} retained, truncated={stdout_truncated}):\n{}\nstderr ({stderr_bytes} bytes, {stderr_retained} retained, truncated={stderr_truncated}):\n{}",
        stdout.trim(),
        stderr.trim(),
    )
}

struct TerminalStreamRedactor {
    pending: Vec<ProjectedTerminalByte>,
    utf8_pending: Vec<u8>,
    secrets: Vec<Vec<u8>>,
    lookahead: usize,
    redacting_generic_secret: bool,
    percent_decoders: Vec<StreamingPercentDecoder>,
    overflow_percent_decoder: StreamingPercentDecoder,
    secret_decode_overflow: bool,
    failed_closed: bool,
}

struct ProjectedTerminalByte {
    value: u8,
    source: TerminalByteSource,
}

enum TerminalByteSource {
    Single(u8),
    Multiple(Vec<u8>),
}

impl TerminalByteSource {
    fn len(&self) -> usize {
        match self {
            Self::Single(_) => 1,
            Self::Multiple(bytes) => bytes.len(),
        }
    }

    fn append_to(self, output: &mut Vec<u8>) {
        match self {
            Self::Single(byte) => output.push(byte),
            Self::Multiple(mut bytes) => output.append(&mut bytes),
        }
    }

    fn append_to_ref(&self, output: &mut Vec<u8>) {
        match self {
            Self::Single(byte) => output.push(*byte),
            Self::Multiple(bytes) => output.extend_from_slice(bytes),
        }
    }
}

#[derive(Default)]
struct StreamingPercentDecoder {
    pending: VecDeque<ProjectedTerminalByte>,
}

impl StreamingPercentDecoder {
    fn push(
        &mut self,
        input: Vec<ProjectedTerminalByte>,
        eof: bool,
    ) -> (Vec<ProjectedTerminalByte>, bool) {
        self.pending.extend(input);
        let mut output = Vec::with_capacity(self.pending.len());
        let mut decoded_escape = false;
        while let Some(first) = self.pending.front() {
            if first.value != b'%' {
                output.push(self.pending.pop_front().expect("percent decoder front"));
                continue;
            }
            if self.pending.len() < 3 {
                if !eof {
                    break;
                }
                output.extend(self.pending.drain(..));
                break;
            }
            let high = percent_hex_value(self.pending.get(1).expect("percent high byte").value);
            let low = percent_hex_value(self.pending.get(2).expect("percent low byte").value);
            let (Some(high), Some(low)) = (high, low) else {
                output.push(self.pending.pop_front().expect("percent decoder front"));
                continue;
            };
            let bytes = [
                self.pending.pop_front().expect("percent marker"),
                self.pending.pop_front().expect("percent high byte"),
                self.pending.pop_front().expect("percent low byte"),
            ];
            let source_len = bytes.iter().map(|byte| byte.source.len()).sum();
            let mut source = Vec::with_capacity(source_len);
            for byte in bytes {
                byte.source.append_to(&mut source);
            }
            output.push(ProjectedTerminalByte {
                value: (high << 4) | low,
                source: TerminalByteSource::Multiple(source),
            });
            decoded_escape = true;
        }
        (output, decoded_escape)
    }
}

impl TerminalStreamRedactor {
    fn new(secrets: &[String]) -> Self {
        let mut secret_decode_overflow = false;
        let mut secrets = secrets
            .iter()
            .flat_map(|secret| match percent_decoded_byte_stages(secret) {
                Some(stages) => stages
                    .into_iter()
                    .map(|stage| stage.bytes)
                    .collect::<Vec<_>>(),
                None => {
                    secret_decode_overflow = true;
                    Vec::new()
                }
            })
            .filter(|secret| !secret.is_empty())
            .collect::<Vec<_>>();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        let lookahead = secrets
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(GENERIC_SECRET_LOOKAHEAD_BYTES);
        Self {
            pending: Vec::new(),
            utf8_pending: Vec::new(),
            secrets,
            lookahead,
            redacting_generic_secret: false,
            percent_decoders: (0..MAX_PERCENT_DECODE_PASSES)
                .map(|_| StreamingPercentDecoder::default())
                .collect(),
            overflow_percent_decoder: StreamingPercentDecoder::default(),
            secret_decode_overflow,
            failed_closed: false,
        }
    }

    fn push(&mut self, chunk: &[u8], eof: bool) -> Vec<u8> {
        if self.failed_closed {
            return Vec::new();
        }
        if self.secret_decode_overflow {
            self.fail_closed();
            return REDACTION_MARKER.to_vec();
        }
        let mut projected = chunk
            .iter()
            .copied()
            .map(|byte| ProjectedTerminalByte {
                value: byte,
                source: TerminalByteSource::Single(byte),
            })
            .collect();
        for decoder in &mut self.percent_decoders {
            projected = decoder.push(projected, eof).0;
        }
        let (projected, percent_decode_overflow) =
            self.overflow_percent_decoder.push(projected, eof);
        if percent_decode_overflow {
            self.fail_closed();
            return REDACTION_MARKER.to_vec();
        }
        let redacted = self.redact_bytes(projected, eof);
        let decoded = self.decode_utf8(redacted, eof);
        let decoded = std::str::from_utf8(&decoded).expect("terminal redactor returns valid UTF-8");
        crate::interactive::control_safe_text(decoded, true).into_bytes()
    }

    fn fail_closed(&mut self) {
        self.pending.clear();
        self.utf8_pending.clear();
        self.redacting_generic_secret = false;
        self.failed_closed = true;
    }

    fn redact_bytes(&mut self, chunk: Vec<ProjectedTerminalByte>, eof: bool) -> Vec<u8> {
        self.pending.extend(chunk);
        let pending_source_bytes = self
            .pending
            .iter()
            .map(|byte| byte.source.len())
            .try_fold(0usize, usize::checked_add);
        if !matches!(pending_source_bytes, Some(bytes) if bytes <= MAX_STREAM_REDACTION_PENDING_BYTES)
        {
            self.fail_closed();
            return REDACTION_MARKER.to_vec();
        }
        let mut output = Vec::with_capacity(self.pending.len());
        let mut index = 0usize;

        if self.redacting_generic_secret {
            while index < self.pending.len()
                && is_generic_secret_body_byte(self.pending[index].value)
            {
                index += 1;
            }
            if index == self.pending.len() {
                self.pending.clear();
                if eof {
                    self.redacting_generic_secret = false;
                }
                return output;
            }
            self.redacting_generic_secret = false;
        }

        let process_before = if eof {
            self.pending.len()
        } else {
            self.pending
                .len()
                .saturating_sub(self.lookahead.saturating_sub(1))
        };
        while index < self.pending.len() && (eof || index < process_before) {
            if let Some(secret_len) = self
                .secrets
                .iter()
                .find(|secret| terminal_bytes_start_with(&self.pending[index..], secret))
                .map(Vec::len)
            {
                output.extend_from_slice(REDACTION_MARKER);
                index += secret_len;
                continue;
            }

            if let Some(prefix_len) = generic_terminal_secret_prefix_len(&self.pending[index..]) {
                let body_start = index + prefix_len;
                let mut end = body_start;
                while end < self.pending.len()
                    && is_generic_secret_body_byte(self.pending[end].value)
                {
                    end += 1;
                }
                if end.saturating_sub(body_start) >= GENERIC_SECRET_MIN_BODY_BYTES {
                    output.extend_from_slice(REDACTION_MARKER);
                    index = end;
                    if end == self.pending.len() && !eof {
                        self.redacting_generic_secret = true;
                    }
                    continue;
                }
                if end == self.pending.len() && !eof {
                    break;
                }
            }

            self.pending[index].source.append_to_ref(&mut output);
            index += 1;
        }
        self.pending.drain(..index);
        output
    }

    fn decode_utf8(&mut self, bytes: Vec<u8>, eof: bool) -> Vec<u8> {
        self.utf8_pending.extend(bytes);
        let mut output = Vec::with_capacity(self.utf8_pending.len());
        loop {
            let validation = match std::str::from_utf8(&self.utf8_pending) {
                Ok(_) => {
                    output.append(&mut self.utf8_pending);
                    break;
                }
                Err(error) => (error.valid_up_to(), error.error_len()),
            };
            let (valid_up_to, error_len) = validation;
            output.extend(self.utf8_pending.drain(..valid_up_to));
            match error_len {
                Some(error_len) => {
                    self.utf8_pending.drain(..error_len);
                    output.extend_from_slice("�".as_bytes());
                }
                None if eof => {
                    self.utf8_pending.clear();
                    output.extend_from_slice("�".as_bytes());
                    break;
                }
                None => break,
            }
        }
        output
    }
}

fn terminal_bytes_start_with(bytes: &[ProjectedTerminalByte], expected: &[u8]) -> bool {
    bytes.len() >= expected.len()
        && bytes
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.value == *expected)
}

fn generic_terminal_secret_prefix_len(bytes: &[ProjectedTerminalByte]) -> Option<usize> {
    if terminal_bytes_start_with(bytes, b"sk-") {
        Some(3)
    } else if terminal_bytes_start_with(bytes, b"xai-") {
        Some(4)
    } else {
        None
    }
}

fn is_generic_secret_body_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

#[async_trait::async_trait]
pub trait ApprovalHandler: Send + Sync {
    fn approval_ceiling(
        &self,
        _call: &ToolCall,
        _level: PermissionLevel,
        _risk: ToolRisk,
    ) -> Option<ApprovalDecision> {
        None
    }

    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision;

    async fn handle_approval_with_context(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        _context: &ApprovalContext,
    ) -> ApprovalDecision {
        self.handle_approval(call, level).await
    }
}

pub trait ToolPolicyHook: Send + Sync {
    fn evaluate(&self, call: &ToolCall, project_root: &Path) -> Option<PolicyRule>;
}

pub struct StdinApprovalHandler;

#[async_trait::async_trait]
impl ApprovalHandler for StdinApprovalHandler {
    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision {
        let context = ApprovalContext::compatibility(call, level);
        self.prompt(&context).await
    }

    async fn handle_approval_with_context(
        &self,
        _call: &ToolCall,
        _level: PermissionLevel,
        context: &ApprovalContext,
    ) -> ApprovalDecision {
        self.prompt(context).await
    }
}

impl StdinApprovalHandler {
    async fn prompt(&self, context: &ApprovalContext) -> ApprovalDecision {
        eprintln!("\nApproval required\n{}", context.render());
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
    implicit_session_id: Option<String>,
    pub approval_mode: ApprovalMode,
    pub project_root: PathBuf,
    pub auto_approve: bool,
    pub execution_config: ExecutionConfig,
    pub terminal_backend: String,
    pub terminal_timeout_secs: u64,
    pub approval_handler: Arc<dyn ApprovalHandler>,
    worktree_manager: Option<WorktreeManager>,
    pub mcp_manager: Option<Arc<McpManager>>,
    policy_rules: Vec<PolicyRule>,
    policy_hooks: Vec<Arc<dyn ToolPolicyHook>>,
    after_tool_hooks: Vec<AfterToolHook>,
    environment: HashMap<String, String>,
    sensitive_values: Vec<String>,
    defer_background_start: bool,
    terminal_output_callback: Option<core::TerminalOutputCallback>,
    cancellation: Option<crate::agent::CancellationSignal>,
}

impl ToolExecutor {
    pub fn new(project_root: PathBuf, execution_config: ExecutionConfig) -> Self {
        let project_root = project_root.canonicalize().unwrap_or(project_root);
        let resolved = resolve_execution_config(&project_root, execution_config);
        Self {
            session_store: None,
            implicit_session_id: None,
            approval_mode: ApprovalMode::Manual,
            project_root,
            auto_approve: false,
            execution_config: resolved.config,
            terminal_backend: TerminalConfig::default().backend,
            terminal_timeout_secs: TerminalConfig::default().timeout,
            approval_handler: Arc::new(StdinApprovalHandler),
            worktree_manager: None,
            mcp_manager: None,
            policy_rules: resolved.policy_rules,
            policy_hooks: Vec::new(),
            after_tool_hooks: Vec::new(),
            environment: HashMap::new(),
            sensitive_values: Vec::new(),
            defer_background_start: false,
            terminal_output_callback: None,
            cancellation: None,
        }
    }

    pub fn effective_execution_posture(
        project_root: &Path,
        execution_config: ExecutionConfig,
        approvals_config: &ApprovalsConfig,
    ) -> EffectiveExecutionPosture {
        let project_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let resolved = resolve_execution_config(&project_root, execution_config);
        let approval_mode = approval_mode_from_config(approvals_config);
        let sandbox_route = crate::sandbox::resolve_sandbox_execution_route(
            &resolved.config.provider,
            &resolved.config.default_profile,
            &resolved.config.boundaries,
        );
        let broad_or_off = approval_mode == ApprovalMode::Off
            || matches!(sandbox_route, crate::sandbox::SandboxExecutionRoute::Direct)
            || resolved.config.boundaries.network == "enabled";
        EffectiveExecutionPosture {
            configured_approval_preset: approvals_config.mode.clone(),
            effective_approval_mode: approval_mode_label(approval_mode),
            provider: resolved.config.provider,
            profile: resolved.config.default_profile,
            network: resolved.config.boundaries.network,
            mutation_plan_gate: resolved.config.plan_mode,
            mutation_owned_worktree_gate: true,
            instruction_posture: resolved.instruction_posture,
            sandbox_route,
            broad_or_off,
        }
    }

    pub fn with_auto_approve(mut self, auto_approve: bool) -> Self {
        self.auto_approve = auto_approve;
        self
    }

    pub fn with_approval_mode(mut self, approval_mode: ApprovalMode) -> Self {
        self.approval_mode = approval_mode;
        self
    }

    pub fn with_approvals_config(mut self, config: &ApprovalsConfig) -> Self {
        self.approval_mode = approval_mode_from_config(config);
        self
    }

    pub fn with_terminal_config(mut self, config: &TerminalConfig) -> Self {
        self.terminal_backend = config.backend.clone();
        self.terminal_timeout_secs = config.timeout.max(1);
        self
    }

    pub fn with_approval_handler(mut self, handler: Arc<dyn ApprovalHandler>) -> Self {
        self.approval_handler = handler;
        self
    }

    pub fn with_session_store(mut self, session_store: SessionStore) -> Self {
        self.session_store = Some(session_store);
        self
    }

    pub fn with_environment(mut self, environment: &HashMap<String, String>) -> Self {
        self.environment = environment.clone();
        self
    }

    pub fn with_sensitive_values(mut self, values: impl IntoIterator<Item = String>) -> Self {
        self.sensitive_values.extend(values);
        normalize_sensitive_values(&mut self.sensitive_values);
        self
    }

    pub fn with_cancellation(mut self, cancellation: crate::agent::CancellationSignal) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Keep background terminal jobs paused until the caller has persisted the
    /// provisional observation that names the task.
    pub fn with_deferred_background_start(mut self, deferred: bool) -> Self {
        self.defer_background_start = deferred;
        self
    }

    pub fn with_terminal_output_callback(mut self, callback: core::TerminalOutputCallback) -> Self {
        self.terminal_output_callback = Some(callback);
        self
    }

    pub fn with_terminal_output_sender(
        self,
        sender: tokio::sync::mpsc::Sender<core::TerminalOutputEvent>,
    ) -> Self {
        self.with_terminal_output_callback(Arc::new(move |event| {
            let _ = sender.try_send(event);
        }))
    }

    pub fn with_mcp_manager(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    pub fn with_policy_rules(mut self, rules: impl IntoIterator<Item = PolicyRule>) -> Self {
        self.policy_rules.extend(rules);
        self
    }

    pub fn with_policy_hook(mut self, hook: Arc<dyn ToolPolicyHook>) -> Self {
        self.policy_hooks.push(hook);
        self
    }

    pub fn with_after_tool_hooks(mut self, hooks: impl IntoIterator<Item = AfterToolHook>) -> Self {
        self.after_tool_hooks.extend(hooks);
        self
    }

    pub async fn get_tools_schema(&self) -> Vec<Value> {
        let mut schemas = tools_json_schema();
        if let Some(mcp) = &self.mcp_manager {
            if let Ok(mcp_tools) = mcp.list_tools().await {
                schemas.extend(mcp_tools.into_iter().map(|tool| {
                    json!({
                        "type": "function",
                        "function": tool,
                    })
                }));
            }
        }
        schemas
    }

    pub async fn execute(&mut self, mut call: ToolCall, session_id: Option<&str>) -> ToolResult {
        let requested_session = session_id
            .map(str::to_string)
            .or_else(|| call.session_id.clone());
        let has_authoritative_session = requested_session.is_some();
        let effective_session = match requested_session {
            Some(session_id) => session_id,
            None => {
                if let Some(session_id) = self.implicit_session_id.clone() {
                    return self
                        .execute_inner(call, Some(&session_id), true, false)
                        .await;
                }
                if self.session_store.is_none() {
                    match SessionStore::for_project(&self.project_root) {
                        Ok(store) => self.session_store = Some(store),
                        Err(error) => {
                            return ToolResult {
                                invocation_id: call.invocation_id,
                                tool_name: call.tool_name,
                                success: false,
                                output: None,
                                error: Some(format!(
                                    "failed to resolve profile session store for mandatory audit: {error}"
                                )),
                                duration_seconds: 0.0,
                                approval_granted: false,
                                approval_source: Some("audit".to_string()),
                            };
                        }
                    }
                }
                let store = self
                    .session_store
                    .as_ref()
                    .expect("session store initialized above");
                let session = match store.try_create_session() {
                    Ok(session) => session,
                    Err(error) => {
                        return ToolResult {
                            invocation_id: call.invocation_id,
                            tool_name: call.tool_name,
                            success: false,
                            output: None,
                            error: Some(format!(
                                "failed to create mandatory tool audit session: {error}"
                            )),
                            duration_seconds: 0.0,
                            approval_granted: false,
                            approval_source: Some("audit".to_string()),
                        };
                    }
                };
                if let Err(error) = store.record_event(
                    &session.id,
                    "implicit_audit_session",
                    json!({"tool_name": self.redact_text(&call.tool_name)}),
                ) {
                    return ToolResult {
                        invocation_id: call.invocation_id,
                        tool_name: call.tool_name,
                        success: false,
                        output: None,
                        error: Some(format!(
                            "failed to initialize mandatory tool audit session: {error}"
                        )),
                        duration_seconds: 0.0,
                        approval_granted: false,
                        approval_source: Some("audit".to_string()),
                    };
                }
                self.implicit_session_id = Some(session.id.clone());
                session.id
            }
        };
        if has_authoritative_session {
            call.session_id = Some(effective_session.clone());
        }
        self.execute_inner(
            call,
            Some(&effective_session),
            true,
            has_authoritative_session,
        )
        .await
    }

    /// Reports whether this call will invoke the configured interactive
    /// approval handler. Policy denials and automatic classifier decisions do
    /// not claim that user approval is pending.
    pub fn requires_interactive_approval(&self, call: &ToolCall) -> bool {
        let metadata = get_tool_metadata(&call.tool_name);
        let is_mcp_tool = metadata.is_none() && call.tool_name.contains("::");
        if metadata.is_none() && !is_mcp_tool {
            return false;
        }
        let level = metadata
            .map(|value| value.permission_level)
            .unwrap_or(PermissionLevel::Network);
        let requires_approval = metadata
            .map(|value| value.requires_approval)
            .unwrap_or(true);
        let risk = if is_mcp_tool {
            ToolRisk::Network
        } else {
            classify_tool_call(call)
        };
        let Ok(effective_root) = self.resolve_scope(call) else {
            return false;
        };
        let evaluations = self.matching_policy_rules(call, &effective_root);

        if evaluations
            .iter()
            .any(|rule| rule.effect == PolicyEffect::Deny)
        {
            return false;
        }
        if evaluations
            .iter()
            .any(|rule| rule.effect == PolicyEffect::RequireApproval)
        {
            return true;
        }
        if evaluations
            .iter()
            .any(|rule| rule.effect == PolicyEffect::Allow)
        {
            return false;
        }
        if matches!(level, PermissionLevel::ReadOnly | PermissionLevel::Plan)
            || risk == ToolRisk::ReadOnly
            || (call.tool_name == "run_terminal"
                && risk == ToolRisk::Safe
                && self.classifier_auto_approval_allowed(call, level, risk))
            || (!requires_approval && risk == ToolRisk::Safe)
        {
            return false;
        }
        !matches!(self.approval_mode, ApprovalMode::Policy | ApprovalMode::Off)
            && !self.auto_approve
    }

    async fn execute_inner(
        &mut self,
        call: ToolCall,
        session_id: Option<&str>,
        run_after_hooks: bool,
        has_authoritative_session: bool,
    ) -> ToolResult {
        let start = Instant::now();
        let effective_session = session_id.or(call.session_id.as_deref());
        if let Some(session_id) = effective_session {
            if !valid_session_id(session_id) {
                return ToolResult {
                    invocation_id: call.invocation_id,
                    tool_name: call.tool_name.clone(),
                    success: false,
                    output: None,
                    error: Some("invalid session id".to_string()),
                    duration_seconds: start.elapsed().as_secs_f64(),
                    approval_granted: false,
                    approval_source: Some("policy".to_string()),
                };
            }
            if self.session_store.is_none() {
                match SessionStore::for_project(&self.project_root) {
                    Ok(store) => self.session_store = Some(store),
                    Err(error) => {
                        return ToolResult {
                            invocation_id: call.invocation_id,
                            tool_name: call.tool_name.clone(),
                            success: false,
                            output: None,
                            error: Some(format!(
                                "failed to resolve profile session store for audit: {error}"
                            )),
                            duration_seconds: start.elapsed().as_secs_f64(),
                            approval_granted: false,
                            approval_source: Some("audit".to_string()),
                        };
                    }
                }
            }
            if let Err(error) = self.record_attempt(&call, session_id) {
                return ToolResult {
                    invocation_id: call.invocation_id,
                    tool_name: call.tool_name.clone(),
                    success: false,
                    output: None,
                    error: Some(format!("failed to audit tool attempt: {error}")),
                    duration_seconds: start.elapsed().as_secs_f64(),
                    approval_granted: false,
                    approval_source: Some("audit".to_string()),
                };
            }
        }
        let metadata = get_tool_metadata(&call.tool_name);
        let is_mcp_tool = metadata.is_none() && call.tool_name.contains("::");
        if metadata.is_none() && !is_mcp_tool {
            return self.finish_failure(
                &call,
                effective_session,
                start,
                format!("Unknown tool: {}", call.tool_name),
                ApprovalDecision::denied_by_policy("unknown tool"),
                PermissionLevel::Destructive,
                ToolRisk::RequiresApproval,
                None,
                None,
            );
        }

        let level = metadata
            .map(|value| value.permission_level)
            .unwrap_or(PermissionLevel::Network);
        let requires_approval = metadata
            .map(|value| value.requires_approval)
            .unwrap_or(true);
        let requires_worktree = metadata
            .map(|value| value.requires_worktree)
            .unwrap_or(false);
        let risk = if is_mcp_tool {
            ToolRisk::Network
        } else {
            classify_tool_call(&call)
        };
        let effective_execution_config = self.effective_execution_config(level, risk);
        let plan_id = self.resolve_plan_id(effective_session);

        let input_schema = if let Some(metadata) = metadata {
            Ok(metadata.input_schema.clone())
        } else {
            match &self.mcp_manager {
                Some(manager) => match manager.list_tools().await {
                    Ok(tools) => tools
                        .into_iter()
                        .find(|tool| {
                            tool.get("name").and_then(Value::as_str) == Some(&call.tool_name)
                        })
                        .and_then(|tool| tool.get("parameters").cloned())
                        .ok_or_else(|| {
                            format!(
                                "MCP tool is not advertised by its server: {}",
                                call.tool_name
                            )
                        }),
                    Err(error) => Err(format!(
                        "failed to load MCP input schema for {}: {error}",
                        call.tool_name
                    )),
                },
                None => Err("MCP tool called but manager is not initialized".to_string()),
            }
        };
        let input_schema = match input_schema {
            Ok(schema) => schema,
            Err(error) => {
                return self.finish_failure(
                    &call,
                    effective_session,
                    start,
                    error,
                    ApprovalDecision::denied_by_policy("tool schema unavailable"),
                    level,
                    risk,
                    None,
                    plan_id,
                )
            }
        };
        let validation_arguments = schema_validation_arguments(&call);
        if let Err(error) =
            validate_tool_arguments(&call.tool_name, &input_schema, &validation_arguments)
        {
            return self.finish_failure(
                &call,
                effective_session,
                start,
                error,
                ApprovalDecision::denied_by_policy("invalid tool arguments"),
                level,
                risk,
                None,
                plan_id,
            );
        }

        let effective_root = match self.resolve_scope(&call) {
            Ok(root) => root,
            Err(error) => {
                return self.finish_failure(
                    &call,
                    effective_session,
                    start,
                    error,
                    ApprovalDecision::denied_by_policy("invalid scope"),
                    level,
                    risk,
                    None,
                    plan_id,
                )
            }
        };

        if (matches!(level, PermissionLevel::Network) || risk == ToolRisk::Network)
            && effective_execution_config.boundaries.network == "disabled"
        {
            return self.finish_failure(
                &call,
                effective_session,
                start,
                "network access is disabled by the effective execution boundary".to_string(),
                ApprovalDecision::denied_by_policy("network boundary disabled"),
                level,
                risk,
                None,
                plan_id,
            );
        }

        if requires_worktree && self.execution_config.plan_mode {
            let plan_approved = effective_session
                .and_then(|id| self.session_store.as_ref()?.load_result(id).ok().flatten())
                .and_then(|session| session.plan)
                .is_some_and(|plan| {
                    plan.approved
                        && plan.has_identity()
                        && plan.is_structured()
                        && !plan.is_complete()
                });
            if !plan_approved {
                return self.finish_failure(
                    &call,
                    effective_session,
                    start,
                    "mutating execution requires an approved persisted session plan with a valid identity and incomplete work"
                        .to_string(),
                    ApprovalDecision::denied_by_policy("missing approved plan gate"),
                    level,
                    risk,
                    None,
                    plan_id,
                );
            }
        }

        let approval = self
            .handle_approval(
                &call,
                level,
                risk,
                requires_approval,
                requires_worktree,
                &effective_root,
                &effective_execution_config,
                effective_session,
            )
            .await;
        if !approval.granted {
            return self.finish_failure(
                &call,
                effective_session,
                start,
                "Approval denied".to_string(),
                approval,
                level,
                risk,
                None,
                plan_id,
            );
        }

        let worktree = match self
            .ensure_worktree(requires_worktree, &effective_root, effective_session)
            .await
        {
            Ok(worktree) => worktree,
            Err(error) => {
                return self.finish_failure(
                    &call,
                    effective_session,
                    start,
                    error,
                    ApprovalDecision::denied_by_policy("worktree isolation failed"),
                    level,
                    risk,
                    None,
                    plan_id,
                )
            }
        };

        let execution_root = worktree.as_deref().unwrap_or(&effective_root);
        let mut dispatch_arguments = call.arguments.clone();
        if matches!(
            call.tool_name.as_str(),
            "spawn_subagent" | "invoke_subagent"
        ) {
            if let Some(arguments) = dispatch_arguments.as_object_mut() {
                arguments.remove("_parent_session_id");
                arguments.remove("_audit_sessions_dir");
                if let Some(session) = effective_session {
                    arguments.insert(
                        "_parent_session_id".to_string(),
                        Value::String(session.to_string()),
                    );
                    if let Some(store) = self.session_store.as_ref() {
                        let audit_sessions_dir =
                            match crate::tools::delegation::serialize_subagent_audit_destination(
                                store,
                            ) {
                                Ok(path) => path,
                                Err(error) => {
                                    return self.finish_failure(
                                        &call,
                                        effective_session,
                                        start,
                                        error,
                                        approval,
                                        level,
                                        risk,
                                        worktree.as_deref(),
                                        plan_id,
                                    );
                                }
                            };
                        arguments.insert("_audit_sessions_dir".to_string(), audit_sessions_dir);
                    }
                }
            }
        }
        let background_terminal = call.tool_name == "run_terminal"
            && call
                .arguments
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if call.tool_name == "schedule" || call.tool_name == "run_terminal" {
            if let Some(arguments) = dispatch_arguments.as_object_mut() {
                arguments.remove("_session_id");
                arguments.remove("_sessions_dir");
                if has_authoritative_session
                    && (call.tool_name == "schedule" || background_terminal)
                {
                    if let (Some(session), Some(store)) =
                        (effective_session, self.session_store.as_ref())
                    {
                        arguments.insert(
                            "_session_id".to_string(),
                            Value::String(session.to_string()),
                        );
                        arguments.insert(
                            "_sessions_dir".to_string(),
                            Value::String(store.sessions_dir().to_string_lossy().to_string()),
                        );
                    }
                }
            }
        }

        let terminal_capture_limit = (call.tool_name == "run_terminal" && !background_terminal)
            .then(|| core::terminal_output_limit(&dispatch_arguments).ok())
            .flatten();
        let (terminal_output_callback, terminal_projection) =
            self.redacted_terminal_output_projection(terminal_capture_limit);
        let outcome = if call.tool_name == "merge_subagent_worktree" {
            self.execute_subagent_merge(&dispatch_arguments, &effective_root, effective_session)
                .await
        } else if is_mcp_tool {
            match &self.mcp_manager {
                Some(mcp) => mcp
                    .call_tool(&call.tool_name, dispatch_arguments)
                    .await
                    .map_err(|error| error.to_string()),
                None => Err("MCP tool called but manager is not initialized".to_string()),
            }
        } else {
            core::dispatch(
                &call.tool_name,
                &dispatch_arguments,
                execution_root,
                &effective_execution_config,
                &self.terminal_backend,
                self.terminal_timeout_secs,
                &self.environment,
                terminal_output_callback.as_ref(),
                self.cancellation.as_ref(),
            )
            .await
        };
        let outcome = outcome.map(|output| {
            self.project_authoritative_terminal_output(
                output,
                terminal_projection.as_ref(),
                terminal_capture_limit,
            )
        });
        let mut prepared_task = PreparedTaskGuard::from_output(
            background_terminal || call.tool_name == "schedule",
            outcome.as_ref().ok(),
        );

        let (success, output, error) = match outcome {
            Ok(output)
                if call.tool_name == "run_terminal"
                    && output.get("command_success").and_then(Value::as_bool) == Some(false) =>
            {
                let error = output
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("terminal command failed")
                    .to_string();
                (
                    false,
                    Some(self.redact_value(output)),
                    Some(self.redact_text(&error)),
                )
            }
            Ok(output) => (true, Some(self.redact_value(output)), None),
            Err(error) => (false, None, Some(self.redact_text(&error))),
        };
        let mut result = ToolResult {
            invocation_id: call.invocation_id,
            tool_name: call.tool_name.clone(),
            success,
            output,
            error,
            duration_seconds: start.elapsed().as_secs_f64(),
            approval_granted: true,
            approval_source: Some(approval.source.clone()),
        };
        if result.success && run_after_hooks {
            let hooks: Vec<_> = self
                .after_tool_hooks
                .iter()
                .filter(|hook| hook.tool_name == call.tool_name)
                .cloned()
                .collect();
            let mut hook_results = Vec::with_capacity(hooks.len());
            let hook_session_id = effective_session.map(str::to_string);
            for hook in hooks {
                let hook_call = ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "run_terminal".to_string(),
                    arguments: json!({
                        "command": hook.command,
                        "hook_source": hook.source,
                        "hook_for": call.tool_name,
                    }),
                    session_id: hook_session_id.clone(),
                    project_root: Some(effective_root.clone()),
                };
                let hook_result = Box::pin(self.execute_inner(
                    hook_call,
                    hook_session_id.as_deref(),
                    false,
                    has_authoritative_session,
                ))
                .await;
                hook_results.push(json!({
                    "source": hook.source,
                    "command": self.redact_text(&hook.command),
                    "success": hook_result.success,
                    "error": hook_result.error,
                }));
                if !hook_result.success {
                    result.success = false;
                    result.error = Some(format!(
                        "after-tool hook from {} failed: {}",
                        hook.source,
                        hook_result.error.as_deref().unwrap_or("unknown error")
                    ));
                    break;
                }
            }
            if !hook_results.is_empty() {
                result.output = Some(match result.output.take() {
                    Some(Value::Object(mut output)) => {
                        output.insert("post_hooks".to_string(), Value::Array(hook_results));
                        Value::Object(output)
                    }
                    Some(output) => json!({"result": output, "post_hooks": hook_results}),
                    None => json!({"post_hooks": hook_results}),
                });
            }
        }
        result.duration_seconds = start.elapsed().as_secs_f64();
        self.sanitize_tool_result(&mut result);
        let record_succeeded = if let Err(error) = self.record(
            &call,
            &result,
            &approval,
            effective_session,
            worktree.as_deref(),
            level,
            risk,
            plan_id,
        ) {
            result.success = false;
            result.error = Some(format!("audit recording failed: {error}"));
            false
        } else {
            true
        };
        if prepared_task.is_armed() {
            if !record_succeeded || !result.success {
                if let Err(compensation_error) = prepared_task.fail(
                    "prepared task was not started because executor reconciliation failed"
                        .to_string(),
                ) {
                    result.success = false;
                    result.error = Some(match result.error.take() {
                        Some(existing) => format!(
                            "{existing}; failed to compensate prepared task: {compensation_error}"
                        ),
                        None => format!("failed to compensate prepared task: {compensation_error}"),
                    });
                }
            } else if self.defer_background_start {
                prepared_task.release();
            } else if let Err(error) = prepared_task.start() {
                result.success = false;
                result.error = Some(match result.error.take() {
                    Some(existing) => {
                        format!("{existing}; failed to start prepared task: {error}")
                    }
                    None => format!("failed to start prepared task: {error}"),
                });
            }
        }
        self.sanitize_tool_result(&mut result);
        result
    }

    async fn execute_subagent_merge(
        &self,
        arguments: &Value,
        project_root: &Path,
        session_id: Option<&str>,
    ) -> Result<Value, String> {
        let subagent_id = arguments
            .get("subagent_id")
            .and_then(Value::as_str)
            .ok_or("missing subagent_id")?;
        let command = arguments
            .get("verification_command")
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .ok_or("verification_command is required before merge")?;
        let timeout = arguments
            .get("verification_timeout")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .clamp(1, 3_600);
        let verification_target = crate::tools::delegation::prepare_subagent_verification_target(
            project_root,
            subagent_id,
            self.cancellation.as_ref(),
        )
        .await?;
        let worktree = verification_target.worktree_path;

        let terminal_config = TerminalConfig {
            backend: self.terminal_backend.clone(),
            timeout: self.terminal_timeout_secs,
        };
        let mut verifier = ToolExecutor::new(worktree.clone(), self.execution_config.clone())
            .with_auto_approve(self.auto_approve)
            .with_approval_mode(self.approval_mode)
            .with_terminal_config(&terminal_config)
            .with_approval_handler(self.approval_handler.clone())
            .with_environment(&self.environment)
            .with_sensitive_values(self.sensitive_values.clone());
        if let Some(store) = self.session_store.clone() {
            verifier = verifier.with_session_store(store);
        }
        verifier.policy_rules.extend(self.policy_rules.clone());
        verifier.policy_hooks = self.policy_hooks.clone();
        verifier.terminal_output_callback = self.terminal_output_callback.clone();

        let configured_provider = verifier.execution_config.provider.clone();
        let sandbox_profile = verifier.execution_config.default_profile.clone();
        let boundaries = verifier.execution_config.boundaries.clone();
        let verification = Box::pin(verifier.execute_inner(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({
                    "command": command,
                    "timeout": timeout,
                }),
                session_id: session_id.map(str::to_string),
                project_root: None,
            },
            session_id,
            false,
            session_id.is_some(),
        ))
        .await;
        let evidence = crate::tools::delegation::VerificationEvidence {
            tool_name: "run_terminal".to_string(),
            command: self.redact_text(command),
            worktree_path: worktree,
            success: verification.success,
            output: verification.output,
            error: verification.error,
            approval_granted: verification.approval_granted,
            approval_source: verification.approval_source,
            duration_seconds: verification.duration_seconds,
            configured_provider,
            sandbox_profile,
            boundaries,
            session_id: session_id.map(str::to_string),
            snapshot_commit: Some(verification_target.snapshot_commit),
            executed_at: Utc::now(),
        };
        crate::tools::delegation::merge_verified_subagent_worktree(
            arguments,
            project_root,
            evidence,
            self.cancellation.as_ref(),
        )
        .await
    }

    fn resolve_scope(&self, call: &ToolCall) -> Result<PathBuf, String> {
        let configured = self.project_root.canonicalize().map_err(|error| {
            format!(
                "configured project root {} is unavailable: {error}",
                self.project_root.display()
            )
        })?;
        let requested = call.project_root.as_deref().unwrap_or(&configured);
        let requested = requested.canonicalize().map_err(|error| {
            format!(
                "requested project root {} is unavailable: {error}",
                requested.display()
            )
        })?;
        if !requested.starts_with(&configured) {
            return Err(format!(
                "requested project root {} is outside configured root {}",
                requested.display(),
                configured.display()
            ));
        }
        Ok(requested)
    }

    #[cfg(test)]
    fn redacted_terminal_output_callback(&self) -> Option<core::TerminalOutputCallback> {
        self.redacted_terminal_output_projection(None).0
    }

    fn redacted_terminal_output_projection(
        &self,
        capture_limit: Option<usize>,
    ) -> (
        Option<core::TerminalOutputCallback>,
        Option<Arc<Mutex<RedactedTerminalProjection>>>,
    ) {
        let callback = self.terminal_output_callback.clone();
        if callback.is_none() && capture_limit.is_none() {
            return (None, None);
        }
        let secrets = normalized_encoded_sensitive_values(self.redaction_secrets());
        let projection = Arc::new(Mutex::new(RedactedTerminalProjection {
            redactor: StreamingTerminalRedactor::new(&secrets),
            capture: capture_limit.map(RedactedTerminalCapture::new),
        }));
        let callback_projection = Arc::clone(&projection);
        let projected_callback: core::TerminalOutputCallback = Arc::new(move |event| {
            let redacted = {
                let Ok(mut projection) = callback_projection.lock() else {
                    return;
                };
                let redacted = projection
                    .redactor
                    .push(event.stream, &event.chunk, event.eof);
                if let Some(capture) = projection.capture.as_mut() {
                    capture.push(event.stream, &redacted);
                }
                redacted
            };
            if redacted.is_empty() {
                return;
            }
            if let Some(callback) = &callback {
                callback(core::TerminalOutputEvent {
                    tool_name: event.tool_name,
                    stream: event.stream,
                    chunk: redacted,
                    background_task_id: event.background_task_id,
                    eof: false,
                });
            }
        });
        (Some(projected_callback), Some(projection))
    }

    fn project_authoritative_terminal_output(
        &self,
        mut output: Value,
        projection: Option<&Arc<Mutex<RedactedTerminalProjection>>>,
        capture_limit: Option<usize>,
    ) -> Value {
        let (Some(projection), Some(limit), Some(object)) =
            (projection, capture_limit, output.as_object_mut())
        else {
            return output;
        };
        let snapshot = projection.lock().ok().and_then(|projection| {
            projection
                .capture
                .as_ref()
                .map(|capture| capture.snapshot())
        });
        let has_sensitive_values = !self.redaction_secrets().is_empty();
        let stdout_truncated = object
            .get("stdout_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let stderr_truncated = object
            .get("stderr_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let (stdout, stderr) = match snapshot {
            Some((stdout, stderr)) => (
                projected_terminal_stream(stdout, limit, stdout_truncated, has_sensitive_values),
                projected_terminal_stream(stderr, limit, stderr_truncated, has_sensitive_values),
            ),
            None => (
                bounded_redaction_marker(limit),
                bounded_redaction_marker(limit),
            ),
        };
        let stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        object.insert("stdout_bytes_retained".to_string(), json!(stdout.len()));
        object.insert("stderr_bytes_retained".to_string(), json!(stderr.len()));
        object.insert("stdout".to_string(), Value::String(stdout));
        object.insert("stderr".to_string(), Value::String(stderr));
        if object.get("command_success").and_then(Value::as_bool) == Some(false) {
            object.insert(
                "error".to_string(),
                Value::String(projected_terminal_error(object)),
            );
        }
        output
    }

    fn redaction_secrets(&self) -> Vec<String> {
        let mut secrets = sensitive_environment_values(&self.environment);
        secrets.extend(self.sensitive_values.iter().cloned());
        normalize_sensitive_values(&mut secrets);
        secrets
    }

    fn redact_text(&self, text: &str) -> String {
        let redacted = redact_text_with_encoded_sensitive_values(text, self.redaction_secrets());
        crate::interactive::control_safe_text(&redacted, true)
    }

    fn redact_value(&self, value: Value) -> Value {
        let redacted = redact_value_with_encoded_sensitive_values(value, self.redaction_secrets());
        control_safe_public_value(redacted)
    }

    fn sanitize_tool_result(&self, result: &mut ToolResult) {
        result.tool_name = self.redact_text(&result.tool_name);
        result.output = result.output.take().map(|output| self.redact_value(output));
        result.error = result.error.take().map(|error| self.redact_text(&error));
        result.approval_source = result
            .approval_source
            .take()
            .map(|source| self.redact_text(&source));
    }

    async fn ensure_worktree(
        &mut self,
        required: bool,
        effective_root: &Path,
        session_id: Option<&str>,
    ) -> Result<Option<PathBuf>, String> {
        if !required {
            return Ok(None);
        }
        let session_id = session_id.ok_or("mutating tools require a session id")?;
        if self.project_root.join(".git").is_file() {
            return Ok(Some(effective_root.to_path_buf()));
        }
        if self.worktree_manager.is_none() {
            self.worktree_manager = Some(WorktreeManager::new(self.project_root.clone()));
        }
        let cancellation = self.cancellation.clone();
        let manager = self
            .worktree_manager
            .as_mut()
            .ok_or("worktree manager unavailable")?;
        let worktree_root = manager
            .create_for_session_cancellable(session_id, cancellation.as_ref())
            .await?;
        let relative = effective_root
            .strip_prefix(&self.project_root)
            .map_err(|_| {
                format!(
                    "effective root {} is not under configured root {}",
                    effective_root.display(),
                    self.project_root.display()
                )
            })?;
        let target = worktree_root
            .join(relative)
            .canonicalize()
            .map_err(|error| format!("isolated execution root cannot be resolved: {error}"))?;
        if !target.starts_with(&worktree_root) {
            return Err("isolated execution root escaped its worktree".to_string());
        }
        Ok(Some(target))
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_approval(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
        requires_approval: bool,
        requires_worktree: bool,
        effective_root: &Path,
        effective_execution_config: &ExecutionConfig,
        session_id: Option<&str>,
    ) -> ApprovalDecision {
        let evaluations = self.matching_policy_rules(call, effective_root);

        if let Some(rule) = evaluations
            .iter()
            .find(|rule| rule.effect == PolicyEffect::Deny)
        {
            return ApprovalDecision::denied_by_policy(rule.reason.clone());
        }
        if let Some(decision) = self.approval_handler.approval_ceiling(call, level, risk) {
            return decision;
        }
        if let Some(rule) = evaluations
            .iter()
            .find(|rule| rule.effect == PolicyEffect::RequireApproval)
        {
            let context = self.approval_context(
                call,
                level,
                risk,
                effective_root,
                effective_execution_config,
                requires_worktree,
                session_id,
                &format!("project or tool policy requires approval: {}", rule.reason),
            );
            let mut decision = self
                .approval_handler
                .handle_approval_with_context(call, level, &context)
                .await;
            decision.note = Some(rule.reason.clone());
            return decision;
        }
        if let Some(rule) = evaluations
            .iter()
            .find(|rule| rule.effect == PolicyEffect::Allow)
        {
            return ApprovalDecision {
                granted: true,
                source: "policy".to_string(),
                note: Some(rule.reason.clone()),
            };
        }

        if matches!(level, PermissionLevel::ReadOnly | PermissionLevel::Plan)
            || risk == ToolRisk::ReadOnly
        {
            return ApprovalDecision::granted_policy();
        }
        if call.tool_name == "run_terminal"
            && risk == ToolRisk::Safe
            && self.classifier_auto_approval_allowed(call, level, risk)
        {
            return ApprovalDecision::granted_classifier();
        }
        if !requires_approval && matches!(risk, ToolRisk::Safe) {
            return ApprovalDecision::granted_policy();
        }
        if self.approval_mode == ApprovalMode::Policy {
            return ApprovalDecision::denied_by_policy("no matching allow policy");
        }
        if self.approval_mode == ApprovalMode::Off {
            return ApprovalDecision::granted_yolo();
        }
        if self.auto_approve {
            return ApprovalDecision::granted_user();
        }
        let context = self.approval_context(
            call,
            level,
            risk,
            effective_root,
            effective_execution_config,
            requires_worktree,
            session_id,
            "effective tool metadata and risk classification require interactive approval",
        );
        self.approval_handler
            .handle_approval_with_context(call, level, &context)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn approval_context(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
        effective_root: &Path,
        effective_execution_config: &ExecutionConfig,
        requires_worktree: bool,
        session_id: Option<&str>,
        reason: &str,
    ) -> ApprovalContext {
        let secrets = normalized_encoded_sensitive_values(self.redaction_secrets());
        let worktree = if requires_worktree && session_id.is_some() {
            "required; a session-owned managed worktree will be created or reused after approval"
        } else if requires_worktree {
            "required; execution will fail closed without an authoritative session"
        } else {
            "not required for this action"
        };
        ApprovalContext {
            action: normalized_approval_action(call, &secrets),
            permission_and_risk: bounded_approval_field(
                &format!("{} / {}", permission_label(level), risk.as_str()),
                &secrets,
            ),
            target_scope: bounded_approval_field(&effective_root.display().to_string(), &secrets),
            network: bounded_approval_field(
                &effective_execution_config.boundaries.network,
                &secrets,
            ),
            worktree: bounded_approval_field(worktree, &secrets),
            reason: bounded_approval_field(reason, &secrets),
            choices: "approve once or deny".to_string(),
        }
    }

    fn matching_policy_rules(&self, call: &ToolCall, effective_root: &Path) -> Vec<PolicyRule> {
        let mut evaluations: Vec<PolicyRule> = self
            .policy_rules
            .iter()
            .filter(|rule| rule.matches(call))
            .cloned()
            .collect();
        evaluations.extend(
            self.policy_hooks
                .iter()
                .filter_map(|hook| hook.evaluate(call, effective_root))
                .filter(|rule| rule.matches(call)),
        );
        evaluations
    }

    /// Resolve the configured sandbox into the least-privileged envelope needed
    /// for the registry permission and the argument-aware classifier result.
    fn effective_execution_config(
        &self,
        level: PermissionLevel,
        risk: ToolRisk,
    ) -> ExecutionConfig {
        let mut config = self.execution_config.clone();
        let elevated = matches!(
            level,
            PermissionLevel::Destructive | PermissionLevel::Network
        ) || matches!(
            risk,
            ToolRisk::RequiresApproval | ToolRisk::Destructive | ToolRisk::Network
        );
        if elevated {
            if config.provider == "internal" {
                config.provider = "hybrid".to_string();
            }
            if config.default_profile == "internal" {
                config.default_profile = "restricted".to_string();
            }
        }
        if elevated
            && !matches!(level, PermissionLevel::Network)
            && risk != ToolRisk::Network
            && config.boundaries.network == "enabled"
        {
            config.boundaries.network = "restricted".to_string();
        }
        config
    }

    fn classifier_auto_approval_allowed(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
    ) -> bool {
        self.classifier_auto_approval_allowed_with_bwrap(
            call,
            level,
            risk,
            crate::sandbox::detect_capabilities().bwrap_available,
        )
    }

    fn classifier_auto_approval_allowed_with_bwrap(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        risk: ToolRisk,
        bwrap_available: bool,
    ) -> bool {
        let Some(command) = call.arguments.get("command").and_then(Value::as_str) else {
            return false;
        };
        if !safe_command_requires_isolation(command) {
            return true;
        }
        let config = self.effective_execution_config(level, risk);
        !matches!(config.provider.as_str(), "internal")
            && config.default_profile != "internal"
            && bwrap_available
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_failure(
        &self,
        call: &ToolCall,
        session_id: Option<&str>,
        start: Instant,
        error: String,
        approval: ApprovalDecision,
        level: PermissionLevel,
        risk: ToolRisk,
        worktree: Option<&Path>,
        plan_id: Option<String>,
    ) -> ToolResult {
        let mut result = ToolResult {
            invocation_id: call.invocation_id,
            tool_name: call.tool_name.clone(),
            success: false,
            output: None,
            error: Some(self.redact_text(&error)),
            duration_seconds: start.elapsed().as_secs_f64(),
            approval_granted: approval.granted,
            approval_source: Some(approval.source.clone()),
        };
        self.sanitize_tool_result(&mut result);
        if let Err(record_error) = self.record(
            call, &result, &approval, session_id, worktree, level, risk, plan_id,
        ) {
            result.error = Some(format!(
                "{}; audit recording failed: {record_error}",
                result.error.as_deref().unwrap_or("tool execution failed")
            ));
        }
        self.sanitize_tool_result(&mut result);
        result
    }

    fn resolve_plan_id(&self, session_id: Option<&str>) -> Option<String> {
        session_id.and_then(|session_id| {
            self.session_store
                .as_ref()?
                .load_result(session_id)
                .ok()??
                .plan
                .filter(|plan| plan.has_identity())
                .map(|plan| plan.id)
        })
    }

    fn record_attempt(&self, call: &ToolCall, session_id: &str) -> Result<(), String> {
        let store = self
            .session_store
            .as_ref()
            .ok_or("session store unavailable")?;
        store
            .record_event(
                session_id,
                "tool_attempted",
                json!({
                    "invocation_id": call.invocation_id,
                    "tool_name": self.redact_text(&call.tool_name),
                    "arguments": self.redact_value(call.arguments.clone()),
                }),
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        call: &ToolCall,
        result: &ToolResult,
        approval: &ApprovalDecision,
        session_id: Option<&str>,
        worktree: Option<&Path>,
        level: PermissionLevel,
        risk: ToolRisk,
        plan_id: Option<String>,
    ) -> Result<(), String> {
        let Some(session_id) = session_id else {
            return Ok(());
        };
        let Some(store) = &self.session_store else {
            return Err("session store unavailable".to_string());
        };

        let mut provider = None;
        let mut sandbox_profile = None;
        let mut bwrap_args: Option<Vec<String>> = None;
        let mut boundaries = None;
        if call.tool_name == "run_terminal"
            || matches!(level, PermissionLevel::Network)
            || risk == ToolRisk::Network
        {
            let effective_config = self.effective_execution_config(level, risk);
            provider = Some(effective_config.provider);
            sandbox_profile = Some(effective_config.default_profile);
            boundaries = Some(effective_config.boundaries);
        }
        if let Some(object) = result.output.as_ref().and_then(Value::as_object) {
            provider = object
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(provider);
            sandbox_profile = object
                .get("sandbox_profile")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(sandbox_profile);
            bwrap_args = object
                .get("bwrap_args")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            boundaries = object
                .get("boundaries")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .or(boundaries);
        }

        provider = provider.map(|value| self.redact_text(&value));
        sandbox_profile = sandbox_profile.map(|value| self.redact_text(&value));
        bwrap_args = bwrap_args.map(|arguments| {
            arguments
                .into_iter()
                .map(|argument| self.redact_text(&argument))
                .collect()
        });
        boundaries = boundaries.map(|boundaries| crate::config::BoundaryConfig {
            allow_write: boundaries
                .allow_write
                .into_iter()
                .map(|path| self.redact_text(&path))
                .collect(),
            network: self.redact_text(&boundaries.network),
        });
        let plan_id = plan_id.map(|value| self.redact_text(&value));
        let worktree_path = worktree.map(|path| self.redact_text(&path.to_string_lossy()));
        let environment_keys = if call.tool_name == "run_terminal" {
            let mut keys: Vec<_> = self
                .environment
                .keys()
                .map(|key| self.redact_text(key))
                .collect();
            keys.sort();
            keys
        } else {
            Vec::new()
        };

        let record = ToolCallRecord {
            invocation_id: Some(call.invocation_id),
            id: Some(format!("tool-{}", Uuid::new_v4())),
            session_id: Some(session_id.to_string()),
            tool_name: Some(self.redact_text(&call.tool_name)),
            arguments: self.redact_value(call.arguments.clone()),
            result: Some(self.redact_value(json!({
                "success": result.success,
                "output": result.output.clone(),
                "error": result.error.clone(),
                "environment_keys": environment_keys,
                "approval": {
                    "granted": approval.granted,
                    "source": self.redact_text(&approval.source),
                    "note": approval.note.as_deref().map(|note| self.redact_text(note)),
                },
                "permission_level": permission_label(level),
                "risk": risk.as_str(),
            }))),
            error: result.error.as_deref().map(|error| self.redact_text(error)),
            duration_seconds: Some(result.duration_seconds),
            worktree_path,
            timestamp: Some(Utc::now()),
            provider,
            sandbox_profile,
            bwrap_args,
            boundaries,
            plan_id,
        };
        store
            .record_tool_call(record)
            .map_err(|error| error.to_string())
    }
}

fn schema_validation_arguments(call: &ToolCall) -> Value {
    let mut arguments = call.arguments.clone();
    let Some(object) = arguments.as_object_mut() else {
        return arguments;
    };
    match call.tool_name.as_str() {
        "spawn_subagent" | "invoke_subagent" => {
            object.remove("_parent_session_id");
            object.remove("_audit_sessions_dir");
        }
        "schedule" | "run_terminal" => {
            object.remove("_session_id");
            object.remove("_sessions_dir");
            object.remove("hook_source");
            object.remove("hook_for");
        }
        "ask_question" => {
            object.remove("answer");
            object.remove("answer_error");
        }
        _ => {}
    }
    arguments
}

fn validate_tool_arguments(
    tool_name: &str,
    schema: &Value,
    arguments: &Value,
) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| format!("invalid input schema for tool '{tool_name}'"))?;
    let errors: Vec<_> = validator
        .iter_errors(arguments)
        .take(5)
        .map(|error| {
            let path = error.instance_path.to_string();
            let constraint = error.schema_path.to_string();
            let location = if path.is_empty() {
                "at the argument root".to_string()
            } else {
                format!("at {path}")
            };
            format!("{location}: failed schema constraint {constraint}")
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        const MAX_VALIDATION_ERROR_BYTES: usize = 8 * 1024;
        let mut error = format!(
            "invalid arguments for tool '{tool_name}': {}",
            errors.join("; ")
        );
        if error.len() > MAX_VALIDATION_ERROR_BYTES {
            let mut end = MAX_VALIDATION_ERROR_BYTES.saturating_sub(3);
            while end > 0 && !error.is_char_boundary(end) {
                end -= 1;
            }
            error.truncate(end);
            error.push_str("...");
        }
        Err(error)
    }
}

pub(crate) fn validate_registered_tool_arguments(
    tool_name: &str,
    arguments: &Value,
) -> Result<(), String> {
    let metadata = crate::tools::registry::get_tool_metadata(tool_name)
        .ok_or_else(|| format!("tool is not registered: {tool_name}"))?;
    validate_tool_arguments(tool_name, &metadata.input_schema, arguments)
}

pub fn tools_json_schema() -> Vec<Value> {
    crate::tools::registry::list_tools()
        .into_iter()
        .filter(|metadata| metadata.mcp_exposable)
        .map(|metadata| {
            json!({
                "type": "function",
                "function": {
                    "name": metadata.name,
                    "description": metadata.description,
                    "parameters": metadata.input_schema,
                }
            })
        })
        .collect()
}

fn permission_label(level: PermissionLevel) -> &'static str {
    match level {
        PermissionLevel::ReadOnly => "read_only",
        PermissionLevel::Plan => "plan",
        PermissionLevel::Safe => "safe",
        PermissionLevel::Destructive => "destructive",
        PermissionLevel::Network => "network",
    }
}

fn bounded_approval_field(value: &str, secrets: &[String]) -> String {
    bounded_approval_field_with_limit(value, secrets, MAX_APPROVAL_FIELD_BYTES)
}

fn bounded_approval_field_with_limit(value: &str, secrets: &[String], limit: usize) -> String {
    let redacted = redact_text_with_encoded_secrets(value, secrets);
    let mut normalized = String::new();
    let mut truncated = false;
    for character in redacted.chars() {
        let escaped = match character {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            character if character.is_control() => format!("\\u{{{:x}}}", character as u32),
            character => character.to_string(),
        };
        if normalized.len().saturating_add(escaped.len()) > limit {
            truncated = true;
            break;
        }
        normalized.push_str(&escaped);
    }
    if truncated {
        let maximum = limit.saturating_sub(3);
        while normalized.len() > maximum {
            normalized.pop();
        }
        normalized.push_str("...");
    }
    if normalized.is_empty() {
        "(none)".to_string()
    } else {
        normalized
    }
}

fn approval_argument_string<'a>(call: &'a ToolCall, field: &str) -> Option<&'a str> {
    call.arguments.get(field).and_then(Value::as_str)
}

fn normalized_approval_action(call: &ToolCall, secrets: &[String]) -> String {
    let summary = match call.tool_name.as_str() {
        "approve_plan" => {
            let plan_id = bounded_approval_field_with_limit(
                approval_argument_string(call, "plan_id").unwrap_or("(missing)"),
                secrets,
                64,
            );
            let goal = bounded_approval_field_with_limit(
                approval_argument_string(call, "goal").unwrap_or("(missing)"),
                secrets,
                80,
            );
            let step_count = call
                .arguments
                .get("steps")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            format!("approve_plan plan_id={plan_id} steps={step_count} goal={goal}")
        }
        "run_terminal" => {
            let command = bounded_approval_field_with_limit(
                approval_argument_string(call, "command").unwrap_or("(missing command)"),
                secrets,
                120,
            );
            let cwd = bounded_approval_field_with_limit(
                approval_argument_string(call, "cwd").unwrap_or("."),
                secrets,
                48,
            );
            let background = call
                .arguments
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            format!("run_terminal command={command} cwd={cwd} background={background}")
        }
        "apply_patch" => {
            let patch = approval_argument_string(call, "patch").unwrap_or_default();
            let mut targets = Vec::new();
            let mut target_count = 0usize;
            let mut in_hunk = false;
            let mut pending_old_header = false;
            for line in patch.lines() {
                if line.starts_with("diff --git ") {
                    in_hunk = false;
                    pending_old_header = false;
                    continue;
                }
                if line.starts_with("@@ ") {
                    in_hunk = true;
                    pending_old_header = false;
                    continue;
                }
                let custom_path = line
                    .strip_prefix("*** Update File: ")
                    .or_else(|| line.strip_prefix("*** Add File: "))
                    .or_else(|| line.strip_prefix("*** Delete File: "));
                let unified_path = (!in_hunk && pending_old_header)
                    .then(|| line.strip_prefix("+++ "))
                    .flatten();
                if !in_hunk && line.starts_with("--- ") {
                    pending_old_header = true;
                    continue;
                }
                pending_old_header = false;
                let Some(path) = custom_path.or(unified_path) else {
                    continue;
                };
                let path = path.trim().trim_start_matches("b/");
                if path == "/dev/null" {
                    continue;
                }
                target_count = target_count.saturating_add(1);
                if targets.len() < 4 {
                    targets.push(bounded_approval_field_with_limit(path, secrets, 40));
                }
            }
            targets.sort();
            targets.dedup();
            let mode = if call
                .arguments
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                "check"
            } else {
                "apply"
            };
            format!(
                "apply_patch mode={mode} files={target_count} targets={}",
                if targets.is_empty() {
                    "(unparsed)".to_string()
                } else {
                    targets.join(",")
                }
            )
        }
        "merge_subagent_worktree" => format!(
            "merge_subagent_worktree subagent_id={}",
            bounded_approval_field_with_limit(
                approval_argument_string(call, "subagent_id").unwrap_or("(missing)"),
                secrets,
                96,
            )
        ),
        "manage_subagents" | "manage_task" | "manage_memory" => {
            let action = bounded_approval_field_with_limit(
                approval_argument_string(call, "action").unwrap_or("(missing)"),
                secrets,
                48,
            );
            let target = ["subagent_id", "task_id", "namespace", "key"]
                .into_iter()
                .filter_map(|field| {
                    approval_argument_string(call, field).map(|value| {
                        format!(
                            " {field}={}",
                            bounded_approval_field_with_limit(value, secrets, 64)
                        )
                    })
                })
                .collect::<String>();
            format!("{} action={action}{target}", call.tool_name)
        }
        _ => bounded_approval_field(&call.tool_name, secrets),
    };
    bounded_approval_field(&summary, secrets)
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn approval_mode_from_config(config: &ApprovalsConfig) -> ApprovalMode {
    match config.mode.to_ascii_lowercase().as_str() {
        "manual" => ApprovalMode::Manual,
        "smart" => ApprovalMode::Smart,
        "policy" => ApprovalMode::Policy,
        "off" => ApprovalMode::Off,
        _ => ApprovalMode::Manual,
    }
}

fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Manual => "manual",
        ApprovalMode::Smart => "smart",
        ApprovalMode::Policy => "policy",
        ApprovalMode::Off => "off",
    }
}

fn resolve_execution_config(
    project_root: &Path,
    mut execution_config: ExecutionConfig,
) -> ResolvedExecutionConfig {
    let configured = execution_config.clone();
    let mut policy_rules = load_instruction_policy_rules(project_root);
    let instruction_posture =
        match apply_instruction_execution_tightening(project_root, &mut execution_config) {
            Ok(()) if execution_config == configured => InstructionExecutionPosture::Configured,
            Ok(()) => InstructionExecutionPosture::Tightened,
            Err(error) => {
                fail_closed_execution_config(&mut execution_config);
                policy_rules.push(PolicyRule {
                    effect: PolicyEffect::Deny,
                    tool_name: "*".to_string(),
                    argument_contains: None,
                    reason: format!("invalid instruction execution directive: {error}"),
                });
                InstructionExecutionPosture::InvalidFailClosed
            }
        };
    ResolvedExecutionConfig {
        config: execution_config,
        policy_rules,
        instruction_posture,
    }
}

fn load_instruction_policy_rules(project_root: &Path) -> Vec<PolicyRule> {
    let files = instruction_policy_files(project_root);
    let mut rules = Vec::new();
    for file in files {
        let contents = match read_instruction_policy_file(&file) {
            Ok(contents) => contents,
            Err(error) => {
                rules.push(PolicyRule {
                    effect: PolicyEffect::Deny,
                    tool_name: "*".to_string(),
                    argument_contains: None,
                    reason: error,
                });
                continue;
            }
        };
        for (index, line) in contents.lines().enumerate() {
            let trimmed = line.trim().trim_start_matches(['-', '*']).trim();
            let Some(directive) = trimmed.strip_prefix("nib-policy:") else {
                continue;
            };
            let mut parts = directive.trim().splitn(3, char::is_whitespace);
            let effect = match parts.next().unwrap_or_default() {
                "allow" => PolicyEffect::Allow,
                "require-approval" => PolicyEffect::RequireApproval,
                "deny" => PolicyEffect::Deny,
                _ => continue,
            };
            let tool_name = parts.next().unwrap_or("*").to_string();
            let argument_contains = parts
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            rules.push(PolicyRule {
                effect,
                tool_name,
                argument_contains,
                reason: format!("{}:{}", file.display(), index + 1),
            });
        }
    }
    rules
}

fn read_instruction_policy_file(path: &Path) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to inspect instruction file {}: {error}",
            path.display()
        )
    })?;
    validate_instruction_policy_metadata(path, &metadata)?;
    let file = std::fs::File::open(path).map_err(|error| {
        format!(
            "failed to read instruction file {}: {error}",
            path.display()
        )
    })?;
    validate_instruction_policy_metadata(
        path,
        &file.metadata().map_err(|error| error.to_string())?,
    )?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_INSTRUCTION_POLICY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "failed to read instruction file {}: {error}",
                path.display()
            )
        })?;
    if bytes.len() as u64 > MAX_INSTRUCTION_POLICY_BYTES {
        return Err(format!(
            "instruction file {} exceeds the {MAX_INSTRUCTION_POLICY_BYTES}-byte policy limit",
            path.display()
        ));
    }
    validate_instruction_policy_metadata(
        path,
        &std::fs::symlink_metadata(path).map_err(|error| error.to_string())?,
    )?;
    String::from_utf8(bytes).map_err(|error| {
        format!(
            "failed to read instruction file {}: {error}",
            path.display()
        )
    })
}

fn validate_instruction_policy_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), String> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "instruction file must be a regular local file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_INSTRUCTION_POLICY_BYTES {
        return Err(format!(
            "instruction file {} exceeds the {MAX_INSTRUCTION_POLICY_BYTES}-byte policy limit",
            path.display()
        ));
    }
    Ok(())
}

fn instruction_policy_files(project_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for name in [
        "AGENTS.md",
        "AGENTS.local.md",
        "CLAUDE.md",
        "CLAUDE.local.md",
    ] {
        let path = project_root.join(name);
        if path.is_file() {
            files.push(path);
        }
    }
    let skills_root = project_root.join(".nib").join("skills");
    if let Ok(entries) = std::fs::read_dir(skills_root) {
        files.extend(
            entries
                .flatten()
                .take(256)
                .map(|entry| entry.path().join("SKILL.md"))
                .filter(|path| {
                    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
                }),
        );
    }
    files
}

fn fail_closed_execution_config(config: &mut ExecutionConfig) {
    config.provider = "bwrap".to_string();
    config.default_profile = "restricted".to_string();
    config.boundaries.network = "disabled".to_string();
    config.boundaries.allow_write.clear();
}

fn valid_boundary_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !matches!(name, "internal" | "restricted")
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn apply_named_boundary_profile(
    config: &mut ExecutionConfig,
    configured_boundaries: &crate::config::BoundaryConfig,
    name: &str,
) -> Result<(), String> {
    if !valid_boundary_profile_name(name) {
        return Err(format!(
            "invalid or reserved boundary profile name '{name}'"
        ));
    }
    let profile = config
        .boundary_profiles
        .get(name)
        .cloned()
        .ok_or_else(|| format!("unknown boundary profile '{name}'"))?;
    boundary_profile_tightening_error(configured_boundaries, &profile).map_err(|reason| {
        format!("boundary profile '{name}' would weaken configured boundaries: {reason}")
    })?;

    config.default_profile = name.to_string();
    config.boundaries = profile;
    if config.boundaries.network == "disabled" {
        config.provider = "bwrap".to_string();
    } else if config.provider == "internal" {
        config.provider = "hybrid".to_string();
    }
    Ok(())
}

fn apply_instruction_execution_tightening(
    project_root: &Path,
    config: &mut ExecutionConfig,
) -> Result<(), String> {
    let configured_boundaries = config.boundaries.clone();
    let mut selected_profile: Option<String> = None;
    let mut require_bwrap = false;
    let mut disable_network = false;

    for file in instruction_policy_files(project_root) {
        let contents = match read_instruction_policy_file(&file) {
            Ok(contents) => contents,
            Err(error) => {
                fail_closed_execution_config(config);
                return Err(error);
            }
        };
        for line in contents.lines() {
            let trimmed = line.trim().trim_start_matches(['-', '*']).trim();
            if trimmed == "nib-sandbox: require-bwrap" {
                require_bwrap = true;
            } else if trimmed == "nib-boundary: disable-network" {
                require_bwrap = true;
                disable_network = true;
            } else if let Some(directive) = trimmed.strip_prefix("nib-boundary:") {
                let mut parts = directive.split_whitespace();
                if parts.next() != Some("profile") {
                    continue;
                }
                let name = parts.next().ok_or_else(|| {
                    format!(
                        "{} contains a boundary profile directive without a name",
                        file.display()
                    )
                })?;
                if parts.next().is_some() {
                    return Err(format!(
                        "{} contains an invalid boundary profile directive",
                        file.display()
                    ));
                }
                if selected_profile
                    .as_deref()
                    .is_some_and(|selected| selected != name)
                {
                    return Err(format!(
                        "conflicting boundary profiles '{}' and '{name}' were selected",
                        selected_profile.as_deref().unwrap_or_default()
                    ));
                }
                selected_profile = Some(name.to_string());
            }
        }
    }

    if let Some(name) = selected_profile {
        apply_named_boundary_profile(config, &configured_boundaries, &name)?;
    }
    if require_bwrap {
        config.provider = "bwrap".to_string();
        if config.default_profile == "internal" {
            config.default_profile = "restricted".to_string();
        }
    }
    if disable_network {
        config.provider = "bwrap".to_string();
        config.boundaries.network = "disabled".to_string();
    }
    Ok(())
}

pub(crate) fn redact_value(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            let mut redacted = Map::new();
            for (key, value) in std::mem::take(object) {
                let key_lower = key.to_ascii_lowercase();
                if ["api_key", "apikey", "token", "password", "secret"]
                    .iter()
                    .any(|sensitive| key_lower.contains(sensitive))
                {
                    redacted.insert(key, Value::String("[REDACTED]".to_string()));
                } else {
                    redacted.insert(key, redact_value(value));
                }
            }
            *object = redacted;
        }
        Value::Array(values) => {
            for value in values {
                *value = redact_value(std::mem::take(value));
            }
        }
        Value::String(text) => *text = redact_text(text),
        _ => {}
    }
    value
}

pub(crate) fn redact_value_with_environment(
    value: Value,
    environment: &HashMap<String, String>,
) -> Value {
    redact_value_with_secrets(value, &sensitive_environment_values(environment))
}

pub(crate) fn redact_value_with_encoded_sensitive_values(
    value: Value,
    values: impl IntoIterator<Item = String>,
) -> Value {
    let secrets = normalized_encoded_sensitive_values(values);
    redact_value_with_encoded_secrets(value, &secrets)
}

fn control_safe_public_value(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            for value in object.values_mut() {
                *value = control_safe_public_value(std::mem::take(value));
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = control_safe_public_value(std::mem::take(value));
            }
        }
        Value::String(text) => {
            *text = crate::interactive::control_safe_text(text, true);
        }
        _ => {}
    }
    value
}

fn redact_value_with_encoded_secrets(mut value: Value, secrets: &[String]) -> Value {
    value = redact_value(value);
    match &mut value {
        Value::Object(object) => {
            for value in object.values_mut() {
                *value = redact_value_with_encoded_secrets(std::mem::take(value), secrets);
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = redact_value_with_encoded_secrets(std::mem::take(value), secrets);
            }
        }
        Value::String(text) => *text = redact_text_with_encoded_secrets(text, secrets),
        _ => {}
    }
    value
}

fn redact_value_with_secrets(mut value: Value, secrets: &[String]) -> Value {
    value = redact_value(value);
    match &mut value {
        Value::Object(object) => {
            for value in object.values_mut() {
                *value = redact_value_with_secrets(std::mem::take(value), secrets);
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = redact_value_with_secrets(std::mem::take(value), secrets);
            }
        }
        Value::String(text) => *text = redact_text_with_secrets(text, secrets),
        _ => {}
    }
    value
}

pub(crate) fn redact_text(text: &str) -> String {
    GENERIC_SECRET_PATTERN
        .replace_all(text, "[REDACTED]")
        .into_owned()
}

pub(crate) fn contains_generic_secret(text: &str) -> bool {
    GENERIC_SECRET_PATTERN.is_match(text)
}

pub(crate) fn redact_text_with_environment(
    text: &str,
    environment: &HashMap<String, String>,
) -> String {
    redact_text_with_secrets(text, &sensitive_environment_values(environment))
}

pub(crate) fn redact_text_with_encoded_sensitive_values(
    text: &str,
    values: impl IntoIterator<Item = String>,
) -> String {
    let secrets = normalized_encoded_sensitive_values(values);
    redact_text_with_encoded_secrets(text, &secrets)
}

fn redact_text_with_secrets(text: &str, secrets: &[String]) -> String {
    let mut redacted = redact_text(text);
    for secret in secrets {
        redacted = redacted.replace(secret, "[REDACTED]");
    }
    redacted
}

fn redact_text_with_encoded_secrets(text: &str, secrets: &[String]) -> String {
    const MAX_ENCODED_REDACTION_INPUT_BYTES: usize = 64 * 1024;

    if text.len() > MAX_ENCODED_REDACTION_INPUT_BYTES {
        return "[REDACTED]".to_string();
    }

    let Some(text_stages) = percent_decoded_byte_stages(text) else {
        return "[REDACTED]".to_string();
    };
    let mut secret_stages = Vec::new();
    for secret in secrets {
        let Some(stages) = percent_decoded_byte_stages(secret) else {
            return "[REDACTED]".to_string();
        };
        secret_stages.extend(
            stages
                .into_iter()
                .map(|stage| stage.bytes)
                .filter(|secret| !secret.is_empty()),
        );
    }
    let mut spans = Vec::new();
    for text_stage in &text_stages {
        for secret in &secret_stages {
            if secret.len() > text_stage.bytes.len() {
                continue;
            }
            for offset in 0..=text_stage.bytes.len() - secret.len() {
                if text_stage.bytes[offset..offset + secret.len()] == *secret.as_slice() {
                    let mut start = text_stage.origins[offset].0;
                    let mut end = text_stage.origins[offset + secret.len() - 1].1;
                    while !text.is_char_boundary(start) {
                        start = start.saturating_sub(1);
                    }
                    while end < text.len() && !text.is_char_boundary(end) {
                        end += 1;
                    }
                    spans.push((start, end));
                }
            }
        }
    }

    if spans.is_empty() {
        return redact_text_with_secrets(text, secrets);
    }
    spans.sort_unstable();
    let mut merged = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some((_, previous_end)) if start <= *previous_end => {
                *previous_end = (*previous_end).max(end);
            }
            _ => merged.push((start, end)),
        }
    }

    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in merged {
        redacted.push_str(&text[cursor..start]);
        redacted.push_str("[REDACTED]");
        cursor = end;
    }
    redacted.push_str(&text[cursor..]);
    redact_text(&redacted)
}

#[derive(Clone)]
struct PercentDecodedBytes {
    bytes: Vec<u8>,
    origins: Vec<(usize, usize)>,
}

fn percent_decoded_byte_stages(value: &str) -> Option<Vec<PercentDecodedBytes>> {
    let mut stages = vec![PercentDecodedBytes {
        bytes: value.as_bytes().to_vec(),
        origins: (0..value.len()).map(|index| (index, index + 1)).collect(),
    }];
    let mut passes = 0;
    loop {
        let current = stages.last().expect("percent-decoding stage");
        let mut next = PercentDecodedBytes {
            bytes: Vec::with_capacity(current.bytes.len()),
            origins: Vec::with_capacity(current.origins.len()),
        };
        let mut index = 0;
        let mut decoded_escape = false;
        while index < current.bytes.len() {
            if current.bytes[index] == b'%' && index + 2 < current.bytes.len() {
                if let (Some(high), Some(low)) = (
                    percent_hex_value(current.bytes[index + 1]),
                    percent_hex_value(current.bytes[index + 2]),
                ) {
                    next.bytes.push((high << 4) | low);
                    next.origins
                        .push((current.origins[index].0, current.origins[index + 2].1));
                    index += 3;
                    decoded_escape = true;
                    continue;
                }
            }
            next.bytes.push(current.bytes[index]);
            next.origins.push(current.origins[index]);
            index += 1;
        }
        if !decoded_escape {
            return Some(stages);
        }
        if passes == MAX_PERCENT_DECODE_PASSES {
            return None;
        }
        stages.push(next);
        passes += 1;
    }
}

fn percent_hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn normalized_encoded_sensitive_values(
    values: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut secrets = values.into_iter().collect::<Vec<_>>();
    secrets.extend(
        secrets
            .iter()
            .map(|secret| secret.trim().to_string())
            .collect::<Vec<_>>(),
    );
    let json_variants = secrets
        .iter()
        .filter_map(|secret| serde_json::to_string(secret).ok())
        .filter_map(|quoted| {
            quoted
                .get(1..quoted.len().saturating_sub(1))
                .map(str::to_string)
        })
        .flat_map(|escaped| [escaped.clone(), escaped.replace('/', "\\/")])
        .collect::<Vec<_>>();
    secrets.extend(json_variants);
    let base64_variants = secrets
        .iter()
        .flat_map(|secret| {
            let standard = base64_secret_variant(secret.as_bytes(), false);
            let url_safe = base64_secret_variant(secret.as_bytes(), true);
            [
                standard.clone(),
                standard.trim_end_matches('=').to_string(),
                url_safe.clone(),
                url_safe.trim_end_matches('=').to_string(),
            ]
        })
        .collect::<Vec<_>>();
    secrets.extend(base64_variants);
    normalize_sensitive_values(&mut secrets);
    secrets
}

fn base64_secret_variant(bytes: &[u8], url_safe: bool) -> String {
    let alphabet = if url_safe {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(alphabet[usize::from(first >> 2)] as char);
        encoded.push(alphabet[usize::from(((first & 0x03) << 4) | (second >> 4))] as char);
        if chunk.len() > 1 {
            encoded.push(alphabet[usize::from(((second & 0x0f) << 2) | (third >> 6))] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(alphabet[usize::from(third & 0x3f)] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn sensitive_environment_values(environment: &HashMap<String, String>) -> Vec<String> {
    let mut secrets: Vec<_> = environment
        .iter()
        .filter(|(key, value)| is_sensitive_environment_key(key) && !value.is_empty())
        .map(|(_, value)| value.clone())
        .collect();
    normalize_sensitive_values(&mut secrets);
    secrets
}

fn normalize_sensitive_values(secrets: &mut Vec<String>) {
    secrets.retain(|value| !value.is_empty());
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
}

fn is_sensitive_environment_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [
        "API_KEY",
        "APIKEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct ContextCapturingApprovalHandler {
        context: Mutex<Option<ApprovalContext>>,
    }

    #[async_trait::async_trait]
    impl ApprovalHandler for ContextCapturingApprovalHandler {
        async fn handle_approval(
            &self,
            _call: &ToolCall,
            _level: PermissionLevel,
        ) -> ApprovalDecision {
            ApprovalDecision::denied()
        }

        async fn handle_approval_with_context(
            &self,
            _call: &ToolCall,
            _level: PermissionLevel,
            context: &ApprovalContext,
        ) -> ApprovalDecision {
            *self.context.lock().expect("context lock") = Some(context.clone());
            ApprovalDecision::granted_user()
        }
    }

    #[tokio::test]
    async fn approval_context_is_redacted_bounded_and_preserves_decision_behavior() {
        let root = tempfile::tempdir().expect("root");
        let capture = Arc::new(ContextCapturingApprovalHandler::default());
        let base64_secret = base64_secret_variant(b"provider-private-sentinel", false);
        let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_approval_handler(capture.clone())
            .with_sensitive_values(["provider-private-sentinel".to_string()]);
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({
                "command": format!("{base64_secret} provider%2Dprivate%2Dsentinel {}LONG_ARGUMENT_SENTINEL", "x".repeat(5_000)),
                "token": "sk-privateapproval123456",
                "control": "\u{1b}[2J\nraw-json-sentinel",
            }),
            session_id: Some("approval-session".to_string()),
            project_root: Some(root.path().to_path_buf()),
        };
        let effective = executor
            .effective_execution_config(PermissionLevel::Destructive, ToolRisk::Destructive);
        let target = PathBuf::from(format!(
            "/tmp/provider-private-sentinel/\u{1b}[2J\n{}LONG_TARGET_SENTINEL",
            "x".repeat(500)
        ));
        let decision = executor
            .handle_approval(
                &call,
                PermissionLevel::Destructive,
                ToolRisk::Destructive,
                true,
                true,
                &target,
                &effective,
                Some("approval-session"),
            )
            .await;
        assert!(decision.granted);
        assert_eq!(decision.source, "user");
        let context = capture
            .context
            .lock()
            .expect("context lock")
            .clone()
            .expect("context captured");
        let rendered = context.render();
        assert_eq!(context.lines().len(), MAX_APPROVAL_LINES);
        assert!(context
            .lines()
            .iter()
            .all(|line| line.len() <= MAX_APPROVAL_LINE_BYTES));
        assert!(rendered.len() < 2_048);
        assert!(rendered.contains("Action: run_terminal command="));
        assert!(rendered.contains("destructive / destructive"));
        assert!(rendered.contains("Network: restricted"));
        assert!(rendered.contains("session-owned managed worktree will be created or reused"));
        assert!(!rendered.contains("provider-private-sentinel"));
        assert!(!rendered.contains("provider%2Dprivate"));
        assert!(!rendered.contains(&base64_secret));
        assert!(!rendered.contains("sk-privateapproval"));
        assert!(!rendered.contains("raw-json-sentinel"));
        assert!(!rendered.contains("LONG_ARGUMENT_SENTINEL"));
        assert!(!rendered.contains("LONG_TARGET_SENTINEL"));
        assert!(context
            .lines()
            .iter()
            .all(|line| !line.chars().any(char::is_control)));
    }

    #[tokio::test]
    async fn contextual_method_defaults_to_legacy_handler_for_compatibility() {
        struct LegacyGrant;
        #[async_trait::async_trait]
        impl ApprovalHandler for LegacyGrant {
            async fn handle_approval(
                &self,
                _call: &ToolCall,
                _level: PermissionLevel,
            ) -> ApprovalDecision {
                ApprovalDecision::granted_user()
            }
        }
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "apply_patch".to_string(),
            arguments: json!({"private": "not presented by compatibility context"}),
            session_id: None,
            project_root: None,
        };
        let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);
        let decision = LegacyGrant
            .handle_approval_with_context(&call, PermissionLevel::Destructive, &context)
            .await;
        assert!(decision.granted);
        assert_eq!(decision.source, "user");
    }

    #[test]
    fn patch_approval_action_names_mode_and_targets_without_patch_body() {
        let root = tempfile::tempdir().expect("root");
        let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_sensitive_values(["configured-patch-secret".to_string()]);
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "apply_patch".to_string(),
            arguments: json!({
                "dry_run": false,
                "patch": "diff --git a/src/old.rs b/src/new.rs\n--- a/src/old.rs\n+++ b/src/new.rs\n@@ -1 +1 @@\n-configured-patch-secret\n+sk-privatepatch123456\ndiff --git a/docs/a.md b/docs/a.md\n--- a/docs/a.md\n+++ b/docs/a.md\n"
            }),
            session_id: Some("patch-session".to_string()),
            project_root: Some(root.path().to_path_buf()),
        };
        let context = executor.approval_context(
            &call,
            PermissionLevel::Safe,
            ToolRisk::Destructive,
            root.path(),
            &executor.execution_config,
            true,
            Some("patch-session"),
            "patch requires approval",
        );
        assert!(context.action.contains("apply_patch mode=apply"));
        assert!(context.action.contains("files=2"));
        assert!(context.action.contains("src/new.rs"));
        assert!(context.action.contains("docs/a.md"));
        assert!(!context.render().contains("configured-patch-secret"));
        assert!(!context.render().contains("sk-privatepatch"));
    }

    #[test]
    fn plan_approval_action_identifies_plan_without_leaking_or_overrunning_goal() {
        let root = tempfile::tempdir().expect("root");
        let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_sensitive_values(["private-plan-secret".to_string()]);
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "approve_plan".to_string(),
            arguments: json!({
                "plan_id": "plan-123",
                "goal": format!("inspect private-plan-secret \u{1b}[2J {}PLAN_GOAL_SENTINEL", "x".repeat(500)),
                "steps": ["inspect", "change", "verify"],
            }),
            session_id: Some("plan-session".to_string()),
            project_root: Some(root.path().to_path_buf()),
        };
        let context = executor.approval_context(
            &call,
            PermissionLevel::Plan,
            ToolRisk::RequiresApproval,
            root.path(),
            &executor.execution_config,
            false,
            Some("plan-session"),
            "approve the persisted plan",
        );
        assert!(context.action.contains("approve_plan plan_id=plan-123"));
        assert!(context.action.contains("steps=3"));
        assert!(context.action.contains("goal=inspect [REDACTED]"));
        assert!(!context.action.contains("private-plan-secret"));
        assert!(!context.action.contains("PLAN_GOAL_SENTINEL"));
        assert!(!context.action.chars().any(char::is_control));
    }

    #[test]
    fn prepared_guard_surfaces_and_audits_durable_compensation_failure() {
        let directory = tempfile::tempdir().expect("tempdir");
        let sessions_dir = directory.path().join("state/sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions");
        let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
        session_store.create_session_with_id("origin");
        let store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
            directory.path().join("state/daemons"),
        )
        .expect("durable store");
        let id = format!("guard-compensation-{}", uuid::Uuid::new_v4().simple());
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
        let mut guard = PreparedTaskGuard {
            task_id: Some(id.clone()),
        };

        let error = guard
            .fail("executor audit failed")
            .expect_err("guard must surface durable compensation failure");
        assert!(error.contains("daemon audit"), "{error}");
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

    #[test]
    fn parses_explicit_agents_policy_rules() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            directory.path().join("AGENTS.md"),
            "- nib-policy: deny run_terminal rm -rf\n",
        )
        .expect("write");
        let rules = load_instruction_policy_rules(directory.path());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].effect, PolicyEffect::Deny);
        assert_eq!(rules[0].argument_contains.as_deref(), Some("rm -rf"));
    }

    #[test]
    fn oversized_instruction_policy_fails_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("AGENTS.md");
        let file = std::fs::File::create(&path).expect("instruction fixture");
        file.set_len(MAX_INSTRUCTION_POLICY_BYTES + 1)
            .expect("oversized fixture");

        let rules = load_instruction_policy_rules(directory.path());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].effect, PolicyEffect::Deny);
        assert_eq!(rules[0].tool_name, "*");
        assert!(rules[0].reason.contains("exceeds"));

        let mut config = ExecutionConfig::default();
        let error = apply_instruction_execution_tightening(directory.path(), &mut config)
            .expect_err("oversized instruction files fail closed");
        assert!(error.contains("exceeds"));
        assert_eq!(config.provider, "bwrap");
        assert_eq!(config.boundaries.network, "disabled");
    }

    #[test]
    fn agents_directives_can_only_tighten_sandbox_boundaries() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            directory.path().join("AGENTS.md"),
            "- nib-boundary: disable-network\n",
        )
        .expect("write");
        let executor = ToolExecutor::new(
            directory.path().to_path_buf(),
            ExecutionConfig {
                provider: "internal".to_string(),
                default_profile: "internal".to_string(),
                boundaries: crate::config::BoundaryConfig {
                    network: "enabled".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        assert_eq!(executor.execution_config.provider, "bwrap");
        assert_eq!(executor.execution_config.default_profile, "restricted");
        assert_eq!(executor.execution_config.boundaries.network, "disabled");
    }

    #[test]
    fn agents_can_select_a_configured_tightening_boundary_profile() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            directory.path().join("AGENTS.md"),
            "- nib-boundary: profile offline-build\n",
        )
        .expect("write");
        let mut config = ExecutionConfig {
            provider: "internal".to_string(),
            default_profile: "internal".to_string(),
            boundaries: crate::config::BoundaryConfig {
                allow_write: vec!["build".to_string(), "cache".to_string()],
                network: "enabled".to_string(),
            },
            ..ExecutionConfig::default()
        };
        config.boundary_profiles.insert(
            "offline-build".to_string(),
            crate::config::BoundaryConfig {
                allow_write: vec!["build".to_string()],
                network: "restricted".to_string(),
            },
        );

        apply_instruction_execution_tightening(directory.path(), &mut config)
            .expect("profile tightens the configured boundary");

        assert_eq!(config.provider, "hybrid");
        assert_eq!(config.default_profile, "offline-build");
        assert_eq!(config.boundaries.network, "restricted");
        assert_eq!(config.boundaries.allow_write, vec!["build"]);
    }

    #[test]
    fn malformed_or_conflicting_agents_profile_directives_fail_closed() {
        for contents in [
            "- nib-boundary: profile\n",
            "- nib-boundary: profile first\n- nib-boundary: profile second\n",
        ] {
            let directory = tempfile::tempdir().expect("tempdir");
            std::fs::write(directory.path().join("AGENTS.md"), contents).expect("write");
            let mut config = ExecutionConfig::default();
            let error = apply_instruction_execution_tightening(directory.path(), &mut config)
                .expect_err("invalid profile directive must fail");
            assert!(
                error.contains("without a name") || error.contains("conflicting boundary profiles"),
                "{error}"
            );
        }
    }

    #[test]
    fn elevated_permissions_tighten_an_internal_execution_envelope() {
        let directory = tempfile::tempdir().expect("tempdir");
        let executor = ToolExecutor::new(
            directory.path().to_path_buf(),
            ExecutionConfig {
                provider: "internal".to_string(),
                default_profile: "internal".to_string(),
                boundaries: crate::config::BoundaryConfig {
                    network: "enabled".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        let effective = executor
            .effective_execution_config(PermissionLevel::Destructive, ToolRisk::Destructive);
        assert_eq!(effective.provider, "hybrid");
        assert_eq!(effective.default_profile, "restricted");
        assert_eq!(effective.boundaries.network, "restricted");
    }

    #[test]
    fn classifier_requires_available_isolation_for_cargo_and_git_commands() {
        let root = tempfile::tempdir().expect("root");
        let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());
        let terminal_call = |command: &str| ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": command}),
            session_id: None,
            project_root: Some(root.path().to_path_buf()),
        };

        for command in ["cargo check", "git status --short"] {
            assert!(!executor.classifier_auto_approval_allowed_with_bwrap(
                &terminal_call(command),
                PermissionLevel::Destructive,
                ToolRisk::Safe,
                false,
            ));
            assert!(executor.classifier_auto_approval_allowed_with_bwrap(
                &terminal_call(command),
                PermissionLevel::Destructive,
                ToolRisk::Safe,
                true,
            ));
        }
        assert!(executor.classifier_auto_approval_allowed_with_bwrap(
            &terminal_call("ls ."),
            PermissionLevel::Destructive,
            ToolRisk::Safe,
            false,
        ));
    }

    #[tokio::test]
    async fn disabled_network_boundary_denies_before_dispatch_and_is_audited() {
        let root = tempfile::tempdir().expect("root");
        let store = SessionStore::new(root.path());
        let session = store.create_session();
        let mut executor = ToolExecutor::new(
            root.path().to_path_buf(),
            ExecutionConfig {
                boundaries: crate::config::BoundaryConfig {
                    network: "disabled".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .with_session_store(store.clone())
        .with_auto_approve(true);

        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "search_web".to_string(),
                    arguments: json!({"query": "must not be dispatched"}),
                    session_id: Some(session.id.clone()),
                    project_root: Some(root.path().to_path_buf()),
                },
                Some(&session.id),
            )
            .await;

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("network access is disabled")));
        assert_eq!(result.approval_source.as_deref(), Some("policy"));
        let audited = store.load(&session.id).expect("audited session");
        let call = audited.tool_calls.last().expect("tool record");
        assert_eq!(call.result.as_ref().unwrap()["permission_level"], "network");
        assert_eq!(call.result.as_ref().unwrap()["risk"], "network");
        assert_eq!(
            call.boundaries
                .as_ref()
                .expect("effective boundary")
                .network,
            "disabled"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_session_audit_destination_fails_closed_without_partial_delegation() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = tempfile::tempdir().expect("root");
        let workspace = root
            .path()
            .join(OsString::from_vec(b"workspace-\xff".to_vec()));
        let sessions_dir = workspace
            .join(".nib")
            .join("profiles")
            .join(OsString::from_vec(b"profile-\xfe".to_vec()))
            .join(OsString::from_vec(b"sessions-\xfd".to_vec()));
        std::fs::create_dir_all(&sessions_dir).expect("non-UTF-8 session directory");
        let store = SessionStore::at_dir(sessions_dir);
        let session = store.create_session_with_id("non-utf8-audit-session");
        let mut executor = ToolExecutor::new(workspace.clone(), ExecutionConfig::default())
            .with_session_store(store.clone())
            .with_auto_approve(true);

        for tool_name in ["spawn_subagent", "invoke_subagent"] {
            let result = executor
                .execute(
                    ToolCall {
                        invocation_id: crate::tools::ToolInvocationId::new(),
                        tool_name: tool_name.to_string(),
                        arguments: json!({"prompt": "must fail before delegation dispatch"}),
                        session_id: Some(session.id.clone()),
                        project_root: Some(workspace.clone()),
                    },
                    Some(&session.id),
                )
                .await;

            assert!(!result.success, "{tool_name} unexpectedly dispatched");
            assert!(
                result.error.as_deref().is_some_and(|error| error.contains(
                    "audit destination cannot be represented without changing its filesystem identity"
                )),
                "unexpected {tool_name} error: {:?}",
                result.error
            );
            let audited = store.load(&session.id).expect("audited failure session");
            let call = audited.tool_calls.last().expect("audited tool failure");
            assert_eq!(call.tool_name.as_deref(), Some(tool_name));
            assert!(call.error.as_deref().is_some_and(|error| error.contains(
                "audit destination cannot be represented without changing its filesystem identity"
            )));
        }

        for path in [
            workspace.join(".nib/subagents"),
            workspace.join(".nib/subagent-owner-leases"),
            workspace.join(".nib/worktrees/subagents"),
        ] {
            assert!(
                !path.exists(),
                "failed path serialization created partial delegation state at {}",
                path.display()
            );
        }
    }

    #[test]
    fn redacts_structured_and_inline_secrets() {
        let value = redact_value(json!({
            "api_key": "secret",
            "output": "using sk-123456789",
        }));
        assert_eq!(value["api_key"], "[REDACTED]");
        assert!(!value["output"].as_str().unwrap().contains("sk-123456789"));
    }

    #[tokio::test]
    async fn executor_results_and_audit_use_encoded_control_safe_projection() {
        let root = tempfile::tempdir().expect("root");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("encoded-tool-audit");
        let secret = "provider/env-secret".to_string();
        let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_session_store(store.clone())
            .with_sensitive_values([secret.clone(), "read-only".to_string()])
            .with_auto_approve(true);

        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "ask_question".to_string(),
                    arguments: json!({
                        "question": format!(
                            "{secret} provider\\/env-secret cHJvdmlkZXIvZW52LXNlY3JldA== \u{1b}[2J"
                        ),
                        "options": ["provider%2Fenv-secret\r"],
                        "answer": "cHJvdmlkZXIvZW52LXNlY3JldA==\t",
                    }),
                    session_id: Some(session.id.clone()),
                    project_root: Some(root.path().to_path_buf()),
                },
                Some(&session.id),
            )
            .await;

        assert!(result.success, "{:?}", result.error);
        let persisted = store.load(&session.id).expect("tool audit session");
        let public_surface = serde_json::to_string(&json!({
            "result": result,
            "session": persisted,
        }))
        .expect("serialize tool public surfaces");
        for forbidden in [
            secret.as_str(),
            r"provider\/env-secret",
            "provider%2Fenv-secret",
            "cHJvdmlkZXIvZW52LXNlY3JldA==",
            "read-only",
            r"\u001b",
        ] {
            assert!(
                !public_surface.contains(forbidden),
                "tool public surface contained {forbidden:?}"
            );
        }
    }

    #[test]
    fn tool_audit_projects_every_auxiliary_metadata_field() {
        const SECRET: &str = "audit/metadata-secret";
        const JSON_SECRET: &str = r"audit\/metadata-secret";
        const PERCENT_SECRET: &str = "audit%2Fmetadata-secret";
        const BASE64_SECRET: &str = "YXVkaXQvbWV0YWRhdGEtc2VjcmV0";
        let root = tempfile::tempdir().expect("root");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("auxiliary-metadata-audit");
        let environment = HashMap::from([(
            format!("ENV_{SECRET}\u{1b}[2J"),
            "non-sensitive-value".to_string(),
        )]);
        let executor = ToolExecutor::new(
            root.path().to_path_buf(),
            ExecutionConfig {
                provider: SECRET.to_string(),
                default_profile: JSON_SECRET.to_string(),
                boundaries: crate::config::BoundaryConfig {
                    allow_write: vec![BASE64_SECRET.to_string()],
                    network: PERCENT_SECRET.to_string(),
                },
                ..ExecutionConfig::default()
            },
        )
        .with_session_store(store.clone())
        .with_environment(&environment)
        .with_sensitive_values([SECRET.to_string()]);
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": "true"}),
            session_id: Some(session.id.clone()),
            project_root: Some(root.path().to_path_buf()),
        };
        let result = ToolResult {
            invocation_id: call.invocation_id,
            tool_name: call.tool_name.clone(),
            success: false,
            output: Some(json!({
                "provider": PERCENT_SECRET,
                "sandbox_profile": JSON_SECRET,
                "bwrap_args": [format!("{BASE64_SECRET}\u{1b}[2J")],
                "boundaries": {
                    "allow_write": [SECRET],
                    "network": PERCENT_SECRET,
                },
                "arbitrary_metadata": format!("{SECRET}\u{202e}"),
            })),
            error: Some(format!("{JSON_SECRET}\u{1b}[2J")),
            duration_seconds: 0.0,
            approval_granted: true,
            approval_source: Some(SECRET.to_string()),
        };
        let approval = ApprovalDecision {
            granted: true,
            source: PERCENT_SECRET.to_string(),
            note: Some(format!("{BASE64_SECRET}\u{202e}")),
        };

        executor
            .record(
                &call,
                &result,
                &approval,
                Some(&session.id),
                Some(Path::new("/tmp/audit/metadata-secret")),
                PermissionLevel::Destructive,
                ToolRisk::Destructive,
                Some(format!("plan-{SECRET}\u{1b}")),
            )
            .expect("record redacted audit metadata");

        let persisted = store.load(&session.id).expect("persisted audit");
        let public = serde_json::to_string(&persisted).expect("serialize audit");
        for forbidden in [
            SECRET,
            JSON_SECRET,
            PERCENT_SECRET,
            BASE64_SECRET,
            r"\u001b",
            r"\u202e",
        ] {
            assert!(
                !public.contains(forbidden),
                "audit contained {forbidden:?}: {public}"
            );
        }
        assert!(public.contains("[REDACTED]"), "{public}");
    }

    #[tokio::test]
    async fn embedded_generic_secret_in_tool_name_is_redacted_from_audit() {
        let directory = tempfile::tempdir().expect("audit project");
        let store = SessionStore::new(directory.path());
        store.create_session_with_id("metadata-audit");
        let mut executor =
            ToolExecutor::new(directory.path().to_path_buf(), ExecutionConfig::default())
                .with_session_store(store.clone());
        let secret = "sk-secretvalue123";

        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: format!("fixture::prefix_{secret}"),
                    arguments: json!({}),
                    session_id: None,
                    project_root: Some(directory.path().to_path_buf()),
                },
                Some("metadata-audit"),
            )
            .await;

        assert!(!result.success);
        let session = store.load("metadata-audit").expect("audited session");
        let serialized = serde_json::to_string(&session).expect("serialize audited session");
        assert!(
            !serialized.contains(secret),
            "secret escaped audit: {serialized}"
        );
        assert!(serialized.contains("[REDACTED]"), "{serialized}");
    }

    #[test]
    fn redacts_sensitive_environment_values_without_hiding_benign_values() {
        let environment = HashMap::from([
            (
                "DEPLOY_TOKEN".to_string(),
                "opaque-profile-value".to_string(),
            ),
            ("COLOR".to_string(), "green".to_string()),
        ]);
        let redacted = redact_value_with_environment(
            json!({"stdout": "opaque-profile-value green"}),
            &environment,
        );

        assert_eq!(redacted["stdout"], "[REDACTED] green");

        let root = tempfile::tempdir().expect("root");
        let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_sensitive_values(["provider-credential-without-prefix".to_string()]);
        assert_eq!(
            executor.redact_text("provider-credential-without-prefix"),
            "[REDACTED]"
        );
    }

    #[test]
    fn provider_redaction_is_symmetric_across_percent_decoding_stages() {
        assert_eq!(base64_secret_variant(b"f", false), "Zg==");
        assert_eq!(base64_secret_variant(b"fo", false), "Zm8=");
        assert_eq!(base64_secret_variant(b"foo", false), "Zm9v");
        assert_eq!(
            redact_text_with_encoded_sensitive_values("request-Zm9v", ["foo".to_string()]),
            "request-[REDACTED]"
        );
        assert_eq!(
            redact_text_with_encoded_sensitive_values(
                r#"before-active\/credential-after"#,
                ["active/credential".to_string()]
            ),
            "before-[REDACTED]-after"
        );
        for (text, secret) in [
            ("model-prefix-env%2Fonly-suffix", "env/only"),
            ("model-prefix-env/only-suffix", "env%2Fonly"),
            ("model-prefix-env%252Fonly-suffix", "env/only"),
        ] {
            let redacted = redact_text_with_encoded_sensitive_values(text, [secret.to_string()]);
            assert_eq!(redacted, "model-prefix-[REDACTED]-suffix");
            assert!(!redacted.contains("env"));
        }

        let redacted = redact_value_with_encoded_sensitive_values(
            json!({"error": "provider echoed env%252Fonly"}),
            [" env/only ".to_string()],
        );
        assert_eq!(redacted["error"], "provider echoed [REDACTED]");

        let adversarial = format!("prefix-%{}41-suffix", "25".repeat(32));
        assert_eq!(
            redact_text_with_encoded_sensitive_values(
                &adversarial,
                ["unrelated-secret".to_string()]
            ),
            "[REDACTED]"
        );

        let secret = "provider-\u{0fff}-secret";
        let standard = base64_secret_variant(secret.as_bytes(), false);
        let url_safe = base64_secret_variant(secret.as_bytes(), true);
        assert_ne!(standard, url_safe);
        for encoded in [
            standard.clone(),
            standard.trim_end_matches('=').to_string(),
            url_safe.clone(),
            url_safe.trim_end_matches('=').to_string(),
        ] {
            assert_eq!(
                redact_text_with_encoded_sensitive_values(
                    &format!("before-{encoded}-after"),
                    [secret.to_string()]
                ),
                "before-[REDACTED]-after"
            );
        }
    }

    #[test]
    fn redaction_preserves_payload_whitespace() {
        assert_eq!(
            redact_text("first line\nsecond\tline\n"),
            "first line\nsecond\tline\n"
        );
    }

    #[test]
    fn generic_schema_validation_reports_paths_and_constraints() {
        let schema = json!({
            "type": "object",
            "properties": {
                "request": {
                    "type": "object",
                    "properties": {
                        "count": {"type": "integer", "minimum": 1}
                    },
                    "required": ["count"],
                    "additionalProperties": false
                }
            },
            "required": ["request"],
            "additionalProperties": false
        });

        validate_tool_arguments("server::nested", &schema, &json!({"request": {"count": 2}}))
            .expect("valid nested MCP arguments");
        let error = validate_tool_arguments(
            "server::nested",
            &schema,
            &json!({"request": {"count": 0, "extra": true}}),
        )
        .expect_err("invalid nested arguments");
        assert!(error.contains("server::nested"));
        assert!(error.contains("/request/count") || error.contains("minimum"));
        assert!(error.contains("additionalProperties"));

        let internal_hook_call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({
                "command": "true",
                "hook_source": "fixture",
                "hook_for": "read_file"
            }),
            session_id: None,
            project_root: None,
        };
        let hook_arguments = schema_validation_arguments(&internal_hook_call);
        validate_tool_arguments(
            "run_terminal",
            &get_tool_metadata("run_terminal").unwrap().input_schema,
            &hook_arguments,
        )
        .expect("executor-owned hook context is removed before validation");
    }

    #[tokio::test]
    async fn executor_rejects_invalid_arguments_before_dispatch() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("file.txt"), "content").expect("fixture");
        let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());

        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "read_file".to_string(),
                    arguments: json!({"path": "file.txt", "max_bytes": "unbounded"}),
                    session_id: None,
                    project_root: Some(root.path().to_path_buf()),
                },
                None,
            )
            .await;

        assert!(!result.success);
        let error = result.error.expect("validation error");
        assert!(error.contains("invalid arguments for tool 'read_file'"));
        assert!(error.contains("max_bytes"));
        assert_eq!(result.approval_source.as_deref(), Some("policy"));
    }

    #[tokio::test]
    async fn executor_schema_diagnostics_never_reflect_boundary_straddling_credentials() {
        let root = tempfile::tempdir().expect("root");
        let store = SessionStore::new(root.path());
        let session = store.create_session_with_id("schema-diagnostic-redaction");
        let secret = format!("schema/boundary/{}", "s".repeat(512));
        let environment = HashMap::from([("DEPLOY_TOKEN".to_string(), secret.clone())]);
        let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_session_store(store.clone())
            .with_environment(&environment);

        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "read_file".to_string(),
                    arguments: json!({
                        "path": format!("{}{}-tail", "p".repeat(8_000), secret)
                    }),
                    session_id: Some(session.id.clone()),
                    project_root: Some(root.path().to_path_buf()),
                },
                Some(&session.id),
            )
            .await;

        assert!(!result.success);
        let persisted = store.load(&session.id).expect("schema audit session");
        let public = serde_json::to_string(&json!({
            "result": result,
            "session": persisted,
        }))
        .expect("serialize public schema surfaces");
        assert!(public.contains("/path"), "{public}");
        assert!(public.contains("maxLength"), "{public}");
        assert!(
            !public.contains(&secret[..128]),
            "credential prefix survived schema diagnostic truncation: {public}"
        );
    }

    #[tokio::test]
    async fn executor_rejects_oversized_tool_inputs_before_approval_or_dispatch() {
        let root = tempfile::tempdir().expect("root");
        let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());

        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "run_terminal".to_string(),
                    arguments: json!({"command": "x".repeat(65_537)}),
                    session_id: None,
                    project_root: Some(root.path().to_path_buf()),
                },
                None,
            )
            .await;

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid arguments")));
        assert_eq!(result.approval_source.as_deref(), Some("policy"));
    }

    #[tokio::test]
    async fn terminal_output_sender_is_bounded_and_redacted() {
        let root = tempfile::tempdir().expect("root");
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let environment = HashMap::from([(
            "DEPLOY_TOKEN".to_string(),
            "profile-secret-value".to_string(),
        )]);
        let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_environment(&environment)
            .with_terminal_output_sender(sender);
        let callback = executor
            .redacted_terminal_output_callback()
            .expect("terminal callback");

        callback(core::TerminalOutputEvent {
            tool_name: "run_terminal".to_string(),
            stream: core::TerminalOutputStream::Stdout,
            chunk: format!("sk-123456789 profile-secret-value{}", "x".repeat(64)).into_bytes(),
            background_task_id: None,
            eof: false,
        });
        callback(core::TerminalOutputEvent {
            tool_name: "run_terminal".to_string(),
            stream: core::TerminalOutputStream::Stdout,
            chunk: b"dropped when full".to_vec(),
            background_task_id: None,
            eof: false,
        });

        let event = receiver.recv().await.expect("stream event");
        let output = String::from_utf8(event.chunk).expect("redacted UTF-8");
        assert!(output.starts_with("[REDACTED] [REDACTED]"));
        assert!(!output.contains("sk-123456789"));
        assert!(!output.contains("profile-secret-value"));
        assert!(receiver.try_recv().is_err(), "full channel must not grow");
    }

    #[test]
    fn terminal_stream_redaction_hides_secrets_split_across_chunks() {
        let root = tempfile::tempdir().expect("root");
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let callback: core::TerminalOutputCallback = Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        });
        let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_sensitive_values(["provider-credential-without-prefix".to_string()])
            .with_terminal_output_callback(callback);
        let redacted = executor
            .redacted_terminal_output_callback()
            .expect("redacted callback");

        let chunks: &[&[u8]] = &[
            b"before provider-cre",
            b"dential-without-prefix and prefix_sk-123",
            b"456789 after and encoded cHJvdmlkZXItY3JlZGVudGlhbC13aXRob3V0LXByZWZpeA== \x1b[31m ",
            &[0xF0, 0x9F],
            &[0x98, 0x80],
        ];
        for chunk in chunks {
            redacted(core::TerminalOutputEvent {
                tool_name: "run_terminal".to_string(),
                stream: core::TerminalOutputStream::Stdout,
                chunk: chunk.to_vec(),
                background_task_id: None,
                eof: false,
            });
        }
        redacted(core::TerminalOutputEvent {
            tool_name: "run_terminal".to_string(),
            stream: core::TerminalOutputStream::Stdout,
            chunk: Vec::new(),
            background_task_id: None,
            eof: true,
        });

        let output = events
            .lock()
            .unwrap()
            .iter()
            .flat_map(|event| event.chunk.iter().copied())
            .collect::<Vec<_>>();
        let output = String::from_utf8(output).expect("redacted stream is valid UTF-8");
        assert_eq!(
            output,
            format!(
                "before [REDACTED] and prefix_[REDACTED] after and encoded [REDACTED] �[31m {}",
                '\u{1f600}'
            )
        );
        assert!(!output.contains("provider-credential-without-prefix"));
        assert!(!output.contains("cHJvdmlkZXItY3JlZGVudGlhbC13aXRob3V0LXByZWZpeA=="));
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains("sk-123456789"));
    }

    #[test]
    fn terminal_stream_redaction_hides_percent_encoded_secrets_across_chunks() {
        let root = tempfile::tempdir().expect("root");
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let callback: core::TerminalOutputCallback = Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        });
        let secret = "A ".repeat(100);
        let percent_secret = "A%20".repeat(100);
        let nested_secret = "A%2520".repeat(100);
        let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_sensitive_values([secret.clone()])
            .with_terminal_output_callback(callback);
        let redacted = executor
            .redacted_terminal_output_callback()
            .expect("redacted callback");

        let input = format!("before {percent_secret} middle {nested_secret} after url=a%20b");
        for chunk in input.as_bytes().chunks(137) {
            redacted(core::TerminalOutputEvent {
                tool_name: "run_terminal".to_string(),
                stream: core::TerminalOutputStream::Stdout,
                chunk: chunk.to_vec(),
                background_task_id: None,
                eof: false,
            });
        }
        redacted(core::TerminalOutputEvent {
            tool_name: "run_terminal".to_string(),
            stream: core::TerminalOutputStream::Stdout,
            chunk: Vec::new(),
            background_task_id: None,
            eof: true,
        });

        let output = events
            .lock()
            .unwrap()
            .iter()
            .flat_map(|event| event.chunk.iter().copied())
            .collect::<Vec<_>>();
        let output = String::from_utf8(output).expect("redacted stream is valid UTF-8");
        assert_eq!(
            output,
            "before [REDACTED] middle [REDACTED] after url=a%20b"
        );
        assert!(!output.contains(&secret));
        assert!(!output.contains(&percent_secret));
        assert!(!output.contains(&nested_secret));
    }

    #[test]
    fn terminal_stream_redaction_decodes_percent_stages_symmetrically() {
        let secret = "credential%2Fmarker";
        let secrets = normalized_encoded_sensitive_values([secret.to_string()]);
        let mut redactor = TerminalStreamRedactor::new(&secrets);
        let input = format!(
            "raw {secret}{} decoded credential/marker{} safe url=a%20b",
            "x".repeat(64),
            "y".repeat(64),
        );
        let mut output = Vec::new();
        for chunk in input.as_bytes().chunks(7) {
            output.extend(redactor.push(chunk, false));
        }
        output.extend(redactor.push(&[], true));

        let output = String::from_utf8(output).expect("redacted stream is valid UTF-8");
        assert_eq!(
            output,
            format!(
                "raw [REDACTED]{} decoded [REDACTED]{} safe url=a%20b",
                "x".repeat(64),
                "y".repeat(64),
            )
        );
        assert!(!output.contains(secret));
        assert!(!output.contains("credential/marker"));
    }

    #[tokio::test]
    async fn cancelling_during_an_after_tool_hook_fails_the_prepared_task() {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
        for arguments in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "nib-tests@example.invalid"],
            vec!["config", "user.name", "nib tests"],
            vec!["add", "README.md"],
            vec!["commit", "--quiet", "-m", "initial"],
        ] {
            let status = std::process::Command::new("git")
                .args(arguments)
                .current_dir(root.path())
                .status()
                .expect("git fixture command");
            assert!(status.success());
        }
        let mut config = crate::config::NibConfig::default();
        config.profiles.default = "selected".to_string();
        config.profiles.active = vec![crate::config::ProfileConfig {
            id: "selected".to_string(),
            root: PathBuf::from("."),
            ..crate::config::ProfileConfig::default()
        }];
        crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");
        let store = SessionStore::for_project(root.path()).expect("session store");
        store.create_session_with_id("hook-cancel");
        let task_store =
            crate::daemons::workload::DurableTaskStore::from_sessions_dir(store.sessions_dir())
                .expect("durable task store");
        let mut executor = ToolExecutor::new(
            root.path().to_path_buf(),
            ExecutionConfig {
                provider: "internal".to_string(),
                default_profile: "internal".to_string(),
                plan_mode: false,
                ..ExecutionConfig::default()
            },
        )
        .with_auto_approve(true)
        .with_session_store(store)
        .with_deferred_background_start(true)
        .with_after_tool_hooks([AfterToolHook {
            source: "blocking-hook".to_string(),
            tool_name: "schedule".to_string(),
            command: "sleep 30".to_string(),
        }]);
        let project_root = root.path().to_path_buf();
        let run = tokio::spawn(async move {
            executor
                .execute(
                    ToolCall {
                        invocation_id: crate::tools::ToolInvocationId::new(),
                        tool_name: "schedule".to_string(),
                        arguments: json!({
                            "prompt": "later",
                            "duration_secs": 3_600,
                        }),
                        session_id: Some("hook-cancel".to_string()),
                        project_root: Some(project_root),
                    },
                    Some("hook-cancel"),
                )
                .await
        });

        let prepared = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(task) = task_store
                    .list()
                    .expect("list durable tasks")
                    .into_iter()
                    .find(|task| task.kind == "schedule")
                {
                    break task;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("schedule was prepared before the hook blocked");
        assert_eq!(prepared.status, "prepared", "{prepared:?}");

        run.abort();
        let _ = run.await;
        let failed = task_store
            .get(&prepared.id)
            .expect("load prepared task")
            .expect("prepared task remains auditable");
        assert_eq!(failed.status, "failed");
        assert!(failed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("executor reconciliation")));
    }

    #[tokio::test]
    async fn schedule_uses_executor_owned_session_context() {
        let root = tempfile::tempdir().expect("root");
        let mut config = crate::config::NibConfig::default();
        config.profiles.default = "selected".to_string();
        config.profiles.active = vec![crate::config::ProfileConfig {
            id: "selected".to_string(),
            root: PathBuf::from("."),
            ..crate::config::ProfileConfig::default()
        }];
        crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");
        let sessions_dir = root
            .path()
            .join(".nib")
            .join("profiles")
            .join("selected")
            .join("sessions");
        let store = SessionStore::at_dir(sessions_dir.clone());
        store.create_session_with_id("origin");
        let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
            .with_session_store(store.clone());

        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "schedule".to_string(),
                    arguments: json!({
                        "prompt": "later",
                        "duration_secs": 3600,
                        "_session_id": "spoofed",
                        "_sessions_dir": root.path().join("spoofed"),
                    }),
                    session_id: Some("origin".to_string()),
                    project_root: Some(root.path().to_path_buf()),
                },
                Some("origin"),
            )
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.output.as_ref().unwrap()["session_id"], "origin");
        assert!(!root.path().join("spoofed").exists());
        let timer_id = result.output.as_ref().unwrap()["task_id"].as_str().unwrap();
        crate::daemons::task::TASK_MANAGER
            .cancel(timer_id)
            .expect("cancel fixture timer");
        let session = store.load("origin").expect("origin session");
        assert!(session
            .events
            .iter()
            .any(|event| event.kind == "timer_scheduled"));
    }

    #[tokio::test]
    async fn schedule_rejects_untrusted_reserved_context_without_a_session() {
        let root = tempfile::tempdir().expect("root");
        let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());
        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "schedule".to_string(),
                    arguments: json!({
                        "prompt": "later",
                        "duration_secs": 60,
                        "_session_id": "spoofed",
                        "_sessions_dir": root.path(),
                    }),
                    session_id: None,
                    project_root: Some(root.path().to_path_buf()),
                },
                None,
            )
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("_session_id"));
        let store = SessionStore::for_project(root.path()).expect("implicit audit store");
        let session_ids = store.list_result().expect("implicit audit sessions");
        assert_eq!(session_ids.len(), 1);
        let session = store
            .load_result(&session_ids[0])
            .expect("load implicit audit")
            .expect("implicit audit session");
        assert!(session
            .events
            .iter()
            .any(|event| event.kind == "implicit_audit_session"));
        let record = session.tool_calls.last().expect("schedule denial audit");
        assert_eq!(record.tool_name.as_deref(), Some("schedule"));
        assert_eq!(record.result.as_ref().unwrap()["success"], false);
    }
}
