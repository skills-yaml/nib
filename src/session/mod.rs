//! File-based session persistence under the active profile's sessions directory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

pub mod memory;

mod split_00;
mod split_01;
mod split_02;
mod split_03;

pub(crate) use split_00::*;
pub(crate) use split_01::*;
pub(crate) use split_02::*;
pub(crate) use split_03::*;

pub use split_00::{normalize_plan_goal, normalize_verification_arguments, Plan};
pub use split_00::{
    ClarificationRecord, PathAttachment, PlanStep, ToolCallRecord, VerificationAuthority,
    VerificationStatus,
};
pub use split_00::{
    ClarificationStatus, HumanIntentKind, HumanIntentRecord, MessageOrigin, MessageProvenance,
    SessionMessage,
};
pub use split_00::{
    VerificationAttempt, VerificationExpectedOutcome, VerificationInvocationSpec,
    VerificationObligation,
};
pub use split_01::{
    QueuedFollowUp, Session, SessionDeleteOutcome, SessionError, SessionEvent, SessionStore,
    SkillUsageRecord,
};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
