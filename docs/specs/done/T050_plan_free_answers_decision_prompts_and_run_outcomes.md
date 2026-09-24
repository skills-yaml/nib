# T050: Plan-Free Answers, Decision Prompts, and Run Outcomes

**Status:** Done

**Related:** [T041](../done/T041_task_aware_context_and_verified_completion.md),
[T047](../done/T047_user_interaction_harmonization.md),
[T026](../done/T026_actionable_redaction_safe_llm_failure_reporting.md), and
[T049](../development/T049_interaction_card.md).

## Summary

Answer clear information requests directly, present proposed answers as explicit
human decisions, and explain every terminal run outcome with a useful next action.
Plain chat, TUI, and one-shot output must agree on the meaning of each decision and
outcome while retaining their native input controls.

## Problem

The answer-only route exists but defaults off. A request such as `help` can therefore
start planning and ask the user to choose an option when a capability explanation
would have answered the request. The current question tool has a question and
optional answer list, but no explicit proposed answer to approve. A user cannot
reliably tell whether choosing an option grants authority, supplies a fact, or
merely selects a view. Several non-provider failures and loop exits appear as
internal outcome tokens or a generic `Agent run failed` message with no reason or
recovery instruction.

## Goals

- A clear information request that needs no inspection, tool, clarification, or
  action receives one answer, no plan, and no forced selection. nib then waits for
  the next user interaction.
- A proposed answer is shown next to its full question and has three explicit
  decisions: Approve the exact proposal, Reject it without answering, or Instruct
  otherwise with a typed answer. Open questions retain a free-text answer path.
- Failure, cancellation, waiting-for-input, limit, and successful completion are
  visibly distinct. A terminal message says what happened, what work remains, and
  the next user action when one exists.
- Persisted session and workload state remain authoritative; display text never
  grants approval or changes a plan on its own.

## Non-Goals

- Automatic approval of tools or plans, implicit acceptance of a proposed answer,
  or execution after Reject.
- A second classifier model call, new terminal framework, new provider error type,
  or exposure of raw provider text and secrets.
- Replanning an active plan merely because the user asks for information.

## Design

### Information requests

Enable the existing tool-free answer route by default for new interactive execute
requests. Its single model request may answer from supplied context or select the
non-executable `request_plan` control. A control request enters normal planning only
when no incomplete plan is owned by the session. With an incomplete plan, a clear
information answer leaves that plan intact; a `request_plan` result explains that
the requested work needs a separate plan or explicit continuation. `nib run` and
explicit plan mode keep their existing plan-first behavior. A literal capability
request such as `help` or `what can you do?` uses the local command registry and
returns supported capabilities without a provider request or plan. It does not
open a selector. The answer-only prompt forbids offering a menu that requires the
user to choose before receiving an informational answer.

### Questions and proposed decisions

Extend `ask_question` with an optional bounded `proposed_answer`. A proposal is
ordinary task information, never tool permission. The prompt shows the full
question, the proposed answer, why the answer is needed, and three actions:
`Approve proposed answer`, `Reject and leave unanswered`, and
`Instruct otherwise`. Approve persists exactly the displayed proposal as a human
question answer. Reject records an unresolved question and blocks dependent work.
Instruct otherwise opens the existing editor; only a non-empty submitted answer
resolves the question. Escape, EOF, and cancellation keep their T047 meanings.

An `ask_question` without `proposed_answer` shows the question and its optional
suggestions with an editable answer; it does not show an Approve action with no
defined object. TUI, plain, one-shot, and later `/questions` recovery use the same
semantics. Proposal text and decision provenance are persisted with the exact
clarification identity. User-visible labels distinguish question decisions from
tool approvals and session selections.

### Terminal outcomes

Use a shared presentation mapping from structured terminal outcomes to a safe
heading, explanation, remaining-work statement, and next action. The mapping covers
at least completed, plan ready, cancelled, waiting for input, model refusal, empty
model response, tool failure, repeated tool failure, blocked step, unresolved
verification, turn or transition limit, instruction context failure, and LLM failure.
Typed LLM failures retain T026's report. Do not infer failure causes from provider
prose or reclassify a failure as assistant speech. Emit one terminal result after
reconciliation and show the same safe meaning on session reload, in plain chat, in
TUI, and from `nib run`. Preserve stable machine outcome tokens and exit codes.

## Affected Areas

- `src/config/mod.rs`, `src/agent/instructions.rs`, `src/agent/loop.rs`: answer route,
  question request, persisted decision, and terminal summary.
- `src/tools/registry.rs`, `src/session/mod.rs`, `src/llm/types.rs`: bounded proposal
  schema and backwards-compatible durable representation.
- `src/interactive.rs`, `src/chat.rs`, `src/console.rs`, `src/tui/mod.rs`, `src/run.rs`:
  common input and output semantics with native controls.
- `scripts/check-interactive-release.sh`: native approval prompt smoke expectation.
- `docs/user/guide.md`, `README.md`, relevant completed spec supersession notes,
  and interaction/runtime tests.

## Acceptance Criteria

- [x] `help`, `what can you do?`, and a clear tool-free information request produce
  one answer, no plan or question, and leave nib ready for the next turn. A request
  needing evidence or action enters planning once without emitting a partial answer.
- [x] An informational answer does not complete or replace an unrelated incomplete
  plan. An explicit plan request and one-shot `nib run` retain their plan contracts.
- [x] A proposed question visibly names the question and exact proposal. Approve
  persists that answer, Reject leaves it unresolved, and Instruct otherwise persists
  only the submitted alternative. Escape, EOF, retry, cancellation, and recovery
  remain deterministic in TUI, plain, and one-shot surfaces.
- [x] No proposal means no ambiguous Approve control. A question answer never
  authorizes a tool or waives required verification.
- [x] Every terminal outcome above has a concise reason and next action where
  applicable; streamed and reloaded presentations agree, with no duplicate final
  event or raw secret/provider text.
- [x] Focused interaction and runtime tests, `task check`, `task docs:check`,
  `task verify`, and relevant native interaction smoke pass.

## Implementation Plan

1. Add answer routing and its no-plan/active-plan regression fixtures.
2. Add the optional proposal and shared decision parsing, then native renderers
   and exact clarification persistence/recovery fixtures.
3. Add shared terminal outcome presentation and cover stream, reload, and one-shot
   paths. Update user documentation and supersession notes.
4. Review spec compliance and code quality, then run the required gates.

## Validation Gates

- `task check`, `task test:interactive`, `task test:runtime-e2e`, and `task docs:check`
  during implementation.
- `task verify` before completion.
- `task smoke:interactive` for native terminal input, recovery, and status output.

## Risks and Mitigations

- A model may answer without enough evidence. Keep the route tool-free, tell it to
  request planning when evidence is missing, and test that control and failure paths.
- A proposal could be mistaken for action permission. Label it as an answer to the
  displayed question and leave all ToolExecutor approval gates unchanged.
- Terminal messages could reveal private failure details. Derive them from safe
  typed fields and bounded local outcome codes; retain existing redaction.

## Rollout

This changes the interactive answer-only default and supersedes T041's opt-in
default. Existing explicit `agent.answer_only = false` remains an override.
Previously persisted clarifications without a proposal retain their open-question
UI and recovery behavior. Existing machine outcome tokens remain unchanged.

## Open Questions

None for the scoped behavior. The user confirmed the proposed-answer decision
meanings and that plan-free replies apply to all clear tool-free information
requests on 2026-09-23.

## Validation (2026-09-24)

- `task test:interactive`: passed, including SIGINT reconciliation.
- `task test:runtime-e2e`: 49 passed with local HTTP fixtures enabled.
- `task docs:check`: passed.
- `task verify`: passed after the scheduled-run report assertion was updated.
- `task smoke:interactive`: passed; a stale approval prompt expectation in the
  native script was corrected and `task smoke:interactive:binary` then passed
  without a timeout diagnostic.
