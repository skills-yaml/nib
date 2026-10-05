//! T043 split.

use super::*;

pub(crate) const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
pub(crate) const MAX_MCP_TOOL_OUTPUT_BYTES: usize = MAX_MCP_FRAME_BYTES / 4;
pub(crate) const MAX_MCP_VALIDATION_ERROR_BYTES: usize = 8 * 1024;
pub(crate) const MAX_ACTIVE_MCP_REQUESTS: usize = 32;
pub(crate) const MCP_REQUEST_CANCELLED_CODE: i64 = -32800;
pub(crate) const MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
#[cfg(not(test))]
pub(crate) const MCP_SUBAGENT_SHUTDOWN_HANDOFF_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
#[cfg(test)]
pub(crate) const MCP_SUBAGENT_SHUTDOWN_HANDOFF_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(500);

pub(crate) struct OwnedResponseWriter {
    pub(crate) response_tx: Option<mpsc::Sender<QueuedResponse>>,
    pub(crate) task: Option<JoinHandle<()>>,
    pub(crate) relay: Option<crate::sandbox::ManagedChild>,
}

#[derive(Debug)]
pub(crate) struct QueuedResponse {
    pub(crate) frame: Vec<u8>,
    pub(crate) request_key: Option<String>,
}

impl OwnedResponseWriter {
    pub(crate) async fn start(
    ) -> Result<(Self, mpsc::Receiver<String>, mpsc::Receiver<String>), String> {
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

    pub(crate) fn from_writer<W>(
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

    pub(crate) fn sender(&self) -> &mpsc::Sender<QueuedResponse> {
        self.response_tx
            .as_ref()
            .expect("MCP response writer is active")
    }

    pub(crate) async fn shutdown(&mut self) {
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

pub(crate) struct ActiveRequest {
    pub(crate) generation: u64,
    pub(crate) id: Value,
    pub(crate) task_kind: ActiveTaskKind,
    pub(crate) lifecycle: SharedRequestLifecycle,
    pub(crate) cancellation_class: RequestCancellationClass,
    pub(crate) cancellation_audit: SharedCancellationAuditSlot,
    pub(crate) cancellation: crate::agent::CancellationSignal,
    pub(crate) task: JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveTaskKind {
    Execution,
    Cancellation,
}

#[derive(Debug, Clone)]
pub(crate) struct CompletedRequest {
    pub(crate) key: String,
    pub(crate) generation: u64,
}

pub(crate) struct RequestCompletionGuard {
    pub(crate) completion_tx: mpsc::Sender<CompletedRequest>,
    pub(crate) missed_completions: Arc<StdMutex<Vec<CompletedRequest>>>,
    pub(crate) missed_completion_overflow: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) completion_notify: Arc<Notify>,
    pub(crate) completion: Option<CompletedRequest>,
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
pub(crate) enum RequestLifecycle {
    Running,
    CancelRequested,
    Reconciling { generation: u64 },
    Completed(Option<Value>),
    CancellationFailed { response: Value, error: String },
    Cancelled,
}

pub(crate) type SharedRequestLifecycle = Arc<StdMutex<RequestLifecycle>>;

pub(crate) enum CancellationOutcome {
    Completed,
    Cancelled,
    Failed(String),
}

pub(crate) enum SubagentCancellationOutcome {
    Cancelled(Value),
    Terminal,
    Unresolved { details: Value, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestCancellationClass {
    Protocol,
    ReadOnlyTool { tool_name: String },
    InterruptibleTool { tool_name: String },
    EffectUnknownTool { tool_name: String },
    SubagentStart { tool_name: String },
}

pub(crate) enum InboundFrame {
    Frame(Vec<u8>),
    Eof,
    Failed(String),
}

pub(crate) struct McpRuntime {
    pub(crate) project_root: PathBuf,
    pub(crate) session_store: SessionStore,
    pub(crate) environment: HashMap<String, String>,
}

pub(crate) struct PreparedToolCall {
    pub(crate) requested_name: String,
    pub(crate) executor_name: String,
    pub(crate) arguments: Value,
    pub(crate) requested_status_id: Option<String>,
}

pub(crate) struct McpCancellationAuditState {
    pub(crate) session_store: SessionStore,
    pub(crate) session_id: String,
    pub(crate) tool_name: String,
    pub(crate) cancellation_id: String,
    pub(crate) status: StdMutex<McpCancellationAuditStatus>,
    #[cfg(test)]
    pub(crate) injected_failures: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(crate) injected_post_commit_failures: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpCancellationAuditStatus {
    Pending,
    CancellationOwned,
    FallbackOwned,
    Completed,
    Cancelled,
}

pub(crate) struct McpCancellationAuditGuard {
    pub(crate) state: Arc<McpCancellationAuditState>,
    pub(crate) armed: bool,
}

#[derive(Default)]
pub(crate) struct CancellationAuditSlot {
    pub(crate) state: StdMutex<Option<Arc<McpCancellationAuditState>>>,
    pub(crate) ready: Notify,
}

pub(crate) type SharedCancellationAuditSlot = Arc<CancellationAuditSlot>;

#[cfg(test)]
pub(crate) struct ReconciliationBarrier {
    pub(crate) subagent_id: String,
    pub(crate) entered: std::sync::atomic::AtomicBool,
    pub(crate) release: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
pub(crate) static RECONCILIATION_BARRIER: std::sync::LazyLock<
    StdMutex<Option<Arc<ReconciliationBarrier>>>,
> = std::sync::LazyLock::new(|| StdMutex::new(None));

pub(crate) struct HandledRequest {
    pub(crate) response: Option<Value>,
    pub(crate) cancellation_audit: Option<McpCancellationAuditGuard>,
}

impl HandledRequest {
    pub(crate) fn without_audit(response: Option<Value>) -> Self {
        Self {
            response,
            cancellation_audit: None,
        }
    }

    pub(crate) fn complete_audit(&mut self) {
        if let Some(audit) = self.cancellation_audit.as_mut() {
            audit.state.complete();
            audit.armed = false;
        }
    }

    pub(crate) fn disarm_audit(&mut self) {
        if let Some(audit) = self.cancellation_audit.as_mut() {
            audit.armed = false;
        }
    }

    pub(crate) fn finalize_cancellation_audit(&mut self, details: Value) -> Result<(), String> {
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
    pub(crate) fn complete(&self) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *status == McpCancellationAuditStatus::Pending {
            *status = McpCancellationAuditStatus::Completed;
        }
    }

    pub(crate) fn finalize_cancelled(&self, details: Value) -> Result<(), String> {
        self.finalize_cancelled_until(
            details,
            std::time::Instant::now() + MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT,
        )
    }

    pub(crate) fn finalize_cancelled_until(
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

    pub(crate) fn finalize_cancelled_until_owned(
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
                .try_update(
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
                            .try_update(
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

    pub(crate) fn claim_cancellation(&self) -> Result<(), String> {
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

    pub(crate) fn claim_fallback(&self) -> bool {
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

    pub(crate) fn finalize_fallback(&self, details: Value) -> Result<(), String> {
        self.finalize_cancelled_until_owned(
            details,
            std::time::Instant::now() + MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT,
            McpCancellationAuditStatus::FallbackOwned,
        )
    }

    pub(crate) fn with_cancellation_id(&self, details: Value) -> Value {
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

    pub(crate) fn record_cancellation_event_once(
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

    pub(crate) fn authoritative_event_exists(
        &self,
        deadline: std::time::Instant,
    ) -> Result<bool, String> {
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
    pub(crate) fn set(&self, state: Arc<McpCancellationAuditState>) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
        self.ready.notify_one();
    }

    pub(crate) fn get(&self) -> Option<Arc<McpCancellationAuditState>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub(crate) fn finalize_cancellation_audit_slot(
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

pub(crate) fn claim_cancellation_audit_slot(
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

pub(crate) async fn finalize_cancellation_audit_slot_blocking(
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

pub(crate) fn cancellation_audit_tool_name(slot: &SharedCancellationAuditSlot) -> Option<String> {
    slot.get().as_ref().map(|audit| audit.tool_name.clone())
}

/// MCP owns stdin, so an approval prompt cannot safely read from it. Explicit
/// policy/configuration can still grant a call before this handler is reached.
pub(crate) struct DenyInteractiveApproval;

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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn serve_mcp_io<R>(
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

pub(crate) fn merge_server_shutdown_result(
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

pub(crate) fn cancellation_target(request: &Value) -> Option<String> {
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || request.get("id").is_some()
        || request.get("method").and_then(Value::as_str) != Some("notifications/cancelled")
    {
        return None;
    }
    let id = request.get("params")?.get("requestId")?.clone();
    Some(request_key(&id))
}

pub(crate) fn request_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".to_string())
}

pub(crate) fn request_id_is_owned(
    active: &HashMap<String, ActiveRequest>,
    pending_response_ids: &HashSet<String>,
    key: &str,
) -> bool {
    active.contains_key(key) || pending_response_ids.contains(key)
}

pub(crate) fn take_completed_request(
    active: &mut HashMap<String, ActiveRequest>,
    completion: &CompletedRequest,
) -> Option<ActiveRequest> {
    let is_current = active
        .get(&completion.key)
        .is_some_and(|request| request.generation == completion.generation);
    is_current.then(|| active.remove(&completion.key)).flatten()
}

pub(crate) fn store_missed_completion(
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

pub(crate) fn take_ready_missed_requests(
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

pub(crate) async fn publish_completed_request(
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

pub(crate) fn subagent_start_tool(request: &Value) -> Option<&str> {
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

pub(crate) fn classify_request_cancellation(request: &Value) -> RequestCancellationClass {
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
    pub(crate) fn subagent_tool_name(&self) -> Option<&str> {
        match self {
            Self::SubagentStart { tool_name } => Some(tool_name),
            _ => None,
        }
    }

    pub(crate) fn tool_name(&self) -> Option<&str> {
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
pub(crate) fn finish_request_lifecycle(
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

pub(crate) async fn finish_request_lifecycle_async(
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

pub(crate) fn begin_request_lifecycle_finish<'a>(
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

pub(crate) fn publish_request_lifecycle_reconciliation(
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
pub(crate) fn reconcile_started_subagent(
    project_root: &Path,
    tool_name: &str,
    handled: &mut HandledRequest,
) -> RequestLifecycle {
    #[cfg(test)]
    pause_at_reconciliation_barrier(handled.response.as_ref());
    let outcome = cancel_started_subagent(project_root, tool_name, handled.response.as_ref());
    finish_started_subagent_reconciliation(handled, outcome)
}

pub(crate) async fn reconcile_started_subagent_async(
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

pub(crate) fn finish_started_subagent_reconciliation(
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
pub(crate) fn pause_at_reconciliation_barrier(response: Option<&Value>) {
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
pub(crate) fn cancel_started_subagent(
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

pub(crate) async fn cancel_started_subagent_async(
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

pub(crate) fn started_subagent_cancellation_target<'a>(
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

pub(crate) fn started_subagent_cancellation_outcome(
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

pub(crate) fn lock_request_lifecycle(
    lifecycle: &SharedRequestLifecycle,
) -> std::sync::MutexGuard<'_, RequestLifecycle> {
    lifecycle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn completed_response(lifecycle: &SharedRequestLifecycle, id: &Value) -> Option<Value> {
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

pub(crate) fn request_subagent_cancellation(request: &ActiveRequest) {
    let mut lifecycle = lock_request_lifecycle(&request.lifecycle);
    if matches!(*lifecycle, RequestLifecycle::Running) {
        *lifecycle = RequestLifecycle::CancelRequested;
        request.cancellation.cancel();
    }
}

pub(crate) async fn wait_for_audit_or_task(
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

pub(crate) fn cancellation_outcome(lifecycle: &SharedRequestLifecycle) -> CancellationOutcome {
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

pub(crate) fn reconcile_cancellation_worker_outcome(
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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn cancel_active_request(mut request: ActiveRequest) -> CancellationOutcome {
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

pub(crate) async fn join_or_handoff_subagent_request(
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

pub(crate) async fn cancel_all_requests(
    active: &mut HashMap<String, ActiveRequest>,
) -> Result<(), String> {
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

pub(crate) fn queue_response(
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

pub(crate) fn bounded_response_frame(response: &Value) -> Result<Vec<u8>, String> {
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
    // Keep the large dispatch future off the request thread's stack.
    let dispatch = Box::pin(handle_request_with_cancellation(
        project_root,
        config,
        request,
        None,
        &cancellation_audit,
    ));
    let mut handled =
        SessionStore::with_lock_policy(MCP_CANCELLATION_AUDIT_LOCK_TIMEOUT, dispatch).await;
    handled.complete_audit();
    handled.response
}

pub(crate) async fn handle_request_with_cancellation(
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
