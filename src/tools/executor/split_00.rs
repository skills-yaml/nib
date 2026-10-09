//! T043 split.

use super::*;

pub(crate) const MAX_INSTRUCTION_POLICY_BYTES: u64 = 1_048_576;
pub(crate) const MAX_APPROVAL_FIELD_BYTES: usize = 240;
pub(crate) const MAX_APPROVAL_LINE_BYTES: usize = 32 * 1024;
pub(crate) const MAX_APPROVAL_DETAIL_PAGE_BYTES: usize = 24 * 1024;
pub(crate) const MAX_APPROVAL_DETAIL_INPUT_BYTES: usize = 2 * 1024 * 1024 + 64 * 1024;
pub(crate) const MAX_APPROVAL_LINES: usize = 6;

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

pub(crate) struct ResolvedExecutionConfig {
    pub(crate) config: ExecutionConfig,
    pub(crate) policy_rules: Vec<PolicyRule>,
    pub(crate) instruction_posture: InstructionExecutionPosture,
}

pub(crate) struct CommandPresentation {
    pub(crate) environment: String,
    pub(crate) shown_command: Option<String>,
    pub(crate) extras: Vec<String>,
    pub(crate) remember_exact: Option<String>,
    pub(crate) offer_grant: bool,
}

pub(crate) fn command_presentation(
    call: &ToolCall,
    secrets: &[String],
    environment: &str,
    allow_remember: bool,
) -> CommandPresentation {
    if call.tool_name != "run_terminal" {
        return CommandPresentation {
            environment: String::new(),
            shown_command: None,
            extras: Vec::new(),
            remember_exact: None,
            offer_grant: true,
        };
    }
    let Some(invocation) = crate::interaction_card::terminal_invocation(call) else {
        return CommandPresentation {
            environment: environment.to_string(),
            shown_command: Some(String::new()),
            extras: Vec::new(),
            remember_exact: None,
            offer_grant: false,
        };
    };
    let shown = crate::interactive::control_safe_text(
        &redact_text_with_encoded_secrets_with_limit(
            &invocation.command,
            secrets,
            MAX_APPROVAL_DETAIL_INPUT_BYTES,
        ),
        true,
    );
    let offer_grant =
        shown == invocation.command && !shown.contains("[REDACTED]") && !shown.trim().is_empty();
    let remember_exact = (allow_remember
        && offer_grant
        && crate::interaction_card::command_can_be_remembered(&invocation.command))
    .then(|| invocation.command.clone());
    CommandPresentation {
        environment: environment.to_string(),
        shown_command: Some(shown),
        extras: crate::interaction_card::command_extra_lines(&invocation),
        remember_exact,
        offer_grant,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalContext {
    pub action: String,
    /// A short, already-redacted subject suitable for approval UIs.
    pub display_subject: String,
    /// An optional, already-redacted location suitable for approval UIs.
    pub display_location: Option<String>,
    pub permission_and_risk: String,
    pub target_scope: String,
    pub network: String,
    pub worktree: String,
    pub reason: String,
    pub choices: String,
    pub details: Vec<String>,
    /// `local` or `sandbox` for a command card. Empty for other tools.
    pub command_environment: String,
    /// Redacted command text. `None` when this approval is not `run_terminal`.
    pub shown_command: Option<String>,
    pub command_extras: Vec<String>,
    /// Exact command that option 2 may remember. Absent when remembering is unsafe.
    pub remember_exact: Option<String>,
    pub offer_grant: bool,
    pub input_error: Option<String>,
}

impl ApprovalContext {
    pub fn compatibility(call: &ToolCall, level: PermissionLevel) -> Self {
        let (display_subject, display_location) = normalized_approval_display(call, &[]);
        let command = command_presentation(call, &[], "local", true);
        Self {
            action: normalized_approval_action(call, &[]),
            display_subject,
            display_location,
            permission_and_risk: format!("{} / not classified", permission_label(level)),
            target_scope: "not available to compatibility handler".to_string(),
            network: "not available to compatibility handler".to_string(),
            worktree: "not available to compatibility handler".to_string(),
            reason: "interactive approval requested".to_string(),
            choices: "approve once or deny".to_string(),
            details: approval_invocation_details(call, &[]),
            command_environment: command.environment,
            shown_command: command.shown_command,
            command_extras: command.extras,
            remember_exact: command.remember_exact,
            offer_grant: command.offer_grant,
            input_error: None,
        }
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
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
        lines.extend(
            self.details.iter().map(|detail| {
                bounded_approval_field_with_limit(detail, &[], MAX_APPROVAL_LINE_BYTES)
            }),
        );
        lines
    }

    pub fn render(&self) -> String {
        self.lines().join("\n")
    }
}

pub(crate) struct PreparedTaskGuard {
    pub(crate) task_id: Option<String>,
}

impl PreparedTaskGuard {
    pub(crate) fn from_output(prepared_work: bool, output: Option<&Value>) -> Self {
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

    pub(crate) fn is_armed(&self) -> bool {
        self.task_id.is_some()
    }

    pub(crate) fn fail(&mut self, error: impl Into<String>) -> Result<(), String> {
        if let Some(task_id) = self.task_id.take() {
            crate::daemons::task::TASK_MANAGER.compensate_prepared_task(&task_id, error.into())?;
        }
        Ok(())
    }

    pub(crate) fn release(&mut self) {
        self.task_id = None;
    }

    pub(crate) fn start(&mut self) -> Result<(), String> {
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

pub(crate) const REDACTION_MARKER: &[u8] = b"[REDACTED]";
pub(crate) const GENERIC_SECRET_MIN_BODY_BYTES: usize = 7;
pub(crate) const GENERIC_SECRET_LOOKAHEAD_BYTES: usize = 4 + GENERIC_SECRET_MIN_BODY_BYTES;
pub(crate) const MAX_PERCENT_DECODE_PASSES: usize = 8;
pub(crate) const MAX_STREAM_REDACTION_PENDING_BYTES: usize = 1024 * 1024;
pub(crate) static GENERIC_SECRET_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?:sk|xai)-[A-Za-z0-9_-]{7,}").expect("static secret pattern is valid")
});

pub(crate) struct StreamingTerminalRedactor {
    pub(crate) stdout: TerminalStreamRedactor,
    pub(crate) stderr: TerminalStreamRedactor,
}

impl StreamingTerminalRedactor {
    pub(crate) fn new(secrets: &[String]) -> Self {
        Self {
            stdout: TerminalStreamRedactor::new(secrets),
            stderr: TerminalStreamRedactor::new(secrets),
        }
    }

    pub(crate) fn push(
        &mut self,
        stream: core::TerminalOutputStream,
        chunk: &[u8],
        eof: bool,
    ) -> Vec<u8> {
        match stream {
            core::TerminalOutputStream::Stdout => self.stdout.push(chunk, eof),
            core::TerminalOutputStream::Stderr => self.stderr.push(chunk, eof),
        }
    }
}

pub(crate) struct RedactedTerminalCapture {
    pub(crate) stdout: VecDeque<u8>,
    pub(crate) stderr: VecDeque<u8>,
    pub(crate) limit: usize,
}

impl RedactedTerminalCapture {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            stdout: VecDeque::with_capacity(limit.min(64 * 1024)),
            stderr: VecDeque::with_capacity(limit.min(64 * 1024)),
            limit,
        }
    }

    pub(crate) fn push(&mut self, stream: core::TerminalOutputStream, chunk: &[u8]) {
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

    pub(crate) fn snapshot(&self) -> (Vec<u8>, Vec<u8>) {
        (
            self.stdout.iter().copied().collect(),
            self.stderr.iter().copied().collect(),
        )
    }
}

pub(crate) struct RedactedTerminalProjection {
    pub(crate) redactor: StreamingTerminalRedactor,
    pub(crate) capture: Option<RedactedTerminalCapture>,
}

pub(crate) fn projected_terminal_stream(
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

pub(crate) fn bounded_redaction_marker(limit: usize) -> Vec<u8> {
    REDACTION_MARKER[..limit.min(REDACTION_MARKER.len())].to_vec()
}

pub(crate) fn projected_terminal_error(output: &Map<String, Value>) -> String {
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

pub(crate) struct TerminalStreamRedactor {
    pub(crate) pending: Vec<ProjectedTerminalByte>,
    pub(crate) utf8_pending: Vec<u8>,
    pub(crate) secrets: Vec<Vec<u8>>,
    pub(crate) lookahead: usize,
    pub(crate) redacting_generic_secret: bool,
    pub(crate) percent_decoders: Vec<StreamingPercentDecoder>,
    pub(crate) overflow_percent_decoder: StreamingPercentDecoder,
    pub(crate) secret_decode_overflow: bool,
    pub(crate) failed_closed: bool,
}

pub(crate) struct ProjectedTerminalByte {
    pub(crate) value: u8,
    pub(crate) source: TerminalByteSource,
}

pub(crate) enum TerminalByteSource {
    Single(u8),
    Multiple(Vec<u8>),
}

impl TerminalByteSource {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Single(_) => 1,
            Self::Multiple(bytes) => bytes.len(),
        }
    }

    pub(crate) fn append_to(self, output: &mut Vec<u8>) {
        match self {
            Self::Single(byte) => output.push(byte),
            Self::Multiple(mut bytes) => output.append(&mut bytes),
        }
    }

    pub(crate) fn append_to_ref(&self, output: &mut Vec<u8>) {
        match self {
            Self::Single(byte) => output.push(*byte),
            Self::Multiple(bytes) => output.extend_from_slice(bytes),
        }
    }
}

#[derive(Default)]
pub(crate) struct StreamingPercentDecoder {
    pub(crate) pending: VecDeque<ProjectedTerminalByte>,
}

impl StreamingPercentDecoder {
    pub(crate) fn push(
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
    pub(crate) fn new(secrets: &[String]) -> Self {
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

    pub(crate) fn push(&mut self, chunk: &[u8], eof: bool) -> Vec<u8> {
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

    pub(crate) fn fail_closed(&mut self) {
        self.pending.clear();
        self.utf8_pending.clear();
        self.redacting_generic_secret = false;
        self.failed_closed = true;
    }

    pub(crate) fn redact_bytes(&mut self, chunk: Vec<ProjectedTerminalByte>, eof: bool) -> Vec<u8> {
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

    pub(crate) fn decode_utf8(&mut self, bytes: Vec<u8>, eof: bool) -> Vec<u8> {
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

pub(crate) fn terminal_bytes_start_with(bytes: &[ProjectedTerminalByte], expected: &[u8]) -> bool {
    bytes.len() >= expected.len()
        && bytes
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.value == *expected)
}

pub(crate) fn generic_terminal_secret_prefix_len(bytes: &[ProjectedTerminalByte]) -> Option<usize> {
    if terminal_bytes_start_with(bytes, b"sk-") {
        Some(3)
    } else if terminal_bytes_start_with(bytes, b"xai-") {
        Some(4)
    } else {
        None
    }
}

pub(crate) fn is_generic_secret_body_byte(byte: u8) -> bool {
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

    /// Whether a person can answer prompts. `policy` mode prompts for
    /// unmatched actions only when this is true and denies otherwise (T080).
    fn can_prompt(&self) -> bool {
        false
    }

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
    pub(crate) async fn prompt(&self, context: &ApprovalContext) -> ApprovalDecision {
        if let Some(command) = context.shown_command.as_deref() {
            let environment = if context.command_environment.is_empty() {
                "local"
            } else {
                context.command_environment.as_str()
            };
            let card = crate::interaction_card::command_approval_card(
                environment,
                &context.reason,
                command,
                &context.command_extras,
                context.remember_exact.as_deref(),
                0,
                false,
            );
            eprintln!("\n{}", card.text);
            if let Some(error) = &context.input_error {
                eprintln!("{error}");
            }
            let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
            loop {
                eprint!("> ");
                let _ = io::stderr().flush();
                let mut line = String::new();
                if reader.read_line(&mut line).await.is_err() {
                    return ApprovalDecision::denied_input_closed();
                }
                match crate::interaction_card::plain_command_line(&card.rows, &line) {
                    crate::interaction_card::PlainCommandLine::Retry(message) => {
                        eprintln!("{message}");
                    }
                    crate::interaction_card::PlainCommandLine::GrantOnce => {
                        return ApprovalDecision::granted_user();
                    }
                    crate::interaction_card::PlainCommandLine::Remember => {
                        let Some(exact) = context.remember_exact.clone() else {
                            eprintln!("this command cannot be remembered");
                            continue;
                        };
                        return ApprovalDecision::granted_remembered(exact);
                    }
                    crate::interaction_card::PlainCommandLine::Deny => {
                        return ApprovalDecision::denied();
                    }
                    crate::interaction_card::PlainCommandLine::NeedReason => {
                        eprint!("Reason to record: ");
                        let _ = io::stderr().flush();
                        let mut reason = String::new();
                        if reader.read_line(&mut reason).await.is_err() {
                            return ApprovalDecision::denied_input_closed();
                        }
                        let reason = reason.trim();
                        if reason.is_empty() {
                            return ApprovalDecision::denied();
                        }
                        if reason.len() > 240 {
                            eprintln!("Input error: the reason is too long");
                            continue;
                        }
                        return ApprovalDecision::denied_with_reason(reason.to_string());
                    }
                }
            }
        }
        eprintln!("\nApproval required\n{}", context.render());
        eprint!("Approve? [y/N]: ");
        let _ = io::stderr().flush();

        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        let mut line = String::new();
        if reader.read_line(&mut line).await.is_ok()
            && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        {
            ApprovalDecision::granted_user()
        } else {
            ApprovalDecision::denied()
        }
    }
}

pub struct ToolExecutor {
    pub(crate) question_outcome_invocation: Option<crate::tools::ToolInvocationId>,
    pub session_store: Option<SessionStore>,
    pub(crate) implicit_session_id: Option<String>,
    pub approval_mode: ApprovalMode,
    pub project_root: PathBuf,
    pub auto_approve: bool,
    pub execution_config: ExecutionConfig,
    pub terminal_backend: String,
    pub terminal_timeout_secs: u64,
    pub approval_handler: Arc<dyn ApprovalHandler>,
    pub(crate) worktree_manager: Option<WorktreeManager>,
    pub(crate) project_read_fallback: bool,
    pub(crate) prepared_worktree_for_batch: bool,
    pub mcp_manager: Option<Arc<McpManager>>,
    pub(crate) policy_rules: Vec<PolicyRule>,
    pub(crate) policy_hooks: Vec<Arc<dyn ToolPolicyHook>>,
    pub(crate) after_tool_hooks: Vec<AfterToolHook>,
    pub(crate) environment: HashMap<String, String>,
    pub(crate) sensitive_values: Vec<String>,
    pub(crate) defer_background_start: bool,
    pub(crate) terminal_output_callback: Option<core::TerminalOutputCallback>,
    pub(crate) cancellation: Option<crate::agent::CancellationSignal>,
}
