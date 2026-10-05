//! Shared interactive command grammar and effects for chat and TUI surfaces.

use crate::config::{load_nib_config_full, update_nib_config};
use crate::context::bounded_session_context;
use crate::llm::factory::provider_diagnostics;
use crate::llm::types::StreamEvent;
use crate::session::{PathAttachment, QueuedFollowUp, Session, SessionEvent, SessionStore};
use crate::tools::executor::{
    EffectiveExecutionPosture, InstructionExecutionPosture, ToolExecutor,
};
use crate::tools::ToolInvocationId;
use crate::{mcp_cmd, skill_cmd};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

mod question_form;
mod question_recovery;
pub use question_form::*;
mod split_00;
mod split_01;
mod split_02;
mod split_03;

pub(crate) use split_00::*;
pub(crate) use split_01::*;
pub(crate) use split_02::*;
pub(crate) use split_03::*;

pub use split_00::reduce_interaction;
pub use split_00::{
    active_interaction_consumer, stream_end_status_line, terminal_outcome_message,
    user_visible_stop_report,
};
pub use split_00::{
    command_effect_class, live_stop_requires_id_message, InteractiveCommand, McpCommand,
    SkillCommand,
};
pub use split_00::{
    interactive_completions, interactive_help, interactive_session_candidate,
    validate_interactive_session_target,
};
pub use split_00::{
    interactive_session_selection, InteractiveCompletion, InteractiveSessionCandidate,
    InteractiveSessionSelection,
};
pub use split_00::{
    modal_command_unsupported_message, InteractiveAgentMode, InteractiveEffect,
    InteractiveProfileScope, ModelSelection,
};
pub use split_00::{
    parse_proposed_question_input, InteractionTerminalOutcome, ProposedQuestionInput,
    TerminalOutcomeMessage,
};
pub use split_00::{
    resolve_interactive_profile_scope, ComposerSubmitKind, SelectorDetailKind,
    TranscriptViewportAction,
};
pub use split_00::{
    CommandEffectClass, InteractiveCompletionSpec, InteractiveWorkerPolicy, INTERACTIVE_COMMANDS,
};
pub use split_00::{
    DraftHistory, DraftHistoryMatch, DraftHistorySearch, InteractionConsumer, InteractionRunState,
};
pub use split_00::{
    InteractionDecision, InteractionInput, InteractionLifecycle, InteractionReduction,
    InteractionState,
};
pub use split_00::{
    InteractiveArgumentSchema, InteractiveAvailability, InteractiveCommandSpec,
    InteractiveMutability,
};
pub use split_00::{
    TranscriptViewport, MAX_DRAFT_HISTORY, MAX_DRAFT_HISTORY_QUERY_BYTES, MAX_DRAFT_HISTORY_RESULTS,
};
pub use split_01::{
    apply_stream_event, project_session_activities, summarize_tool_result, tool_argument_summary,
};
pub use split_01::{
    bottom_scroll_for_wrap, project_session_conversation, wrapped_display_rows, wrapped_line_count,
};
pub use split_01::{bounded_public_text, format_tui_interaction_chrome, TuiChrome};
pub use split_01::{
    claim_next_queued_follow_up_after_startup, restore_queued_follow_up_after_start_failure,
    unicode_display_width,
};
pub use split_01::{
    persist_queued_follow_up, queue_disposition_message, queued_follow_up_count,
    take_next_queued_follow_up,
};
pub use split_02::set_active_model;
pub use split_02::{
    display_stream_event_with_sensitive_values, execute_interactive_command,
    parse_interactive_command, resolve_session,
};
pub use split_02::{
    execute_interactive_command_in_state, persist_recovered_proposed_answer,
    persist_recovered_question_answer,
};
pub use split_02::{
    format_interaction_chrome, format_session_status, path_completions, resolve_path_attachments,
};
pub use split_02::{
    maybe_assign_session_display_name, session_title_from_conversation, SessionResolution,
    StreamDisplay,
};
pub use split_03::parse_steer_line;
pub use split_03::{
    classify_composer_submit, parse_queue_line, steer_hint_message, ActivityEntry, ActivityKind,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub use question_recovery::{
    complete_question_recovery, recover_question_conversation, QuestionRecoveryEffect,
};
