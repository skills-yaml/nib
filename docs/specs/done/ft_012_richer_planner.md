# FT-012: Richer Planner (Symphony-Style)

**Status:** Done
**Related:** [architecture.md](../../tech/architecture.md)

## Summary
Enhance the existing `AgentLoop` to support multi-step reasoning and a dedicated planning phase before execution, inspired by Symphony-style workflows.

## Problem Statement
At the feature baseline, the LLM took a greedy approach and executed tool calls from
the immediate context. The reconciliation below records the shipped plan-first loop.

## Goals
- Introduce a formal `Planner` module.
- Before executing destructive changes, the planner must output a structured sequence of steps.
- The `AgentLoop` will traverse these steps, allowing the LLM to context-switch between implementing, testing, and reviewing specific sub-tasks.
- Ensure the plan is recorded in the session store and updated as reality diverges.

## Scope
- Create a `planner.rs` module that invokes the LLM to create a structured plan (e.g. list of steps) from a given goal.
- Extend `AgentLoop` to check if a plan exists in the current session. If not, generate one.
- Allow `AgentLoop` to track which step is currently being executed and update the plan state.
- Update the session store schema to persist the plan alongside messages.

## Acceptance Criteria
- Given a complex goal, the agent first generates a multi-step plan without executing tools.
- The plan is saved in the session store.
- The agent loop executes each step sequentially, passing the current step's context to the LLM.
- `task check` passes without issues.
- All related unit tests pass.

## Affected Areas
- `src/agent/planner.rs` (new file)
- `src/agent/loop.rs` (AgentLoop logic)
- `src/session/mod.rs` (Session schema for plan storage)
- `src/agent/state.rs` (State machine updates)
- `src/llm/types.rs` (if new parsing types needed for planner)

## Validation Gates
- `task check`
- `task test`
- Manual verification of a multi-step task run resulting in a saved plan.

## Reopened Audit (2026-07-15)

Scope: require non-empty structured plans, expose explicit approval, update steps on
tool/test reality, and verify persistence plus sequential traversal.

Affected areas: `src/agent/`, `src/session/`, planner prompts/types, and planner tests.

Validation gates: generation/persistence/approval/progression tests, `task check`,
and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Generate a non-empty structured plan from bounded assembled context, persist it,
require approval, execute sequential `PlanStep`s, and update outcomes from tool reality.

### Acceptance Criteria

- [x] `src/agent/planner.rs` submits and validates structured plans via `submit_plan`.
- [x] Planner input contains bounded AGENTS, selected skills, memory, workload, session, goal, and tool schema.
- [x] Plans and step status/outcomes persist in profile session JSON.
- [x] Explicit plan approval precedes execution; denial has no side effect.
- [x] Reconciliation advances, blocks, completes, or cancels the active step.
- [x] Final aggregate gates are green.

### Affected Areas

`src/agent/planner.rs`, `src/agent/loop.rs`, `src/agent/state.rs`,
`src/context/budget.rs`, `src/session/mod.rs`, and planner/runtime tests.

### Implementation Evidence

`Plan`/`PlanStep` live in `src/session/mod.rs`; planner generation and aggregate
context budgeting live in `src/agent/planner.rs` and `src/context/budget.rs`.

### Validation Evidence

`contextual_planner_receives_bounded_runtime_and_session_markers`, loop plan/denial
tests, and `tests/test_runtime_e2e.rs` runtime trace and physical-edit scenarios.

### Validation Gates

- [x] Generation, context, persistence, approval, denial, progression, and cancellation tests exist.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

No separate user-facing plan editor exists; approval accepts/rejects the generated
structure. Rich editing would require separate scope.

## Reopened Audit (2026-07-16)

### Scope

Give each structured plan an authoritative identity and goal binding so plan reuse is
limited to a same-goal continuation.

### Acceptance Criteria

- [x] Planner output persists a non-empty unique plan ID and normalized goal.
- [x] Plan structural validation rejects legacy/unbound plans for execution.
- [x] A different goal invalidates the prior plan and generates a fresh revision.
- [x] Tool audit records link to the exact plan ID that authorized execution.
- [x] Same-goal incomplete-plan resumption remains supported and tested.

### Affected Areas

`src/agent/planner.rs`, `src/agent/loop.rs`, `src/session/mod.rs`,
`src/tools/executor.rs`, and planner/runtime tests.

### Implementation Plan

1. Generate and persist a unique plan ID with the normalized goal.
2. Include identity in structural validation, approval events, and audit linkage.
3. Invalidate legacy, completed, and mismatched plans before routing the new turn.
4. Verify same-goal resumption and different-goal revision generation.

### Risks

Legacy serialized sessions remain readable but their plans cannot authorize new
mutations. Audited invalidation preserves traceability while a fresh plan restores a
valid execution path.

### Completion Evidence

Planner output is wrapped in `Plan::new`, producing a unique persisted ID and normalized
goal. Structural validation binds identity, cursor, approval, and step statuses;
legacy, malformed, completed, and mismatched plans are invalidated before routing.
Executor audit IDs come only from the persisted active plan, and completed plans cannot
authorize later mutations.

### Validation Gates

Planner persistence/reuse tests, full-loop coding E2E, `task check`, `task test`, and
`task coverage`.
