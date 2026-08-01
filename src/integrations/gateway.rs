//! Normalized ingress/egress models for console and messaging gateways.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::Instant;

const MAX_GATEWAY_SESSION_ID_BYTES: usize = 128;
const SHA256_HEX_BYTES: usize = 64;
const MAX_CONCURRENT_GATEWAY_DISPATCHES: usize = 64;
const GATEWAY_LOCK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const GATEWAY_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

static GATEWAY_ADMISSION: Semaphore = Semaphore::const_new(MAX_CONCURRENT_GATEWAY_DISPATCHES);

struct GatewayDispatchGuard {
    _anchor_file: File,
    _admission: SemaphorePermit<'static>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayPlatform {
    Console,
    Telegram,
    Slack,
    Discord,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayMessage {
    pub platform: GatewayPlatform,
    pub conversation_id: String,
    pub user_id: String,
    pub message_id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayRequest {
    pub message: GatewayMessage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayReply {
    pub platform: GatewayPlatform,
    pub conversation_id: String,
    pub reply_to: Option<String>,
    pub text: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GatewayError {
    #[error("missing gateway field: {0}")]
    Missing(&'static str),
    #[error("gateway field cannot be empty: {0}")]
    Empty(&'static str),
    #[error("gateway message has no user-authored text")]
    NotUserText,
}

impl GatewayMessage {
    pub fn validate(&self) -> Result<(), GatewayError> {
        for (field, value) in [
            ("conversation_id", self.conversation_id.as_str()),
            ("user_id", self.user_id.as_str()),
            ("text", self.text.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(GatewayError::Empty(field));
            }
        }
        if self
            .message_id
            .as_deref()
            .is_some_and(|message_id| message_id.trim().is_empty())
        {
            return Err(GatewayError::Empty("message_id"));
        }
        Ok(())
    }
}

fn string_or_number(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

pub fn parse_gateway_message(
    platform: GatewayPlatform,
    payload: &Value,
) -> Result<GatewayMessage, GatewayError> {
    let message = match platform {
        GatewayPlatform::Console => {
            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .ok_or(GatewayError::Missing("text"))?;
            GatewayMessage {
                platform,
                conversation_id: payload
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .unwrap_or("console")
                    .to_string(),
                user_id: payload
                    .get("user_id")
                    .and_then(Value::as_str)
                    .unwrap_or("local-user")
                    .to_string(),
                message_id: payload
                    .get("message_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                text: text.to_string(),
                metadata: payload.clone(),
            }
        }
        GatewayPlatform::Telegram => {
            let message = payload
                .get("message")
                .ok_or(GatewayError::Missing("message"))?;
            let text = message
                .get("text")
                .and_then(Value::as_str)
                .ok_or(GatewayError::NotUserText)?;
            GatewayMessage {
                platform,
                conversation_id: string_or_number(message.pointer("/chat/id"))
                    .ok_or(GatewayError::Missing("message.chat.id"))?,
                user_id: string_or_number(message.pointer("/from/id"))
                    .ok_or(GatewayError::Missing("message.from.id"))?,
                message_id: string_or_number(message.get("message_id")),
                text: text.to_string(),
                metadata: payload.clone(),
            }
        }
        GatewayPlatform::Slack => {
            let event = payload.get("event").ok_or(GatewayError::Missing("event"))?;
            if event.get("bot_id").is_some() || event.get("subtype").is_some() {
                return Err(GatewayError::NotUserText);
            }
            GatewayMessage {
                platform,
                conversation_id: string_or_number(event.get("channel"))
                    .ok_or(GatewayError::Missing("event.channel"))?,
                user_id: string_or_number(event.get("user"))
                    .ok_or(GatewayError::Missing("event.user"))?,
                message_id: string_or_number(event.get("ts")),
                text: event
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(GatewayError::NotUserText)?
                    .to_string(),
                metadata: payload.clone(),
            }
        }
        GatewayPlatform::Discord => {
            if payload.pointer("/author/bot").and_then(Value::as_bool) == Some(true) {
                return Err(GatewayError::NotUserText);
            }
            GatewayMessage {
                platform,
                conversation_id: string_or_number(payload.get("channel_id"))
                    .ok_or(GatewayError::Missing("channel_id"))?,
                user_id: string_or_number(payload.pointer("/author/id"))
                    .ok_or(GatewayError::Missing("author.id"))?,
                message_id: string_or_number(payload.get("id")),
                text: payload
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or(GatewayError::NotUserText)?
                    .to_string(),
                metadata: payload.clone(),
            }
        }
    };
    message.validate()?;
    Ok(message)
}

pub fn render_gateway_reply(reply: &GatewayReply) -> Value {
    match reply.platform {
        GatewayPlatform::Console => json!({"text": reply.text}),
        GatewayPlatform::Telegram => json!({
            "chat_id": reply.conversation_id,
            "text": reply.text,
            "reply_to_message_id": reply.reply_to,
        }),
        GatewayPlatform::Slack => json!({
            "channel": reply.conversation_id,
            "text": reply.text,
            "thread_ts": reply.reply_to,
        }),
        GatewayPlatform::Discord => json!({
            "channel_id": reply.conversation_id,
            "content": reply.text,
            "message_reference": reply.reply_to.as_ref().map(|id| json!({"message_id": id})),
        }),
    }
}

async fn acquire_gateway_dispatch_guard(
    sessions_dir: &Path,
    session_id: &str,
    timeout: Duration,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<GatewayDispatchGuard, String> {
    let deadline = Instant::now() + timeout;
    let admission = loop {
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            return Err("gateway dispatch cancelled while waiting for admission".to_string());
        }
        match GATEWAY_ADMISSION.try_acquire() {
            Ok(permit) => break permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "gateway dispatch admission timed out after {} seconds",
                        timeout.as_secs()
                    ));
                }
                tokio::time::sleep(GATEWAY_LOCK_POLL_INTERVAL).await;
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err("gateway dispatch admission is closed".to_string());
            }
        }
    };

    crate::fs_security::verify_directory_without_symlinks(sessions_dir)
        .map_err(|error| format!("gateway session directory is unsafe: {error}"))?;
    let lock_path = gateway_lock_path(sessions_dir, session_id);
    let anchor_path = gateway_lock_anchor_path(sessions_dir, session_id)?;
    let anchor_dir = anchor_path.parent().ok_or_else(|| {
        format!(
            "gateway lock anchor has no parent: {}",
            anchor_path.display()
        )
    })?;
    crate::fs_security::verify_directory_without_symlinks(anchor_dir)
        .map_err(|error| format!("gateway lock anchor directory is unsafe: {error}"))?;
    let anchor_file = open_gateway_lock_anchor(&lock_path, &anchor_path)?;
    let locked_identity = gateway_lock_identity(&anchor_file, &anchor_path)?;

    acquire_gateway_file_lock_with(
        || anchor_file.try_lock(),
        &lock_path,
        deadline,
        timeout,
        cancellation,
    )
    .await?;

    crate::fs_security::verify_directory_without_symlinks(sessions_dir)
        .map_err(|error| format!("gateway session directory changed while locking: {error}"))?;
    crate::fs_security::verify_directory_without_symlinks(anchor_dir)
        .map_err(|error| format!("gateway lock anchor directory changed while locking: {error}"))?;
    for path in [&anchor_path, &lock_path] {
        let path_identity = open_gateway_lock_identity(path)?;
        if locked_identity != path_identity {
            return Err(format!(
                "gateway session lock identity changed while it was acquired: {}",
                path.display()
            ));
        }
    }

    Ok(GatewayDispatchGuard {
        _anchor_file: anchor_file,
        _admission: admission,
    })
}

async fn acquire_gateway_file_lock_with(
    mut try_lock: impl FnMut() -> Result<(), std::fs::TryLockError>,
    lock_path: &Path,
    deadline: Instant,
    timeout: Duration,
    cancellation: Option<&crate::agent::CancellationSignal>,
) -> Result<(), String> {
    loop {
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            return Err(
                "gateway dispatch cancelled while waiting for its session lock".to_string(),
            );
        }
        match try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error))
                if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!("failed to acquire gateway session lock: {error}"));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(gateway_lock_timeout_error(lock_path, timeout));
        }
        tokio::time::sleep(GATEWAY_LOCK_POLL_INTERVAL.min(deadline - now)).await;
        if Instant::now() >= deadline {
            return Err(gateway_lock_timeout_error(lock_path, timeout));
        }
    }
}

fn gateway_lock_timeout_error(lock_path: &Path, timeout: Duration) -> String {
    format!(
        "gateway session lock timed out after {} seconds: {}",
        timeout.as_secs(),
        lock_path.display()
    )
}

fn gateway_lock_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!(".{session_id}.gateway.lock"))
}

fn gateway_lock_anchor_path(sessions_dir: &Path, session_id: &str) -> Result<PathBuf, String> {
    let profile_dir = sessions_dir.parent().ok_or_else(|| {
        format!(
            "gateway session directory has no profile parent: {}",
            sessions_dir.display()
        )
    })?;
    Ok(profile_dir.join(format!(".{session_id}.gateway.lock.anchor")))
}

#[cfg(any(unix, windows))]
fn open_gateway_lock_anchor(lock_path: &Path, anchor_path: &Path) -> Result<File, String> {
    let lock_exists = gateway_lock_path_exists(lock_path)?;
    let anchor_exists = gateway_lock_path_exists(anchor_path)?;
    match (lock_exists, anchor_exists) {
        (false, false) => {
            drop(open_gateway_lock_file(lock_path)?);
            create_gateway_lock_link(lock_path, anchor_path)?;
        }
        (true, false) => create_gateway_lock_link(lock_path, anchor_path)?,
        (false, true) => create_gateway_lock_link(anchor_path, lock_path)?,
        (true, true) => {}
    }

    let anchor_file = open_gateway_lock_file(anchor_path)?;
    let anchor_identity = gateway_lock_identity(&anchor_file, anchor_path)?;
    let lock_identity = open_gateway_lock_identity(lock_path)?;
    if anchor_identity != lock_identity {
        return Err(format!(
            "gateway session lock and persistent anchor have different identities: {}",
            lock_path.display()
        ));
    }
    Ok(anchor_file)
}

#[cfg(not(any(unix, windows)))]
fn open_gateway_lock_anchor(lock_path: &Path, _anchor_path: &Path) -> Result<File, String> {
    Err(format!(
        "gateway session lock anchors are unsupported on this platform: {}",
        lock_path.display()
    ))
}

#[cfg(any(unix, windows))]
fn gateway_lock_path_exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "gateway session lock must not be a symlink: {}",
            path.display()
        )),
        Ok(metadata) if !metadata.is_file() => Err(format!(
            "gateway session lock must be a regular local file: {}",
            path.display()
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "failed to inspect gateway session lock {}: {error}",
            path.display()
        )),
    }
}

#[cfg(any(unix, windows))]
fn create_gateway_lock_link(source: &Path, destination: &Path) -> Result<(), String> {
    match fs::hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!(
            "failed to create persistent gateway lock anchor {} from {}: {error}",
            destination.display(),
            source.display()
        )),
    }
}

#[cfg(any(unix, windows))]
fn open_gateway_lock_file(path: &Path) -> Result<File, String> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "gateway session lock must not be a symlink: {}",
            path.display()
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("failed to open gateway session lock: {error}"))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect gateway session lock: {error}"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect open gateway session lock: {error}"))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !opened_metadata.is_file()
    {
        return Err(format!(
            "gateway session lock must be a regular local file: {}",
            path.display()
        ));
    }
    let opened_identity = gateway_lock_identity(&file, path)?;
    let path_identity = open_gateway_lock_identity(path)?;
    if opened_identity != path_identity {
        return Err(format!(
            "gateway session lock changed while it was opened: {}",
            path.display()
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_gateway_lock_file(path: &Path) -> Result<File, String> {
    Err(format!(
        "gateway session locks are unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
fn gateway_lock_identity(
    file: &File,
    path: &Path,
) -> Result<crate::fs_security::FileIdentity, String> {
    crate::fs_security::FileIdentity::from_file(
        file.try_clone()
            .map_err(|error| format!("failed to clone gateway session lock: {error}"))?,
    )
    .map_err(|error| {
        format!(
            "failed to identify gateway session lock {}: {error}",
            path.display()
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn gateway_lock_identity(_file: &File, path: &Path) -> Result<(), String> {
    Err(format!(
        "gateway session lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
fn open_gateway_lock_identity(path: &Path) -> Result<crate::fs_security::FileIdentity, String> {
    let file = open_gateway_lock_probe(path)?;
    crate::fs_security::FileIdentity::from_file(file).map_err(|error| {
        format!(
            "failed to identify gateway session lock path {}: {error}",
            path.display()
        )
    })
}

#[cfg(not(any(unix, windows)))]
fn open_gateway_lock_identity(path: &Path) -> Result<(), String> {
    Err(format!(
        "gateway session lock identity is unsupported on this platform: {}",
        path.display()
    ))
}

#[cfg(any(unix, windows))]
fn open_gateway_lock_probe(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(path)
        .map_err(|error| format!("failed to re-open gateway session lock: {error}"))
}

/// Execute one normalized gateway message through the regular persisted agent
/// loop. Adapters remain responsible for transport authentication, listeners,
/// and reply delivery.
pub async fn dispatch_gateway_request(
    project_root: &Path,
    request: GatewayRequest,
    config: crate::agent::AgentLoopConfig,
) -> Result<GatewayReply, String> {
    request
        .message
        .validate()
        .map_err(|error| error.to_string())?;
    let session_id = gateway_session_id(&request.message);
    let store = crate::session::SessionStore::for_project(project_root)?;
    let _dispatch_guard = acquire_gateway_dispatch_guard(
        store.sessions_dir(),
        &session_id,
        GATEWAY_LOCK_TIMEOUT,
        config.cancellation.as_ref(),
    )
    .await?;
    let summary = crate::agent::run_agent_loop(
        project_root.to_path_buf(),
        &session_id,
        &request.message.text,
        config,
    )
    .await?;
    if summary.is_failure() {
        return Err(format!(
            "gateway agent run failed for session {}: {}",
            summary.session_id, summary.outcome
        ));
    }
    let text = summary
        .last_message
        .unwrap_or_else(|| format!("nib run {}: {}", summary.outcome, summary.session_id));
    Ok(GatewayReply {
        platform: request.message.platform,
        conversation_id: request.message.conversation_id,
        reply_to: request.message.message_id,
        text,
    })
}

pub fn gateway_session_id(message: &GatewayMessage) -> String {
    let platform = match message.platform {
        GatewayPlatform::Console => "console",
        GatewayPlatform::Telegram => "telegram",
        GatewayPlatform::Slack => "slack",
        GatewayPlatform::Discord => "discord",
    };
    let fixed_bytes = format!("gateway-{platform}--").len() + SHA256_HEX_BYTES;
    let readable_bytes = MAX_GATEWAY_SESSION_ID_BYTES.saturating_sub(fixed_bytes);
    let conversation: String = message
        .conversation_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(readable_bytes)
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(platform.as_bytes());
    hasher.update([0]);
    hasher.update(message.conversation_id.as_bytes());
    let identity = format!("{:x}", hasher.finalize());
    format!("gateway-{platform}-{conversation}-{identity}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentLoopConfig, CancellationSignal};
    use crate::config::{save_nib_config_full, NibConfig};
    use crate::llm::StreamEvent;
    use crate::session::SessionStore;
    use std::cell::Cell;
    use std::sync::Arc;
    use tempfile::tempdir;

    const LOCK_CHILD_SESSIONS_DIR: &str = "NIB_GATEWAY_LOCK_CHILD_SESSIONS_DIR";
    const LOCK_CHILD_SESSION_ID: &str = "NIB_GATEWAY_LOCK_CHILD_SESSION_ID";
    const LOCK_CHILD_EXPECTATION: &str = "NIB_GATEWAY_LOCK_CHILD_EXPECTATION";
    const GATEWAY_TEST_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);

    fn mock_loop_config() -> AgentLoopConfig {
        AgentLoopConfig {
            max_steps: 8,
            provider: Some("mock".to_string()),
            auto_approve: true,
            ..Default::default()
        }
    }

    fn stream_mock_loop_config(
        stream_tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> AgentLoopConfig {
        AgentLoopConfig {
            stream_tx: Some(stream_tx),
            ..mock_loop_config()
        }
    }

    fn gateway_message(conversation_id: &str, message_id: &str, text: &str) -> GatewayMessage {
        GatewayMessage {
            platform: GatewayPlatform::Slack,
            conversation_id: conversation_id.to_string(),
            user_id: "U1".to_string(),
            message_id: Some(message_id.to_string()),
            text: text.to_string(),
            metadata: Value::Null,
        }
    }

    fn configure_mock_runtime(root: &Path) {
        let mut config = NibConfig::default();
        config.daemons.cron_enabled = false;
        config.daemons.curator_enabled = false;
        save_nib_config_full(root, &mut config).expect("mock runtime config");
    }

    #[test]
    fn normalizes_supported_gateway_payloads() {
        let telegram = parse_gateway_message(
            GatewayPlatform::Telegram,
            &json!({"message": {"message_id": 7, "chat": {"id": 42}, "from": {"id": 9}, "text": "ship it"}}),
        )
        .expect("telegram message");
        assert_eq!(telegram.conversation_id, "42");

        let slack = parse_gateway_message(
            GatewayPlatform::Slack,
            &json!({"event": {"channel": "C1", "user": "U1", "ts": "1.2", "text": "check CI"}}),
        )
        .expect("slack message");
        assert_eq!(slack.text, "check CI");

        let discord = parse_gateway_message(
            GatewayPlatform::Discord,
            &json!({"id": "M1", "channel_id": "D1", "author": {"id": "U2", "bot": false}, "content": "review"}),
        )
        .expect("discord message");
        assert_eq!(discord.user_id, "U2");
    }

    #[test]
    fn rejects_bot_messages_and_renders_platform_replies() {
        let bot = parse_gateway_message(
            GatewayPlatform::Slack,
            &json!({"event": {"channel": "C1", "user": "U1", "bot_id": "B1", "text": "loop"}}),
        );
        assert_eq!(bot, Err(GatewayError::NotUserText));

        let rendered = render_gateway_reply(&GatewayReply {
            platform: GatewayPlatform::Telegram,
            conversation_id: "42".to_string(),
            reply_to: Some("7".to_string()),
            text: "done".to_string(),
        });
        assert_eq!(rendered["chat_id"], "42");
        assert_eq!(rendered["reply_to_message_id"], "7");
    }

    #[test]
    fn rejects_invalid_normalized_payloads_and_caller_supplied_tools() {
        let blank_text = parse_gateway_message(
            GatewayPlatform::Console,
            &json!({"conversation_id": "console", "user_id": "local", "text": "  "}),
        );
        assert_eq!(blank_text, Err(GatewayError::Empty("text")));

        let request = json!({
            "message": {
                "platform": "console",
                "conversation_id": "console",
                "user_id": "local",
                "message_id": null,
                "text": "status",
                "metadata": null
            },
            "tools": [{"type": "function"}]
        });
        let error = serde_json::from_value::<GatewayRequest>(request)
            .expect_err("gateway callers cannot inject tool schemas");
        assert!(error.to_string().contains("unknown field `tools`"));
    }

    #[tokio::test]
    async fn invalid_dispatch_inputs_fail_before_session_persistence() {
        let root = tempdir().expect("tempdir");
        let invalid_messages = [
            (
                "conversation_id",
                GatewayMessage {
                    platform: GatewayPlatform::Slack,
                    conversation_id: " ".to_string(),
                    user_id: "U1".to_string(),
                    message_id: Some("1.0".to_string()),
                    text: "status".to_string(),
                    metadata: Value::Null,
                },
            ),
            (
                "user_id",
                GatewayMessage {
                    platform: GatewayPlatform::Slack,
                    conversation_id: "C1".to_string(),
                    user_id: "".to_string(),
                    message_id: Some("1.0".to_string()),
                    text: "status".to_string(),
                    metadata: Value::Null,
                },
            ),
            (
                "message_id",
                GatewayMessage {
                    platform: GatewayPlatform::Slack,
                    conversation_id: "C1".to_string(),
                    user_id: "U1".to_string(),
                    message_id: Some("\t".to_string()),
                    text: "status".to_string(),
                    metadata: Value::Null,
                },
            ),
            (
                "text",
                GatewayMessage {
                    platform: GatewayPlatform::Slack,
                    conversation_id: "C1".to_string(),
                    user_id: "U1".to_string(),
                    message_id: None,
                    text: "\n".to_string(),
                    metadata: Value::Null,
                },
            ),
        ];

        for (field, message) in invalid_messages {
            let error = dispatch_gateway_request(
                root.path(),
                GatewayRequest { message },
                mock_loop_config(),
            )
            .await
            .expect_err("invalid normalized input");
            assert_eq!(error, format!("gateway field cannot be empty: {field}"));
        }
        assert!(!root.path().join(".nib").exists());
    }

    #[tokio::test]
    async fn dispatch_reuses_a_persisted_mock_agent_session() {
        let root = tempdir().expect("tempdir");
        configure_mock_runtime(root.path());

        let first_message = parse_gateway_message(
            GatewayPlatform::Slack,
            &json!({"event": {"channel": "C-runtime", "user": "U1", "ts": "1.0", "text": "explore gateway state"}}),
        )
        .expect("first normalized message");
        let session_id = gateway_session_id(&first_message);
        let first_reply = dispatch_gateway_request(
            root.path(),
            GatewayRequest {
                message: first_message,
            },
            mock_loop_config(),
        )
        .await
        .expect("first gateway dispatch");
        assert_eq!(first_reply.platform, GatewayPlatform::Slack);
        assert_eq!(first_reply.conversation_id, "C-runtime");
        assert_eq!(first_reply.reply_to.as_deref(), Some("1.0"));
        assert_eq!(
            first_reply.text,
            "Final answer: task complete. (mock LLM response)"
        );

        let second_message = parse_gateway_message(
            GatewayPlatform::Slack,
            &json!({"event": {"channel": "C-runtime", "user": "U1", "ts": "2.0", "text": "explore gateway state again"}}),
        )
        .expect("second normalized message");
        let second_reply = dispatch_gateway_request(
            root.path(),
            GatewayRequest {
                message: second_message,
            },
            mock_loop_config(),
        )
        .await
        .expect("second gateway dispatch");
        assert_eq!(second_reply.reply_to.as_deref(), Some("2.0"));

        let store = SessionStore::for_project(root.path()).expect("profile session store");
        assert!(store
            .sessions_dir()
            .join(format!("{session_id}.json"))
            .is_file());
        assert_eq!(store.list(), vec![session_id.clone()]);
        let session = store.load(&session_id).expect("persisted gateway session");
        session
            .validate_message_sequence()
            .expect("valid persisted role sequence");
        for prompt in ["explore gateway state", "explore gateway state again"] {
            assert!(session
                .messages
                .iter()
                .any(|message| message.role == "user" && message.content == prompt));
        }
        assert!(session.tool_calls.len() >= 2);
        assert!(session.tool_calls.iter().all(|record| {
            record.tool_name.as_deref() == Some("list_directory")
                && record.error.is_none()
                && record.result.is_some()
        }));
        assert!(
            session
                .events
                .iter()
                .filter(|event| event.kind == "reconciliation")
                .count()
                >= 2
        );
    }

    #[tokio::test]
    async fn session_locks_are_process_visible_bounded_and_independent() {
        let root = tempdir().expect("tempdir");
        configure_mock_runtime(root.path());
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let first_id = gateway_session_id(&gateway_message("C-lock-one", "1", "status"));
        let second_id = gateway_session_id(&gateway_message("C-lock-two", "2", "status"));

        let first = acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &first_id,
            Duration::from_secs(1),
            None,
        )
        .await
        .expect("first independent OS lock handle");
        let same_error = acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &first_id,
            Duration::from_millis(75),
            None,
        )
        .await
        .err()
        .expect("a second handle cannot acquire the same session lock");
        assert!(same_error.contains("timed out"));

        let distinct = acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &second_id,
            Duration::from_millis(75),
            None,
        )
        .await
        .expect("a distinct session does not share one global OS lock");
        drop(distinct);
        drop(first);

        acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &first_id,
            Duration::from_millis(75),
            None,
        )
        .await
        .expect("dropping the guard releases the OS lock");
    }

    #[test]
    fn gateway_lock_replacement_child_process() {
        let Some(sessions_dir) = std::env::var_os(LOCK_CHILD_SESSIONS_DIR) else {
            return;
        };
        let session_id = std::env::var(LOCK_CHILD_SESSION_ID).expect("child session ID");
        let expectation = std::env::var(LOCK_CHILD_EXPECTATION).expect("child expectation");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("child runtime");
        let error = runtime
            .block_on(acquire_gateway_dispatch_guard(
                Path::new(&sessions_dir),
                &session_id,
                Duration::from_millis(100),
                None,
            ))
            .err()
            .expect("replacement must not create a second lock domain");
        match expectation.as_str() {
            "timeout" => assert!(error.contains("timed out")),
            "identity" => assert!(error.contains("persistent anchor have different identities")),
            value => panic!("unsupported child expectation: {value}"),
        }
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn persistent_anchor_rejects_replaced_lock_path_in_a_child_process() {
        let root = tempdir().expect("tempdir");
        configure_mock_runtime(root.path());
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session_id = gateway_session_id(&gateway_message("C-replaced", "1", "status"));
        let held = acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &session_id,
            Duration::from_secs(1),
            None,
        )
        .await
        .expect("held persistent gateway lock");

        let run_child = |expectation: &str| {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "integrations::gateway::tests::gateway_lock_replacement_child_process",
                    "--nocapture",
                ])
                .env(LOCK_CHILD_SESSIONS_DIR, store.sessions_dir())
                .env(LOCK_CHILD_SESSION_ID, &session_id)
                .env(LOCK_CHILD_EXPECTATION, expectation)
                .output()
                .expect("run gateway lock child process");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run_child("timeout");

        let lock_path = gateway_lock_path(store.sessions_dir(), &session_id);
        let displaced_path = lock_path.with_extension("lock.displaced");
        fs::rename(&lock_path, &displaced_path).expect("displace primary lock path");
        fs::write(&lock_path, b"replacement").expect("replace primary lock path");
        run_child("identity");

        fs::remove_file(&lock_path).expect("remove replacement lock path");
        fs::rename(&displaced_path, &lock_path).expect("restore anchored lock path");

        let sessions_path = store.sessions_dir().to_path_buf();
        let displaced_sessions = sessions_path.with_extension("displaced");
        #[cfg(unix)]
        {
            fs::rename(&sessions_path, &displaced_sessions).expect("displace sessions directory");
            fs::create_dir(&sessions_path).expect("replace sessions directory");
            run_child("timeout");
            fs::remove_dir_all(&sessions_path).expect("remove replacement sessions directory");
            fs::rename(&displaced_sessions, &sessions_path).expect("restore sessions directory");
        }
        #[cfg(windows)]
        {
            fs::rename(&sessions_path, &displaced_sessions)
                .expect_err("live Windows gateway lock pins the sessions directory");
            assert!(sessions_path.is_dir());
            assert!(!displaced_sessions.exists());
            run_child("timeout");
        }

        drop(held);
        acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &session_id,
            Duration::from_millis(100),
            None,
        )
        .await
        .expect("restored identity remains usable");
    }

    #[tokio::test]
    async fn interrupted_lock_attempts_sleep_and_obey_the_deadline() {
        let attempts = Cell::new(0usize);
        let timeout = Duration::from_millis(60);
        let started = Instant::now();
        let error = acquire_gateway_file_lock_with(
            || {
                attempts.set(attempts.get() + 1);
                Err(std::fs::TryLockError::Error(std::io::Error::from(
                    std::io::ErrorKind::Interrupted,
                )))
            },
            Path::new("interrupted.gateway.lock"),
            started + timeout,
            timeout,
            None,
        )
        .await
        .expect_err("repeated interruptions must still time out");

        assert!(error.contains("timed out"));
        assert!(started.elapsed() >= GATEWAY_LOCK_POLL_INTERVAL);
        assert!(
            (1..=4).contains(&attempts.get()),
            "interrupted retry loop spun {} times",
            attempts.get()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gateway_session_lock_rejects_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("tempdir");
        let outside = tempdir().expect("outside");
        configure_mock_runtime(root.path());
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session_id = gateway_session_id(&gateway_message("C-symlink", "1", "status"));
        let target = outside.path().join("outside.lock");
        fs::write(&target, "sentinel").expect("outside lock target");
        symlink(
            &target,
            gateway_lock_path(store.sessions_dir(), &session_id),
        )
        .expect("gateway lock symlink");

        let error = acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &session_id,
            Duration::from_millis(100),
            None,
        )
        .await
        .err()
        .expect("symlinked gateway lock fails closed");

        assert!(error.contains("must not be a symlink"));
        assert_eq!(
            fs::read_to_string(target).expect("outside target"),
            "sentinel"
        );
    }

    #[tokio::test]
    async fn cancelled_session_lock_wait_releases_admission_and_waiter_state() {
        let root = tempdir().expect("tempdir");
        configure_mock_runtime(root.path());
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session_id = gateway_session_id(&gateway_message("C-cancel", "1", "status"));
        let held = acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &session_id,
            Duration::from_secs(1),
            None,
        )
        .await
        .expect("held session lock");

        let cancellation = CancellationSignal::new();
        let sessions_dir = store.sessions_dir().to_path_buf();
        let waiting_id = session_id.clone();
        let waiting_cancellation = cancellation.clone();
        let waiting = tokio::spawn(async move {
            acquire_gateway_dispatch_guard(
                &sessions_dir,
                &waiting_id,
                Duration::from_secs(5),
                Some(&waiting_cancellation),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(cancellation.cancel());
        let error = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("cancelled waiter exits promptly")
            .expect("waiter task")
            .err()
            .expect("cancelled lock wait");
        assert!(error.contains("cancelled"));

        drop(held);
        acquire_gateway_dispatch_guard(
            store.sessions_dir(),
            &session_id,
            Duration::from_millis(100),
            None,
        )
        .await
        .expect("cancelled waiter did not retain the lock or admission permit");
    }

    #[tokio::test]
    async fn same_conversation_runs_serialize_while_distinct_conversations_progress() {
        let root = Arc::new(tempdir().expect("tempdir"));
        configure_mock_runtime(root.path());

        let (first_tx, mut first_rx) = tokio::sync::mpsc::channel(1);
        let first_capacity = first_tx.clone();
        let first_root = Arc::clone(&root);
        let first = tokio::spawn(async move {
            dispatch_gateway_request(
                first_root.path(),
                GatewayRequest {
                    message: gateway_message("C-serial", "1", "first gateway run"),
                },
                stream_mock_loop_config(first_tx),
            )
            .await
        });

        tokio::time::timeout(GATEWAY_TEST_PROGRESS_TIMEOUT, async {
            while first_capacity.capacity() != 0 || first.is_finished() {
                assert!(
                    !first.is_finished(),
                    "first dispatch finished before blocking"
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first dispatch fills its stream and remains in the agent loop");

        let (same_tx, mut same_rx) = tokio::sync::mpsc::channel(64);
        let same_root = Arc::clone(&root);
        let same = tokio::spawn(async move {
            dispatch_gateway_request(
                same_root.path(),
                GatewayRequest {
                    message: gateway_message("C-serial", "2", "second gateway run"),
                },
                stream_mock_loop_config(same_tx),
            )
            .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(250), same_rx.recv())
                .await
                .is_err(),
            "the second same-session run must not enter the agent loop"
        );

        let distinct_root = Arc::clone(&root);
        let distinct = tokio::spawn(async move {
            dispatch_gateway_request(
                distinct_root.path(),
                GatewayRequest {
                    message: gateway_message("C-parallel", "3", "parallel gateway run"),
                },
                mock_loop_config(),
            )
            .await
        });
        tokio::time::timeout(GATEWAY_TEST_PROGRESS_TIMEOUT, distinct)
            .await
            .expect("a distinct session progresses while the first run is blocked")
            .expect("distinct dispatch task")
            .expect("distinct dispatch");

        drop(first_capacity);
        let first_drain = tokio::spawn(async move { while first_rx.recv().await.is_some() {} });
        tokio::time::timeout(GATEWAY_TEST_PROGRESS_TIMEOUT, first)
            .await
            .expect("first dispatch completes after its stream is drained")
            .expect("first dispatch task")
            .expect("first dispatch");
        tokio::time::timeout(GATEWAY_TEST_PROGRESS_TIMEOUT, first_drain)
            .await
            .expect("first stream drain completes")
            .expect("first stream drain task");

        tokio::time::timeout(GATEWAY_TEST_PROGRESS_TIMEOUT, async {
            while same_rx.recv().await.is_some() {}
        })
        .await
        .expect("serialized dispatch stream closes");
        tokio::time::timeout(GATEWAY_TEST_PROGRESS_TIMEOUT, same)
            .await
            .expect("serialized dispatch completes after the first")
            .expect("serialized dispatch task")
            .expect("serialized dispatch");

        let store = SessionStore::for_project(root.path()).expect("profile session store");
        let session_id = gateway_session_id(&gateway_message("C-serial", "ignored", "ignored"));
        let session = store.load(&session_id).expect("serialized gateway session");
        session
            .validate_message_sequence()
            .expect("same-session role sequence remains valid");
        let user_prompts = session
            .messages
            .iter()
            .filter(|message| {
                message.role == "user"
                    && matches!(
                        message.content.as_str(),
                        "first gateway run" | "second gateway run"
                    )
            })
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(user_prompts, ["first gateway run", "second gateway run"]);
    }

    #[test]
    fn gateway_session_ids_are_stable_and_path_safe() {
        let message = GatewayMessage {
            platform: GatewayPlatform::Slack,
            conversation_id: "team/channel ../one".to_string(),
            user_id: "U1".to_string(),
            message_id: None,
            text: "status".to_string(),
            metadata: Value::Null,
        };
        let first = gateway_session_id(&message);
        let second = gateway_session_id(&message);
        assert_eq!(first, second);
        assert!(first.starts_with("gateway-slack-team_channel____one-"));
        assert!(first.len() <= MAX_GATEWAY_SESSION_ID_BYTES);
        assert!(first
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')));
        let digest = first.rsplit('-').next().expect("identity digest");
        assert_eq!(digest.len(), SHA256_HEX_BYTES);
        assert!(digest
            .chars()
            .all(|character| character.is_ascii_hexdigit()));
    }

    #[test]
    fn punctuation_variants_do_not_collide_after_readable_sanitization() {
        let message = |conversation_id: &str| GatewayMessage {
            platform: GatewayPlatform::Slack,
            conversation_id: conversation_id.to_string(),
            user_id: "U1".to_string(),
            message_id: None,
            text: "status".to_string(),
            metadata: Value::Null,
        };

        let slash = gateway_session_id(&message("team/channel"));
        let question = gateway_session_id(&message("team?channel"));

        assert!(slash.starts_with("gateway-slack-team_channel-"));
        assert!(question.starts_with("gateway-slack-team_channel-"));
        assert_ne!(slash, question);
    }

    #[test]
    fn shared_long_prefixes_and_platforms_have_distinct_bounded_ids() {
        let shared = "a".repeat(256);
        let message = |platform, suffix: &str| GatewayMessage {
            platform,
            conversation_id: format!("{shared}{suffix}"),
            user_id: "U1".to_string(),
            message_id: None,
            text: "status".to_string(),
            metadata: Value::Null,
        };

        let slack_one = gateway_session_id(&message(GatewayPlatform::Slack, "one"));
        let slack_two = gateway_session_id(&message(GatewayPlatform::Slack, "two"));
        let discord_one = gateway_session_id(&message(GatewayPlatform::Discord, "one"));

        assert_ne!(slack_one, slack_two);
        assert_ne!(slack_one, discord_one);
        for session_id in [slack_one, slack_two, discord_one] {
            assert!(session_id.len() <= MAX_GATEWAY_SESSION_ID_BYTES);
            assert!(session_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            }));
        }
    }
}
