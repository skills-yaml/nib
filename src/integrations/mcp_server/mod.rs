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

mod split_00;
mod split_01;

pub(crate) use split_00::*;
pub(crate) use split_01::*;

pub use split_00::{handle_request, run_mcp_server, run_stdio_relay};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
