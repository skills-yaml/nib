# T039: Visible Tool Blocks and Explicit Approval Card

**Status:** Development

**Related:**
[T038: TUI Block Transcript and Key Contract](T038_tui_block_transcript_and_key_contract.md),
[T031: FT-019 Interaction Model and Ledger TUI](../done/T031_ft019_interaction_model_and_ledger_tui.md),
[T018: ratatui Approval Flow](../done/T018_ratatui_tui_approval.md),
[FT-019: Codex-Inspired Chat and TUI Interactions](../done/ft_019_codex_inspired_chat_and_tui_interactions.md)

## Summary

Make live tool work and approval decisions unmistakable in the TUI. Tool blocks get a
Grok-style diamond marker, argument summary, and status coloring. The approval dock
becomes a bordered card that names the action, risk, and explicit Y/N choices without
covering the transcript or changing approval policy. Slash and path completion reserve
rows under the composer instead of overlaying the conversation. While a turn is running
or waiting, a one-row meter shows a spinner plus job, plan step, elapsed time, tokens,
and status.

## Problem Statement

T038 collapsed tools to one mutating line, but the default view still reads as a log
label (`tool  list_directory running`) without the path, command, or result shape the
user needs to scan. The approval dock is a yellow `approval  Action: …` dump with a
muted key hint; it does not look like a blocking decision, and the status row only
says `awaiting you`.

## Product Decisions

- Tool headers use a diamond marker plus the `tool` role label so they stay readable
  without color.
- Requested/running titles include a bounded argument hint (path, command, pattern).
- Completed titles keep that hint and a result summary (`N lines`, `N entries`,
  `exit N`). Failed tools stay red even when folded.
- Expanded tool bodies are indented with a left accent. Folded tools remain one line
  with `›` when detail exists.
- Approval is still a dock, not a covering modal. The dock is a bordered card titled
  `Approval required` that shows action, permission/risk, scope, and two labeled
  choices: approve once, or deny.
- `Y`, `Enter`, and `1` approve once. `N`, `Esc`, and `2` deny. Policy is unchanged:
  one-shot grant or deny; no always-allow in this slice.
- While an approval is pending, the TUI status reads `WAITING APPROVAL` and the
  footer shows only the approval keys.
- Questions keep their existing dock; only approval chrome is redesigned.
- `/` and `@` completion is a reserved band under the composer, not a `Clear` overlay
  over the transcript. The conversation stays visible and the composer stays above the
  option list.
- A waiting meter appears between the transcript and the composer while a run is active
  or an approval/question is pending. It shows spinner, current job, plan step, elapsed
  time, a token estimate, and status.

## Scope

- Tool title composition with argument hints and diamond/accent rendering.
- Approval card layout, explicit choice rows, status/footer override, Enter/1/2 aliases.
- Layout reservation for completion under the composer and the waiting meter row.
- Tests and user-guide copy.
- No change to `ApprovalDecision`, sandbox, or always-allow policy.

## Non-Goals

- Always-allow, scope widening, or YOLO mode UI.
- Markdown, diffs-as-hunks, mouse, animation FPS, or command palette.
- Changing Y/N authority or making Esc park without answering (Grok parks; nib still
  denies on Esc, matching T018/T031).

## Acceptance Criteria

- [ ] Live tool headers render as `◆ tool  <name> <phase>` with an argument hint when
      the stream provided path/command/pattern.
- [ ] Collapsed completed `list_directory` still shows an entry count, not JSON.
- [ ] Expanded tool bodies are indented with a left accent and remain bounded.
- [ ] Failed tools are visually distinct without color (`failed` in the title) and red
      when color is available.
- [ ] The approval dock is a bordered card titled `Approval required` that names the
      action and permission/risk and shows `Y Approve once` and `N Deny` as separate
      labeled rows.
- [ ] Transcript text above the dock remains visible at ordinary terminal sizes.
- [ ] `Y`, `Enter`, and `1` grant once; `N`, `Esc`, and `2` deny.
- [ ] Status shows `WAITING APPROVAL` and the footer lists only approval keys while
      the card is open; `NO_COLOR` still has the same words.
- [ ] Focused interactive tests, `task docs:check`, `task check`, and
      `task test:interactive` pass.
- [ ] Slash completion options render under the composer; conversation text above the
      input remains visible and is not cleared.
- [ ] The waiting meter shows spinner, job, step, elapsed time, tokens, and status
      while a run is active or the TUI is waiting.

## Affected Areas

- `src/interactive.rs` — tool argument hints, title composition, display_text.
- `src/tui/mod.rs` — tool row styling, approval card, footer/status override, keys,
  below-composer completion layout, waiting meter.
- `docs/user/guide.md` — approval, tool-block, completion placement, and waiting-meter copy.
- `docs/specs/README.md` — lifecycle inventory.

## Implementation Plan

1. Compose tool titles with bounded argument hints across requested/running/terminal.
2. Render diamond + accent tool blocks with status coloring.
3. Replace the approval dock dump with a bordered explicit-choice card and status/footer.
4. Add Enter/1/2 aliases; cover with TestBackend and key-dispatch tests.
5. Reserve completion rows under the composer and stop overlaying the transcript.
6. Add the waiting meter row for job, step, time, tokens, and status.

## Validation Gates

- Unit tests for argument hints and diamond display text.
- Ratatui tests for the approval card title, choice rows, transcript visibility, and
  WAITING APPROVAL status.
- Key tests for Enter/1 grant and 2 deny.
- `task docs:check`, `task check`, `task test:interactive`.

## Risks and Mitigations

- **Small terminals:** clamp the card so the transcript keeps at least three rows.
- **Secret leakage in argument hints:** reuse existing redaction/bounding helpers.
- **Enter collision:** approval owns input while the card is open, so Enter cannot
  submit the composer.

## Rollout Notes

Presentation and TUI key aliases only. T038 remains the interaction-contract owner.
T023 and FT-020 stay out of scope.
