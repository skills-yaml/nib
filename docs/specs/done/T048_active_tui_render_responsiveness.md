# T048: Active TUI Render Responsiveness

**Status:** Done

**Related:** [T038 block transcript](../done/T038_tui_block_transcript_and_key_contract.md),
[T042 context budgeting and visibility](../backlog/T042_context_budgeting_and_live_visibility.md),
[T047 interaction harmonization](../done/T047_user_interaction_harmonization.md)

## Summary and Problem

Keep keyboard input responsive and CPU use bounded while an agent runs in a long
session. The current TUI loop polls every 200 ms, reloads and validates the full
session under its lock, flattens every activity into wrapped/markdown rows, and
rebuilds the complete pointer-selection text before reading a key. This makes each
idle animation frame scale with retained session and transcript size.

## Scope and Goals

- Cache the presentation snapshot used for chrome, queue count, plan step, and the
  approximate token label; refresh it at a bounded cadence without holding the UI
  behind a busy session lock.
- Cache transcript layout across unchanged frames. Input processing, spinner and
  elapsed-time feedback, scrolling, folding, copy selection, and session switching
  must remain correct.
- Bound event-drain work so a sustained stream cannot indefinitely defer keyboard
  input. Preserve event order and exact-run/session filtering.
- Record deterministic cache/rebuild evidence and a long-transcript interaction
  fixture, including active agent output and typing.

## Non-Goals

T042 owns complete context accounting and monitoring snapshots. This task does not
change model prompts, provider usage, persistence authority, approval policy, tool
execution, or the transcript's public content. It does not introduce a new TUI
framework or background session database.

## Design

Keep `SessionStore` authoritative. The TUI holds a display snapshot scoped to one
session and requests a newer snapshot on a background reader no more than once per
second. The frame loop only checks a nonblocking result channel. Timed lock
acquisition leaves the last snapshot visible until the next attempt.
An explicit session switch discards that snapshot before rendering the new session.
The approximate token label is calculated when the snapshot changes, not on each
frame.

Keep the rendered transcript rows in a presentation-only cache keyed by the active
session, activity change generation, terminal width, color mode, and live title time
bucket. Unchanged frames reuse wrapped/markdown rows and only clone the visible rows
for drawing. Fold and session-switch actions invalidate the cache; a live thought or
running tool updates at least once per second. Pointer row mapping is derived from
the same cached rows used for rendering, so copy and selection cannot drift.
When a stream event changes one activity, unchanged activity renders are reused.

Drain at most a fixed batch of stream events per UI iteration; leave remaining events
in the channel in order for later iterations. Preserve terminal reconciliation and
approval/question responsiveness when the stream is busy.

## Affected Areas and Implementation Plan

Affected areas: `src/tui/mod.rs`, focused TUI tests, and this spec. Validation also
stabilized the local HTTP recovery fixture in `tests/llm_failure_cli.rs` and added
its focused Task entrypoint. No session schema or product documentation changes
are expected.

1. Add the session-display and transcript-layout caches with explicit invalidation.
2. Bound stream drain and test that pending keyboard work can make progress.
3. Run focused interaction and shutdown tests, then the canonical local gate and
   inspect the diff for stale or cross-session presentation.

## Acceptance Criteria

- [x] The TUI idle frame path does zero full session loads after startup and unchanged
      frames within a live-title time bucket do zero transcript flatten/markdown
      passes. Background refresh is at most once per second and a busy session lock
      cannot block keyboard handling.
- [x] A large retained transcript does not make each key wait for full-history
      formatting; visible rows, selection, copy, fold, resize, and session switch
      remain correct.
- [x] Active spinner/elapsed feedback updates at least once per second, and
      session-backed chrome/queue/plan labels update within a bounded second when
      the store is available.
- [x] Sustained stream output cannot monopolize the TUI iteration; events remain
      ordered and reconcile exactly once.

## Validation Gates

- [x] `task check`, `task test:interactive`, `task test:tui-shutdown`,
      `task verify`, `task docs:check`, and `git diff --check` pass. Relevant native
      interactive smoke passes before completion.

## Validation Record (2026-09-23)

`task verify` passed the complete serial local suite, including 1,184 library tests,
integration tests, and doctests. `task test:interactive`, `task test:tui-shutdown`,
`task check`, and `task docs:check` passed. The current-tree optimized binary built
with `task build` and passed `task smoke:interactive:binary` on Linux. The focused
local HTTP recovery fixture passed four consecutive runs after bounded polling and
diagnostic improvements. `git diff --check` passed.

## Risks, Alternatives, and Rollout

A cached view may be briefly stale under lock contention; show the last valid
snapshot and retry on the next cadence. The display cache never grants authority.
Full row rebuilding on each tick is simpler but retains the reported slowdown.
Moving the renderer to another framework or truncating scrollback would change
interaction behavior and is outside scope. This is a local TUI change with no
persistence migration or feature flag.

## Open Questions

None block the implementation. Cache invalidation and batch size are implementation
details subject to the acceptance fixtures above.
