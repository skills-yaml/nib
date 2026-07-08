use crate::config::McpServerEntry;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Debug, Error)]
pub enum McpError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Server not found: {0}")]
    ServerNotFound(String),
    #[error("RPC error: {0}")]
    Rpc(String),
}

type PendingMap = HashMap<usize, oneshot::Sender<Result<Value, String>>>;

pub struct McpManager {
    servers: HashMap<String, Arc<McpServerClient>>,
}

pub struct McpServerClient {
    _name: String,
    request_tx: mpsc::Sender<(Value, oneshot::Sender<Result<Value, String>>)>,
}

impl McpManager {
    pub async fn new(config: &HashMap<String, McpServerEntry>) -> Result<Self, McpError> {
        let mut servers = HashMap::new();
        for (name, entry) in config {
            let client = McpServerClient::start(name.clone(), entry).await?;
            servers.insert(name.clone(), Arc::new(client));
        }
        Ok(Self { servers })
    }

    pub async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let mut all_tools = Vec::new();
        for (name, client) in &self.servers {
            let res = client
                .request("tools/list", json!({}))
                .await
                .map_err(McpError::Rpc)?;

            if let Some(tools) = res.get("tools").and_then(|t| t.as_array()) {
                for tool in tools {
                    let mut t = tool.clone();
                    if let Some(n) = t.get("name").and_then(|v| v.as_str()) {
                        let prefixed = format!("{}::{}", name, n);
                        t["name"] = json!(prefixed);
                        all_tools.push(t);
                    }
                }
            }
        }
        Ok(all_tools)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        let parts: Vec<&str> = name.splitn(2, "::").collect();
        if parts.len() != 2 {
            return Err(McpError::ServerNotFound(
                "Invalid tool name format".to_string(),
            ));
        }
        let server_name = parts[0];
        let original_name = parts[1];

        let client = self
            .servers
            .get(server_name)
            .ok_or_else(|| McpError::ServerNotFound(server_name.to_string()))?;

        let res = client
            .request(
                "tools/call",
                json!({
                    "name": original_name,
                    "arguments": arguments
                }),
            )
            .await
            .map_err(McpError::Rpc)?;

        Ok(res)
    }
}

impl McpServerClient {
    pub async fn start(name: String, entry: &McpServerEntry) -> Result<Self, McpError> {
        let mut cmd = Command::new(&entry.command);
        cmd.args(&entry.args)
            .envs(&entry.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        let mut child = cmd.spawn()?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;

        let (request_tx, mut request_rx) =
            mpsc::channel::<(Value, oneshot::Sender<Result<Value, String>>)>(32);

        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();

        let next_id = Arc::new(AtomicUsize::new(1));
        let next_id_clone = next_id.clone();

        tokio::spawn(async move {
            while let Some((req, tx)) = request_rx.recv().await {
                let id = next_id_clone.fetch_add(1, Ordering::SeqCst);
                let mut full_req = req.clone();
                full_req["jsonrpc"] = json!("2.0");
                full_req["id"] = json!(id);

                pending_clone.lock().await.insert(id, tx);

                let mut line = serde_json::to_string(&full_req).unwrap();
                line.push('\n');

                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let pending_clone2 = pending.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();

            while let Ok(n) = reader.read_line(&mut line).await {
                if n == 0 {
                    break;
                }

                if let Ok(val) = serde_json::from_str::<Value>(&line) {
                    if let Some(id) = val.get("id").and_then(|i| i.as_u64()) {
                        let id_usize = id as usize;
                        if let Some(tx) = pending_clone2.lock().await.remove(&id_usize) {
                            if let Some(err) = val.get("error") {
                                let _ = tx.send(Err(err.to_string()));
                            } else if let Some(res) = val.get("result") {
                                let _ = tx.send(Ok(res.clone()));
                            }
                        }
                    }
                }
                line.clear();
            }
        });

        let client = Self {
            _name: name,
            request_tx,
        };

        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "nib",
                        "version": "0.1.0"
                    }
                }),
            )
            .await
            .map_err(|e| McpError::Rpc(format!("Init failed: {}", e)))?;

        let _ = client.notify("notifications/initialized", json!({})).await;

        Ok(client)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        let req = json!({
            "method": method,
            "params": params
        });

        self.request_tx
            .send((req, tx))
            .await
            .map_err(|_| "Client channel closed")?;

        rx.await.map_err(|_| "Response dropped")?
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let (tx, _rx) = oneshot::channel();
        let req = json!({
            "method": method,
            "params": params
        });
        self.request_tx
            .send((req, tx))
            .await
            .map_err(|_| "Client channel closed")?;
        Ok(())
    }
}
