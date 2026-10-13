# T085: Compact Claude-Style Waiting Indicator

**Status:** Development
State: development
Primary Feature: interactive

## Authority and Scope

The user requested a wait line like `✽ Flowing… (12s · ↓ 743 tokens)` on
2026-10-13. Replace the dense TUI waiting meter with an animated star, a short
activity phrase, elapsed run time and a generated-output estimate. Keep pending
approval, question and workspace-permission labels explicit. Use current-run
received model content/tool-call chunks for the estimate, mark it approximate,
and never relabel conversation context or terminal output as generated tokens.
No billed usage, provider protocol, plain-mode controls or workload state changes.

## Affected Areas

src/tui/chrome.rs, draw.rs, state.rs, rendering/state tests and the user guide.
Version mirrors, release membership, catalog and memory records reconcile delivery.
No governed instructions change.

## Acceptance Criteria

- [ ] AC-1: Active running work renders the requested compact animated style,
  with elapsed time and an explicitly approximate generated token count.
- [ ] AC-2: Approval, question, permission, planning, compression and cancellation
  remain truthful; narrow and monochrome terminals render safe bounded text.
- [ ] AC-3: Estimates reset for a new run, accumulate model output, and exclude
  session history and terminal-tool output; run execution remains unchanged.
- [ ] AC-4: Focused render/state tests, documentation and required final gates pass.

## Implementation Plan

1. Reconcile observed 0.5.0 publication and reserve patch 0.5.1 atomically before
   implementation. Independent prospective review covers the bounded historical
   ledger read-basis adaptation; the version owner applies mirrors once.
2. Implement the compact renderer and presentation-only current-run estimate.
3. Cover active/modal/cancellation, widths, animation, counter boundaries and
   new-run reset; update user guide and record the stable user preference.
4. Obtain exact independent interface review, freeze and run task verify.
5. Keep local work in development until a confirmed shared integration event.

## Validation Gates

Use task test:interactive for focused observable render/state regressions and
Task formatting/static checks while iterating. Freeze after review and run
complete task verify; run task versions:check and task docs:check as applicable.

## Risks and Compatibility

The estimate is derived from received model output bytes, not billed usage;
mark it with `~`. Keep explicit human-wait labels and use display-cell truncation
without adding new terminal dependencies. This changes presentation only and
can be reverted without modifying persisted sessions or execution authority.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-waiting-indicator | Compatible display refinement after publicly published 0.5.0; reserve fresh 0.5.1 and apply once before artifacts. |

## Memory Impact

Status: pending
Rationale: Record the user's compact Claude-style wait-indicator preference once
implementation resolves its display and estimate contract.
