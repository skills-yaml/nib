# T025: Interactive Chat and TUI Capability Parity

**Status:** Done
**Related:** [FT-011](../done/ft_011_llm_streaming_and_tui.md), [T018](../done/T018_ratatui_tui_approval.md), [T011](../done/T011_end_user_documentation.md)

## Summary

Make `nib chat` and `nib tui` equivalent interactive entry points for the same
session-scoped agent capabilities. Their presentation may differ, but a user must be
able to authenticate before launch, select or create a session, submit repeated goals,
inspect and change the active model, inspect providers, manage skills and MCP servers,
answer questions, approve or deny actions, observe live execution, and exit or cancel
without switching interfaces.

## Problem Statement

`nib chat` is a multi-turn REPL with session, model, provider, skill, and MCP commands.
`nib tui` is currently a session browser that can execute only one startup `--run`
goal. It has richer live rendering and modal approval/question handling, but it cannot
accept another goal, resume a selected session for execution, or perform the chat
management commands. The two interfaces therefore expose different product
capabilities even though they use the same agent loop.

## Scope

- Define one shared interactive command vocabulary for chat and TUI.
- Add a TUI composer for repeated goal submission into one active session.
- Add TUI `--session` and `--auth` entry behavior matching chat.
- Support help, provider inspection, session inspection/reset, model selection, skill
  management, and MCP management from both interactive interfaces.
- Allow the TUI session browser to make a selected session active for later turns.
- Surface streamed model and tool lifecycle output in both interfaces.
- Preserve TUI approval, question, detail, cancellation, and bounded-rendering behavior.
- Preserve profile-scoped session persistence and exact agent-loop reconciliation.

## Non-Goals

- Pixel-identical output or identical key bindings between line mode and the TUI.
- A new provider, tool, persistence model, or authentication mechanism.
- HTTP MCP transport or external messaging-provider UI.
- Changing non-interactive `nib run` behavior.

## Acceptance Criteria

- [x] Chat and TUI recognize the same slash-command names and argument grammar.
- [x] Both interfaces support repeated agent turns in the same active session.
- [x] Both interfaces can start with a new session or resume an existing session.
- [x] Both interfaces expose provider/model inspection and model selection.
- [x] Both interfaces expose skill list/install/remove and MCP list/add/remove.
- [x] Both interfaces support pre-launch authentication and automatically offer it when
      no provider is configured.
- [x] Both interfaces surface streamed content and tool lifecycle progress.
- [x] Both interfaces route approvals and questions through their native input surface.
- [x] TUI exit during a run cancels, reconciles, drains events, and joins the worker.
- [x] Command, session, streaming, and modal state remain bounded and deterministic.
- [x] The user guide accurately documents parity and interface-specific controls.
- [x] `task docs:check`, `task check`, and `task test` pass.

## Affected Areas

- `src/chat.rs`
- `src/main.rs`
- `src/lib.rs`
- `src/tui/mod.rs`
- Shared interactive command/session support under `src/`
- `src/skill_cmd.rs` and `src/mcp_cmd.rs` if presentation-neutral operations must be
  exposed to both interfaces
- `docs/user/guide.md`
- Interactive unit and deterministic rendering tests

## Implementation Plan

1. Introduce a shared, presentation-neutral slash-command parser and help contract.
2. Reuse presentation-neutral model, provider, skill, MCP, and session operations from
   chat and TUI.
3. Add TUI active-session state, composer focus, repeated worker launches, session
   resume/new-session behavior, and command result rendering.
4. Stream agent events in chat while retaining the TUI's bounded live view.
5. Add command-parity, multi-turn, session, modal, cancellation, and rendering tests.
6. Update the user guide and reconcile this spec only after canonical gates pass.

## Validation Gates

- Deterministic shared-command parser and command-effect tests.
- TUI input/focus/session/worker lifecycle tests using Ratatui `TestBackend` and direct
  key dispatch.
- Chat streaming plus existing command/session/approval/question tests.
- Manual raw-terminal smoke for chat and TUI multi-turn, model selection, approval,
  question, cancellation, and session resume.
- `task docs:check`.
- `task check`.
- Independent `task test`.

## Risks

- Raw-mode input can conflict with session-navigation keys; focus must be explicit and
  modal input must remain highest priority.
- Long-running skill installation must not corrupt terminal restoration or lose output.
- A completed TUI worker must release all senders before another turn starts.
- Shared command refactoring must preserve existing configuration validation,
  redaction, and atomic update guarantees.
- Existing unrelated worktree changes in provider/catalog code must remain intact.

## Implementation Reconciliation (2026-08-15)

- `src/interactive.rs` now owns the presentation-neutral command grammar, session
  resolution, model/provider operations, skill and MCP effects, help text, and stream
  event formatting used by both interactive surfaces.
- Chat preserves its multi-turn console workflow while rendering model, tool, and
  reconciliation events as they arrive. TUI now has a bounded composer, an active
  session, repeated worker launches, session resume, model selection, and the same
  slash commands as chat.
- TUI approval and question modals remain native to the raw-terminal interface.
  Ctrl+C cancels an active worker, shutdown drains pending interaction channels and
  joins the worker, and Ctrl+Q exits.
- Skill and MCP command modules expose presentation-neutral formatting/mutation helpers
  so neither interactive surface duplicates management behavior.
- README, user-guide, and project-structure documentation describe the shared
  capabilities and interface-specific controls.

## Validation Evidence (2026-08-15)

- Shared command/effect, chat session/stream/input, TUI composer/model/session/worker,
  CLI option, and existing modal/cancellation tests passed.
- `task check`: installer checks, formatting, Clippy with warnings denied, compilation,
  the full test suite, and documentation integrity passed.
- Independent `task test`: 968 tests passed; the credential-gated live-provider test
  remained intentionally ignored.
- `task docs:check`: all five documentation invariants passed.
- `task check:all-targets` passed.
- Linux raw-terminal smoke passed TUI repeated turns, approval, question selection,
  cancellation/exit, exact model selection, and persisted session resume. Chat
  multi-turn approval and streamed lifecycle output also passed against the mock
  provider.
