//! Durable execution scopes for independently supervised foreground processes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, LazyLock, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

// Version 2 records bind `direct_child` to the Linux namespace init rather than
// the outer bubblewrap monitor, so version 1 state must fail closed on recovery.

mod split_00;
mod split_01;
mod split_02;

pub(crate) use split_00::*;
pub(crate) use split_01::*;
pub(crate) use split_02::*;

pub use split_00::{
    supervise_foreground, ProcessScopeRecord, ProcessScopeStore, SupervisedCommand,
    SupervisedOutput,
};
pub use split_00::{
    CleanupLeaseState, CleanupProof, LaunchAbortProof, ProcessIdentity, ProcessScopeBackend,
    ProcessScopeStatus,
};
pub use split_01::CleanupLease;
pub use split_01::{
    supervise_foreground_with_claimed_cleanup,
    supervise_foreground_with_claimed_cleanup_and_commit, supervise_foreground_with_ready,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
