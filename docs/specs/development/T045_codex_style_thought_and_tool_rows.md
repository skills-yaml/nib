# T045: Codex-Style Thought and Tool Rows

**Status:** Development

**Related:**
[T039: Visible Tool Blocks](../done/T039_visible_tool_blocks_and_explicit_approval.md),
[T038: TUI Block Transcript and Key Contract](../done/T038_tui_block_transcript_and_key_contract.md)

## Summary

Render a turn as a scan list: a folded `▸ Thought for Ns` header, stacked
`● {tool}  {hint}` rows, and a nested spinner under the running tool. Do not copy
Claude `Ctrl+O` or tip chrome. T038 keys, T039 chrome, markdown speech, plan
todos, copy-on-release, and under-composer decisions stay.

## Scope

TUI thought and tool row presentation in the current-session transcript, plus
matching thought titles in the shared activity model. Approval, chrome, speech
markdown, plan todos, and copy-on-release are out of this slice.

## Problem Statement

Tool and thought rows still read as a status log (`● planning`,
`● read_file running · path`). The compact source layout is a thought summary
plus one-line tool calls, with live work nested under the active tool.

## Goals

- Thought is a chevron header `Thought for {elapsed}` and, when the waiting
  meter has a token count, `, N tokens`. Folded thought is header-only.
- Quiet tool rows hide `running` and `ok`. Keep `failed` and result summaries
  (`exit N`, `N lines`).
- A running tool shows a nested spinner line (`Running command…` /
  `Reading…`). Full command and output stay in the expanded body.
- Fold remains Left/Right on the selected block.

## Non-Goals

- New fold key (`Ctrl+O`).
- Claude/Cursor reject-edit tips.
- Changing approval policy or under-composer lists.
- Reopening T039.
- Exact per-thought token accounting beyond the existing meter estimate.
- Restyling plain-mode logs beyond matching thought/tool titles.

## Design

- New thought blocks start folded. A later tool or assistant row freezes the
  open thought's duration into its title and starts a new thought on the next
  planning state.
- TUI thought marker is `▸` folded and `▾` expanded. Tools keep `●`.
- Stored tool titles still include phase for status coloring and job labels.
  The TUI quiet row drops `running`/`ok`/`requested`.
- Running tools stay folded for body output; the nested live line is always
  shown while the phase is running or requested.
- Token suffix uses the meter value when it is not `-`.

## Affected Areas

- `src/tui/mod.rs` — thought/tool/live row rendering.
- `src/interactive.rs` — thought create/freeze, default fold.
- `docs/user/guide.md` — transcript copy.
- TUI and interactive tests.

## Implementation Plan

1. Thought header title helper and freeze-on-tool/speech.
2. Quiet tool header and nested running line.
3. Tests and user-guide update.

## Validation Gates

- `task check`
- `task test:interactive`
- `task docs:check`

## Risks

- Historical sessions still project `planning` titles; TUI maps those to
  `Thought`.
- Hiding `running` must not hide `failed`.

## Acceptance Criteria

- [x] Folded thought renders as `▸ Thought for {elapsed}` (and `, N tokens`
      when the meter has tokens). Body is hidden until Left/Right expand
      (`▾`).
- [x] A new thought block starts after tools; the previous thought keeps its
      frozen duration.
- [x] Quiet completed/running tool rows are `● {name}  {hint}` without
      `running` or `ok`. Failed rows still include `failed`.
- [x] A running `run_terminal` shows a nested spinner line; command/cwd remain
      in the expanded body.
- [x] Left/Right still fold the selected block. No `Ctrl+O`.
- [x] Focused interactive tests, `task check`, and `task docs:check` pass.

## Memory Impact

Status: pending

## T047 coordination (2026-09-21)

[T047](../done/T047_user_interaction_harmonization.md) retains T045's drag-release copy
interaction and scan-list presentation while owning truthful native/OSC52/failure
feedback and the rule that redirected output receives no clipboard escape sequence.
T045's focused rendering evidence does not qualify those changed clipboard semantics;
T047 carries their cross-surface and native validation.
