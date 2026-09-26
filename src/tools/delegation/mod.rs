use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::File;
#[cfg(any(test, debug_assertions))]
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Output;
#[cfg(not(test))]
use std::process::Stdio;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::BoundaryConfig;
use crate::tools::executor::ApprovalHandler;
use crate::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};

mod part_00;
mod part_01;
mod part_01b;
mod part_02;
mod part_03;
mod part_04;
mod part_04b;
mod part_05;
mod part_06;
mod part_06b;
mod part_07;

#[allow(unused_imports)]
pub(crate) use part_00::*;
#[allow(unused_imports)]
pub(crate) use part_01::*;
#[allow(unused_imports)]
pub(crate) use part_01b::*;
#[allow(unused_imports)]
pub(crate) use part_02::*;
#[allow(unused_imports)]
pub(crate) use part_03::*;
#[allow(unused_imports)]
pub(crate) use part_04::*;
#[allow(unused_imports)]
pub(crate) use part_04b::*;
#[allow(unused_imports)]
pub(crate) use part_05::*;
#[allow(unused_imports)]
pub(crate) use part_06::*;
#[allow(unused_imports)]
pub(crate) use part_06b::*;
#[allow(unused_imports)]
pub(crate) use part_07::*;

#[cfg(debug_assertions)]
pub use part_00::{install_merge_interruption_test_barrier, MergeInterruptionTestBarrier};
pub use part_00::{SubagentRecord, VerificationEvidence};
pub use part_02::spawn_subagent;
pub use part_03::spawn_subagent_cancellable;
pub use part_04::{run_subagent_supervisor, run_subagent_worker};
pub use part_04b::{merge_subagent_worktree, send_message_to_subagent};
pub use part_05::{get_subagent_record, list_subagents};
pub use part_06::{cancel_subagent, write_subagent_record};
pub use part_07::confirm_no_legacy_subagent_processes;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
