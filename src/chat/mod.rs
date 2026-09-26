use clap::Args;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::auth::run_auth_wizard;
use crate::console::ConsoleInput;
use nib::config::load_nib_config_full;
use nib::interactive::{
    claim_next_queued_follow_up_after_startup, execute_interactive_command_in_state,
    format_session_status, interactive_completions, interactive_session_candidate,
    maybe_assign_session_display_name, persist_queued_follow_up, project_session_conversation,
    queue_disposition_message, reduce_interaction, resolve_interactive_profile_scope,
    resolve_session, set_active_model, validate_interactive_session_target, ActivityKind,
    DraftHistory, InteractionConsumer, InteractionDecision, InteractionInput, InteractionReduction,
    InteractionRunState, InteractionState, InteractionTerminalOutcome, InteractiveAgentMode,
    InteractiveCommand, InteractiveEffect, InteractiveSessionSelection, ModelSelection,
    SelectorDetailKind, SessionResolution, StreamDisplay,
};
use nib::session::SessionStore;

mod split_00;
mod split_01;

pub(crate) use split_00::*;
pub(crate) use split_01::*;

pub use split_00::{run_interactive, ChatArgs};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
