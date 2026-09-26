//! Git worktrees dedicated to linked subagent jobs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};

mod cleanup;
mod git;
mod git_run;
mod lifecycle;
mod ownership;

pub(crate) use cleanup::*;
pub(crate) use git::*;
pub(crate) use git_run::*;
pub(crate) use lifecycle::*;
pub(crate) use ownership::*;

pub use lifecycle::Worktree;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
