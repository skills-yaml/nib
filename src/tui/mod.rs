//! Current-session-first Ratatui interface with live agent lifecycle rendering.

mod markdown;

#[cfg(test)]
use crate::interactive::safe_event_fields;
use crate::interactive::{
    apply_stream_event, claim_next_queued_follow_up_after_startup,
    display_stream_event_with_sensitive_values, execute_interactive_command_in_state,
    format_tui_interaction_chrome, interactive_completions, interactive_session_candidate,
    interactive_session_selection, is_legacy_thought_title, maybe_assign_session_display_name,
    meter_token_label, path_completions, persist_queued_follow_up, project_session_activities,
    queue_disposition_message, reduce_interaction, resolve_session,
    restore_queued_follow_up_after_start_failure, set_active_model, thought_header_title,
    truncate_display_cells, unicode_display_width, validate_interactive_session_target,
    wrapped_display_rows, ActivityEntry, ActivityKind, DraftHistory, DraftHistorySearch,
    InteractionConsumer, InteractionDecision, InteractionInput, InteractionReduction,
    InteractionRunState, InteractionState, InteractionTerminalOutcome, InteractiveAgentMode,
    InteractiveCompletion, InteractiveEffect, InteractiveSessionCandidate,
    InteractiveSessionSelection, ModelSelection, SelectorDetailKind, StreamDisplay,
    TranscriptViewport, TranscriptViewportAction, TuiChrome, MAX_DRAFT_HISTORY_QUERY_BYTES,
};
use crate::interactive::{bounded_public_text, control_safe_text};
use crate::llm::types::StreamEvent;
use crate::session::SessionStore;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::DefaultTerminal;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use unicode_segmentation::UnicodeSegmentation;

use crate::agent::CancellationSignal;
use crate::tools::executor::{ApprovalContext, ApprovalHandler};
use crate::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};

mod activity;
mod chrome;
mod clipboard;
mod draw;
mod input;
mod overlay;
mod pointer;
mod reports;
mod restore;
mod state;
mod worker;

pub(crate) use activity::*;
pub(crate) use chrome::*;
pub(crate) use clipboard::*;
pub(crate) use draw::*;
pub(crate) use input::*;
pub(crate) use overlay::*;
pub(crate) use pointer::*;
pub(crate) use reports::*;
pub(crate) use restore::*;
pub(crate) use state::*;
pub(crate) use worker::*;

pub use clipboard::copy_text_to_clipboard;
pub use state::{TuiApprovalHandler, TuiApprovalRequest, TuiQuestionHandler, TuiQuestionRequest};

pub fn run_tui(
    project_root: &Path,
    run_goal: Option<String>,
    requested_session: Option<String>,
    update_notice: Option<String>,
) -> io::Result<()> {
    if let Some(session_id) = requested_session.as_deref() {
        crate::config::load_nib_config_full(project_root)
            .map_err(io::Error::other)?
            .validate_public_session_id(session_id)
            .map_err(io::Error::other)?;
    }
    preflight_tui()?;
    let terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = restore_terminal();
            return Err(io::Error::new(
                error.kind(),
                format!("failed to initialize the full-screen TUI: {error}; use --plain instead"),
            ));
        }
    };
    let mut restore_guard = TerminalRestoreGuard::active();
    if let Err(error) = enable_bracketed_paste() {
        let restoration = restore_guard.restore();
        return Err(match restoration {
            Ok(()) => io::Error::new(
                error.kind(),
                format!("failed to enable bracketed paste: {error}; use --plain instead"),
            ),
            Err(restoration) => io::Error::other(format!(
                "failed to enable bracketed paste: {error}; terminal restoration also failed: {restoration}"
            )),
        });
    }
    let _ = enable_mouse_capture();

    // Terminal ownership is established before session resolution. A terminal startup
    // failure therefore cannot create a session or submit the optional initial goal.
    let result = (|| {
        let profile_scope = crate::interactive::resolve_interactive_profile_scope(project_root)
            .map_err(io::Error::other)?;
        let profile_id = profile_scope.profile_id().to_string();
        let store = profile_scope.into_session_store();
        let resolution =
            resolve_session(&store, requested_session.as_deref()).map_err(io::Error::other)?;
        let active_session_id = resolution.session_id().to_string();
        let session_origin = resolution.tui_origin().to_string();
        let session_notice = resolution.tui_notice();
        let welcome = StartupWelcome::new(project_root, update_notice);
        draw_loop(
            terminal,
            project_root,
            profile_id,
            store,
            run_goal,
            active_session_id,
            session_origin,
            session_notice,
            welcome,
        )
    })();
    let restoration = restore_guard.restore();
    match (result, restoration) {
        (Ok(notice), Ok(())) => {
            if let Some(notice) = notice {
                eprintln!("{notice}");
            }
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(notice), Err(error)) => {
            if let Some(notice) = notice {
                eprintln!("{notice}");
            }
            Err(error)
        }
        (Err(error), Err(restoration)) => Err(io::Error::other(format!(
            "{error}; terminal restoration also failed: {restoration}"
        ))),
    }
}

/// Validate the process-level terminal capabilities required by the full-screen UI.
/// This check is read-only and must run before authentication or session mutation.
pub fn preflight_tui() -> io::Result<()> {
    let term = std::env::var("TERM").ok();
    if let Some(reason) = tui_environment_rejection(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        term.as_deref(),
    ) {
        return Err(io::Error::other(format!("{reason}; use --plain instead")));
    }
    Ok(())
}

/// Return a stable rejection reason for terminal capabilities that cannot host the TUI.
pub fn tui_environment_rejection(
    input_is_terminal: bool,
    output_is_terminal: bool,
    term: Option<&str>,
) -> Option<&'static str> {
    if !input_is_terminal || !output_is_terminal {
        return Some("the full-screen TUI requires terminal input and output");
    }
    if term.is_some_and(|term| term.eq_ignore_ascii_case("dumb")) {
        return Some("TERM=dumb does not support the full-screen TUI");
    }
    None
}

fn enable_bracketed_paste_to(output: &mut impl io::Write) -> io::Result<()> {
    execute!(output, EnableBracketedPaste)
}

fn enable_bracketed_paste() -> io::Result<()> {
    enable_bracketed_paste_to(&mut io::stdout())
}

fn enable_mouse_capture_to(output: &mut impl io::Write) -> io::Result<()> {
    execute!(output, EnableMouseCapture)
}

fn enable_mouse_capture() -> io::Result<()> {
    enable_mouse_capture_to(&mut io::stdout())
}

#[cfg(test)]
mod tests;
