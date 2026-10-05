# T058: Preflight Diagnostics and Independent Read Progress

**Status:** Done

State: done
Primary Feature: tools-sandbox

**Related:** [T054](T054_reconcile_local_preflight_failures.md),
[T056](T056_session_worktree_preflight_recovery.md),
[T057](../agent-runtime/T057_contextual_answers_and_plan_progress.md).

## Problem

Two 2026-09-29 interactions stopped before any proposed tool attempt. A repository
analysis proposed three repository reads and a read-only Git command; managed
worktree preparation failed and discarded the exact error. A skill how-to
proposed two repository reads plus `an external skill directory`; the outside-worktree
scope caused all three reads to stop as `instruction_context_missing`, with
irrelevant context-length advice. `nib doctor` checks Git availability but does
not validate durable session worktree receipts.

## Goals

- Make local preflight failures diagnosable through bounded, redaction-safe
  categories and stages while preserving raw private errors only in process.
- Report an outside-worktree tool path as a path-scope error with a relevant
  recovery action.
- Let independent eligible read tools make progress when another proposed tool
  fails a local preflight, returning one audited failure for the rejected tool.
- Give the agent a reliable repository status inspection path without requiring
  a managed worktree for read-only analysis.
- Answer a context-sufficient skill-creation question without unnecessary
  filesystem inspection.

## Non-Goals

- Relaxing worktree isolation for mutating commands or allowing arbitrary
  external paths under the instruction resolver.
- Automatically deleting or repairing an ambiguous worktree receipt.
- Inferring the unrecorded raw error in a historical session.
- Adding a new skill creation command or changing skill installation formats.

## Design

Worktree preflight emits a stable category and stage, with bounded public copy.
Doctor performs read-only receipt checks and reports missing or mismatched
owned artifacts; uncertain records remain untouched. An outside-worktree
declared tool path receives a distinct local tool failure and a specific user
message. For mixed batches, preflight each tool scope independently, retain
valid instructions and tools, and produce failed observations for rejected
calls in the original invocation order. A tool requiring an unavailable
worktree fails individually; other eligible repository reads can proceed.
Security boundaries remain fail closed per tool.

Expose repository Git status through a bounded read-only tool, or an equivalent
read-only capability whose filesystem and Git scope cannot mutate the main
checkout. Include the documented nib skill format and paths in answer context,
so the answer-only route can answer an ordinary how-to from supplied evidence.

## Affected Areas

- Agent tool-batch preflight and reconciliation, instruction scope resolution,
  worktree error classification, and terminal outcomes.
- Tool registry and read-only repository inspection, doctor diagnostics.
- Interactive plain/TUI failure projection, user guide, and deterministic
  runtime, worktree, doctor, and interaction tests.
- Persisted session events gain additive safe fields; no schema migration or
  external service change.

## Acceptance Criteria

- [x] A failed worktree preparation records a bounded category and stage,
  blocks only tools that need that worktree, and never persists raw Git stderr.
- [x] Doctor reports a stale owned receipt with its session identifier and
  missing artifact type, without modifying the receipt or Git state.
- [x] An outside-worktree path yields a specific failure and recovery message;
  context-size advice is absent. Eligible same-batch project reads execute and
  every proposed invocation receives exactly one result.
- [x] Repository status can be inspected without creating a managed worktree;
  mutating terminal commands still require the existing boundary.
- [x] A skill-creation how-to can be answered from supplied documentation with
  no external-path tool request.
- [x] Approval, instruction, worktree ownership, plan verification, and
  provider-continuation invariants remain intact.

## Implementation Plan

1. Add safe typed preflight categories and doctor receipt inspection.
2. Make tool-scope and worktree preflight independent per proposed call.
3. Add bounded read-only Git status and supplied skill guidance.
4. Add deterministic success and failure regressions; reconcile the user guide.

## Validation Gates

- Focused `task test:agent-context`, `task test:worktree`, `task test:doctor`,
  `task test:interactive`, and `task test:runtime-e2e` while iterating.
- `task check`, `task docs:check`, and complete `task verify` before completion.

## Risks and Rollback

Partial-batch execution must not bypass an affected tool's instructions or
approval gate. Preserve the original invocation IDs and result order. Treat
ambiguous worktree ownership as diagnostic only. Rollback restores prior
preflight behavior without migrating session data.

## Prior Memory Notes

The stable preflight and diagnostic contracts are recorded in
`workspace/agents/memory/decisions.md`; session paths and transient errors are omitted.

## Completion Evidence (2026-09-29)

- `task test:agent-context`, `task test:worktree`, `task test:doctor`, and
  `task test:interactive` passed during focused development.
- `task test:runtime-e2e` passed all 51 cases after preserving the historical
  instruction-failure event and plan outcome.
- `task verify` passed strict Clippy, 1,247 library tests, 93 binary tests,
  integration targets, and doctests. `task docs:check` validates the final spec
  state.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | historical | Retrospective classification of the preserved pre-v7 outcome; no new bump or release identity is inferred. |

## Memory Impact

Status: none
Rationale: This historical outcome is preserved; migration adds no new durable decision for this spec. Existing dated memory evidence remains authoritative.
