# T005: Full Runtime State Machine and Lifecycle

**Status:** Done

**Related Feature:** T002: Agent Framework Runtime and Orchestration Engine for nib

> Historical proposal note: the problem and Python design sections capture the
> pre-state-machine baseline. The 2026-07-15 reconciliation defines the shipped Rust
> lifecycle and its persisted transition evidence.

## Historical Problem Statement (Proposal-Time)

nib's current execution is ad-hoc via ToolExecutor calls within the context of tasks, without a formal finite state machine or bounded orchestration loop. This leads to unpredictable behavior in long-running sessions: no clear lifecycle (IDLE → BUILD_CONTEXT → INSPECT_LLM → APPROVAL → EXECUTE → UPDATE → loop), no enforcement of max_turns or resource bounds, and fragile handling of state transitions. The provided specification requires a robust state machine to manage autonomous loops, prevent infinite execution, and ensure predictable interactions from user input to final response.

## Goals

- Implement an explicit finite state machine for the agent runtime (IDLE, BUILD_CONTEXT, INSPECT_LLM, USER_APPROVAL, TOOL_EXECUTE, UPDATE_MEMORY, etc.), bounded by max_turns and config.
- Integrate with Context Engine (T003), ToolExecutor (with full permissions), WorkloadStore, and maintenance (T004).
- Enforce invariants like role alternation in sessions and compression triggers.
- Support the full end-to-end sequence diagram from T002.
- Enable bounded, reliable autonomous operation while respecting human-in-the-loop and workload buckets (backlog/working/done).

## Non-Goals

- Replacing the entire agent with a new LLM (use existing bindings).
- Handling non-linear or concurrent state (focus on single-turn sequential loop initially).

## Historical Proposed Design

Add `src/nib/core/runtime.py` with a StateMachine class.

**States (mapped from spec):**
- IDLE: Waiting for input.
- BUILD_CONTEXT: Assemble prompt using context loader (T003), skills (T006), AGENTS.md, workload snapshot.
- INSPECT_LLM: POST to model with history + tools.
- USER_APPROVAL: Guard via ToolExecutor permissions (manual/smart/policy).
- TOOL_EXECUTE: Dispatch to sandbox (worktree, MCP, etc.).
- UPDATE_MEMORY: Append to session (T003), record to workload, optional compression (T003), curator hooks (T004).
- RENDER/LOOP: Return response or continue if turns remain.

**Lifecycle:**
Use an async loop in the runtime:
```python
while turns < max_turns:
    state = next_state(state, input)
    if state == BUILD_CONTEXT: ...
    ...
    if final_text: break
```

Integrate:
- Call context.build_compressed_context() in BUILD_CONTEXT.
- Use ToolExecutor.execute() in TOOL_EXECUTE (enforces approvals).
- Persist session/memory after UPDATE.
- Bound by agent.max_turns; trigger compression as needed.

**Error Handling:** Per spec — raw errors to model, backoff, etc.

Update runtime entrypoints in cli/tui to use the state machine.

## Alternatives Considered

- Keep ad-hoc executor: Rejected — doesn't meet bounded loop or state requirements.
- Use external framework for states: Rejected for minimalism.

## Risks and Tradeoffs

- Complexity in state transitions (mitigation: diagram-driven implementation, tests).
- Loop overhead (tradeoff for reliability).

## Rollout Plan

1. Define states and transitions in code.
2. Wire into existing executor/context.
3. Add loop control and max_turns.
4. Integrate with T003/T006.
5. Validate against T002 diagram and T008 tests.

## Validation and Acceptance Criteria

- State machine executes full cycle without violating bounds or invariants.
- Compression and approvals happen at correct states.
- Diagram in T002 matches implementation flow.
- `task test` covers state transitions.

## Open Questions

- Handling of parallel sub-agents in the state machine?
- Exact transition triggers for compression?

## Reopened Audit (2026-07-15)

Scope: make approval an observable lifecycle state, enforce session invariants and
bounds, expose transition traces, and align the implementation with T002.

Affected areas: `src/agent/`, `src/tools/executor.rs`, `src/session/`, and runtime tests.

Validation gates: transition/order/bound/error tests, diagram trace assertions,
`task check`, and `task test`.

## Implementation Reconciliation (2026-07-15)

### Scope

Run planning, plan approval, bounded context/compression, streamed inspection, tool
approval/execution, memory update, clarification, cancellation, and reconciliation as
an explicit persisted lifecycle.

### Acceptance Criteria

- [x] `AgentState` defines and validates the shipped transition graph.
- [x] Execution cannot precede structured plan approval.
- [x] Turn and transition bounds reconcile instead of dispatching extra work.
- [x] Approval denial, failed tools, questions, and user cancellation produce audited terminal outcomes.
- [x] Transition traces and stream events make the lifecycle observable.
- [x] Final repository gates are green.

### Affected Areas

`src/agent/state.rs`, `src/agent/loop.rs`, `src/session/`, `src/tools/executor.rs`,
`src/tui/mod.rs`, and lifecycle/E2E tests.

### Implementation Evidence

- `src/agent/state.rs` owns the transition graph.
- `src/agent/loop.rs` owns bounded dispatch, cancellation reconciliation, plan/step
  updates, questions, tool observations, and terminal `StreamEvent::End` emission.

### Validation Evidence

- `src/agent/state.rs`: `lifecycle_accepts_diagram_order` and
  `lifecycle_rejects_execution_before_plan_approval`.
- `src/agent/loop.rs`: audited lifecycle, denied plan, configured bound, question,
  failed-tool correction, and cancellation tests.
- `tests/test_runtime_e2e.rs`: runtime trace and configured-bound scenarios.

### Validation Gates

- [x] Transition, denial, bound, question, correction, and cancellation tests exist.
- [x] `task check`.
- [x] `task test`.

### Genuine Gaps

Parallel subagents remain separate child loops linked through delegation records; the
single loop intentionally remains sequential. This is an explicit concurrency
boundary, not an incomplete acceptance criterion.

## Reopened Audit (2026-07-16)

### Scope

Prevent an approved incomplete plan from crossing a user-goal boundary when a session
is resumed.

### Acceptance Criteria

- [x] Idle-state routing reuses a plan only when its persisted goal exactly matches the
  current normalized goal.
- [x] Mismatched and legacy unbound plans are invalidated with an auditable event before
  the state machine leaves `Idle`.
- [x] Same-goal resumptions retain their approved incomplete plan and continue safely.
- [x] Regression tests cover mismatched-goal invalidation and same-goal resumption.

### Affected Areas

`src/session/mod.rs`, `src/agent/loop.rs`, state/lifecycle tests, and runtime E2E tests.

### Implementation Plan

1. Classify existing plans as same-goal resumable, completed, mismatched, or legacy.
2. Persist invalidation evidence before routing from `Idle`.
3. Add loop-level regressions for same-goal continuation and different-goal replanning.

### Risks

Over-eager invalidation could discard valid progress. The classifier therefore retains
only incomplete, identity-bearing plans whose normalized goal exactly matches the new
request, and tests assert that approval is not repeated for that continuation.

### Completion Evidence

Idle routing performs invalidation and route selection atomically. Plan structure now
binds cursor, approval, and step statuses; malformed, completed, legacy, and
goal-mismatched plans cannot authorize execution. A whole-run session lease prevents
concurrent plan/worktree ownership, while exact-plan CAS ignores an approval response
if an out-of-band writer replaces the displayed plan.

### Validation Gates

Focused plan-reuse lifecycle tests, `task check`, `task test`, and `task coverage`.
