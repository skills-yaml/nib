//! Core agent loop: planned, approved, bounded LLM reasoning and tool execution.

use crate::agent::state::AgentState;
use crate::context::agents::{tool_instruction_scopes, InstructionResolver};
use crate::context::budget::{
    approximate_llm_input_tokens, build_bounded_runtime_input,
    ensure_required_instructions_present, RuntimePromptRequest,
};
use crate::context::skills::{Skill, SkillPolicyEffect};
use crate::context::{
    assemble_runtime_context_sections, attachment_context_sections, RuntimeContextSection,
};
use crate::llm::{
    LlmClient, LlmError, LlmErrorClass, LlmErrorPhase, LlmRequest, LlmRequestScope, LlmResponse,
    LlmStream, LlmTerminalStatus, ProviderContinuation, StreamEvent, ToolCallRequest,
    ToolResult as ProviderToolResult, ToolResultClass,
};
use crate::session::{
    normalize_plan_goal, ClarificationRecord, ClarificationStatus, HumanIntentKind,
    HumanIntentRecord, MessageOrigin, MessageProvenance, Session, SessionEvent, SessionMessage,
    SessionRunLease, SessionStore, ToolCallRecord, VerificationAuthority,
    VerificationExpectedOutcome, VerificationObligation,
};
use crate::tools::executor::ApprovalHandler;
use crate::tools::models::{AfterToolHook, PolicyEffect, PolicyRule, ToolCall};
use crate::tools::{ToolExecutor, ToolInvocationId};
use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, mpsc::Sender, Notify};

mod discussion;
mod entry;
mod finish;
mod inner;
mod question_forms;
mod support;
pub(crate) use discussion::*;
pub(crate) use question_forms::*;
mod verify;

pub(crate) use entry::*;
pub(crate) use finish::*;
pub(crate) use inner::*;
pub(crate) use support::*;
pub(crate) use verify::*;

pub use entry::{run_agent_loop, run_agent_loop_for_profile};
pub use support::{
    exact_run_steering_channel, AgentLoopConfig, AgentRunSummary, CancellationSignal,
    ExactRunSteeringHandle, ExactRunSteeringReceiver, QuestionFormRequestContext, QuestionHandler,
    QuestionOutcome, QuestionRequestContext, MAX_STEERING_INPUT_BYTES,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
