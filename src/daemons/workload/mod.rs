//! Durable profile-scoped background work and detached workers.

use crate::agent::AgentLoopConfig;
use crate::config::ExecutionConfig;
use crate::daemons::task::{
    deliver_background_task_observation, BackgroundTaskSession, DaemonAuditLog, DaemonAuditRecord,
};
use crate::profile::{Profile, ProfileRegistry};
use crate::session::{SessionError, SessionEvent, SessionRunLease, SessionStore};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(all(test, windows))]
use std::process::Command;
#[cfg(not(windows))]
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
#[cfg(not(windows))]
use std::sync::{
    mpsc::{self, Receiver, RecvTimeoutError, Sender},
    OnceLock,
};
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};

mod split_00;
mod split_01;
mod split_02;

pub(crate) use split_00::*;
pub(crate) use split_01::*;
pub(crate) use split_02::*;

pub use split_00::{run_worker, DurableScheduleRequest, DurableTaskStore};
pub use split_00::{
    DurableReconcileReport, DurableReconciledTask, DurableTaskRecord, DurableTerminalRequest,
    SessionOwnedDurableTask,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
