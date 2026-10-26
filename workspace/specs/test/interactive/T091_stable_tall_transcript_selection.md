# T091: Stable Selection of Tall Transcript Blocks

**Status:** Test — integrated into development through PR #80 on 2026-10-26.
State: test
Primary Feature: interactive

## Problem

Copy and row selection share one transcript viewport. After the rendered chat
grows past one page, choosing a block taller than that page calls
`ensure_selected_visible` on every redraw. The helper scrolls to the first
hidden end, then to the other end on the next frame. The TUI redraws about
every 200ms, so the selected row flickers between the start and the end of
the block. Short blocks that fit on the page already settle in one move.

## Scope

In scope: the transcript row viewport used when a chat block is selected for
reading or copy. Out of scope: clipboard delivery, drag-to-copy text
extraction, keyboard scrolling, follow-tail keys, and composer paste.

## Goals

- Selecting a transcript block keeps a stable row viewport while that block
  stays selected.
- A block taller than the page stays where it is when any of its rows are
  already visible, and otherwise opens at its first row.
- Blocks that fit on the page still scroll into view from either side.
- Follow-tail remains in effect when the visible tail of a tall block is
  selected and the user has not scrolled away.

## Non-Goals

- Changing copy delivery, clipboard fallback, drag-to-copy text extraction,
  or which block `Ctrl+Y` copies.
- Changing keyboard scrolling, follow-tail keys, or composer paste.
- Reworking mouse hit testing or native terminal selection.

## Proposed Behavior

`ensure_selected_visible` measures the selected block in rendered rows. When
that span is taller than the current page and the current window already
overlaps it, the viewport does not move. When the span is taller than the
page and lies completely outside the window, the viewport reveals the first
row of the block once. A second call with the same layout leaves that top
row unchanged. Shorter blocks keep the existing reveal-start or reveal-end
behavior.

## Affected Areas

`src/tui/activity.rs` (`ensure_selected_visible`), the transcript render test
in `src/tui/tests/render.rs`, and the transcript selection paragraph in
`workspace/docs/user/guide.md`. Version mirrors, the release ledger, and the
spec catalog record the patch. No governed instructions, session storage, or
copy payload format changes.

## Acceptance Criteria

- [x] AC-1: Repeated selection of a block taller than the page, including one
  whose tail is already visible while follow-tail is on, keeps the same top
  row and does not clear follow-tail.
- [x] AC-2: A tall block that sits entirely below the current window reveals
  its first row once, and a second reveal leaves that top row unchanged.
- [x] AC-3: A short block still scrolls to its first row when it starts above
  the window and to its last row when it ends below the window.
- [x] AC-4: The user guide states that a block taller than the page stays put
  when already visible and otherwise opens at its first row.

## Implementation Plan

1. Reserve a fresh patch from the latest published development release and apply it once. Published 0.7.1 belongs to T090, so this fix uses 0.7.2.
2. Stop `ensure_selected_visible` from alternating the ends of a block taller
   than the page, and cover that with a repeated-reveal regression.
3. Describe the stable rule in the user guide.

## Validation Gates

Focused transcript render tests while iterating (`tui::tests::render`). Then
`task test:interactive` for the TUI consumer, `task docs:check` and
`task versions:check` for the spec and reservation, and frozen `task verify`
before handoff. No live provider calls.

## Risks and Compatibility

Presentation only. A tall selected block no longer jumps to its far end, so
keyboard selection of a long message shows the visible portion or its start
rather than alternating. Reverting the helper restores the previous scroll
with no persisted state to migrate.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-tall-selection | Compatible viewport fix. Published 0.7.1 is T090 and closed, so fresh 0.7.2 is applied once from that baseline before integration. |

## Memory Impact

Status: none
Rationale: The stable viewport is a defect repair of the existing selection
contract. It adds no new preference, decision, or durable fact beyond this spec.

## Integration Evidence

Revision: 18705e72e9c8b6f8dc341205e929a20feded1f05
Outcome: passed

PR [#80](https://github.com/skills-yaml/nib/pull/80) merged into `development` at
`18705e72e9c8b6f8dc341205e929a20feded1f05` on 2026-10-26T15:50:03Z. The merged
tree matches branch tip `e657398625ba8fc94a50c3b1cd860007462be239`. This record
moves T091 to `test` and applies no second version bump. Patch 0.7.2 stays
applied. Main delivery and publication remain separate and unverified.

AC-1 and AC-2 are the repeated-reveal cases in
`selected_block_taller_than_the_page_does_not_flicker`. AC-3 is
`selected_transcript_block_is_kept_inside_the_row_viewport`. AC-4 is the
selection paragraph in `workspace/docs/user/guide.md`. Those render tests
passed on `a1018ed` before the T090 combine; the selection sources were
unchanged by that combine, and the merged tree matches the resolution tip.
`task test:interactive`, `task docs:check`, and `task versions:check` passed
on the fix branch before that combine. `task verify` and hosted CI were not
run on `e657398` or `18705e72`.
