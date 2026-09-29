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

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
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
#[path = "mcp_tests.rs"]
mod tests;
