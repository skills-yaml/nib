# T057: Contextual Answers and Plan Progress

**Status:** Development

**Related:** [T050](../done/T050_plan_free_answers_decision_prompts_and_run_outcomes.md),
[T039](../done/T039_visible_tool_blocks_and_explicit_approval.md), and
[T055](../done/T055_conversational_repository_aware_help.md).

## Problem

The interactive answer route is enabled by default but gated by the unrelated
`execution.plan_mode` setting, which also defaults on. Ordinary questions therefore
enter planning. The transcript always shows a plan checklist, including for a
single action. Live checklist updates reparse bounded display text and can disagree
with the persisted plan, especially at final completion or a blocked step.

## Goals

- Answer clear, context-sufficient interactive questions directly, without a plan
  or approval prompt.
- Keep mutation authorization and tool approval rules intact.
- Show a transcript checklist only for plans with two or more steps. Keep `/plan`
  available for inspecting any persisted plan.
- Update visible multi-step progress from authoritative persisted step state,
  including pending, active, blocked, stopped, and completed steps.

## Scope

Interactive conversational routing and the transcript projection of persisted
plans. The change covers plain chat and TUI through their shared activity stream.

## Non-Goals

- Changing the explicit `/plan` or one-shot `nib run` plan-first contract.
- Automatically approving tool actions or changing persisted plan semantics.
- Adding a new todo database, command, or independent workload model.

## Design

Interactive answer eligibility depends on `agent.answer_only`, interactive
execute mode, and safe run/session state. `execution.plan_mode` continues to gate
mutating tools but does not prevent a tool-free answer. Requests needing evidence
outside supplied context may still enter planning under T050's existing control.

The live transcript receives a redacted plan progress snapshot after persisted
generation, approval, and reconciliation. TUI renders checklist markers from the
snapshot; plain chat prints a concise progress line. A snapshot carries only
public display fields and exact plan identity; it cannot grant approval or change
the saved plan. A new plan gets its own display entry; updates affect only an entry
with the same plan identity. A one-step plan emits no transcript checklist. The
TUI refreshes from the saved plan when its worker finishes, covering a dropped
progress event. The persisted plan remains available through `/plan` and the
session audit.

## Affected Areas

- Agent answer routing, plan lifecycle streaming, and tool gate separation.
- Typed stream events and interactive/TUI plan projection.
- Runtime and interaction regression tests; user guide and spec catalog.
- No persisted schema migration or external service change.

## Acceptance Criteria

- [x] Default interactive configuration answers a context-sufficient question
  without generating a plan or seeking approval; explicit plan mode and one-shot
  behavior retain their contracts.
- [x] A single-step plan has no transcript todo entry in live and reloaded views;
  `/plan` still describes it.
- [x] A multi-step plan shows exactly one current checklist per plan identity and
  reflects saved pending, in-progress, blocked, stopped, and completed statuses.
- [x] Final completion updates the last checkmark immediately; reload shows the
  same progress as the live view.
- [x] Verification detail survives progress updates; long display truncation and
  stale updates cannot invent completion or alter another plan's checklist.
- [x] Existing mutation, tool approval, and verification gates remain effective.

## Implementation Plan

1. Decouple interactive answer eligibility from the mutation plan gate and add a
   default-config regression.
2. Add a public, bounded plan progress event sourced from saved plan state;
   update the live checklist by plan identity and suppress one-step entries.
3. Cover final, blocked, stale, and reload paths; update the guide and run gates.

## Validation Gates

- `task check`, `task test:interactive`, `task test:runtime-e2e`, and
  `task docs:check` while iterating.
- `task verify` before claiming completion.

## Risks and Rollback

Progress events must not expose plan outcomes, secrets, or private verification
content. Keep only the fields needed for display and apply existing redaction and
bounds. If a progress event is missing, worker completion or session reload
still projects the saved plan.
Rollback restores the previous stream and rendering code without data migration.

## Memory Impact

The stable distinction between the mutation plan gate and direct-answer routing,
and the multi-step-only checklist rule, are recorded in
`agents/memory/decisions.md`.

## Local Validation Evidence

On 2026-09-29, `task verify` passed the complete static, serial test, and
doctest gate: 1,244 library tests plus CLI and integration suites. Focused
`task test:interactive`, `task test:agent-context`, `task test:runtime-e2e`,
`task test:llm-failure-cli`, and `task docs:check` also passed during iteration.
The spec remains in Development pending integration evidence.
