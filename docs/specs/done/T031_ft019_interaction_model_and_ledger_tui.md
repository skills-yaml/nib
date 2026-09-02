# T031: FT-019 Interaction Model, Ledger TUI, and Queue-Only Live Input

**Status:** Done
**Related:** [FT-019](../done/ft_019_codex_inspired_chat_and_tui_interactions.md),
[T025](T025_interactive_chat_tui_capability_parity.md),
[T028](T028_current_session_first_tui_and_slash_command_completion.md),
[T030](T030_unified_interactive_cli_and_plain_mode_fallback.md),
[T018](T018_ratatui_tui_approval.md),
[T003](../done/T003_context_engine_with_dynamic_compression_and_session_management.md),
[T026](../done/T026_actionable_redaction_safe_llm_failure_reporting.md),
[FT-012](ft_012_richer_planner.md),
[FT-017](../done/ft_017_managed_process_supervisor.md)

## Summary

Implement the first FT-019 delivery slice: one presentation-neutral interaction
model and command registry, queue-only live input, additive session queue/name/fork
fields, a ledger TUI, and plain/chat semantic equivalents. Capability-gate
`/compact`, `/ps`, `/stop`, and steer until their runtime owners exist.

This child spec is what authorizes FT-019 to move from backlog to development.
Implementation must not start from the backlog umbrella alone.

## Problem Statement

T025/T028/T030 shipped one interactive product whose TUI is still a string dump
with a one-line composer and covering approval modals. The registry lacks FT-019
command families. Submit-while-running is rejected instead of queued. The agent
loop cannot bind a steer to the exact active run.

## Product Decisions

- Shared reducer and registry live in `src/interactive`. TUI and plain map keys and
  prompts onto it; they do not own session effects.
- Enter submits when idle and queues when a turn is running. Enter never steers.
  `Ctrl+S` reports that steer is unavailable.
- Queued follow-ups persist on the session before the UI acknowledges them. They
  start at most once after the preceding run is reaped. Cancel, exit, and session
  switch report whether queue entries are retained on that session.
- `/new` aliases `/clear`. `/resume` aliases `/session` preview-and-confirm.
- `/compact`, `/ps`, and `/stop` parse and appear in help/completion but execute as
  explicit unavailability messages. They must not pretend to compact or stop work.
- `/fork` copies the source transcript into a new session with `forked_from` set and
  does not mutate the source. `/rename` sets additive `display_name`.
- TUI is the four-region ledger. Approval and question are docks that leave the
  transcript visible. Selector errors stay on the overlay. Exact-ID preview of a
  listed candidate replaces that candidate including `snapshot_token`.
- Default keys: `Ctrl+J` newline, `Enter` submit/queue, `Ctrl+C` cancel, `Ctrl+Q`
  quit. `KeyEventKind::Repeat` is treated as press.

## Persistence Decisions

Additive `Session` fields, defaulted so old JSON remains readable:

- `queued_follow_ups: Vec<QueuedFollowUp>` with `id`, `text`, `created_at`, `source`.
- `display_name: Option<String>` (bounded).
- `forked_from: Option<String>`.

Crash recovery never auto-starts a queued follow-up; the interactive product starts
the next queued item only after a foreground worker it owns has been joined in that
process. Ambiguous leftover queue remains persisted and is shown by `/status`.

## Scope

- Expand the command registry with FT-019 names, aliases, availability, and help.
- Add classify/persist/take APIs for idle submit vs queue vs disabled steer.
- Project session + stream events into typed activity entries.
- Render TUI header/status rows, activity transcript, wrapped composer, docks.
- Fix switcher overlay errors and exact-ID snapshot refresh.
- Plain/chat remains turn-synchronous. Users enqueue with a `queue: <text>` line;
  after a turn completes, the next queued item starts at most once. TUI Enter while
  running persists a queue entry before acknowledgement.

- Implement ungated command bodies: `/status`, `/permissions`, `/plan`, `/review`,
  `/diff`, `/new`, `/resume`, `/fork`, `/rename`, `/copy`.
- Gate `/compact`, `/ps`, `/stop`.
- Update user guide and tests.

## Non-Goals

- Exact-run steering, T003 user compact, FT-017 `/ps`/`/stop` bodies.
- Codex clone, theming, mouse-only, file tree, `nib run` changes.

## Acceptance Criteria

- [x] FT-019 is in `docs/specs/development/` and this spec has scope, acceptance,
      affected areas, persistence decisions, and validation gates.
- [x] Registry parse/help/completion cover new and compatibility names; gated
      commands explain why they are unavailable.
- [x] Idle submit starts one turn; running Enter queues and never steers; steer
      action reports unavailability.
- [x] Queue persist-before-ack; cancel/exit/switch report disposition; queued work
      starts at most once after worker join.
- [x] TUI ledger: two header/status rows, typed activity roles, wrapped composer,
      approval/question docks that leave transcript text visible, unicode-width
      follow-tail.
- [x] Switcher errors overlay-local; exact-ID refresh of listed candidates.
- [x] Plain `--plain` `/help` lists supported names; incomplete `/` is not a goal.
- [x] `task docs:check`, `task check`, and `task test` pass.

## Implementation Plan

1. Add additive session fields and queue persist/take APIs.
2. Expand the shared command registry, parser, effects, activity projection, and
   status chrome.
3. Render the TUI ledger, docks, wrapped composer, overlay-local switcher errors,
   and exact-ID snapshot refresh.
4. Wire plain/chat `/help`, `/status`, `queue:`, and post-turn queue start.
5. Gate `/compact`, `/ps`, `/stop`, and steer. Prove with unit, TestBackend, and
   `--plain` child-process tests.

## Affected Areas

`src/interactive.rs` (or `src/interactive/`), `src/tui/`, `src/chat.rs`,
`src/session/mod.rs`, `src/console.rs` as needed, `docs/user/guide.md`,
`docs/specs/`, `tests/interactive_cli.rs`, TUI unit tests.

## Validation Gates

- Unit tests on shipped parse/classify/queue/status/fork functions.
- Ratatui `TestBackend` tests for ledger layout, docks, overlay errors, exact-ID
  refresh, unicode-width scroll helper.
- `tests/interactive_cli.rs` `--plain` child-process `/help` and `/status`.
- `task docs:check`, `task check`, `task test`.

## Risks and Mitigations

- Session mismatch checks must include new fields.
- Do not auto-start queued work on process start.
- Gated commands must not call compact or process-stop APIs.

## Implementation Reconciliation (2026-08-21)

T031's acceptance criteria are implemented in `src/interactive.rs`, `src/tui/mod.rs`,
`src/chat.rs`, and `src/session/mod.rs`. Queue persist-before-ack, ledger rendering,
approval/question docks, overlay-local switcher errors, exact-ID snapshot refresh,
composer caret, wrap-based height, and bounded draft history are covered by library
and `--plain` child-process tests. `/compact`, `/ps`, `/stop`, and steer remain
explicitly gated. Canonical `task docs:check` and `task check` evidence is recorded
on the completing revision. Umbrella FT-019 stays in development for remaining
steer/compact/process bodies and native macOS/Windows terminal jobs.

## Final Independent Review Evidence (2026-08-27)

A fresh independent spec-compliance review passed all 8/8 T031 acceptance criteria,
and a separate fresh code-quality/security review passed with no unresolved findings.
The reviews covered the now-shipped registry/reducer, modal framing, queue and failure
dispositions, exact-run steering, terminal reconciliation, and profile/session/worktree
authority. Later FT019 child slices supersede the historical note above that
`/compact`, `/ps`, `/stop`, and steering were gated; T031's delivered model and ledger
boundaries remain compatible with their implementations. Focused validation passed
all 160 interactive tests, including the lazy console-input broker regression, all 39
installer/static-contract tests, `task docs:check`,
`task check:all-targets`, and `git diff --check`. Native FT019 completion evidence
remains owned by T034 and the development umbrella rather than this completed slice.
