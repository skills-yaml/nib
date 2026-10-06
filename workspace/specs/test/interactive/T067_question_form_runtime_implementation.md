# T067: T061 Question Form Runtime Implementation

State: test
Primary Feature: interactive

## Problem

[T061](T061_question_form.md) defines the approved question-form and conversational
recovery contract. Its accepted-contract documentation contributed the minor
impact to the already-published 0.2.0 release. This linked implementation spec
versions all new runtime code at 0.3.0 while preserving that published history.
No product scope or accepted interaction decision is changed.

## Scope

Implement every T061 contract and acceptance criterion, including legacy/tool
schema compatibility, related sets, choice descriptions, visible custom editors,
discussion versus interruption, persisted linked obligations, exact conversational
operation recovery, slash-command removal, runtime prompt and user documentation.
No paid live LLM qualification or governed instruction changes are included.

## Acceptance Criteria

- [ ] AC-1: Legacy and structured single-question cards meet T061 selection,
  description, visible text, digit navigation and local retry behavior.
- [ ] AC-2: Sets of two to eight preserve drafts and return answers only on Submit;
  one-item arrays keep the single card; incomplete Submit focuses unanswered input.
- [ ] AC-3: Discussion succeeds while keeping every dependency obligation unresolved;
  Esc/EOF/cancellation reconcile and stop the worker as specified by T061.
- [ ] AC-4: Proposed-answer rows approve only the displayed value, preserve rejection,
  allow replacement and whole-call discussion, including mixed sets.
- [ ] AC-5: Plain/chat/console/one-shot share rows, descriptions, literal text escaping,
  discussion input, sequential drafts and final set submit/reopen.
- [ ] AC-6: Exact reuse, changed-description re-ask and linked operation recovery survive
  restart; ambiguous/unrelated inputs cannot clear or resume the wrong obligation.
- [ ] AC-7: /plan and /questions disappear from interactive registration/help/completion,
  preserving internal plans, progress and audit state.
- [ ] AC-8: Answers and discussion never approve tools or waive verification; malformed
  calls are rejected before handlers and invalid input retries locally.
- [ ] AC-9: Behavioral fixtures, documentation and frozen native/hosted gates pass;
  T061 and this linked implementation reconcile acceptance and actual lifecycle events.

## Affected Areas

All T061 affected areas apply: src/tools, src/agent, src/session, src/interactive,
src/tui, src/chat, src/console.rs, src/run.rs, related tests, and user guide.
Cargo.toml, Cargo.lock, skills.yaml and workspace/releases.json carry the single
applied nib-question-form 0.3.0 version; this spec owns its runtime membership.

## Implementation Plan

Follow the adjacent [T061 plan](T061_question_form.plan.md). The preparation owner
applies version0.3 once; isolated lanes implement/review the full T061 contract,
then frozen combined gates and actual development/main events govern completion.

## Validation Gates

- task check and task test (the exact ordered task verify composition).
- task test:interactive, task test:agent-context and task docs:check.
- Applicable native interactive/release-binary qualification and hosted
  Linux/macOS/Windows gates on the exact combined implementation candidate.

## Risks and Compatibility

T061's legacy entrypoints and persistence defaults stay compatible. Successful
discussion cannot discharge dependency obligations. Auto-resume must bind exact
operation/plan/invocation/question identities and preserve execution admission.
Published 0.2.0 history is retained; rollback never decrements a published target.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | minor | nib-question-form | Ships all approved T061 runtime additions at fresh 0.3.0 from published 0.2.0; the preparation owner applies the bump once at development-start. |

## Memory Impact

Status: pending
Rationale: Resolve shared T061 shipped form/recovery/compatibility facts from exact
implementation and review evidence, then update category and changelog once.

## Implementation Evidence (2026-10-06)

The [T061 evidence table](T061_question_form.md#implementation-evidence-2026-10-06)
maps all nine criteria to behavioral fixtures and passing exact native/hosted gates.
The single 0.3.0 reservation remains applied; no second bump is introduced.
Confirmed shared development integration `a3b99632ae8cb452d5bfaff5555a3728999e8ba5` was pushed and its remote ref verified on 2026-10-06 at 06:37:20 UTC. Main delivery is pending.
Main acceptance and memory reconciliation remain pending.

## Integration Evidence

Revision: a3b99632ae8cb452d5bfaff5555a3728999e8ba5
Outcome: passed

The exact qualified source was pushed to shared development and its remote ref
verified on 2026-10-06 at 06:37:20 UTC. Independent whole-spec acceptance, frozen
native Task gates and Linux/macOS/Windows CI 37419726793 passed before this event.
