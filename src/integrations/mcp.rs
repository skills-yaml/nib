use super::mcp_framing::{encode_json, encode_json_line, read_async_frame, MAX_MCP_FRAME_BYTES};
use crate::config::{
    is_valid_mcp_server_name, McpServerEntry, NibConfig, MAX_MCP_CONFIGURED_SERVERS,
    MAX_MCP_REQUEST_TIMEOUT_SECS,
};
use crate::tools::executor::{
    contains_generic_secret, normalized_encoded_sensitive_values, redact_text, redact_value,
};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, AhoCorasickKind, MatchKind};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex, Notify, Semaphore};
use tokio::time::Instant;

const MAX_EXTERNAL_TOOLS_PER_SERVER: usize = 256;
const MAX_EXTERNAL_TOOLS_TOTAL: usize = 1_024;
const MAX_EXTERNAL_TOOL_NAME_BYTES: usize = 128;
const MAX_EXTERNAL_TOOL_DESCRIPTION_BYTES: usize = 4 * 1024;
const MAX_EXTERNAL_TOOL_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_PENDING_REQUESTS: usize = 32;
const MAX_MCP_SENSITIVE_VALUE_BYTES: usize = 64 * 1024;
const MAX_MCP_SENSITIVE_SPELLINGS: usize = 65_536;
const MAX_MCP_SENSITIVE_SPELLING_BYTES: usize = 8 * 1024 * 1024;
const MCP_METADATA_SECRET_ERROR: &str =
    "MCP tools/list metadata rejected by the secret boundary: [REDACTED]";
const MCP_SECRET_BOUNDARY_LIMIT_ERROR: &str =
    "MCP sensitive-value boundary exceeds its safe resource limit";

#[derive(Debug, Error)]
pub enum McpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid MCP configuration: {0}")]
    InvalidConfiguration(String),
    #[error("server not found: {0}")]
    ServerNotFound(String),
    #[error("tool not advertised by its MCP server: {0}")]
    ToolNotFound(String),
    #[error("RPC error: {0}")]
    Rpc(String),
}

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, String>>>;
type SharedPendingMap = Arc<StdMutex<PendingMap>>;

#[derive(Debug)]
struct OutboundFrame {
    bytes: Vec<u8>,
    delivery: Option<oneshot::Sender<Result<(), String>>>,
}

impl OutboundFrame {
    fn unacknowledged(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            delivery: None,
        }
    }

    fn acknowledged(bytes: Vec<u8>, delivery: oneshot::Sender<Result<(), String>>) -> Self {
        Self {
            bytes,
            delivery: Some(delivery),
        }
    }

    fn acknowledge(&mut self, result: Result<(), String>) {
        if let Some(delivery) = self.delivery.take() {
            let _ = delivery.send(result);
        }
    }
}

#[derive(Clone, Default)]
struct TransportHooks {
    #[cfg(test)]
    write_barrier: Option<Arc<TransportWriteBarrier>>,
}

impl TransportHooks {
    async fn before_write(&self) {
        #[cfg(test)]
        if let Some(barrier) = &self.write_barrier {
            barrier.before_write().await;
        }
    }

    fn fatal_enqueued(&self) {
        #[cfg(test)]
        if let Some(barrier) = &self.write_barrier {
            barrier.mark_fatal_enqueued();
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct TransportWriteBarrier {
    armed: AtomicBool,
    block_write_number: AtomicU64,
    writes_seen: AtomicU64,
    write_blocked: AtomicBool,
    fatal_enqueued: AtomicBool,
    write_blocked_notify: Notify,
    fatal_enqueued_notify: Notify,
    release_write: Notify,
}

#[cfg(test)]
impl TransportWriteBarrier {
    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    fn arm_on_write(&self, write_number: u64) {
        self.block_write_number
            .store(write_number, Ordering::Release);
    }

    async fn before_write(&self) {
        let write_number = self.writes_seen.fetch_add(1, Ordering::AcqRel) + 1;
        let configured_write = self.block_write_number.load(Ordering::Acquire);
        if !self.armed.swap(false, Ordering::AcqRel) && configured_write != write_number {
            return;
        }
        self.write_blocked.store(true, Ordering::Release);
        self.write_blocked_notify.notify_waiters();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.release_write.notified()).await;
    }

    fn mark_fatal_enqueued(&self) {
        self.fatal_enqueued.store(true, Ordering::Release);
        self.fatal_enqueued_notify.notify_waiters();
    }

    async fn wait_for_write_block(&self) {
        while !self.write_blocked.load(Ordering::Acquire) {
            let notified = self.write_blocked_notify.notified();
            if self.write_blocked.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }

    async fn wait_for_fatal_enqueue(&self) {
        while !self.fatal_enqueued.load(Ordering::Acquire) {
            let notified = self.fatal_enqueued_notify.notified();
            if self.fatal_enqueued.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }

    fn release_write(&self) {
        self.release_write.notify_one();
    }
}

struct TransportCompletion {
    stopped: AtomicBool,
    notify: Notify,
}

impl TransportCompletion {
    fn new() -> Self {
        Self {
            stopped: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn finish(&self) {
        self.stopped.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        while !self.stopped.load(Ordering::Acquire) {
            let notified = self.notify.notified();
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
    }
}

struct TransportCompletionGuard(Arc<TransportCompletion>);

impl Drop for TransportCompletionGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

struct TransportSupervisor {
    shutdown_tx: mpsc::Sender<()>,
    completion: Arc<TransportCompletion>,
    thread: StdMutex<Option<std::thread::JoinHandle<()>>>,
}

struct TransportState {
    pending: SharedPendingMap,
    transport_error: Arc<Mutex<Option<String>>>,
    secret_matcher: Arc<McpSecretMatcher>,
    hooks: TransportHooks,
}

enum ManagedRootOutcome {
    Exited(String),
    WaitFailed(String),
    Terminated,
}

struct TransportStartupGuard {
    shutdown_tx: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
    armed: bool,
}

impl TransportStartupGuard {
    fn new(shutdown_tx: mpsc::Sender<()>, thread: std::thread::JoinHandle<()>) -> Self {
        Self {
            shutdown_tx,
            thread: Some(thread),
            armed: true,
        }
    }

    fn disarm(mut self) -> std::thread::JoinHandle<()> {
        self.armed = false;
        self.thread
            .take()
            .expect("MCP startup guard always owns its supervisor thread")
    }
}

impl Drop for TransportStartupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.shutdown_tx.try_send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl TransportSupervisor {
    async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(()).await;
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        } else {
            self.completion.wait().await;
        }
    }
}

impl Drop for TransportSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.try_send(());
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

struct PendingRequestGuard {
    pending: SharedPendingMap,
    request_tx: mpsc::Sender<OutboundFrame>,
    id: u64,
    enqueued: bool,
    completed: bool,
}

impl PendingRequestGuard {
    fn register(
        pending: SharedPendingMap,
        request_tx: mpsc::Sender<OutboundFrame>,
        id: u64,
        reply: oneshot::Sender<Result<Value, String>>,
    ) -> Self {
        lock_pending(&pending).insert(id, reply);
        Self {
            pending,
            request_tx,
            id,
            enqueued: false,
            completed: false,
        }
    }

    fn mark_enqueued(&mut self) {
        self.enqueued = true;
    }

    fn mark_completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        lock_pending(&self.pending).remove(&self.id);
        if !self.enqueued || self.completed {
            return;
        }

        // MCP cancellation is advisory: a server may ignore it, and side effects
        // that already happened cannot be rolled back by this notification.
        if let Ok(frame) = encode_json_line(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {
                "requestId": self.id,
                "reason": "request cancelled by nib"
            }
        })) {
            let _ = self
                .request_tx
                .try_send(OutboundFrame::unacknowledged(frame));
        }
    }
}

#[derive(Debug)]
enum PendingResponse {
    Received(Result<Value, String>),
    ChannelClosed,
    TimedOut,
}

fn lock_pending(pending: &SharedPendingMap) -> StdMutexGuard<'_, PendingMap> {
    pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug)]
struct McpSecretMatcher {
    exact: Option<AhoCorasick>,
}

impl McpSecretMatcher {
    fn new(sensitive_values: &[String]) -> Result<Self, McpError> {
        let spellings = mcp_sensitive_spellings(
            sensitive_values,
            MAX_MCP_SENSITIVE_SPELLINGS,
            MAX_MCP_SENSITIVE_SPELLING_BYTES,
        )?;
        let exact = if spellings.is_empty() {
            None
        } else {
            Some(
                AhoCorasickBuilder::new()
                    .kind(Some(AhoCorasickKind::ContiguousNFA))
                    .match_kind(MatchKind::LeftmostFirst)
                    .build(&spellings)
                    .map_err(|_| {
                        McpError::InvalidConfiguration(MCP_SECRET_BOUNDARY_LIMIT_ERROR.to_string())
                    })?,
            )
        };
        Ok(Self { exact })
    }

    fn contains(&self, value: &str) -> bool {
        contains_generic_secret(value)
            || self
                .exact
                .as_ref()
                .is_some_and(|matcher| matcher.is_match(value))
    }

    fn redact(&self, value: &str) -> String {
        let generically_redacted = redact_text(value);
        let redacted = match &self.exact {
            Some(matcher) if matcher.is_match(&generically_redacted) => {
                let mut redacted = String::with_capacity(generically_redacted.len());
                let mut cursor = 0usize;
                for matched in matcher.find_iter(generically_redacted.as_bytes()) {
                    redacted.push_str(&generically_redacted[cursor..matched.start()]);
                    redacted.push_str("[REDACTED]");
                    cursor = matched.end();
                }
                redacted.push_str(&generically_redacted[cursor..]);
                redacted
            }
            _ => generically_redacted,
        };
        let mut safe = crate::interactive::control_safe_text(&redacted, true);
        if safe.len() > MAX_MCP_SENSITIVE_VALUE_BYTES {
            let mut end = MAX_MCP_SENSITIVE_VALUE_BYTES.saturating_sub(3);
            while end > 0 && !safe.is_char_boundary(end) {
                end -= 1;
            }
            safe.truncate(end);
            safe.push_str("...");
        }
        safe
    }
}

#[derive(Clone, Debug)]
struct ExternalTool {
    name: String,
    description: String,
    input_schema: Value,
}

pub struct McpManager {
    servers: HashMap<String, Arc<McpServerClient>>,
    tools: HashMap<String, Vec<ExternalTool>>,
    secret_matcher: Arc<McpSecretMatcher>,
}

struct McpServerClient {
    name: String,
    request_tx: mpsc::Sender<OutboundFrame>,
    pending: SharedPendingMap,
    request_slots: Arc<Semaphore>,
    transport_error: Arc<Mutex<Option<String>>>,
    next_id: AtomicU64,
    transport: TransportSupervisor,
    request_timeout: Duration,
    secret_matcher: Arc<McpSecretMatcher>,
}

impl McpManager {
    pub async fn new(
        config: &HashMap<String, McpServerEntry>,
        sensitive_values: &[String],
    ) -> Result<Self, McpError> {
        let secret_matcher = Arc::new(McpSecretMatcher::new(sensitive_values)?);
        validate_server_config(config).map_err(|error| redact_mcp_error(error, &secret_matcher))?;
        let mut entries = config.iter().collect::<Vec<_>>();
        entries.sort_by_key(|(name, _)| *name);
        let mut servers: HashMap<String, Arc<McpServerClient>> = HashMap::new();
        let mut tools = HashMap::new();
        let mut tool_count = 0usize;
        for (name, entry) in entries {
            let client = match McpServerClient::start_with_matcher(
                name.clone(),
                entry,
                Arc::clone(&secret_matcher),
            )
            .await
            {
                Ok(client) => Arc::new(client),
                Err(error) => {
                    for client in servers.values() {
                        client.shutdown().await;
                    }
                    return Err(redact_mcp_error(error, &secret_matcher));
                }
            };
            servers.insert(name.clone(), Arc::clone(&client));

            let server_tools = match client.raw_tools().await {
                Ok(tools) => tools,
                Err(error) => {
                    for client in servers.values() {
                        client.shutdown().await;
                    }
                    return Err(redact_mcp_error(error, &secret_matcher));
                }
            };
            tool_count = tool_count.saturating_add(server_tools.len());
            if tool_count > MAX_EXTERNAL_TOOLS_TOTAL {
                for client in servers.values() {
                    client.shutdown().await;
                }
                return Err(McpError::Rpc(format!(
                    "external MCP tools exceed the {MAX_EXTERNAL_TOOLS_TOTAL}-tool aggregate limit"
                )));
            }
            let exposed_tools = Value::Array(
                server_tools
                    .iter()
                    .map(|tool| exposed_external_tool(name, tool))
                    .collect(),
            );
            if let Err(error) =
                validate_external_tool_metadata_boundary(&exposed_tools, &secret_matcher)
            {
                for client in servers.values() {
                    client.shutdown().await;
                }
                return Err(redact_mcp_error(error, &secret_matcher));
            }
            tools.insert(name.clone(), server_tools);
        }
        Ok(Self {
            servers,
            tools,
            secret_matcher,
        })
    }

    pub async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let result = async {
            let mut all_tools = Vec::new();
            let mut server_names = self.tools.keys().collect::<Vec<_>>();
            server_names.sort();
            for server_name in server_names {
                let client = self.servers.get(server_name).ok_or_else(|| {
                    McpError::Rpc("MCP discovery cache has no matching server".to_string())
                })?;
                if let Some(error) = client.transport_error.lock().await.clone() {
                    return Err(McpError::Rpc(error));
                }
                let server_tools = self.tools.get(server_name).ok_or_else(|| {
                    McpError::Rpc("MCP server has no discovery cache".to_string())
                })?;
                for tool in server_tools {
                    all_tools.push(exposed_external_tool(server_name, tool));
                }
            }
            Ok(all_tools)
        }
        .await;
        result.map_err(|error| redact_mcp_error(error, &self.secret_matcher))
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let result = async {
            let (server_name, original_name) = name
                .split_once("::")
                .filter(|(server, tool)| !server.is_empty() && is_valid_external_tool_name(tool))
                .ok_or_else(|| McpError::ServerNotFound("invalid tool name format".to_string()))?;
            let client = self
                .servers
                .get(server_name)
                .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;

            let advertised = self
                .tools
                .get(server_name)
                .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;
            if !advertised.iter().any(|tool| tool.name == original_name) {
                return Err(McpError::ToolNotFound(name.to_string()));
            }

            client
                .request(
                    "tools/call",
                    json!({"name": original_name, "arguments": arguments}),
                )
                .await
                .map_err(McpError::Rpc)
        }
        .await;
        result.map_err(|error| redact_mcp_error(error, &self.secret_matcher))
    }
}

impl McpServerClient {
    #[cfg(test)]
    pub(crate) async fn start(
        name: String,
        entry: &McpServerEntry,
        sensitive_values: Arc<Vec<String>>,
    ) -> Result<Self, McpError> {
        let secret_matcher = Arc::new(McpSecretMatcher::new(&sensitive_values)?);
        Self::start_with_matcher(name, entry, secret_matcher).await
    }

    async fn start_with_matcher(
        name: String,
        entry: &McpServerEntry,
        secret_matcher: Arc<McpSecretMatcher>,
    ) -> Result<Self, McpError> {
        Self::start_with_transport_hooks(
            name,
            entry,
            Arc::clone(&secret_matcher),
            TransportHooks::default(),
        )
        .await
        .map_err(|error| redact_mcp_error(error, &secret_matcher))
    }

    #[cfg(test)]
    async fn start_with_write_barrier(
        name: String,
        entry: &McpServerEntry,
        sensitive_values: Arc<Vec<String>>,
        write_barrier: Arc<TransportWriteBarrier>,
    ) -> Result<Self, McpError> {
        let secret_matcher = Arc::new(McpSecretMatcher::new(&sensitive_values)?);
        Self::start_with_transport_hooks(
            name,
            entry,
            Arc::clone(&secret_matcher),
            TransportHooks {
                write_barrier: Some(write_barrier),
            },
        )
        .await
        .map_err(|error| redact_mcp_error(error, &secret_matcher))
    }

    async fn start_with_transport_hooks(
        name: String,
        entry: &McpServerEntry,
        secret_matcher: Arc<McpSecretMatcher>,
        hooks: TransportHooks,
    ) -> Result<Self, McpError> {
        validate_server_entry(&name, entry)?;

        let (request_tx, mut request_rx) = mpsc::channel::<OutboundFrame>(MAX_PENDING_REQUESTS);
        let pending: SharedPendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let transport_error = Arc::new(Mutex::new(None));
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let completion = Arc::new(TransportCompletion::new());
        let thread_completion = Arc::clone(&completion);
        let thread_pending = Arc::clone(&pending);
        let thread_transport_error = Arc::clone(&transport_error);
        let thread_secret_matcher = Arc::clone(&secret_matcher);
        let thread_entry = entry.clone();
        let (startup_tx, startup_rx) = oneshot::channel();
        let supervisor_thread = std::thread::Builder::new()
            .name(format!("nib-mcp-{name}"))
            .spawn(move || {
                let _completion = TransportCompletionGuard(thread_completion);
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = startup_tx.send(Err(McpError::Io(error)));
                        return;
                    }
                };
                runtime.block_on(supervise_mcp_transport(
                    thread_entry,
                    TransportState {
                        pending: thread_pending,
                        transport_error: thread_transport_error,
                        secret_matcher: thread_secret_matcher,
                        hooks,
                    },
                    &mut request_rx,
                    &mut shutdown_rx,
                    startup_tx,
                ));
            })
            .map_err(McpError::Io)?;
        let startup_guard = TransportStartupGuard::new(shutdown_tx.clone(), supervisor_thread);
        match startup_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(error);
            }
            Err(_) => {
                return Err(McpError::Io(std::io::Error::other(
                    "MCP transport supervisor stopped during startup",
                )));
            }
        }
        let supervisor_thread = startup_guard.disarm();

        let client = Self {
            name,
            request_tx,
            pending,
            request_slots: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
            transport_error,
            next_id: AtomicU64::new(1),
            transport: TransportSupervisor {
                shutdown_tx,
                completion,
                thread: StdMutex::new(Some(supervisor_thread)),
            },
            request_timeout: Duration::from_secs(entry.request_timeout_secs),
            secret_matcher,
        };
        if let Err(error) = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "nib", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await
        {
            client.shutdown().await;
            return Err(McpError::Rpc(format!("initialization failed: {error}")));
        }
        if let Err(error) = client.notify("notifications/initialized", json!({})).await {
            client.shutdown().await;
            return Err(McpError::Rpc(error));
        }
        Ok(client)
    }

    async fn shutdown(&self) {
        self.transport.shutdown().await;
    }

    async fn raw_tools(&self) -> Result<Vec<ExternalTool>, McpError> {
        let result = self
            .request("tools/list", json!({}))
            .await
            .map_err(McpError::Rpc)?;
        parse_external_tools(&self.name, &result, &self.secret_matcher)
            .map_err(|error| redact_mcp_error(error, &self.secret_matcher))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_unredacted(method, params)
            .await
            .map_err(|error| self.redact_error(&error))
    }

    async fn request_unredacted(&self, method: &str, params: Value) -> Result<Value, String> {
        if let Some(error) = self.transport_error.lock().await.clone() {
            return Err(error);
        }
        let deadline = Instant::now() + self.request_timeout;
        let _request_slot =
            tokio::time::timeout_at(deadline, Arc::clone(&self.request_slots).acquire_owned())
                .await
                .map_err(|_| format!("MCP request '{method}' timed out"))?
                .map_err(|_| "MCP request limiter closed".to_string())?;
        let id = self
            .next_id
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |id| id.checked_add(1))
            .map_err(|_| "MCP request id space exhausted".to_string())?;
        let frame = encode_json_line(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .map_err(|error| format!("cannot encode MCP request '{method}': {error}"))?;
        let (reply, response) = oneshot::channel();
        let mut pending_request = PendingRequestGuard::register(
            Arc::clone(&self.pending),
            self.request_tx.clone(),
            id,
            reply,
        );
        if let Some(error) = self.transport_error.lock().await.clone() {
            return Err(error);
        }

        match tokio::time::timeout_at(
            deadline,
            self.request_tx.send(OutboundFrame::unacknowledged(frame)),
        )
        .await
        {
            Ok(Ok(())) => pending_request.mark_enqueued(),
            Ok(Err(_)) => return Err("MCP client channel closed".to_string()),
            Err(_) => return Err(format!("MCP request '{method}' timed out")),
        }

        match await_pending_response(deadline, response).await {
            PendingResponse::Received(result) => {
                pending_request.mark_completed();
                result
            }
            PendingResponse::ChannelClosed => Err("MCP response channel closed".to_string()),
            PendingResponse::TimedOut => Err(format!("MCP request '{method}' timed out")),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.notify_unredacted(method, params)
            .await
            .map_err(|error| self.redact_error(&error))
    }

    async fn notify_unredacted(&self, method: &str, params: Value) -> Result<(), String> {
        if let Some(error) = self.transport_error.lock().await.clone() {
            return Err(error);
        }
        let frame =
            encode_json_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
                .map_err(|error| format!("cannot encode MCP notification '{method}': {error}"))?;
        let deadline = Instant::now() + self.request_timeout;
        let (delivery_tx, delivery_rx) = oneshot::channel();
        tokio::time::timeout_at(
            deadline,
            self.request_tx
                .send(OutboundFrame::acknowledged(frame, delivery_tx)),
        )
        .await
        .map_err(|_| format!("MCP notification '{method}' timed out"))?
        .map_err(|_| "MCP client channel closed".to_string())?;
        tokio::time::timeout_at(deadline, delivery_rx)
            .await
            .map_err(|_| format!("MCP notification '{method}' delivery timed out"))?
            .map_err(|_| "MCP notification delivery channel closed".to_string())?
    }

    fn redact_error(&self, error: &str) -> String {
        self.secret_matcher.redact(error)
    }
}

async fn supervise_mcp_transport(
    entry: McpServerEntry,
    state: TransportState,
    request_rx: &mut mpsc::Receiver<OutboundFrame>,
    shutdown_rx: &mut mpsc::Receiver<()>,
    startup_tx: oneshot::Sender<Result<(), McpError>>,
) {
    let TransportState {
        pending,
        transport_error,
        secret_matcher,
        hooks,
    } = state;
    let mut command = mcp_child_command(&entry);
    let mut child = match crate::sandbox::spawn_managed_child(&mut command) {
        Ok(child) => child,
        Err(error) => {
            let _ = startup_tx.send(Err(McpError::Io(error)));
            return;
        }
    };
    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            child.terminate_and_reap().await;
            let _ = startup_tx.send(Err(McpError::Io(std::io::Error::other(
                "MCP server has no stdin",
            ))));
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            child.terminate_and_reap().await;
            let _ = startup_tx.send(Err(McpError::Io(std::io::Error::other(
                "MCP server has no stdout",
            ))));
            return;
        }
    };
    let mut stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            child.terminate_and_reap().await;
            let _ = startup_tx.send(Err(McpError::Io(std::io::Error::other(
                "MCP server has no stderr",
            ))));
            return;
        }
    };
    // Descendants may inherit the transport handles, so EOF is not proof that the
    // direct server is alive. This task is the managed child's sole waiter.
    let (child_terminate_tx, child_terminate_rx) = oneshot::channel();
    let (child_outcome_tx, mut child_outcome_rx) = oneshot::channel();
    let child_watcher = tokio::spawn(watch_mcp_root(child, child_terminate_rx, child_outcome_tx));
    let mut child_terminate_tx = Some(child_terminate_tx);
    let mut child_outcome_observed = false;
    let stderr_task = tokio::spawn(async move {
        // MCP stderr is server-controlled. Drain it so the child cannot block,
        // but never let it bypass the configured redaction boundary.
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });
    let (fatal_tx, mut fatal_rx) = mpsc::channel::<String>(1);
    let reader_pending = Arc::clone(&pending);
    let reader_secret_matcher = Arc::clone(&secret_matcher);
    let reader_hooks = hooks.clone();
    let reader_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let failure = loop {
            let frame = match read_async_frame(&mut reader).await {
                Ok(Some(frame)) => frame,
                Ok(None) => break "MCP server closed stdout".to_string(),
                Err(error) => break format!("invalid MCP stdout frame: {error}"),
            };
            if frame.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value = match serde_json::from_slice::<Value>(&frame) {
                Ok(value) => value,
                Err(error) => break format!("invalid JSON from MCP server: {error}"),
            };
            if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                break "MCP response has an invalid JSON-RPC version".to_string();
            }
            let Some(id_value) = value.get("id") else {
                continue;
            };
            let Some(id) = id_value.as_u64() else {
                break "MCP response has a non-numeric request id".to_string();
            };
            let reply = lock_pending(&reader_pending).remove(&id);
            if let Some(reply) = reply {
                let response = match (value.get("result"), value.get("error")) {
                    (Some(result), None) => Ok(result.clone()),
                    (None, Some(error)) => {
                        let structurally_redacted = redact_value(error.clone()).to_string();
                        Err(reader_secret_matcher.redact(&structurally_redacted))
                    }
                    _ => {
                        Err("RPC response must contain exactly one of result or error".to_string())
                    }
                };
                let _ = reply.send(response);
            }
        };

        if fatal_tx.send(failure).await.is_ok() {
            reader_hooks.fatal_enqueued();
        }
    });
    let _ = startup_tx.send(Ok(()));

    let failure = 'transport: loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => {
                break "MCP client shut down".to_string();
            }
            Some(failure) = fatal_rx.recv() => {
                break failure;
            }
            outcome = &mut child_outcome_rx => {
                child_outcome_observed = true;
                break managed_root_failure(outcome);
            }
            frame = request_rx.recv() => {
                let Some(mut frame) = frame else {
                    break "MCP client request channel closed".to_string();
                };
                hooks.before_write().await;
                let write = async {
                    stdin.write_all(&frame.bytes).await?;
                    stdin.flush().await
                };
                tokio::pin!(write);
                tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        frame.acknowledge(Err("MCP client shut down before frame delivery".to_string()));
                        break 'transport "MCP client shut down".to_string();
                    }
                    Some(failure) = fatal_rx.recv() => {
                        frame.acknowledge(Err(failure.clone()));
                        break 'transport failure;
                    }
                    outcome = &mut child_outcome_rx => {
                        child_outcome_observed = true;
                        let failure = managed_root_failure(outcome);
                        frame.acknowledge(Err(failure.clone()));
                        break 'transport failure;
                    }
                    result = &mut write => {
                        if let Err(error) = result {
                            let error = format!("failed to write MCP stdin: {error}");
                            frame.acknowledge(Err(error.clone()));
                            break 'transport error;
                        }
                        frame.acknowledge(Ok(()));
                    }
                }
            }
        }
    };

    request_rx.close();
    while let Ok(mut frame) = request_rx.try_recv() {
        frame.acknowledge(Err(failure.clone()));
    }
    reader_task.abort();
    let _ = reader_task.await;
    stderr_task.abort();
    let _ = stderr_task.await;
    fail_transport(&pending, &transport_error, failure, &secret_matcher).await;
    if !child_outcome_observed {
        if let Some(child_terminate_tx) = child_terminate_tx.take() {
            let _ = child_terminate_tx.send(());
        }
    }
    let _ = child_watcher.await;
}

async fn watch_mcp_root(
    mut child: crate::sandbox::ManagedChild,
    mut terminate_rx: oneshot::Receiver<()>,
    outcome_tx: oneshot::Sender<ManagedRootOutcome>,
) {
    let outcome = tokio::select! {
        biased;
        _ = &mut terminate_rx => {
            child.terminate_and_reap().await;
            ManagedRootOutcome::Terminated
        }
        result = child.wait() => match result {
            Ok(status) => ManagedRootOutcome::Exited(status.to_string()),
            Err(error) => {
                let error = error.to_string();
                child.terminate_and_reap().await;
                ManagedRootOutcome::WaitFailed(error)
            }
        }
    };
    let _ = outcome_tx.send(outcome);
}

fn managed_root_failure(outcome: Result<ManagedRootOutcome, oneshot::error::RecvError>) -> String {
    match outcome {
        Ok(ManagedRootOutcome::Exited(status)) => {
            format!("MCP server process exited: {status}")
        }
        Ok(ManagedRootOutcome::WaitFailed(error)) => {
            format!("failed to wait for MCP server process: {error}")
        }
        Ok(ManagedRootOutcome::Terminated) => "MCP server process stopped unexpectedly".to_string(),
        Err(_) => "MCP server process watcher stopped unexpectedly".to_string(),
    }
}

fn mcp_child_command(entry: &McpServerEntry) -> Command {
    let mut command = Command::new(&entry.command);
    command
        .args(&entry.args)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::sandbox::apply_child_environment(&mut command, &entry.env);
    if let Some(cwd) = &entry.cwd {
        command.current_dir(cwd);
    }
    command
}

async fn await_pending_response(
    deadline: Instant,
    response: oneshot::Receiver<Result<Value, String>>,
) -> PendingResponse {
    match tokio::time::timeout_at(deadline, response).await {
        Ok(Ok(result)) => PendingResponse::Received(result),
        Ok(Err(_)) => PendingResponse::ChannelClosed,
        Err(_) => PendingResponse::TimedOut,
    }
}

async fn fail_transport(
    pending: &SharedPendingMap,
    transport_error: &Arc<Mutex<Option<String>>>,
    error: String,
    secret_matcher: &McpSecretMatcher,
) {
    let error = secret_matcher.redact(&error);
    let error = {
        let mut stored = transport_error.lock().await;
        stored.get_or_insert(error).clone()
    };
    let replies: Vec<_> = lock_pending(pending)
        .drain()
        .map(|(_, reply)| reply)
        .collect();
    for reply in replies {
        let _ = reply.send(Err(error.clone()));
    }
}

fn redact_mcp_error(error: McpError, secret_matcher: &McpSecretMatcher) -> McpError {
    let redact = |value: String| secret_matcher.redact(&value);
    match error {
        McpError::Io(error) => {
            McpError::Io(std::io::Error::new(error.kind(), redact(error.to_string())))
        }
        McpError::Json(error) => McpError::Json(serde_json::Error::io(std::io::Error::new(
            error
                .io_error_kind()
                .unwrap_or(std::io::ErrorKind::InvalidData),
            redact(error.to_string()),
        ))),
        McpError::InvalidConfiguration(error) => McpError::InvalidConfiguration(redact(error)),
        McpError::ServerNotFound(error) => McpError::ServerNotFound(redact(error)),
        McpError::ToolNotFound(error) => McpError::ToolNotFound(redact(error)),
        McpError::Rpc(error) => McpError::Rpc(redact(error)),
    }
}

fn mcp_sensitive_spellings(
    sensitive_values: &[String],
    max_count: usize,
    max_bytes: usize,
) -> Result<Vec<String>, McpError> {
    let mut spellings = HashSet::new();
    let mut spelling_bytes = 0usize;
    for sensitive in sensitive_values {
        if sensitive.is_empty() {
            continue;
        }
        if sensitive.len() > MAX_MCP_SENSITIVE_VALUE_BYTES {
            return Err(McpError::InvalidConfiguration(
                MCP_SECRET_BOUNDARY_LIMIT_ERROR.to_string(),
            ));
        }
        let normalized = normalized_encoded_sensitive_values([sensitive.clone()]);
        let percent_variants = [sensitive.as_str(), sensitive.trim()]
            .into_iter()
            .flat_map(|spelling| {
                let upper = percent_encode_sensitive_spelling(spelling, false);
                let lower = percent_encode_sensitive_spelling(spelling, true);
                [
                    upper.clone(),
                    upper.replace('%', "%25"),
                    lower.clone(),
                    lower.replace('%', "%25"),
                ]
            })
            .collect::<Vec<_>>();
        let mut candidates = normalized
            .into_iter()
            .map(|spelling| {
                let depth = u8::from(spelling != sensitive.as_str());
                (spelling, depth)
            })
            .collect::<Vec<_>>();
        candidates.extend(percent_variants.into_iter().map(|spelling| (spelling, 2)));
        while let Some((spelling, depth)) = candidates.pop() {
            if spelling.is_empty() || spellings.contains(&spelling) {
                continue;
            }
            let Some(next_bytes) = spelling_bytes.checked_add(spelling.len()) else {
                return Err(McpError::InvalidConfiguration(
                    MCP_SECRET_BOUNDARY_LIMIT_ERROR.to_string(),
                ));
            };
            if spellings.len() >= max_count || next_bytes > max_bytes {
                return Err(McpError::InvalidConfiguration(
                    MCP_SECRET_BOUNDARY_LIMIT_ERROR.to_string(),
                ));
            }
            spelling_bytes = next_bytes;
            spellings.insert(spelling.clone());
            if depth >= 2 {
                continue;
            }
            if let Ok(json) = serde_json::to_string(&spelling) {
                if json.len() >= 2 {
                    candidates.push((json[1..json.len() - 1].to_string(), depth + 1));
                }
                candidates.push((json, depth + 1));
            }
            let debug = format!("{spelling:?}");
            if debug.len() >= 2 {
                candidates.push((debug[1..debug.len() - 1].to_string(), depth + 1));
            }
            candidates.push((debug, depth + 1));
        }
    }
    let mut spellings = spellings.into_iter().collect::<Vec<_>>();
    spellings.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    Ok(spellings)
}

fn percent_encode_sensitive_spelling(value: &str, lowercase: bool) -> String {
    let digits = if lowercase {
        b"0123456789abcdef"
    } else {
        b"0123456789ABCDEF"
    };
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(digits[usize::from(byte >> 4)]));
            encoded.push(char::from(digits[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn validate_server_config(config: &HashMap<String, McpServerEntry>) -> Result<(), McpError> {
    if config.len() > MAX_MCP_CONFIGURED_SERVERS {
        return Err(McpError::InvalidConfiguration(format!(
            "at most {MAX_MCP_CONFIGURED_SERVERS} MCP servers may be configured"
        )));
    }
    for (name, entry) in config {
        validate_server_entry(name, entry)?;
    }
    Ok(())
}

fn validate_server_entry(name: &str, entry: &McpServerEntry) -> Result<(), McpError> {
    if !is_valid_mcp_server_name(name) {
        return Err(McpError::InvalidConfiguration(format!(
            "invalid MCP server name '{name}'"
        )));
    }
    if entry.command.trim().is_empty() {
        return Err(McpError::InvalidConfiguration(format!(
            "MCP server '{name}' has an empty command"
        )));
    }
    if !(1..=MAX_MCP_REQUEST_TIMEOUT_SECS).contains(&entry.request_timeout_secs) {
        return Err(McpError::InvalidConfiguration(format!(
            "MCP server '{name}' request timeout must be between 1 and {MAX_MCP_REQUEST_TIMEOUT_SECS} seconds"
        )));
    }
    if entry
        .cwd
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(McpError::InvalidConfiguration(format!(
            "MCP server '{name}' has an empty working directory"
        )));
    }
    let mut config = NibConfig::default();
    config.mcp.servers.insert(name.to_string(), entry.clone());
    config
        .validate()
        .map_err(|error| McpError::InvalidConfiguration(error.to_string()))?;
    Ok(())
}

fn parse_external_tools(
    server_name: &str,
    result: &Value,
    secret_matcher: &McpSecretMatcher,
) -> Result<Vec<ExternalTool>, McpError> {
    let tools_value = result
        .get("tools")
        .ok_or_else(|| invalid_tools_response(server_name, "tools must be an array"))?;
    let tools = tools_value
        .as_array()
        .ok_or_else(|| invalid_tools_response(server_name, "tools must be an array"))?;
    if tools.len() > MAX_EXTERNAL_TOOLS_PER_SERVER {
        return Err(invalid_tools_response(
            server_name,
            format!(
                "tool count {} exceeds the {MAX_EXTERNAL_TOOLS_PER_SERVER}-tool limit",
                tools.len()
            ),
        ));
    }
    validate_external_tool_metadata_boundary(tools_value, secret_matcher)?;

    let mut names = HashSet::new();
    let mut validated = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let object = tool.as_object().ok_or_else(|| {
            invalid_tools_response(server_name, format!("tool {index} must be an object"))
        })?;
        let name = object.get("name").and_then(Value::as_str).ok_or_else(|| {
            invalid_tools_response(server_name, format!("tool {index} must have a string name"))
        })?;
        if !is_valid_external_tool_name(name) {
            return Err(invalid_tools_response(
                server_name,
                format!(
                    "tool {index} name must be 1 to {MAX_EXTERNAL_TOOL_NAME_BYTES} bytes of ASCII letters, digits, '.', '-', or '_'"
                ),
            ));
        }
        if !names.insert(name) {
            return Err(invalid_tools_response(
                server_name,
                format!("tool name '{name}' is duplicated"),
            ));
        }

        let description = match object.get("description") {
            None => "",
            Some(Value::String(description)) => description,
            Some(_) => {
                return Err(invalid_tools_response(
                    server_name,
                    format!("tool '{name}' description must be a string"),
                ));
            }
        };
        if description.len() > MAX_EXTERNAL_TOOL_DESCRIPTION_BYTES {
            return Err(invalid_tools_response(
                server_name,
                format!(
                    "tool '{name}' description exceeds the {MAX_EXTERNAL_TOOL_DESCRIPTION_BYTES}-byte limit"
                ),
            ));
        }

        let input_schema = object
            .get("inputSchema")
            .or_else(|| object.get("parameters"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        if !input_schema.is_object() {
            return Err(invalid_tools_response(
                server_name,
                format!("tool '{name}' input schema must be an object"),
            ));
        }
        encode_json(&input_schema, MAX_EXTERNAL_TOOL_SCHEMA_BYTES).map_err(|error| {
            invalid_tools_response(
                server_name,
                format!(
                    "tool '{name}' input schema exceeds the {MAX_EXTERNAL_TOOL_SCHEMA_BYTES}-byte limit: {error}"
                ),
            )
        })?;

        validated.push(ExternalTool {
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
        });
    }
    Ok(validated)
}

fn exposed_external_tool(server_name: &str, tool: &ExternalTool) -> Value {
    json!({
        "name": format!("{server_name}::{}", tool.name),
        "description": tool.description.clone(),
        "parameters": tool.input_schema.clone(),
        "x-nib-mcp-server": server_name,
    })
}

fn validate_external_tool_metadata_boundary(
    metadata: &Value,
    secret_matcher: &McpSecretMatcher,
) -> Result<(), McpError> {
    let serialized = encode_json(metadata, MAX_MCP_FRAME_BYTES).map_err(|_| {
        McpError::Rpc("MCP tools/list metadata exceeds the frame limit".to_string())
    })?;
    let serialized = std::str::from_utf8(&serialized)
        .map_err(|_| McpError::Rpc("MCP tools/list metadata is not UTF-8".to_string()))?;
    if contains_terminal_active_control(metadata) || secret_matcher.contains(serialized) {
        return Err(McpError::Rpc(MCP_METADATA_SECRET_ERROR.to_string()));
    }
    Ok(())
}

fn contains_terminal_active_control(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.values().any(contains_terminal_active_control),
        Value::Array(values) => values.iter().any(contains_terminal_active_control),
        Value::String(value) => {
            crate::interactive::control_safe_text(value, true) != value.as_str()
        }
        _ => false,
    }
}

fn invalid_tools_response(server_name: &str, message: impl Into<String>) -> McpError {
    McpError::Rpc(format!(
        "invalid tools/list response from MCP server '{server_name}': {}",
        message.into()
    ))
}

fn is_valid_external_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_EXTERNAL_TOOL_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use serial_test::serial;

    fn tool(name: impl Into<String>) -> Value {
        json!({
            "name": name.into(),
            "description": "test tool",
            "inputSchema": {"type": "object"}
        })
    }

    fn secret_matcher(values: &[String]) -> McpSecretMatcher {
        McpSecretMatcher::new(values).expect("bounded MCP secret matcher")
    }

    #[test]
    fn hostile_oversized_external_schema_is_rejected() {
        let result = json!({
            "tools": [{
                "name": "hostile",
                "inputSchema": {
                    "type": "object",
                    "description": "x".repeat(MAX_EXTERNAL_TOOL_SCHEMA_BYTES)
                }
            }]
        });

        let error = parse_external_tools("fixture", &result, &secret_matcher(&[]))
            .expect_err("oversized external schema must fail closed");

        assert!(error.to_string().contains("input schema exceeds"));
    }

    #[test]
    fn external_tool_count_name_and_description_limits_are_enforced() {
        let too_many = json!({
            "tools": (0..=MAX_EXTERNAL_TOOLS_PER_SERVER)
                .map(|index| tool(format!("tool_{index}")))
                .collect::<Vec<_>>()
        });
        assert!(
            parse_external_tools("fixture", &too_many, &secret_matcher(&[]))
                .expect_err("tool count must be bounded")
                .to_string()
                .contains("tool count")
        );

        let long_name = json!({"tools": [tool("x".repeat(MAX_EXTERNAL_TOOL_NAME_BYTES + 1))]});
        assert!(
            parse_external_tools("fixture", &long_name, &secret_matcher(&[]))
                .expect_err("tool name must be bounded")
                .to_string()
                .contains("tool 0 name")
        );

        let long_description = json!({
            "tools": [{
                "name": "described",
                "description": "x".repeat(MAX_EXTERNAL_TOOL_DESCRIPTION_BYTES + 1),
                "inputSchema": {"type": "object"}
            }]
        });
        assert!(
            parse_external_tools("fixture", &long_description, &secret_matcher(&[]))
                .expect_err("tool description must be bounded")
                .to_string()
                .contains("description exceeds")
        );
    }

    #[test]
    fn successful_tool_metadata_rejects_configured_secret_spellings_atomically() {
        let name_secret = "configuredNameCredential";
        let quoted_secret = "quoted-\"credential\\value";
        let multiline_secret = "multiline-credential\nnext-line";
        let quoted_json = serde_json::to_string(quoted_secret).expect("quoted secret JSON");
        let escaped_quoted = quoted_json[1..quoted_json.len() - 1].to_string();
        let multiline_json =
            serde_json::to_string(multiline_secret).expect("multiline secret JSON");
        let mut properties = serde_json::Map::new();
        properties.insert(escaped_quoted.clone(), json!({"type": "string"}));
        let sensitive_values = secret_matcher(&[
            name_secret.to_string(),
            quoted_secret.to_string(),
            multiline_secret.to_string(),
        ]);
        let cases = [
            (
                "name",
                json!({"name": name_secret, "inputSchema": {"type": "object"}}),
            ),
            (
                "description",
                json!({
                    "name": "description_secret",
                    "description": quoted_secret,
                    "inputSchema": {"type": "object"},
                }),
            ),
            (
                "multiline description",
                json!({
                    "name": "multiline_secret",
                    "description": multiline_secret,
                    "inputSchema": {"type": "object"},
                }),
            ),
            (
                "schema key",
                json!({
                    "name": "schema_key_secret",
                    "inputSchema": {"type": "object", "properties": properties},
                }),
            ),
            (
                "schema value",
                json!({
                    "name": "schema_value_secret",
                    "inputSchema": {"type": "object", "description": quoted_json.clone()},
                }),
            ),
        ];

        for (surface, malicious_tool) in cases {
            let result = json!({
                "tools": [tool("accepted_prefix_must_not_escape"), malicious_tool]
            });
            let error = parse_external_tools("fixture", &result, &sensitive_values)
                .expect_err("secret-bearing metadata must reject the complete server list")
                .to_string();
            assert_eq!(
                error,
                format!("RPC error: {MCP_METADATA_SECRET_ERROR}"),
                "unexpected rejection for {surface}"
            );
        }

        let error = parse_external_tools(
            "fixture",
            &json!({"tools": [
                tool("accepted_prefix_must_not_escape"),
                {"name": name_secret, "inputSchema": {"type": "object"}},
            ]}),
            &sensitive_values,
        )
        .expect_err("secret-bearing metadata must reject the complete server list")
        .to_string();
        assert!(error.len() < 256, "metadata rejection must stay bounded");
        for offending in [
            name_secret,
            quoted_secret,
            multiline_secret,
            quoted_json.as_str(),
            escaped_quoted.as_str(),
            multiline_json.as_str(),
            "accepted_prefix_must_not_escape",
        ] {
            assert!(!error.contains(offending), "metadata escaped: {error}");
        }
    }

    #[test]
    fn mcp_metadata_and_errors_reject_encoded_secrets_and_controls() {
        let secret = "active/credential".to_string();
        let matcher = secret_matcher(std::slice::from_ref(&secret));
        for spelling in [
            "active%2Fcredential",
            r"active\/credential",
            "YWN0aXZlL2NyZWRlbnRpYWw=",
            "YWN0aXZlL2NyZWRlbnRpYWw",
            "\u{1b}[2J",
        ] {
            let result = json!({
                "tools": [{
                    "name": "encoded_secret",
                    "description": format!("metadata={spelling}"),
                    "inputSchema": {"type": "object"},
                }]
            });
            assert_eq!(
                parse_external_tools("fixture", &result, &matcher)
                    .expect_err("unsafe MCP metadata must reject atomically")
                    .to_string(),
                format!("RPC error: {MCP_METADATA_SECRET_ERROR}"),
            );
        }

        let error = matcher.redact(&format!(
            "remote={secret}; percent=active%2Fcredential; base64=YWN0aXZlL2NyZWRlbnRpYWw=; \u{1b}[31m"
        ));
        for forbidden in [
            secret.as_str(),
            "active%2Fcredential",
            "YWN0aXZlL2NyZWRlbnRpYWw=",
        ] {
            assert!(!error.contains(forbidden));
        }
        assert!(!error.contains('\u{1b}'));
    }

    #[test]
    fn successful_tool_metadata_rejects_unconfigured_generic_secret_spelling() {
        for (surface, malicious_tool, spelling) in [
            (
                "name",
                json!({
                    "name": "sk-unconfiguredMetadata123",
                    "inputSchema": {"type": "object"},
                }),
                "sk-unconfiguredMetadata123",
            ),
            (
                "schema value",
                json!({
                    "name": "generic_schema_secret",
                    "inputSchema": {
                        "type": "object",
                        "description": "xai-unconfiguredMetadata123",
                    },
                }),
                "xai-unconfiguredMetadata123",
            ),
            (
                "embedded name",
                json!({
                    "name": "prefix_sk-secretvalue123",
                    "inputSchema": {"type": "object"},
                }),
                "sk-secretvalue123",
            ),
            (
                "embedded schema value",
                json!({
                    "name": "embedded_schema_secret",
                    "inputSchema": {
                        "type": "object",
                        "description": "xxsk-secretvalue123",
                    },
                }),
                "sk-secretvalue123",
            ),
        ] {
            let result = json!({"tools": [malicious_tool]});
            let error = parse_external_tools("fixture", &result, &secret_matcher(&[]))
                .expect_err("generic secret metadata must fail closed")
                .to_string();

            assert_eq!(
                error,
                format!("RPC error: {MCP_METADATA_SECRET_ERROR}"),
                "unexpected rejection for {surface}"
            );
            assert!(!error.contains(spelling));
        }
    }

    #[test]
    fn sensitive_spelling_limits_fail_with_a_constant_error() {
        for result in [
            mcp_sensitive_spellings(&["first".to_string()], 1, usize::MAX),
            mcp_sensitive_spellings(&["first".to_string()], usize::MAX, 1),
        ] {
            let error = result.expect_err("secret spelling resources must be bounded");
            assert_eq!(
                error.to_string(),
                format!("invalid MCP configuration: {MCP_SECRET_BOUNDARY_LIMIT_ERROR}")
            );
        }
    }

    #[test]
    fn final_namespaced_metadata_is_validated_before_exposure() {
        let tool = ExternalTool {
            name: "safe_tool".to_string(),
            description: "safe description".to_string(),
            input_schema: json!({"type": "object"}),
        };
        let configured = "configuredServerCredential".to_string();
        for (server_name, matcher) in [
            (
                configured.as_str(),
                secret_matcher(std::slice::from_ref(&configured)),
            ),
            ("prefix_sk-secretvalue123", secret_matcher(&[])),
        ] {
            let metadata = Value::Array(vec![exposed_external_tool(server_name, &tool)]);
            let error = validate_external_tool_metadata_boundary(&metadata, &matcher)
                .expect_err("namespaced secret must fail before exposure");
            assert_eq!(
                error.to_string(),
                format!("RPC error: {MCP_METADATA_SECRET_ERROR}")
            );
        }
    }

    #[tokio::test]
    async fn timed_out_request_is_removed_from_pending_map() {
        let pending: SharedPendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let (reply, response) = oneshot::channel();
        let mut pending_request =
            PendingRequestGuard::register(Arc::clone(&pending), request_tx, 7, reply);
        pending_request.mark_enqueued();

        assert!(matches!(
            await_pending_response(Instant::now(), response).await,
            PendingResponse::TimedOut
        ));

        drop(pending_request);
        assert!(lock_pending(&pending).is_empty());
        let cancellation = request_rx.recv().await.expect("cancellation notification");
        let cancellation: Value = serde_json::from_slice(&cancellation.bytes).unwrap();
        assert_eq!(cancellation["method"], "notifications/cancelled");
        assert_eq!(cancellation["params"]["requestId"], 7);
    }

    #[test]
    fn request_dropped_before_enqueue_does_not_notify_server() {
        let pending: SharedPendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let (reply, _response) = oneshot::channel();
        let pending_request =
            PendingRequestGuard::register(Arc::clone(&pending), request_tx, 9, reply);

        drop(pending_request);

        assert!(lock_pending(&pending).is_empty());
        assert!(request_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn aborted_request_removes_pending_registration_and_notifies_server() {
        let pending: SharedPendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let (request_tx, mut request_rx) = mpsc::channel(2);
        let task_pending = Arc::clone(&pending);
        let task_request_tx = request_tx.clone();
        let (registered_tx, registered_rx) = oneshot::channel();
        let request = tokio::spawn(async move {
            let (reply, response) = oneshot::channel();
            let mut pending_request = PendingRequestGuard::register(
                Arc::clone(&task_pending),
                task_request_tx.clone(),
                11,
                reply,
            );
            task_request_tx
                .send(OutboundFrame::unacknowledged(b"request\n".to_vec()))
                .await
                .expect("request frame enqueued");
            pending_request.mark_enqueued();
            let _ = registered_tx.send(());
            await_pending_response(Instant::now() + Duration::from_secs(60), response).await
        });

        registered_rx.await.expect("request registered");
        assert_eq!(lock_pending(&pending).len(), 1);
        assert_eq!(
            request_rx.recv().await.expect("request frame").bytes,
            b"request\n"
        );
        request.abort();
        assert!(request.await.expect_err("request aborted").is_cancelled());

        assert!(lock_pending(&pending).is_empty());
        let cancellation = tokio::time::timeout(Duration::from_secs(1), request_rx.recv())
            .await
            .expect("cancellation notification timeout")
            .expect("cancellation notification");
        let cancellation: Value = serde_json::from_slice(&cancellation.bytes).unwrap();
        assert_eq!(cancellation["method"], "notifications/cancelled");
        assert_eq!(cancellation["params"]["requestId"], 11);
    }

    #[test]
    fn manager_rejects_invalid_config_before_starting_processes() {
        let config = HashMap::from([(
            "bad::name".to_string(),
            McpServerEntry {
                command: "must-not-run".to_string(),
                ..McpServerEntry::default()
            },
        )]);

        let error = validate_server_config(&config).expect_err("invalid name must fail");

        assert!(matches!(error, McpError::InvalidConfiguration(_)));

        let invalid_environment = HashMap::from([(
            "fixture".to_string(),
            McpServerEntry {
                command: "must-not-run".to_string(),
                env: HashMap::from([("BAD-NAME".to_string(), "value".to_string())]),
                ..McpServerEntry::default()
            },
        )]);
        assert!(matches!(
            validate_server_config(&invalid_environment),
            Err(McpError::InvalidConfiguration(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initialization_errors_redact_configured_secrets_and_child_stderr_is_not_inherited() {
        let secret = "mcp-\"quoted\\line\nnext";
        let json_spelling = serde_json::to_string(secret).expect("secret JSON spelling");
        let escaped_spelling = json_spelling[1..json_spelling.len() - 1].to_string();
        let debug_spelling = format!("{secret:?}");
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32000, "message": format!("escaped={escaped_spelling}")}
        })
        .to_string();
        let servers = HashMap::from([(
            "fixture".to_string(),
            McpServerEntry {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    format!(
                        "IFS= read -r request; printf '%s\\n' \"$MCP_API_TOKEN\" >&2; printf '%s\\n' '{response}'; sleep 1"
                    ),
                ],
                env: HashMap::from([("MCP_API_TOKEN".to_string(), secret.to_string())]),
                ..McpServerEntry::default()
            },
        )]);

        let error = match McpManager::new(&servers, &[secret.to_string()]).await {
            Ok(_) => panic!("fixture initialization must fail"),
            Err(error) => error,
        };
        let error = error.to_string();

        assert!(!error.contains(secret), "raw secret escaped: {error}");
        assert!(
            !error.contains(&escaped_spelling),
            "JSON-escaped secret leaked: {error}"
        );
        assert!(
            !error.contains(&json_spelling),
            "quoted JSON secret leaked: {error}"
        );
        assert!(
            !error.contains(&debug_spelling),
            "debug secret leaked: {error}"
        );
        assert!(
            error.contains("[REDACTED]"),
            "redaction marker missing: {error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_client_start_normalizes_quoted_and_multiline_secret_spellings() {
        let secret = "direct-\"quoted\\line\nnext";
        let json_spelling = serde_json::to_string(secret).expect("secret JSON spelling");
        let escaped_spelling = json_spelling[1..json_spelling.len() - 1].to_string();
        let debug_spelling = format!("{secret:?}");
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32000,
                "message": format!("json={json_spelling}; escaped={escaped_spelling}; debug={debug_spelling}")
            }
        })
        .to_string();
        let entry = McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("IFS= read -r request; printf '%s\\n' '{response}'"),
            ],
            ..McpServerEntry::default()
        };

        let error = match McpServerClient::start(
            "fixture".to_string(),
            &entry,
            Arc::new(vec![secret.to_string()]),
        )
        .await
        {
            Ok(_) => panic!("fixture initialization must fail"),
            Err(error) => error.to_string(),
        };

        for spelling in [secret, &json_spelling, &escaped_spelling, &debug_spelling] {
            assert!(!error.contains(spelling), "secret spelling leaked: {error}");
        }
        assert!(error.contains("[REDACTED]"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn manager_construction_rejects_secret_bearing_successful_discovery_atomically() {
        let directory = tempfile::tempdir().expect("metadata boundary fixture");
        let pid_file = directory.path().join("metadata-boundary-pids");
        let name_secret = "managerMetadataCredential";
        let quoted_secret = "manager-\"quoted\\credential";
        let multiline_secret = "manager-multiline\ncredential";
        let quoted_json = serde_json::to_string(quoted_secret).expect("quoted secret JSON");
        let escaped_quoted = quoted_json[1..quoted_json.len() - 1].to_string();
        let mut properties = serde_json::Map::new();
        properties.insert(
            escaped_quoted.clone(),
            json!({
                "type": "string",
                "description": quoted_json,
                "default": "xai-genericMetadata123",
            }),
        );
        let response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "tools": [
                    tool("accepted_prefix_must_not_register"),
                    {
                        "name": format!("tool_{name_secret}"),
                        "description": format!("{quoted_secret}; {multiline_secret}"),
                        "inputSchema": {
                            "type": "object",
                            "properties": properties,
                        },
                    }
                ]
            }
        })
        .to_string();
        let servers = HashMap::from([(
            "fixture".to_string(),
            McpServerEntry {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    concat!(
                        "sleep 60 >&- 2>&- & child=$!; ",
                        "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                        "IFS= read -r initialize; ",
                        "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                        "IFS= read -r initialized; ",
                        "IFS= read -r list_tools; ",
                        "printf '%s\\n' \"$MCP_TOOLS_RESPONSE\"; ",
                        "sleep 60"
                    )
                    .to_string(),
                ],
                env: HashMap::from([
                    ("MCP_TOOLS_RESPONSE".to_string(), response),
                    (
                        "MCP_PID_FILE".to_string(),
                        pid_file.to_string_lossy().into_owned(),
                    ),
                ]),
                ..McpServerEntry::default()
            },
        )]);

        let error = match McpManager::new(
            &servers,
            &[
                name_secret.to_string(),
                quoted_secret.to_string(),
                multiline_secret.to_string(),
            ],
        )
        .await
        {
            Ok(_) => panic!("secret-bearing discovery must reject manager construction"),
            Err(error) => error.to_string(),
        };

        assert_eq!(error, format!("RPC error: {MCP_METADATA_SECRET_ERROR}"));
        assert!(error.len() < 256, "metadata rejection must stay bounded");
        for offending in [
            name_secret,
            quoted_secret,
            multiline_secret,
            quoted_json.as_str(),
            escaped_quoted.as_str(),
            "accepted_prefix_must_not_register",
            "xai-genericMetadata123",
        ] {
            assert!(!error.contains(offending), "metadata escaped: {error}");
        }
        let pids = wait_for_mcp_fixture_pids(&pid_file).await;
        assert_mcp_fixture_pids_stop(&pids).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initialized_notification_requires_supervisor_write_acknowledgement() {
        let directory = tempfile::tempdir().expect("initialized delivery fixture");
        let close_file = directory.path().join("close-stdout");
        let entry = McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "IFS= read -r initialize; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                    "while [ ! -e \"$MCP_CLOSE_FILE\" ]; do sleep 0.01; done; ",
                    "exec 1>&-; sleep 60"
                )
                .to_string(),
            ],
            env: HashMap::from([(
                "MCP_CLOSE_FILE".to_string(),
                close_file.to_string_lossy().into_owned(),
            )]),
            ..McpServerEntry::default()
        };
        let barrier = Arc::new(TransportWriteBarrier::default());
        barrier.arm_on_write(2);
        let start_barrier = Arc::clone(&barrier);
        let startup = tokio::spawn(async move {
            McpServerClient::start_with_write_barrier(
                "fixture".to_string(),
                &entry,
                Arc::new(Vec::new()),
                start_barrier,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), barrier.wait_for_write_block())
            .await
            .expect("initialized notification reached write barrier");
        std::fs::write(&close_file, b"close").expect("trigger stdout failure");
        tokio::time::timeout(Duration::from_secs(2), barrier.wait_for_fatal_enqueue())
            .await
            .expect("stdout failure reached supervisor");
        barrier.release_write();

        let result = tokio::time::timeout(Duration::from_secs(2), startup)
            .await
            .expect("startup failure is bounded")
            .expect("startup task");
        let error = match result {
            Ok(_) => panic!("undelivered initialized notification must fail startup"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("closed stdout"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fatal_transport_reaps_server_descendants_and_rejects_late_requests() {
        let directory = tempfile::tempdir().expect("fatal transport fixture");
        let pid_file = directory.path().join("pids");
        let late_request_file = directory.path().join("late-request");
        let servers = HashMap::from([(
            "fixture".to_string(),
            McpServerEntry {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    concat!(
                        "IFS= read -r initialize; ",
                        "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                        "IFS= read -r initialized; ",
                        "IFS= read -r list_tools; ",
                        "printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\\n'; ",
                        "sleep 60 >&- 2>&- & child=$!; ",
                        "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                        "exec 1>&-; ",
                        "if IFS= read -r late; then printf '%s' \"$late\" > \"$MCP_LATE_FILE\"; fi; ",
                        "wait"
                    )
                    .to_string(),
                ],
                env: HashMap::from([
                    (
                        "MCP_PID_FILE".to_string(),
                        pid_file.to_string_lossy().into_owned(),
                    ),
                    (
                        "MCP_LATE_FILE".to_string(),
                        late_request_file.to_string_lossy().into_owned(),
                    ),
                ]),
                ..McpServerEntry::default()
            },
        )]);

        let manager = McpManager::new(&servers, &[])
            .await
            .expect("fixture initializes before closing stdout");
        let ids = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(ids) = std::fs::read_to_string(&pid_file) {
                    break ids;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture process ids");
        let client = manager.servers.get("fixture").expect("fixture client");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if client.transport_error.lock().await.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fatal transport state");
        tokio::time::timeout(Duration::from_secs(2), client.transport.completion.wait())
            .await
            .expect("fatal transport child reap");

        let error = manager
            .list_tools()
            .await
            .expect_err("closed transport must reject later requests");
        assert!(error.to_string().contains("closed stdout"), "{error}");
        assert!(
            !late_request_file.exists(),
            "late request reached failed server"
        );

        let pids = ids
            .split_whitespace()
            .map(|value| value.parse::<i32>().expect("numeric fixture pid"))
            .collect::<Vec<_>>();
        let terminated = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if pids.iter().all(|pid| !mcp_test_process_is_active(*pid)) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        if !terminated {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{}", pids[0])])
                .status();
        }
        assert!(terminated, "fatal MCP transport left descendants running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fatal_enqueue_wins_over_blocked_write_and_drains_pending_once() {
        let directory = tempfile::tempdir().expect("fatal enqueue fixture");
        let pid_file = directory.path().join("fatal-enqueue-pids");
        let close_file = directory.path().join("close-stdout");
        let late_request_file = directory.path().join("late-request");
        let entry = McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "IFS= read -r initialize; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                    "IFS= read -r initialized; ",
                    "sleep 60 >&- 2>&- & child=$!; ",
                    "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                    "while [ ! -e \"$MCP_CLOSE_FILE\" ]; do sleep 0.01; done; ",
                    "exec 1>&-; ",
                    "if IFS= read -r late; then printf '%s' \"$late\" > \"$MCP_LATE_FILE\"; fi; ",
                    "wait"
                )
                .to_string(),
            ],
            env: HashMap::from([
                (
                    "MCP_PID_FILE".to_string(),
                    pid_file.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_CLOSE_FILE".to_string(),
                    close_file.to_string_lossy().into_owned(),
                ),
                (
                    "MCP_LATE_FILE".to_string(),
                    late_request_file.to_string_lossy().into_owned(),
                ),
            ]),
            ..McpServerEntry::default()
        };
        let barrier = Arc::new(TransportWriteBarrier::default());
        let client = Arc::new(
            McpServerClient::start_with_write_barrier(
                "fixture".to_string(),
                &entry,
                Arc::new(Vec::new()),
                Arc::clone(&barrier),
            )
            .await
            .expect("fatal enqueue fixture initializes"),
        );
        let pids = wait_for_mcp_fixture_pids(&pid_file).await;
        barrier.arm();
        let request_client = Arc::clone(&client);
        let request =
            tokio::spawn(async move { request_client.request("tools/list", json!({})).await });
        tokio::time::timeout(Duration::from_secs(2), barrier.wait_for_write_block())
            .await
            .expect("request reached pre-write barrier");

        std::fs::write(&close_file, b"close").expect("trigger fatal stdout closure");
        tokio::time::timeout(Duration::from_secs(2), barrier.wait_for_fatal_enqueue())
            .await
            .expect("fatal transport signal enqueued");
        barrier.release_write();

        let error = request
            .await
            .expect("request task")
            .expect_err("fatal transport rejects blocked request");
        assert!(error.contains("closed stdout"), "{error}");
        tokio::time::timeout(Duration::from_secs(2), client.transport.completion.wait())
            .await
            .expect("fatal transport cleanup");
        assert!(lock_pending(&client.pending).is_empty());
        assert!(
            !late_request_file.exists(),
            "post-fatal frame was delivered to the server"
        );
        assert_mcp_fixture_pids_stop(&pids).await;
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_manager_reaps_healthy_server_and_descendants() {
        let directory = tempfile::tempdir().expect("manager drop fixture");
        let pid_file = directory.path().join("manager-drop-pids");
        let servers = HashMap::from([(
            "fixture".to_string(),
            McpServerEntry {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    concat!(
                        "IFS= read -r initialize; ",
                        "sleep 60 >&- 2>&- & child=$!; ",
                        "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                        "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                        "IFS= read -r initialized; ",
                        "IFS= read -r list_tools; ",
                        "printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\\n'; ",
                        "wait"
                    )
                    .to_string(),
                ],
                env: HashMap::from([(
                    "MCP_PID_FILE".to_string(),
                    pid_file.to_string_lossy().into_owned(),
                )]),
                ..McpServerEntry::default()
            },
        )]);

        let manager = McpManager::new(&servers, &[])
            .await
            .expect("healthy manager initializes");
        let pids = wait_for_mcp_fixture_pids(&pid_file).await;

        drop(manager);

        assert_mcp_fixture_pids_stop(&pids).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_client_start_joins_supervisor_and_reaps_server_tree() {
        let directory = tempfile::tempdir().expect("client startup cancellation fixture");
        let pid_file = directory.path().join("startup-cancel-pids");
        let entry = McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "sleep 60 >&- 2>&- & child=$!; ",
                    "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                    "IFS= read -r initialize; wait"
                )
                .to_string(),
            ],
            env: HashMap::from([(
                "MCP_PID_FILE".to_string(),
                pid_file.to_string_lossy().into_owned(),
            )]),
            ..McpServerEntry::default()
        };
        let startup = tokio::spawn(async move {
            McpServerClient::start("fixture".to_string(), &entry, Arc::new(Vec::new())).await
        });
        let pids = wait_for_mcp_fixture_pids(&pid_file).await;

        startup.abort();
        match startup.await {
            Err(error) if error.is_cancelled() => {}
            Err(error) => panic!("unexpected client startup task failure: {error}"),
            Ok(_) => panic!("client startup task completed instead of being cancelled"),
        }

        assert_mcp_fixture_pids_stop(&pids).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn second_server_init_failure_awaits_cleanup_of_every_started_tree() {
        let directory = tempfile::tempdir().expect("partial manager fixture");
        let first_pid_file = directory.path().join("first-pids");
        let second_pid_file = directory.path().join("second-pids");
        let server = |pid_file: &std::path::Path, response: &str| McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!(
                    concat!(
                        "IFS= read -r initialize; ",
                        "sleep 60 >&- 2>&- & child=$!; ",
                        "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                        "printf '%s\\n' '{}'; ",
                        "IFS= read -r initialized || true; ",
                        "IFS= read -r list_tools || true; ",
                        "printf '%s\\n' '{}'; ",
                        "wait"
                    ),
                    response, r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#
                ),
            ],
            env: HashMap::from([(
                "MCP_PID_FILE".to_string(),
                pid_file.to_string_lossy().into_owned(),
            )]),
            ..McpServerEntry::default()
        };
        let servers = HashMap::from([
            (
                "a_first".to_string(),
                server(&first_pid_file, r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            ),
            (
                "b_second".to_string(),
                server(
                    &second_pid_file,
                    r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"no"}}"#,
                ),
            ),
        ]);

        let error = match McpManager::new(&servers, &[]).await {
            Ok(_) => panic!("second server initialization must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("initialization failed"),
            "{error}"
        );
        let first_pids = wait_for_mcp_fixture_pids(&first_pid_file).await;
        let second_pids = wait_for_mcp_fixture_pids(&second_pid_file).await;

        assert_mcp_fixture_pids_stop(&first_pids).await;
        assert_mcp_fixture_pids_stop(&second_pids).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn second_server_secret_metadata_rejects_atomically_and_cleans_every_tree() {
        let directory = tempfile::tempdir().expect("partial metadata manager fixture");
        let first_pid_file = directory.path().join("first-metadata-pids");
        let second_pid_file = directory.path().join("second-metadata-pids");
        let server = |pid_file: &std::path::Path, response: Value| McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                concat!(
                    "IFS= read -r initialize; ",
                    "sleep 60 >&- 2>&- & child=$!; ",
                    "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                    "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                    "IFS= read -r initialized; ",
                    "IFS= read -r list_tools; ",
                    "printf '%s\\n' \"$MCP_TOOLS_RESPONSE\"; ",
                    "wait"
                )
                .to_string(),
            ],
            env: HashMap::from([
                (
                    "MCP_PID_FILE".to_string(),
                    pid_file.to_string_lossy().into_owned(),
                ),
                ("MCP_TOOLS_RESPONSE".to_string(), response.to_string()),
            ]),
            ..McpServerEntry::default()
        };
        let servers = HashMap::from([
            (
                "a_first".to_string(),
                server(
                    &first_pid_file,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "result": {"tools": [tool("safe_first_tool")]},
                    }),
                ),
            ),
            (
                "b_second".to_string(),
                server(
                    &second_pid_file,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "result": {
                            "tools": [tool("prefix_sk-secretvalue123")],
                        },
                    }),
                ),
            ),
        ]);

        let error = match McpManager::new(&servers, &[]).await {
            Ok(_) => panic!("secret-bearing second discovery must fail atomically"),
            Err(error) => error.to_string(),
        };
        assert_eq!(error, format!("RPC error: {MCP_METADATA_SECRET_ERROR}"));
        assert!(!error.contains("safe_first_tool"), "{error}");
        assert!(!error.contains("sk-secretvalue123"), "{error}");

        let first_pids = wait_for_mcp_fixture_pids(&first_pid_file).await;
        let second_pids = wait_for_mcp_fixture_pids(&second_pid_file).await;
        assert_mcp_fixture_pids_stop(&first_pids).await;
        assert_mcp_fixture_pids_stop(&second_pids).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_uses_validated_discovery_cache_and_shared_matcher() {
        let directory = tempfile::tempdir().expect("cached metadata fixture");
        let pid_file = directory.path().join("cached-metadata-pids");
        let changed_file = directory.path().join("changed-metadata-sent");
        let unexpected_request_file = directory.path().join("unexpected-tools-list-request");
        let safe = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"tools": [tool("safe_cached_tool")]},
        });
        let changed = json!({
            "jsonrpc": "2.0",
            "id": 900,
            "result": {"tools": [tool("prefix_sk-secretvalue123")]},
        });
        let servers = HashMap::from([(
            "fixture".to_string(),
            McpServerEntry {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    concat!(
                        "sleep 60 >&- 2>&- & child=$!; ",
                        "printf '%s %s' \"$$\" \"$child\" > \"$MCP_PID_FILE\"; ",
                        "IFS= read -r initialize; ",
                        "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\\n'; ",
                        "IFS= read -r initialized; ",
                        "IFS= read -r list_tools; ",
                        "printf '%s\\n' \"$MCP_SAFE_RESPONSE\"; ",
                        "printf '%s\\n' \"$MCP_CHANGED_RESPONSE\"; ",
                        ": > \"$MCP_CHANGED_FILE\"; ",
                        "if IFS= read -r unexpected; then ",
                        "printf '%s' \"$unexpected\" > \"$MCP_UNEXPECTED_REQUEST_FILE\"; ",
                        "fi; wait"
                    )
                    .to_string(),
                ],
                env: HashMap::from([
                    ("MCP_SAFE_RESPONSE".to_string(), safe.to_string()),
                    ("MCP_CHANGED_RESPONSE".to_string(), changed.to_string()),
                    (
                        "MCP_CHANGED_FILE".to_string(),
                        changed_file.to_string_lossy().into_owned(),
                    ),
                    (
                        "MCP_UNEXPECTED_REQUEST_FILE".to_string(),
                        unexpected_request_file.to_string_lossy().into_owned(),
                    ),
                    (
                        "MCP_PID_FILE".to_string(),
                        pid_file.to_string_lossy().into_owned(),
                    ),
                ]),
                ..McpServerEntry::default()
            },
        )]);

        let manager = McpManager::new(&servers, &[])
            .await
            .expect("safe discovery initializes manager");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !changed_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("changed discovery frame sent");

        let client = manager.servers.get("fixture").expect("fixture client");
        assert!(Arc::ptr_eq(&manager.secret_matcher, &client.secret_matcher));
        for _ in 0..2 {
            let tools = manager.list_tools().await.expect("cached discovery");
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["name"], "fixture::safe_cached_tool");
            let serialized = tools[0].to_string();
            assert!(!serialized.contains("sk-secretvalue123"), "{serialized}");
        }
        assert!(manager
            .call_tool("fixture::prefix_sk-secretvalue123", json!({}))
            .await
            .is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !unexpected_request_file.exists(),
            "runtime reissued tools/list instead of using its cache"
        );

        let pids = wait_for_mcp_fixture_pids(&pid_file).await;
        drop(manager);
        assert_mcp_fixture_pids_stop(&pids).await;
    }

    #[cfg(unix)]
    async fn wait_for_mcp_fixture_pids(path: &std::path::Path) -> Vec<i32> {
        let ids = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(ids) = std::fs::read_to_string(path) {
                    break ids;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MCP fixture process ids");
        ids.split_whitespace()
            .map(|value| value.parse::<i32>().expect("numeric MCP fixture pid"))
            .collect()
    }

    #[cfg(unix)]
    async fn assert_mcp_fixture_pids_stop(pids: &[i32]) {
        let terminated = tokio::time::timeout(Duration::from_secs(2), async {
            while pids.iter().any(|pid| mcp_test_process_is_active(*pid)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        if !terminated {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{}", pids[0])])
                .status();
        }
        assert!(terminated, "MCP fixture process tree survived shutdown");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn mcp_child_drops_ambient_secrets_and_keeps_configured_environment() {
        const AMBIENT: &str = "NIB_MCP_AMBIENT_TOKEN";
        let previous = std::env::var_os(AMBIENT);
        std::env::set_var(AMBIENT, "must-not-reach-child");
        let entry = McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf '%s\\n%s' \"${NIB_MCP_AMBIENT_TOKEN-}\" \"${NIB_MCP_CONFIGURED-}\""
                    .to_string(),
            ],
            env: HashMap::from([("NIB_MCP_CONFIGURED".to_string(), "configured".to_string())]),
            ..McpServerEntry::default()
        };
        let output = mcp_child_command(&entry).output().await;
        match previous {
            Some(value) => std::env::set_var(AMBIENT, value),
            None => std::env::remove_var(AMBIENT),
        }

        let output = output.expect("MCP child fixture");
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "\nconfigured");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_mcp_child_terminates_descendant_processes() {
        use tokio::io::AsyncBufReadExt;

        let entry = McpServerEntry {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "sleep 60 & child=$!; printf '%s %s\\n' \"$$\" \"$child\"; wait".to_string(),
            ],
            ..McpServerEntry::default()
        };
        let mut command = mcp_child_command(&entry);
        let mut child = crate::sandbox::spawn_managed_child(&mut command).expect("MCP child");
        let stdout = child.stdout.take().expect("MCP child stdout");
        let mut reader = BufReader::new(stdout);
        let mut ids = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut ids))
            .await
            .expect("MCP child ids timeout")
            .expect("MCP child ids");
        let mut ids = ids.split_whitespace().map(|value| {
            value
                .parse::<i32>()
                .expect("fixture process id must be numeric")
        });
        let process_group = ids.next().expect("process group leader id");
        let descendant = ids.next().expect("descendant process id");

        drop(reader);
        drop(child);
        let terminated = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !mcp_test_process_is_active(descendant) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        if !terminated {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{process_group}")])
                .status();
        }
        assert!(terminated, "descendant process survived MCP child drop");
    }

    #[cfg(unix)]
    fn mcp_test_process_is_active(pid: i32) -> bool {
        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            return stat
                .rsplit_once(") ")
                .and_then(|(_, fields)| fields.chars().next())
                .is_some_and(|state| state != 'Z');
        }

        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
