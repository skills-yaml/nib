//! MCP JSON-RPC server backed by the same gated executor as the CLI.

use super::mcp_framing::{encode_json_line, read_async_frame, MAX_MCP_FRAME_BYTES};
use crate::config::{load_nib_config_full, NibConfig};
use crate::session::{SessionEvent, SessionStore};
use crate::tools::executor::ApprovalHandler;
use crate::tools::models::{ApprovalDecision, PermissionLevel, ToolCall, ToolResult};
use crate::tools::{registry, ToolExecutor};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_MCP_TOOL_OUTPUT_BYTES: usize = MAX_MCP_FRAME_BYTES / 4;
const MAX_MCP_VALIDATION_ERROR_BYTES: usize = 8 * 1024;
const MAX_ACTIVE_MCP_REQUESTS: usize = 32;
const MCP_REQUEST_CANCELLED_CODE: i64 = -32800;
const MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(not(test))]
const MCP_SUBAGENT_SHUTDOWN_HANDOFF_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
#[cfg(test)]
const MCP_SUBAGENT_SHUTDOWN_HANDOFF_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(500);

struct OwnedResponseWriter {
    response_tx: Option<mpsc::Sender<QueuedResponse>>,
    task: Option<JoinHandle<()>>,
    relay: Option<crate::sandbox::ManagedChild>,
}

#[derive(Debug)]
struct QueuedResponse {
    frame: Vec<u8>,
    request_key: Option<String>,
}

impl OwnedResponseWriter {
    async fn start() -> Result<(Self, mpsc::Receiver<String>, mpsc::Receiver<String>), String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("failed to resolve MCP stdout relay executable: {error}"))?;
        let mut command = tokio::process::Command::new(executable);
        command
            .arg("mcp-stdio-relay")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null());
        crate::sandbox::apply_child_environment(&mut command, &HashMap::new());
        let mut relay = crate::sandbox::spawn_managed_stdio_relay_child(&mut command)
            .map_err(|error| format!("failed to start MCP stdout relay: {error}"))?;
        let stdin = match relay.stdin.take() {
            Some(stdin) => stdin,
            None => {
                relay.terminate_and_reap().await;
                return Err("MCP stdout relay has no stdin".to_string());
            }
        };
        Ok(Self::from_writer(stdin, Some(relay)))
    }

    fn from_writer<W>(
        mut writer: W,
        relay: Option<crate::sandbox::ManagedChild>,
    ) -> (Self, mpsc::Receiver<String>, mpsc::Receiver<String>)
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (response_tx, mut response_rx) =
            mpsc::channel::<QueuedResponse>(MAX_ACTIVE_MCP_REQUESTS);
        let (failure_tx, failure_rx) = mpsc::channel::<String>(1);
        let (written_tx, written_rx) = mpsc::channel::<String>(MAX_ACTIVE_MCP_REQUESTS);
        let task = tokio::spawn(async move {
            while let Some(response) = response_rx.recv().await {
                if let Err(error) = writer.write_all(&response.frame).await {
                    let _ = failure_tx
                        .send(format!("failed to write MCP stdout: {error}"))
                        .await;
                    return;
                }
                if let Err(error) = writer.flush().await {
                    let _ = failure_tx
                        .send(format!("failed to flush MCP stdout: {error}"))
                        .await;
                    return;
                }
                if let Some(request_key) = response.request_key {
                    if written_tx.send(request_key).await.is_err() {
                        return;
                    }
                }
            }
        });
        (
            Self {
                response_tx: Some(response_tx),
                task: Some(task),
                relay,
            },
            failure_rx,
            written_rx,
        )
    }

    fn sender(&self) -> &mpsc::Sender<QueuedResponse> {
        self.response_tx
            .as_ref()
            .expect("MCP response writer is active")
    }

    async fn shutdown(&mut self) {
        self.response_tx.take();
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(relay) = self.relay.as_mut() {
            relay.terminate_and_reap().await;
        }
        self.relay.take();
    }
}

struct ActiveRequest {
    generation: u64,
    id: Value,
    task_kind: ActiveTaskKind,
    lifecycle: SharedRequestLifecycle,
    cancellation_class: RequestCancellationClass,
    cancellation_audit: SharedCancellationAuditSlot,
    cancellation: crate::agent::CancellationSignal,
    task: JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTaskKind {
    Execution,
    Cancellation,
}

#[derive(Debug, Clone)]
struct CompletedRequest {
    key: String,
    generation: u64,
}

struct RequestCompletionGuard {
    completion_tx: mpsc::Sender<CompletedRequest>,
    missed_completions: Arc<StdMutex<Vec<CompletedRequest>>>,
    missed_completion_overflow: Arc<std::sync::atomic::AtomicBool>,
    completion_notify: Arc<Notify>,
    completion: Option<CompletedRequest>,
}

impl Drop for RequestCompletionGuard {
    fn drop(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        if self.completion_tx.try_send(completion.clone()).is_err() {
            store_missed_completion(
                &self.missed_completions,
                &self.missed_completion_overflow,
                &self.completion_notify,
                completion,
            );
        }
    }
}

#[derive(Debug)]
enum RequestLifecycle {
    Running,
    CancelRequested,
    Reconciling { generation: u64 },
    Completed(Option<Value>),
    CancellationFailed { response: Value, error: String },
    Cancelled,
}

type SharedRequestLifecycle = Arc<StdMutex<RequestLifecycle>>;

enum CancellationOutcome {
    Completed,
    Cancelled,
    Failed(String),
}

enum SubagentCancellationOutcome {
    Cancelled(Value),
    Terminal,
    Unresolved { details: Value, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RequestCancellationClass {
    Protocol,
    ReadOnlyTool { tool_name: String },
    InterruptibleTool { tool_name: String },
    EffectUnknownTool { tool_name: String },
    SubagentStart { tool_name: String },
}

enum InboundFrame {
    Frame(Vec<u8>),
    Eof,
    Failed(String),
}

struct McpRuntime {
    project_root: PathBuf,
    session_store: SessionStore,
    environment: HashMap<String, String>,
}

struct PreparedToolCall {
    requested_name: String,
    executor_name: String,
    arguments: Value,
    requested_status_id: Option<String>,
}

struct McpCancellationAuditState {
    session_store: SessionStore,
    session_id: String,
    tool_name: String,
    cancellation_id: String,
    status: StdMutex<McpCancellationAuditStatus>,
    #[cfg(test)]
    injected_failures: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    injected_post_commit_failures: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpCancellationAuditStatus {
    Pending,
    CancellationOwned,
    FallbackOwned,
    Completed,
    Cancelled,
}

struct McpCancellationAuditGuard {
    state: Arc<McpCancellationAuditState>,
    armed: bool,
}

#[derive(Default)]
struct CancellationAuditSlot {
    state: StdMutex<Option<Arc<McpCancellationAuditState>>>,
    ready: Notify,
}

type SharedCancellationAuditSlot = Arc<CancellationAuditSlot>;

#[cfg(test)]
struct ReconciliationBarrier {
    subagent_id: String,
    entered: std::sync::atomic::AtomicBool,
    release: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
static RECONCILIATION_BARRIER: std::sync::LazyLock<StdMutex<Option<Arc<ReconciliationBarrier>>>> =
    std::sync::LazyLock::new(|| StdMutex::new(None));

struct HandledRequest {
    response: Option<Value>,
    cancellation_audit: Option<McpCancellationAuditGuard>,
}

impl HandledRequest {
    fn without_audit(response: Option<Value>) -> Self {
        Self {
            response,
            cancellation_audit: None,
        }
    }

    fn complete_audit(&mut self) {
        if let Some(audit) = self.cancellation_audit.as_mut() {
            audit.state.complete();
            audit.armed = false;
        }
    }

    fn disarm_audit(&mut self) {
        if let Some(audit) = self.cancellation_audit.as_mut() {
            audit.armed = false;
        }
    }

    fn finalize_cancellation_audit(&mut self, details: Value) -> Result<(), String> {
        let audit = self
            .cancellation_audit
            .as_mut()
            .ok_or_else(|| "MCP cancellation audit was not initialized".to_string())?;
        let result = audit.state.finalize_cancelled(details);
        audit.armed = false;
        result
    }
}

impl McpCancellationAuditState {
    fn complete(&self) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *status == McpCancellationAuditStatus::Pending {
            *status = McpCancellationAuditStatus::Completed;
        }
    }

    fn finalize_cancelled(&self, details: Value) -> Result<(), String> {
        self.finalize_cancelled_until(
            details,
            std::time::Instant::now() + MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT,
        )
    }

    fn finalize_cancelled_until(
        &self,
        details: Value,
        deadline: std::time::Instant,
    ) -> Result<(), String> {
        self.finalize_cancelled_until_owned(
            details,
            deadline,
            McpCancellationAuditStatus::CancellationOwned,
        )
    }

    fn finalize_cancelled_until_owned(
        &self,
        details: Value,
        deadline: std::time::Instant,
        owner: McpCancellationAuditStatus,
    ) -> Result<(), String> {
        debug_assert!(matches!(
            owner,
            McpCancellationAuditStatus::CancellationOwned
                | McpCancellationAuditStatus::FallbackOwned
        ));
        let details = self.with_cancellation_id(details);
        let mut last_error = None;
        for _ in 0..2 {
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match *status {
                McpCancellationAuditStatus::Cancelled => return Ok(()),
                McpCancellationAuditStatus::Completed => {
                    return Err(
                        "MCP request already completed before cancellation audit".to_string()
                    )
                }
                McpCancellationAuditStatus::Pending => *status = owner,
                current if current == owner => {}
                McpCancellationAuditStatus::CancellationOwned
                | McpCancellationAuditStatus::FallbackOwned => {
                    return Err(
                        "MCP cancellation audit is owned by another reconciliation path"
                            .to_string(),
                    )
                }
            }
            #[cfg(test)]
            if self
                .injected_failures
                .fetch_update(
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                last_error = Some("injected MCP cancellation audit write failure".to_string());
                continue;
            }
            let write_result = self.record_cancellation_event_once(&details, deadline);
            match write_result {
                Ok(_wrote_event) => {
                    #[cfg(test)]
                    if _wrote_event
                        && self
                            .injected_post_commit_failures
                            .fetch_update(
                                std::sync::atomic::Ordering::AcqRel,
                                std::sync::atomic::Ordering::Acquire,
                                |remaining| remaining.checked_sub(1),
                            )
                            .is_ok()
                    {
                        last_error =
                            Some("injected post-commit MCP cancellation audit failure".to_string());
                        if self.authoritative_event_exists(deadline)? {
                            *status = McpCancellationAuditStatus::Cancelled;
                            return Ok(());
                        }
                        continue;
                    }
                    *status = McpCancellationAuditStatus::Cancelled;
                    return Ok(());
                }
                Err(error) => {
                    last_error = Some(error);
                    if self.authoritative_event_exists(deadline)? {
                        *status = McpCancellationAuditStatus::Cancelled;
                        return Ok(());
                    }
                }
            }
        }
        Err(format!(
            "failed to persist MCP cancellation audit: {}",
            last_error.unwrap_or_else(|| "unknown persistence failure".to_string())
        ))
    }

    fn claim_cancellation(&self) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *status {
            McpCancellationAuditStatus::Pending => {
                *status = McpCancellationAuditStatus::CancellationOwned;
                Ok(())
            }
            McpCancellationAuditStatus::CancellationOwned
            | McpCancellationAuditStatus::Cancelled => Ok(()),
            McpCancellationAuditStatus::FallbackOwned => {
                Err("MCP cancellation audit fallback already owns reconciliation".to_string())
            }
            McpCancellationAuditStatus::Completed => {
                Err("MCP request already completed before cancellation audit ownership".to_string())
            }
        }
    }

    fn claim_fallback(&self) -> bool {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *status != McpCancellationAuditStatus::Pending {
            return false;
        }
        *status = McpCancellationAuditStatus::FallbackOwned;
        true
    }

    fn finalize_fallback(&self, details: Value) -> Result<(), String> {
        self.finalize_cancelled_until_owned(
            details,
            std::time::Instant::now() + MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT,
            McpCancellationAuditStatus::FallbackOwned,
        )
    }

    fn with_cancellation_id(&self, details: Value) -> Value {
        match details {
            Value::Object(mut object) => {
                object.insert(
                    "cancellation_id".to_string(),
                    Value::String(self.cancellation_id.clone()),
                );
                Value::Object(object)
            }
            details => json!({
                "cancellation_id": self.cancellation_id.clone(),
                "details": details,
            }),
        }
    }

    fn record_cancellation_event_once(
        &self,
        details: &Value,
        deadline: std::time::Instant,
    ) -> Result<bool, String> {
        let cancellation_id = self.cancellation_id.clone();
        self.session_store
            .update_or_create_session_with_deadline(&self.session_id, deadline, |session| {
                if session.events.iter().any(|event| {
                    event.kind == "mcp_request_cancelled"
                        && event.details["cancellation_id"].as_str()
                            == Some(cancellation_id.as_str())
                }) {
                    return Ok(false);
                }
                session.events.push(SessionEvent {
                    index: session.events.len(),
                    kind: "mcp_request_cancelled".to_string(),
                    details: details.clone(),
                    timestamp: Some(chrono::Utc::now()),
                });
                Ok(true)
            })
            .map_err(|error| error.to_string())
    }

    fn authoritative_event_exists(&self, deadline: std::time::Instant) -> Result<bool, String> {
        self.session_store
            .load_result_with_deadline(&self.session_id, deadline)
            .map_err(|error| {
                format!(
                    "failed to reread MCP cancellation audit session {}: {error}",
                    self.session_id
                )
            })?
            .ok_or_else(|| {
                format!(
                    "MCP cancellation audit session {} disappeared",
                    self.session_id
                )
            })
            .map(|session| {
                session.events.iter().any(|event| {
                    event.kind == "mcp_request_cancelled"
                        && event.details["cancellation_id"].as_str()
                            == Some(self.cancellation_id.as_str())
                })
            })
    }
}

impl Drop for McpCancellationAuditGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if !self.state.claim_fallback() {
            return;
        }
        if let Err(error) = self.state.finalize_fallback(json!({
            "tool_name": self.state.tool_name.clone(),
            "outcome": "unresolved",
            "reconciled": false,
            "effect_state": "unknown",
            "source": "drop_fallback",
        })) {
            eprintln!(
                "failed to persist fallback MCP cancellation audit for session {}: {error}",
                self.state.session_id
            );
        }
    }
}

impl CancellationAuditSlot {
    fn set(&self, state: Arc<McpCancellationAuditState>) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
        self.ready.notify_one();
    }

    fn get(&self) -> Option<Arc<McpCancellationAuditState>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn finalize_cancellation_audit_slot(
    slot: &SharedCancellationAuditSlot,
    required: bool,
    details: Value,
    deadline: std::time::Instant,
) -> Result<(), String> {
    let audit = slot.get();
    match audit {
        Some(audit) => audit.finalize_cancelled_until(details, deadline),
        None if required => Err("MCP cancellation audit was not initialized".to_string()),
        None => Ok(()),
    }
}

fn claim_cancellation_audit_slot(
    slot: &SharedCancellationAuditSlot,
    required: bool,
) -> Result<(), String> {
    let audit = slot.get();
    match audit {
        Some(audit) => audit.claim_cancellation(),
        None if required => Err("MCP cancellation audit was not initialized".to_string()),
        None => Ok(()),
    }
}

async fn finalize_cancellation_audit_slot_blocking(
    slot: SharedCancellationAuditSlot,
    required: bool,
    details: Value,
    deadline: std::time::Instant,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        finalize_cancellation_audit_slot(&slot, required, details, deadline)
    })
    .await
    .map_err(|error| format!("MCP cancellation audit worker failed: {error}"))?
}

fn cancellation_audit_tool_name(slot: &SharedCancellationAuditSlot) -> Option<String> {
    slot.get().as_ref().map(|audit| audit.tool_name.clone())
}

/// MCP owns stdin, so an approval prompt cannot safely read from it. Explicit
/// policy/configuration can still grant a call before this handler is reached.
struct DenyInteractiveApproval;

#[async_trait]
impl ApprovalHandler for DenyInteractiveApproval {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        ApprovalDecision::denied_by_policy(
            "interactive approval is unavailable over MCP stdio; add an allow policy",
        )
    }
}

pub async fn run_mcp_server(project_root: &Path) -> Result<(), String> {
    let config = load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    if !config.mcp.server_enabled {
        return Err("MCP server is disabled by configuration".to_string());
    }
    resolve_mcp_runtime(project_root, &config)?;

    serve_mcp_io(
        project_root.to_path_buf(),
        config,
        BufReader::new(tokio::io::stdin()),
    )
    .await
}

pub fn run_stdio_relay() -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    io::copy(&mut input, &mut output)
        .map_err(|error| format!("MCP stdout relay failed: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("MCP stdout relay flush failed: {error}"))
}

async fn serve_mcp_io<R>(
    project_root: PathBuf,
    config: NibConfig,
    mut reader: R,
) -> Result<(), String>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    let (mut response_writer, mut writer_failure_rx, mut written_rx) =
        OwnedResponseWriter::start().await?;

    let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundFrame>(16);
    let reader_task = tokio::spawn(async move {
        loop {
            match read_async_frame(&mut reader).await {
                Ok(Some(frame)) => {
                    if inbound_tx.send(InboundFrame::Frame(frame)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    let _ = inbound_tx.send(InboundFrame::Eof).await;
                    return;
                }
                Err(error) => {
                    let _ = inbound_tx
                        .send(InboundFrame::Failed(format!(
                            "failed to read MCP stdin frame: {error}"
                        )))
                        .await;
                    return;
                }
            }
        }
    });

    let project_root = Arc::new(project_root);
    let config = Arc::new(config);
    let (completion_tx, mut completion_rx) =
        mpsc::channel::<CompletedRequest>(MAX_ACTIVE_MCP_REQUESTS);
    let completion_notify = Arc::new(Notify::new());
    let missed_completions = Arc::new(StdMutex::new(Vec::<CompletedRequest>::new()));
    let missed_completion_overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut active = HashMap::<String, ActiveRequest>::new();
    let mut pending_response_ids = HashSet::<String>::new();
    let mut generation = 0_u64;

    let outcome = 'coordinator: loop {
        tokio::select! {
            biased;

            completion = completion_rx.recv() => {
                let Some(completion) = completion else {
                    break Err("MCP request completion channel closed".to_string());
                };
                let Some(request) = take_completed_request(&mut active, &completion) else {
                    continue;
                };
                if let Err(error) = publish_completed_request(
                    request,
                    response_writer.sender(),
                    &mut pending_response_ids,
                ).await {
                    break Err(error);
                }
            }
            _ = completion_notify.notified() => {
                tokio::task::yield_now().await;
                if missed_completion_overflow.load(std::sync::atomic::Ordering::Acquire) {
                    break Err(
                        "MCP missed-completion queue exceeded its bounded capacity".to_string()
                    );
                }
                for request in take_ready_missed_requests(&mut active, &missed_completions) {
                    if let Err(error) = publish_completed_request(
                        request,
                        response_writer.sender(),
                        &mut pending_response_ids,
                    ).await {
                        break 'coordinator Err(error);
                    }
                }
                if !missed_completions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
                {
                    completion_notify.notify_one();
                }
            }
            Some(error) = writer_failure_rx.recv() => {
                break Err(error);
            }
            Some(request_key) = written_rx.recv() => {
                pending_response_ids.remove(&request_key);
            }
            inbound = inbound_rx.recv() => {
                let Some(inbound) = inbound else {
                    break Err("MCP stdin reader stopped unexpectedly".to_string());
                };
                let frame = match inbound {
                    InboundFrame::Frame(frame) => frame,
                    InboundFrame::Eof => break Ok(()),
                    InboundFrame::Failed(error) => break Err(error),
                };
                if frame.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }

                let request = match serde_json::from_slice::<Value>(&frame) {
                    Ok(request) => request,
                    Err(error) => {
                        let response = rpc_error(
                            Value::Null,
                            -32700,
                            format!("Parse error: {error}"),
                        );
                        if let Err(error) = queue_response(
                            response_writer.sender(),
                            &mut pending_response_ids,
                            &response,
                            None,
                        ) {
                            break Err(error);
                        }
                        continue;
                    }
                };

                if let Some(key) = cancellation_target(&request) {
                    let Some(current) = active.get(&key) else {
                        continue;
                    };
                    if current.task_kind == ActiveTaskKind::Cancellation {
                        continue;
                    }
                    if current.cancellation_class.subagent_tool_name().is_some() {
                        request_subagent_cancellation(current);
                        continue;
                    }

                    let active_request = active
                        .remove(&key)
                        .expect("current cancellation target remains active");
                    active_request.cancellation.cancel();
                    generation = generation.wrapping_add(1);
                    let cancellation_generation = generation;
                    let target_id = active_request.id.clone();
                    let lifecycle = Arc::clone(&active_request.lifecycle);
                    let task_lifecycle = Arc::clone(&lifecycle);
                    let cancellation_class = active_request.cancellation_class.clone();
                    let cancellation_audit = Arc::clone(&active_request.cancellation_audit);
                    let cancellation = active_request.cancellation.clone();
                    let request_key = key.clone();
                    let request_completion_tx = completion_tx.clone();
                    let request_completion_notify = Arc::clone(&completion_notify);
                    let request_missed_completions = Arc::clone(&missed_completions);
                    let request_missed_completion_overflow =
                        Arc::clone(&missed_completion_overflow);
                    let cancellation_id = target_id.clone();
                    let task = tokio::spawn(async move {
                        let _completion_guard = RequestCompletionGuard {
                            completion_tx: request_completion_tx,
                            missed_completions: request_missed_completions,
                            missed_completion_overflow: request_missed_completion_overflow,
                            completion_notify: request_completion_notify,
                            completion: Some(CompletedRequest {
                                key: request_key,
                                generation: cancellation_generation,
                            }),
                        };
                        let outcome = cancel_active_request(active_request).await;
                        reconcile_cancellation_worker_outcome(
                            &task_lifecycle,
                            &cancellation_id,
                            outcome,
                        );
                    });
                    active.insert(
                        key,
                        ActiveRequest {
                            generation: cancellation_generation,
                            id: target_id,
                            task_kind: ActiveTaskKind::Cancellation,
                            lifecycle,
                            cancellation_class,
                            cancellation_audit,
                            cancellation,
                            task,
                        },
                    );
                    continue;
                }

                let Some(id) = request.get("id").cloned() else {
                    // Lifecycle and unknown notifications intentionally receive no response.
                    continue;
                };
                let key = request_key(&id);
                if request_id_is_owned(&active, &pending_response_ids, &key) {
                    // A duplicate cannot cancel the owner or create a second response for its ID.
                    continue;
                }
                if active.len() >= MAX_ACTIVE_MCP_REQUESTS {
                    let response = rpc_error(id, -32000, "Too many active MCP requests");
                    if let Err(error) = queue_response(
                        response_writer.sender(),
                        &mut pending_response_ids,
                        &response,
                        Some(key),
                    ) {
                        break Err(error);
                    }
                    continue;
                }

                generation = generation.wrapping_add(1);
                let request_generation = generation;
                let request_key = key.clone();
                let request_root = Arc::clone(&project_root);
                let request_config = Arc::clone(&config);
                let request_completion_tx = completion_tx.clone();
                let request_completion_notify = Arc::clone(&completion_notify);
                let request_missed_completions = Arc::clone(&missed_completions);
                let request_missed_completion_overflow =
                    Arc::clone(&missed_completion_overflow);
                let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Running));
                let task_lifecycle = Arc::clone(&lifecycle);
                let cancellation = crate::agent::CancellationSignal::new();
                let task_cancellation = cancellation.clone();
                let cancellation_class = classify_request_cancellation(&request);
                let task_cancellation_class = cancellation_class.clone();
                let task_root = Arc::clone(&project_root);
                let cancellation_audit = Arc::new(CancellationAuditSlot::default());
                let task_cancellation_audit = Arc::clone(&cancellation_audit);
                let task = tokio::spawn(async move {
                    let _completion_guard = RequestCompletionGuard {
                        completion_tx: request_completion_tx,
                        missed_completions: request_missed_completions,
                        missed_completion_overflow: request_missed_completion_overflow,
                        completion_notify: request_completion_notify,
                        completion: Some(CompletedRequest {
                            key: request_key,
                            generation: request_generation,
                        }),
                    };
                    SessionStore::with_lock_policy(
                        MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT,
                        async move {
                            let handled = handle_request_with_cancellation(
                                request_root.as_path(),
                                request_config.as_ref(),
                                request,
                                Some(&task_cancellation),
                                &task_cancellation_audit,
                            )
                            .await;
                            finish_request_lifecycle_async(
                                &task_lifecycle,
                                request_generation,
                                task_root.as_path(),
                                &task_cancellation_class,
                                handled,
                            )
                            .await;
                        },
                    )
                    .await;
                });
                active.insert(
                    key,
                    ActiveRequest {
                        generation: request_generation,
                        id,
                        task_kind: ActiveTaskKind::Execution,
                        lifecycle,
                        cancellation_class,
                        cancellation_audit,
                        cancellation,
                        task,
                    },
                );
            }
        }
    };

    let shutdown_result = cancel_all_requests(&mut active).await;
    reader_task.abort();
    let _ = reader_task.await;
    response_writer.shutdown().await;
    merge_server_shutdown_result(outcome, shutdown_result)
}

fn merge_server_shutdown_result(
    outcome: Result<(), String>,
    shutdown_result: Result<(), String>,
) -> Result<(), String> {
    match (outcome, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(shutdown_error)) => Err(format!(
            "{error}; MCP request shutdown failed: {shutdown_error}"
        )),
    }
}

fn cancellation_target(request: &Value) -> Option<String> {
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || request.get("id").is_some()
        || request.get("method").and_then(Value::as_str) != Some("notifications/cancelled")
    {
        return None;
    }
    let id = request.get("params")?.get("requestId")?.clone();
    Some(request_key(&id))
}

fn request_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".to_string())
}

fn request_id_is_owned(
    active: &HashMap<String, ActiveRequest>,
    pending_response_ids: &HashSet<String>,
    key: &str,
) -> bool {
    active.contains_key(key) || pending_response_ids.contains(key)
}

fn take_completed_request(
    active: &mut HashMap<String, ActiveRequest>,
    completion: &CompletedRequest,
) -> Option<ActiveRequest> {
    let is_current = active
        .get(&completion.key)
        .is_some_and(|request| request.generation == completion.generation);
    is_current.then(|| active.remove(&completion.key)).flatten()
}

fn store_missed_completion(
    missed_completions: &Arc<StdMutex<Vec<CompletedRequest>>>,
    overflow: &Arc<std::sync::atomic::AtomicBool>,
    notify: &Arc<Notify>,
    completion: CompletedRequest,
) {
    let mut missed = missed_completions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if missed.len() >= MAX_ACTIVE_MCP_REQUESTS {
        overflow.store(true, std::sync::atomic::Ordering::Release);
    } else {
        missed.push(completion);
    }
    drop(missed);
    notify.notify_one();
}

fn take_ready_missed_requests(
    active: &mut HashMap<String, ActiveRequest>,
    missed_completions: &Arc<StdMutex<Vec<CompletedRequest>>>,
) -> Vec<ActiveRequest> {
    let completions = {
        let mut missed = missed_completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *missed)
    };
    let mut ready_requests = Vec::new();
    let mut still_pending = Vec::new();
    for completion in completions {
        let ready = active.get(&completion.key).is_some_and(|request| {
            request.generation == completion.generation && request.task.is_finished()
        });
        if ready {
            ready_requests.push(
                take_completed_request(active, &completion)
                    .expect("ready missed completion is current"),
            );
        } else if active
            .get(&completion.key)
            .is_some_and(|request| request.generation == completion.generation)
        {
            still_pending.push(completion);
        }
    }
    *missed_completions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = still_pending;
    ready_requests
}

async fn publish_completed_request(
    request: ActiveRequest,
    response_tx: &mpsc::Sender<QueuedResponse>,
    pending_response_ids: &mut HashSet<String>,
) -> Result<(), String> {
    let ActiveRequest {
        id,
        lifecycle,
        task,
        ..
    } = request;
    let _ = task.await;
    if let Some(response) = completed_response(&lifecycle, &id) {
        queue_response(
            response_tx,
            pending_response_ids,
            &response,
            Some(request_key(&id)),
        )?;
    }
    Ok(())
}

fn subagent_start_tool(request: &Value) -> Option<&str> {
    if request.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    match request
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
    {
        Some(name @ ("nib_run" | "spawn_subagent" | "invoke_subagent")) => Some(name),
        _ => None,
    }
}

fn classify_request_cancellation(request: &Value) -> RequestCancellationClass {
    if let Some(tool_name) = subagent_start_tool(request) {
        return RequestCancellationClass::SubagentStart {
            tool_name: tool_name.to_string(),
        };
    }
    if request.get("method").and_then(Value::as_str) != Some("tools/call") {
        return RequestCancellationClass::Protocol;
    }
    let tool_name = request
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    if tool_name == "nib_get_status"
        || registry::get_permission_level(&tool_name) == Some(PermissionLevel::ReadOnly)
    {
        return RequestCancellationClass::ReadOnlyTool { tool_name };
    }
    if tool_name == "run_terminal"
        && request["params"]["arguments"]["background"].as_bool() != Some(true)
    {
        return RequestCancellationClass::InterruptibleTool { tool_name };
    }
    RequestCancellationClass::EffectUnknownTool { tool_name }
}

impl RequestCancellationClass {
    fn subagent_tool_name(&self) -> Option<&str> {
        match self {
            Self::SubagentStart { tool_name } => Some(tool_name),
            _ => None,
        }
    }

    fn tool_name(&self) -> Option<&str> {
        match self {
            Self::Protocol => None,
            Self::ReadOnlyTool { tool_name }
            | Self::InterruptibleTool { tool_name }
            | Self::EffectUnknownTool { tool_name }
            | Self::SubagentStart { tool_name } => Some(tool_name),
        }
    }
}

#[cfg(test)]
fn finish_request_lifecycle(
    lifecycle: &SharedRequestLifecycle,
    generation: u64,
    project_root: &Path,
    cancellation_class: &RequestCancellationClass,
    mut handled: HandledRequest,
) {
    let Some(subagent_tool_name) =
        begin_request_lifecycle_finish(lifecycle, generation, cancellation_class, &mut handled)
    else {
        return;
    };
    let publication = reconcile_started_subagent(project_root, subagent_tool_name, &mut handled);
    publish_request_lifecycle_reconciliation(lifecycle, generation, publication);
}

async fn finish_request_lifecycle_async(
    lifecycle: &SharedRequestLifecycle,
    generation: u64,
    project_root: &Path,
    cancellation_class: &RequestCancellationClass,
    mut handled: HandledRequest,
) {
    let Some(subagent_tool_name) =
        begin_request_lifecycle_finish(lifecycle, generation, cancellation_class, &mut handled)
    else {
        return;
    };
    let publication =
        reconcile_started_subagent_async(project_root, subagent_tool_name, &mut handled).await;
    publish_request_lifecycle_reconciliation(lifecycle, generation, publication);
}

fn begin_request_lifecycle_finish<'a>(
    lifecycle: &SharedRequestLifecycle,
    generation: u64,
    cancellation_class: &'a RequestCancellationClass,
    handled: &mut HandledRequest,
) -> Option<&'a str> {
    let subagent_tool_name = cancellation_class.subagent_tool_name();
    let mut state = lock_request_lifecycle(lifecycle);
    match &*state {
        RequestLifecycle::Running => {
            handled.complete_audit();
            *state = RequestLifecycle::Completed(handled.response.take());
            None
        }
        RequestLifecycle::CancelRequested
            if subagent_tool_name.is_some() && handled.cancellation_audit.is_some() =>
        {
            *state = RequestLifecycle::Reconciling { generation };
            subagent_tool_name
        }
        RequestLifecycle::CancelRequested => {
            handled.complete_audit();
            *state = RequestLifecycle::Completed(handled.response.take());
            None
        }
        RequestLifecycle::Completed(_) | RequestLifecycle::CancellationFailed { .. } => {
            handled.complete_audit();
            None
        }
        RequestLifecycle::Reconciling { .. } | RequestLifecycle::Cancelled => {
            handled.disarm_audit();
            None
        }
    }
}

fn publish_request_lifecycle_reconciliation(
    lifecycle: &SharedRequestLifecycle,
    generation: u64,
    publication: RequestLifecycle,
) {
    let mut state = lock_request_lifecycle(lifecycle);
    if matches!(
        &*state,
        RequestLifecycle::Reconciling {
            generation: current
        } if *current == generation
    ) {
        *state = publication;
    }
}

#[cfg(test)]
fn reconcile_started_subagent(
    project_root: &Path,
    tool_name: &str,
    handled: &mut HandledRequest,
) -> RequestLifecycle {
    #[cfg(test)]
    pause_at_reconciliation_barrier(handled.response.as_ref());
    let outcome = cancel_started_subagent(project_root, tool_name, handled.response.as_ref());
    finish_started_subagent_reconciliation(handled, outcome)
}

async fn reconcile_started_subagent_async(
    project_root: &Path,
    tool_name: &str,
    handled: &mut HandledRequest,
) -> RequestLifecycle {
    #[cfg(test)]
    pause_at_reconciliation_barrier(handled.response.as_ref());
    let outcome =
        cancel_started_subagent_async(project_root, tool_name, handled.response.as_ref()).await;
    finish_started_subagent_reconciliation(handled, outcome)
}

fn finish_started_subagent_reconciliation(
    handled: &mut HandledRequest,
    outcome: SubagentCancellationOutcome,
) -> RequestLifecycle {
    let response_id = handled
        .response
        .as_ref()
        .and_then(|response| response.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    match outcome {
        SubagentCancellationOutcome::Terminal => {
            handled.complete_audit();
            RequestLifecycle::Completed(handled.response.take())
        }
        SubagentCancellationOutcome::Cancelled(details) => {
            match handled.finalize_cancellation_audit(details) {
                Ok(()) => RequestLifecycle::Cancelled,
                Err(error) => {
                    let error = format!("MCP cancellation audit failed: {error}");
                    RequestLifecycle::CancellationFailed {
                        response: rpc_error(response_id, -32603, error.clone()),
                        error,
                    }
                }
            }
        }
        SubagentCancellationOutcome::Unresolved { details, error } => {
            let error = match handled.finalize_cancellation_audit(details) {
                Ok(()) => error,
                Err(audit_error) => {
                    format!("{error}; MCP cancellation audit failed: {audit_error}")
                }
            };
            RequestLifecycle::CancellationFailed {
                response: rpc_error(response_id, -32603, error.clone()),
                error,
            }
        }
    }
}

#[cfg(test)]
fn pause_at_reconciliation_barrier(response: Option<&Value>) {
    let Some(subagent_id) = response
        .and_then(|response| response["result"]["structuredContent"]["subagent_id"].as_str())
    else {
        return;
    };
    let barrier = RECONCILIATION_BARRIER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(barrier) = barrier.filter(|barrier| barrier.subagent_id == subagent_id) else {
        return;
    };
    barrier
        .entered
        .store(true, std::sync::atomic::Ordering::Release);
    while !barrier.release.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
fn cancel_started_subagent(
    project_root: &Path,
    tool_name: &str,
    response: Option<&Value>,
) -> SubagentCancellationOutcome {
    let subagent_id = match started_subagent_cancellation_target(tool_name, response) {
        Ok(subagent_id) => subagent_id,
        Err(outcome) => return outcome,
    };
    started_subagent_cancellation_outcome(
        tool_name,
        subagent_id,
        crate::tools::delegation::resolve_subagent_cancellation(project_root, subagent_id),
    )
}

async fn cancel_started_subagent_async(
    project_root: &Path,
    tool_name: &str,
    response: Option<&Value>,
) -> SubagentCancellationOutcome {
    let subagent_id = match started_subagent_cancellation_target(tool_name, response) {
        Ok(subagent_id) => subagent_id,
        Err(outcome) => return outcome,
    };
    let resolution =
        crate::tools::delegation::resolve_subagent_cancellation_async(project_root, subagent_id)
            .await;
    started_subagent_cancellation_outcome(tool_name, subagent_id, resolution)
}

fn started_subagent_cancellation_target<'a>(
    tool_name: &str,
    response: Option<&'a Value>,
) -> Result<&'a str, SubagentCancellationOutcome> {
    let Some(response) = response else {
        return Err(SubagentCancellationOutcome::Unresolved {
            details: json!({
                "tool_name": tool_name,
                "outcome": "unresolved",
                "reconciled": false,
                "error": format!("{tool_name} produced no response"),
            }),
            error: format!("{tool_name} cancellation produced no response to reconcile"),
        });
    };
    if response.get("error").is_some() || response["result"]["isError"] == true {
        return Err(SubagentCancellationOutcome::Cancelled(json!({
            "tool_name": tool_name,
            "outcome": "cancelled",
            "reconciled": true,
            "phase": "precommit",
        })));
    }
    let Some(subagent_id) = response["result"]["structuredContent"]["subagent_id"].as_str() else {
        return Err(SubagentCancellationOutcome::Unresolved {
            details: json!({
                "tool_name": tool_name,
                "outcome": "unresolved",
                "reconciled": false,
                "error": format!("successful {tool_name} response omitted subagent_id"),
            }),
            error: format!("successful {tool_name} response omitted its authoritative subagent ID"),
        });
    };
    Ok(subagent_id)
}

fn started_subagent_cancellation_outcome(
    tool_name: &str,
    subagent_id: &str,
    resolution: crate::tools::delegation::CancelSubagentResolution,
) -> SubagentCancellationOutcome {
    match resolution {
        crate::tools::delegation::CancelSubagentResolution::Cancelled { record } => {
            SubagentCancellationOutcome::Cancelled(json!({
                "tool_name": tool_name,
                "subagent_id": record.id,
                "subagent_status": record.status,
                "outcome": "cancelled",
                "reconciled": true,
            }))
        }
        crate::tools::delegation::CancelSubagentResolution::Terminal { .. } => {
            SubagentCancellationOutcome::Terminal
        }
        crate::tools::delegation::CancelSubagentResolution::Unresolved {
            manager_stopped,
            observed_status,
            error,
        } => SubagentCancellationOutcome::Unresolved {
            details: json!({
                "tool_name": tool_name,
                "subagent_id": subagent_id,
                "manager_stopped": manager_stopped,
                "observed_status": observed_status,
                "outcome": "unresolved",
                "reconciled": false,
                "error": error.clone(),
            }),
            error: format!(
                "{tool_name} cancellation could not be reconciled for subagent {subagent_id}: {error}"
            ),
        },
    }
}

fn lock_request_lifecycle(
    lifecycle: &SharedRequestLifecycle,
) -> std::sync::MutexGuard<'_, RequestLifecycle> {
    lifecycle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn completed_response(lifecycle: &SharedRequestLifecycle, id: &Value) -> Option<Value> {
    match &*lock_request_lifecycle(lifecycle) {
        RequestLifecycle::Completed(response) => response.clone(),
        RequestLifecycle::CancellationFailed { response, .. } => Some(response.clone()),
        RequestLifecycle::Cancelled => Some(rpc_error(
            id.clone(),
            MCP_REQUEST_CANCELLED_CODE,
            "Request cancelled",
        )),
        RequestLifecycle::Running
        | RequestLifecycle::CancelRequested
        | RequestLifecycle::Reconciling { .. } => Some(rpc_error(
            id.clone(),
            -32603,
            "MCP request task ended without a reconciled outcome",
        )),
    }
}

fn request_subagent_cancellation(request: &ActiveRequest) {
    let mut lifecycle = lock_request_lifecycle(&request.lifecycle);
    if matches!(*lifecycle, RequestLifecycle::Running) {
        *lifecycle = RequestLifecycle::CancelRequested;
        request.cancellation.cancel();
    }
}

async fn wait_for_audit_or_task(
    slot: &SharedCancellationAuditSlot,
    task: &mut JoinHandle<()>,
    deadline: std::time::Instant,
) -> bool {
    loop {
        let notified = slot.ready.notified();
        if slot.get().is_some() {
            return false;
        }
        if task.is_finished() {
            let _ = task.await;
            return true;
        }
        tokio::select! {
            _ = notified => {}
            _ = &mut *task => return true,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return false,
        }
    }
}

fn cancellation_outcome(lifecycle: &SharedRequestLifecycle) -> CancellationOutcome {
    match &*lock_request_lifecycle(lifecycle) {
        RequestLifecycle::Completed(_) => CancellationOutcome::Completed,
        RequestLifecycle::CancellationFailed { error, .. } => {
            CancellationOutcome::Failed(error.clone())
        }
        RequestLifecycle::Cancelled => CancellationOutcome::Cancelled,
        RequestLifecycle::Running
        | RequestLifecycle::CancelRequested
        | RequestLifecycle::Reconciling { .. } => CancellationOutcome::Failed(
            "MCP request task ended without a reconciled outcome".to_string(),
        ),
    }
}

fn reconcile_cancellation_worker_outcome(
    lifecycle: &SharedRequestLifecycle,
    id: &Value,
    outcome: CancellationOutcome,
) {
    let CancellationOutcome::Failed(error) = outcome else {
        return;
    };
    let mut lifecycle = lock_request_lifecycle(lifecycle);
    if matches!(
        *lifecycle,
        RequestLifecycle::Running
            | RequestLifecycle::CancelRequested
            | RequestLifecycle::Reconciling { .. }
    ) {
        *lifecycle = RequestLifecycle::CancellationFailed {
            response: rpc_error(id.clone(), -32603, error.clone()),
            error,
        };
    }
}

async fn cancel_active_request(mut request: ActiveRequest) -> CancellationOutcome {
    if request.cancellation_class.subagent_tool_name().is_some() {
        let terminal = {
            let mut lifecycle = lock_request_lifecycle(&request.lifecycle);
            match &*lifecycle {
                RequestLifecycle::Completed(_) => Some(CancellationOutcome::Completed),
                RequestLifecycle::CancellationFailed { error, .. } => {
                    Some(CancellationOutcome::Failed(error.clone()))
                }
                RequestLifecycle::Cancelled => Some(CancellationOutcome::Cancelled),
                RequestLifecycle::Running => {
                    *lifecycle = RequestLifecycle::CancelRequested;
                    None
                }
                RequestLifecycle::CancelRequested | RequestLifecycle::Reconciling { .. } => None,
            }
        };
        if terminal.is_none() {
            request.cancellation.cancel();
        }
        return join_or_handoff_subagent_request(request, terminal).await;
    }

    let terminal = {
        let lifecycle = lock_request_lifecycle(&request.lifecycle);
        match &*lifecycle {
            RequestLifecycle::Completed(_) => Some(CancellationOutcome::Completed),
            RequestLifecycle::CancellationFailed { error, .. } => {
                Some(CancellationOutcome::Failed(error.clone()))
            }
            RequestLifecycle::Cancelled => Some(CancellationOutcome::Cancelled),
            RequestLifecycle::Running
            | RequestLifecycle::CancelRequested
            | RequestLifecycle::Reconciling { .. } => None,
        }
    };
    if let Some(outcome) = terminal {
        let _ = request.task.await;
        return outcome;
    }

    let reconciliation_deadline = std::time::Instant::now() + MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT;

    if request.cancellation_audit.get().is_none() {
        request.cancellation.cancel();
        if wait_for_audit_or_task(
            &request.cancellation_audit,
            &mut request.task,
            reconciliation_deadline,
        )
        .await
        {
            return cancellation_outcome(&request.lifecycle);
        }
    }

    let owns_reconciliation = {
        let mut lifecycle = lock_request_lifecycle(&request.lifecycle);
        match &*lifecycle {
            RequestLifecycle::Completed(_) | RequestLifecycle::CancellationFailed { .. } => false,
            RequestLifecycle::Cancelled => false,
            RequestLifecycle::Running | RequestLifecycle::CancelRequested => {
                *lifecycle = RequestLifecycle::Reconciling {
                    generation: request.generation,
                };
                true
            }
            RequestLifecycle::Reconciling { .. } => false,
        }
    };
    if !owns_reconciliation {
        let _ = request.task.await;
        return cancellation_outcome(&request.lifecycle);
    }
    let audit_claim = claim_cancellation_audit_slot(&request.cancellation_audit, true);
    request.cancellation.cancel();
    request.task.abort();
    let _ = request.task.await;

    let tool_name = request
        .cancellation_class
        .tool_name()
        .map(str::to_string)
        .or_else(|| cancellation_audit_tool_name(&request.cancellation_audit))
        .unwrap_or_else(|| "unknown".to_string());
    let effect_unknown = matches!(
        request.cancellation_class,
        RequestCancellationClass::EffectUnknownTool { .. }
    );
    let details = if effect_unknown {
        json!({
            "tool_name": tool_name,
            "outcome": "unresolved",
            "reconciled": false,
            "effect_state": "unknown",
            "source": "request_cancellation",
        })
    } else {
        json!({
            "tool_name": tool_name,
            "outcome": "cancelled",
            "reconciled": true,
            "effect_state": if matches!(request.cancellation_class, RequestCancellationClass::ReadOnlyTool { .. }) {
                "none"
            } else {
                "terminated"
            },
            "source": "request_cancellation",
        })
    };
    let audit_result = match audit_claim {
        Ok(()) => {
            finalize_cancellation_audit_slot_blocking(
                Arc::clone(&request.cancellation_audit),
                true,
                details,
                reconciliation_deadline,
            )
            .await
        }
        Err(error) => Err(error),
    };

    let mut lifecycle = lock_request_lifecycle(&request.lifecycle);
    if matches!(
        &*lifecycle,
        RequestLifecycle::Reconciling { generation } if *generation == request.generation
    ) {
        *lifecycle = if effect_unknown {
            let mut error = format!(
                "cancellation interrupted effectful tool '{tool_name}' after dispatch; effect state is unknown"
            );
            if let Err(audit_error) = audit_result {
                error.push_str(&format!("; MCP cancellation audit failed: {audit_error}"));
            }
            RequestLifecycle::CancellationFailed {
                response: rpc_error(request.id.clone(), -32603, error.clone()),
                error,
            }
        } else {
            match audit_result {
                Ok(()) => RequestLifecycle::Cancelled,
                Err(error) => {
                    let error = format!("MCP cancellation audit failed: {error}");
                    RequestLifecycle::CancellationFailed {
                        response: rpc_error(request.id.clone(), -32603, error.clone()),
                        error,
                    }
                }
            }
        };
    }
    drop(lifecycle);
    cancellation_outcome(&request.lifecycle)
}

async fn join_or_handoff_subagent_request(
    mut request: ActiveRequest,
    terminal: Option<CancellationOutcome>,
) -> CancellationOutcome {
    match tokio::time::timeout(MCP_SUBAGENT_SHUTDOWN_HANDOFF_TIMEOUT, &mut request.task).await {
        Ok(_) => terminal.unwrap_or_else(|| cancellation_outcome(&request.lifecycle)),
        Err(_) => {
            let request_id = request.id.clone();
            let lifecycle = Arc::clone(&request.lifecycle);
            let mut task = request.task;
            tokio::spawn(async move {
                if tokio::time::timeout(MCP_SUBAGENT_SHUTDOWN_HANDOFF_TIMEOUT, &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                    let _ = task.await;
                }
                if let CancellationOutcome::Failed(error) = cancellation_outcome(&lifecycle) {
                    eprintln!(
                        "MCP subagent cancellation handoff finished without reconciliation: {error}"
                    );
                }
            });
            CancellationOutcome::Failed(format!(
                "MCP subagent request {request_id} did not stop within {} seconds; durable cancellation reconciliation was handed off",
                MCP_SUBAGENT_SHUTDOWN_HANDOFF_TIMEOUT.as_secs_f64()
            ))
        }
    }
}

async fn cancel_all_requests(active: &mut HashMap<String, ActiveRequest>) -> Result<(), String> {
    let requests = active
        .drain()
        .map(|(_, request)| request)
        .collect::<Vec<_>>();
    for request in &requests {
        if request.task_kind == ActiveTaskKind::Execution {
            request.cancellation.cancel();
        }
    }
    let mut cancellations = tokio::task::JoinSet::new();
    for request in requests {
        cancellations.spawn(async move {
            match request.task_kind {
                ActiveTaskKind::Execution => cancel_active_request(request).await,
                ActiveTaskKind::Cancellation => match request.task.await {
                    Ok(()) => cancellation_outcome(&request.lifecycle),
                    Err(error) => CancellationOutcome::Failed(format!(
                        "MCP cancellation reconciliation task failed: {error}"
                    )),
                },
            }
        });
    }

    let mut failures = Vec::new();
    while let Some(joined) = cancellations.join_next().await {
        let outcome = match joined {
            Ok(outcome) => outcome,
            Err(error) => CancellationOutcome::Failed(format!(
                "MCP shutdown cancellation task failed: {error}"
            )),
        };
        if let CancellationOutcome::Failed(error) = outcome {
            failures.push(error);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn queue_response(
    response_tx: &mpsc::Sender<QueuedResponse>,
    pending_response_ids: &mut HashSet<String>,
    response: &Value,
    owned_request_key: Option<String>,
) -> Result<(), String> {
    if owned_request_key
        .as_ref()
        .is_some_and(|key| pending_response_ids.contains(key))
    {
        return Err("attempted to queue more than one response for an MCP request ID".to_string());
    }
    let response_frame = bounded_response_frame(response)?;
    response_tx
        .try_send(QueuedResponse {
            frame: response_frame,
            request_key: owned_request_key.clone(),
        })
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                "MCP stdout response queue exceeded its bounded capacity".to_string()
            }
            mpsc::error::TrySendError::Closed(_) => "MCP stdout writer closed".to_string(),
        })?;
    if let Some(key) = owned_request_key {
        pending_response_ids.insert(key);
    }
    Ok(())
}

fn bounded_response_frame(response: &Value) -> Result<Vec<u8>, String> {
    match encode_json_line(response) {
        Ok(frame) => Ok(frame),
        Err(error) => {
            let id = response.get("id").cloned().unwrap_or(Value::Null);
            encode_json_line(&rpc_error(
                id,
                -32603,
                format!("MCP response exceeds the bounded stdio frame: {error}"),
            ))
            .map_err(|fallback_error| {
                format!("failed to encode bounded MCP error response: {fallback_error}")
            })
        }
    }
}

pub async fn handle_request(
    project_root: &Path,
    config: &NibConfig,
    request: Value,
) -> Option<Value> {
    let cancellation_audit = Arc::new(CancellationAuditSlot::default());
    let mut handled = SessionStore::with_lock_policy(
        MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT,
        handle_request_with_cancellation(project_root, config, request, None, &cancellation_audit),
    )
    .await;
    handled.complete_audit();
    handled.response
}

async fn handle_request_with_cancellation(
    project_root: &Path,
    config: &NibConfig,
    request: Value,
    cancellation: Option<&crate::agent::CancellationSignal>,
    cancellation_audit_slot: &SharedCancellationAuditSlot,
) -> HandledRequest {
    let has_id = request.get("id").is_some();
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return HandledRequest::without_audit(
            has_id.then(|| rpc_error(id, -32600, "Invalid Request")),
        );
    }
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return HandledRequest::without_audit(
            has_id.then(|| rpc_error(id, -32600, "Invalid Request")),
        );
    };

    if !has_id {
        // MCP lifecycle notifications intentionally do not receive responses.
        return HandledRequest::without_audit(None);
    }

    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let public_sensitive_values = config.public_session_sensitive_values();
    let mut cancellation_audit = None;
    let response = Some(match method {
        "initialize" => rpc_result(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {
                    "name": "nib-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({"tools": advertised_tools()})),
        "tools/call" => match parse_tool_call(&params) {
            Ok((name, arguments)) => match prepare_tool_call(name, arguments) {
                Ok(call) => {
                    let (result, audit) = call_tool(
                        project_root,
                        config,
                        call,
                        cancellation,
                        cancellation_audit_slot,
                    )
                    .await;
                    cancellation_audit = audit;
                    rpc_result(id, tool_result_content(result))
                }
                Err(error) => rpc_error(
                    id,
                    -32602,
                    safe_mcp_validation_error(&error, &public_sensitive_values),
                ),
            },
            Err(error) => rpc_error(
                id,
                -32602,
                safe_mcp_validation_error(&error, &public_sensitive_values),
            ),
        },
        _ => rpc_error(id, -32601, "Method not found"),
    });
    HandledRequest {
        response,
        cancellation_audit,
    }
}

fn advertised_tools() -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "nib_run",
            "description": "Start a linked nib agent run through the gated executor.",
            "inputSchema": nib_run_input_schema()
        }),
        json!({
            "name": "nib_get_status",
            "description": "Return the persisted status for one nib agent run.",
            "inputSchema": nib_status_input_schema()
        }),
    ];
    tools.extend(
        registry::list_tools()
            .into_iter()
            .filter(|metadata| metadata.mcp_exposable)
            .map(|metadata| {
                json!({
                    "name": metadata.name,
                    "description": metadata.description,
                    "inputSchema": metadata.input_schema,
                    "annotations": {
                        "permissionLevel": format!("{:?}", metadata.permission_level).to_lowercase(),
                        "requiresApproval": metadata.requires_approval,
                        "requiresWorktree": metadata.requires_worktree
                    }
                })
            }),
    );
    tools.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    tools
}

fn nib_run_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal": {"type": "string", "minLength": 1, "maxLength": 20000},
            "max_steps": {"type": "integer", "minimum": 1, "maximum": 100}
        },
        "required": ["goal"],
        "additionalProperties": false
    })
}

fn nib_status_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": {"type": "string", "minLength": 1, "maxLength": 128}
        },
        "required": ["session_id"],
        "additionalProperties": false
    })
}

fn parse_tool_call(params: &Value) -> Result<(String, Value), String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "tools/call requires a non-empty name".to_string())?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err("tools/call arguments must be an object".to_string());
    }
    Ok((name.to_string(), arguments))
}

fn prepare_tool_call(
    requested_name: String,
    mut arguments: Value,
) -> Result<PreparedToolCall, String> {
    let (executor_name, schema, requested_status_id) = match requested_name.as_str() {
        "nib_run" => ("spawn_subagent".to_string(), nib_run_input_schema(), None),
        "nib_get_status" => (
            "manage_subagents".to_string(),
            nib_status_input_schema(),
            arguments
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        name => {
            let metadata = registry::get_tool_metadata(name)
                .filter(|tool| tool.mcp_exposable)
                .ok_or_else(|| "tool is not advertised by nib MCP".to_string())?;
            (name.to_string(), metadata.input_schema.clone(), None)
        }
    };
    validate_mcp_tool_arguments(&requested_name, &schema, &arguments)?;

    match requested_name.as_str() {
        "nib_run" => {
            let goal = arguments
                .get("goal")
                .and_then(Value::as_str)
                .expect("nib_run schema requires a string goal");
            let mut mapped = json!({"prompt": goal});
            if let Some(max_steps) = arguments.get("max_steps").cloned() {
                mapped["max_steps"] = max_steps;
            }
            arguments = mapped;
        }
        "nib_get_status" => arguments = json!({"action": "list"}),
        _ => {}
    }

    Ok(PreparedToolCall {
        requested_name,
        executor_name,
        arguments,
        requested_status_id,
    })
}

fn validate_mcp_tool_arguments(
    tool_name: &str,
    schema: &Value,
    arguments: &Value,
) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("invalid input schema for tool '{tool_name}': {error}"))?;
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
        Err(format!(
            "invalid arguments for tool '{tool_name}': {}",
            errors.join("; ")
        ))
    }
}

fn safe_mcp_validation_error(error: &str, sensitive_values: &[String]) -> String {
    crate::interactive::bounded_public_text(
        error,
        sensitive_values,
        MAX_MCP_VALIDATION_ERROR_BYTES,
        false,
    )
}

async fn call_tool(
    project_root: &Path,
    config: &NibConfig,
    prepared: PreparedToolCall,
    cancellation: Option<&crate::agent::CancellationSignal>,
    cancellation_audit_slot: &SharedCancellationAuditSlot,
) -> (ToolResult, Option<McpCancellationAuditGuard>) {
    let PreparedToolCall {
        requested_name,
        executor_name,
        arguments,
        requested_status_id,
    } = prepared;
    if requested_name == "nib_get_status" {
        let session_id = requested_status_id
            .as_deref()
            .expect("nib_get_status schema requires a session id");
        if let Err(error) = config.validate_public_session_id(session_id) {
            return (invalid_tool_result(&requested_name, error), None);
        }
    }
    let runtime = match run_mcp_session_io(|| resolve_mcp_runtime(project_root, config)) {
        Ok(runtime) => runtime,
        Err(error) => return (invalid_tool_result(&requested_name, error), None),
    };
    let audit_session_id = uuid::Uuid::new_v4().to_string();
    let cancellation_audit = Arc::new(McpCancellationAuditState {
        session_store: runtime.session_store.clone(),
        session_id: audit_session_id.clone(),
        tool_name: requested_name.clone(),
        cancellation_id: uuid::Uuid::new_v4().to_string(),
        status: StdMutex::new(McpCancellationAuditStatus::Pending),
        #[cfg(test)]
        injected_failures: std::sync::atomic::AtomicUsize::new(0),
        #[cfg(test)]
        injected_post_commit_failures: std::sync::atomic::AtomicUsize::new(0),
    });
    cancellation_audit_slot.set(Arc::clone(&cancellation_audit));
    let cancellation_audit_guard = McpCancellationAuditGuard {
        state: cancellation_audit,
        armed: true,
    };
    let audit_session = match run_mcp_session_io(|| {
        runtime
            .session_store
            .try_create_session_with_id(audit_session_id)
    }) {
        Ok(session) => session,
        Err(error) => {
            return (
                invalid_tool_result(
                    &requested_name,
                    format!("failed to create MCP audit session: {error}"),
                ),
                Some(cancellation_audit_guard),
            )
        }
    };

    let mut executor = ToolExecutor::new(runtime.project_root.clone(), config.execution.clone())
        .with_terminal_config(&config.terminal)
        .with_approvals_config(&config.approvals)
        .with_session_store(runtime.session_store)
        .with_environment(&runtime.environment)
        .with_sensitive_values(config.public_session_sensitive_values())
        .with_approval_handler(std::sync::Arc::new(DenyInteractiveApproval));
    if let Some(cancellation) = cancellation {
        executor = executor.with_cancellation(cancellation.clone());
    }
    let mut result = SessionStore::with_lock_policy(
        MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT,
        executor.execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: executor_name,
                arguments,
                session_id: Some(audit_session.id.clone()),
                project_root: Some(runtime.project_root),
            },
            Some(&audit_session.id),
        ),
    )
    .await;

    result.tool_name = requested_name.clone();
    if requested_name == "nib_get_status" && result.success {
        let session_id = requested_status_id.expect("nib_get_status schema requires a session id");
        let matching = result
            .output
            .as_ref()
            .and_then(|output| output.get("subagents"))
            .and_then(Value::as_array)
            .and_then(|subagents| {
                subagents.iter().find(|record| {
                    record.get("id").and_then(Value::as_str) == Some(session_id.as_str())
                        || record.get("child_session_id").and_then(Value::as_str)
                            == Some(session_id.as_str())
                })
            })
            .cloned();
        result.output = Some(
            matching.unwrap_or_else(|| json!({"session_id": session_id, "status": "not_found"})),
        );
    }
    (result, Some(cancellation_audit_guard))
}

fn run_mcp_session_io<T>(operation: impl FnOnce() -> T) -> T {
    let can_block_in_place = tokio::runtime::Handle::try_current().is_ok_and(|handle| {
        matches!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        )
    });
    if can_block_in_place {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

fn resolve_mcp_runtime(project_root: &Path, config: &NibConfig) -> Result<McpRuntime, String> {
    let profiles = crate::profile::ProfileRegistry::load(project_root, &config.profiles)
        .map_err(|error| error.to_string())?;
    let profile = profiles
        .for_workspace(project_root)
        .unwrap_or_else(|| profiles.default_profile());
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;

    let session_store = SessionStore::for_project(project_root)?
        .with_lock_timeout(MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT);
    if session_store.sessions_dir() != profile.sessions_dir() {
        return Err(format!(
            "selected profile changed while initializing MCP runtime: expected {}, got {}",
            profile.sessions_dir().display(),
            session_store.sessions_dir().display()
        ));
    }

    Ok(McpRuntime {
        project_root: profile.root_path().to_path_buf(),
        session_store,
        environment: profile.custom_env().clone(),
    })
}

fn invalid_tool_result(tool_name: &str, error: impl Into<String>) -> ToolResult {
    ToolResult {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: tool_name.to_string(),
        success: false,
        output: None,
        error: Some(error.into()),
        duration_seconds: 0.0,
        approval_granted: false,
        approval_source: Some("validation".to_string()),
    }
}

struct BoundedOutputWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedOutputWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(8192)),
            limit,
        }
    }
}

impl Write for BoundedOutputWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP tool output exceeds its serialized byte limit",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_bounded_tool_output(output: &Value) -> Result<String, String> {
    let mut writer = BoundedOutputWriter::new(MAX_MCP_TOOL_OUTPUT_BYTES);
    serde_json::to_writer(&mut writer, output).map_err(|_| {
        format!("tool output exceeds the {MAX_MCP_TOOL_OUTPUT_BYTES}-byte serialized MCP limit")
    })?;
    String::from_utf8(writer.bytes)
        .map_err(|error| format!("tool output was not valid UTF-8 JSON: {error}"))
}

fn tool_result_content(result: ToolResult) -> Value {
    let (text, structured_content, is_error) = if result.success {
        match result.output {
            Some(output) => match serialize_bounded_tool_output(&output) {
                Ok(text) => (text, Some(output), false),
                Err(error) => (error, None, true),
            },
            None => ("null".to_string(), None, false),
        }
    } else {
        (
            result
                .error
                .unwrap_or_else(|| "tool execution failed".to_string()),
            None,
            true,
        )
    };
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured_content,
        "isError": is_error,
        "_meta": {
            "tool": result.tool_name,
            "approvalGranted": result.approval_granted,
            "approvalSource": result.approval_source,
            "durationSeconds": result.duration_seconds
        }
    })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        load_nib_config_full, save_nib_config_full, LlmApiMode, ProfileConfig, ProfilesConfig,
        ProviderEntry,
    };
    use crate::llm::test_support::serve_once;
    use std::pin::Pin;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};
    use tempfile::tempdir;

    const MANAGED_PROCESS_FIXTURE_CHILD_ENV: &str = "NIB_TEST_MCP_PROCESS_SCOPE_CHILD";
    const MANAGED_PROCESS_FIXTURE_CHILD_TEST: &str =
        "integrations::mcp_server::tests::managed_process_scope_fixture_child";

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

    struct ManagedProcessFixture {
        store: crate::sandbox::process::ProcessScopeStore,
        scope: crate::sandbox::process::ProcessScopeRecord,
        cleanup_lease: Option<crate::sandbox::process::CleanupLease>,
        child: Option<std::process::Child>,
    }

    impl ManagedProcessFixture {
        fn start(project_root: &Path, scope_id: &str, execution_generation: u64) -> Self {
            let store = crate::sandbox::process::ProcessScopeStore::open(project_root)
                .expect("managed-process fixture store");
            let prepared = store
                .prepare(
                    scope_id,
                    "subagent",
                    execution_generation,
                    crate::sandbox::process::ProcessIdentity::current()
                        .expect("managed-process fixture owner"),
                    native_process_scope_backend(),
                )
                .expect("prepare managed-process fixture");
            let cleanup_lease = store
                .acquire_cleanup_lease(&prepared)
                .expect("acquire managed-process fixture cleanup lease");
            let child = Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    MANAGED_PROCESS_FIXTURE_CHILD_TEST,
                    "--test-threads=1",
                ])
                .env(MANAGED_PROCESS_FIXTURE_CHILD_ENV, "1")
                .current_dir(project_root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn managed-process fixture child");
            let direct_child = crate::sandbox::process::ProcessIdentity::capture(child.id())
                .expect("capture managed-process fixture child");
            let mut fixture = Self {
                store,
                scope: prepared,
                cleanup_lease: Some(cleanup_lease),
                child: Some(child),
            };
            fixture.scope = fixture
                .store
                .mark_running(
                    scope_id,
                    execution_generation,
                    &fixture.scope.cleanup_lease_id,
                    crate::sandbox::process::ProcessIdentity::current()
                        .expect("managed-process fixture supervisor"),
                    direct_child,
                )
                .expect("mark managed-process fixture running");
            fixture
        }

        fn scope(&self) -> &crate::sandbox::process::ProcessScopeRecord {
            &self.scope
        }

        fn complete(mut self, outcome: &str) -> crate::sandbox::process::CleanupProof {
            let mut child = self
                .child
                .take()
                .expect("managed-process fixture child remains owned");
            match child.kill() {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
                Err(error) => panic!("kill managed-process fixture child: {error}"),
            }
            child.wait().expect("reap managed-process fixture child");
            // Waiting on the owned process handle proves this exact child
            // generation exited. A PID lookup can still find the exited
            // process object on Windows while another handle keeps it alive.
            drop(child);
            let direct_child = self
                .scope
                .direct_child
                .clone()
                .expect("managed-process fixture direct child");
            #[cfg(not(windows))]
            assert!(
                !direct_child.still_matches(),
                "cleanup proof requires the exact child generation to be gone"
            );

            let completed_at = chrono::Utc::now();
            let proof = crate::sandbox::process::CleanupProof {
                execution_generation: self.scope.execution_generation,
                cleanup_lease_id: self.scope.cleanup_lease_id.clone(),
                backend: self.scope.backend,
                direct_child,
                outcome: outcome.to_string(),
                descendants_reaped: true,
                completed_at,
            };
            let mut complete = self.scope.clone();
            complete.status = crate::sandbox::process::ProcessScopeStatus::Complete;
            complete.cleanup_reason = Some(outcome.to_string());
            complete.cleanup_proof = Some(proof.clone());
            complete.updated_at = completed_at;

            // The production supervisor owns this transition. The fixture
            // publishes the same CAS state only after reaping its controlled child.
            let directory_path = self
                .store
                .project_root()
                .join(".nib")
                .join("process-scopes");
            let directory = crate::daemons::state::StableDirectory::open(&directory_path)
                .expect("open managed-process fixture directory");
            let scope_path = directory_path.join(format!("{}.json", complete.scope_id));
            let opened = directory
                .open_read(&scope_path)
                .expect("open running managed-process fixture");
            directory
                .save_json_atomically_expected(
                    &scope_path,
                    &complete,
                    crate::daemons::state::FileExpectation::Present(&opened),
                )
                .expect("publish completed managed-process fixture");

            self.cleanup_lease
                .take()
                .expect("managed-process fixture cleanup lease remains owned")
                .release_after_proof(&proof)
                .expect("release managed-process fixture cleanup lease after proof");
            assert_eq!(
                self.store
                    .cleanup_lease_state(&complete)
                    .expect("inspect released managed-process fixture cleanup lease"),
                crate::sandbox::process::CleanupLeaseState::Missing
            );
            assert_eq!(
                self.store
                    .load(&complete.scope_id)
                    .expect("reload completed managed-process fixture"),
                complete
            );
            proof
        }
    }

    impl Drop for ManagedProcessFixture {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn native_process_scope_backend() -> crate::sandbox::process::ProcessScopeBackend {
        #[cfg(target_os = "linux")]
        {
            crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace
        }
        #[cfg(windows)]
        {
            crate::sandbox::process::ProcessScopeBackend::WindowsJobObject
        }
        #[cfg(target_os = "macos")]
        {
            crate::sandbox::process::ProcessScopeBackend::MacosProcessGroup
        }
        #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
        {
            panic!("managed-process fixtures are unsupported on this platform")
        }
    }

    #[test]
    fn managed_process_scope_fixture_child() {
        if std::env::var_os(MANAGED_PROCESS_FIXTURE_CHILD_ENV).is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    struct PendingWriter {
        polled: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl AsyncWrite for PendingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.polled.store(true, Ordering::Release);
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Drop for PendingWriter {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn owned_response_writer_cancels_and_joins_a_blocked_write() {
        let polled = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let (mut writer, _failure_rx, _written_rx) = OwnedResponseWriter::from_writer(
            PendingWriter {
                polled: Arc::clone(&polled),
                dropped: Arc::clone(&dropped),
            },
            None,
        );
        writer
            .sender()
            .try_send(QueuedResponse {
                frame: vec![b'x'; 1024],
                request_key: Some("blocked".to_string()),
            })
            .expect("queue blocked response");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !polled.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer reached blocked poll");

        tokio::time::timeout(std::time::Duration::from_secs(1), writer.shutdown())
            .await
            .expect("blocked writer shutdown is bounded");

        assert!(dropped.load(Ordering::Acquire));
        assert!(writer.task.is_none());
        assert!(writer.response_tx.is_none());
    }

    #[tokio::test]
    async fn response_id_remains_owned_until_writer_flush_ack() {
        let (mut writer, _failure_rx, mut written_rx) =
            OwnedResponseWriter::from_writer(tokio::io::sink(), None);
        let mut pending = HashSet::new();
        let key = request_key(&json!("flush-owned"));
        queue_response(
            writer.sender(),
            &mut pending,
            &rpc_result(json!("flush-owned"), json!({})),
            Some(key.clone()),
        )
        .expect("queue response");
        assert!(pending.contains(&key));
        assert!(request_id_is_owned(&HashMap::new(), &pending, &key));

        let acknowledged =
            tokio::time::timeout(std::time::Duration::from_secs(1), written_rx.recv())
                .await
                .expect("writer ack is bounded")
                .expect("writer ack channel remains open");
        assert_eq!(acknowledged, key);
        assert!(
            pending.contains(&key),
            "ack must be consumed by coordinator"
        );
        pending.remove(&acknowledged);
        assert!(!request_id_is_owned(&HashMap::new(), &pending, &key));

        queue_response(
            writer.sender(),
            &mut pending,
            &rpc_result(json!("flush-owned"), json!({"second": true})),
            Some(key),
        )
        .expect("ID can be reused only after flush ack");
        writer.shutdown().await;
    }

    #[tokio::test]
    async fn response_queue_accepts_full_completion_burst_and_keeps_parse_errors_unowned() {
        let (response_tx, mut response_rx) =
            mpsc::channel::<QueuedResponse>(MAX_ACTIVE_MCP_REQUESTS);
        let mut pending = HashSet::new();
        for index in 0..MAX_ACTIVE_MCP_REQUESTS {
            publish_completed_request(
                ActiveRequest {
                    generation: index as u64,
                    id: json!(index),
                    task_kind: ActiveTaskKind::Execution,
                    lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Completed(Some(
                        rpc_result(json!(index), json!({"index": index})),
                    )))),
                    cancellation_class: RequestCancellationClass::Protocol,
                    cancellation_audit: Arc::new(CancellationAuditSlot::default()),
                    cancellation: crate::agent::CancellationSignal::new(),
                    task: tokio::spawn(async {}),
                },
                &response_tx,
                &mut pending,
            )
            .await
            .expect("every admitted request has response capacity");
        }
        assert_eq!(pending.len(), MAX_ACTIVE_MCP_REQUESTS);
        for _ in 0..MAX_ACTIVE_MCP_REQUESTS {
            assert!(response_rx.try_recv().is_ok());
        }

        let parse_error = rpc_error(Value::Null, -32700, "parse error");
        queue_response(&response_tx, &mut pending, &parse_error, None)
            .expect("first parse error response");
        queue_response(&response_tx, &mut pending, &parse_error, None)
            .expect("second parse error response with id:null");
        assert_eq!(pending.len(), MAX_ACTIVE_MCP_REQUESTS);
        let null_key = request_key(&Value::Null);
        queue_response(
            &response_tx,
            &mut pending,
            &rpc_result(Value::Null, json!({"legitimate": true})),
            Some(null_key.clone()),
        )
        .expect("parse errors do not reserve a legitimate null request ID");
        assert!(pending.contains(&null_key));
    }

    #[tokio::test]
    async fn duplicate_active_or_pending_id_never_replaces_the_original_request() {
        let key = request_key(&json!("duplicate"));
        let task = tokio::spawn(std::future::pending::<()>());
        let mut active = HashMap::from([(
            key.clone(),
            ActiveRequest {
                generation: 7,
                id: json!("duplicate"),
                task_kind: ActiveTaskKind::Execution,
                lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Running)),
                cancellation_class: RequestCancellationClass::Protocol,
                cancellation_audit: Arc::new(CancellationAuditSlot::default()),
                cancellation: crate::agent::CancellationSignal::new(),
                task,
            },
        )]);
        assert!(request_id_is_owned(&active, &HashSet::new(), &key));
        assert_eq!(active[&key].generation, 7);

        let original = active.remove(&key).expect("original remains active");
        let pending = HashSet::from([key.clone()]);
        assert!(request_id_is_owned(&active, &pending, &key));
        original.task.abort();
        assert!(original
            .task
            .await
            .expect_err("test request cancellation")
            .is_cancelled());
    }

    #[tokio::test]
    async fn saturated_completion_fallback_is_level_triggered_and_hard_bounded() {
        let (completion_tx, _completion_rx) = mpsc::channel::<CompletedRequest>(1);
        completion_tx
            .try_send(CompletedRequest {
                key: "occupied".to_string(),
                generation: 0,
            })
            .expect("occupy completion channel");
        let missed = Arc::new(StdMutex::new(Vec::new()));
        let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let task_missed = Arc::clone(&missed);
        let task_overflow = Arc::clone(&overflow);
        let task_notify = Arc::clone(&notify);
        let task_release = Arc::clone(&release);
        let completion = CompletedRequest {
            key: request_key(&json!("missed")),
            generation: 3,
        };
        let task_completion = completion.clone();
        let task = tokio::spawn(async move {
            if completion_tx.try_send(task_completion.clone()).is_err() {
                store_missed_completion(
                    &task_missed,
                    &task_overflow,
                    &task_notify,
                    task_completion,
                );
            }
            task_release.notified().await;
        });
        let mut active = HashMap::from([(
            completion.key.clone(),
            ActiveRequest {
                generation: completion.generation,
                id: json!("missed"),
                task_kind: ActiveTaskKind::Execution,
                lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Completed(None))),
                cancellation_class: RequestCancellationClass::Protocol,
                cancellation_audit: Arc::new(CancellationAuditSlot::default()),
                cancellation: crate::agent::CancellationSignal::new(),
                task,
            },
        )]);

        notify.notified().await;
        assert!(take_ready_missed_requests(&mut active, &missed).is_empty());
        assert_eq!(
            missed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "wake-before-finish must retain the completion identity"
        );
        release.notify_one();
        while !active
            .get(&completion.key)
            .expect("request remains active")
            .task
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
        let mut ready = take_ready_missed_requests(&mut active, &missed);
        assert_eq!(ready.len(), 1);
        ready.pop().expect("ready request").task.await.unwrap();
        assert!(active.is_empty());

        for index in 0..=MAX_ACTIVE_MCP_REQUESTS {
            store_missed_completion(
                &missed,
                &overflow,
                &notify,
                CompletedRequest {
                    key: format!("overflow-{index}"),
                    generation: 1,
                },
            );
        }
        assert_eq!(
            missed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            MAX_ACTIVE_MCP_REQUESTS
        );
        assert!(overflow.load(std::sync::atomic::Ordering::Acquire));
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git starts");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn initialize_git_repository(root: &Path) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "nib-tests@example.invalid"]);
        git(root, &["config", "user.name", "nib tests"]);
        std::fs::write(root.join(".gitignore"), ".nib/\n").expect("gitignore");
        std::fs::write(root.join("README.md"), "fixture\n").expect("fixture");
        git(root, &["add", ".gitignore", "README.md"]);
        git(root, &["commit", "-qm", "initial"]);
    }

    fn save_profile_config(root: &Path) -> NibConfig {
        std::fs::write(
            root.join(".profile.env"),
            "NIB_PROFILE_VALUE=profile-scoped\n",
        )
        .expect("profile env");
        let mut config = NibConfig::default();
        config.execution.plan_mode = false;
        config.profiles = ProfilesConfig {
            default: "workspace".to_string(),
            active: vec![ProfileConfig {
                id: "workspace".to_string(),
                root: PathBuf::from("."),
                env_file: Some(PathBuf::from(".profile.env")),
                ..ProfileConfig::default()
            }],
        };
        save_nib_config_full(root, &mut config).expect("save config");
        config
    }

    fn test_cancellation_audit(
        store: &SessionStore,
        session_id: &str,
        tool_name: &str,
    ) -> (McpCancellationAuditGuard, Arc<McpCancellationAuditState>) {
        let state = Arc::new(McpCancellationAuditState {
            session_store: store.clone(),
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            cancellation_id: uuid::Uuid::new_v4().to_string(),
            status: StdMutex::new(McpCancellationAuditStatus::Pending),
            injected_failures: std::sync::atomic::AtomicUsize::new(0),
            injected_post_commit_failures: std::sync::atomic::AtomicUsize::new(0),
        });
        (
            McpCancellationAuditGuard {
                state: Arc::clone(&state),
                armed: true,
            },
            state,
        )
    }

    fn subagent_start_class(tool_name: &str) -> RequestCancellationClass {
        RequestCancellationClass::SubagentStart {
            tool_name: tool_name.to_string(),
        }
    }

    #[test]
    fn list_contains_aliases_and_registry_schemas() {
        let tools = advertised_tools();
        assert!(tools.iter().any(|tool| tool["name"] == "nib_run"));
        assert!(tools.iter().any(|tool| tool["name"] == "read_file"));
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool["name"] == "read_file")
                .unwrap()["inputSchema"]["required"][0],
            "path"
        );
    }

    fn tool_request(name: &str, arguments: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": name,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        })
    }

    #[tokio::test]
    async fn nib_get_status_projects_running_subagent_ownership_authority() {
        let root = tempdir().expect("MCP status project");
        let config = save_profile_config(root.path());
        let id = format!("sub-public-mcp-{}", uuid::Uuid::new_v4());
        let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
            .expect("live owner lease");
        crate::tools::delegation::write_subagent_record(
            root.path(),
            &crate::tools::delegation::SubagentRecord {
                id: id.clone(),
                parent_session_id: Some("parent".to_string()),
                child_session_id: id.clone(),
                prompt: "MCP public projection fixture".to_string(),
                status: "running".to_string(),
                execution_generation: Some(owner_lease.execution_generation()),
                owner_lease: Some(owner_lease.lease_id().to_string()),
                worktree_path: root.path().join("worktree"),
                branch: format!("nib/subagent/{id}"),
                branch_oid: None,
                result: Some(json!({
                    "_ownership_audit_target": {
                        "sessions_dir": root.path().join("private-mcp-audit-sessions"),
                        "directory_identity": "private-mcp-identity",
                    }
                })),
                error: None,
                verification: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        )
        .expect("running subagent record");

        let response = handle_request(
            root.path(),
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "public-status",
                "method": "tools/call",
                "params": {
                    "name": "nib_get_status",
                    "arguments": {"session_id": id},
                }
            }),
        )
        .await
        .expect("MCP status response");
        assert_eq!(response["result"]["isError"], false, "{response}");
        let public = &response["result"]["structuredContent"];
        assert_eq!(public["status"], "running");
        assert!(public["result"].is_null());
        assert!(public.get("execution_generation").is_none());
        assert!(public.get("owner_lease").is_none());
        let encoded = response.to_string();
        assert!(!encoded.contains("_ownership_audit_target"));
        assert!(!encoded.contains("private-mcp-audit-sessions"));
        assert!(!encoded.contains("private-mcp-identity"));
    }

    #[test]
    fn cancellation_classification_covers_all_subagent_aliases_and_effect_boundaries() {
        for name in ["nib_run", "spawn_subagent", "invoke_subagent"] {
            assert_eq!(
                subagent_start_tool(&tool_request(name, json!({}))),
                Some(name)
            );
            assert_eq!(
                classify_request_cancellation(&tool_request(name, json!({}))),
                RequestCancellationClass::SubagentStart {
                    tool_name: name.to_string(),
                }
            );
        }
        assert_eq!(
            classify_request_cancellation(&tool_request("read_file", json!({"path": "x"}))),
            RequestCancellationClass::ReadOnlyTool {
                tool_name: "read_file".to_string(),
            }
        );
        assert_eq!(
            classify_request_cancellation(&tool_request(
                "run_terminal",
                json!({"command": "sleep 1"}),
            )),
            RequestCancellationClass::InterruptibleTool {
                tool_name: "run_terminal".to_string(),
            }
        );
        assert_eq!(
            classify_request_cancellation(&tool_request(
                "run_terminal",
                json!({"command": "sleep 1", "background": true}),
            )),
            RequestCancellationClass::EffectUnknownTool {
                tool_name: "run_terminal".to_string(),
            }
        );
        assert_eq!(
            classify_request_cancellation(&tool_request("schedule", json!({}))),
            RequestCancellationClass::EffectUnknownTool {
                tool_name: "schedule".to_string(),
            }
        );
        assert_eq!(
            classify_request_cancellation(&json!({
                "jsonrpc": "2.0",
                "id": "ping",
                "method": "ping"
            })),
            RequestCancellationClass::Protocol
        );
        assert!(subagent_start_tool(&tool_request("spawn_subagents", json!({}))).is_none());
    }

    #[test]
    fn every_subagent_alias_uses_commit_aware_cancellation_reconciliation() {
        let root = tempdir().expect("alias cancellation repository");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        for tool_name in ["nib_run", "spawn_subagent", "invoke_subagent"] {
            let session = store.try_create_session().expect("audit session");
            let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
            let (audit, _state) = test_cancellation_audit(&store, &session.id, tool_name);
            finish_request_lifecycle(
                &lifecycle,
                1,
                root.path(),
                &subagent_start_class(tool_name),
                HandledRequest {
                    response: Some(rpc_error(json!(tool_name), -32602, "precommit failure")),
                    cancellation_audit: Some(audit),
                },
            );
            assert!(matches!(
                &*lock_request_lifecycle(&lifecycle),
                RequestLifecycle::Cancelled
            ));
            let session = store.load(&session.id).expect("audit session remains");
            let event = session
                .events
                .iter()
                .find(|event| event.kind == "mcp_request_cancelled")
                .expect("alias cancellation audit");
            assert_eq!(event.details["tool_name"], tool_name);
            assert_eq!(event.details["phase"], "precommit");
        }
    }

    #[tokio::test]
    async fn failed_subagent_audit_remains_a_shutdown_error() {
        let root = tempdir().expect("failed audit repository");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
        let (audit, state) = test_cancellation_audit(&store, &session.id, "nib_run");
        state.injected_failures.store(2, Ordering::Release);
        finish_request_lifecycle(
            &lifecycle,
            1,
            root.path(),
            &subagent_start_class("nib_run"),
            HandledRequest {
                response: Some(rpc_error(
                    json!("failed-audit"),
                    -32602,
                    "precommit failure",
                )),
                cancellation_audit: Some(audit),
            },
        );
        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::CancellationFailed { .. }
        ));
        let mut active = HashMap::from([(
            request_key(&json!("failed-audit")),
            ActiveRequest {
                generation: 1,
                id: json!("failed-audit"),
                task_kind: ActiveTaskKind::Execution,
                lifecycle,
                cancellation_class: subagent_start_class("nib_run"),
                cancellation_audit: Arc::new(CancellationAuditSlot::default()),
                cancellation: crate::agent::CancellationSignal::new(),
                task: tokio::spawn(async {}),
            },
        )]);
        let error = cancel_all_requests(&mut active)
            .await
            .expect_err("shutdown must propagate cancellation audit failure");
        assert!(error.contains("cancellation audit failed"), "{error}");
    }

    #[test]
    fn cancellation_reservation_can_persist_before_session_initialization() {
        let root = tempdir().expect("audit repository");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session_id = "reserved-before-initialization";
        let (mut guard, state) = test_cancellation_audit(&store, session_id, "read_file");

        state
            .claim_cancellation()
            .expect("claim cancellation reservation");
        state
            .finalize_cancelled(json!({
                "tool_name": "read_file",
                "outcome": "cancelled",
                "reconciled": true,
                "effect_state": "none",
            }))
            .expect("reservation creates its authoritative audit session");
        guard.armed = false;

        let initialized = store
            .try_create_session_with_id(session_id)
            .expect("normal initialization is idempotent after cancellation");
        let events = initialized
            .events
            .iter()
            .filter(|event| event.kind == "mcp_request_cancelled")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].details["tool_name"], "read_file");
    }

    #[test]
    fn cancellation_and_drop_fallback_claim_exactly_one_owner() {
        let root = tempdir().expect("audit repository");
        let store = SessionStore::for_project(root.path()).expect("profile session store");

        for index in 0..32 {
            let (mut guard, state) =
                test_cancellation_audit(&store, &format!("ownership-race-{index}"), "read_file");
            guard.armed = false;
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let explicit_state = Arc::clone(&state);
            let explicit_barrier = Arc::clone(&barrier);
            let explicit = std::thread::spawn(move || {
                explicit_barrier.wait();
                explicit_state.claim_cancellation().is_ok()
            });
            let fallback_state = Arc::clone(&state);
            let fallback_barrier = Arc::clone(&barrier);
            let fallback = std::thread::spawn(move || {
                fallback_barrier.wait();
                fallback_state.claim_fallback()
            });
            barrier.wait();
            let explicit_won = explicit.join().expect("explicit ownership thread");
            let fallback_won = fallback.join().expect("fallback ownership thread");
            assert_ne!(explicit_won, fallback_won, "iteration {index}");
            let status = *state
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                status,
                if explicit_won {
                    McpCancellationAuditStatus::CancellationOwned
                } else {
                    McpCancellationAuditStatus::FallbackOwned
                },
                "iteration {index}"
            );
        }

        let session = store.try_create_session().expect("audit session");
        let (guard, state) = test_cancellation_audit(&store, &session.id, "read_file");
        state
            .claim_cancellation()
            .expect("explicit cancellation owns audit");
        drop(guard);
        state
            .finalize_cancelled(json!({
                "tool_name": "read_file",
                "source": "explicit_test_owner",
            }))
            .expect("explicit owner persists precise details");
        let session = store.load(&session.id).expect("audit session remains");
        let event = session
            .events
            .iter()
            .find(|event| event.kind == "mcp_request_cancelled")
            .expect("explicit cancellation audit");
        assert_eq!(event.details["source"], "explicit_test_owner");
    }

    #[test]
    fn post_commit_audit_error_is_reconciled_without_duplicate_events() {
        let root = tempdir().expect("audit repository");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        let (guard, state) = test_cancellation_audit(&store, &session.id, "apply_patch");
        state
            .injected_post_commit_failures
            .store(1, Ordering::Release);

        let details = json!({
            "tool_name": "apply_patch",
            "outcome": "unresolved",
            "reconciled": false,
            "effect_state": "unknown",
        });
        state
            .finalize_cancelled(details.clone())
            .expect("authoritative reread observes committed audit");
        state
            .finalize_cancelled(details)
            .expect("idempotent repeated finalization");
        drop(guard);

        let session = store.load(&session.id).expect("audit session remains");
        let events = session
            .events
            .iter()
            .filter(|event| event.kind == "mcp_request_cancelled")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            events[0].details["cancellation_id"],
            Value::String(state.cancellation_id.clone())
        );
    }

    #[tokio::test]
    async fn immediate_read_only_cancellation_waits_for_audit_initialization() {
        let root = tempdir().expect("immediate cancellation repository");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        let cancellation = crate::agent::CancellationSignal::new();
        let task_cancellation = cancellation.clone();
        let slot = Arc::new(CancellationAuditSlot::default());
        let task_slot = Arc::clone(&slot);
        let task_store = store.clone();
        let task_session_id = session.id.clone();
        let task = tokio::spawn(async move {
            task_cancellation.cancelled().await;
            let (guard, state) =
                test_cancellation_audit(&task_store, &task_session_id, "read_file");
            task_slot.set(state);
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        let mut active = HashMap::from([(
            request_key(&json!("immediate")),
            ActiveRequest {
                generation: 1,
                id: json!("immediate"),
                task_kind: ActiveTaskKind::Execution,
                lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Running)),
                cancellation_class: RequestCancellationClass::ReadOnlyTool {
                    tool_name: "read_file".to_string(),
                },
                cancellation_audit: slot,
                cancellation,
                task,
            },
        )]);

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cancel_active_request(active.drain().next().expect("active request").1),
        )
        .await
        .expect("immediate cancellation is bounded");
        assert!(matches!(outcome, CancellationOutcome::Cancelled));
        let session = store.load(&session.id).expect("audit session remains");
        let event = session
            .events
            .iter()
            .find(|event| event.kind == "mcp_request_cancelled")
            .expect("cancellation audit event");
        assert_eq!(event.details["reconciled"], true);
        assert_eq!(event.details["effect_state"], "none");
    }

    #[tokio::test]
    async fn effectful_cancellation_fails_closed_with_unknown_effect_state() {
        let root = tempdir().expect("effect cancellation repository");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        let (guard, state) = test_cancellation_audit(&store, &session.id, "apply_patch");
        let slot = Arc::new(CancellationAuditSlot::default());
        slot.set(Arc::clone(&state));
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Running));
        let outcome = cancel_active_request(ActiveRequest {
            generation: 4,
            id: json!("effectful"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle: Arc::clone(&lifecycle),
            cancellation_class: RequestCancellationClass::EffectUnknownTool {
                tool_name: "apply_patch".to_string(),
            },
            cancellation_audit: slot,
            cancellation: crate::agent::CancellationSignal::new(),
            task: tokio::spawn(std::future::pending::<()>()),
        })
        .await;
        drop(guard);

        let CancellationOutcome::Failed(error) = outcome else {
            panic!("effectful cancellation must fail closed");
        };
        assert!(error.contains("effect state is unknown"), "{error}");
        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::CancellationFailed { .. }
        ));
        let session = store.load(&session.id).expect("audit session remains");
        let event = session
            .events
            .iter()
            .find(|event| event.kind == "mcp_request_cancelled")
            .expect("cancellation audit event");
        assert_eq!(event.details["reconciled"], false);
        assert_eq!(event.details["effect_state"], "unknown");
    }

    #[tokio::test]
    async fn shutdown_joins_requests_and_propagates_cancellation_failure() {
        let joined = Arc::new(AtomicBool::new(false));
        let task_joined = Arc::clone(&joined);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = release_rx.await;
            task_joined.store(true, Ordering::Release);
        });
        let mut active = HashMap::from([(
            request_key(&json!("shutdown")),
            ActiveRequest {
                generation: 1,
                id: json!("shutdown"),
                task_kind: ActiveTaskKind::Execution,
                lifecycle: Arc::new(StdMutex::new(RequestLifecycle::CancellationFailed {
                    response: rpc_error(json!("shutdown"), -32603, "audit failed"),
                    error: "audit failed".to_string(),
                })),
                cancellation_class: RequestCancellationClass::Protocol,
                cancellation_audit: Arc::new(CancellationAuditSlot::default()),
                cancellation: crate::agent::CancellationSignal::new(),
                task,
            },
        )]);

        let mut shutdown = Box::pin(cancel_all_requests(&mut active));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut shutdown)
                .await
                .is_err(),
            "shutdown must not return before the owned request task exits"
        );
        release_tx.send(()).expect("release request task");
        let shutdown = tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown is bounded after task release");
        let shutdown_error = shutdown.expect_err("audit failure propagates");
        assert_eq!(shutdown_error, "audit failed");
        assert!(
            joined.load(Ordering::Acquire),
            "request task must be joined"
        );
        assert_eq!(
            merge_server_shutdown_result(Ok(()), Err(shutdown_error)),
            Err("audit failed".to_string())
        );
        let combined = merge_server_shutdown_result(
            Err("transport failed".to_string()),
            Err("audit failed".to_string()),
        )
        .expect_err("both failures propagate");
        assert!(combined.contains("transport failed"));
        assert!(combined.contains("request shutdown failed: audit failed"));
    }

    #[tokio::test]
    async fn shutdown_hands_off_a_permanently_pending_subagent_request() {
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Running));
        let cancellation = crate::agent::CancellationSignal::new();
        let observed_cancellation = cancellation.clone();
        let mut active = HashMap::from([(
            request_key(&json!("pending-subagent")),
            ActiveRequest {
                generation: 1,
                id: json!("pending-subagent"),
                task_kind: ActiveTaskKind::Execution,
                lifecycle: Arc::clone(&lifecycle),
                cancellation_class: subagent_start_class("nib_run"),
                cancellation_audit: Arc::new(CancellationAuditSlot::default()),
                cancellation,
                task: tokio::spawn(std::future::pending::<()>()),
            },
        )]);

        let started = std::time::Instant::now();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            cancel_all_requests(&mut active),
        )
        .await
        .expect("subagent shutdown handoff is bounded")
        .expect_err("an incomplete handoff remains a shutdown error");

        assert!(active.is_empty());
        assert!(observed_cancellation.is_cancelled());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(error.contains("durable cancellation reconciliation was handed off"));
        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::CancelRequested
        ));
    }

    #[tokio::test]
    async fn subagent_shutdown_joins_held_lock_reconciliation_before_returning() {
        let root = tempdir().expect("held-lock cancellation repository");
        let id = format!("sub-held-cancel-{}", uuid::Uuid::new_v4());
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
            .expect("subagent owner lease");
        crate::tools::delegation::write_subagent_record(
            root.path(),
            &crate::tools::delegation::SubagentRecord {
                id: id.clone(),
                parent_session_id: Some(session.id.clone()),
                child_session_id: id.clone(),
                prompt: "held record lock".to_string(),
                status: "running".to_string(),
                execution_generation: Some(owner_lease.execution_generation()),
                owner_lease: Some(owner_lease.lease_id().to_string()),
                worktree_path: root.path().to_path_buf(),
                branch: format!("nib/subagent/{id}"),
                branch_oid: None,
                result: None,
                error: None,
                verification: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        )
        .expect("running subagent record");
        drop(owner_lease);
        let held = crate::tools::delegation::hold_subagent_record_lock_for_test(root.path(), &id)
            .expect("held subagent record lock");
        crate::daemons::task::TASK_MANAGER
            .register_task(id.clone(), "subagent")
            .expect("subagent task");
        let manager_task = tokio::spawn(std::future::pending::<()>());
        crate::daemons::task::TASK_MANAGER
            .attach_abort_handle(&id, manager_task.abort_handle())
            .expect("subagent abort handle");

        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Running));
        let cancellation = crate::agent::CancellationSignal::new();
        let task_cancellation = cancellation.clone();
        let task_lifecycle = Arc::clone(&lifecycle);
        let task_root = root.path().to_path_buf();
        let cancellation_class = subagent_start_class("nib_run");
        let task_cancellation_class = cancellation_class.clone();
        let response = rpc_result(
            json!("held-lock-cancel"),
            json!({
                "isError": false,
                "structuredContent": {"subagent_id": id.clone()}
            }),
        );
        let (audit, audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");
        let audit_slot = Arc::new(CancellationAuditSlot::default());
        audit_slot.set(audit_state);
        let task = tokio::spawn(async move {
            task_cancellation.cancelled().await;
            finish_request_lifecycle_async(
                &task_lifecycle,
                1,
                &task_root,
                &task_cancellation_class,
                HandledRequest {
                    response: Some(response),
                    cancellation_audit: Some(audit),
                },
            )
            .await;
        });

        let started = std::time::Instant::now();
        let outcome = cancel_active_request(ActiveRequest {
            generation: 1,
            id: json!("held-lock-cancel"),
            task_kind: ActiveTaskKind::Execution,
            lifecycle: Arc::clone(&lifecycle),
            cancellation_class,
            cancellation_audit: audit_slot,
            cancellation,
            task,
        })
        .await;
        let CancellationOutcome::Failed(error) = outcome else {
            panic!("held-lock reconciliation must fail closed");
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "MCP shutdown exceeded its owned reconciliation window"
        );
        assert!(!error.contains("handed off"), "{error}");
        assert!(
            error.contains("delegation state lock deadline elapsed"),
            "{error}"
        );
        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::CancellationFailed { .. }
        ));
        assert!(manager_task
            .await
            .expect_err("subagent manager task is cancelled")
            .is_cancelled());

        drop(held);
        tokio::time::sleep(
            crate::tools::delegation::SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT * 2,
        )
        .await;
        let record_path = root
            .path()
            .join(".nib")
            .join("subagents")
            .join(format!("{id}.json"));
        let persisted: crate::tools::delegation::SubagentRecord = serde_json::from_slice(
            &std::fs::read(record_path).expect("unreconciled subagent record bytes"),
        )
        .expect("unreconciled subagent record");
        assert_eq!(persisted.status, "running");
        assert!(
            persisted.result.is_none(),
            "MCP returned while a detached reconciler could still mutate durable state"
        );
    }

    #[test]
    fn oversized_server_response_becomes_a_bounded_rpc_error() {
        let response = rpc_result(
            json!("oversized"),
            json!({"payload": "x".repeat(MAX_MCP_FRAME_BYTES)}),
        );

        let frame = bounded_response_frame(&response).expect("bounded fallback response");

        assert!(frame.len() <= MAX_MCP_FRAME_BYTES);
        assert_eq!(frame.last(), Some(&b'\n'));
        let decoded: Value = serde_json::from_slice(&frame).expect("valid JSON response");
        assert_eq!(decoded["id"], "oversized");
        assert_eq!(decoded["error"]["code"], -32603);
    }

    #[test]
    fn oversized_tool_output_becomes_a_small_error_before_content_duplication() {
        let content = tool_result_content(ToolResult {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "fixture".to_string(),
            success: true,
            output: Some(json!({"payload": "x".repeat(MAX_MCP_TOOL_OUTPUT_BYTES)})),
            error: None,
            duration_seconds: 0.1,
            approval_granted: true,
            approval_source: Some("fixture".to_string()),
        });

        assert_eq!(content["isError"], true);
        assert!(content["structuredContent"].is_null());
        assert!(content["content"][0]["text"]
            .as_str()
            .expect("bounded error text")
            .contains("serialized MCP limit"));
        let response = rpc_result(json!(1), content);
        let frame = bounded_response_frame(&response).expect("small bounded response");
        assert!(frame.len() < 4096);
    }

    #[tokio::test]
    async fn read_tool_uses_executor_and_records_audit() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("hello.txt"), "hello MCP").unwrap();
        let response = handle_request(
            root.path(),
            &NibConfig::default(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "read_file", "arguments": {"path": "hello.txt"}}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], false);
        let store = SessionStore::for_project(root.path()).expect("profile store");
        let audited = store
            .list()
            .into_iter()
            .filter_map(|id| store.load(&id))
            .any(|session| {
                session
                    .tool_calls
                    .iter()
                    .any(|call| call.tool_name.as_deref() == Some("read_file"))
            });
        assert!(audited, "MCP calls must be recorded by ToolExecutor");
        assert!(!root.path().join(".nib/sessions").exists());
    }

    #[tokio::test]
    async fn direct_executor_uses_selected_profile_store_and_environment() {
        let root = tempdir().unwrap();
        initialize_git_repository(root.path());
        std::fs::write(
            root.path().join("AGENTS.md"),
            "- nib-policy: allow run_terminal printf\n",
        )
        .expect("explicit noninteractive allow policy");
        let config = save_profile_config(root.path());

        let response = handle_request(
            root.path(),
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "profile-env",
                "method": "tools/call",
                "params": {
                    "name": "run_terminal",
                    "arguments": {"command": "printf %s \"$NIB_PROFILE_VALUE\""}
                }
            }),
        )
        .await
        .unwrap();

        assert_eq!(response["result"]["isError"], false, "{response}");
        assert_eq!(
            response["result"]["structuredContent"]["stdout"],
            "profile-scoped"
        );
        let store = SessionStore::for_project(root.path()).expect("profile store");
        let expected_sessions = root.path().join(".nib/profiles/workspace/sessions");
        assert!(
            crate::fs_security::canonical_paths_match(store.sessions_dir(), &expected_sessions),
            "profile store {} does not match expected sessions path {}",
            store.sessions_dir().display(),
            expected_sessions.display()
        );
        assert!(store.list().into_iter().any(|id| {
            store.load(&id).is_some_and(|session| {
                session.tool_calls.iter().any(|call| {
                    call.tool_name.as_deref() == Some("run_terminal")
                        && call.result.as_ref().is_some_and(|result| {
                            result["environment_keys"].as_array().is_some_and(|keys| {
                                keys.iter().any(|key| key == "NIB_PROFILE_VALUE")
                            })
                        })
                })
            })
        }));
        assert!(!root.path().join(".nib/sessions").exists());
    }

    #[tokio::test]
    async fn destructive_tool_fails_closed_without_interactive_approval() {
        let root = tempdir().unwrap();
        let response = handle_request(
            root.path(),
            &NibConfig::default(),
            json!({
                "jsonrpc": "2.0",
                "id": "deny",
                "method": "tools/call",
                "params": {"name": "run_terminal", "arguments": {"command": "touch denied"}}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(!root.path().join("denied").exists());
    }

    #[tokio::test]
    async fn status_returns_only_the_requested_agent_run() {
        let root = tempdir().unwrap();
        let now = chrono::Utc::now();
        for id in ["first", "second"] {
            crate::tools::delegation::write_subagent_record(
                root.path(),
                &crate::tools::delegation::SubagentRecord {
                    id: id.to_string(),
                    parent_session_id: Some("parent".to_string()),
                    child_session_id: format!("child-{id}"),
                    prompt: "test".to_string(),
                    status: "completed".to_string(),
                    execution_generation: None,
                    owner_lease: None,
                    worktree_path: root.path().to_path_buf(),
                    branch: format!("nib/{id}"),
                    branch_oid: None,
                    result: Some(json!({"id": id})),
                    error: None,
                    verification: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .unwrap();
        }

        let response = handle_request(
            root.path(),
            &NibConfig::default(),
            json!({
                "jsonrpc": "2.0",
                "id": "status",
                "method": "tools/call",
                "params": {
                    "name": "nib_get_status",
                    "arguments": {"session_id": "child-second"}
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["structuredContent"]["id"], "second");
        assert!(!response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("first"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn status_rejects_credential_derived_identifiers_before_audit_or_reflection() {
        const SECRET: &str = "mcp/status-secret";
        const ENVIRONMENT_SECRET: &str = "environment-status-secret";
        let root = tempdir().unwrap();
        let _environment = EnvironmentGuard::set("OPENAI_API_KEY", ENVIRONMENT_SECRET);
        let mut config = NibConfig::default();
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "fixture-model".to_string(),
                api_key: Some(SECRET.to_string()),
                ..ProviderEntry::default()
            },
        );

        for session_id in [
            SECRET.to_string(),
            format!("prefix-{SECRET}-suffix"),
            "mcp%2Fstatus-secret".to_string(),
            "bWNwL3N0YXR1cy1zZWNyZXQ=".to_string(),
            r"mcp\/status-secret".to_string(),
            ENVIRONMENT_SECRET.to_string(),
            "ZW52aXJvbm1lbnQtc3RhdHVzLXNlY3JldA==".to_string(),
        ] {
            let response = handle_request(
                root.path(),
                &config,
                json!({
                    "jsonrpc": "2.0",
                    "id": "sensitive-status",
                    "method": "tools/call",
                    "params": {
                        "name": "nib_get_status",
                        "arguments": {"session_id": session_id}
                    }
                }),
            )
            .await
            .expect("sensitive status response");
            let rendered = response.to_string();
            assert_eq!(response["result"]["isError"], true, "{response}");
            assert!(
                rendered.contains("session identifier conflicts with configured sensitive data")
            );
            assert!(!rendered.contains(SECRET), "{rendered}");
            assert!(!rendered.contains("bWNwL3N0YXR1cy1zZWNyZXQ"), "{rendered}");
            assert!(!rendered.contains(ENVIRONMENT_SECRET), "{rendered}");
            assert!(
                !rendered.contains("ZW52aXJvbm1lbnQtc3RhdHVzLXNlY3JldA"),
                "{rendered}"
            );
        }

        assert!(
            !root.path().join(".nib").exists(),
            "rejected status calls must not initialize profile state or create audit sessions"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn schema_validation_errors_never_reflect_environment_credentials() {
        const SECRET: &str = "mcp/invalid-arg-secret";
        const BASE64_SECRET: &str = "bWNwL2ludmFsaWQtYXJnLXNlY3JldA==";
        let root = tempdir().expect("MCP project");
        let _environment = EnvironmentGuard::set("OPENAI_API_KEY", SECRET);

        for invalid_value in [
            SECRET.to_string(),
            r"mcp\/invalid-arg-secret".to_string(),
            BASE64_SECRET.to_string(),
            format!("{SECRET}\u{1b}[2J\u{202e}"),
        ] {
            let response = handle_request(
                root.path(),
                &NibConfig::default(),
                json!({
                    "jsonrpc": "2.0",
                    "id": "invalid-arguments",
                    "method": "tools/call",
                    "params": {
                        "name": "nib_run",
                        "arguments": {
                            "goal": "safe goal",
                            "max_steps": invalid_value
                        }
                    }
                }),
            )
            .await
            .expect("schema error response");
            let message = response["error"]["message"]
                .as_str()
                .expect("bounded validation message");
            assert_eq!(response["error"]["code"], -32602, "{response}");
            assert!(message.contains("/max_steps"), "{message}");
            assert!(message.contains("schema constraint"), "{message}");
            for forbidden in [
                SECRET,
                r"mcp\/invalid-arg-secret",
                BASE64_SECRET,
                "\u{1b}",
                "\u{202e}",
            ] {
                assert!(!message.contains(forbidden), "{message:?}");
            }
            assert!(message.len() <= MAX_MCP_VALIDATION_ERROR_BYTES);
        }

        assert!(
            !root.path().join(".nib").exists(),
            "schema rejection must precede audit-session initialization"
        );
    }

    #[tokio::test]
    async fn notifications_have_no_response_and_unknown_tools_are_errors() {
        let root = tempdir().unwrap();
        assert!(handle_request(
            root.path(),
            &NibConfig::default(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await
        .is_none());

        let response = handle_request(
            root.path(),
            &NibConfig::default(),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "not_real", "arguments": {}}
            }),
        )
        .await
        .unwrap();
        assert_eq!(response["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn postcommit_nib_run_completion_win_has_no_cancellation_audit() {
        let root = tempdir().expect("completion-win repository");
        initialize_git_repository(root.path());
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        crate::tools::delegation::write_subagent_record(
            root.path(),
            &crate::tools::delegation::SubagentRecord {
                id: id.clone(),
                parent_session_id: Some(session.id.clone()),
                child_session_id: id.clone(),
                prompt: "already complete".to_string(),
                status: "completed".to_string(),
                execution_generation: None,
                owner_lease: None,
                worktree_path: root.path().to_path_buf(),
                branch: format!("nib/subagent/{id}"),
                branch_oid: None,
                result: Some(json!({"outcome": "completed"})),
                error: None,
                verification: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        )
        .expect("completed subagent record");
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
        let response = rpc_result(
            json!("postcommit-complete"),
            json!({
                "isError": false,
                "structuredContent": {
                    "subagent_id": id.clone(),
                    "parent_session_id": session.id.clone(),
                }
            }),
        );
        let (audit, _audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");
        let handled = HandledRequest {
            response: Some(response),
            cancellation_audit: Some(audit),
        };

        finish_request_lifecycle(
            &lifecycle,
            1,
            root.path(),
            &subagent_start_class("nib_run"),
            handled,
        );

        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::Completed(Some(_))
        ));
        let session = store.load(&session.id).expect("audit session remains");
        assert_eq!(
            session
                .events
                .iter()
                .filter(|event| event.kind == "mcp_request_cancelled")
                .count(),
            0,
            "completion winner must not be audited as cancellation"
        );
    }

    #[tokio::test]
    async fn one_worker_postcommit_nib_run_cancellation_records_exactly_one_rich_audit() {
        let root = tempdir().expect("cancellation-win repository");
        #[cfg(windows)]
        let cancellation_timeout = std::time::Duration::from_secs(15);
        #[cfg(not(windows))]
        let cancellation_timeout = std::time::Duration::from_secs(2);
        let _timeout =
            crate::tools::delegation::SubagentCancellationTimeoutGuard::set(cancellation_timeout);
        initialize_git_repository(root.path());
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let worktree = crate::sandbox::worktree::Worktree::create(root.path(), &id)
            .expect("subagent worktree");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
            .expect("subagent owner lease");
        let cleanup_proof =
            ManagedProcessFixture::start(root.path(), &id, owner_lease.execution_generation())
                .complete("cancelled");
        crate::tools::delegation::write_subagent_record(
            root.path(),
            &crate::tools::delegation::SubagentRecord {
                id: id.clone(),
                parent_session_id: Some(session.id.clone()),
                child_session_id: id.clone(),
                prompt: "cancel me".to_string(),
                status: "running".to_string(),
                execution_generation: Some(owner_lease.execution_generation()),
                owner_lease: Some(owner_lease.lease_id().to_string()),
                worktree_path: worktree.path.clone(),
                branch: worktree.branch.clone(),
                branch_oid: None,
                result: None,
                error: None,
                verification: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        )
        .expect("running subagent record");
        crate::daemons::task::TASK_MANAGER
            .register_task(id.clone(), "subagent")
            .expect("subagent task");
        let running = tokio::spawn(std::future::pending::<()>());
        crate::daemons::task::TASK_MANAGER
            .attach_abort_handle(&id, running.abort_handle())
            .expect("subagent abort handle");
        let owner_id = id.clone();
        let owner_release = tokio::spawn(async move {
            let started = std::time::Instant::now();
            while crate::daemons::task::TASK_MANAGER
                .get_status(&owner_id)
                .as_deref()
                != Some("cancelled")
            {
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(5),
                    "subagent manager never entered cancelled state"
                );
                tokio::task::yield_now().await;
            }
            drop(owner_lease);
        });
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
        let response = rpc_result(
            json!("postcommit-cancel"),
            json!({
                "isError": false,
                "structuredContent": {
                    "subagent_id": id.clone(),
                    "parent_session_id": session.id.clone(),
                }
            }),
        );
        let (audit, audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");
        audit_state.injected_failures.store(1, Ordering::Release);
        crate::tools::delegation::inject_cancelled_record_write_failures(&id, 1);
        let handled = HandledRequest {
            response: Some(response),
            cancellation_audit: Some(audit),
        };

        finish_request_lifecycle_async(
            &lifecycle,
            1,
            root.path(),
            &subagent_start_class("nib_run"),
            handled,
        )
        .await;
        owner_release.await.expect("owner lease release task");

        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::Cancelled
        ));
        assert!(
            running.await.expect_err("cancelled task").is_cancelled(),
            "subagent execution must be aborted"
        );
        let session = store.load(&session.id).expect("audit session remains");
        let cancellation_events = session
            .events
            .iter()
            .filter(|event| event.kind == "mcp_request_cancelled")
            .collect::<Vec<_>>();
        assert_eq!(cancellation_events.len(), 1, "{cancellation_events:?}");
        assert_eq!(cancellation_events[0].details["subagent_id"], id);
        assert_eq!(
            crate::tools::delegation::get_subagent_record(root.path(), &id)
                .expect("cancelled record reread")
                .status,
            "cancelled"
        );
        let cancelled_record = crate::tools::delegation::get_subagent_record(root.path(), &id)
            .expect("public cancelled record");
        assert!(cancelled_record.execution_generation.is_none());
        assert!(cancelled_record.owner_lease.is_none());
        assert!(cancelled_record.result.as_ref().is_none_or(|result| {
            result.get("ownership_reconciliation").is_none()
                && result.get("cleanup_proof").is_none()
        }));
        let persisted: crate::tools::delegation::SubagentRecord = serde_json::from_slice(
            &std::fs::read(
                root.path()
                    .join(".nib/subagents")
                    .join(format!("{id}.json")),
            )
            .expect("persisted cancelled record"),
        )
        .expect("persisted cancelled record JSON");
        assert_eq!(
            persisted.result.as_ref().and_then(|result| {
                result
                    .get("ownership_reconciliation")
                    .and_then(|evidence| evidence.get("cleanup_proof"))
            }),
            Some(
                &serde_json::to_value(&cleanup_proof)
                    .expect("encode expected managed-process cleanup proof")
            )
        );
        assert!(
            crate::sandbox::process::ProcessScopeStore::open(root.path())
                .expect("managed-process fixture store")
                .try_load(&id)
                .expect("retired managed-process fixture lookup")
                .is_none(),
            "terminal workload proof must retire the completed process scope"
        );
        crate::sandbox::worktree::Worktree::remove(root.path(), &id)
            .expect("cancelled worktree cleanup");
    }

    #[tokio::test]
    async fn unsafe_manager_cancellation_is_an_internal_error_and_remains_running() {
        let root = tempdir().expect("unresolved cancellation repository");
        initialize_git_repository(root.path());
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        let owner_lease = crate::tools::delegation::create_test_subagent_owner_lease(root.path())
            .expect("subagent owner lease");
        let process_scope =
            ManagedProcessFixture::start(root.path(), &id, owner_lease.execution_generation());
        crate::tools::delegation::write_subagent_record(
            root.path(),
            &crate::tools::delegation::SubagentRecord {
                id: id.clone(),
                parent_session_id: Some(session.id.clone()),
                child_session_id: id.clone(),
                prompt: "cannot cancel safely".to_string(),
                status: "running".to_string(),
                execution_generation: Some(owner_lease.execution_generation()),
                owner_lease: Some(owner_lease.lease_id().to_string()),
                worktree_path: root.path().to_path_buf(),
                branch: format!("nib/subagent/{id}"),
                branch_oid: None,
                result: None,
                error: None,
                verification: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        )
        .expect("running subagent record");
        crate::daemons::task::TASK_MANAGER
            .register_task(id.clone(), "subagent")
            .expect("unattached subagent task");
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
        let response = rpc_result(
            json!("unresolved-cancel"),
            json!({
                "isError": false,
                "structuredContent": {"subagent_id": id.clone()}
            }),
        );
        let (audit, _audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");

        finish_request_lifecycle(
            &lifecycle,
            7,
            root.path(),
            &subagent_start_class("nib_run"),
            HandledRequest {
                response: Some(response),
                cancellation_audit: Some(audit),
            },
        );

        let error_response = match &*lock_request_lifecycle(&lifecycle) {
            RequestLifecycle::CancellationFailed { response, .. } => response.clone(),
            state => panic!("unexpected lifecycle state: {state:?}"),
        };
        assert_eq!(error_response["error"]["code"], -32603);
        assert_eq!(
            crate::tools::delegation::get_subagent_record(root.path(), &id)
                .expect("running record remains readable")
                .status,
            "running"
        );
        let session = store.load(&session.id).expect("audit session remains");
        let events = session
            .events
            .iter()
            .filter(|event| event.kind == "mcp_request_cancelled")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].details["outcome"], "unresolved");
        assert_eq!(events[0].details["manager_stopped"], false);
        assert_eq!(
            crate::sandbox::process::ProcessScopeStore::open(root.path())
                .expect("managed-process fixture store")
                .cleanup_lease_state(process_scope.scope())
                .expect("live managed-process cleanup lease"),
            crate::sandbox::process::CleanupLeaseState::Live
        );
        crate::daemons::task::TASK_MANAGER
            .rollback_unattached_task(&id)
            .expect("unattached task cleanup");
        process_scope.complete("test_teardown");
        drop(owner_lease);
    }

    #[tokio::test]
    async fn missing_successful_subagent_is_never_reported_as_cancelled() {
        let root = tempdir().expect("missing cancellation repository");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
        let missing_id = format!("sub-{}", uuid::Uuid::new_v4());
        let (audit, _audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");

        finish_request_lifecycle(
            &lifecycle,
            3,
            root.path(),
            &subagent_start_class("nib_run"),
            HandledRequest {
                response: Some(rpc_result(
                    json!("missing-subagent"),
                    json!({
                        "isError": false,
                        "structuredContent": {"subagent_id": missing_id}
                    }),
                )),
                cancellation_audit: Some(audit),
            },
        );

        match &*lock_request_lifecycle(&lifecycle) {
            RequestLifecycle::CancellationFailed { response, .. } => {
                assert_eq!(response["error"]["code"], -32603);
            }
            state => panic!("unexpected lifecycle state: {state:?}"),
        }
        let session = store.load(&session.id).expect("audit session remains");
        assert_eq!(
            session
                .events
                .iter()
                .filter(|event| event.kind == "mcp_request_cancelled")
                .count(),
            1
        );
    }

    #[test]
    fn stale_lifecycle_generation_cannot_publish() {
        let root = tempdir().expect("stale lifecycle repository");
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::Reconciling {
            generation: 9,
        }));

        finish_request_lifecycle(
            &lifecycle,
            8,
            root.path(),
            &RequestCancellationClass::Protocol,
            HandledRequest::without_audit(Some(rpc_result(json!(8), json!({})))),
        );

        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::Reconciling { generation: 9 }
        ));
    }

    #[tokio::test]
    async fn reconciliation_releases_mutex_and_cas_rejects_stale_publication() {
        let root = tempdir().expect("reconciliation barrier repository");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session = store.try_create_session().expect("audit session");
        crate::tools::delegation::write_subagent_record(
            root.path(),
            &crate::tools::delegation::SubagentRecord {
                id: id.clone(),
                parent_session_id: Some(session.id.clone()),
                child_session_id: id.clone(),
                prompt: "terminal reconciliation".to_string(),
                status: "completed".to_string(),
                execution_generation: None,
                owner_lease: None,
                worktree_path: root.path().to_path_buf(),
                branch: format!("nib/subagent/{id}"),
                branch_oid: None,
                result: Some(json!({"outcome": "completed"})),
                error: None,
                verification: None,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        )
        .expect("terminal subagent record");
        let lifecycle = Arc::new(StdMutex::new(RequestLifecycle::CancelRequested));
        let barrier = Arc::new(ReconciliationBarrier {
            subagent_id: id.clone(),
            entered: std::sync::atomic::AtomicBool::new(false),
            release: std::sync::atomic::AtomicBool::new(false),
        });
        *RECONCILIATION_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&barrier));
        let (audit, _audit_state) = test_cancellation_audit(&store, &session.id, "nib_run");
        let thread_lifecycle = Arc::clone(&lifecycle);
        let project_root = root.path().to_path_buf();
        let thread = std::thread::spawn(move || {
            finish_request_lifecycle(
                &thread_lifecycle,
                1,
                &project_root,
                &subagent_start_class("nib_run"),
                HandledRequest {
                    response: Some(rpc_result(
                        json!("stale-reconciliation"),
                        json!({
                            "isError": false,
                            "structuredContent": {"subagent_id": id}
                        }),
                    )),
                    cancellation_audit: Some(audit),
                },
            );
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !barrier.entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reconciliation reached barrier");

        {
            let mut state = lifecycle
                .try_lock()
                .expect("lifecycle mutex must be released during reconciliation I/O");
            *state = RequestLifecycle::Reconciling { generation: 2 };
        }
        barrier.release.store(true, Ordering::Release);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || thread.join()),
        )
        .await
        .expect("reconciliation thread finished")
        .expect("reconciliation join task")
        .expect("reconciliation thread did not panic");
        *RECONCILIATION_BARRIER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        assert!(matches!(
            &*lock_request_lifecycle(&lifecycle),
            RequestLifecycle::Reconciling { generation: 2 }
        ));
        let session = store.load(&session.id).expect("audit session remains");
        assert_eq!(
            session
                .events
                .iter()
                .filter(|event| event.kind == "mcp_request_cancelled")
                .count(),
            0,
            "terminal completion must not be audited as cancellation"
        );
    }

    #[tokio::test]
    async fn stale_completion_cannot_remove_reused_active_request_id() {
        let key = request_key(&json!("reused"));
        let task = tokio::spawn(std::future::pending::<()>());
        let mut active = HashMap::from([(
            key.clone(),
            ActiveRequest {
                generation: 2,
                id: json!("reused"),
                task_kind: ActiveTaskKind::Execution,
                lifecycle: Arc::new(StdMutex::new(RequestLifecycle::Running)),
                cancellation_class: RequestCancellationClass::Protocol,
                cancellation_audit: Arc::new(CancellationAuditSlot::default()),
                cancellation: crate::agent::CancellationSignal::new(),
                task,
            },
        )]);

        assert!(take_completed_request(
            &mut active,
            &CompletedRequest {
                key: key.clone(),
                generation: 1,
            },
        )
        .is_none());
        assert_eq!(active.len(), 1);
        let current = take_completed_request(&mut active, &CompletedRequest { key, generation: 2 })
            .expect("current generation completes");
        assert!(active.is_empty());
        current.task.abort();
        assert!(current
            .task
            .await
            .expect_err("test request cancellation")
            .is_cancelled());
    }

    #[tokio::test]
    async fn nib_run_provider_failure_reaches_mcp_status_as_typed_llm_error() {
        const SECRET: &str = "mcp/provider+secret";
        const SECRET_PERCENT: &str = "mcp%2Fprovider%2Bsecret";
        const SECRET_BASE64: &str = "bWNwL3Byb3ZpZGVyK3NlY3JldA==";
        const REMOTE_BODY: &str = "REMOTE_MCP_PROVIDER_BODY";
        let root = tempdir().expect("mcp llm-failure repository");
        initialize_git_repository(root.path());
        let (base_url, _) = serve_once(
            "401 Unauthorized",
            "application/json",
            serde_json::json!({
                "error": {
                    "code": "invalid_api_key",
                    "message": format!(
                        "{REMOTE_BODY} {SECRET} {SECRET_PERCENT} {SECRET_BASE64} <red>[bold] \u{1b}[31m"
                    )
                }
            })
            .to_string(),
        );
        let mut config = NibConfig::default();
        config.execution.plan_mode = false;
        config.skills.enabled = false;
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "fixture-model".to_string(),
                api_key: Some(SECRET.to_string()),
                base_url: Some(base_url),
                api: Some(LlmApiMode::ChatCompletions),
                ..ProviderEntry::default()
            },
        );
        save_nib_config_full(root.path(), &mut config).expect("save config");
        let config = load_nib_config_full(root.path()).expect("reload config");

        let started = handle_request(
            root.path(),
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "run",
                "method": "tools/call",
                "params": {
                    "name": "nib_run",
                    "arguments": {"goal": "inspect the workspace", "max_steps": 1}
                }
            }),
        )
        .await
        .expect("nib_run response");
        assert_eq!(started["result"]["isError"], false, "{started}");
        let subagent_id = started["result"]["structuredContent"]["subagent_id"]
            .as_str()
            .expect("subagent id")
            .to_string();
        let started_payload = started.to_string();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let record = loop {
            let record = crate::tools::delegation::get_subagent_record(root.path(), &subagent_id)
                .expect("subagent record");
            if record.status != "running" {
                assert_eq!(record.status, "failed", "{record:?}");
                break record;
            }
            if std::time::Instant::now() > deadline {
                panic!("subagent did not finish: {record:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let result = record.result.as_ref().expect("typed subagent result");
        assert_eq!(result["outcome"], "planning_failed");
        assert_eq!(result["failure"]["incident_code"], "LLM-AUTH");
        assert_eq!(result["failure"]["class"], "authentication");

        let child_store = crate::session::SessionStore::for_project(&record.worktree_path)
            .expect("MCP child session store");
        let child = child_store
            .load(&record.child_session_id)
            .expect("MCP child failure session");
        child
            .validate_message_sequence()
            .expect("MCP child message sequence");
        assert!(!child.messages.iter().any(|message| {
            message.role == "assistant"
                && (message.content.contains("LLM") || message.content.contains("failed"))
        }));

        let status = handle_request(
            root.path(),
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "status",
                "method": "tools/call",
                "params": {
                    "name": "nib_get_status",
                    "arguments": {"session_id": subagent_id}
                }
            }),
        )
        .await
        .expect("status response");
        let payload = status.to_string();
        assert_eq!(status["result"]["isError"], false, "{status}");
        let structured = &status["result"]["structuredContent"];
        assert_eq!(structured["status"], "failed", "{status}");
        assert_eq!(structured["result"]["outcome"], "planning_failed");
        assert_eq!(structured["result"]["failure"]["incident_code"], "LLM-AUTH");
        assert_eq!(structured["result"]["failure"]["class"], "authentication");

        let repeated = handle_request(
            root.path(),
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "status-repeat",
                "method": "tools/call",
                "params": {
                    "name": "nib_get_status",
                    "arguments": {"session_id": subagent_id}
                }
            }),
        )
        .await
        .expect("repeated status response");
        assert_eq!(repeated["result"]["isError"], false, "{repeated}");
        assert_eq!(
            repeated["result"]["structuredContent"],
            status["result"]["structuredContent"]
        );

        let persisted_record = serde_json::to_string(&record).expect("subagent record JSON");
        let persisted_session = serde_json::to_string(&child).expect("child session JSON");
        let repeated_payload = repeated.to_string();
        for surface in [
            &started_payload,
            &payload,
            &repeated_payload,
            &persisted_record,
            &persisted_session,
        ] {
            assert!(surface.len() <= 64 * 1024, "observer payload is unbounded");
            for forbidden in [
                SECRET,
                SECRET_PERCENT,
                SECRET_BASE64,
                REMOTE_BODY,
                "invalid_api_key",
                "<red>",
                "[red]",
                "[bold]",
                "\u{1b}",
                "\\u001b",
                "\\u001B",
            ] {
                assert!(!surface.contains(forbidden), "found {forbidden}: {surface}");
            }
            assert!(surface.chars().all(|character| {
                !character.is_control() || matches!(character, '\n' | '\r' | '\t')
            }));
        }
    }

    #[tokio::test]
    async fn invalid_tool_calls_never_create_audit_sessions() {
        let root = tempdir().unwrap();
        let invalid_params = [
            json!({"name": "", "arguments": {}}),
            json!({"name": "not_real", "arguments": {}}),
            json!({"name": "read_file", "arguments": {}}),
            json!({"name": "read_file", "arguments": {"path": 7}}),
            json!({"name": "nib_run", "arguments": {"goal": ""}}),
            json!({"name": "nib_run", "arguments": {"goal": "x", "max_steps": 101}}),
            json!({"name": "nib_get_status", "arguments": {"session_id": ""}}),
        ];

        for index in 0..256 {
            let response = handle_request(
                root.path(),
                &NibConfig::default(),
                json!({
                    "jsonrpc": "2.0",
                    "id": index,
                    "method": "tools/call",
                    "params": invalid_params[index % invalid_params.len()].clone()
                }),
            )
            .await
            .expect("invalid call response");
            assert_eq!(response["error"]["code"], -32602, "{response}");
        }

        assert!(
            !root.path().join(".nib").exists(),
            "invalid calls must not initialize profile state or persist sessions"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mcp_subagent_flow_accepts_a_dos_short_project_root() {
        let root = tempdir().expect("MCP DOS-alias repository");
        let _timeout = crate::tools::delegation::SubagentCancellationTimeoutGuard::set(
            std::time::Duration::from_secs(10),
        );
        initialize_git_repository(root.path());
        let config = save_profile_config(root.path());
        let canonical_root = root.path().canonicalize().expect("canonical repository");
        let short_root = crate::fs_security::windows_dos_short_path_for_test(&canonical_root)
            .expect("DOS short project root");
        if short_root == crate::fs_security::path_without_windows_verbatim_prefix(&canonical_root) {
            return;
        }

        let started = handle_request(
            &short_root,
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "dos-alias-run",
                "method": "tools/call",
                "params": {
                    "name": "nib_run",
                    "arguments": {"goal": "Return a bounded fixture response.", "max_steps": 1}
                }
            }),
        )
        .await
        .expect("MCP subagent start response");
        assert_eq!(started["result"]["isError"], false, "{started}");
        let id = started["result"]["structuredContent"]["subagent_id"]
            .as_str()
            .expect("subagent id");
        let record = crate::tools::delegation::get_subagent_record(&short_root, id)
            .expect("MCP delegation record");
        assert!(record.worktree_path.starts_with(&canonical_root));
        assert!(!record.worktree_path.starts_with(&short_root));

        let status = handle_request(
            &short_root,
            &config,
            json!({
                "jsonrpc": "2.0",
                "id": "dos-alias-status",
                "method": "tools/call",
                "params": {
                    "name": "nib_get_status",
                    "arguments": {"session_id": id}
                }
            }),
        )
        .await
        .expect("MCP status response");
        assert_eq!(status["result"]["isError"], false, "{status}");

        match crate::tools::delegation::resolve_subagent_cancellation_async(&short_root, id).await {
            crate::tools::delegation::CancelSubagentResolution::Cancelled { .. }
            | crate::tools::delegation::CancelSubagentResolution::Terminal { .. } => {}
            crate::tools::delegation::CancelSubagentResolution::Unresolved { error, .. } => {
                panic!("DOS-alias MCP cancellation was unresolved: {error}")
            }
        }
        crate::sandbox::worktree::Worktree::remove(&short_root, id)
            .expect("remove MCP worktree through DOS short root");
    }
}
